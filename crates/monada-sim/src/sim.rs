//! The fixed-step driver contract.
//!
//! A [`Simulation`] is anything that advances one deterministic tick
//! at a time and can canonically hash its state. The wall-clock cadence
//! (fixed N Hz, or `on_command` for turn-based maps — DESIGN.md §3.1)
//! is the host's concern; the sim only knows about discrete ticks, so
//! replaying inputs is just re-running [`step`](Simulation::step).

/// A deterministic, tickable simulation.
pub trait Simulation {
    /// Advance exactly one tick. Must be a pure function of current
    /// state plus whatever inputs were folded in before the call.
    fn step(&mut self);

    /// The current tick counter.
    fn tick(&self) -> u64;

    /// Canonical state digest for desync detection (DESIGN.md §3.1).
    fn state_hash(&self) -> u64;
}

/// Advance a simulation by `ticks` steps.
pub fn advance<S: Simulation>(sim: &mut S, ticks: u64) {
    for _ in 0..ticks {
        sim.step();
    }
}
