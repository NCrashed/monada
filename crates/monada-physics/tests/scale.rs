//! P5 acceptance (docs/plans/voxel-physics.md §6 P5 + amendments):
//! body-vs-body contacts (stacks, momentum transfer, piles), islands
//! and sleeping with every wake source, `World::raycast`.

use monada_fixed::{Fixed, FixedQuat, FixedVec3};
use monada_physics::{
    Material, MaterialId, PhysicsWorld, VoxelBodyDef, VoxelField, VoxelShape, WheelInput,
    SLEEP_ANGULAR, SLEEP_LINEAR, SLEEP_TICKS,
};

fn close(a: Fixed, b: Fixed, eps_bits: i64) {
    let d = (a.to_bits() - b.to_bits()).abs();
    assert!(d <= eps_bits, "‖{a:?} - {b:?}‖ = {d} bits > {eps_bits}");
}

fn vec3(x: i32, y: i32, z: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::from_int(z))
}

fn fx(n: i32, d: i32) -> Fixed {
    Fixed::from_ratio(n, d)
}

struct Floor;
impl VoxelField for Floor {
    fn occupied(&self, _x: i64, _y: i64, z: i64) -> bool {
        z < 0
    }
    fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
        MaterialId(0)
    }
}

fn new_world() -> (PhysicsWorld, MaterialId) {
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(vec3(0, 0, -10));
    let mat = world.register_material(Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
    });
    (world, mat)
}

fn cube(
    world: &mut PhysicsWorld,
    mat: MaterialId,
    n: i32,
    position: FixedVec3,
) -> monada_physics::BodyId {
    cube_moving(world, mat, n, position, FixedVec3::ZERO)
}

fn cube_moving(
    world: &mut PhysicsWorld,
    mat: MaterialId,
    n: i32,
    position: FixedVec3,
    velocity: FixedVec3,
) -> monada_physics::BodyId {
    let mut shape = VoxelShape::new(n, n, n);
    shape.fill_box((0, 0, 0), (n - 1, n - 1, n - 1), mat);
    world.spawn_voxels(&VoxelBodyDef {
        shape,
        position,
        orientation: FixedQuat::IDENTITY,
        linear_velocity: velocity,
        angular_velocity: FixedVec3::ZERO,
    })
}

fn at_rest(world: &PhysicsWorld, id: monada_physics::BodyId) -> bool {
    let b = world.body(id).unwrap();
    b.linear_velocity().length() < SLEEP_LINEAR && b.angular_velocity().length() < SLEEP_ANGULAR
}

/// The P5 headline: a cube dropped onto a resting cube stacks — both
/// come to rest, the top one a cube-height above the bottom one, and
/// the whole island falls asleep.
#[test]
fn dropped_cube_stacks_and_the_island_sleeps() {
    let (mut world, mat) = new_world();
    let bottom = cube(&mut world, mat, 4, vec3(0, 0, 6));
    for _ in 0..150 {
        world.step(&Floor);
    }
    assert!(at_rest(&world, bottom));
    let top = cube(&mut world, mat, 4, vec3(0, 0, 12));
    for _ in 0..250 {
        world.step(&Floor);
    }
    assert!(
        at_rest(&world, bottom) && at_rest(&world, top),
        "stack settles"
    );
    // Bottom CoM at 2, top at ~6 (4 voxels up), both within contact
    // tolerance.
    close(
        world.body(bottom).unwrap().position().z,
        Fixed::from_int(2),
        1 << 27,
    );
    close(
        world.body(top).unwrap().position().z,
        Fixed::from_int(6),
        1 << 27,
    );
    // The island sleeps as a unit.
    for _ in 0..i32::try_from(SLEEP_TICKS).unwrap() + 30 {
        world.step(&Floor);
    }
    assert!(world.body(bottom).unwrap().asleep());
    assert!(world.body(top).unwrap().asleep());
}

/// Momentum transfer: a moving cube strikes a resting one head-on —
/// the target picks up forward velocity, the striker slows, and the
/// combined momentum stays within the friction bleed of the floor.
#[test]
fn impact_transfers_momentum() {
    let (mut world, mat) = new_world();
    // Both settle on the floor first.
    let target = cube(&mut world, mat, 3, vec3(10, 0, 2));
    let striker = cube(&mut world, mat, 3, vec3(0, 0, 2));
    for _ in 0..100 {
        world.step(&Floor);
    }
    world.apply_impulse(target, FixedVec3::ZERO); // wake-noop guard (already awake)
                                                  // Shove the striker at the target.
    world.apply_impulse(striker, vec3(270, 0, 0)); // m = 27 → 10 voxels/s
    let p_before = world.body(striker).unwrap().linear_velocity().x * Fixed::from_int(27);
    let mut hit = false;
    for _ in 0..100 {
        world.step(&Floor);
        if world.body(target).unwrap().linear_velocity().x > fx(1, 2) {
            hit = true;
        }
    }
    assert!(hit, "the target never moved");
    let v_striker = world.body(striker).unwrap().linear_velocity().x;
    let v_target = world.body(target).unwrap().linear_velocity().x;
    assert!(
        v_target >= v_striker - fx(1, 10),
        "striker should not pass through the target"
    );
    // Momentum after ≤ momentum before (friction only bleeds it).
    let p_after = (v_striker + v_target) * Fixed::from_int(27);
    assert!(
        p_after <= p_before + Fixed::ONE,
        "momentum appeared from nowhere: {p_after:?} > {p_before:?}"
    );
}

