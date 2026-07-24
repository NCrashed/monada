//! P2 acceptance (docs/plans/voxel-physics.md §6 P2 + amendments):
//! voxel mass derivation, skin correctness, the drop test, Coulomb
//! friction under tilted gravity, impulses, the speed clamp.

use monada_fixed::{Fixed, FixedVec3};
use monada_physics::{
    BodyDef, EmptyField, Material, MaterialId, PhysicsWorld, VoxelBodyDef, VoxelField, VoxelShape,
    SLEEP_ANGULAR, SLEEP_LINEAR,
};

/// Assert two `Fixed` are within `eps` raw steps of each other.
fn close(a: Fixed, b: Fixed, eps_bits: i64) {
    let d = (a.to_bits() - b.to_bits()).abs();
    assert!(d <= eps_bits, "‖{a:?} - {b:?}‖ = {d} bits > {eps_bits}");
}

fn vec3(x: i32, y: i32, z: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::from_int(z))
}

/// Flat terrain: every cell below z = 0 is solid, material 0.
struct Floor;

impl VoxelField for Floor {
    fn occupied(&self, _x: i64, _y: i64, z: i64) -> bool {
        z < 0
    }
    fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
        MaterialId(0)
    }
}

/// The default test material: unit density, μ as given, e = 0.
fn material(friction_num: i32, friction_den: i32) -> Material {
    Material {
        density: Fixed::ONE,
        friction: Fixed::from_ratio(friction_num, friction_den),
        restitution: Fixed::ZERO,
    }
}

/// A world at 25 Hz with straight-down gravity −10 and one registered
/// material; returns (world, material id).
fn world_with(friction_num: i32, friction_den: i32) -> (PhysicsWorld, MaterialId) {
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(vec3(0, 0, -10));
    let mat = world.register_material(material(friction_num, friction_den));
    (world, mat)
}

/// An n³ cube shape of one material.
fn cube_shape(n: i32, mat: MaterialId) -> VoxelShape {
    let mut shape = VoxelShape::new(n, n, n);
    shape.fill_box((0, 0, 0), (n - 1, n - 1, n - 1), mat);
    shape
}

/// Mass, `CoM`, and inertia of a uniform n³ cube against the discrete
/// closed forms of the solid-cube convention (plan, P2 amendments):
/// `M = ρn³`, `CoM` at the geometric centre, and per axis
/// `I = M·(Σ_pairwise d² terms)` — for a uniform cube the discrete sum
/// collapses to `I = M·(n²−1)/6 + M/6 = M·n²/6` … exactly the
/// *continuous* cube value, because voxel own-terms plus the discrete
/// parallel-axis sum reproduce it: Σ over a 1-D lattice of k² spacings
/// is `n²(n²−1)/12` per pair of axes, ×2 + n·(1/6) per voxel.
#[test]
fn cube_mass_properties_match_closed_form() {
    for n in [1i32, 2, 3, 4] {
        let (mut world, mat) = world_with(1, 2);
        let id = world.spawn_voxels(&VoxelBodyDef {
            shape: cube_shape(n, mat),
            position: FixedVec3::ZERO,
            orientation: monada_fixed::FixedQuat::IDENTITY,
            linear_velocity: FixedVec3::ZERO,
            angular_velocity: FixedVec3::ZERO,
        });
        let body = world.body(id).unwrap();
        let m = Fixed::from_int(n * n * n);
        assert_eq!(body.mass(), m, "mass of {n}³ cube");
        // I = M·n²/6 per axis (uniform density 1, solid-cube terms).
        let expected = m * Fixed::from_int(n * n) / Fixed::from_int(6);
        let i = body.inertia_body();
        // Diagonal within accumulated rounding of the voxel sum
        // (n³ terms, each a couple of ulps): 4n³ ulp bound.
        let eps = 4 * i64::from(n).pow(3);
        close(i.x_axis.x, expected, eps);
        close(i.y_axis.y, expected, eps);
        close(i.z_axis.z, expected, eps);
        // Off-diagonals vanish by symmetry.
        close(i.x_axis.y, Fixed::ZERO, eps);
        close(i.x_axis.z, Fixed::ZERO, eps);
        close(i.y_axis.z, Fixed::ZERO, eps);
    }
}

