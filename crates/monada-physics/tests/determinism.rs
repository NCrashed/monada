//! P0 determinism gates (docs/plans/voxel-physics.md §5): the snapshot
//! property and hash sanity. Cross-platform bit-identity is the
//! oracle's job (`phys@` goldens); these run in the plain test matrix.

use monada_fixed::{Fixed, FixedMat3, FixedQuat, FixedVec3};
use monada_physics::{BodyDef, PhysicsWorld};

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
                restored.step();
            }
            let mut uninterrupted = world.clone();
            for _ in 0..50 {
                uninterrupted.step();
            }
            assert_eq!(
                restored.state_hash(),
                uninterrupted.state_hash(),
                "round-trip diverged after tick {tick}"
            );
        }
        world.step();
    }
}

/// The hash covers every state field: stepping changes it, and worlds
/// built at different tick rates differ from tick 0.
#[test]
fn hash_covers_tick_and_dt() {
    let mut a = PhysicsWorld::new(25);
    let h0 = a.state_hash();
    a.step();
    assert_ne!(h0, a.state_hash(), "tick is hashed");
    assert_ne!(
        PhysicsWorld::new(25).state_hash(),
        PhysicsWorld::new(30).state_hash(),
        "dt is hashed"
    );
}
