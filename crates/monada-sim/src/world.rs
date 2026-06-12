//! The generic, script-agnostic simulation world (DESIGN.md §3.3, §4.1).
//!
//! The engine ships no genre: it knows about *entities*, *archetypes*,
//! and *fields*, never about circles or chess. A [`World`] holds, per
//! script-declared [`ArchetypeId`], a struct-of-arrays of the entities
//! of that archetype — an ascending [`EntityId`] column, a mandatory
//! position, and one `Fixed` column per declared field. Gameplay state
//! lives here (not in the script), so it is canonically hashable for
//! desync detection, serde-serializable for snapshots, and queryable —
//! and the same world is what a future WASM backend reads (decision A2).
//!
//! `monada-script` is the only crate that turns script calls into
//! mutations of this world; the world itself has no notion of a script.

use std::collections::BTreeMap;

use monada_fixed::{Fixed, FixedVec3};

use crate::entity::{EntityAllocator, EntityId};
use crate::hash::{StateHash, StateHasher};
use crate::rng::DeterministicRng;

/// Identifies a registered archetype within a [`World`] (its index in
/// the world's archetype list).
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct ArchetypeId(pub u32);

/// SoA storage for one archetype: ascending ids, a position column, and
/// one `Fixed` column per declared field (parallel; same length).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct ArchetypeStore {
    /// Field names, in declaration order — the canonical iteration order
    /// for hashing and column access.
    field_names: Vec<String>,
    /// Ascending; index `i` owns row `i` of every column below.
    ids: Vec<EntityId>,
    position: Vec<FixedVec3>,
    /// `fields[f][i]` is row `i`'s value for declared field `f`.
    fields: Vec<Vec<Fixed>>,
}

impl ArchetypeStore {
    fn new(field_names: Vec<String>) -> ArchetypeStore {
        let fields = vec![Vec::new(); field_names.len()];
        ArchetypeStore {
            field_names,
            ids: Vec::new(),
            position: Vec::new(),
            fields,
        }
    }

    fn field_index(&self, name: &str) -> Option<usize> {
        self.field_names.iter().position(|n| n == name)
    }

    fn row_of(&self, id: EntityId) -> Option<usize> {
        self.ids.binary_search(&id).ok()
    }

    /// Append an entity with a zeroed position and zeroed fields. `id`
    /// must exceed every existing id (the monotonic allocator
    /// guarantees it), keeping the column ascending.
    fn spawn(&mut self, id: EntityId) {
        debug_assert!(
            self.ids.last().map_or(true, |&last| id > last),
            "ArchetypeStore::spawn: ids must be strictly ascending"
        );
        self.ids.push(id);
        self.position.push(FixedVec3::ZERO);
        for col in &mut self.fields {
            col.push(Fixed::ZERO);
        }
    }

    /// Order-preserving remove, keeping the ascending invariant.
    fn remove_row(&mut self, row: usize) {
        self.ids.remove(row);
        self.position.remove(row);
        for col in &mut self.fields {
            col.remove(row);
        }
    }
}

impl StateHash for ArchetypeStore {
    fn hash(&self, h: &mut StateHasher) {
        // Schema is part of the canonical state: a renamed/reordered
        // field set is a different world.
        h.write_u64(self.field_names.len() as u64);
        for name in &self.field_names {
            h.write_u64(name.len() as u64);
            h.write_bytes(name.as_bytes());
        }
        self.ids.hash(h);
        self.position.hash(h);
        for col in &self.fields {
            col.hash(h);
        }
    }
}

/// The deterministic simulation world. Logic-free: it stores entity
/// state and the seeded RNG; a [`crate::Simulation`] driver (a Rust rule
/// or `monada-script`) advances it.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct World {
    pub tick: u64,
    pub rng: DeterministicRng,
    alloc: EntityAllocator,
    archetypes: Vec<ArchetypeStore>,
    /// Entity -> owning archetype. `BTreeMap` (never `HashMap`) so any
    /// iteration is deterministic (DESIGN.md §3.1).
    entity_arch: BTreeMap<EntityId, u32>,
}

impl World {
    /// A fresh world seeded for its deterministic RNG.
    #[must_use]
    pub fn new(seed: u64) -> World {
        World {
            tick: 0,
            rng: DeterministicRng::seed_from_u64(seed),
            alloc: EntityAllocator::new(),
            archetypes: Vec::new(),
            entity_arch: BTreeMap::new(),
        }
    }

