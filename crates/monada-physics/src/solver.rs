//! Sequential-impulse contact solver
//! (docs/plans/voxel-physics.md §3 `solver`, P2/P5 amendments).
//!
//! Velocity pass: accumulated normal impulses (λₙ ≥ 0) with a
//! restitution bias only — no Baumgarte, so no positional energy leaks
//! into velocities. Friction as a two-direction Coulomb clamp
//! (|λₜ| ≤ μ·λₙ), warm-started per direction. Position pass: full-K
//! NGS pseudo-impulses — translation *and* orientation — so
//! angular-origin penetration resolves as rotation, not a body-wide
//! shift.
//!
//! P5: contacts may join two bodies. All constraint math then runs on
//! RELATIVE contact-point velocity (the restitution threshold
//! included), the effective mass sums both ends, and every impulse
//! applies `+` to the sphere owner and `−` to the grid body; NGS
//! splits its positional correction across both ends through the same
//! full-K operator.
//!
//! Iteration order is canonical everywhere: the contact Vec arrives
//! sorted by key, and every pass walks it front to back a fixed number
//! of times.

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
/// Impacts slower than this along the normal don't bounce (they
/// land). Compared against the RELATIVE normal velocity for pairs.
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

/// The effective mass of a unit impulse along `axis` at offset `r` on
/// ONE body: `1 / (m⁻¹ + ((I⁻¹(r×axis)) × r) · axis)`. The wheel pass
/// uses this single-ended form; contacts use [`pair_mass`].
pub(crate) fn effective_mass(
    body: &RigidBody,
    inv_inertia: &FixedMat3,
    r: FixedVec3,
    axis: FixedVec3,
) -> Fixed {
    Fixed::ONE / k_term(body, inv_inertia, r, axis)
}

/// One body's contribution to a constraint denominator.
fn k_term(body: &RigidBody, inv_inertia: &FixedMat3, r: FixedVec3, axis: FixedVec3) -> Fixed {
    let rn = r.cross(axis);
    body.inv_mass + (*inv_inertia * rn).cross(r).dot(axis)
}

/// Effective mass of a contact constraint along `axis` — both ends
/// for a pair, one for terrain.
fn pair_mass(
    bodies: &[RigidBody],
    inv_inertias: &[FixedMat3],
    c: &Contact,
    axis: FixedVec3,
) -> Fixed {
    let mut k = k_term(
        &bodies[c.body_index],
        &inv_inertias[c.body_index],
        c.r,
        axis,
    );
    if let Some(other) = c.other_index {
        k += k_term(&bodies[other], &inv_inertias[other], c.r_other, axis);
    }
    Fixed::ONE / k
}

/// Velocity of the body at `CoM` offset `r`.
fn velocity_at(body: &RigidBody, r: FixedVec3) -> FixedVec3 {
    body.linear_velocity + body.angular_velocity.cross(r)
}

/// Relative contact-point velocity: owner minus grid body (terrain is
/// static, so terrain contacts reduce to the owner's point velocity).
fn relative_velocity(bodies: &[RigidBody], c: &Contact) -> FixedVec3 {
    let own = velocity_at(&bodies[c.body_index], c.r);
    match c.other_index {
        None => own,
        Some(other) => own - velocity_at(&bodies[other], c.r_other),
    }
}

/// Apply a contact impulse: `+J` on the sphere owner, `−J` on the grid
/// body (if any).
fn apply_contact(
    bodies: &mut [RigidBody],
    inv_inertias: &[FixedMat3],
    c: &Contact,
    impulse: FixedVec3,
) {
    {
        let body = &mut bodies[c.body_index];
        body.linear_velocity += impulse.scale(body.inv_mass);
        body.angular_velocity += inv_inertias[c.body_index] * c.r.cross(impulse);
    }
    if let Some(other) = c.other_index {
        let body = &mut bodies[other];
        body.linear_velocity -= impulse.scale(body.inv_mass);
        body.angular_velocity -= inv_inertias[other] * c.r_other.cross(impulse);
    }
}

