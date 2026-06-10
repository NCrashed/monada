//! Hand-rolled struct-of-arrays storage, one per archetype
//! (DESIGN.md §4.1).
//!
//! No ECS library: a [`Columns`] implementor *is* the SoA — one
//! `Vec<T>` per component — and [`ArchetypeStorage`] wraps it with a
//! parallel, **always-ascending** `Vec<EntityId>`. Because ids are
//! monotonic ([`crate::EntityAllocator`]) and spawns only ever append,
//! the id column stays sorted with zero bookkeeping; despawn is an
//! order-preserving remove so the invariant survives. Iteration and
//! hashing are then a single fixed walk in id order — no query
//! language, no scheduler, no hash-iteration seam to fence.

use crate::entity::EntityId;
use crate::hash::{StateHash, StateHasher};

/// The struct-of-arrays component columns for one archetype.
///
/// Implementors hold a `Vec<T>` per component and keep them the same
/// length.
///
/// # Invariant — and how it is protected
///
/// Every column must stay the same length as the storage's id column,
/// in the same order. Two rules keep that true:
///
/// 1. **Keep the `Vec` columns private.** A public `Vec` field would
///    let any caller `push`/`remove`/reorder one column out from under
///    the ids, silently corrupting `binary_search` and the canonical
///    hash with no panic until much later. Expose `&[T]` / `&mut [T]`
///    *slice* accessors instead: callers can mutate values but cannot
///    change length or structure through a slice.
/// 2. **Length only ever changes via the storage.**
///    [`push_row`](Columns::push_row) / [`remove_at`](Columns::remove_at)
///    are storage plumbing, driven exclusively by
///    [`ArchetypeStorage::spawn`] / [`despawn`](ArchetypeStorage::despawn)
///    so the id column moves in lockstep. Gameplay code never calls
///    them directly — it spawns/despawns, and mutates values through
///    the concrete columns' slice accessors. The storage debug-asserts
///    the length invariant after each structural op.
pub trait Columns: Default {
    /// The per-entity component bundle accepted by `push_row`. A named
    /// struct is strongly preferred over a positional tuple: this trait
    /// is the eventual codegen target for script-declared archetypes,
    /// and a tuple of same-typed fields is trivial to misorder.
    type Row;

    /// **Storage plumbing — call [`ArchetypeStorage::spawn`] instead.**
    /// Append one entity's components across all columns.
    fn push_row(&mut self, row: Self::Row);

    /// **Storage plumbing — call [`ArchetypeStorage::despawn`] instead.**
    /// Remove the entity at `index` from every column, **preserving
    /// order** (i.e. `Vec::remove`, not `swap_remove`).
    fn remove_at(&mut self, index: usize);

    /// Number of rows (must equal every column's length).
    fn len(&self) -> usize;

    /// Whether the storage holds no entities.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Per-archetype storage: the SoA columns plus an ascending id column.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ArchetypeStorage<C: Columns> {
    /// Ascending, parallel to `columns`. Index `i` belongs to entity
    /// `ids[i]`.
    ids: Vec<EntityId>,
    columns: C,
}

impl<C: Columns> ArchetypeStorage<C> {
    #[must_use]
    pub fn new() -> ArchetypeStorage<C> {
        ArchetypeStorage {
            ids: Vec::new(),
            columns: C::default(),
        }
    }

    /// Number of live entities.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether the storage is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The ascending id column. `ids()[i]` owns column index `i`.
    #[must_use]
    pub fn ids(&self) -> &[EntityId] {
        &self.ids
    }

    /// Shared access to the SoA columns.
    #[must_use]
    pub fn columns(&self) -> &C {
        &self.columns
    }

    /// Mutable access to the SoA columns for per-tick **value** updates.
    ///
    /// The concrete columns type only exposes slice accessors through
    /// this, so values can change but lengths cannot (see [`Columns`]).
    /// Structural changes go through [`spawn`](Self::spawn) /
    /// [`despawn`](Self::despawn), never here.
    pub fn columns_mut(&mut self) -> &mut C {
        &mut self.columns
    }

    /// Spawn an entity. `id` must exceed every existing id, which the
    /// monotonic allocator guarantees; this keeps the id column sorted.
    ///
    /// # Panics
    /// Panics (debug-assert) if `id` would break the ascending
    /// invariant — that can only happen if ids are not coming from a
    /// single [`crate::EntityAllocator`].
    pub fn spawn(&mut self, id: EntityId, row: C::Row) {
        debug_assert!(
            self.ids.last().map_or(true, |&last| id > last),
            "ArchetypeStorage::spawn: ids must be strictly ascending"
        );
        self.ids.push(id);
        self.columns.push_row(row);
        debug_assert_eq!(
            self.ids.len(),
            self.columns.len(),
            "ArchetypeStorage::spawn: column count diverged from id count"
        );
    }

    /// The column index of `id`, via binary search on the sorted column.
    #[must_use]
    pub fn index_of(&self, id: EntityId) -> Option<usize> {
        self.ids.binary_search(&id).ok()
    }

    /// Despawn `id`, returning whether it was present. Order-preserving
    /// so the ascending invariant holds.
    pub fn despawn(&mut self, id: EntityId) -> bool {
        match self.ids.binary_search(&id) {
            Ok(index) => {
                self.ids.remove(index);
                self.columns.remove_at(index);
                debug_assert_eq!(
                    self.ids.len(),
                    self.columns.len(),
                    "ArchetypeStorage::despawn: column count diverged from id count"
                );
                true
            }
            Err(_) => false,
        }
    }
}

impl<C: Columns> Default for ArchetypeStorage<C> {
    fn default() -> ArchetypeStorage<C> {
        ArchetypeStorage::new()
    }
}

impl<C: Columns + StateHash> StateHash for ArchetypeStorage<C> {
    /// Canonical: id column first (length-prefixed), then the columns
    /// in their fixed declaration order.
    fn hash(&self, h: &mut StateHasher) {
        self.ids.hash(h);
        self.columns.hash(h);
    }
}
