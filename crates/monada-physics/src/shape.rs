//! Body voxel occupancy + derived mass properties and collision skin
//! (docs/plans/voxel-physics.md §3 `shape`).
//!
//! A body's voxels live in a dense material grid over its local AABB;
//! cell `(x, y, z)` occupies the unit cube `[x, x+1)³` with its centre
//! at `(x+½, y+½, z+½)`, all in shape-local coordinates. Mass, centre
//! of mass, and inertia derive from the grid and the material
//! densities; the collision skin is the surface voxels (those with an
//! empty or out-of-bounds 6-neighbour) as spheres of radius ½ at the
//! voxel centres.
//!
//! **Support-polygon inset**: a sphere touches a plane directly under
//! its *centre*, so a body's support polygon is its footprint inset by
//! the ½-voxel skin radius on every side — corners are effectively
//! rounded (a 2³ cube stands on a ±½ square, not ±1, and its tipping
//! threshold is 0.5, below tan 30°). Inherent to the skin
//! representation and part of the chunky target feel; stability tests
//! and vehicle footprints must budget for it.
//!
//! **Inertia convention (plan, P2 amendments)**: each voxel is a solid
//! unit cube, contributing `ρ·[(|d|²·E − d⊗d) + E/6]` about the `CoM` —
//! the parallel-axis term plus the cube's own `E/6`. Point masses
//! would hand a singular tensor to legal input (a single voxel, a
//! 1×1×n rod); the cube term makes every non-empty body's tensor SPD
//! by construction.

use monada_fixed::{Fixed, FixedMat3, FixedVec3};
use monada_sim::{StateHash, StateHasher};

use crate::material::{Material, MaterialId};

/// The sentinel for an empty cell in the dense grid.
const EMPTY: u16 = u16::MAX;

/// A dense voxel grid in body-local space: dimensions plus one
/// `MaterialId` (or empty) per cell.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoxelShape {
    dims: (i32, i32, i32),
    /// Row-major `x + dx·(y + dy·z)`; `EMPTY` marks an empty cell.
    cells: Vec<u16>,
}

impl VoxelShape {
    /// An empty grid of the given dimensions.
    ///
    /// # Panics
    /// Panics on a non-positive dimension, or a cell count that would
    /// not fit a `Vec` index comfortably (> 2^24 cells — a 256³ body;
    /// far beyond any vehicle).
    #[must_use]
    pub fn new(dx: i32, dy: i32, dz: i32) -> VoxelShape {
        assert!(
            dx > 0 && dy > 0 && dz > 0,
            "VoxelShape::new: dimensions must be positive, got ({dx}, {dy}, {dz})"
        );
        let count = i64::from(dx) * i64::from(dy) * i64::from(dz);
        assert!(
            count <= 1 << 24,
            "VoxelShape::new: {count} cells exceeds the 2^24 sanity bound"
        );
        VoxelShape {
            dims: (dx, dy, dz),
            cells: vec![EMPTY; usize::try_from(count).expect("bounded above")],
        }
    }

    /// Grid dimensions.
    #[must_use]
    pub fn dims(&self) -> (i32, i32, i32) {
        self.dims
    }

    #[inline]
    fn index(&self, x: i32, y: i32, z: i32) -> usize {
        assert!(
            x >= 0 && x < self.dims.0 && y >= 0 && y < self.dims.1 && z >= 0 && z < self.dims.2,
            "VoxelShape: cell ({x}, {y}, {z}) out of bounds {:?}",
            self.dims
        );
        usize::try_from(x + self.dims.0 * (y + self.dims.1 * z)).expect("in bounds")
    }

    /// Set one cell's material.
    ///
    /// # Panics
    /// Panics out of bounds, or on a sentinel-colliding id (65535).
    pub fn set(&mut self, x: i32, y: i32, z: i32, mat: MaterialId) {
        assert!(
            mat.0 != EMPTY,
            "VoxelShape::set: material id {EMPTY} is reserved"
        );
        let i = self.index(x, y, z);
        self.cells[i] = mat.0;
    }

    /// Fill the inclusive box `[min, max]` with one material.
    ///
    /// # Panics
    /// Panics if any corner is out of bounds or `min > max` on an axis.
    pub fn fill_box(&mut self, min: (i32, i32, i32), max: (i32, i32, i32), mat: MaterialId) {
        assert!(
            min.0 <= max.0 && min.1 <= max.1 && min.2 <= max.2,
            "VoxelShape::fill_box: min {min:?} > max {max:?}"
        );
        for z in min.2..=max.2 {
            for y in min.1..=max.1 {
                for x in min.0..=max.0 {
                    self.set(x, y, z, mat);
                }
            }
        }
    }

    /// The cell's material, if occupied. Out-of-bounds reads as empty
    /// (that is what makes boundary voxels surface voxels).
    #[must_use]
    pub fn get(&self, x: i32, y: i32, z: i32) -> Option<MaterialId> {
        if x < 0 || x >= self.dims.0 || y < 0 || y >= self.dims.1 || z < 0 || z >= self.dims.2 {
            return None;
        }
        let raw = *self
            .cells
            .get(usize::try_from(x + self.dims.0 * (y + self.dims.1 * z)).ok()?)?;
        (raw != EMPTY).then_some(MaterialId(raw))
    }