/// A pile of nine cubes dropped in three tiers: deterministic across
/// two runs, snapshot-stable mid-settle, and fully asleep at the end.
#[test]
fn pile_settles_deterministically_and_sleeps() {
    let build = || {
        let (mut world, mat) = new_world();
        let mut ids = Vec::new();
        for tier in 0..3i32 {
            for i in 0..3i32 {
                ids.push(cube(
                    &mut world,
                    mat,
                    2,
                    vec3(i * 3 - 3, (tier % 2) * 2 - 1, 3 + tier * 4),
                ));
            }
        }
        (world, ids)
    };
    let (mut a, ids) = build();
    let (mut b, _) = build();
    for _ in 0..150 {
        a.step(&Floor);
        b.step(&Floor);
    }
    assert_eq!(a.state_hash(), b.state_hash(), "pile diverged");

    // Snapshot mid-settle continues identically.
    let json = serde_json::to_string(&a).expect("serialize");
    let mut restored: PhysicsWorld = serde_json::from_str(&json).expect("deserialize");
    for _ in 0..300 {
        a.step(&Floor);
        restored.step(&Floor);
    }
    assert_eq!(a.state_hash(), restored.state_hash());
    // Everything asleep by now.
    for id in &ids {
        assert!(a.body(*id).unwrap().asleep(), "{id:?} still awake");
    }
}

/// Every wake source, and the non-sources: real contact (via a thrown
/// cube), impulses, wheel input, carve, gravity change, a nearby
/// terrain edit — while a far edit and a drive-by candidate pair wake
/// nothing.
#[test]
fn wake_sources_and_non_sources() {
    let sleeping_cube = |world: &mut PhysicsWorld, mat| {
        let id = cube(world, mat, 3, vec3(0, 0, 4));
        for _ in 0..200 {
            world.step(&Floor);
        }
        assert!(world.body(id).unwrap().asleep(), "setup: cube must sleep");
        id
    };

    // Impulse wakes.
    let (mut world, mat) = new_world();
    let id = sleeping_cube(&mut world, mat);
    world.apply_impulse(id, vec3(1, 0, 0));
    assert!(!world.body(id).unwrap().asleep());

    // Carve wakes (and the survivor re-settles with fresh bookkeeping).
    let (mut world, mat) = new_world();
    let id = sleeping_cube(&mut world, mat);
    let outcome = world.remove_voxels(id, &[(0, 0, 2)]);
    assert_eq!(outcome.survivor, Some(id));
    assert!(!world.body(id).unwrap().asleep());
    for _ in 0..300 {
        world.step(&Floor);
    }
    assert!(
        world.body(id).unwrap().asleep(),
        "re-sleeps after the carve"
    );

    // Gravity change wakes everything.
    let (mut world, mat) = new_world();
    let id = sleeping_cube(&mut world, mat);
    world.set_gravity(vec3(0, 0, -9));
    assert!(!world.body(id).unwrap().asleep());

    // A nearby terrain edit wakes; a far one does not.
    let (mut world, mat) = new_world();
    let id = sleeping_cube(&mut world, mat);
    world.notify_terrain_edit((40, 40, -1), (44, 44, -1));
    assert!(world.body(id).unwrap().asleep(), "far edit must not wake");
    world.notify_terrain_edit((0, 0, -1), (2, 2, -1));
    assert!(!world.body(id).unwrap().asleep(), "near edit must wake");

    // A thrown cube wakes THROUGH a real contact; a drive-by pair
    // candidate does not.
    let (mut world, mat) = new_world();
    let id = sleeping_cube(&mut world, mat);
    // Drive-by: a cube passing 6 voxels away — broadphase-adjacent at
    // 16-voxel cells, never touching.
    let passer = cube_moving(&mut world, mat, 2, vec3(-20, 6, 2), vec3(15, 0, 0));
    for _ in 0..70 {
        world.step(&Floor);
        assert!(world.body(id).unwrap().asleep(), "drive-by must not wake");
    }
    let _ = passer;
    // Direct hit: thrown straight at the sleeper.
    let thrown = cube_moving(&mut world, mat, 2, vec3(-15, 0, 2), vec3(20, 0, 0));
    let mut woke = false;
    for _ in 0..80 {
        world.step(&Floor);
        if !world.body(id).unwrap().asleep() {
            woke = true;
            break;
        }
    }
    assert!(woke, "a real contact must wake the sleeper");
    let _ = thrown;
}

