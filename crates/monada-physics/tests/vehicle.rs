//! P3 acceptance (docs/plans/voxel-physics.md §6 P3 + amendments):
//! suspension statics, drive/steer, the friction circle on ice, the
//! rollover pair, the lost wheel, the literal voxel staircase.

use monada_fixed::{Fixed, FixedVec3};
use monada_physics::{
    Material, MaterialId, PhysicsWorld, VoxelBodyDef, VoxelField, VoxelShape, WheelDef, WheelId,
    WheelInput,
};

fn close(a: Fixed, b: Fixed, eps_bits: i64) {
    let d = (a.to_bits() - b.to_bits()).abs();
    assert!(d <= eps_bits, "‖{a:?} - {b:?}‖ = {d} bits > {eps_bits}");
}

fn vec3(x: i32, y: i32, z: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::from_int(z))
}

fn fx(r: i32, d: i32) -> Fixed {
    Fixed::from_ratio(r, d)
}

/// Flat terrain below z = 0, material 0.
struct Floor;
impl VoxelField for Floor {
    fn occupied(&self, _x: i64, _y: i64, z: i64) -> bool {
        z < 0
    }
    fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
        MaterialId(0)
    }
}

/// Stairs along +x for x ≥ 0 (1 voxel rise per 2 run, ≈ 26.6°), flat
/// floor for x < 0. Material 0.
struct Stairs;
impl VoxelField for Stairs {
    fn occupied(&self, x: i64, _y: i64, z: i64) -> bool {
        z < x.div_euclid(2).clamp(0, 40)
    }
    fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
        MaterialId(0)
    }
}

const GRAVITY_Z: i32 = -10;

/// The standard test vehicle: 6×4×2 slab chassis (mass 48), four
/// wheels at the corners. Suspension: k = 240, c = 80 per wheel →
/// static compression m·g/(4k) = 0.5, ride height (`CoM`) = 2.5.
/// Returns (world, body, [fl, fr, rl, rr]).
fn vehicle(
    terrain_friction: (i32, i32),
    ballast_top: bool,
    ballast_bottom: bool,
) -> (PhysicsWorld, monada_physics::BodyId, [WheelId; 4]) {
    let z = if ballast_top { 5 } else { 3 };
    vehicle_at_impl(terrain_friction, ballast_top, ballast_bottom, vec3(0, 0, z))
}

/// The plain (unballasted) vehicle at an arbitrary spawn position.
fn vehicle_at(
    terrain_friction: (i32, i32),
    position: FixedVec3,
) -> (PhysicsWorld, monada_physics::BodyId, [WheelId; 4]) {
    vehicle_at_impl(terrain_friction, false, false, position)
}

fn vehicle_at_impl(
    terrain_friction: (i32, i32),
    ballast_top: bool,
    ballast_bottom: bool,
    position: FixedVec3,
) -> (PhysicsWorld, monada_physics::BodyId, [WheelId; 4]) {
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(vec3(0, 0, GRAVITY_Z));
    let ground = world.register_material(Material {
        density: Fixed::ONE,
        friction: fx(terrain_friction.0, terrain_friction.1),
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(50),
    });
    assert_eq!(ground, MaterialId(0), "terrain material is id 0");
    let dense = world.register_material(Material {
        density: Fixed::from_int(4),
        friction: fx(1, 2),
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(50),
    });

    // Chassis 6×4×2; optional dense ballast as a 2×2×2 tower on top
    // or in the bottom layer. The tower is deliberately moderate — a
    // taller one raises the CoM past the wheelie threshold for any
    // usable drive, and the resulting nose-up stance grinds the rear
    // skin on the floor (all correct physics, none of it the subject
    // under test).
    let (dx, dy, dz) = (6, 4, if ballast_top { 4 } else { 2 });
    let mut shape = VoxelShape::new(dx, dy, dz);
    shape.fill_box((0, 0, 0), (5, 3, 1), ground);
    if ballast_top {
        shape.fill_box((2, 1, 2), (3, 2, 3), dense);
    }
    if ballast_bottom {
        shape.fill_box((1, 1, 0), (4, 2, 0), dense);
    }

    let body = world.spawn_voxels(&VoxelBodyDef {
        shape,
        position,
        orientation: monada_fixed::FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });

    // Anchors are authored in SHAPE coordinates on the underside plane
    // (z_s = 0) and rebased through the derived CoM — with ballast the
    // CoM moves, and an anchor written directly in the body frame
    // would silently keep the CoM-to-ground distance constant across
    // variants (killing the rollover differential). Wheelbase ±3.5
    // (virtual anchors overhang the slab): pitch stiffness grows with
    // the base squared, keeping drive torque well under the wheelie
    // threshold. Track ±2.5 — wheels OUTSIDE the ±2 slab edge, so a
    // sliding-at-grip vehicle leans on its suspension, not on the
    // chassis skin (whose support polygon is inset to ±1.5 and trips
    // the body over — the P2 inset striking again).
    let com = world.body(body).unwrap().com_in_shape();
    let mut wheel = |sx: Fixed, sy: Fixed| {
        let def = WheelDef {
            anchor: FixedVec3::new(sx, sy, Fixed::ZERO) - com,
            rest_length: fx(3, 2),
            radius: Fixed::HALF,
            stiffness: Fixed::from_int(240),
            damping: Fixed::from_int(80),
            friction: fx(4, 5),
        };
        world.attach_wheel(body, &def)
    };
    let (front, rear) = (fx(13, 2), -fx(1, 2));
    let (left, right) = (fx(9, 2), -fx(1, 2));
    let fl = wheel(front, left);
    let fr = wheel(front, right);
    let rl = wheel(rear, left);
    let rr = wheel(rear, right);
    (world, body, [fl, fr, rl, rr])
}