    /// Iterate occupied cells in canonical order (x fastest, then y,
    /// then z — storage order).
    pub(crate) fn occupied_cells(&self) -> impl Iterator<Item = (i32, i32, i32, MaterialId)> + '_ {
        let (dx, dy, dz) = self.dims;
        (0..dz).flat_map(move |z| {
            (0..dy).flat_map(move |y| {
                (0..dx).filter_map(move |x| self.get(x, y, z).map(|m| (x, y, z, m)))
            })
        })
    }

    /// A surface voxel has at least one empty (or out-of-bounds)
    /// 6-neighbour.
    pub(crate) fn is_surface(&self, x: i32, y: i32, z: i32) -> bool {
        self.get(x + 1, y, z).is_none()
            || self.get(x - 1, y, z).is_none()
            || self.get(x, y + 1, z).is_none()
            || self.get(x, y - 1, z).is_none()
            || self.get(x, y, z + 1).is_none()
            || self.get(x, y, z - 1).is_none()
    }
}

impl StateHash for VoxelShape {
    /// Dims, then the raw cell grid in storage order (length-prefixed).
    fn hash(&self, h: &mut StateHasher) {
        h.write_i64(i64::from(self.dims.0));
        h.write_i64(i64::from(self.dims.1));
        h.write_i64(i64::from(self.dims.2));
        h.write_u64(self.cells.len() as u64);
        for &c in &self.cells {
            h.write_u64(u64::from(c));
        }
    }
}

/// One collision-skin sphere: a surface voxel as a radius-½ sphere.
/// `offset` is the voxel centre relative to the body's `CoM` (body
/// frame) — precomputed so the hot path is one rotate + add.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkinSphere {
    pub(crate) offset: FixedVec3,
    pub(crate) material: MaterialId,
}

/// The skin sphere radius: half a voxel.
pub(crate) const SKIN_RADIUS: Fixed = Fixed::from_ratio(1, 2);

/// Mass properties derived from a shape + material table.
pub(crate) struct MassProperties {
    pub mass: Fixed,
    /// `CoM` in shape-local coordinates.
    pub com: FixedVec3,
    /// About the `CoM`, shape-local axes.
    pub inertia: FixedMat3,
}

/// Half-voxel offset to a cell's centre.
fn cell_center(x: i32, y: i32, z: i32) -> FixedVec3 {
    FixedVec3::new(
        Fixed::from_int(x) + Fixed::HALF,
        Fixed::from_int(y) + Fixed::HALF,
        Fixed::from_int(z) + Fixed::HALF,
    )
}

/// Sum mass, `CoM`, and inertia (solid-cube convention, see module docs)
/// over the shape's occupied cells.
///
/// # Panics
/// Panics on an empty shape or a material id outside `materials`.
pub(crate) fn mass_properties(shape: &VoxelShape, materials: &[Material]) -> MassProperties {
    let mut mass = Fixed::ZERO;
    let mut weighted = FixedVec3::ZERO;
    for (x, y, z, mat) in shape.occupied_cells() {
        let density = materials
            .get(usize::from(mat.0))
            .unwrap_or_else(|| {
                panic!(
                    "shape uses material {}, world has {} registered",
                    mat.0,
                    materials.len()
                )
            })
            .density;
        mass += density;
        weighted += cell_center(x, y, z).scale(density);
    }
    assert!(mass > Fixed::ZERO, "voxel body must have positive mass");
    let com = weighted.scale(Fixed::ONE / mass);

    let sixth = Fixed::from_ratio(1, 6);
    let mut inertia = FixedMat3::ZERO;
    for (x, y, z, mat) in shape.occupied_cells() {
        let density = materials[usize::from(mat.0)].density;
        let d = cell_center(x, y, z) - com;
        let dd = d.dot(d);
        // ρ·[(|d|²·E − d⊗d) + E/6]
        let parallel = FixedMat3::from_diagonal(FixedVec3::new(dd, dd, dd)) - outer(d);
        inertia = inertia
            + (parallel + FixedMat3::from_diagonal(FixedVec3::new(sixth, sixth, sixth)))
                .scale(density);
    }
    MassProperties { mass, com, inertia }
}

/// The outer product `d ⊗ d` (symmetric).
fn outer(d: FixedVec3) -> FixedMat3 {
    FixedMat3::from_cols(
        FixedVec3::new(d.x * d.x, d.y * d.x, d.z * d.x),
        FixedVec3::new(d.x * d.y, d.y * d.y, d.z * d.y),
        FixedVec3::new(d.x * d.z, d.y * d.z, d.z * d.z),
    )
}

/// Derive the collision skin: surface voxels as CoM-relative spheres,
/// in canonical (storage-order) sequence — the solver's iteration
/// order and the warm-start key space.
pub(crate) fn derive_skin(shape: &VoxelShape, com: FixedVec3) -> Vec<SkinSphere> {
    shape
        .occupied_cells()
        .filter(|&(x, y, z, _)| shape.is_surface(x, y, z))
        .map(|(x, y, z, material)| SkinSphere {
            offset: cell_center(x, y, z) - com,
            material,
        })
        .collect()
}
