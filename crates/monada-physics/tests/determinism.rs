//! P0 determinism gates (docs/plans/voxel-physics.md §5): the snapshot
//! property and hash sanity. Cross-platform bit-identity is the
//! oracle's job (`phys@` goldens); these run in the plain test matrix.

use monada_physics::PhysicsWorld;

/// Snapshot → restore → continue must match the uninterrupted run,
/// bit-for-bit, from any tick.
#[test]
fn snapshot_restore_continue_matches_uninterrupted() {
    let mut world = PhysicsWorld::new(25);
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
