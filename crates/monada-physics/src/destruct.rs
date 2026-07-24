//! Destruction: voxel removal, connectivity, splits, debris
//! (docs/plans/voxel-physics.md §3 `destruct`, P4 amendments).
//!
//! The engine decides *what* to cut (P6 wires the drill loop); this
//! module owns what happens to the body afterwards. Everything is
//! synchronous inside [`remove_voxels`](crate::PhysicsWorld::remove_voxels)
//! and reported through [`Removal`] — a return value, not a world
//! event buffer (a pending-event queue would be hashed, serialized
//! sim state with drain-order rules; the single caller wants the
//! outcome immediately).

use monada_fixed::{Fixed, FixedQuat, FixedVec3};

use crate::ids::BodyId;
use crate::material::{Material, MaterialId};
use crate::shape::VoxelShape;
use crate::wheels::WheelId;

/// The synchronous outcome of one `remove_voxels` call.
#[must_use]
pub struct Removal {
    /// Voxels actually carved. Empty and out-of-bounds cells are
    /// skipped silently, and duplicated cells collapse (the second
    /// occurrence sees an already-empty cell) — a contract the P6
    /// drill may lean on, not an accident.
    pub removed: u32,
    /// The original id, still alive — `None` if the body vanished
    /// (fully carved, or every remaining component fell below the
    /// debris threshold and degraded to debris).
    pub survivor: Option<BodyId>,
    /// New bodies split off (components at or above the debris
    /// threshold that did not keep the identity), ids monotonic,
    /// ordered by each component's lexicographically smallest cell in
    /// the PARENT's as-authored grid.
    pub split_off: Vec<BodyId>,
    /// Sub-threshold clusters — never spawned as bodies; the engine's
    /// debris consumer (demo script today, falling-sand layer later)
    /// takes them from here. Same ordering as `split_off`.
    pub debris: Vec<DebrisCluster>,
    /// Wheels auto-detached because their anchor's nearest occupied
    /// pre-carve cell departed with a fragment (or was carved away).
    /// Ascending by id. The engine decides what, if anything, to
    /// spawn in their place.
    pub detached_wheels: Vec<WheelId>,
}

/// One sub-threshold voxel cluster, ready for an effects consumer.
///
/// Frame conventions: `position` and `linear_velocity` are world
/// space; `voxels` offsets are relative to the cluster's centre of
/// mass in the BODY frame (rotate by `orientation` to get world
/// offsets).
#[derive(Clone, Debug)]
pub struct DebrisCluster {
    /// World-space centre of mass of the cluster.
    pub position: FixedVec3,
    /// The parent body's orientation at split time.
    pub orientation: FixedQuat,
    /// Rigid-body point velocity of the cluster `CoM`:
    /// `v_parent + ω_parent × r`.
    pub linear_velocity: FixedVec3,
    /// Voxel centres relative to the cluster `CoM` (body frame), with
    /// their materials.
    pub voxels: Vec<(FixedVec3, MaterialId)>,
}

/// One 6-connected component of a carved shape, in the parent's
/// as-authored grid coordinates.
pub(crate) struct Component {
    /// Cells in storage order (x fastest).
    pub cells: Vec<(i32, i32, i32)>,
    /// Lexicographically smallest (x, y, z) cell — the deterministic
    /// ordering and tie-break key.
    pub min_cell: (i32, i32, i32),
    pub mass: Fixed,
    /// Mass-weighted centre in shape coordinates.
    pub com: FixedVec3,
}

/// Half-voxel offset to a cell's centre (shape coordinates).
pub(crate) fn cell_center(c: (i32, i32, i32)) -> FixedVec3 {
    FixedVec3::new(
        Fixed::from_int(c.0) + Fixed::HALF,
        Fixed::from_int(c.1) + Fixed::HALF,
        Fixed::from_int(c.2) + Fixed::HALF,
    )
}

/// 6-connected flood fill over the shape's occupied cells. Components
/// are returned ordered by `min_cell` (lexicographic x, y, z); their
/// cell lists are in storage order. Deterministic by construction:
/// seeds scan in storage order, the stack pushes neighbours in a
/// fixed sequence, and the final sort key is total.
pub(crate) fn components(shape: &VoxelShape, materials: &[Material]) -> Vec<Component> {
    let mut visited: std::collections::BTreeSet<(i32, i32, i32)> =
        std::collections::BTreeSet::new();
    let mut out: Vec<Component> = Vec::new();
    for (sx, sy, sz, _) in shape.occupied_cells() {
        if visited.contains(&(sx, sy, sz)) {
            continue;
        }
        let mut cells = Vec::new();
        let mut stack = vec![(sx, sy, sz)];
        visited.insert((sx, sy, sz));
        let mut mass = Fixed::ZERO;
        let mut weighted = FixedVec3::ZERO;
        let mut min_cell = (sx, sy, sz);
        while let Some(c) = stack.pop() {
            let mat = shape
                .get(c.0, c.1, c.2)
                .expect("stacked cells are occupied");
            let density = materials[usize::from(mat.0)].density;
            mass += density;
            weighted += cell_center(c).scale(density);
            if c < min_cell {
                min_cell = c;
            }
            cells.push(c);
            for n in [
                (c.0 + 1, c.1, c.2),
                (c.0 - 1, c.1, c.2),
                (c.0, c.1 + 1, c.2),
                (c.0, c.1 - 1, c.2),
                (c.0, c.1, c.2 + 1),
                (c.0, c.1, c.2 - 1),
            ] {
                if shape.get(n.0, n.1, n.2).is_some() && visited.insert(n) {
                    stack.push(n);
                }
            }
        }
        cells.sort_unstable_by_key(|&(x, y, z)| (z, y, x)); // storage order
        out.push(Component {
            cells,
            min_cell,
            mass,
            // Direct division — reciprocal rounding would be amplified
            // by |weighted| (see `shape::div_by`).
            com: crate::shape::div_by(weighted, mass),
        });
    }
    out.sort_unstable_by_key(|c| c.min_cell);
    out
}

/// The occupied pre-carve cell nearest to `point` (shape coordinates),
/// ties broken by lexicographic cell order. The wheel-detach rule's
/// anchor cell — anchors may be virtual (outside the shape entirely),
/// so "the containing cell" is not well-defined; "nearest structure"
/// is.
pub(crate) fn nearest_cell(point: FixedVec3, cells: &[(i32, i32, i32)]) -> (i32, i32, i32) {
    let mut best = cells[0];
    let mut best_d = Fixed::MAX;
    for &c in cells {
        let d = point - cell_center(c);
        let dist = d.dot(d);
        if dist < best_d || (dist == best_d && c < best) {
            best_d = dist;
            best = c;
        }
    }
    best
}
