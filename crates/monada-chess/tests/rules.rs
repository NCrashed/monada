//! The chess map's rules as a unit-level canary (DESIGN.md §6). Loads the
//! rules **from the packed `map/` archive** (exercising the format round-
//! trip) under a [`NullBridge`] (so `init`'s render calls are no-ops), then
//! drives the `command` handler directly and asserts the resulting **world
//! state**: opening setup, legal movement, turn alternation, illegal-move
//! rejection with the sim hash untouched, capture = despawn, and win-on-
//! king-capture. The seed of the M4 oracle golden (slice 4).

use std::path::Path;
use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_script::{
    shared_world, NullBridge, RhaiBackend, ScriptBackend, SharedBridge, SharedWorld,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const PIECE: ArchetypeId = ArchetypeId(0);
const GAME: ArchetypeId = ArchetypeId(1);
const MOVE: u32 = 1;
const ANY: PlayerId = PlayerId(0); // turn is enforced by piece colour, not id

/// The chess rules, loaded through the real archive path: pack `map/`,
/// read it back, take the manifest's entry script.
fn chess_script() -> String {
    let map_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack chess map");
    let map = monada_format::Map::read(&bytes).expect("read chess map");
    map.entry_script()
        .expect("chess map has an entry script")
        .to_string()
}

fn fresh() -> (SharedWorld, RhaiBackend) {
    let world = shared_world(SEED);
    let mut backend = RhaiBackend::new(world.clone());
    // `init` defines models / paints the board — needs a bridge (no-op).
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    backend.load(&chess_script()).expect("compile main.rhai");
    backend.on_init().expect("init runs");
    (world, backend)
}

fn square(x: i32, y: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO)
}

/// The piece entity standing on `(x, y)`, if any.
fn piece_at(world: &SharedWorld, x: i32, y: i32) -> Option<EntityId> {
    let w = world.lock().unwrap();
    w.entities(PIECE)
        .iter()
        .copied()
        .find(|&e| w.position(e) == Some(square(x, y)))
}

fn occupied(world: &SharedWorld, x: i32, y: i32) -> bool {
    piece_at(world, x, y).is_some()
}

fn piece_count(world: &SharedWorld) -> usize {
    world.lock().unwrap().count(PIECE)
}

/// Read the singleton `game` entity's integer field (`to_move`/`winner`).
fn game_field(world: &SharedWorld, field: &str) -> i64 {
    let w = world.lock().unwrap();
    let g = w.entities(GAME)[0];
    i64::from(w.field(g, field).unwrap().floor_to_int())
}

/// Move whatever stands on `(fx,fy)` to `(tx,ty)` (as the side to move).
fn mv(b: &mut RhaiBackend, world: &SharedWorld, fx: i32, fy: i32, tx: i32, ty: i32) {
    let e = piece_at(world, fx, fy).expect("a piece on the source square");
    b.on_command(ANY, &Command::on(MOVE, e, square(tx, ty)))
        .expect("handler runs");
}

#[test]
fn opening_position_is_standard() {
    let (world, _b) = fresh();
    assert_eq!(piece_count(&world), 32, "16 pieces a side");
    assert!(occupied(&world, 4, 1), "white e-pawn on e2");
    assert!(occupied(&world, 0, 7), "black rook on a8");
    assert!(!occupied(&world, 4, 3), "e4 empty at the start");
    assert_eq!(game_field(&world, "to_move"), 0, "white to move");
    assert_eq!(game_field(&world, "winner"), -1, "game in progress");
}

