//! Sand that will not stand in a wall (docs/plans/desert-game.md §4d).
//!
//! Without this, terraforming is voxel Lego: a Dweller cuts a vertical
//! trench and it stays vertical forever, a Surfling raises a berm with
//! sheer sides, and an explosion leaves a crater with a lip you could
//! park on. The desert's three factions all shape loose material, so the
//! material has to answer back — a slope steeper than sand can hold
//! collapses, which is what gives a trench a cost, a berm a footprint,
//! and a crater a shape.
//!
//! **Where it lives.** §14.2 left this open between `monada-physics` and
//! the rules crate; it is neither. Physics cannot see [`VolumeStore`] —
//! the dependency runs the other way — and the rules crate is the wrong
//! home for something every future map wants the moment it has gravel or
//! snow. So the automaton is the runtime's, beside the terrain it
//! reshapes, and *when* to run it and *how much* is the map's call
//! through the terraform budget (§4e). Engine owns the rule; map owns the
//! pacing.
//!
//! **Determinism.** Integer throughout, a canonical sweep over a sorted
//! dirty set, a fixed neighbour order, and a per-call budget so the work
//! is bounded whatever the map does. Quiet terrain costs nothing: no
//! dirty cells, no sweep.

use std::collections::{BTreeMap, BTreeSet};

use monada_physics::MaterialId;
use monada_sim::{StateHash, StateHasher};

use crate::VolumeStore;

/// How a granular material behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Repose {
    /// The steepest drop a column of this material will hold against a
    /// neighbour, in cells. Sand is 1 — anything steeper slides. Gravel
    /// would be 2, a packed fill is not granular at all.
    pub max_drop: i64,
}

/// The order neighbours are examined in, and the order a tie is broken
/// by: E, N, W, S. Part of the determinism contract — do not reorder.
const NEIGHBOURS: [(i64, i64); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];

/// The settling automaton: which materials flow, and where the terrain
/// has been disturbed since it last settled.
#[derive(Default, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Granular {
    materials: BTreeMap<u16, Repose>,
    /// Columns to examine. A *column*, not a cell: what slides is the
    /// top of a stack, so the question is always "what is the surface
    /// here" and the z is found rather than remembered.
    dirty: BTreeSet<(i64, i64)>,
}

impl Granular {
    #[must_use]
    pub fn new() -> Granular {
        Granular::default()
    }

    /// Declare a material granular. A material never declared is stable
    /// at any slope — rock, packed fill, glass.
    pub fn register(&mut self, material: MaterialId, repose: Repose) {
        self.materials.insert(material.0, repose);
    }

    /// Whether anything has been declared granular yet.
    #[must_use]
    pub fn is_inert(&self) -> bool {
        self.materials.is_empty() && self.dirty.is_empty()
    }

    /// How many columns are waiting to settle — the map's cue that a
    /// slope is still moving.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.dirty.len()
    }

    /// Mark the columns an edit touched, plus the ring around them: a
    /// column does not slide because *it* changed, it slides because its
    /// neighbour did.
    pub fn disturb(&mut self, lo: (i64, i64), hi: (i64, i64)) {
        if self.materials.is_empty() {
            return; // nothing flows; do not accumulate work for no one
        }
        for y in (lo.1 - 1)..=(hi.1 + 1) {
            for x in (lo.0 - 1)..=(hi.0 + 1) {
                self.dirty.insert((x, y));
            }
        }
    }

    /// Let the terrain settle, moving at most `budget` cells.
    ///
    /// Returns how many moved. A pile does not finish in one call and is
    /// not meant to: the remaining columns stay dirty, so a slope keeps
    /// slumping over the following ticks — bounded work per tick, and a
    /// collapse the player can watch happen.
    pub fn settle(&mut self, store: &mut VolumeStore, budget: u32) -> u32 {
        if self.materials.is_empty() || budget == 0 {
            return 0;
        }
        let mut moved = 0;
        // Take the whole set and re-add what still needs work: iterating
        // a set while inserting into it is neither borrowable nor
        // canonical, and "what was dirty when the sweep began" is the
        // honest unit of work.
        let pending = std::mem::take(&mut self.dirty);
        let mut deferred = BTreeSet::new();
        for (x, y) in pending {
            if moved >= budget {
                // Out of budget: the rest is next tick's business.
                deferred.insert((x, y));
                continue;
            }
            if self.slide_one(store, x, y) {
                moved += 1;
                // The column that lost a cell and the one that gained it
                // are both unsettled now, along with their rings.
                deferred.insert((x, y));
                for (dx, dy) in NEIGHBOURS {
                    deferred.insert((x + dx, y + dy));
                }
            }
        }
        self.dirty.extend(deferred);
        moved
    }

    /// Move one cell off the top of a column, if it is standing steeper
    /// than its material allows.
    fn slide_one(&self, store: &mut VolumeStore, x: i64, y: i64) -> bool {
        let Some((z, material)) = surface(store, x, y) else {
            return false;
        };
        let Some(&repose) = self.materials.get(&material) else {
            return false; // rock does not flow
        };

        // The lowest neighbour that is too far below, ties broken by the
        // fixed neighbour order — so a symmetric pile collapses the same
        // way on every machine rather than whichever way the iteration
        // happened to go.
        let mut best: Option<(i64, i64, i64)> = None;
        for (dx, dy) in NEIGHBOURS {
            let (nx, ny) = (x + dx, y + dy);
            let nz = surface(store, nx, ny).map_or(i64::MIN / 4, |(z, _)| z);
            if z - nz > repose.max_drop && best.map_or(true, |(bz, _, _)| nz < bz) {
                best = Some((nz, nx, ny));
            }
        }
        let Some((nz, nx, ny)) = best else {
            return false;
        };

        store.clear(x, y, z);
        store.set(nx, ny, nz + 1, MaterialId(material));
        true
    }
}

/// The topmost solid cell of a column and what it is made of.
///
/// Bounded by the material a search would have to scan: the store is
/// unbounded in z, so the walk starts from the column's own contents
/// rather than from a fixed sky.
fn surface(store: &VolumeStore, x: i64, y: i64) -> Option<(i64, u16)> {
    store.column_top(x, y)
}

impl StateHash for Granular {
    fn hash(&self, h: &mut StateHasher) {
        // An inert automaton contributes nothing, so a map that declares
        // no granular material hashes exactly as it did before this
        // existed — the same canonical-form rule the store applies to an
        // empty chunk.
        if self.is_inert() {
            return;
        }
        h.write_u64(self.materials.len() as u64);
        for (&mat, repose) in &self.materials {
            h.write_u64(u64::from(mat));
            h.write_i64(repose.max_drop);
        }
        h.write_u64(self.dirty.len() as u64);
        for &(x, y) in &self.dirty {
            h.write_i64(x);
            h.write_i64(y);
        }
    }
}

