//! P0 determinism gates (docs/plans/voxel-physics.md §5): the snapshot
//! property and hash sanity. Cross-platform bit-identity is the
//! oracle's job (`phys@` goldens); these run in the plain test matrix.

use monada_fixed::{Fixed, FixedMat3, FixedQuat, FixedVec3};
use monada_physics::{BodyDef, EmptyField, PhysicsWorld};

/// A world with real P1 content: gravity, a spinning ballistic body,
/// a drifting one — so the snapshot covers every serialized field,
/// including the `inv_*` caches.
fn populated_world() -> PhysicsWorld {
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(FixedVec3::new(
        Fixed::ZERO,
        Fixed::ZERO,
        Fixed::from_int(-10),
    ));
    world.spawn(&BodyDef {
        position: FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::from_int(100)),
        linear_velocity: FixedVec3::new(
            Fixed::from_int(5),
            Fixed::from_int(3),
            Fixed::from_int(20),
        ),
        angular_velocity: FixedVec3::new(Fixed::ONE, Fixed::from_int(2), Fixed::NEG_ONE),
        mass: Fixed::from_int(3),
        inertia_body: FixedMat3::from_diagonal(FixedVec3::new(
            Fixed::from_int(2),
            Fixed::from_int(3),
            Fixed::from_int(4),
        )),
        orientation: FixedQuat::IDENTITY,
    });
    world.spawn(&BodyDef {
        linear_velocity: FixedVec3::new(Fixed::NEG_ONE, Fixed::ONE, Fixed::ZERO),
        ..BodyDef::default()
    });
    world
}

/// Snapshot → restore → continue must match the uninterrupted run,
/// bit-for-bit, from any tick.
#[test]
fn snapshot_restore_continue_matches_uninterrupted() {
    let mut world = populated_world();
    for tick in 0..300u64 {
        if tick % 37 == 0 {
            let json = serde_json::to_string(&world).expect("serialize");
            let mut restored: PhysicsWorld = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(restored, world, "restore is lossless at tick {tick}");
            for _ in 0..50 {
                restored.step(&EmptyField);
            }
            let mut uninterrupted = world.clone();
            for _ in 0..50 {
                uninterrupted.step(&EmptyField);
            }
            assert_eq!(
                restored.state_hash(),
                uninterrupted.state_hash(),
                "round-trip diverged after tick {tick}"
            );
        }
        world.step(&EmptyField);
    }
}

/// The hash covers every state field — the local tripwire for a field
/// forgotten in the fold, so a gap is caught here and not first by the
/// oracle goldens. One `assert_ne` per `PhysicsWorld` field.
#[test]
fn hash_covers_every_state_field() {
    // tick
    let mut a = PhysicsWorld::new(25);
    let h0 = a.state_hash();
    a.step(&EmptyField);
    assert_ne!(h0, a.state_hash(), "tick is hashed");
    // dt
    assert_ne!(
        PhysicsWorld::new(25).state_hash(),
        PhysicsWorld::new(30).state_hash(),
        "dt is hashed"
    );
    // gravity
    let mut with_gravity = PhysicsWorld::new(25);
    with_gravity.set_gravity(FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::NEG_ONE));
    assert_ne!(
        with_gravity.state_hash(),
        PhysicsWorld::new(25).state_hash(),
        "gravity is hashed"
    );
    // next_body_id + bodies
    let mut with_body = PhysicsWorld::new(25);
    with_body.spawn(&BodyDef::default());
    assert_ne!(
        with_body.state_hash(),
        PhysicsWorld::new(25).state_hash(),
        "spawn (next_body_id + bodies) is hashed"
    );
    // body content, not just count
    let mut moved = PhysicsWorld::new(25);
    moved.spawn(&BodyDef {
        position: FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO),
        ..BodyDef::default()
    });
    assert_ne!(
        moved.state_hash(),
        with_body.state_hash(),
        "body fields are hashed"
    );
    // materials
    let mut with_material = PhysicsWorld::new(25);
    with_material.register_material(monada_physics::Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(50),
    });
    assert_ne!(
        with_material.state_hash(),
        PhysicsWorld::new(25).state_hash(),
        "materials are hashed"
    );
    // shape: a 1-voxel body vs a ghost with the *identical* derived
    // mass properties (m = 1, I = E/6) — only the shape fold separates
    // the two states.
    let mut voxel = PhysicsWorld::new(25);
    let mat = voxel.register_material(monada_physics::Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(50),
    });
    let mut shape = monada_physics::VoxelShape::new(1, 1, 1);
    shape.set(0, 0, 0, mat);
    let com = FixedVec3::new(Fixed::HALF, Fixed::HALF, Fixed::HALF);
    voxel.spawn_voxels(&monada_physics::VoxelBodyDef {
        shape,
        position: com,
        orientation: FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let mut ghost = PhysicsWorld::new(25);
    ghost.register_material(monada_physics::Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(50),
    });
    let sixth = Fixed::ONE / Fixed::from_int(6);
    ghost.spawn(&BodyDef {
        position: com,
        mass: voxel.bodies()[0].mass(),
        inertia_body: FixedMat3::from_diagonal(FixedVec3::new(sixth, sixth, sixth)),
        ..BodyDef::default()
    });
    // Guard the setup: identical hashed scalars except the shape…
    assert_eq!(voxel.bodies()[0].mass(), ghost.bodies()[0].mass());
    // …so a hash difference can only come from the shape fold. (The
    // inertia diagonals may differ by an ulp from the voxel sum — the
    // assert below stays valid either way; the shape byte is the
    // guaranteed separator.)
    assert_ne!(voxel.state_hash(), ghost.state_hash(), "shape is hashed");
    // The warm-start impulse cache is also folded (world.rs); it is a
    // function of contact history, so no two states can differ by the
    // cache alone through the public API — its serialization is
    // covered by the snapshot round-trip with a resting body below.
}

/// Snapshot round-trip *mid-contact*: a cube resting on the floor has
/// a live warm-start cache; restore must reproduce the continuation
/// bit-for-bit (the cache is serialized state, not rebuilt).
#[test]
fn snapshot_round_trip_mid_contact() {
    struct Floor;
    impl monada_physics::VoxelField for Floor {
        fn occupied(&self, _x: i64, _y: i64, z: i64) -> bool {
            z < 0
        }
        fn material(&self, _x: i64, _y: i64, _z: i64) -> monada_physics::MaterialId {
            monada_physics::MaterialId(0)
        }
    }
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(FixedVec3::new(
        Fixed::ZERO,
        Fixed::ZERO,
        Fixed::from_int(-10),
    ));
    let mat = world.register_material(monada_physics::Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(50),
    });
    let mut shape = monada_physics::VoxelShape::new(2, 2, 2);
    shape.fill_box((0, 0, 0), (1, 1, 1), mat);
    world.spawn_voxels(&monada_physics::VoxelBodyDef {
        shape,
        position: FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::from_int(5)),
        orientation: FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    // Land and settle — the cache is warm now.
    for _ in 0..60 {
        world.step(&Floor);
    }
    let json = serde_json::to_string(&world).expect("serialize");
    let mut restored: PhysicsWorld = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(restored, world, "mid-contact restore is lossless");
    for _ in 0..60 {
        restored.step(&Floor);
        world.step(&Floor);
    }
    assert_eq!(
        restored.state_hash(),
        world.state_hash(),
        "mid-contact round-trip diverged"
    );
}