fn drive_all(
    world: &mut PhysicsWorld,
    body: monada_physics::BodyId,
    w: &[WheelId; 4],
    input: WheelInput,
) {
    for id in w {
        world.set_wheel_input(body, *id, input);
    }
}

fn body_up_z(world: &PhysicsWorld, body: monada_physics::BodyId) -> Fixed {
    (world.body(body).unwrap().orientation() * vec3(0, 0, 1)).z
}

/// Suspension statics: the vehicle settles with per-wheel load
/// m·g/4 = 120 → compression 0.5 → `CoM` ride height 2.5, and stays
/// (damper sign is right — a flipped damper pumps the oscillator and
/// this never converges).
#[test]
fn suspension_settles_to_static_compression() {
    let (mut world, body, _) = vehicle((1, 2), false, false);
    for _ in 0..200 {
        world.step(&Floor);
    }
    let b = world.body(body).unwrap();
    // ±0.05 voxel: NGS-free equilibrium, spring vs gravity only.
    close(b.position().z, fx(5, 2), 1 << 27);
    assert!(b.linear_velocity().length() < fx(1, 20), "still moving");
    // Another 300 ticks: no oscillation growth.
    for _ in 0..300 {
        world.step(&Floor);
        assert!(
            world.body(body).unwrap().linear_velocity().length() < fx(1, 10),
            "suspension oscillating"
        );
    }
}

/// Constant drive torque on a flat floor: monotonic early acceleration
/// along body +x, no sideways drift (the bulk wheel pass is symmetric —
/// a sequential pass would pull toward the first wheel).
#[test]
fn drive_accelerates_forward_symmetrically() {
    let (mut world, body, wheels) = vehicle((1, 2), false, false);
    for _ in 0..100 {
        world.step(&Floor); // settle
    }
    drive_all(
        &mut world,
        body,
        &wheels,
        WheelInput {
            steer: Fixed::ZERO,
            drive: Fixed::from_int(30),
            brake: Fixed::ZERO,
        },
    );
    let mut prev_vx = world.body(body).unwrap().linear_velocity().x;
    for _ in 0..50 {
        world.step(&Floor);
        let v = world.body(body).unwrap().linear_velocity();
        assert!(v.x >= prev_vx, "acceleration not monotonic");
        prev_vx = v.x;
    }
    let b = world.body(body).unwrap();
    assert!(b.linear_velocity().x > Fixed::from_int(3), "too slow");
    assert!(
        b.linear_velocity().y.abs() < fx(1, 20),
        "parasitic sideways drift: {:?}",
        b.linear_velocity().y
    );
}