#[test]
fn legal_moves_alternate_turns() {
    let (world, mut b) = fresh();

    mv(&mut b, &world, 4, 1, 4, 3); // 1. e4 (white double step)
    assert!(occupied(&world, 4, 3) && !occupied(&world, 4, 1));
    assert_eq!(game_field(&world, "to_move"), 1, "black to move");

    mv(&mut b, &world, 6, 7, 5, 5); // 1... Nf6 (the L-move canary)
    assert!(occupied(&world, 5, 5) && !occupied(&world, 6, 7));
    assert_eq!(game_field(&world, "to_move"), 0, "white to move");

    mv(&mut b, &world, 5, 0, 2, 3); // 2. Bc4 (bishop slides the diagonal)
    assert!(occupied(&world, 2, 3) && !occupied(&world, 5, 0));
    assert_eq!(game_field(&world, "to_move"), 1);
}

#[test]
fn illegal_moves_are_rejected_without_touching_state() {
    let (world, mut b) = fresh();
    let hash0 = world.lock().unwrap().state_hash();

    // Black to move out of turn (white's turn): rejected by colour.
    let e = piece_at(&world, 4, 6).unwrap();
    b.on_command(ANY, &Command::on(MOVE, e, square(4, 4))).unwrap();

    // White knight to a non-L (empty) square.
    let e = piece_at(&world, 1, 0).unwrap();
    b.on_command(ANY, &Command::on(MOVE, e, square(1, 2))).unwrap();

    // A blocked rook (own pawn in front) cannot move.
    let e = piece_at(&world, 0, 0).unwrap();
    b.on_command(ANY, &Command::on(MOVE, e, square(0, 3))).unwrap();

    assert_eq!(
        world.lock().unwrap().state_hash(),
        hash0,
        "no illegal attempt may perturb the deterministic state"
    );
    assert_eq!(piece_count(&world), 32);
    assert_eq!(game_field(&world, "to_move"), 0, "still white to move");
}

#[test]
fn capture_removes_the_taken_piece() {
    let (world, mut b) = fresh();

    mv(&mut b, &world, 4, 1, 4, 3); // 1. e4
    mv(&mut b, &world, 3, 6, 3, 4); // 1... d5
    mv(&mut b, &world, 4, 3, 3, 4); // 2. exd5

    assert_eq!(piece_count(&world), 31, "one black pawn gone");
    let taken = piece_at(&world, 3, 4).expect("white pawn now stands on d5");
    assert_eq!(
        world.lock().unwrap().field(taken, "color"),
        Some(Fixed::from_int(0)),
        "the survivor on d5 is the white pawn"
    );
    assert_eq!(game_field(&world, "to_move"), 1, "black to move after capture");
}

#[test]
fn capturing_the_king_wins() {
    let (world, mut b) = fresh();

    // A fool's-mate shape, played to actually *take* the king (the subset
    // has no check rule — king capture is the win condition).
    mv(&mut b, &world, 5, 1, 5, 2); // 1. f3
    mv(&mut b, &world, 4, 6, 4, 4); // 1... e5
    mv(&mut b, &world, 6, 1, 6, 3); // 2. g4
    mv(&mut b, &world, 3, 7, 7, 3); // 2... Qh4 (slides the open diagonal)
    mv(&mut b, &world, 0, 1, 0, 2); // 3. a3 (a waiting move)
    mv(&mut b, &world, 7, 3, 4, 0); // 3... Qxe1 — takes the king

    assert_eq!(game_field(&world, "winner"), 1, "black wins");
    assert_eq!(piece_count(&world), 31, "white king removed");
    let on_e1 = piece_at(&world, 4, 0).expect("the black queen occupies e1");
    let w = world.lock().unwrap();
    assert_eq!(w.field(on_e1, "color"), Some(Fixed::from_int(1)), "it is black");
    assert_eq!(w.field(on_e1, "kind"), Some(Fixed::from_int(4)), "it is the queen");
    drop(w);

    // The game is decided: further commands are no-ops.
    let before = world.lock().unwrap().state_hash();
    let e = piece_at(&world, 0, 2).unwrap();
    b.on_command(ANY, &Command::on(MOVE, e, square(0, 3))).unwrap();
    assert_eq!(world.lock().unwrap().state_hash(), before, "game over: no moves");
}