    /// Declare an archetype with the given `Fixed` field names; returns
    /// its id. Field order is fixed at declaration and defines the
    /// canonical column order.
    pub fn register_archetype(&mut self, fields: &[&str]) -> ArchetypeId {
        let id = ArchetypeId(self.archetypes.len() as u32);
        let names = fields.iter().map(|s| (*s).to_string()).collect();
        self.archetypes.push(ArchetypeStore::new(names));
        id
    }

    fn store(&self, a: ArchetypeId) -> Option<&ArchetypeStore> {
        self.archetypes.get(a.0 as usize)
    }

    fn store_mut(&mut self, a: ArchetypeId) -> Option<&mut ArchetypeStore> {
        self.archetypes.get_mut(a.0 as usize)
    }

    /// Spawn an entity of archetype `a` (zeroed position + fields).
    ///
    /// # Panics
    /// Panics if `a` is not a registered archetype.
    pub fn spawn(&mut self, a: ArchetypeId) -> EntityId {
        let id = self.alloc.alloc();
        self.store_mut(a).expect("unknown archetype").spawn(id);
        self.entity_arch.insert(id, a.0);
        id
    }

    /// Despawn `e`; returns whether it was present.
    pub fn despawn(&mut self, e: EntityId) -> bool {
        let Some(arch) = self.entity_arch.remove(&e) else {
            return false;
        };
        let store = &mut self.archetypes[arch as usize];
        if let Some(row) = store.row_of(e) {
            store.remove_row(row);
        }
        true
    }

    /// The archetype + row of `e`, if it exists.
    fn locate(&self, e: EntityId) -> Option<(usize, usize)> {
        let arch = *self.entity_arch.get(&e)? as usize;
        let row = self.archetypes[arch].row_of(e)?;
        Some((arch, row))
    }

    /// Set an entity's position.
    pub fn set_position(&mut self, e: EntityId, p: FixedVec3) -> bool {
        match self.locate(e) {
            Some((arch, row)) => {
                self.archetypes[arch].position[row] = p;
                true
            }
            None => false,
        }
    }

    /// Get an entity's position.
    #[must_use]
    pub fn position(&self, e: EntityId) -> Option<FixedVec3> {
        let (arch, row) = self.locate(e)?;
        Some(self.archetypes[arch].position[row])
    }

    /// Set a named `Fixed` field of `e`; returns whether the entity and
    /// field both exist.
    pub fn set_field(&mut self, e: EntityId, field: &str, value: Fixed) -> bool {
        let Some((arch, row)) = self.locate(e) else {
            return false;
        };
        let store = &mut self.archetypes[arch];
        match store.field_index(field) {
            Some(f) => {
                store.fields[f][row] = value;
                true
            }
            None => false,
        }
    }

    /// Get a named `Fixed` field of `e`.
    #[must_use]
    pub fn field(&self, e: EntityId, field: &str) -> Option<Fixed> {
        let (arch, row) = self.locate(e)?;
        let store = &self.archetypes[arch];
        let f = store.field_index(field)?;
        Some(store.fields[f][row])
    }

    /// Ascending ids of every live entity of archetype `a`.
    #[must_use]
    pub fn entities(&self, a: ArchetypeId) -> &[EntityId] {
        self.store(a).map_or(&[], |s| &s.ids)
    }

    /// Positions of every live entity of archetype `a`, parallel to
    /// [`entities`](Self::entities). Handy for the render bridge.
    #[must_use]
    pub fn positions(&self, a: ArchetypeId) -> &[FixedVec3] {
        self.store(a).map_or(&[], |s| &s.position)
    }

    /// Number of live entities of archetype `a`.
    #[must_use]
    pub fn count(&self, a: ArchetypeId) -> usize {
        self.store(a).map_or(0, |s| s.ids.len())
    }

    /// Every live entity id, ascending, across all archetypes. The
    /// canonical iteration order for scripts (deterministic, since the
    /// backing map is a `BTreeMap`).
    #[must_use]
    pub fn all_entities(&self) -> Vec<EntityId> {
        self.entity_arch.keys().copied().collect()
    }

    /// Canonical state digest: tick, RNG, allocator, then every
    /// archetype store in id order (DESIGN.md §3.1).
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = StateHasher::new();
        h.write_u64(self.tick);
        self.rng.hash(&mut h);
        self.alloc.hash(&mut h);
        h.write_u64(self.archetypes.len() as u64);
        for store in &self.archetypes {
            store.hash(&mut h);
        }
        h.finish()
    }
}
