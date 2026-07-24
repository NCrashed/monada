//! Raycast wheel suspension (docs/plans/voxel-physics.md §3 `wheels`,
//! P3 amendments).
//!
//! A wheel is a chassis attachment, not a body: no spin DOF, no
//! dynamic state at all — compression and damping derive from the
//! current raycast and the body's velocity, so wheels carry nothing
//! across ticks but their definition and the retained input. Drive
//! torque becomes a longitudinal contact force (`τ/radius`) directly;
//! the render side derives wheel rotation from ground speed.
//!
//! Per tick, per wheel (all impulses at the raycast **hit point** —
//! lateral friction below the `CoM` is what rolls a top-heavy vehicle
//! over, and that is a feature):
//!
//! ```text
//! d = R·(0,0,−1);  DDA(anchor_world, d, rest_length + radius) → hit?
//! x = clamp(rest_length − (t − radius), 0, rest_length)
//! N = max(0, k·x + c·((v_hit − g·dt)·d))   — damper ADDS on compression,
//!                                            sampled pre-gravity
//! J_susp = n·(N·(−d·n)·dt)                 — along the CONTACT NORMAL,
//!                                            cosine-projected
//! f = (R·Rz(steer)·x̂) projected ⊥ n; skip friction if ~zero
//! l = n × f
//! (J_long, J_lat) → friction circle: clamped as a 2-vector to μ·N·dt
//! ```
//!
//! The whole pass reads a **pre-pass velocity snapshot**: no wheel
//! sees another wheel's impulse, so the physics is independent of
//! `WheelId` order (which remains pure iteration/hash canonicality).
//! See [`wheel_pass`] for the two stabilizers that make the bulk
//! (Jacobi) application behave: load-weighted slip kills and the
//! static-friction feed-forward.

use monada_fixed::{trig, Fixed, FixedVec3};
use monada_sim::{StateHash, StateHasher};

use crate::body::RigidBody;
use crate::field::VoxelField;
use crate::material::Material;
use crate::raycast;
use crate::solver;

/// A wheel's identity within its body: monotonic per body, never
/// reused (detached ids stay retired).
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct WheelId(pub u32);

/// Spawn-time wheel description. Units: lengths in voxels, stiffness
/// in force per voxel of compression, damping in force per (voxel/s)
/// of compression rate (force = mass·voxel/s², as everywhere).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WheelDef {
    /// Attachment point on the chassis, body frame (CoM-relative).
    pub anchor: FixedVec3,
    /// Suspension travel from the anchor along body-down.
    pub rest_length: Fixed,
    /// Wheel radius; the ray reaches `rest_length + radius`.
    pub radius: Fixed,
    /// Spring constant `k`.
    pub stiffness: Fixed,
    /// Damper constant `c` (opposes compression *and* rebound).
    pub damping: Fixed,
    /// Tire μ; combined with terrain as `√(μ_tire·μ_terrain)`.
    pub friction: Fixed,
}

/// Retained per-tick control input (hashed state, not an event: it
/// stays until the next `set_wheel_input` — the lockstep model).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WheelInput {
    /// Steering angle, radians about body z; 0 = body +x.
    pub steer: Fixed,
    /// Drive torque; sign picks the direction.
    pub drive: Fixed,
    /// Braking force ceiling (≥ 0): clamps the impulse that opposes
    /// rolling.
    pub brake: Fixed,
}

impl Default for WheelInput {
    fn default() -> WheelInput {
        WheelInput {
            steer: Fixed::ZERO,
            drive: Fixed::ZERO,
            brake: Fixed::ZERO,
        }
    }
}

/// One attached wheel: definition + retained input. All fields are
/// hashed (no derived caches, no dynamic state).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Wheel {
    pub(crate) id: WheelId,
    pub(crate) def: WheelDef,
    pub(crate) input: WheelInput,
}

impl Wheel {
    #[must_use]
    pub fn id(&self) -> WheelId {
        self.id
    }

    #[must_use]
    pub fn def(&self) -> &WheelDef {
        &self.def
    }

    /// The retained control input (last `set_wheel_input`).
    #[must_use]
    pub fn input(&self) -> &WheelInput {
        &self.input
    }
}

