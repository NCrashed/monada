//! QUIC transport integration test (DESIGN.md §3.1, M3 Phase B).
//!
//! Drives two *real* [`QuicTransport`]s over loopback UDP — the same
//! quinn path two `monada-host` processes use — through lockstep sessions
//! and asserts they stay in sync and the replay reproduces. Gated on the
//! `quic` feature; run with `cargo test -p monada-net --features quic`.
#![cfg(feature = "quic")]

use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};

use monada_net::{LockstepSession, MatchInfo, QuicTransport, SessionConfig, SimDriver, Transport};
use monada_sim::{Command, PlayerId};

const P0: PlayerId = PlayerId(0);
const P1: PlayerId = PlayerId(1);
/// Ticks to drive the two QUIC peers through.
const TARGET: u64 = 40;

/// Order/tick-sensitive fold (same idea as the loopback protocol tests).
struct FoldDriver {
    acc: u64,
    ticks: u64,
}

impl FoldDriver {
    fn new() -> FoldDriver {
        FoldDriver {
            acc: 0xcbf2_9ce4_8422_2325,
            ticks: 0,
        }
    }
}

impl SimDriver for FoldDriver {
    fn apply_command(&mut self, player: PlayerId, command: &Command) {
        self.acc ^= self.ticks.wrapping_add(u64::from(player.0));
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

fn info() -> MatchInfo {
    MatchInfo {
        seed: 1,
        map_hash: [2; 32],
        engine_version: "test".to_string(),
    }
}

fn make_session<T: Transport>(
    driver: FoldDriver,
    transport: T,
    player: PlayerId,
) -> LockstepSession<T, FoldDriver> {
    LockstepSession::new(
        driver,
        transport,
        player,
        &[P0, P1],
        SessionConfig {
            command_delay: 2,
            checksum_interval: 8,
        },
        info(),
    )
}

#[test]
fn two_quic_sessions_stay_in_sync() {
    let addr: SocketAddr = "127.0.0.1:54021".parse().unwrap();

    // Both constructors block until the link is up; run them on threads so
    // they rendezvous regardless of start order (connect retries).
    let server_t = thread::spawn(move || QuicTransport::listen(addr));
    let client_t = thread::spawn(move || QuicTransport::connect(addr));
    let server = server_t.join().unwrap().expect("listen");
    let client = client_t.join().unwrap().expect("connect");

    let mut a = make_session(FoldDriver::new(), server, P0);
    let mut b = make_session(FoldDriver::new(), client, P1);

    let deadline = Instant::now() + Duration::from_secs(20);
    while a.tick() < TARGET || b.tick() < TARGET {
        // Each player issues a step-derived verb so the fold is non-trivial.
        let ca = if a.tick() % 3 == 0 {
            vec![Command::new(u32::try_from(a.tick()).unwrap() + 1)]
        } else {
            vec![]
        };
        let cb = if b.tick() % 4 == 0 {
            vec![Command::new(u32::try_from(b.tick()).unwrap() + 100)]
        } else {
            vec![]
        };
        let pa = a.step(ca).expect("no desync (a)");
        let pb = b.step(cb).expect("no desync (b)");
        if !pa && !pb {
            assert!(
                Instant::now() < deadline,
                "timed out waiting on the network"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    assert_eq!(a.tick(), TARGET);
    assert_eq!(b.tick(), TARGET);
    assert_eq!(
        a.driver().state_hash(),
        b.driver().state_hash(),
        "QUIC peers diverged"
    );

    // Replay of A reproduces A's final state bit-exactly.
    let final_hash = a.driver().state_hash();
    let mut fresh = FoldDriver::new();
    assert_eq!(a.replay().playback(&mut fresh), final_hash);
}