/// The same drive on ice — terrain μ = 1/50, so the *combined*
/// √(`μ_tire·μ_terrain`) collapses and the friction circle clamps the
/// drive force. Exercises the combining formula through the wheel
/// path: after the same 50 ticks the ice run is far slower.
#[test]
fn ice_clamps_drive_through_combined_friction() {
    let speed_after = |terrain: (i32, i32)| {
        let (mut world, body, wheels) = vehicle(terrain, false, false);
        for _ in 0..100 {
            world.step(&Floor);
        }
        drive_all(
            &mut world,
            body,
            &wheels,
            WheelInput {
                steer: Fixed::ZERO,
                drive: Fixed::from_int(30),
                brake: Fixed::ZERO,
            },
        );
        for _ in 0..50 {
            world.step(&Floor);
        }
        world.body(body).unwrap().linear_velocity().x
    };
    let asphalt = speed_after((1, 2));
    let ice = speed_after((1, 50));
    assert!(
        ice < asphalt / Fixed::from_int(2),
        "ice ({ice:?}) should be far slower than asphalt ({asphalt:?})"
    );
}

/// Drive + fixed steer: yaw accumulates with the steer sign.
#[test]
fn steer_turns_the_trajectory() {
    let (mut world, body, wheels) = vehicle((1, 2), false, false);
    for _ in 0..100 {
        world.step(&Floor);
    }
    drive_all(
        &mut world,
        body,
        &wheels,
        WheelInput {
            steer: Fixed::ZERO,
            drive: Fixed::from_int(25),
            brake: Fixed::ZERO,
        },
    );
    for _ in 0..75 {
        world.step(&Floor);
    }
    // Steer the front pair left (+z yaw for +x travel).
    for id in &wheels[..2] {
        world.set_wheel_input(
            body,
            *id,
            WheelInput {
                steer: fx(3, 10),
                drive: Fixed::from_int(25),
                brake: Fixed::ZERO,
            },
        );
    }
    for _ in 0..150 {
        world.step(&Floor);
    }
    let b = world.body(body).unwrap();
    // Yaw shows up as the body-x axis rotating toward +y.
    let forward = b.orientation() * vec3(1, 0, 0);
    assert!(
        forward.y > fx(1, 5),
        "no leftward yaw accumulated: forward = ({:?}, {:?})",
        forward.x,
        forward.y
    );
    assert!(
        b.linear_velocity().y > Fixed::ZERO,
        "trajectory not curving"
    );
}

/// The rollover acceptance pair: same footprint and wheels, ballast up
/// high vs down low; a scripted hard turn at speed rolls the tall one
/// over and leaves the low one upright. Thresholds account for the
/// P2 support-polygon inset (±½ voxel), though wheels — not the skin —
/// carry the vehicle here; what matters is track width (2·1 + inset
/// effects) vs `CoM` height.
#[test]
fn high_com_rolls_over_low_com_does_not() {
    let run = |top: bool, bottom: bool| {
        // Criticals (a = g·halftrack/h, halftrack 2.5): tall variant
        // h ≈ 3.0 → ≈ 8.4; squat h ≈ 2.0 → ≈ 12.3. Terrain μ = 1.25
        // puts the grip ceiling √(0.8·1.25)·g = 10 between them with
        // ~20% margin each way — the tall one rolls at the limit, the
        // squat one plows.
        let (mut world, body, wheels) = vehicle((5, 4), top, bottom);
        for _ in 0..100 {
            world.step(&Floor);
        }
        // Gentle throttle, long straight: drive 60 on this wheelbase
        // (4) with the tall CoM (h ≈ 3.55) pops a permanent wheelie
        // (pitch torque F·h beats the m·g·2 wheelbase margin) and the
        // vehicle crawls — physically right, useless for the test.
        drive_all(
            &mut world,
            body,
            &wheels,
            WheelInput {
                steer: Fixed::ZERO,
                drive: Fixed::from_int(20),
                brake: Fixed::ZERO,
            },
        );
        for _ in 0..300 {
            world.step(&Floor); // wind up speed
        }
        assert!(
            world.body(body).unwrap().linear_velocity().length() > Fixed::from_int(8),
            "wind-up failed — the turn below would test nothing"
        );
        // Hard turn with most of the throttle lifted: full throttle
        // would eat the lateral budget through the friction circle
        // and blunt exactly the roll moment under test, while zero
        // throttle lets scrub bleed the speed before the roll builds.
        for (i, id) in wheels.iter().enumerate() {
            world.set_wheel_input(
                body,
                *id,
                WheelInput {
                    steer: if i < 2 { fx(1, 2) } else { Fixed::ZERO },
                    drive: Fixed::from_int(15),
                    brake: Fixed::ZERO,
                },
            );
        }
        let mut min_up = Fixed::ONE;
        for _ in 0..250 {
            world.step(&Floor);
            min_up = min_up.min(body_up_z(&world, body));
        }
        min_up
    };
    let tall = run(true, false);
    let squat = run(false, true);
    assert!(
        tall < fx(1, 4),
        "high-CoM vehicle should roll (min up·z = {tall:?})"
    );
    assert!(
        squat > fx(3, 4),
        "low-CoM vehicle should stay upright (min up·z = {squat:?})"
    );
}

