//! P1 acceptance (docs/plans/voxel-physics.md §6 P1 + amendments):
//! ballistic closed form, rotation integration, the exact energy-drift
//! slope, canonical spawn order.

use monada_fixed::{trig, Fixed, FixedMat3, FixedQuat, FixedVec3};
use monada_physics::{BodyDef, EmptyField, PhysicsWorld};

/// Assert two `Fixed` are within `eps` raw steps of each other.
fn close(a: Fixed, b: Fixed, eps_bits: i64) {
    let d = (a.to_bits() - b.to_bits()).abs();
    assert!(d <= eps_bits, "‖{a:?} - {b:?}‖ = {d} bits > {eps_bits}");
}

fn close_vec(a: FixedVec3, b: FixedVec3, eps_bits: i64) {
    close(a.x, b.x, eps_bits);
    close(a.y, b.y, eps_bits);
    close(a.z, b.z, eps_bits);
}

fn vec3(x: i32, y: i32, z: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::from_int(z))
}

const GRAVITY: FixedVec3 = FixedVec3 {
    x: Fixed::ZERO,
    y: Fixed::ZERO,
    z: Fixed::from_int(-10),
};

/// A ballistic world with one body: `v0 = (5, 3, 20)`, spawned at
/// `(0, 0, 100)`, 25 Hz.
fn ballistic_world() -> PhysicsWorld {
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(GRAVITY);
    world.spawn(&BodyDef {
        position: vec3(0, 0, 100),
        linear_velocity: vec3(5, 3, 20),
        ..BodyDef::default()
    });
    world
}

/// Velocity against the discrete closed form — **bit-exact**: the sim
/// adds the same rounded `g·dt` every tick and `Fixed` addition is
/// exact, so `v_n = v0 + n·(g·dt)` with no tolerance at all (the
/// `int × Fixed` product is exact too). Runs to 4000 ticks — well
/// below the `max_speed` ceiling (2000 voxels/s ≈ tick 5050 here),
/// where the P2 clamp starts rescaling; saturation itself is covered
/// by `long_fall_saturates_at_max_speed`.
#[test]
fn ballistic_velocity_is_bit_exact() {
    let mut world = ballistic_world();
    let g_dt = GRAVITY.scale(world.dt());
    let v0 = world.bodies()[0].linear_velocity();
    for n in 1..=4_000i32 {
        world.step(&EmptyField);
        let expected = v0 + g_dt.scale(Fixed::from_int(n));
        assert_eq!(
            world.bodies()[0].linear_velocity(),
            expected,
            "velocity diverged from closed form at tick {n}"
        );
    }
}

/// Position against the discrete closed form
/// `p_n = p0 + v0·(n·dt) + (g·dt)·(dt·n(n+1)/2)`. Error budget: the sim
/// rounds `v·dt` once per tick (round-to-nearest, ≤ ½ ulp, bias-free)
/// → ≤ n/2 ulp; the closed form is ordered so no early rounding gets
/// amplified by a large factor (`dt·tri` first, *then* the small
/// `g·dt` and `v0` products) → ≤ ~½·max(|v0|, |g·dt|) ≈ 10 ulp.
/// Tolerance `n + 32` ulps; at 600 ticks that is ≈ 1.5e-7 voxels.
#[test]
fn ballistic_position_matches_closed_form() {
    let mut world = ballistic_world();
    let dt = world.dt();
    let g_dt = GRAVITY.scale(dt);
    let p0 = world.bodies()[0].position();
    let v0 = world.bodies()[0].linear_velocity();
    for n in 1..=600i32 {
        world.step(&EmptyField);
        // n(n+1)/2 is exact in i32 for n ≤ 600.
        let tri = Fixed::from_int(n * (n + 1) / 2);
        let expected = p0 + v0.scale(dt * Fixed::from_int(n)) + g_dt.scale(dt * tri);
        close_vec(world.bodies()[0].position(), expected, i64::from(n) + 32);
    }
}

/// The continuous parabola `z(t) = z0 + vz·t − 5t²` is an O(dt) model
/// of the discrete trajectory (global error `≤ ½·|g|·dt·t`). At 25 Hz
/// over 4 s that bound is 0.8 voxels; assert within it.
#[test]
fn ballistic_tracks_continuous_parabola() {
    let mut world = ballistic_world();
    for _ in 0..100 {
        world.step(&EmptyField);
    }
    // t = 4 s: z = 100 + 20·4 − 5·16 = 100.
    let z = world.bodies()[0].position().z;
    let err = (z - Fixed::from_int(100)).abs();
    assert!(
        err <= Fixed::from_ratio(4, 5),
        "z off the parabola by {err:?} voxels (> 0.8)"
    );
}

