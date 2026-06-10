//! M0 determinism gate (DESIGN.md §3.1, §7). The canonical scenario
//! must hash identically run-to-run, survive a snapshot round-trip,
//! and reach the committed golden digest.

use monada_sim::scenario::{CircleSim, CANONICAL_HASH_AT_600};
use monada_sim::{advance, Simulation};

const TICKS: u64 = 600;

#[test]
fn run_is_bit_identical_across_runs() {
    let mut a = CircleSim::canonical();
    let mut b = CircleSim::canonical();
    advance(&mut a, TICKS);
    advance(&mut b, TICKS);
    assert_eq!(a.state_hash(), b.state_hash());
    assert_eq!(a.tick(), TICKS);
}

#[test]
fn matches_committed_golden() {
    let mut sim = CircleSim::canonical();
    advance(&mut sim, TICKS);
    assert_eq!(
        sim.state_hash(),
        CANONICAL_HASH_AT_600,
        "state hash drifted from golden — a determinism regression \
         (or an intentional scenario change that needs the golden updated)"
    );
}

#[test]
fn snapshot_round_trip_then_resume_matches() {
    // Snapshot at tick 0, advance the original, then deserialize the
    // snapshot and advance it the same way — replay must converge.
    let sim0 = CircleSim::canonical();
    let snapshot = serde_json::to_string(&sim0).expect("serialize");

    let mut direct = sim0;
    advance(&mut direct, TICKS);

    let mut resumed: CircleSim = serde_json::from_str(&snapshot).expect("deserialize");
    advance(&mut resumed, TICKS);

    assert_eq!(direct.state_hash(), resumed.state_hash());
}

#[test]
fn hash_changes_as_the_world_evolves() {
    // Sanity: the hash actually tracks state — distinct ticks differ.
    let mut sim = CircleSim::canonical();
    let h0 = sim.state_hash();
    sim.step();
    let h1 = sim.state_hash();
    assert_ne!(h0, h1);
}
