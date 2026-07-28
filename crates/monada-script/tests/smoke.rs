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

/// Angle constants `pi()`, `pi_2()`, `tau()` match the trig module values.
#[test]
fn constants() {
    use monada_fixed::trig;

    const SCRIPT: &str = r#"
        fn init() {
            let a = archetype(["pi", "pi2", "tau"]);
            let e = entity_create(a);
            entity_set_field(e, "pi",  pi());
            entity_set_field(e, "pi2", pi_2());
            entity_set_field(e, "tau", tau());
        }
        fn tick() {}
    "#;

    let world = shared_world(1);
    let mut backend = RhaiBackend::new(world.clone());
    backend.load(SCRIPT).expect("compile");
    backend.on_init().expect("init");

    let w = world.lock().unwrap();
    let e = w.entities(ArchetypeId(0))[0];
    assert_eq!(w.field(e, "pi"), Some(trig::PI));
    assert_eq!(w.field(e, "pi2"), Some(trig::FRAC_PI_2));
    assert_eq!(w.field(e, "tau"), Some(trig::TAU));
}

/// floor/ceil/round for positive and negative values.
/// floor goes toward -inf, ceil toward +inf, round half-away-from-zero.
#[test]
fn rounding() {
    const SCRIPT: &str = r#"
        fn init() {
            let a = archetype(["floor_pos", "floor_neg",
                               "ceil_pos",  "ceil_neg",
                               "round_pos", "round_neg"]);
            let e = entity_create(a);
            let pos = fixed(1) + fixed(3) / fixed(4);  //  1.75
            let neg = -(fixed(1) + fixed(3) / fixed(4)); // -1.75
            entity_set_field(e, "floor_pos", floor(pos));
            entity_set_field(e, "floor_neg", floor(neg));
            entity_set_field(e, "ceil_pos",  ceil(pos));
            entity_set_field(e, "ceil_neg",  ceil(neg));
            entity_set_field(e, "round_pos", round(pos));
            entity_set_field(e, "round_neg", round(neg));
        }
        fn tick() {}
    "#;

    let world = shared_world(1);
    let mut backend = RhaiBackend::new(world.clone());
    backend.load(SCRIPT).expect("compile");
    backend.on_init().expect("init");

    let w = world.lock().unwrap();
    let e = w.entities(ArchetypeId(0))[0];
    let field = |name| w.field(e, name).unwrap();

    assert_eq!(field("floor_pos"), Fixed::from_int(1));
    assert_eq!(field("floor_neg"), Fixed::from_int(-2));
    assert_eq!(field("ceil_pos"), Fixed::from_int(2));
    assert_eq!(field("ceil_neg"), Fixed::from_int(-1));
    assert_eq!(field("round_pos"), Fixed::from_int(2));
    assert_eq!(field("round_neg"), Fixed::from_int(-2));
}

/// A fixed-rate map's `tick(dt)` receives the correct duration and can use
/// it for time-independent movement. Hz=4 gives dt=0.25 exactly (power-of-two
/// denominator), so `speed(2) * dt(0.25) * ticks(4)` = 2.0 with no rounding.
#[test]
fn tick_with_dt_receives_correct_duration() {
    const HZ: u32 = 4;
    const TICKS: u32 = 4;
    const SCRIPT_DT: &str = r#"
        fn init() {
            let a = archetype(["x"]);
            let e = entity_create(a);
            entity_set_field(e, "x", fixed(0));
        }
        fn tick(dt) {
            let speed = fixed(2);
            for e in entities() {
                let x = entity_field(e, "x");
                entity_set_field(e, "x", x + speed * dt);
            }
        }
    "#;

    let world = shared_world(42);
    let mut backend = RhaiBackend::new(world.clone());
    backend.set_tick_hz(HZ);
    backend.load(SCRIPT_DT).expect("compile");
    backend.on_init().expect("init");

    for _ in 0..TICKS {
        backend.on_tick().expect("tick");
    }

    let w = world.lock().unwrap();
    let e = w.entities(ArchetypeId(0))[0];
    // 2 cells/sec * 0.25 sec/tick * 4 ticks = 2.0 cells
    assert_eq!(w.field(e, "x"), Some(Fixed::from_int(2)));
}

#[test]
fn host_api_gate_accepts_only_the_supported_range() {
    use monada_script::{check_host_api, HOST_API_OLDEST, HOST_API_VERSION};
    assert!(check_host_api(HOST_API_OLDEST).is_ok());
    assert!(check_host_api(HOST_API_VERSION).is_ok());
    // v0 never existed (it is `HOST_API_OLDEST - 1` while the floor
    // trails at 1), and the future is by definition unsupported.
    let too_old = check_host_api(0).unwrap_err();
    assert!(too_old.contains("requires host API"), "{too_old}");
    let too_new = check_host_api(HOST_API_VERSION + 1).unwrap_err();
    assert!(too_new.contains("requires host API"), "{too_new}");
}
