//! Co-op action-RPG demo over lockstep, headless (no window) — the automated cover
//! for the LAN path. Two `LockstepSession<LoopbackTransport, RhaiDriver>`
//! play the same arena from a shared per-tick input stream (each peer
//! submitting its own real-time input) and must agree on the world hash at
//! every tick. Each peer applies *both* players' inputs in lockstep, so both
//! heroes spawn on both peers (no friendly fire, shared waves). Mirrors the
//! `monada-chess`
//! `lockstep.rs`, but for continuous real-time input instead of turn moves.

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_net::{LockstepSession, LoopbackTransport, MatchInfo, SessionConfig, SimDriver};
use monada_script::{shared_world, RhaiDriver, SharedBridge, TerrainBridge};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const PLAYER: ArchetypeId = ArchetypeId(0);
const VERB_INPUT: u32 = 0;
const P0: PlayerId = PlayerId(0);
const P1: PlayerId = PlayerId(1);

type Session = LockstepSession<LoopbackTransport, RhaiDriver>;

fn rpg_map() -> monada_format::Map {
    let map_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack rpg map");
    monada_format::Map::read(&bytes).expect("read rpg map")
}

fn session(player: PlayerId, transport: LoopbackTransport, map: &monada_format::Map) -> Session {
    // Each peer gets its own terrain store (its own `MapRender` would, live);
    // both inits paint identically, so collision queries agree.
    let bridge: SharedBridge = Arc::new(Mutex::new(TerrainBridge::new()));
    let script = map.entry_script().expect("entry script");
    let driver =
        RhaiDriver::with_bridge(shared_world(SEED), script, &bridge).expect("compile rpg map");
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

fn input(mx: i32, my: i32, buttons: u64) -> Command {
    Command::on(
        VERB_INPUT,
        EntityId(buttons),
        FixedVec3::new(Fixed::from_int(mx), Fixed::from_int(my), Fixed::ZERO),
    )
}

/// A distinct, deterministic per-peer real-time input for tick `t`.
fn peer_input(player: u64, t: u64) -> Command {
    let (mx, my) = if player == 0 {
        match t % 4 {
            0 => (1, 0),
            1 => (0, 1),
            2 => (-1, 0),
            _ => (0, -1),
        }
    } else {
        match t % 4 {
            0 => (0, 1),
            1 => (-1, 0),
            2 => (0, -1),
            _ => (1, 0),
        }
    };
    let mut btn = 0;
    if t % 9 == 0 {
        btn |= 1; // attack
    }
    if t % 17 == 0 {
        btn |= 2; // dodge
    }
    input(mx, my, btn)
}

#[test]
fn co_op_two_peers_stay_in_sync() {
    let map = rpg_map();
    let (ta, tb) = LoopbackTransport::pair();
    let mut a = session(P0, ta, &map);
    let mut b = session(P1, tb, &map);

    // Every tick, each peer submits its own input; the lockstep session folds
    // both into the same released tick on both ends.
    for t in 0..200u64 {
        assert!(a.step(vec![peer_input(0, t)]).expect("peer A no desync"));
        assert!(b.step(vec![peer_input(1, t)]).expect("peer B no desync"));
        assert_eq!(
            a.driver().state_hash(),
            b.driver().state_hash(),
            "co-op peers diverged at tick {t}"
        );
    }

    // Both peers applied both players' inputs, so both heroes exist on both.
    assert_eq!(
        a.driver().world().lock().unwrap().count(PLAYER),
        2,
        "co-op spawned a hero per player on peer A"
    );
    assert_eq!(
        b.driver().world().lock().unwrap().count(PLAYER),
        2,
        "co-op spawned a hero per player on peer B"
    );
}

#[test]
fn replay_reproduces_the_co_op_session() {
    let map = rpg_map();
    let (ta, tb) = LoopbackTransport::pair();
    let mut a = session(P0, ta, &map);
    let mut b = session(P1, tb, &map);
    for t in 0..120u64 {
        a.step(vec![peer_input(0, t)]).expect("a");
        b.step(vec![peer_input(1, t)]).expect("b");
    }
    let final_hash = a.driver().state_hash();

    // A fresh driver replaying A's recorded input stream must reach the same
    // state (the verified replay path checks map hash + engine version).
    let bridge: SharedBridge = Arc::new(Mutex::new(TerrainBridge::new()));
    let mut fresh =
        RhaiDriver::with_bridge(shared_world(SEED), map.entry_script().unwrap(), &bridge)
            .expect("compile rpg map");
    assert_eq!(
        a.replay().playback(&mut fresh),
        final_hash,
        "replay reproduces co-op"
    );
}