/// Constant ω about +z for n ticks lands within a documented bound of
/// the one-shot rotation `from_axis_angle(z, ω·n·dt)`. Kept under one
/// revolution (0.5 rad/s · 250 ticks · 0.04 s = 5 rad < 2π) so this
/// tests the integrator, not the LUT's angle reduction (plan, P1
/// amendments). Per-tick LUT + renormalize error is ~2⁻²⁰ and
/// accumulates linearly; bound: 2⁻¹² per component (`1 << 20`) at
/// n = 250.
#[test]
fn rotation_integration_matches_one_shot() {
    let mut world = PhysicsWorld::new(25);
    let omega_z = Fixed::from_ratio(1, 2);
    world.spawn(&BodyDef {
        angular_velocity: FixedVec3::new(Fixed::ZERO, Fixed::ZERO, omega_z),
        ..BodyDef::default()
    });
    let dt = world.dt();
    for n in 1..=250i32 {
        world.step(&EmptyField);
        let angle = omega_z * dt * Fixed::from_int(n);
        let expected = FixedQuat::from_axis_angle(vec3(0, 0, 1), angle);
        let got = world.bodies()[0].orientation();
        // Both paths trace the same continuous curve on the quat
        // sphere (no hemisphere flips below one full revolution), so
        // components compare directly.
        close(got.x, expected.x, 1 << 20);
        close(got.y, expected.y, 1 << 20);
        close(got.z, expected.z, 1 << 20);
        close(got.w, expected.w, 1 << 20);
    }
}

/// Renormalization holds the quaternion on the unit sphere over 10k
/// ticks: `|‖q‖² − 1| ≤ 2⁻²⁰` at every tick (a couple of rounding ulps
/// past the normalize; without the renormalize this drifts without
/// bound).
#[test]
fn rotation_norm_is_retained_over_10k_ticks() {
    let mut world = PhysicsWorld::new(25);
    world.spawn(&BodyDef {
        // A skew axis so all four components stay in play.
        angular_velocity: FixedVec3::new(
            Fixed::from_ratio(1, 3),
            Fixed::from_ratio(-1, 2),
            Fixed::from_ratio(2, 3),
        ),
        ..BodyDef::default()
    });
    for _ in 0..10_000 {
        world.step(&EmptyField);
        let q = world.bodies()[0].orientation();
        close(q.length_squared(), Fixed::ONE, 1 << 12);
    }
}

/// The exact energy-drift law of semi-implicit Euler in a uniform
/// field (plan, P1 amendments): `E_n − E_0 = −½·m·|g|²·dt²·n`,
/// linear with a known slope — not a measured envelope. Specific
/// energy is used (m = 1); tolerance `4n` ulps covers the per-tick
/// rounding of the two dot products and the height fold. Runs to
/// 4000 ticks — below the `max_speed` ceiling, past which velocity
/// saturates and drift becomes trivially bounded instead of linear.
#[test]
fn energy_drift_matches_exact_slope() {
    let mut world = ballistic_world();
    let dt = world.dt();
    // −½·|g|²·dt² per tick, in specific-energy units.
    let g_sq = GRAVITY.dot(GRAVITY);
    let slope = -(g_sq * dt * dt) / Fixed::from_int(2);
    let energy = |world: &PhysicsWorld| {
        let b = &world.bodies()[0];
        let v = b.linear_velocity();
        // E/m = ½|v|² − g·p (uniform-field potential).
        v.dot(v) / Fixed::from_int(2) - GRAVITY.dot(b.position())
    };
    let e0 = energy(&world);
    for n in 1..=4_000i32 {
        world.step(&EmptyField);
        let drift = energy(&world) - e0;
        let expected = slope * Fixed::from_int(n);
        close(drift, expected, i64::from(4 * n));
    }
}

/// Without gravity, kinetic energy is bit-exact forever: `v` never
/// changes, and constant ω leaves rotational KE untouched by
/// construction (no gyroscopic term — plan, P1 amendments).
#[test]
fn kinetic_energy_is_exact_without_gravity() {
    let mut world = PhysicsWorld::new(25);
    world.spawn(&BodyDef {
        linear_velocity: vec3(7, -2, 3),
        angular_velocity: vec3(1, 2, -1),
        ..BodyDef::default()
    });
    let v0 = world.bodies()[0].linear_velocity();
    let w0 = world.bodies()[0].angular_velocity();
    for _ in 0..10_000 {
        world.step(&EmptyField);
    }
    assert_eq!(world.bodies()[0].linear_velocity(), v0);
    assert_eq!(world.bodies()[0].angular_velocity(), w0);
}

