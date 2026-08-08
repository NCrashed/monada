//! [`VolumeStore`] — the chunked 3D voxel terrain store behind
//! `terrain = "volume"` maps (docs/plans/digger-demo.md §1a).
//!
//! The column heightmap ([`VoxelStore`](crate::VoxelStore)) cannot
//! represent overhangs or holes, so maps built around tunnels opt into
//! this store instead: a dense material id per voxel, in 16³ chunks.
//! Unlike the column store — a render/bridge-side pure function of the
//! script's paint calls, never hashed — the volume store is **sim
//! state**: physics reads it as its [`VoxelField`] terrain, edits feed
//! [`notify_terrain_edit`](monada_physics::PhysicsWorld::notify_terrain_edit),
//! and its digest folds into the driver's combined `state_hash`.
//!
//! Hashing: each chunk carries an XOR of per-cell digests, updated
//! INCREMENTALLY by every write, so the store hashes in O(chunks) and a
//! single write costs O(1) instead of a fold over the chunk's 8 KB. A chunk whose last solid voxel is
//! cleared is dropped from the map entirely, so "filled then emptied"
//! and "never touched" are the same canonical form (and the same hash).

use std::collections::{BTreeMap, BTreeSet};

use monada_physics::{MaterialId, VoxelField};
use monada_sim::{StateHash, StateHasher};

/// The empty-cell sentinel — the same encoding as
/// [`VoxelShape`](monada_physics::VoxelShape) (`u16::MAX`), so material
/// ids flow between terrain and bodies without translation.
pub const EMPTY: u16 = u16::MAX;

/// Chunk edge length in voxels.
const CHUNK_EDGE: i64 = 16;
/// Cells per chunk (16³).
const CHUNK_CELLS: usize = 4096;

/// One dense 16³ chunk. Row-major `x + 16·(y + 16·z)` over chunk-local
/// coordinates.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Chunk {
    #[serde(with = "chunk_cells_serde")]
    cells: Box<[u16; CHUNK_CELLS]>,
    /// Cached FNV of `cells`, refreshed on edit. Serialized (like the
    /// physics crate's derived caches) so snapshot → restore → hash is
    /// bit-equal by construction; the store digest folds this, never
    /// the cells.
    hash: u64,
    /// Non-empty cell count — the "drop this chunk" tripwire. Derived,
    /// serialized, never hashed.
    filled: u32,
}

impl Chunk {
    fn empty() -> Chunk {
        let mut chunk = Chunk {
            cells: Box::new([EMPTY; CHUNK_CELLS]),
            hash: 0,
            filled: 0,
        };
        chunk.rehash();
        chunk
    }

    /// Recompute the digest from scratch — the fold every write used to
    /// pay for, kept for construction and for the test that proves the
    /// incremental form agrees with it.
    fn rehash(&mut self) {
        self.hash = 0;
        for (i, &cell) in self.cells.iter().enumerate() {
            if cell != EMPTY {
                self.hash ^= cell_hash(i, cell);
            }
        }
    }

    /// Write one cell and carry the digest with it, in O(1).
    ///
    /// The digest used to be an FNV fold over all 8 KB, recomputed per
    /// write. Invisible to the digger, which drills one bounded sweep a
    /// tick; ruinous for a game whose three factions reshape terrain
    /// continuously — the spike measured 7.01 µs for a cell written alone
    /// against 0.07 µs in a batch, and every microsecond of that gap was
    /// re-hashing cells that had not changed
    /// (docs/plans/desert-game.md §13a).
    ///
    /// XOR makes it incremental: remove the old term, add the new. It is
    /// order-independent, which is exactly right for a canonical form —
    /// the digest depends on the cells, not on the sequence of writes that
    /// produced them — and the index is mixed in, so two cells cannot
    /// cancel by holding the same material.
    fn write(&mut self, index: usize, value: u16) -> bool {
        let old = self.cells[index];
        if old == value {
            return false;
        }
        if old != EMPTY {
            self.hash ^= cell_hash(index, old);
            self.filled -= 1;
        }
        if value != EMPTY {
            self.hash ^= cell_hash(index, value);
            self.filled += 1;
        }
        self.cells[index] = value;
        true
    }
}