/// Losing the front-left wheel mid-drive: the chassis sags toward that
/// corner (roll toward +y, pitch toward the nose) and the trajectory
/// pulls to the left, both relative to an intact baseline.
#[test]
fn lost_wheel_sags_and_pulls() {
    let run = |detach: bool| {
        let (mut world, body, wheels) = vehicle((1, 2), false, false);
        for _ in 0..100 {
            world.step(&Floor);
        }
        drive_all(
            &mut world,
            body,
            &wheels,
            WheelInput {
                steer: Fixed::ZERO,
                drive: Fixed::from_int(20),
                brake: Fixed::ZERO,
            },
        );
        for _ in 0..50 {
            world.step(&Floor);
        }
        if detach {
            world.detach_wheel(body, wheels[0]); // front-left (+x, +y)
        }
        for _ in 0..200 {
            world.step(&Floor);
        }
        let b = world.body(body).unwrap();
        let up = b.orientation() * vec3(0, 0, 1);
        (b.position().y, up)
    };
    let (y_base, up_base) = run(false);
    let (y_lost, up_lost) = run(true);
    // Sag toward the missing front-left corner: the up axis leans
    // +x/+y relative to baseline.
    assert!(
        up_lost.x > up_base.x + fx(1, 100),
        "no nose-down pitch: {:?} vs {:?}",
        up_lost.x,
        up_base.x
    );
    assert!(
        up_lost.y > up_base.y + fx(1, 100),
        "no left roll: {:?} vs {:?}",
        up_lost.y,
        up_base.y
    );
    // Pull: trajectory drifts left (+y) vs baseline.
    assert!(
        y_lost > y_base + Fixed::ONE,
        "no leftward pull: y {y_lost:?} vs baseline {y_base:?}"
    );
}

/// The literal voxel staircase promised by the P2 amendment: a run-up
/// climbs the 26.6° stairs (a standing start against a riser taller
/// than the ½-voxel wheel radius must NOT work — that is wheel
/// physics, not a bug); parked on the steps with brakes it holds.
#[test]
fn staircase_climb_and_brake_hold() {
    // Climb: spawn on the flat apron (stairs rise for x ≥ 0), wind up
    // speed, carry momentum up the steps while the suspension eats
    // the 1-voxel compression jumps.
    let (mut world, body, wheels) = vehicle_at((7, 10), vec3(-12, 0, 3));
    for _ in 0..100 {
        world.step(&Stairs);
    }
    let z0 = world.body(body).unwrap().position().z;
    drive_all(
        &mut world,
        body,
        &wheels,
        WheelInput {
            steer: Fixed::ZERO,
            drive: Fixed::from_int(50),
            brake: Fixed::ZERO,
        },
    );
    for _ in 0..400 {
        world.step(&Stairs);
    }
    let b = world.body(body).unwrap();
    assert!(
        b.position().z > z0 + Fixed::from_int(3),
        "did not climb: z {:?} from {z0:?}",
        b.position().z
    );

    // Brake hold: park astride the steps, full brake, no drive. The
    // CoM sits at x = 11 so both axles (±2) land mid-tread (steps are
    // 2 wide with edges at even x) — wheels parked exactly on step
    // edges would ray into the risers, where friction is skipped by
    // design (degenerate-steer branch).
    let (mut parked, pbody, pwheels) = vehicle_at((7, 10), vec3(11, 0, 9));
    // Brakes ON from tick 0 — free wheels on a 26.6° grade roll away
    // during any un-braked settle (correctly: that is a car in
    // neutral on a hill).
    drive_all(
        &mut parked,
        pbody,
        &pwheels,
        WheelInput {
            steer: Fixed::ZERO,
            drive: Fixed::ZERO,
            brake: Fixed::from_int(200),
        },
    );
    for _ in 0..100 {
        parked.step(&Stairs);
    }
    let x0 = parked.body(pbody).unwrap().position().x;
    // Guard the setup: still up on the stairs after settling.
    assert!(
        x0 > Fixed::from_int(6),
        "slid off the stairs during braked settle: x = {x0:?}"
    );
    for _ in 0..300 {
        parked.step(&Stairs);
    }
    let drift = (parked.body(pbody).unwrap().position().x - x0).abs();
    assert!(drift < fx(1, 2), "brakes did not hold: drifted {drift:?}");
}