/// Spawn order is the id order is the iteration order; two worlds
/// built identically agree bit-for-bit.
#[test]
fn spawn_order_is_canonical() {
    let build = || {
        let mut world = PhysicsWorld::new(25);
        world.set_gravity(GRAVITY);
        let a = world.spawn(&BodyDef {
            position: vec3(1, 0, 0),
            ..BodyDef::default()
        });
        let b = world.spawn(&BodyDef {
            position: vec3(2, 0, 0),
            ..BodyDef::default()
        });
        (world, a, b)
    };
    let (mut w1, a, b) = build();
    let (mut w2, ..) = build();
    assert_eq!(a.0, 0);
    assert_eq!(b.0, 1);
    assert_eq!(
        w1.bodies()
            .iter()
            .map(monada_physics::RigidBody::id)
            .collect::<Vec<_>>(),
        vec![a, b]
    );
    assert_eq!(w1.body(a).unwrap().position(), vec3(1, 0, 0));
    for _ in 0..100 {
        w1.step(&EmptyField);
        w2.step(&EmptyField);
    }
    assert_eq!(w1.state_hash(), w2.state_hash());
}

/// Spawn rejects garbage loudly: non-positive mass and a
/// non-positive-definite inertia tensor.
#[test]
#[should_panic(expected = "mass must be positive")]
fn spawn_rejects_zero_mass() {
    PhysicsWorld::new(25).spawn(&BodyDef {
        mass: Fixed::ZERO,
        ..BodyDef::default()
    });
}

#[test]
#[should_panic(expected = "positive-definite")]
fn spawn_rejects_indefinite_inertia() {
    PhysicsWorld::new(25).spawn(&BodyDef {
        inertia_body: FixedMat3::from_diagonal(vec3(1, -1, 1)),
        ..BodyDef::default()
    });
}

/// A non-unit orientation is normalised at spawn, not trusted.
#[test]
fn spawn_normalizes_orientation() {
    let mut world = PhysicsWorld::new(25);
    let doubled = FixedQuat::new(
        Fixed::ZERO,
        Fixed::ZERO,
        trig::sin(Fixed::from_ratio(1, 2)) * Fixed::from_int(2),
        trig::cos(Fixed::from_ratio(1, 2)) * Fixed::from_int(2),
    );
    let id = world.spawn(&BodyDef {
        orientation: doubled,
        ..BodyDef::default()
    });
    close(
        world.body(id).unwrap().orientation().length_squared(),
        Fixed::ONE,
        1 << 12,
    );
}

/// An angular impulse turns without shoving — the couple an off-centre
/// linear impulse cannot express (docs/plans/ship-physics.md D6, the gyro
/// / RCS primitive). On the default unit-inertia body `Δω = L` exactly,
/// and the centre of mass must not move at all.
#[test]
fn an_angular_impulse_turns_without_shoving() {
    let mut world = PhysicsWorld::new(25);
    let id = world.spawn(&BodyDef::default());
    world.apply_angular_impulse(id, vec3(0, 0, 2));

    let body = world.body(id).unwrap();
    close_vec(body.angular_velocity(), vec3(0, 0, 2), 1 << 12);
    assert_eq!(
        body.linear_velocity(),
        FixedVec3::ZERO,
        "a couple imparts no linear velocity"
    );

    // …and it keeps turning: nothing damps a free body in vacuum.
    for _ in 0..10 {
        world.step(&EmptyField);
    }
    close_vec(
        world.body(id).unwrap().angular_velocity(),
        vec3(0, 0, 2),
        1 << 12,
    );
    assert_eq!(
        world.body(id).unwrap().position(),
        FixedVec3::ZERO,
        "and the body never leaves the spot it spun on"
    );
}

/// Every external mutation wakes its body (P5). A stabiliser that fired
/// into a sleeping hull and did nothing would be the worst kind of bug:
/// correct code, no effect, no error.
#[test]
fn an_angular_impulse_wakes_a_sleeping_body() {
    let mut world = PhysicsWorld::new(25);
    // A REAL body, not a ghost: sleep needs a collision skin (a skinless
    // body is the hover-cart case and never sleeps).
    world.register_material(monada_physics::Material {
        density: Fixed::ONE,
        friction: Fixed::from_ratio(1, 2),
        restitution: Fixed::ZERO,
        hardness: Fixed::ONE,
    });
    let mut shape = monada_physics::VoxelShape::new(2, 2, 2);
    shape.fill_box((0, 0, 0), (1, 1, 1), monada_physics::MaterialId(0));
    let id = world.spawn_voxels(&monada_physics::VoxelBodyDef {
        shape,
        position: FixedVec3::ZERO,
        orientation: FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    for _ in 0..=monada_physics::SLEEP_TICKS {
        world.step(&EmptyField);
    }
    assert!(world.body(id).unwrap().asleep(), "a still body sleeps");

    world.apply_angular_impulse(id, vec3(0, 0, 1));
    assert!(!world.body(id).unwrap().asleep(), "and a torque wakes it");
}