/// A cell's contribution to its chunk's digest: splitmix64 over the
/// `(index, material)` pair. Integer throughout, so two machines cannot
/// disagree about it.
fn cell_hash(index: usize, value: u16) -> u64 {
    let mut z = ((index as u64) << 16) | u64::from(value);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// `Box<[u16; 4096]>` ↔ serde, via a length-checked `Vec` (serde derives
/// arrays only up to 32 elements).
mod chunk_cells_serde {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::CHUNK_CELLS;

    pub fn serialize<S: Serializer>(cells: &[u16; CHUNK_CELLS], s: S) -> Result<S::Ok, S::Error> {
        cells.as_slice().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Box<[u16; CHUNK_CELLS]>, D::Error> {
        let cells = Vec::<u16>::deserialize(d)?;
        let len = cells.len();
        cells.into_boxed_slice().try_into().map_err(|_| {
            D::Error::custom(format!("chunk must have {CHUNK_CELLS} cells, got {len}"))
        })
    }
}

/// The chunk a world cell lives in.
fn chunk_key(x: i64, y: i64, z: i64) -> (i64, i64, i64) {
    (
        x.div_euclid(CHUNK_EDGE),
        y.div_euclid(CHUNK_EDGE),
        z.div_euclid(CHUNK_EDGE),
    )
}

/// The cell's index inside its chunk.
fn cell_index(x: i64, y: i64, z: i64) -> usize {
    let local = x.rem_euclid(CHUNK_EDGE)
        + CHUNK_EDGE * (y.rem_euclid(CHUNK_EDGE) + CHUNK_EDGE * z.rem_euclid(CHUNK_EDGE));
    usize::try_from(local).expect("chunk-local index is non-negative and < 4096")
}

/// A chunked 3D voxel terrain store: material id per cell, `EMPTY`
/// elsewhere. Coordinates are sim-space voxels (z-up), unbounded in all
/// axes. `BTreeMap` keying gives the canonical walk the digest needs.
#[derive(Default, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolumeStore {
    /// Serialized as a (sorted) sequence of `(key, chunk)` pairs, not a
    /// map — self-describing formats (JSON) reject non-string map keys.
    #[serde(with = "chunks_serde")]
    chunks: BTreeMap<(i64, i64, i64), Chunk>,
}

/// `BTreeMap<(i64,i64,i64), Chunk>` ↔ serde as a pair sequence (the
/// map's own sorted order in, re-collected on the way out).
mod chunks_serde {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serializer};

    use super::Chunk;

    type ChunkMap = BTreeMap<(i64, i64, i64), Chunk>;

    pub fn serialize<S: Serializer>(chunks: &ChunkMap, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(chunks.iter())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<ChunkMap, D::Error> {
        let pairs = Vec::<((i64, i64, i64), Chunk)>::deserialize(d)?;
        Ok(pairs.into_iter().collect())
    }
}

impl VolumeStore {
    /// A fresh, fully empty store.
    #[must_use]
    pub fn new() -> VolumeStore {
        VolumeStore::default()
    }

    /// The material at `(x, y, z)`, or `None` for air.
    #[must_use]
    pub fn get(&self, x: i64, y: i64, z: i64) -> Option<MaterialId> {
        let chunk = self.chunks.get(&chunk_key(x, y, z))?;
        let raw = chunk.cells[cell_index(x, y, z)];
        (raw != EMPTY).then_some(MaterialId(raw))
    }

    /// Set one voxel to `mat`.
    ///
    /// # Panics
    /// Panics if `mat` is the `EMPTY` sentinel (use [`clear`](VolumeStore::clear)).
    pub fn set(&mut self, x: i64, y: i64, z: i64, mat: MaterialId) {
        assert!(
            mat.0 != EMPTY,
            "VolumeStore::set: material {EMPTY} is the empty sentinel"
        );
        let chunk = self
            .chunks
            .entry(chunk_key(x, y, z))
            .or_insert_with(Chunk::empty);
        chunk.write(cell_index(x, y, z), mat.0);
    }

    /// Clear one voxel back to air. Dropping a chunk's last solid voxel
    /// drops the chunk (see the module docs on canonical form).
    pub fn clear(&mut self, x: i64, y: i64, z: i64) {
        let key = chunk_key(x, y, z);
        let Some(chunk) = self.chunks.get_mut(&key) else {
            return;
        };
        chunk.write(cell_index(x, y, z), EMPTY);
        if chunk.filled == 0 {
            self.chunks.remove(&key);
        }
    }

    /// Clear an inclusive box back to air — the batched inverse of
    /// [`fill`](VolumeStore::fill).
    ///
    /// `voxel_clear` punches one cell, which is the tunnel primitive; a
    /// bore, a trench and a crater are boxes, and asking for them one
    /// cell at a time was the shape the spike caught costing a hundred
    /// times what it should (§13a).
    pub fn carve(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64) {
        let (xa, xb) = (x0.min(x1), x0.max(x1));
        let (ya, yb) = (y0.min(y1), y0.max(y1));
        let (za, zb) = (z0.min(z1), z0.max(z1));
        let mut touched: BTreeSet<(i64, i64, i64)> = BTreeSet::new();
        for z in za..=zb {
            for y in ya..=yb {
                for x in xa..=xb {
                    let key = chunk_key(x, y, z);
                    if let Some(chunk) = self.chunks.get_mut(&key) {
                        chunk.write(cell_index(x, y, z), EMPTY);
                        touched.insert(key);
                    }
                }
            }
        }
        // A chunk emptied by the carve leaves the store, so "nothing was
        // ever painted here" and "everything here was removed" stay the
        // same canonical form — and the same digest.
        for key in touched {
            if self.chunks.get(&key).is_some_and(|c| c.filled == 0) {
                self.chunks.remove(&key);
            }
        }
    }

    /// Fill the inclusive box between corners `(x0, y0, z0)` and
    /// `(x1, y1, z1)` (either order, like the script verbs) with `mat`.
    /// Touched chunks are rehashed once at the end, not per voxel.
    ///
    /// # Panics
    /// Panics if `mat` is the `EMPTY` sentinel.
    #[allow(clippy::too_many_arguments)] // two corners + material, the script-verb shape
    pub fn fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, mat: MaterialId) {
        assert!(
            mat.0 != EMPTY,
            "VolumeStore::fill: material {EMPTY} is the empty sentinel"
        );
        let (xa, xb) = (x0.min(x1), x0.max(x1));
        let (ya, yb) = (y0.min(y1), y0.max(y1));
        let (za, zb) = (z0.min(z1), z0.max(z1));
        for z in za..=zb {
            for y in ya..=yb {
                for x in xa..=xb {
                    let chunk = self
                        .chunks
                        .entry(chunk_key(x, y, z))
                        .or_insert_with(Chunk::empty);
                    chunk.write(cell_index(x, y, z), mat.0);
                }
            }
        }
    }

    /// Whether any voxel is set — an empty store is exactly `new()`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// The store's canonical digest: chunk count, then each chunk's key
    /// and cached cell hash in `BTreeMap` order.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = StateHasher::new();
        StateHash::hash(self, &mut h);
        h.finish()
    }
}