/// Wheels are hashed state: attach, input, and detach all re-key.
#[test]
fn wheels_are_hashed() {
    let (world_a, _, _) = vehicle((1, 2), false, false);
    let (mut world_b, body_b, wheels_b) = vehicle((1, 2), false, false);
    assert_eq!(world_a.state_hash(), world_b.state_hash(), "same build");
    world_b.set_wheel_input(
        body_b,
        wheels_b[0],
        WheelInput {
            steer: fx(1, 10),
            drive: Fixed::ZERO,
            brake: Fixed::ZERO,
        },
    );
    assert_ne!(world_a.state_hash(), world_b.state_hash(), "input hashed");
    let (mut world_c, body_c, wheels_c) = vehicle((1, 2), false, false);
    world_c.detach_wheel(body_c, wheels_c[3]);
    assert_ne!(world_a.state_hash(), world_c.state_hash(), "detach hashed");
}

/// Snapshot round-trip mid-drive on the stairs (wheel inputs retained,
/// suspension mid-compression).
#[test]
fn snapshot_round_trip_mid_drive() {
    let (mut world, body, wheels) = vehicle((7, 10), false, false);
    for _ in 0..80 {
        world.step(&Stairs);
    }
    drive_all(
        &mut world,
        body,
        &wheels,
        WheelInput {
            steer: fx(1, 10),
            drive: Fixed::from_int(40),
            brake: Fixed::ZERO,
        },
    );
    for _ in 0..60 {
        world.step(&Stairs);
    }
    let json = serde_json::to_string(&world).expect("serialize");
    let mut restored: PhysicsWorld = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, world, "mid-drive restore is lossless");
    for _ in 0..100 {
        restored.step(&Stairs);
        world.step(&Stairs);
    }
    assert_eq!(restored.state_hash(), world.state_hash());
}

/// The hover-cart contract (plan, P3 amendments): wheels on a ghost
/// body are legal — the suspension raycasts terrain and needs no
/// collision skin. Guards the contract through P4's `RigidBody`
/// surgery: a ghost with four wheels hovers at its suspension ride
/// height instead of falling through the floor.
#[test]
fn ghost_body_hovers_on_wheels() {
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(vec3(0, 0, GRAVITY_Z));
    world.register_material(Material {
        density: Fixed::ONE,
        friction: fx(1, 2),
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(50),
    });
    // mass 4, unit inertia — spawn well above ride height.
    let body = world.spawn(&monada_physics::BodyDef {
        position: vec3(0, 0, 4),
        mass: Fixed::from_int(4),
        ..monada_physics::BodyDef::default()
    });
    assert_eq!(
        world.body(body).unwrap().skin_len(),
        0,
        "a ghost has no skin"
    );
    for (sx, sy) in [(1, 1), (1, -1), (-1, 1), (-1, -1)] {
        world.attach_wheel(
            body,
            &WheelDef {
                anchor: vec3(sx, sy, 0),
                rest_length: fx(3, 2),
                radius: Fixed::HALF,
                // Per-wheel load m·g/4 = 10 → compression 0.25.
                stiffness: Fixed::from_int(40),
                damping: Fixed::from_int(10),
                friction: fx(4, 5),
            },
        );
    }
    for _ in 0..300 {
        world.step(&Floor);
    }
    let b = world.body(body).unwrap();
    // Ride height: contact 0 + radius 0.5 + suspension (1.5 − 0.25)
    // + anchor→CoM 0 = 1.75, within contact tolerance.
    close(b.position().z, fx(7, 4), 1 << 27);
    assert!(
        b.linear_velocity().length() < fx(1, 10),
        "hover not settled: |v| = {:?}",
        b.linear_velocity().length()
    );
}
