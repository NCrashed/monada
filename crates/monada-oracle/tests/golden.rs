//! The CI determinism gate. If this test fails on any platform, the
//! sim has diverged (or the scenario changed and the goldens need a
//! deliberate `--bless`).

use monada_oracle::{canonical_checkpoints, diff, parse_goldens, Verdict};
use monada_sim::scenario::CANONICAL_HASH_AT_600;

/// The committed goldens, embedded at compile time so the test always
/// checks exactly what is on disk.
const COMMITTED: &str = include_str!("../../../monada-hashes.txt");

#[test]
fn checkpoints_match_committed_goldens() {
    let checkpoints = canonical_checkpoints();
    let goldens = parse_goldens(COMMITTED).expect("goldens file parses");
    for (cp, verdict) in diff(&checkpoints, &goldens) {
        assert_eq!(
            verdict,
            Verdict::Match,
            "checkpoint {} diverged from committed golden — determinism regression \
             (or re-bless if the scenario changed intentionally)",
            cp.key()
        );
    }
}

#[test]
fn final_checkpoint_matches_sim_constant() {
    // The oracle and the sim crate must agree on the headline golden.
    let final_cp = canonical_checkpoints()
        .into_iter()
        .last()
        .expect("at least one checkpoint");
    assert_eq!(final_cp.tick, 600);
    assert_eq!(final_cp.hash, CANONICAL_HASH_AT_600);
}