/// The single voxel — the boundary case where the point-mass and
/// solid-cube conventions diverge: point masses give a singular
/// tensor, the cube term gives `ρ/6·E` (SPD, spawnable).
#[test]
fn single_voxel_has_cube_inertia() {
    let (mut world, mat) = world_with(1, 2);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape: cube_shape(1, mat),
        position: FixedVec3::ZERO,
        orientation: monada_fixed::FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let body = world.body(id).unwrap();
    assert_eq!(body.mass(), Fixed::ONE);
    assert_eq!(body.skin_len(), 1);
    let sixth = Fixed::ONE / Fixed::from_int(6);
    close(body.inertia_body().x_axis.x, sixth, 4);
    close(body.inertia_body().y_axis.y, sixth, 4);
    close(body.inertia_body().z_axis.z, sixth, 4);
}

/// A 1×1×n rod: zero moment about its own axis under point masses —
/// the other input the cube convention rescues.
#[test]
fn rod_spawns_with_positive_axial_inertia() {
    let (mut world, mat) = world_with(1, 2);
    let mut shape = VoxelShape::new(1, 1, 5);
    shape.fill_box((0, 0, 0), (0, 0, 4), mat);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape,
        position: FixedVec3::ZERO,
        orientation: monada_fixed::FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    // Axial (z) moment = M/6 from the own-terms alone.
    let axial = world.body(id).unwrap().inertia_body().z_axis.z;
    close(axial, Fixed::from_int(5) / Fixed::from_int(6), 8);
}

/// Two-material body: the `CoM` shifts towards the denser half. Also
/// the ground truth P4's incremental recompute will be checked
/// against.
#[test]
fn two_material_com_shifts_to_denser_side() {
    let mut world = PhysicsWorld::new(25);
    let light = world.register_material(Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
    });
    let heavy = world.register_material(Material {
        density: Fixed::from_int(3),
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
    });
    // 2×1×1: light at x∈[0,1), heavy at x∈[1,2).
    let mut shape = VoxelShape::new(2, 1, 1);
    shape.set(0, 0, 0, light);
    shape.set(1, 0, 0, heavy);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape,
        position: FixedVec3::ZERO,
        orientation: monada_fixed::FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let body = world.body(id).unwrap();
    assert_eq!(body.mass(), Fixed::from_int(4));
    // CoM_x in shape coords: (1·0.5 + 3·1.5)/4 = 1.25 → 0.25 beyond
    // the geometric centre (1.0). `position` is the CoM, so the shape
    // sits asymmetrically around it — verified via the skin: the two
    // sphere offsets are −1.25+0.5 = −0.75 (light) and +0.25 (heavy).
    // Observable through shape(): still the as-authored grid.
    assert!(body.shape().is_some());
    // Indirect but public check: spin it about z and the light end
    // sweeps a wider arc — inertia about z must exceed the symmetric
    // ρ=2 case's value... simpler: the closed form. I_z = Σ ρ(dx²)+ρ/6:
    // light d=−0.75: 1·0.5625; heavy d=+0.25: 3·0.0625 → 0.75 + 4/6.
    let expected = Fixed::from_ratio(3, 4) + Fixed::from_int(4) / Fixed::from_int(6);
    close(body.inertia_body().z_axis.z, expected, 16);
}

/// Skin size: for an n³ cube all boundary voxels are surface —
/// n³ − (n−2)³; a hollow interior is excluded.
#[test]
fn skin_is_surface_voxels_only() {
    for n in [1i32, 2, 3, 4, 5] {
        let (mut world, mat) = world_with(1, 2);
        let id = world.spawn_voxels(&VoxelBodyDef {
            shape: cube_shape(n, mat),
            position: FixedVec3::ZERO,
            orientation: monada_fixed::FixedQuat::IDENTITY,
            linear_velocity: FixedVec3::ZERO,
            angular_velocity: FixedVec3::ZERO,
        });
        let interior = (n - 2).max(0).pow(3);
        let expected = usize::try_from(n.pow(3) - interior).expect("positive");
        assert_eq!(
            world.body(id).unwrap().skin_len(),
            expected,
            "skin of {n}³ cube"
        );
    }
}

