//! [`PhysicsWorld`] — the physics state root (docs/plans/voxel-physics.md
//! §3 `world`).
//!
//! P0 scope: the deterministic shell only — tick counter, fixed
//! timestep, canonical hash, serde snapshot. Bodies, contacts, and the
//! solver arrive in P1+; the shape here is what the oracle golden and
//! the snapshot property gate from day one.

use monada_fixed::Fixed;
use monada_sim::{StateHash, StateHasher};

/// The physics state root. One instance per sim; stepped once per sim
/// tick, hashed into the same desync digest as the rest of the sim.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhysicsWorld {
    /// Ticks stepped since construction. Part of the hashed state so a
    /// paused world and a stepped-but-empty world cannot collide.
    tick: u64,
    /// The fixed timestep, `1 / sim_hz` (docs/plans/voxel-physics.md:
    /// no `dt` parameter anywhere downstream of construction).
    dt: Fixed,
}

impl PhysicsWorld {
    /// A world stepping at `sim_hz` ticks per second (the map's
    /// `sim_hz`; the engine default is 25).
    ///
    /// # Panics
    /// Panics if `sim_hz` is zero.
    #[must_use]
    pub fn new(sim_hz: u32) -> PhysicsWorld {
        assert!(sim_hz > 0, "PhysicsWorld::new: sim_hz must be non-zero");
        PhysicsWorld {
            tick: 0,
            // Lossless: manifest sim_hz is far below i32::MAX.
            dt: Fixed::from_ratio(1, i32::try_from(sim_hz).expect("sim_hz fits i32")),
        }
    }

    /// Ticks stepped since construction.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The fixed timestep in seconds.
    #[must_use]
    pub fn dt(&self) -> Fixed {
        self.dt
    }

    /// Advance one fixed tick. P0: no bodies yet, so only the tick
    /// counter moves — but the call is already the one seam the engine
    /// drives, and the goldens hash across it.
    pub fn step(&mut self) {
        self.tick += 1;
    }

    /// The canonical FNV-1a digest of the full physics state, for the
    /// per-tick desync exchange and the oracle goldens.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = StateHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

impl StateHash for PhysicsWorld {
    /// Canonical fold order: `tick`, then `dt`. Any change here —
    /// including appending a field — re-keys every `phys@` golden and
    /// requires an explicit bless; keeping the order append-only just
    /// makes the diff-time story legible (old fields keep their
    /// positions, the bless commit points at exactly what grew).
    fn hash(&self, h: &mut StateHasher) {
        self.tick.hash(h);
        self.dt.hash(h);
    }
}
