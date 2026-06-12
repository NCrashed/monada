//! The CI determinism gate. If this fails on any platform, a scenario
//! has diverged (or it changed intentionally and the goldens need a
//! deliberate `--bless`).

use monada_oracle::{all_checkpoints, diff, parse_goldens, walk_final_hash, Verdict};

/// The committed goldens, embedded at compile time so the test always
/// checks exactly what is on disk.
const COMMITTED: &str = include_str!("../../../monada-hashes.txt");

#[test]
fn checkpoints_match_committed_goldens() {
    let checkpoints = all_checkpoints();
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
fn walk_final_matches_checkpoint() {
    // The standalone runner and the checkpoint walk must agree on the
    // headline scripted golden (walk@600).
    let final_cp = all_checkpoints()
        .into_iter()
        .find(|c| c.scenario == "walk" && c.tick == 600)
        .expect("walk@600 checkpoint");
    assert_eq!(final_cp.hash, walk_final_hash());
}