/// The P2 headline: a 4³ cube dropped from z = 10 onto the flat floor
/// comes to rest and stays. Documented settle bound: rest (both
/// speeds under the sleep thresholds) within 100 ticks (4 s) — the
/// free fall alone is ~32 ticks; observed settle is well inside. The
/// resting height is floor top + half-height = 2, within NGS slop
/// (penetration ≤ SLOP + margin ≈ 0.02).
#[test]
fn dropped_cube_comes_to_rest_and_stays() {
    let (mut world, mat) = world_with(1, 2);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape: cube_shape(4, mat),
        position: vec3(0, 0, 10),
        orientation: monada_fixed::FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let at_rest = |world: &PhysicsWorld| {
        let b = world.body(id).unwrap();
        b.linear_velocity().length() < SLEEP_LINEAR && b.angular_velocity().length() < SLEEP_ANGULAR
    };
    let mut settled_at = None;
    for tick in 1..=100 {
        world.step(&Floor);
        if settled_at.is_none() && at_rest(&world) {
            settled_at = Some(tick);
        }
    }
    let settled_at = settled_at.expect("cube settles within 100 ticks");
    // Stays at rest: 500 more ticks below the thresholds, height
    // stable at 2 within a generous contact tolerance.
    for _ in 0..500 {
        world.step(&Floor);
        assert!(at_rest(&world), "woke after settling at {settled_at}");
    }
    let z = world.body(id).unwrap().position().z;
    close(z, Fixed::from_int(2), 1 << 27); // ±2⁻⁵ = ±0.03 voxels
}

/// Restitution 0: after the first ground contact the cube's peak
/// height never exceeds the contact-tick height — catches energy
/// pumping of any origin (solver bias, NGS overshoot).
#[test]
fn zero_restitution_does_not_bounce() {
    let (mut world, mat) = world_with(1, 2);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape: cube_shape(2, mat),
        position: vec3(0, 0, 8),
        orientation: monada_fixed::FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let mut touched = false;
    let mut ceiling = Fixed::ZERO;
    for _ in 0..300 {
        world.step(&Floor);
        let b = world.body(id).unwrap();
        // Strict `>`: in free fall vz equals g·dt·n exactly (see the
        // P1 bit-exact velocity law), so only a solver impulse can
        // lift it strictly above one tick's worth of gravity.
        if !touched && b.linear_velocity().z > Fixed::from_int(-10) * world.dt() {
            // Normal impulse fired this tick (fall speed absorbed).
            touched = true;
            ceiling = b.position().z + Fixed::from_ratio(1, 16);
        }
        if touched {
            assert!(
                b.position().z <= ceiling,
                "bounced above first-contact height: {:?} > {ceiling:?}",
                b.position().z
            );
        }
    }
    assert!(touched, "never reached the floor");
}

