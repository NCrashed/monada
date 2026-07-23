//! Sequential-impulse contact solver
//! (docs/plans/voxel-physics.md §3 `solver`, P2 amendments).
//!
//! Velocity pass: accumulated normal impulses (λₙ ≥ 0) with a
//! restitution bias only — no Baumgarte, so no positional energy leaks
//! into velocities. Friction as a two-direction Coulomb clamp
//! (|λₜ| ≤ μ·λₙ), warm-started per direction. Position pass: full-K
//! NGS pseudo-impulses — translation *and* orientation — so
//! angular-origin penetration resolves as rotation, not a body-wide
//! shift.
//!
//! Iteration order is canonical everywhere: the contact Vec is
//! generated sorted (see `contact`), and every pass walks it front to
//! back a fixed number of times.

use monada_fixed::{Fixed, FixedMat3, FixedQuat, FixedVec3};

use crate::body::RigidBody;
use crate::contact::Contact;

pub(crate) const VELOCITY_ITERATIONS: u32 = 8;
pub(crate) const POSITION_ITERATIONS: u32 = 2;
/// NGS relaxation per iteration.
const NGS_BETA: Fixed = Fixed::from_ratio(1, 5);
/// Penetration below this depth is left alone (rest tolerance).
pub(crate) const SLOP: Fixed = Fixed::from_ratio(1, 128);
/// Max positional correction per contact per iteration, in voxels.
const MAX_CORRECTION: Fixed = Fixed::from_ratio(1, 5);
/// Impacts slower than this along the normal don't bounce (they land).
const RESTITUTION_THRESHOLD: Fixed = Fixed::ONE;

/// A deterministic orthonormal tangent basis for a unit normal.
///
/// `t1 = normalize(n × e)` where `e` is the world axis least aligned
/// with `n` — smallest |component|, ties broken by fixed axis order
/// x → y → z (so an axis normal like `+z` always yields the same
/// basis). `t2 = n × t1`. Stable across ticks for an unchanged normal,
/// which is what keeps per-direction friction warm-starting honest.
pub(crate) fn tangent_basis(n: FixedVec3) -> (FixedVec3, FixedVec3) {
    let (ax, ay, az) = (
        n.x.to_bits().abs(),
        n.y.to_bits().abs(),
        n.z.to_bits().abs(),
    );
    let e = if ax <= ay && ax <= az {
        FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO)
    } else if ay <= az {
        FixedVec3::new(Fixed::ZERO, Fixed::ONE, Fixed::ZERO)
    } else {
        FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE)
    };
    let t1 = n.cross(e).normalize();
    let t2 = n.cross(t1);
    (t1, t2)
}

/// World-frame inverse inertia: `R · I⁻¹_body · Rᵀ`.
pub(crate) fn inv_inertia_world(body: &RigidBody) -> FixedMat3 {
    let r = FixedMat3::from_quat(body.orientation);
    r * body.inv_inertia_body * r.transpose()
}

/// The effective mass of a unit impulse along `axis` at offset `r`:
/// `1 / (m⁻¹ + ((I⁻¹(r×axis)) × r) · axis)`.
fn effective_mass(
    body: &RigidBody,
    inv_inertia: &FixedMat3,
    r: FixedVec3,
    axis: FixedVec3,
) -> Fixed {
    let rn = r.cross(axis);
    let k = body.inv_mass + (*inv_inertia * rn).cross(r).dot(axis);
    Fixed::ONE / k
}

/// Velocity of the body at `CoM` offset `r`.
fn velocity_at(body: &RigidBody, r: FixedVec3) -> FixedVec3 {
    body.linear_velocity + body.angular_velocity.cross(r)
}

/// Apply an impulse at `CoM` offset `r`.
fn apply_at(body: &mut RigidBody, inv_inertia: &FixedMat3, impulse: FixedVec3, r: FixedVec3) {
    body.linear_velocity += impulse.scale(body.inv_mass);
    body.angular_velocity += *inv_inertia * r.cross(impulse);
}

