//! The physics half of a volume map's simulation state
//! (docs/plans/digger-demo.md §1b).
//!
//! A `terrain = "volume"` map embeds a [`PhysicsWorld`] and a
//! [`VolumeStore`] beside the entity [`World`](monada_sim::World): one
//! sim, one combined digest, one desync stream. This is the shared
//! handle a driver steps and hashes.
//!
//! It lives in the runtime rather than beside a language's registration
//! for the same reason the terrain store does: what a body collides with
//! is simulation state, and both backends need the identical answer
//! (docs/plans/desert-game.md §3a, decision L7). The `phys_*` verbs that
//! expose it to Rhai stay in `monada-script`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use monada_fixed::FixedVec3;
use monada_physics::PhysicsWorld;
use monada_sim::{StateHash, StateHasher};

use crate::VolumeStore;
/// A body's drill-tool geometry (plan §1c): set once at vehicle spawn
/// via `phys_drill_tool`, in BODY coordinates (the shape→body rebase
/// happens at registration, like wheel anchors). Pitch is the per-call
/// degree of freedom of `phys_drill`, not stored here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DrillToolDef {
    pub anchor: FixedVec3,
    pub half_extents: FixedVec3,
}

/// The physics side of a volume map's sim state: the rigid-body world,
/// the terrain it collides with, and the per-body drill tools. Snapshots
/// serialize it whole; the driver folds its digests into the combined
/// `state_hash`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhysicsSim {
    pub world: PhysicsWorld,
    pub terrain: VolumeStore,
    /// Drill tools by body id (D3). Sim state like everything else here:
    /// registered deterministically, serialized, and folded into the
    /// driver hash via [`tools_hash`](PhysicsSim::tools_hash).
    pub tools: BTreeMap<u64, DrillToolDef>,
}

impl PhysicsSim {
    /// Canonical digest of the drill-tool table (count, then per entry:
    /// body id, anchor, half-extents, in `BTreeMap` order).
    #[must_use]
    pub fn tools_hash(&self) -> u64 {
        let mut h = StateHasher::new();
        h.write_u64(self.tools.len() as u64);
        for (&body, def) in &self.tools {
            h.write_u64(body);
            def.anchor.hash(&mut h);
            def.half_extents.hash(&mut h);
        }
        h.finish()
    }
}

/// The shared physics handle — `Arc<Mutex<…>>` for the same reason as
/// [`SharedWorld`](crate::SharedWorld): `sync`-feature Rhai needs
/// `Send + Sync` host closures, and the single-threaded sim never
/// contends the lock.
pub type SharedPhysics = Arc<Mutex<PhysicsSim>>;

/// A fresh shared physics sim stepped at `sim_hz` (the manifest's fixed
/// tick rate — physics `dt` must equal the sim tick). Gravity starts at
/// the physics crate's opt-in ZERO; the map script owns feel constants
/// and sets it via `phys_gravity` (plan §1f).
#[must_use]
pub fn shared_physics(sim_hz: u32) -> SharedPhysics {
    Arc::new(Mutex::new(PhysicsSim {
        world: PhysicsWorld::new(sim_hz),
        terrain: VolumeStore::new(),
        tools: BTreeMap::new(),
    }))
}
