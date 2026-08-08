//! 1v1 RTS over lockstep, headless — the automated cover for the LAN path
//! AND the R-C group-order contract: a box select submits one MOVE command
//! per selected unit in a single tick (`Vec<Command>` per player on the
//! wire), so this drives multi-command BURSTS through the schedule and
//! asserts both peers fold them identically. Mirrors monada-rpg's
//! `lockstep.rs`, but with event-driven orders instead of a per-tick input
//! stream (most ticks carry no commands at all — the RTS wire shape).

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_net::{LockstepSession, LoopbackTransport, MatchInfo, SessionConfig, SimDriver};
use monada_script::{shared_world, RhaiDriver, SharedBridge, NullBridge};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const UNIT: ArchetypeId = ArchetypeId(0);
const VERB_MOVE: u32 = 1;
const P0: PlayerId = PlayerId(0);
const P1: PlayerId = PlayerId(1);

type Session = LockstepSession<LoopbackTransport, RhaiDriver>;

fn rts_map() -> monada_format::Map {
    let map_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack rts map");
    monada_format::Map::read(&bytes).expect("read rts map")
}

fn session(player: PlayerId, transport: LoopbackTransport, map: &monada_format::Map) -> Session {
    // Each peer gets its own terrain store (its own `MapRender` would,
    // live); both inits paint identically, so nav + collision agree.
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let script = map.entry_script().expect("entry script");
    let mut driver =
        RhaiDriver::with_bridge(shared_world(SEED), script, &bridge).expect("compile rts map");
    if let monada_format::SimHz::Fixed(hz) = map.manifest.sim_hz {
        driver.set_tick_hz(hz);
    }
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

fn move_cmd(unit: EntityId, x: i32, y: i32) -> Command {
    Command::on(
        VERB_MOVE,
        unit,
        FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO),
    )
}

/// The six starting workers, in spawn order: [0..3) player 0, [3..6)
/// player 1. Init is deterministic, so ids agree across peers.
fn workers(s: &Session) -> Vec<EntityId> {
    s.driver().world().lock().unwrap().entities(UNIT).to_vec()
}

/// P0's group order (tick 3): all three workers to a lowland rally point —
/// a box select's burst, three commands in ONE step. P1's (tick 7): its
/// three workers to a different rally.
fn burst(player: u64, units: &[EntityId]) -> Vec<Command> {
    if player == 0 {
        vec![
            move_cmd(units[0], 22, 24),
            move_cmd(units[1], 24, 24),
            move_cmd(units[2], 26, 22),
        ]
    } else {
        vec![
            move_cmd(units[3], 30, 26),
            move_cmd(units[4], 28, 26),
            move_cmd(units[5], 30, 28),
        ]
    }
}

#[test]
fn group_order_bursts_stay_in_sync() {
    let map = rts_map();
    let (ta, tb) = LoopbackTransport::pair();
    let mut a = session(P0, ta, &map);
    let mut b = session(P1, tb, &map);
    let us = workers(&a);
    assert_eq!(us.len(), 6, "both start squads exist before any input");

    for t in 0..400u64 {
        let cmds_a = if t == 3 { burst(0, &us) } else { Vec::new() };
        let cmds_b = if t == 7 { burst(1, &us) } else { Vec::new() };
        assert!(a.step(cmds_a).expect("peer A no desync"));
        assert!(b.step(cmds_b).expect("peer B no desync"));
        assert_eq!(
            a.driver().state_hash(),
            b.driver().state_hash(),
            "1v1 peers diverged at tick {t}"
        );
    }

    // Every unit of both squads marched off its plateau and settled at its
    // own rally slot — the whole burst was applied, in the same order, on
    // both peers.
    for (peer, s) in [("A", &a), ("B", &b)] {
        let w = s.driver().world().lock().unwrap();
        for (i, &(ex, ey)) in [(22, 24), (24, 24), (26, 22), (30, 26), (28, 26), (30, 28)]
            .iter()
            .enumerate()
        {
            let p = w.position(us[i]).expect("worker alive");
            let (px, py) = (p.x.to_f64(), p.y.to_f64());
            assert!(
                (px - f64::from(ex)).abs() < 0.5 && (py - f64::from(ey)).abs() < 0.5,
                "peer {peer}: worker {i} reached its rally (at {px:.2}, {py:.2}, wanted {ex}, {ey})"
            );
        }
    }
}

#[test]
fn replay_reproduces_the_bursts() {
    let map = rts_map();
    let (ta, tb) = LoopbackTransport::pair();
    let mut a = session(P0, ta, &map);
    let mut b = session(P1, tb, &map);
    let us = workers(&a);
    for t in 0..250u64 {
        let cmds_a = if t == 3 { burst(0, &us) } else { Vec::new() };
        let cmds_b = if t == 7 { burst(1, &us) } else { Vec::new() };
        a.step(cmds_a).expect("a");
        b.step(cmds_b).expect("b");
    }
    let final_hash = a.driver().state_hash();

    // A fresh driver replaying A's recorded stream (bursts and all) must
    // land on the same hash.
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let mut fresh =
        RhaiDriver::with_bridge(shared_world(SEED), map.entry_script().unwrap(), &bridge)
            .expect("compile rts map");
    if let monada_format::SimHz::Fixed(hz) = map.manifest.sim_hz {
        fresh.set_tick_hz(hz);
    }
    assert_eq!(
        a.replay().playback(&mut fresh),
        final_hash,
        "replay reproduces the 1v1 bursts"
    );
}