/// Fill solver scratch (bases, effective masses, restitution bias) and
/// warm-start: re-apply last tick's accumulated impulses.
pub(crate) fn prepare(
    bodies: &mut [RigidBody],
    inv_inertias: &[FixedMat3],
    contacts: &mut [Contact],
) {
    for c in contacts.iter_mut() {
        let body = &bodies[c.body_index];
        let inv_inertia = &inv_inertias[c.body_index];
        c.tangents = tangent_basis(c.normal);
        c.normal_mass = effective_mass(body, inv_inertia, c.r, c.normal);
        c.tangent_mass = (
            effective_mass(body, inv_inertia, c.r, c.tangents.0),
            effective_mass(body, inv_inertia, c.r, c.tangents.1),
        );
        // Restitution: measured before the solve, applied as a bias.
        let vn = velocity_at(body, c.r).dot(c.normal);
        c.restitution_bias = if vn < -RESTITUTION_THRESHOLD {
            c.restitution * vn
        } else {
            Fixed::ZERO
        };
        // Warm start (accumulated impulses were loaded from the cache
        // by the caller before this).
        let impulse = c.normal.scale(c.accumulated_normal)
            + c.tangents.0.scale(c.accumulated_tangent.0)
            + c.tangents.1.scale(c.accumulated_tangent.1);
        apply_at(&mut bodies[c.body_index], inv_inertia, impulse, c.r);
    }
}

/// One full velocity pass: normal then friction per contact, contacts
/// in canonical order.
pub(crate) fn solve_velocities(
    bodies: &mut [RigidBody],
    inv_inertias: &[FixedMat3],
    contacts: &mut [Contact],
) {
    for _ in 0..VELOCITY_ITERATIONS {
        for c in contacts.iter_mut() {
            let inv_inertia = &inv_inertias[c.body_index];
            // Normal: push out, never pull in (λₙ ≥ 0 accumulated).
            let body = &bodies[c.body_index];
            let vn = velocity_at(body, c.r).dot(c.normal);
            let lambda = -(vn + c.restitution_bias) * c.normal_mass;
            let new_acc = (c.accumulated_normal + lambda).max(Fixed::ZERO);
            let delta = new_acc - c.accumulated_normal;
            c.accumulated_normal = new_acc;
            apply_at(
                &mut bodies[c.body_index],
                inv_inertia,
                c.normal.scale(delta),
                c.r,
            );
            // Friction: Coulomb box clamp per tangent direction.
            let limit = c.friction * c.accumulated_normal;
            let body = &bodies[c.body_index];
            let vt0 = velocity_at(body, c.r).dot(c.tangents.0);
            let l0 = -vt0 * c.tangent_mass.0;
            let new0 = (c.accumulated_tangent.0 + l0).clamp(-limit, limit);
            let d0 = new0 - c.accumulated_tangent.0;
            c.accumulated_tangent.0 = new0;
            apply_at(
                &mut bodies[c.body_index],
                inv_inertia,
                c.tangents.0.scale(d0),
                c.r,
            );
            let body = &bodies[c.body_index];
            let vt1 = velocity_at(body, c.r).dot(c.tangents.1);
            let l1 = -vt1 * c.tangent_mass.1;
            let new1 = (c.accumulated_tangent.1 + l1).clamp(-limit, limit);
            let d1 = new1 - c.accumulated_tangent.1;
            c.accumulated_tangent.1 = new1;
            apply_at(
                &mut bodies[c.body_index],
                inv_inertia,
                c.tangents.1.scale(d1),
                c.r,
            );
        }
    }
}

/// Full-K NGS position pass: recompute each contact's penetration from
/// the *current* pose (sphere world position vs its cell) and push the
/// body out along the stored normal — translation and orientation both
/// (plan, P2 amendments). Pseudo-impulses only; velocities untouched.
pub(crate) fn solve_positions(bodies: &mut [RigidBody], contacts: &[Contact]) {
    for _ in 0..POSITION_ITERATIONS {
        for c in contacts {
            let body = &bodies[c.body_index];
            // Current sphere centre: the skin offset this contact came
            // from, under the body's *current* pose.
            let sphere = body.skin[usize::try_from(c.key.sphere).expect("skin index fits")];
            let center = body.position + body.orientation * sphere.offset;
            let closest = closest_on_cell(c.key.cell, center);
            let separation = (center - closest).dot(c.normal) - crate::shape::SKIN_RADIUS;
            let depth = -(separation + SLOP);
            if depth <= Fixed::ZERO {
                continue;
            }
            let correction = (NGS_BETA * depth).min(MAX_CORRECTION);
            // Full K at the current pose.
            let inv_inertia = inv_inertia_world(body);
            let r = closest - body.position;
            let k = effective_mass(body, &inv_inertia, r, c.normal);
            let impulse = c.normal.scale(correction * k);
            let body = &mut bodies[c.body_index];
            body.position += impulse.scale(body.inv_mass);
            body.orientation = (FixedQuat::from_scaled_axis(inv_inertia * r.cross(impulse))
                * body.orientation)
                .normalize();
        }
    }
}

/// Closest point on the unit cell to `p` (duplicated from `contact` to
/// keep module boundaries clean; both are the same three clamps).
fn closest_on_cell(cell: (i64, i64, i64), p: FixedVec3) -> FixedVec3 {
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