/// Fill solver scratch (bases, effective masses, restitution +
/// speculative bias) and warm-start: re-apply last tick's accumulated
/// impulses.
pub(crate) fn prepare(
    bodies: &mut [RigidBody],
    inv_inertias: &[FixedMat3],
    contacts: &mut [Contact],
    dt: Fixed,
) {
    for c in contacts.iter_mut() {
        c.tangents = tangent_basis(c.normal);
        c.normal_mass = pair_mass(bodies, inv_inertias, c, c.normal);
        c.tangent_mass = (
            pair_mass(bodies, inv_inertias, c, c.tangents.0),
            pair_mass(bodies, inv_inertias, c, c.tangents.1),
        );
        // SPECULATIVE bias (P5): a contact generated inside the
        // persistence margin but not yet touching (separation > 0) may
        // close its gap freely — the normal impulse only fires on
        // approach FASTER than `separation/dt`. This is what lets the
        // margin be generous (persistent manifolds, live warm starts)
        // without making contacts sticky or leaving bodies hovering.
        let speculative = c.separation.max(Fixed::ZERO) / dt;
        // Restitution: measured before the solve, applied as a bias.
        // Caveat: contact i's vn already includes the warm-start
        // impulses of contacts j < i on the same body (this loop
        // interleaves both). Box2D splits bias measurement and warm
        // start into two passes; with restitution ≈ 0 (target feel)
        // and the 1.0 threshold the difference is currently zero —
        // split the passes when bouncy multi-contact impacts become
        // real.
        let vn = relative_velocity(bodies, c).dot(c.normal);
        c.restitution_bias = speculative
            + if vn < -RESTITUTION_THRESHOLD {
                c.restitution * vn
            } else {
                Fixed::ZERO
            };
        // Warm start (accumulated impulses were loaded from the cache
        // by the caller before this).
        let impulse = c.normal.scale(c.accumulated_normal)
            + c.tangents.0.scale(c.accumulated_tangent.0)
            + c.tangents.1.scale(c.accumulated_tangent.1);
        apply_contact(bodies, inv_inertias, c, impulse);
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
            // Normal: push apart, never pull in (λₙ ≥ 0 accumulated).
            let vn = relative_velocity(bodies, c).dot(c.normal);
            let lambda = -(vn + c.restitution_bias) * c.normal_mass;
            let new_acc = (c.accumulated_normal + lambda).max(Fixed::ZERO);
            let delta = new_acc - c.accumulated_normal;
            c.accumulated_normal = new_acc;
            apply_contact(bodies, inv_inertias, c, c.normal.scale(delta));
            // Friction: Coulomb box clamp per tangent direction.
            let limit = c.friction * c.accumulated_normal;
            let vt0 = relative_velocity(bodies, c).dot(c.tangents.0);
            let l0 = -vt0 * c.tangent_mass.0;
            let new0 = (c.accumulated_tangent.0 + l0).clamp(-limit, limit);
            let d0 = new0 - c.accumulated_tangent.0;
            c.accumulated_tangent.0 = new0;
            apply_contact(bodies, inv_inertias, c, c.tangents.0.scale(d0));
            let vt1 = relative_velocity(bodies, c).dot(c.tangents.1);
            let l1 = -vt1 * c.tangent_mass.1;
            let new1 = (c.accumulated_tangent.1 + l1).clamp(-limit, limit);
            let d1 = new1 - c.accumulated_tangent.1;
            c.accumulated_tangent.1 = new1;
            apply_contact(bodies, inv_inertias, c, c.tangents.1.scale(d1));
        }
    }
}

/// Full-K NGS position pass: recompute each contact's penetration from
/// the *current* poses and push the ends apart along the stored
/// normal — translation and orientation both, on both bodies of a
/// pair (plan, P2/P5 amendments). Pseudo-impulses only; velocities
/// untouched.
pub(crate) fn solve_positions(bodies: &mut [RigidBody], contacts: &[Contact]) {
    for _ in 0..POSITION_ITERATIONS {
        for c in contacts {
            let owner = &bodies[c.body_index];
            // Current sphere centre under the owner's current pose.
            let sphere = owner.skin[usize::try_from(c.key.sphere).expect("skin index fits")];
            let center = owner.position + owner.orientation * sphere.offset;
            // Closest point on the contact cell under the counterpart's
            // current pose (terrain cells are world-fixed).
            let closest = match c.other_index {
                None => closest_on_cell(c.key.cell, center),
                Some(other) => {
                    let grid_body = &bodies[other];
                    let local = grid_body.orientation.inverse() * (center - grid_body.position)
                        + grid_body.com_local;
                    let closest_local = closest_on_cell(c.key.cell, local);
                    grid_body.position
                        + grid_body.orientation * (closest_local - grid_body.com_local)
                }
            };
            let separation = (center - closest).dot(c.normal) - crate::shape::SKIN_RADIUS;
            let depth = -(separation + SLOP);
            if depth <= Fixed::ZERO {
                continue;
            }
            let correction = (NGS_BETA * depth).min(MAX_CORRECTION);
            // Full K at the current poses, both ends.
            let inv_own = inv_inertia_world(owner);
            let r_own = closest - owner.position;
            let mut k = k_term(owner, &inv_own, r_own, c.normal);
            let other_side = c.other_index.map(|other| {
                let grid_body = &bodies[other];
                let inv = inv_inertia_world(grid_body);
                let r = closest - grid_body.position;
                k += k_term(grid_body, &inv, r, c.normal);
                (other, inv, r)
            });
            let impulse = c.normal.scale(correction / k);
            {
                let body = &mut bodies[c.body_index];
                body.position += impulse.scale(body.inv_mass);
                body.orientation = (FixedQuat::from_scaled_axis(inv_own * r_own.cross(impulse))
                    * body.orientation)
                    .normalize();
            }
            if let Some((other, inv, r)) = other_side {
                let body = &mut bodies[other];
                body.position -= impulse.scale(body.inv_mass);
                body.orientation = (FixedQuat::from_scaled_axis(-(inv * r.cross(impulse)))
                    * body.orientation)
                    .normalize();
            }
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
