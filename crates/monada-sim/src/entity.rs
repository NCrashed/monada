//! Stable entity identity (DESIGN.md §3.1, §4.1).
//!
//! Ids are monotonically allocated `u64`s and **never reused**. A
//! 64-bit counter cannot realistically exhaust (2^64 spawns), so we
//! skip generational indices entirely — which keeps the per-archetype
//! storage sorted by id forever (new ids only ever append). Sorted-by-
//! id is the determinism invariant that lets iteration and hashing be a
//! single fixed walk, not an arena-index- or hash-order-dependent one.

use crate::hash::{StateHash, StateHasher};

/// A stable, never-reused entity identifier.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct EntityId(pub u64);

impl StateHash for EntityId {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(self.0);
    }
}

/// Monotonic id allocator. One per world; part of the serialized
/// snapshot so a resumed replay keeps allocating where it left off.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct EntityAllocator {
    next: u64,
}

impl EntityAllocator {
    #[must_use]
    pub fn new() -> EntityAllocator {
        EntityAllocator { next: 0 }
    }

    /// Allocate the next id. Strictly increasing across the world's
    /// lifetime.
    pub fn alloc(&mut self) -> EntityId {
        let id = EntityId(self.next);
        self.next += 1;
        id
    }

    /// How many ids have been handed out.
    #[must_use]
    pub fn issued(&self) -> u64 {
        self.next
    }
}

impl Default for EntityAllocator {
    fn default() -> EntityAllocator {
        EntityAllocator::new()
    }
}

impl StateHash for EntityAllocator {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(self.next);
    }
}
