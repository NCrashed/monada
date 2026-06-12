//! Smoke test of the scripting wall: a Rhai script declares an
//! archetype, spawns an entity, and mutates world state through the host
//! API — and the engine never sees Rhai.

use monada_fixed::Fixed;
use monada_script::{shared_world, RhaiBackend, ScriptBackend};
use monada_sim::ArchetypeId;

const SCRIPT: &str = r#"
    fn init() {
        let mover = archetype(["angle", "radius"]);
        let e = entity_create(mover);
        entity_set_field(e, "angle", fixed(7));
        entity_set_field(e, "radius", ratio(3, 2));
        entity_set_position(e, vec3(fixed(1), fixed(2), fixed(3)));
    }
    fn tick() {
        for e in entities() {
            entity_set_field(e, "angle", entity_field(e, "angle") + fixed(1));
        }
    }
"#;

#[test]
fn script_drives_the_world_through_the_host_api() {
    let world = shared_world(1);
    let mut backend = RhaiBackend::new(world.clone());
    backend.load(SCRIPT).expect("compile");

    backend.on_init().expect("init");
    {
        let w = world.lock().unwrap();
        assert_eq!(w.count(ArchetypeId(0)), 1);
        let e = w.entities(ArchetypeId(0))[0];
        assert_eq!(w.field(e, "angle"), Some(Fixed::from_int(7)));
        assert_eq!(w.field(e, "radius"), Some(Fixed::from_ratio(3, 2)));
        assert_eq!(w.position(e).unwrap().z, Fixed::from_int(3));
        assert_eq!(w.tick, 0);
    }

    backend.on_tick().expect("tick");
    {
        let w = world.lock().unwrap();
        assert_eq!(w.tick, 1, "on_tick advances the world tick");
        let e = w.entities(ArchetypeId(0))[0];
        assert_eq!(w.field(e, "angle"), Some(Fixed::from_int(8)));
    }
}