/// The wake wave climbs a sleeping stack one layer per tick — a
/// deliberate, documented choice (sleeping bodies make no contacts, so
/// the island rebuilds outward from the woken base): after an impulse
/// to the bottom cube, the whole three-cube tower is awake within a
/// few ticks, not instantly.
#[test]
fn wake_wave_climbs_the_stack() {
    let (mut world, mat) = new_world();
    let bottom = cube(&mut world, mat, 3, vec3(0, 0, 4));
    let middle = cube(&mut world, mat, 3, vec3(0, 0, 9));
    let top = cube(&mut world, mat, 3, vec3(0, 0, 14));
    for _ in 0..400 {
        world.step(&Floor);
    }
    for id in [bottom, middle, top] {
        assert!(world.body(id).unwrap().asleep(), "setup: tower must sleep");
    }
    world.apply_impulse(bottom, vec3(0, 0, 30));
    assert!(!world.body(bottom).unwrap().asleep());
    // Within a handful of ticks the wave reaches the top (one layer
    // per tick plus contact slack).
    let mut all_awake_at = None;
    for tick in 1..=10 {
        world.step(&Floor);
        if [bottom, middle, top]
            .iter()
            .all(|id| !world.body(*id).unwrap().asleep())
        {
            all_awake_at = Some(tick);
            break;
        }
    }
    assert!(
        all_awake_at.is_some(),
        "wake wave never reached the top of the tower"
    );
}

/// `World::raycast`: terrain hits, body hits (shape-frame cell on a
/// rotated body), nearest-of-two, `max_t` cutoff, ghost invisibility,
/// sleeping visibility.
#[test]
fn raycast_terrain_and_bodies() {
    let (mut world, mat) = new_world();
    // A cube ahead of the origin, another behind it.
    let near = cube(&mut world, mat, 2, vec3(6, 0, 2));
    let _far = cube(&mut world, mat, 2, vec3(12, 0, 2));
    // A ghost in between — must be invisible.
    let _ghost = world.spawn(&monada_physics::BodyDef {
        position: vec3(3, 0, 2),
        ..monada_physics::BodyDef::default()
    });
    let origin = vec3(0, 0, 2);
    let dir = vec3(1, 0, 0);

    let hit = world
        .raycast(&Floor, origin, dir, Fixed::from_int(20))
        .expect("hits the near cube");
    assert_eq!(hit.body, Some(near));
    // Entry face: the cube spans x ∈ [5, 7] → t = 5, normal −x.
    close(hit.t, Fixed::from_int(5), 1 << 12);
    close(hit.normal.x, Fixed::NEG_ONE, 1 << 12);

    // Straight down: terrain.
    let down = world
        .raycast(&Floor, vec3(0, 30, 5), vec3(0, 0, -1), Fixed::from_int(10))
        .expect("hits the floor");
    assert_eq!(down.body, None);
    assert_eq!(down.cell, (0, 30, -1));
    close(down.t, Fixed::from_int(5), 1 << 8);

    // max_t cuts off.
    assert!(world
        .raycast(&Floor, origin, dir, Fixed::from_int(3))
        .is_none());

    // Sleeping bodies stay visible. (The settled cubes sit a voxel
    // lower than at spawn, so the probe ray drops to mid-body height.)
    for _ in 0..200 {
        world.step(&Floor);
    }
    assert!(world.body(near).unwrap().asleep());
    assert_eq!(
        world
            .raycast(&Floor, vec3(0, 0, 1), dir, Fixed::from_int(20))
            .expect("still hits")
            .body,
        Some(near)
    );
}

/// Sleep state is hashed (tripwire) and wheels obey the wake contract
/// (`set_wheel_input` / `detach_wheel` wake).
#[test]
fn sleep_state_is_hashed_and_wheel_apis_wake() {
    let (mut world, mat) = new_world();
    let id = cube(&mut world, mat, 3, vec3(0, 0, 4));
    let wheel = world.attach_wheel(
        id,
        &monada_physics::WheelDef {
            anchor: FixedVec3::ZERO,
            rest_length: Fixed::ONE,
            radius: Fixed::HALF,
            stiffness: Fixed::from_int(1),
            damping: Fixed::ONE,
            friction: Fixed::HALF,
        },
    );
    for _ in 0..300 {
        world.step(&Floor);
    }
    assert!(world.body(id).unwrap().asleep(), "setup: must sleep");
    let asleep_hash = world.state_hash();

    // Wheel input wakes — and the hash tripwires on the sleep fields.
    let mut with_input = world.clone();
    with_input.set_wheel_input(
        id,
        wheel,
        WheelInput {
            steer: Fixed::ZERO,
            drive: Fixed::ZERO,
            brake: Fixed::ZERO,
        },
    );
    assert!(!with_input.body(id).unwrap().asleep());
    assert_ne!(
        with_input.state_hash(),
        asleep_hash,
        "asleep/sleep_timer must be hashed"
    );

    // detach_wheel wakes too.
    let mut with_detach = world.clone();
    with_detach.detach_wheel(id, wheel);
    assert!(!with_detach.body(id).unwrap().asleep());
}