/// Coulomb friction under 30°-tilted gravity over the flat floor
/// (plan, P2 amendments): μ = 0.7 > tan 30° ≈ 0.577 holds; μ = 0.3
/// slides. Same material on body and terrain keeps √(μ·μ) = μ. The
/// tilted g = 10·(sin 30°, 0, −cos 30°) = (5, 0, −8.660254…).
///
/// The body is a 6×6×2 slab, not a cube: the sphere skin insets the
/// support polygon by its ½-voxel radius (see `shape` docs), so a 2³
/// cube's tipping threshold is 0.5 < tan 30° and it would (correctly!)
/// topple and tumble downhill. The slab's threshold is 2.5/1 = 2.5 —
/// tipping is out of the picture and the test isolates friction.
#[test]
fn friction_holds_or_slides_on_30_degrees() {
    let tilted = FixedVec3::new(
        Fixed::from_int(5),
        Fixed::ZERO,
        -Fixed::from_ratio(8_660_254, 1_000_000),
    );
    let run = |mu_num: i32, mu_den: i32| {
        let mut world = PhysicsWorld::new(25);
        world.set_gravity(tilted);
        let mat = world.register_material(material(mu_num, mu_den));
        let mut slab = VoxelShape::new(6, 6, 2);
        slab.fill_box((0, 0, 0), (5, 5, 1), mat);
        let id = world.spawn_voxels(&VoxelBodyDef {
            shape: slab,
            position: vec3(0, 0, 2),
            orientation: monada_fixed::FixedQuat::IDENTITY,
            linear_velocity: FixedVec3::ZERO,
            angular_velocity: FixedVec3::ZERO,
        });
        // Settle onto the floor first (a few ticks of straight drop).
        for _ in 0..25 {
            world.step(&Floor);
        }
        let x0 = world.body(id).unwrap().position().x;
        for _ in 0..500 {
            world.step(&Floor);
        }
        world.body(id).unwrap().position().x - x0
    };
    // μ = 0.7: static hold — drift under 0.1 voxel over 20 s.
    let held = run(7, 10);
    assert!(
        held.abs() < Fixed::from_ratio(1, 10),
        "μ=0.7 should hold on 30°, drifted {held:?}"
    );
    // μ = 0.3: slides — several voxels downhill (+x) over 20 s.
    let slid = run(3, 10);
    assert!(
        slid > Fixed::from_int(4),
        "μ=0.3 should slide on 30°, moved only {slid:?}"
    );
}

/// `apply_impulse`: Δv = J/m, bit-exact against the same arithmetic.
/// `apply_impulse_at`: Δω = `I⁻¹_world·(r` × J) for an axis-aligned body
/// — closed form within a few ulps.
#[test]
fn impulses_match_closed_form() {
    let (mut world, mat) = world_with(1, 2);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape: cube_shape(2, mat),
        position: vec3(0, 0, 10),
        orientation: monada_fixed::FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let mass = world.body(id).unwrap().mass(); // 8
    let j = vec3(16, -8, 4);
    world.apply_impulse(id, j);
    assert_eq!(
        world.body(id).unwrap().linear_velocity(),
        j.scale(Fixed::ONE / mass),
        "Δv = J/m"
    );
    // Off-centre impulse: r = (1, 0, 0), J = (0, 0, 8) → r×J = (0, −8, 0);
    // I per axis = M·n²/6 = 8·4/6 = 16/3 → Δω_y = −8·3/16 = −1.5.
    let before = world.body(id).unwrap().angular_velocity();
    world.apply_impulse_at(id, vec3(0, 0, 8), vec3(1, 0, 10));
    let delta = world.body(id).unwrap().angular_velocity() - before;
    close(delta.y, -Fixed::from_ratio(3, 2), 1 << 8);
    close(delta.x, Fixed::ZERO, 1 << 8);
    close(delta.z, Fixed::ZERO, 1 << 8);
}

/// A long fall saturates at `max_speed` exactly (clamp is a hard
/// ceiling, bit-stable once reached).
#[test]
fn long_fall_saturates_at_max_speed() {
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(vec3(0, 0, -10));
    world.spawn(&BodyDef::default());
    // 2000 voxels/s at g=10 needs 200 s = 5000 ticks; run 5200.
    for _ in 0..5_200 {
        world.step(&EmptyField);
    }
    let v = world.bodies()[0].linear_velocity();
    close(v.length(), Fixed::from_int(2000), 1 << 12);
    // The ceiling holds tick after tick (the clamp's rescale rounds a
    // few ulps differently each application — a hard bit-stable fixed
    // point is not promised, staying at the ceiling is).
    for _ in 0..10 {
        world.step(&EmptyField);
        close(
            world.bodies()[0].linear_velocity().length(),
            Fixed::from_int(2000),
            1 << 12,
        );
    }
}
