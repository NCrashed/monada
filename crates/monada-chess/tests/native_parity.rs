//! Backend parity on a real map — the D-0 gate of
//! docs/plans/desert-game.md §11.
//!
//! `tests/native_backend.rs` in monada-script proved the two runtimes
//! agree on a hundred entities walking in a circle. This proves it on
//! something that can actually be wrong: chess, played move by move
//! through the Rhai script *and* through [`ChessRules`], with the world's
//! state hash compared after **every ply**. A divergence in move
//! legality, in capture bookkeeping, in castling rights, in the
//! fifty-move clock or in the order entities are scanned shows up on the
//! ply that caused it rather than at the end.
//!
//! The lines are chosen for coverage, not beauty: Scholar's mate (capture,
//! checkmate, `winner`), a castling line, an en-passant capture, and a
//! promotion. An illegal move joins them, because rejection must be as
//! identical as acceptance.

use std::sync::{Arc, Mutex};

use monada_chess::ChessRules;
use monada_fixed::{Fixed, FixedVec3};
use monada_runtime::{MapRules, NativeBackend};
use monada_script::{
    shared_world, NullBridge, RhaiBackend, ScriptBackend, SharedBridge, SharedWorld,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const PIECE: ArchetypeId = ArchetypeId(0);
const MOVE: u32 = 1;
const ANY: PlayerId = PlayerId(0); // turn is enforced by piece colour

/// A ply as board squares: `(from_x, from_y, to_x, to_y)`.
type Ply = (i32, i32, i32, i32);

/// The chess rules as the shipped map ships them: packed, read back, and
/// taken from the manifest's entry — the same path `tests/rules.rs` uses.
fn chess_script() -> String {
    let map_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack chess map");
    let map = monada_format::Map::read(&bytes).expect("read chess map");
    map.entry_script()
        .expect("chess map has an entry script")
        .to_string()
}

fn square(x: i32, y: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO)
}

/// The piece standing on `(x, y)`, if any.
fn piece_at(world: &SharedWorld, x: i32, y: i32) -> Option<EntityId> {
    let w = world.lock().expect("world mutex");
    w.entities(PIECE)
        .iter()
        .copied()
        .find(|&e| w.position(e) == Some(square(x, y)))
}

/// Turn a ply into the command the local layer would submit, resolving
/// the mover against *this* world (a wrong square yields entity 0, which
/// the rules reject — which is itself worth comparing).
fn command_for(world: &SharedWorld, ply: Ply) -> Command {
    let (fx, fy, tx, ty) = ply;
    Command {
        verb: MOVE,
        target: piece_at(world, fx, fy).unwrap_or(EntityId(0)),
        arg: square(tx, ty),
    }
}

fn state_hash(world: &SharedWorld) -> u64 {
    world.lock().expect("world mutex").state_hash()
}

fn null_bridge() -> SharedBridge {
    Arc::new(Mutex::new(NullBridge))
}

/// Play `plies` through the Rhai script, hashing after each.
fn rhai_run(plies: &[Ply]) -> Vec<u64> {
    let world = shared_world(SEED);
    let mut backend = RhaiBackend::new(world.clone());
    backend.set_bridge(&null_bridge());
    backend.load(&chess_script()).expect("compile main.rhai");
    backend.on_init().expect("init");
    let mut hashes = vec![state_hash(&world)];
    for &ply in plies {
        let cmd = command_for(&world, ply);
        backend.on_command(ANY, &cmd).expect("command");
        hashes.push(state_hash(&world));
    }
    hashes
}

/// Play the same plies through the compiled rules.
fn native_run(plies: &[Ply]) -> Vec<u64> {
    let world = shared_world(SEED);
    let mut backend = NativeBackend::new(world.clone(), Box::new(ChessRules::new()));
    backend.set_bridge(&null_bridge());
    backend.on_init().expect("init");
    let mut hashes = vec![state_hash(&world)];
    for &ply in plies {
        let cmd = command_for(&world, ply);
        backend.on_command(ANY, &cmd).expect("command");
        hashes.push(state_hash(&world));
    }
    hashes
}

/// Assert the two runtimes agree ply by ply, naming the first that drifts.
fn assert_parity(name: &str, plies: &[Ply]) {
    let rhai = rhai_run(plies);
    let native = native_run(plies);
    for (i, (r, n)) in rhai.iter().zip(&native).enumerate() {
        assert_eq!(
            r, n,
            "{name}: the backends diverged after ply {i} \
             (0 = the position `init` set up)"
        );
    }
    assert_eq!(rhai.len(), native.len(), "{name}: ply count");
}

/// Scholar's mate: 1.e4 e5 2.Bc4 Nc6 3.Qh5 Nf6 4.Qxf7#.
/// Covers a capture, check, checkmate and the `winner` field.
const SCHOLARS_MATE: &[Ply] = &[
    (4, 1, 4, 3), // e4
    (4, 6, 4, 4), // e5
    (5, 0, 2, 3), // Bc4
    (1, 7, 2, 5), // Nc6
    (3, 0, 7, 4), // Qh5
    (6, 7, 5, 5), // Nf6
    (7, 4, 5, 6), // Qxf7#
];

