//! M2 determinism gate: the scripted walk-in-circle scenario must hash
//! identically run-to-run and reach a committed golden. This is the
//! scripting analogue of the M0 sim-kernel gate — it covers the whole
//! Rhai path (compile, host API, fixed-point trig, deterministic RNG).

use monada_script::{run_script, WALK_CIRCLE_SCRIPT};

/// Seed + tick count for the canonical scripted scenario.
const SEED: u64 = 0x4D4F_4E41_4441_5F30; // "MONADA_0"
const TICKS: u64 = 600;

/// Golden `World::state_hash` of the scripted walk-circle after 600
/// ticks. Bump only alongside a deliberate change to the scenario or the
/// world layout.
const GOLDEN: u64 = 7_227_763_778_376_693_000;

fn run() -> u64 {
    let world = run_script(SEED, WALK_CIRCLE_SCRIPT, TICKS).expect("script runs");
    let hash = world.lock().unwrap().state_hash();
    hash
}

#[test]
fn scripted_scenario_is_bit_identical_across_runs() {
    assert_eq!(run(), run());
}

#[test]
fn scripted_scenario_matches_golden() {
    assert_eq!(
        run(),
        GOLDEN,
        "scripted walk-circle hash drifted — determinism regression, or \
         re-bless if the scenario/world layout changed intentionally"
    );
}
