//! Protocol-level lockstep tests (DESIGN.md §3.1, M3). Platform-
//! independent: they assert the *barrier*, *command-delay*, *desync
//! detection*, and *replay round-trip* behaviours over the in-process
//! [`LoopbackTransport`], with tiny stand-in [`SimDriver`]s — no rhai,
//! no world.

// Step indices are bounded (< 200) and only fold into command verbs; the
// `as u32` casts are deliberate and lossless in range.
#![allow(clippy::cast_possible_truncation)]

use monada_net::{
    Desync, InputBundle, LockstepSession, LoopbackTransport, MatchInfo, NetMessage, SessionConfig,
    SimDriver, Transport,
};
use monada_sim::{Command, PlayerId};

const P0: PlayerId = PlayerId(0);
const P1: PlayerId = PlayerId(1);

fn info() -> MatchInfo {
    MatchInfo {
        seed: 0x4D4F_4E41_4441_5F30,
        map_hash: 0xABCD,
        engine_version: "test".to_string(),
    }
}

/// Order- and tick-sensitive fold: any divergence in *which* commands
/// apply, in *what order*, on *which tick* shows up in the hash.
#[derive(Clone)]
struct FoldDriver {
    acc: u64,
    ticks: u64,
}

impl FoldDriver {
    fn new(salt: u64) -> FoldDriver {
        FoldDriver {
            acc: 0xcbf2_9ce4_8422_2325 ^ salt,
            ticks: 0,
        }
    }
}

impl SimDriver for FoldDriver {
    fn apply_command(&mut self, player: PlayerId, command: &Command) {
        // Fold in the current (about-to-execute) tick, player, and verb.
        self.acc ^= self
            .ticks
            .wrapping_mul(0x100)
            .wrapping_add(u64::from(player.0));
        self.acc = self.acc.wrapping_mul(0x0000_0100_0000_01b3);
        self.acc ^= u64::from(command.verb);
    }

    fn step(&mut self) {
        self.ticks += 1;
        self.acc = self.acc.wrapping_add(self.ticks);
    }

    fn state_hash(&self) -> u64 {
        self.acc ^ self.ticks.rotate_left(32)
    }
}

/// Records `(executing_tick, verb)` for every applied command — used to
/// prove command-delay offsets execution by exactly `command_delay`.
#[derive(Default)]
struct RecordDriver {
    ticks: u64,
    applied: Vec<(u64, u32)>,
}

impl SimDriver for RecordDriver {
    fn apply_command(&mut self, _player: PlayerId, command: &Command) {
        self.applied.push((self.ticks, command.verb));
    }
    fn step(&mut self) {
        self.ticks += 1;
    }
    fn state_hash(&self) -> u64 {
        self.ticks
    }
}

fn cmd(verb: u32) -> Command {
    Command::new(verb)
}

#[test]
fn two_sessions_stay_in_sync() {
    let (ta, tb) = LoopbackTransport::pair();
    let cfg = SessionConfig {
        command_delay: 2,
        checksum_interval: 7,
    };
    let mut a = LockstepSession::new(FoldDriver::new(0), ta, P0, &[P0, P1], cfg, info());
    let mut b = LockstepSession::new(FoldDriver::new(0), tb, P1, &[P0, P1], cfg, info());

    for s in 0..200u64 {
        // Each player issues a verb derived from the step; both clients
        // run the *same* program so equal hashes prove the inputs folded
        // identically on both sides.
        let ca = if s % 3 == 0 {
            vec![cmd(s as u32 + 1)]
        } else {
            vec![]
        };
        let cb = if s % 5 == 0 {
            vec![cmd(s as u32 + 100)]
        } else {
            vec![]
        };
        assert!(a.step(ca).expect("no desync"), "A should advance at {s}");
        assert!(b.step(cb).expect("no desync"), "B should advance at {s}");
        assert_eq!(
            a.driver().state_hash(),
            b.driver().state_hash(),
            "hash diverged after step {s}"
        );
    }
    assert_eq!(a.tick(), 200);
    assert_eq!(b.tick(), 200);
}