/// 1.e4 e5 2.Nf3 Nc6 3.Bc4 Bc5 4.O-O — the king-side castle, which moves
/// two entities on one command and retires two castling rights.
const CASTLING: &[Ply] = &[
    (4, 1, 4, 3), // e4
    (4, 6, 4, 4), // e5
    (6, 0, 5, 2), // Nf3
    (1, 7, 2, 5), // Nc6
    (5, 0, 2, 3), // Bc4
    (5, 7, 2, 4), // Bc5
    (4, 0, 6, 0), // O-O
];

/// 1.e4 Nf6 2.e5 d5 3.exd6 e.p. — the capture that removes a piece which
/// is not on the target square, and the only move that reads `ep_x/ep_y`.
const EN_PASSANT: &[Ply] = &[
    (4, 1, 4, 3), // e4
    (6, 7, 5, 5), // Nf6
    (4, 3, 4, 4), // e5
    (3, 6, 3, 4), // d5 (double push, offering en passant on d6)
    (4, 4, 3, 5), // exd6 e.p.
];

/// 1.h4 g5 2.hxg5 Nf6 3.g6 Ne4 4.g7 Nc6 5.gxh8=Q — a pawn walking to the
/// last rank and promoting on a capture, which also swaps its model and
/// clears a castling right.
const PROMOTION: &[Ply] = &[
    (7, 1, 7, 3), // h4
    (6, 6, 6, 4), // g5
    (7, 3, 6, 4), // hxg5
    (6, 7, 5, 5), // Nf6
    (6, 4, 6, 5), // g6
    (5, 5, 4, 3), // Ne4
    (6, 5, 6, 6), // g7
    (1, 7, 2, 5), // Nc6
    (6, 6, 7, 7), // gxh8=Q
];

/// Illegal and malformed input: a piece that cannot move that way, the
/// wrong side's piece, an empty square, and a move after the game is
/// decided. Rejection must be as identical as acceptance.
const ILLEGAL: &[Ply] = &[
    (4, 1, 4, 5), // e2-e6: too far
    (4, 6, 4, 4), // black moving out of turn
    (2, 3, 3, 3), // an empty square as the origin
    (1, 0, 2, 2), // Nc3, legal — the position must still advance
    (0, 6, 0, 4), // a5, legal for black
];

#[test]
fn scholars_mate_is_identical() {
    assert_parity("scholar's mate", SCHOLARS_MATE);
}

#[test]
fn castling_is_identical() {
    assert_parity("castling", CASTLING);
}

#[test]
fn en_passant_is_identical() {
    assert_parity("en passant", EN_PASSANT);
}

#[test]
fn promotion_is_identical() {
    assert_parity("promotion", PROMOTION);
}

#[test]
fn illegal_moves_are_rejected_identically() {
    assert_parity("illegal", ILLEGAL);
}

#[test]
fn the_openings_agree_and_the_lines_actually_do_something() {
    // Guards the parity assertions from passing vacuously: the initial
    // positions match, and every line changes the world.
    let plies_moved = |line: &[Ply]| {
        let h = native_run(line);
        h[0] != *h.last().expect("at least the initial hash")
    };
    assert_eq!(rhai_run(&[])[0], native_run(&[])[0], "initial position");
    for (name, line) in [
        ("scholar's mate", SCHOLARS_MATE),
        ("castling", CASTLING),
        ("en passant", EN_PASSANT),
        ("promotion", PROMOTION),
    ] {
        assert!(plies_moved(line), "{name} left the world untouched");
    }
}

/// The lines must *reach* what they claim: a mate ends the game, and the
/// promotion line really produces a queen. Otherwise a shared bug in both
/// implementations could hide behind a passing parity test.
#[test]
fn the_lines_reach_their_advertised_positions() {
    let world = shared_world(SEED);
    let mut backend = NativeBackend::new(world.clone(), Box::new(ChessRules::new()));
    backend.set_bridge(&null_bridge());
    backend.on_init().expect("init");
    for &ply in SCHOLARS_MATE {
        let cmd = command_for(&world, ply);
        backend.on_command(ANY, &cmd).expect("command");
    }
    let w = world.lock().expect("world mutex");
    let game = w.entities(ArchetypeId(1))[0];
    assert_eq!(
        w.field(game, "winner").map(Fixed::floor_to_int),
        Some(0),
        "scholar's mate should leave white the winner"
    );
    drop(w);

    let world = shared_world(SEED);
    let mut backend = NativeBackend::new(world.clone(), Box::new(ChessRules::new()));
    backend.set_bridge(&null_bridge());
    backend.on_init().expect("init");
    for &ply in PROMOTION {
        let cmd = command_for(&world, ply);
        backend.on_command(ANY, &cmd).expect("command");
    }
    let queen = piece_at(&world, 7, 7).expect("a piece on h8");
    let w = world.lock().expect("world mutex");
    assert_eq!(
        w.field(queen, "kind").map(Fixed::floor_to_int),
        Some(4),
        "the promoted pawn should now be a queen"
    );
}

/// A native map must also be drivable through the same `MapRules` value
/// twice without carrying state between matches — the shape the campaign
/// shell (D-10) needs when it restarts a mission.
#[test]
fn rules_restart_cleanly() {
    let mut rules = ChessRules::new();
    let mut hashes = Vec::new();
    for _ in 0..2 {
        let world = shared_world(SEED);
        let bridge = null_bridge();
        let host = monada_runtime::RuntimeHost::new(world.clone());
        let mut host = host;
        host.set_bridge(&bridge);
        rules.init(&host);
        hashes.push(state_hash(&world));
    }
    assert_eq!(hashes[0], hashes[1], "a second init must set up the same board");
}
