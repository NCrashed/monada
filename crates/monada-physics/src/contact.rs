//! Narrowphase: skin spheres vs the terrain field, and (P5) vs other
//! bodies' voxel grids (docs/plans/voxel-physics.md §3 `narrowphase`).
//!
//! Each skin sphere (radius ½) overlaps at most a 2×2×2 block of unit
//! cells (3 per axis inside the ~2·margin window); every occupied cell
//! whose closest point lies within the sphere (plus a persistence
//! margin) yields one contact.
//!
//! **Pair narrowphase is asymmetric** (P5 amendments): the body with
//! the SMALLER skin contributes the spheres, the other contributes its
//! voxel grid (queried in its shape frame — rotation preserves
//! distances, so the containment test is exact). Ties break to the
//! lower id. Two documented consequences: a carve can flip a pair's
//! owner (all its warm-start keys re-key — one deterministically cold
//! tick), and coverage has chunky seams — a corner of body B entering
//! exactly between four face spheres of body A goes unnoticed until
//! ~⅓ voxel of penetration (the same √2/2 worst-case offset as the
//! terrain path since P2).
//!
//! Contacts are generated terrain-first then pair-by-pair; with pair
//! contacts the generation order is no longer globally lexicographic,
//! so the caller applies one deterministic sort by [`ContactKey`]
//! before the solve (P5 revision of the P2 "sorted by construction"
//! wording — see the plan).

use monada_fixed::{Fixed, FixedVec3};
use monada_sim::{StateHash, StateHasher};

use crate::body::RigidBody;
use crate::field::VoxelField;
use crate::ids::BodyId;
use crate::material::Material;
use crate::shape::SKIN_RADIUS;

/// Contacts closer than `radius + margin` persist, so a resting body
/// keeps (and warm-starts) its manifold across ticks instead of
/// flickering at the exact touch distance. Sized against the physics,
/// not taste: one tick of free fall from standstill is
/// `|g|·dt² = 0.016` voxels (at defaults), and a pair's gap can jitter
/// by a few of those between solves — a margin below that window made
/// stacked contacts flicker on/off every other tick, which broke warm
/// starting and pumped the stack (P5 find). 1/8 keeps resting
/// manifolds alive through impact transients; SPECULATIVE velocity
/// bias (see `solver::prepare`) lets bodies close a margin gap freely,
/// so the generous margin cannot make contacts sticky.
pub(crate) const CONTACT_MARGIN: Fixed = Fixed::from_ratio(1, 8);

/// Identity of one contact across ticks: which body's sphere, against
/// which counterpart (`None` = terrain, `Some` = that body's grid),
/// which cell (terrain cell, or the counterpart's shape cell widened
/// to `i64`).
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize,
)]
pub(crate) struct ContactKey {
    pub body: BodyId,
    pub sphere: u32,
    pub other: Option<BodyId>,
    pub cell: (i64, i64, i64),
}

impl StateHash for ContactKey {
    fn hash(&self, h: &mut StateHasher) {
        self.body.hash(h);
        h.write_u64(u64::from(self.sphere));
        match self.other {
            None => h.write_u8(0),
            Some(id) => {
                h.write_u8(1);
                id.hash(h);
            }
        }
        h.write_i64(self.cell.0);
        h.write_i64(self.cell.1);
        h.write_i64(self.cell.2);
    }
}

/// One entry of the persistent warm-start cache: the accumulated
/// impulses a contact ended the tick with. Sorted by key; hashed —
/// accumulated impulses feed the next tick's solve (plan, P2
/// amendments).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ContactCacheEntry {
    pub key: ContactKey,
    pub normal_impulse: Fixed,
    pub tangent_impulse: (Fixed, Fixed),
}

impl StateHash for ContactCacheEntry {
    fn hash(&self, h: &mut StateHasher) {
        self.key.hash(h);
        self.normal_impulse.hash(h);
        self.tangent_impulse.0.hash(h);
        self.tangent_impulse.1.hash(h);
    }
}

/// One live contact this tick (transient — rebuilt every step).
pub(crate) struct Contact {
    pub key: ContactKey,
    /// Index of the sphere owner in the world's body Vec.
    pub body_index: usize,
    /// Index of the grid body for a pair contact; `None` = terrain.
    pub other_index: Option<usize>,
    /// Contact normal, world frame, pointing away from the terrain /
    /// grid body (towards the sphere owner being pushed out).
    pub normal: FixedVec3,
    /// Contact point − owner `CoM`, world frame.
    pub r: FixedVec3,
    /// Contact point − grid body's `CoM`, world frame (ZERO for
    /// terrain).
    pub r_other: FixedVec3,
    /// Combined Coulomb μ = √(`μ_a·μ_b`).
    pub friction: Fixed,
    /// Combined restitution = `max(e_a`, `e_b`).
    pub restitution: Fixed,
    /// Signed gap along the closest-point axis at generation time
    /// (`dist − r`; negative = penetrating). Feeds the SPECULATIVE
    /// velocity bias: a margin contact may close its gap freely.
    pub separation: Fixed,
    // Solver scratch, filled by prepare():
    pub tangents: (FixedVec3, FixedVec3),
    pub normal_mass: Fixed,
    pub tangent_mass: (Fixed, Fixed),
    pub restitution_bias: Fixed,
    pub accumulated_normal: Fixed,
    pub accumulated_tangent: (Fixed, Fixed),
}