impl StateHash for VolumeStore {
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(self.chunks.len() as u64);
        for (&(cx, cy, cz), chunk) in &self.chunks {
            h.write_i64(cx);
            h.write_i64(cy);
            h.write_i64(cz);
            h.write_u64(chunk.hash);
        }
    }
}

impl VoxelField for VolumeStore {
    fn occupied(&self, x: i64, y: i64, z: i64) -> bool {
        self.get(x, y, z).is_some()
    }
    fn material(&self, x: i64, y: i64, z: i64) -> MaterialId {
        self.get(x, y, z).unwrap_or(MaterialId(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_clear_round_trip_across_chunk_and_sign_boundaries() {
        let mut store = VolumeStore::new();
        // Cells straddling chunk seams and negative space.
        for &(x, y, z) in &[(0, 0, 0), (15, 15, 15), (16, 0, -1), (-1, -16, -17)] {
            assert_eq!(store.get(x, y, z), None);
            store.set(x, y, z, MaterialId(7));
            assert_eq!(store.get(x, y, z), Some(MaterialId(7)));
            store.clear(x, y, z);
            assert_eq!(store.get(x, y, z), None);
        }
        assert!(store.is_empty());
    }

    #[test]
    fn overhangs_hold_solid_above_air() {
        // The whole point of the volume store: a roof with nothing under
        // it — the column heightmap cannot say this.
        let mut store = VolumeStore::new();
        store.set(0, 0, 10, MaterialId(1));
        assert!(store.occupied(0, 0, 10));
        assert!(!store.occupied(0, 0, 9));
        assert!(!store.occupied(0, 0, 0));
    }

    #[test]
    fn fill_matches_per_voxel_sets() {
        let mut filled = VolumeStore::new();
        // Corners in either order, crossing a chunk boundary on x.
        filled.fill(14, 0, 0, 17, 2, 1, MaterialId(3));
        let mut set = VolumeStore::new();
        for x in 14..=17 {
            for y in 0..=2 {
                for z in 0..=1 {
                    set.set(x, y, z, MaterialId(3));
                }
            }
        }
        assert_eq!(filled, set);
        assert_eq!(filled.state_hash(), set.state_hash());
    }

    #[test]
    fn edits_re_key_the_digest_and_clears_restore_it() {
        let mut store = VolumeStore::new();
        let empty = store.state_hash();
        store.set(5, 5, 5, MaterialId(1));
        let one = store.state_hash();
        assert_ne!(empty, one, "a set must re-key the store digest");
        store.set(5, 5, 5, MaterialId(2));
        assert_ne!(one, store.state_hash(), "a material change must re-key");
        store.set(40, 5, 5, MaterialId(1));
        let two_chunks = store.state_hash();
        store.clear(40, 5, 5);
        store.set(5, 5, 5, MaterialId(1));
        assert_eq!(
            store.state_hash(),
            one,
            "same content, same digest — regardless of edit history"
        );
        assert_ne!(two_chunks, one);
        store.clear(5, 5, 5);
        assert_eq!(
            store.state_hash(),
            empty,
            "clearing a chunk's last voxel drops it back to the canonical empty form"
        );
        assert!(store.is_empty());
    }

    #[test]
    fn cached_chunk_hash_matches_a_from_scratch_rebuild() {
        // The cache's own tripwire: an incrementally edited store hashes
        // exactly like one rebuilt from its final contents.
        let mut edited = VolumeStore::new();
        edited.fill(0, 0, 0, 20, 20, 3, MaterialId(1));
        edited.fill(4, 4, 0, 8, 8, 3, MaterialId(2));
        for x in 0..=20 {
            edited.clear(x, 10, 2);
        }
        let mut rebuilt = VolumeStore::new();
        for (&(cx, cy, cz), chunk) in &edited.chunks {
            for z in 0..CHUNK_EDGE {
                for y in 0..CHUNK_EDGE {
                    for x in 0..CHUNK_EDGE {
                        let raw = chunk.cells[cell_index(x, y, z)];
                        if raw != EMPTY {
                            rebuilt.set(
                                cx * CHUNK_EDGE + x,
                                cy * CHUNK_EDGE + y,
                                cz * CHUNK_EDGE + z,
                                MaterialId(raw),
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(edited, rebuilt);
        assert_eq!(edited.state_hash(), rebuilt.state_hash());
    }

    #[test]
    fn voxel_field_reports_occupancy_and_material() {
        let mut store = VolumeStore::new();
        store.set(3, 4, 5, MaterialId(9));
        let field: &dyn VoxelField = &store;
        assert!(field.occupied(3, 4, 5));
        assert!(!field.occupied(3, 4, 6));
        assert_eq!(field.material(3, 4, 5), MaterialId(9));
    }

    #[test]
    fn serde_round_trip_is_bit_equal() {
        let mut store = VolumeStore::new();
        store.fill(-5, -5, 0, 5, 5, 2, MaterialId(1));
        store.set(0, 0, 10, MaterialId(2));
        let json = serde_json::to_string(&store).expect("serialize");
        let back: VolumeStore = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(store, back);
        assert_eq!(store.state_hash(), back.state_hash());
    }
}

#[cfg(test)]
mod incremental_hash_tests {
    use super::{cell_hash, Chunk, MaterialId, VolumeStore, EMPTY};

    /// The digest a write carries must equal the one a full recompute
    /// would produce. This is the whole safety of making it incremental:
    /// if the two ever part, peers agree on their cells and disagree on
    /// their hash, which is a desync with no cause visible in the world.
    #[test]
    fn carrying_the_hash_agrees_with_recomputing_it() {
        let mut chunk = Chunk::empty();
        let writes = [(0usize, 7u16), (4095, 3), (2000, 1), (0, 9), (4095, EMPTY)];
        for (i, v) in writes {
            chunk.write(i, v);
            let carried = chunk.hash;
            chunk.rehash();
            assert_eq!(
                carried, chunk.hash,
                "after writing {v} at {i} the carried digest drifted"
            );
        }
    }

    /// Order must not matter: the digest describes the cells, not the
    /// sequence that produced them. A peer that paints a berm before a
    /// trench must agree with one that paints them the other way round.
    #[test]
    fn the_digest_is_order_independent() {
        let mut a = VolumeStore::new();
        a.fill(0, 0, 0, 3, 3, 3, MaterialId(1));
        a.carve(1, 1, 1, 2, 2, 2);

        let mut b = VolumeStore::new();
        b.fill(0, 0, 0, 3, 3, 0, MaterialId(1));
        b.fill(0, 0, 1, 3, 3, 3, MaterialId(1));
        b.carve(2, 2, 2, 1, 1, 1); // reversed corners, same box
        assert_eq!(a.state_hash(), b.state_hash());
    }

    /// Emptied is indistinguishable from never painted — the canonical
    /// form the module has always promised, now upheld by `carve` too.
    #[test]
    fn a_carved_out_chunk_leaves_no_trace() {
        let mut store = VolumeStore::new();
        let empty = store.state_hash();
        store.fill(40, 40, 40, 45, 45, 45, MaterialId(2));
        assert_ne!(store.state_hash(), empty);
        store.carve(40, 40, 40, 45, 45, 45);
        assert_eq!(store.state_hash(), empty, "a carved box must vanish");
        assert!(store.is_empty());
    }

    /// Two cells holding the same material must not cancel each other in
    /// an XOR — the one way this scheme could go quietly wrong.
    #[test]
    fn identical_materials_at_different_cells_do_not_cancel() {
        assert_ne!(cell_hash(0, 5), cell_hash(1, 5));
        let mut store = VolumeStore::new();
        store.set(0, 0, 0, MaterialId(5));
        store.set(1, 0, 0, MaterialId(5));
        assert_ne!(store.state_hash(), VolumeStore::new().state_hash());
    }
}
