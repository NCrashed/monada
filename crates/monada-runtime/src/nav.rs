//! The engine's side of three-dimensional navigation: the stand-graph
//! caches and their invalidation (docs/plans/desert-game.md §4c).
//!
//! `monada-nav` owns the search and knows nothing about monada; this
//! module is the half that has to live beside the terrain, for one
//! reason: **whatever is derived from the ground must be invalidated by
//! whoever changes the ground.** The runtime owns the volume store, so a
//! paint can drop the affected columns itself. Leaving that to the map
//! would make every terraforming verb a place to forget it, and a stale
//! stand is not a visible bug — it is a unit walking confidently through
//! a wall that was raised two seconds ago.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use monada_nav::{MoverProfile, NavVolume, VolumeWorld};

use crate::VolumeStore;

/// The volume terrain, as something `monada-nav` can plan against. The
/// store answers materials; the search only asks "is anything there".
impl VolumeWorld for VolumeStore {
    fn solid(&self, x: i64, y: i64, z: i64) -> bool {
        self.get(x, y, z).is_some()
    }
}

/// A profile as a map key. [`MoverProfile`] is deliberately not `Ord` —
/// it is a description, not an identifier — so the ordering lives here,
/// where it is only ever a lookup.
type ProfileKey = (i64, i64, bool);

fn key(p: MoverProfile) -> ProfileKey {
    (p.height, p.max_step, p.tunnels)
}

/// One stand graph per mover profile, plus the invalidation both share.
#[derive(Default)]
pub struct NavCache {
    volumes: BTreeMap<ProfileKey, NavVolume>,
}

impl NavCache {
    #[must_use]
    pub fn new() -> NavCache {
        NavCache::default()
    }

    /// The graph for this profile, created on first use. Separate graphs
    /// because what counts as a stand depends on the clearance asked for:
    /// one shared cache would answer a harvester's question with
    /// infantry's ground.
    pub fn for_profile(&mut self, profile: MoverProfile) -> &mut NavVolume {
        self.volumes
            .entry(key(profile))
            .or_insert_with(|| NavVolume::new(profile))
    }

    /// Drop the cached stands of every column in the inclusive box, in
    /// **every** profile's graph — a berm is a berm to infantry and to
    /// armour alike.
    pub fn invalidate(&mut self, lo: (i64, i64), hi: (i64, i64)) {
        for volume in self.volumes.values_mut() {
            volume.invalidate(lo, hi);
        }
    }

    /// Total cached columns across profiles — for tests and for a host
    /// that would rather watch the cache than guess at it.
    #[must_use]
    pub fn cached_columns(&self) -> usize {
        self.volumes.values().map(NavVolume::cached_columns).sum()
    }
}

/// The shared handle, held beside the world and the terrain for the same
/// `Send + Sync` reason as everything else here.
pub type SharedNav = Arc<Mutex<NavCache>>;

/// A fresh shared cache.
#[must_use]
pub fn shared_nav() -> SharedNav {
    Arc::new(Mutex::new(NavCache::new()))
}
