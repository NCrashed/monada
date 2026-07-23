//! Narrowphase: skin spheres vs the terrain field
//! (docs/plans/voxel-physics.md §3 `narrowphase`).
//!
//! Each skin sphere (radius ½) overlaps at most a 2×2×2 block of unit
//! cells; every occupied cell in that block whose closest point lies
//! within the sphere (plus a persistence margin) yields one contact.
//! Contacts are generated in canonical order — bodies ascending by id,
//! spheres by skin index, cells in fixed (x, y, z) order — which makes
//! the warm-start cache Vec sorted by construction.

use monada_fixed::{Fixed, FixedVec3};
use monada_sim::{StateHash, StateHasher};

use crate::body::RigidBody;
use crate::field::VoxelField;
use crate::ids::BodyId;
use crate::material::Material;
use crate::shape::SKIN_RADIUS;

/// Contacts closer than `radius + margin` persist, so a resting body
/// keeps (and warm-starts) its manifold across ticks instead of
/// flickering at the exact touch distance.
const CONTACT_MARGIN: Fixed = Fixed::from_ratio(1, 64);

/// Identity of one contact across ticks: which body, which skin
/// sphere, which terrain cell. Lexicographic `Ord` matches generation
/// order.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize,
)]
pub(crate) struct ContactKey {
    pub body: BodyId,
    pub sphere: u32,
    pub cell: (i64, i64, i64),
}

impl StateHash for ContactKey {
    fn hash(&self, h: &mut StateHasher) {
        self.body.hash(h);
        h.write_u64(u64::from(self.sphere));
        h.write_i64(self.cell.0);
        h.write_i64(self.cell.1);
        h.write_i64(self.cell.2);
    }
}

/// One entry of the persistent warm-start cache: the accumulated
/// impulses a contact ended the tick with. Sorted by key (see module
/// docs); hashed — accumulated impulses feed the next tick's solve
/// (plan, P2 amendments).
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
    /// Index into the world's body Vec.
    pub body_index: usize,
    /// Contact normal, pointing away from the terrain (towards the
    /// body being pushed out).
    pub normal: FixedVec3,
    /// Contact point − body `CoM`, world frame.
    pub r: FixedVec3,
    /// Combined Coulomb μ = √(`μ_body·μ_terrain`).
    pub friction: Fixed,
    /// Combined restitution = `max(e_body`, `e_terrain`).
    pub restitution: Fixed,
    // Solver scratch, filled by prepare():
    pub tangents: (FixedVec3, FixedVec3),
    pub normal_mass: Fixed,
    pub tangent_mass: (Fixed, Fixed),
    pub restitution_bias: Fixed,
    pub accumulated_normal: Fixed,
    pub accumulated_tangent: (Fixed, Fixed),
}

/// Closest point on the unit cell `[cx, cx+1)³` to `p`.
fn closest_point_on_cell(cell: (i64, i64, i64), p: FixedVec3) -> FixedVec3 {
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

/// Contact normal from the occupancy gradient in the 3×3×3
/// neighbourhood of the cell: the average direction of *empty*
/// neighbours (equivalently, minus the occupied-mass direction).
/// Fallbacks, in order, when the gradient cancels (deep penetration in
/// a fully solid block, or a symmetric pocket): the closest-point axis
/// `p − q`, then the cell-centre-to-sphere axis, then `+z` — terrain
/// is floor-like in v1, so "push up" is the least-wrong answer.
fn contact_normal(
    field: &dyn VoxelField,
    cell: (i64, i64, i64),
    sphere_center: FixedVec3,
    closest: FixedVec3,
) -> FixedVec3 {
    let mut grad = FixedVec3::ZERO;
    for dz in -1..=1i64 {
        for dy in -1..=1i64 {
            for dx in -1..=1i64 {
                if (dx, dy, dz) == (0, 0, 0) {
                    continue;
                }
                if !field.occupied(cell.0 + dx, cell.1 + dy, cell.2 + dz) {
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
    let n = (sphere_center - closest).normalize();
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

/// Generate this tick's contacts in canonical order. `materials` is
/// the world's registered table; terrain material ids are validated
/// against it here (the cross-crate contract on [`VoxelField`]).
pub(crate) fn generate(
    bodies: &[RigidBody],
    field: &dyn VoxelField,
    materials: &[Material],
) -> Vec<Contact> {
    let mut out = Vec::new();
    for (body_index, body) in bodies.iter().enumerate() {
        for (sphere_index, sphere) in body.skin.iter().enumerate() {
            let center = body.position + body.orientation * sphere.offset;
            let reach = SKIN_RADIUS + CONTACT_MARGIN;
            // Cells overlapping the sphere's AABB: at most 2×2×2.
            let lo = |v: Fixed| i64::from((v - reach).floor_to_int());
            let hi = |v: Fixed| i64::from((v + reach).floor_to_int());
            for cz in lo(center.z)..=hi(center.z) {
                for cy in lo(center.y)..=hi(center.y) {
                    for cx in lo(center.x)..=hi(center.x) {
                        if !field.occupied(cx, cy, cz) {
                            continue;
                        }
                        let closest = closest_point_on_cell((cx, cy, cz), center);
                        let delta = center - closest;
                        let dist_sq = delta.dot(delta);
                        if dist_sq >= reach * reach {
                            continue;
                        }
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
                        let normal = contact_normal(field, (cx, cy, cz), center, closest);
                        out.push(Contact {
                            key: ContactKey {
                                body: body.id,
                                sphere: u32::try_from(sphere_index).expect("skin < 2^32"),
                                cell: (cx, cy, cz),
                            },
                            body_index,
                            normal,
                            r: closest - body.position,
                            friction: (own.friction * terrain.friction).sqrt(),
                            restitution: own.restitution.max(terrain.restitution),
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