impl StateHash for Wheel {
    /// Canonical fold: id, definition fields, input fields.
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(u64::from(self.id.0));
        self.def.anchor.hash(h);
        self.def.rest_length.hash(h);
        self.def.radius.hash(h);
        self.def.stiffness.hash(h);
        self.def.damping.hash(h);
        self.def.friction.hash(h);
        self.input.steer.hash(h);
        self.input.drive.hash(h);
        self.input.brake.hash(h);
    }
}

/// Run the wheel pass for every body (canonical order: bodies
/// ascending by id, wheels by `WheelId`). See the module docs for the
/// per-wheel math and the snapshot rule.
///
/// Two stabilizers on top of the plain per-wheel math (P3 amendments):
///
/// - **Load-weighted slip kill.** Impulses are bulk-applied from one
///   snapshot (Jacobi, for order independence) — but K wheels each
///   killing the *body's* shared slip from the same snapshot would
///   overcorrect K-fold and ring the roll axis. Constraint-type
///   impulses (lateral kill, brake) are therefore weighted by each
///   wheel's load share `N_i/ΣN`: symmetric stances sum to exactly one
///   full kill, asymmetric ones under-correct and converge over ticks.
///   The drive term is a real force, not a constraint — never scaled.
/// - **The damper samples pre-gravity velocity** (`v − g·dt`). The
///   integrator adds gravity before the wheel pass, so the raw
///   velocity always carries one tick's gravity kick; a damper fed
///   that kick fights gravity permanently and shifts the static ride
///   height by `c·|g|·dt/k` per wheel. Subtracting it makes stiffness
///   alone set the ride height (`x = m·g/(4k)`), which is what a
///   designer expects.
pub(crate) fn wheel_pass(
    bodies: &mut [RigidBody],
    field: &dyn VoxelField,
    materials: &[Material],
    gravity_dt: FixedVec3,
    dt: Fixed,
) {
    // Per-wheel raycast results for the current body (scratch).
    struct Grounded {
        wheel_index: usize,
        cell: (i64, i64, i64),
        normal: FixedVec3,
        r: FixedVec3,
        normal_force: Fixed,
    }

    // Index loop, not an iterator: the body is read immutably through
    // both passes and mutated only at the bulk-apply tail.
    #[allow(clippy::needless_range_loop)]
    for body_index in 0..bodies.len() {
        if bodies[body_index].wheels.is_empty() || bodies[body_index].asleep {
            continue;
        }
        let body = &bodies[body_index];
        // Pre-pass snapshot: every wheel computes from these.
        let v0 = body.linear_velocity;
        let w0 = body.angular_velocity;
        let position = body.position;
        let orientation = body.orientation;
        let inv_inertia = solver::inv_inertia_world(body);
        let down = orientation * FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::NEG_ONE);

        // Pass 1: raycasts + suspension forces (also the load sum for
        // the slip-kill weights).
        let mut grounded: Vec<Grounded> = Vec::new();
        let mut total_load = Fixed::ZERO;
        for (wheel_index, wheel) in body.wheels.iter().enumerate() {
            let anchor = position + orientation * wheel.def.anchor;
            let max_t = wheel.def.rest_length + wheel.def.radius;
            let Some(hit) = raycast::cast(field, anchor, down, max_t) else {
                continue;
            };
            let compression = (wheel.def.rest_length - (hit.t - wheel.def.radius))
                .clamp(Fixed::ZERO, wheel.def.rest_length);
            let hit_point = anchor + down.scale(hit.t);
            let r = hit_point - position;
            // Damper rate from pre-gravity velocity (see above).
            let damp_rate = (v0 - gravity_dt + w0.cross(r)).dot(down);
            let normal_force = (wheel.def.stiffness * compression + wheel.def.damping * damp_rate)
                .max(Fixed::ZERO);
            if normal_force == Fixed::ZERO {
                continue;
            }
            total_load += normal_force;
            grounded.push(Grounded {
                wheel_index,
                cell: hit.cell,
                normal: hit.normal,
                r,
                normal_force,
            });
        }

        // Suspension impulses act along the CONTACT NORMAL, scaled by
        // the ray↔normal cosine (P3 amendments, revised): compression
        // and damping measure along the ray, but the force is the
        // surface's reaction. Pushing along the ray itself self-locks
        // tall vehicles — drive torque pitches the body, the tilted
        // spring axis gains a tangential component that cancels the
        // drive (100% anti-squat), and on stair treads it shoves a
        // pitched body downhill.
        let mut suspension: Vec<FixedVec3> = Vec::with_capacity(grounded.len());
        let mut suspension_total = FixedVec3::ZERO;
        for g in &grounded {
            let cos = (-down.dot(g.normal)).max(Fixed::ZERO);
            let j = g.normal.scale(g.normal_force * cos * dt);
            suspension_total += j;
            suspension.push(j);
        }
        // Static-friction feed-forward (P3 amendments): the velocity
        // kills only see the snapshot, and the suspension impulses are
        // applied AFTER it — any tangential component they retain
        // (riser faces, mixed normals) would re-feed a creep every
        // tick that no velocity kill can hold. Tires react that
        // *known* impulse up front: laterally always, longitudinally
        // under braking. Gravity's feed is already inside the snapshot
        // and is NOT included (that would double-count).

        // Pass 2: tire impulses from the same snapshot, kills weighted
        // by load share.
        let mut impulses: Vec<(FixedVec3, FixedVec3)> = Vec::new();
        for (g_index, g) in grounded.iter().enumerate() {
            let wheel = &body.wheels[g.wheel_index];
            let mut impulse = suspension[g_index];

            let terrain_mat = field.material(g.cell.0, g.cell.1, g.cell.2);
            let terrain = materials
                .get(usize::from(terrain_mat.0))
                .unwrap_or_else(|| {
                    panic!(
                        "terrain returned material {}, world has {} registered",
                        terrain_mat.0,
                        materials.len()
                    )
                });
            let steer_cos = trig::cos(wheel.input.steer);
            let steer_sin = trig::sin(wheel.input.steer);
            let forward_body = FixedVec3::new(steer_cos, steer_sin, Fixed::ZERO);
            let forward = (orientation * forward_body).reject(g.normal).normalize();
            // Degenerate steer (near-vertical face): deterministic
            // skip, never a zero-normalize (P3 amendments).
            if forward != FixedVec3::ZERO {
                let lateral = g.normal.cross(forward);
                let v_hit = v0 + w0.cross(g.r);
                let weight = g.normal_force / total_load;
                let mu = (wheel.def.friction * terrain.friction).sqrt();
                // NB (P5/P6 follow-up, plan §6): the budget and the
                // kill weights use the raw ray-compression N — a wheel
                // whose ray hit a vertical riser face gets cos ≈ 0
                // suspension push yet still claims friction and a
                // load share, gripping a face nothing presses it
                // into. Harmless on treads (today's scenarios); for
                // walls/3D terrain weigh `N_eff = N·cos` here and in
                // `total_load`.
                let budget = mu * g.normal_force * dt;
                // Longitudinal: drive force (unscaled) + weighted
                // brake kill against rolling + brake-gated
                // feed-forward of the suspension push (see above).
                let v_long = v_hit.dot(forward);
                let m_long = solver::effective_mass(body, &inv_inertia, g.r, forward);
                let brake_cap = wheel.input.brake * dt;
                let j_long = (wheel.input.drive / wheel.def.radius) * dt
                    + ((-v_long * m_long - suspension_total.dot(forward)) * weight)
                        .clamp(-brake_cap, brake_cap);
                // Lateral: weighted slip kill + feed-forward (tires
                // always resist sideways).
                let m_lat = solver::effective_mass(body, &inv_inertia, g.r, lateral);
                let j_lat = (-v_hit.dot(lateral) * m_lat - suspension_total.dot(lateral)) * weight;
                // Friction circle.
                let tire = FixedVec3::new(j_long, j_lat, Fixed::ZERO).clamp_length_max(budget);
                impulse += forward.scale(tire.x) + lateral.scale(tire.y);
            }
            impulses.push((impulse, g.r));
        }

        // Bulk apply — after every wheel has read the snapshot.
        let body = &mut bodies[body_index];
        for (impulse, r) in impulses {
            body.linear_velocity += impulse.scale(body.inv_mass);
            body.angular_velocity += inv_inertia * r.cross(impulse);
        }
    }
}