/// Closest point on the unit cell `[cx, cx+1)³` to `p`.
pub(crate) fn closest_point_on_cell(cell: (i64, i64, i64), p: FixedVec3) -> FixedVec3 {
    let clamp_axis = |v: Fixed, lo: i64| {
        let lo = Fixed::from_bits(lo << 32);
        v.clamp(lo, lo + Fixed::ONE)
    };
    FixedVec3::new(
        clamp_axis(p.x, cell.0),
        clamp_axis(p.y, cell.1),
        clamp_axis(p.z, cell.2),
    )
}

/// Contact normal. Priority (P5 revision): the closest-point axis
/// `center − closest` FIRST — it is the exact sphere-vs-box normal
/// whenever the sphere centre is outside the cell, and unlike the
/// occupancy gradient it cannot go diagonal on the corner/edge cells
/// of a finite body grid (the P2 gradient-first order fed a stacked
/// cube ~0.5z-diagonal normals from its counterpart's corner cells and
/// the stack ground itself sideways). The 3×3×3 occupancy gradient
/// remains the DEEP fallback (centre inside the cell, where the
/// closest-point axis degenerates), then the cell-centre axis, then
/// `+z`. `occupied` abstracts terrain cells vs a body grid's shape
/// cells.
fn contact_normal(
    occupied: &dyn Fn(i64, i64, i64) -> bool,
    cell: (i64, i64, i64),
    sphere_center: FixedVec3,
    closest: FixedVec3,
) -> FixedVec3 {
    let n = (sphere_center - closest).normalize();
    if n != FixedVec3::ZERO {
        return n;
    }
    let mut grad = FixedVec3::ZERO;
    for dz in -1..=1i64 {
        for dy in -1..=1i64 {
            for dx in -1..=1i64 {
                if (dx, dy, dz) == (0, 0, 0) {
                    continue;
                }
                if !occupied(cell.0 + dx, cell.1 + dy, cell.2 + dz) {
                    grad += FixedVec3::new(
                        Fixed::from_int(i32::try_from(dx).expect("small")),
                        Fixed::from_int(i32::try_from(dy).expect("small")),
                        Fixed::from_int(i32::try_from(dz).expect("small")),
                    );
                }
            }
        }
    }
    let n = grad.normalize();
    if n != FixedVec3::ZERO {
        return n;
    }
    let cell_center = FixedVec3::new(
        Fixed::from_bits(cell.0 << 32) + Fixed::HALF,
        Fixed::from_bits(cell.1 << 32) + Fixed::HALF,
        Fixed::from_bits(cell.2 << 32) + Fixed::HALF,
    );
    let n = (sphere_center - cell_center).normalize();
    if n != FixedVec3::ZERO {
        return n;
    }
    FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE)
}

/// Generate this tick's terrain contacts (awake bodies only —
/// sleeping bodies are frozen and keep no manifold). `materials` is
/// the world's registered table; terrain ids are validated against it
/// here (the cross-crate contract on [`VoxelField`]).
pub(crate) fn generate_terrain(
    bodies: &[RigidBody],
    field: &dyn VoxelField,
    materials: &[Material],
) -> Vec<Contact> {
    let mut out = Vec::new();
    for (body_index, body) in bodies.iter().enumerate() {
        if body.asleep {
            continue;
        }
        for (sphere_index, sphere) in body.skin.iter().enumerate() {
            let center = body.position + body.orientation * sphere.offset;
            let reach = SKIN_RADIUS + CONTACT_MARGIN;
            let lo = |v: Fixed| i64::from((v - reach).floor_to_int());
            let hi = |v: Fixed| i64::from((v + reach).floor_to_int());
            for cx in lo(center.x)..=hi(center.x) {
                for cy in lo(center.y)..=hi(center.y) {
                    for cz in lo(center.z)..=hi(center.z) {
                        if !field.occupied(cx, cy, cz) {
                            continue;
                        }
                        let closest = closest_point_on_cell((cx, cy, cz), center);
                        let delta = center - closest;
                        let dist_sq = delta.dot(delta);
                        if dist_sq >= reach * reach {
                            continue;
                        }
                        let separation = dist_sq.sqrt() - SKIN_RADIUS;
                        let terrain_mat = field.material(cx, cy, cz);
                        let terrain =
                            materials
                                .get(usize::from(terrain_mat.0))
                                .unwrap_or_else(|| {
                                    panic!(
                                        "terrain returned material {}, world has {} registered",
                                        terrain_mat.0,
                                        materials.len()
                                    )
                                });
                        let own = materials[usize::from(sphere.material.0)];
                        let normal = contact_normal(
                            &|x, y, z| field.occupied(x, y, z),
                            (cx, cy, cz),
                            center,
                            closest,
                        );
                        out.push(Contact {
                            key: ContactKey {
                                body: body.id,
                                sphere: u32::try_from(sphere_index).expect("skin < 2^32"),
                                other: None,
                                cell: (cx, cy, cz),
                            },
                            body_index,
                            other_index: None,
                            normal,
                            r: closest - body.position,
                            r_other: FixedVec3::ZERO,
                            friction: (own.friction * terrain.friction).sqrt(),
                            restitution: own.restitution.max(terrain.restitution),
                            separation,
                            tangents: (FixedVec3::ZERO, FixedVec3::ZERO),
                            normal_mass: Fixed::ZERO,
                            tangent_mass: (Fixed::ZERO, Fixed::ZERO),
                            restitution_bias: Fixed::ZERO,
                            accumulated_normal: Fixed::ZERO,
                            accumulated_tangent: (Fixed::ZERO, Fixed::ZERO),
                        });
                    }
                }
            }
        }
    }
    out
}

