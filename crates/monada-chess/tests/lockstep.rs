//! Networked chess over lockstep, headless (no window) — the key
//! automated cover for the M4 slice-4 net path. Two
//! `LockstepSession<LoopbackTransport, RhaiDriver>` play the *same* fixed
//! game from one command stream and must agree on the world hash at every
//! move; the recorded replay must then reproduce the final state through
//! the **verified** path (map hash + engine version), exactly as the GUI
//! `--listen`/`--connect` clients do.

use std::path::Path;
use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_net::{LockstepSession, LoopbackTransport, MatchInfo, SessionConfig, SimDriver};
use monada_script::{shared_world, NullBridge, RhaiDriver, SharedBridge, SharedWorld};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const PIECE: ArchetypeId = ArchetypeId(0);
const GAME: ArchetypeId = ArchetypeId(1);
const MOVE: u32 = 1;
const P0: PlayerId = PlayerId(0);
const P1: PlayerId = PlayerId(1);

type Session = LockstepSession<LoopbackTransport, RhaiDriver>;

/// The packed chess map: archive bytes, its SHA-256 identity, entry script.
fn chess_map() -> monada_format::Map {
    let map_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack chess map");
    monada_format::Map::read(&bytes).expect("read chess map")
}

fn session(player: PlayerId, transport: LoopbackTransport, map: &monada_format::Map) -> Session {
    // NullBridge: init paints the board / defines models, but headless we
    // only care about sim state. Stateless, so both sessions can share it.
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let script = map.entry_script().expect("entry script");
    let driver =
        RhaiDriver::with_bridge(shared_world(SEED), script, &bridge).expect("compile chess map");
    let info = MatchInfo {
        seed: SEED,
        map_hash: map.hash,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    LockstepSession::new(
        driver,
        transport,
        player,
        &[P0, P1],
        SessionConfig::default(),
        info,
    )
}

fn square(x: i32, y: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO)
}

/// The piece entity on `(x, y)` in `world`, if any.
fn piece_at(world: &SharedWorld, x: i32, y: i32) -> Option<EntityId> {
    let w = world.lock().unwrap();
    w.entities(PIECE)
        .iter()
        .copied()
        .find(|&e| w.position(e) == Some(square(x, y)))
}

fn game_field(world: &SharedWorld, field: &str) -> i64 {
    let w = world.lock().unwrap();
    let g = w.entities(GAME)[0];
    i64::from(w.field(g, field).unwrap().floor_to_int())
}

/// Play one move over lockstep: look the piece up on the (settled) board,
/// submit the command on session A, advance both sessions until it has
/// executed, and assert the two worlds still agree. Targeting by square is
/// safe because we only submit the next move after the previous one has
/// fully executed, so the board the lookup sees is stable.
fn play(a: &mut Session, b: &mut Session, fx: i32, fy: i32, tx: i32, ty: i32) {
    let e = piece_at(a.driver().world(), fx, fy).expect("a piece on the source square");
    // Colour-enforced: the player id is irrelevant; submit on A, empty on B.
    assert!(a
        .step(vec![Command::on(MOVE, e, square(tx, ty))])
        .expect("a"));
    assert!(b.step(Vec::new()).expect("b"));
    // The command executes `command_delay` ticks ahead; drain those ticks.
    for _ in 0..=SessionConfig::default().command_delay {
        assert!(a.step(Vec::new()).expect("a"));
        assert!(b.step(Vec::new()).expect("b"));
    }
    assert_eq!(
        a.driver().state_hash(),
        b.driver().state_hash(),
        "lockstep chess diverged"
    );
}

#[test]
fn two_clients_play_a_game_and_replay_reproduces() {
    let map = chess_map();
    let (ta, tb) = LoopbackTransport::pair();
    let mut a = session(P0, ta, &map);
    let mut b = session(P1, tb, &map);

    // A fool's-mate line played to actually capture the king (the subset's
    // win condition) — exercises moves, a capture, and game-over in sync.
    play(&mut a, &mut b, 5, 1, 5, 2); // 1. f3
    play(&mut a, &mut b, 4, 6, 4, 4); // 1... e5
    play(&mut a, &mut b, 6, 1, 6, 3); // 2. g4
    play(&mut a, &mut b, 3, 7, 7, 3); // 2... Qh4
    play(&mut a, &mut b, 0, 1, 0, 2); // 3. a3
    play(&mut a, &mut b, 7, 3, 4, 0); // 3... Qxe1 — takes the king

    assert_eq!(game_field(a.driver().world(), "winner"), 1, "black wins");
    assert_eq!(
        a.driver().world().lock().unwrap().count(PIECE),
        31,
        "white king removed"
    );

    // The replay reproduces A's final state through the verified path,
    // which checks the replay's map hash + engine version against this map.
    let final_hash = a.driver().state_hash();
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let mut fresh =
        RhaiDriver::with_bridge(shared_world(SEED), map.entry_script().unwrap(), &bridge)
            .expect("compile chess map");
    let replayed = a
        .replay()
        .playback_verified(&mut fresh, map.hash, env!("CARGO_PKG_VERSION"))
        .expect("replay identity matches the map");
    assert_eq!(replayed, final_hash, "replay did not reproduce the game");
}
