//! Generic-world (A2) behaviour: declared fields stored in the world,
//! deterministic hashing over schema+state, serde round-trip.

use monada_fixed::{Fixed, FixedVec3};
use monada_sim::World;

/// Build a small world: one "mover" archetype with angle/radius fields,
/// `n` entities at deterministic positions/fields.
fn build(seed: u64, n: u32) -> monada_sim::World {
    let mut world = World::new(seed);
    let mover = world.register_archetype(&["angle", "radius"]);
    for i in 0..n {
        let ent = world.spawn(mover);
        let radius = Fixed::from_int(4) + world.rng.next_fixed_01() * Fixed::from_int(8);
        let angle = world.rng.next_fixed_01();
        world.set_field(ent, "angle", angle);
        world.set_field(ent, "radius", radius);
        let z = Fixed::from_int(i32::try_from(i).expect("n fits i32"));
        world.set_position(ent, FixedVec3::new(radius, angle, z));
    }
    world
}

#[test]
fn fields_and_positions_round_trip_in_world() {
    let mut w = World::new(1);
    let mover = w.register_archetype(&["angle", "radius"]);
    let e = w.spawn(mover);

    assert_eq!(w.position(e), Some(FixedVec3::ZERO));
    assert_eq!(w.field(e, "angle"), Some(Fixed::ZERO));

    assert!(w.set_field(e, "angle", Fixed::from_int(3)));
    assert!(w.set_position(e, FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO)));
    assert_eq!(w.field(e, "angle"), Some(Fixed::from_int(3)));
    assert_eq!(w.position(e).unwrap().x, Fixed::ONE);

    // Unknown field / entity surface as false / None.
    assert!(!w.set_field(e, "nope", Fixed::ONE));
    assert_eq!(w.field(e, "nope"), None);
    assert_eq!(w.count(mover), 1);
    assert_eq!(w.entities(mover).len(), 1);
}

#[test]
fn hashing_is_deterministic_and_state_sensitive() {
    let a = build(0xABCD, 50);
    let b = build(0xABCD, 50);
    assert_eq!(a.state_hash(), b.state_hash());

    // A different seed → different state.
    assert_ne!(a.state_hash(), build(0xABCE, 50).state_hash());

    // Mutating one field changes the hash.
    let mut c = build(0xABCD, 50);
    let first = c.entities(monada_sim::ArchetypeId(0))[0];
    c.set_field(first, "angle", Fixed::from_int(99));
    assert_ne!(a.state_hash(), c.state_hash());
}

#[test]
fn schema_is_part_of_the_hash() {
    let mut a = World::new(7);
    a.register_archetype(&["angle", "radius"]);
    let mut b = World::new(7);
    b.register_archetype(&["radius", "angle"]); // reordered fields
    assert_ne!(
        a.state_hash(),
        b.state_hash(),
        "field order is canonical and must affect the hash"
    );
}

#[test]
fn despawn_preserves_ascending_ids() {
    let mut w = World::new(2);
    let mover = w.register_archetype(&["k"]);
    let ids: Vec<_> = (0..5).map(|_| w.spawn(mover)).collect();
    assert!(w.despawn(ids[2]));
    let remaining = w.entities(mover);
    assert_eq!(remaining, &[ids[0], ids[1], ids[3], ids[4]]);
    assert!(remaining.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!w.despawn(ids[2])); // already gone
    assert_eq!(w.position(ids[2]), None);
}

#[test]
fn serde_snapshot_round_trip() {
    let w = build(0x55, 30);
    let json = serde_json::to_string(&w).expect("serialize");
    let back: World = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(w.state_hash(), back.state_hash());
}
