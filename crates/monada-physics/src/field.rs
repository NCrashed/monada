//! The terrain seam (docs/plans/voxel-physics.md §4).
//!
//! Physics does not own terrain: the engine implements [`VoxelField`]
//! over whatever store it keeps (today `monada-script`'s `VoxelStore`
//! column map; a true 3D store later), and hands it to
//! [`PhysicsWorld::step`](crate::PhysicsWorld::step) as a read-only
//! per-tick input. Queries are in sim-space voxel cells (z-up), the
//! same frame `monada-nav` walks; the argument style follows
//! `NavWorld` (separate `i64`s, not a vector type).

use crate::material::MaterialId;

/// Read-only voxel occupancy + material queries against terrain.
///
/// **Cell convention**: cell `(x, y, z)` occupies the unit cube
/// `[x, x+1) × [y, y+1) × [z, z+1)` in sim-space voxels.
///
/// **Material contract (cross-crate)**: the ids returned by
/// [`material`](VoxelField::material) must come from the *same world's*
/// [`register_material`](crate::PhysicsWorld::register_material) calls —
/// registration order defines the id space. The solver asserts (with a
/// legible message) on an id the world has never registered, rather
/// than crashing data-dependently mid-solve.
pub trait VoxelField {
    /// Is the cell solid?
    fn occupied(&self, x: i64, y: i64, z: i64) -> bool;

    /// The cell's material. Only queried for cells that reported
    /// [`occupied`](VoxelField::occupied).
    fn material(&self, x: i64, y: i64, z: i64) -> MaterialId;
}

/// The trivially empty field: nothing is ever occupied. The P1
/// free-flight scenarios and any test that wants gravity without
/// ground step against this.
pub struct EmptyField;

impl VoxelField for EmptyField {
    #[inline]
    fn occupied(&self, _x: i64, _y: i64, _z: i64) -> bool {
        false
    }

    #[inline]
    fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
        MaterialId(0)
    }
}