#[test]
fn barrier_stalls_without_peer_input() {
    // A alone: warmup ticks 0..delay execute, then it stalls waiting for
    // B's first real bundle.
    let (ta, _tb) = LoopbackTransport::pair();
    let cfg = SessionConfig {
        command_delay: 2,
        checksum_interval: 30,
    };
    let mut a = LockstepSession::new(RecordDriver::default(), ta, P0, &[P0, P1], cfg, info());

    assert!(a.step(vec![]).unwrap(), "tick 0 (warmup) runs");
    assert!(a.step(vec![]).unwrap(), "tick 1 (warmup) runs");
    assert!(
        !a.step(vec![]).unwrap(),
        "tick 2 must stall — no input from P1"
    );
    assert!(!a.step(vec![]).unwrap(), "still stalled");
    assert_eq!(a.tick(), 2, "did not advance past the barrier");
}

#[test]
fn command_delay_offsets_execution() {
    let (ta, tb) = LoopbackTransport::pair();
    let cfg = SessionConfig {
        command_delay: 3,
        checksum_interval: 30,
    };
    let mut a = LockstepSession::new(RecordDriver::default(), ta, P0, &[P0, P1], cfg, info());
    let mut b = LockstepSession::new(RecordDriver::default(), tb, P1, &[P0, P1], cfg, info());

    for s in 0..10u64 {
        // P0 issues verb 42 only at step 0; it should execute at tick 3.
        let ca = if s == 0 { vec![cmd(42)] } else { vec![] };
        a.step(ca).unwrap();
        b.step(vec![]).unwrap();
    }
    // Both clients applied verb 42 at exactly tick == command_delay.
    assert_eq!(a.driver().applied, vec![(3, 42)]);
    assert_eq!(b.driver().applied, vec![(3, 42)]);
}

#[test]
fn step_buffers_commands_across_a_stall() {
    // A command passed to `step` while stalled must be held and emitted on
    // the next executed tick — never dropped.
    let (ta, mut peer) = LoopbackTransport::pair();
    let cfg = SessionConfig {
        command_delay: 2,
        checksum_interval: 30,
    };
    let mut a = LockstepSession::new(RecordDriver::default(), ta, P0, &[P0, P1], cfg, info());

    // Warmup ticks 0,1 run without a peer.
    assert!(a.step(vec![]).unwrap());
    assert!(a.step(vec![]).unwrap());

    // Tick 2 needs P1's input — stall. Issue a command; it must be held,
    // not lost, even though the call returns `Ok(false)`.
    assert!(!a.step(vec![cmd(7)]).unwrap(), "stalled at the barrier");

    // Now deliver P1's (empty) bundles so A can advance past the stall.
    for tick in 2..6 {
        peer.send(NetMessage::Input(InputBundle {
            tick,
            player: P1,
            commands: vec![],
        }));
    }
    // Drain every newly-ready tick. The buffered command rides the first
    // executed tick (2) and so schedules for 2 + command_delay = 4.
    while a.step(vec![]).unwrap() {}

    assert_eq!(
        a.driver().applied,
        vec![(4, 7)],
        "buffered command must apply exactly once at tick 4, not be dropped"
    );
}

#[test]
fn desync_is_detected() {
    // Two clients whose sims disagree (different salt) must trip the
    // checksum exchange.
    let (ta, tb) = LoopbackTransport::pair();
    let cfg = SessionConfig {
        command_delay: 2,
        checksum_interval: 1, // probe every tick → fires immediately
    };
    let mut a = LockstepSession::new(FoldDriver::new(0), ta, P0, &[P0, P1], cfg, info());
    let mut b = LockstepSession::new(FoldDriver::new(999), tb, P1, &[P0, P1], cfg, info());

    a.step(vec![])
        .expect("A computes + sends its tick-0 checksum");
    let err: Desync = b
        .step(vec![])
        .expect_err("B must detect the hash mismatch at tick 0");
    assert_eq!(err.tick, 0);
    assert_eq!(err.peer, P0);
    assert_ne!(err.local, err.remote);
}