/// Narrowphase for one candidate pair: the smaller-skin body's spheres
/// against the other's voxel grid (see the module docs for the owner
/// convention and its consequences). Returns the pair's contacts;
/// empty means "not actually touching" — the caller uses that to
/// decide wake-ups, so broadphase adjacency alone can never wake a
/// sleeper (no sleep-thrash from drive-bys).
#[allow(clippy::many_single_char_names)]
pub(crate) fn generate_pair(
    bodies: &[RigidBody],
    materials: &[Material],
    a: usize,
    b: usize,
) -> Vec<Contact> {
    // Owner = fewer skin spheres; ties to the lower index (== lower id,
    // the body Vec is id-sorted).
    let (owner_index, grid_index) = if bodies[a].skin.len() <= bodies[b].skin.len() {
        (a, b)
    } else {
        (b, a)
    };
    let owner = &bodies[owner_index];
    let grid_body = &bodies[grid_index];
    let Some(grid) = grid_body.shape.as_ref() else {
        return Vec::new(); // ghosts have no grid to collide with
    };
    let inv_rot = grid_body.orientation.inverse();
    let (dx, dy, dz) = grid.dims();
    let reach = SKIN_RADIUS + CONTACT_MARGIN;

    let mut out = Vec::new();
    for (sphere_index, sphere) in owner.skin.iter().enumerate() {
        let center_world = owner.position + owner.orientation * sphere.offset;
        // Into the grid body's shape frame (distances preserved).
        let center = inv_rot * (center_world - grid_body.position) + grid_body.com_local;
        let lo = |v: Fixed, max: i32| i64::from((v - reach).floor_to_int().clamp(0, max - 1));
        let hi = |v: Fixed, max: i32| i64::from((v + reach).floor_to_int().clamp(0, max - 1));
        for cx in lo(center.x, dx)..=hi(center.x, dx) {
            for cy in lo(center.y, dy)..=hi(center.y, dy) {
                for cz in lo(center.z, dz)..=hi(center.z, dz) {
                    let (sx, sy, sz) = (
                        i32::try_from(cx).expect("clamped to dims"),
                        i32::try_from(cy).expect("clamped to dims"),
                        i32::try_from(cz).expect("clamped to dims"),
                    );
                    let Some(cell_mat) = grid.get(sx, sy, sz) else {
                        continue;
                    };
                    let closest = closest_point_on_cell((cx, cy, cz), center);
                    let delta = center - closest;
                    let dist_sq = delta.dot(delta);
                    if dist_sq >= reach * reach {
                        continue;
                    }
                    let separation = dist_sq.sqrt() - SKIN_RADIUS;
                    let normal_shape = contact_normal(
                        &|x, y, z| {
                            let (Ok(x), Ok(y), Ok(z)) =
                                (i32::try_from(x), i32::try_from(y), i32::try_from(z))
                            else {
                                return false;
                            };
                            grid.get(x, y, z).is_some()
                        },
                        (cx, cy, cz),
                        center,
                        closest,
                    );
                    let normal = grid_body.orientation * normal_shape;
                    let point_world = grid_body.position
                        + grid_body.orientation * (closest - grid_body.com_local);
                    let own = materials[usize::from(sphere.material.0)];
                    let theirs = materials[usize::from(cell_mat.0)];
                    out.push(Contact {
                        key: ContactKey {
                            body: owner.id,
                            sphere: u32::try_from(sphere_index).expect("skin < 2^32"),
                            other: Some(grid_body.id),
                            cell: (cx, cy, cz),
                        },
                        body_index: owner_index,
                        other_index: Some(grid_index),
                        normal,
                        r: point_world - owner.position,
                        r_other: point_world - grid_body.position,
                        friction: (own.friction * theirs.friction).sqrt(),
                        restitution: own.restitution.max(theirs.restitution),
                        separation,
                        tangents: (FixedVec3::ZERO, FixedVec3::ZERO),
                        normal_mass: Fixed::ZERO,
                        tangent_mass: (Fixed::ZERO, Fixed::ZERO),
                        restitution_bias: Fixed::ZERO,
                        accumulated_normal: Fixed::ZERO,
                        accumulated_tangent: (Fixed::ZERO, Fixed::ZERO),
                    });
                }
            }
        }
    }
    out
}