#[test]
fn replay_reproduces_final_hash() {
    let (ta, tb) = LoopbackTransport::pair();
    let cfg = SessionConfig {
        command_delay: 2,
        checksum_interval: 30,
    };
    let mut a = LockstepSession::new(FoldDriver::new(0), ta, P0, &[P0, P1], cfg, info());
    let mut b = LockstepSession::new(FoldDriver::new(0), tb, P1, &[P0, P1], cfg, info());

    for s in 0..120u64 {
        let ca = if s % 4 == 0 {
            vec![cmd(s as u32)]
        } else {
            vec![]
        };
        let cb = if s % 6 == 0 {
            vec![cmd(s as u32 + 7)]
        } else {
            vec![]
        };
        a.step(ca).unwrap();
        b.step(cb).unwrap();
    }
    let live_hash = a.driver().state_hash();

    // Re-run the recorded inputs through a fresh, identically-seeded
    // driver: bit-exact.
    let mut fresh = FoldDriver::new(0);
    let replay_hash = a.replay().playback(&mut fresh);
    assert_eq!(replay_hash, live_hash);

    // And the replay survives an encode/decode round-trip.
    let bytes = a.replay().encode().unwrap();
    let decoded = monada_net::Replay::decode(&bytes).unwrap();
    let mut fresh2 = FoldDriver::new(0);
    assert_eq!(decoded.playback(&mut fresh2), live_hash);
}

#[test]
fn checksum_bookkeeping_stays_bounded() {
    // Over a long match the checkpoint maps must be pruned as checkpoints
    // confirm — not grow one entry per checksum tick.
    let (ta, tb) = LoopbackTransport::pair();
    let cfg = SessionConfig {
        command_delay: 2,
        checksum_interval: 5,
    };
    let mut a = LockstepSession::new(FoldDriver::new(0), ta, P0, &[P0, P1], cfg, info());
    let mut b = LockstepSession::new(FoldDriver::new(0), tb, P1, &[P0, P1], cfg, info());

    for _ in 0..300u64 {
        a.step(vec![]).unwrap();
        b.step(vec![]).unwrap();
    }
    // 300 / 5 = 60 checksum ticks; unpruned that would be ~60 entries each.
    assert!(
        a.outstanding_checksums() <= 4,
        "a leaked: {}",
        a.outstanding_checksums()
    );
    assert!(
        b.outstanding_checksums() <= 4,
        "b leaked: {}",
        b.outstanding_checksums()
    );
}

#[test]
fn replay_verified_rejects_wrong_map_and_version() {
    let (ta, tb) = LoopbackTransport::pair();
    let cfg = SessionConfig::default();
    let mut a = LockstepSession::new(FoldDriver::new(0), ta, P0, &[P0, P1], cfg, info());
    let mut b = LockstepSession::new(FoldDriver::new(0), tb, P1, &[P0, P1], cfg, info());
    for _ in 0..10 {
        a.step(vec![]).unwrap();
        b.step(vec![]).unwrap();
    }
    // `info()` records map_hash 0xABCD, engine_version "test".
    let mut fresh = FoldDriver::new(0);
    assert!(matches!(
        a.replay()
            .playback_verified(&mut fresh, 0xDEAD, "test")
            .unwrap_err(),
        monada_net::ReplayError::MapMismatch { .. }
    ));
    let mut fresh = FoldDriver::new(0);
    assert!(matches!(
        a.replay()
            .playback_verified(&mut fresh, 0xABCD, "v999")
            .unwrap_err(),
        monada_net::ReplayError::VersionMismatch { .. }
    ));
    // Correct identity → plays back fine.
    let mut fresh = FoldDriver::new(0);
    assert!(a
        .replay()
        .playback_verified(&mut fresh, 0xABCD, "test")
        .is_ok());
}
