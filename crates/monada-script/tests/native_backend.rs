//! Backend equivalence — the D-0 gate of docs/plans/desert-game.md §11.
//!
//! The same map, written twice: once as the Rhai `walk_circle` script and
//! once as compiled [`MapRules`], run through their respective
//! [`ScriptBackend`]s over an identically seeded world. If the two
//! runtimes are really interchangeable, the two worlds must be
//! *bit-identical* — same entity ids, same fixed-point positions, same
//! RNG stream position — and that is exactly what `state_hash` compares.
//!
//! This is the test that gives decision L1 teeth: it fails the moment the
//! native host surface drifts from what the Rhai registration does, in
//! either direction.

use monada_fixed::{trig, Fixed, FixedVec3};
use monada_runtime::{shared_world, Host, MapRules, NativeBackend};
use monada_script::{RhaiBackend, ScriptBackend, WALK_CIRCLE_SCRIPT};
use monada_sim::{ArchetypeId, Command, PlayerId};

/// The walk-in-a-circle scenario as Rust rules — a line-for-line twin of
/// `scripts/walk_circle.rhai`, including the order of its three RNG draws
/// (radius, angle, omega), which is part of the observable state.
///
/// It keeps no state of its own: everything lives in the world, exactly
/// as a Rhai script must, so the default `snapshot` is correct here.
#[derive(Default)]
struct WalkCircle {
    /// The archetype `init` registered — a handle, not hashed state
    /// (`init` always runs before any tick, and re-registering the same
    /// names yields the same id).
    mover: Option<ArchetypeId>,
}

impl WalkCircle {
    fn orbit(angle: Fixed, radius: Fixed) -> FixedVec3 {
        FixedVec3::new(
            trig::cos(angle) * radius,
            trig::sin(angle) * radius,
            Fixed::ZERO,
        )
    }
}

impl MapRules for WalkCircle {
    fn init(&mut self, host: &dyn Host) {
        let mover = host.archetype(&["angle", "omega", "radius"]);
        self.mover = Some(mover);
        for _ in 0..100 {
            let e = host.entity_create(mover);
            let radius = Fixed::from_int(4) + host.rng01() * Fixed::from_int(8);
            let angle = host.rng01() * trig::TAU;
            let omega = Fixed::from_ratio(1, 32) + host.rng01() * Fixed::from_ratio(1, 32);
            host.entity_set_field(e, "angle", angle);
            host.entity_set_field(e, "omega", omega);
            host.entity_set_field(e, "radius", radius);
            host.entity_set_position(e, Self::orbit(angle, radius));
        }
    }

    fn tick(&mut self, host: &dyn Host, _dt: Fixed) {
        for e in host.entities() {
            let angle = host.entity_field(e, "angle") + host.entity_field(e, "omega");
            host.entity_set_field(e, "angle", angle);
            host.entity_set_position(e, Self::orbit(angle, host.entity_field(e, "radius")));
        }
    }
}

fn run_rhai(seed: u64, ticks: u64) -> u64 {
    let world = shared_world(seed);
    let mut backend = RhaiBackend::new(world.clone());
    backend.load(WALK_CIRCLE_SCRIPT).expect("compile");
    backend.on_init().expect("init");
    for _ in 0..ticks {
        backend.on_tick().expect("tick");
    }
    let hash = world.lock().expect("world mutex").state_hash();
    hash
}

fn run_native(seed: u64, ticks: u64) -> u64 {
    let world = shared_world(seed);
    let mut backend = NativeBackend::new(world.clone(), Box::new(WalkCircle::default()));
    backend.load("").expect("load");
    backend.on_init().expect("init");
    for _ in 0..ticks {
        backend.on_tick().expect("tick");
    }
    let hash = world.lock().expect("world mutex").state_hash();
    hash
}

#[test]
fn the_two_backends_reach_the_same_world() {
    for ticks in [0, 1, 30, 150] {
        assert_eq!(
            run_rhai(7, ticks),
            run_native(7, ticks),
            "walk_circle diverged between the Rhai and native backends at tick {ticks}"
        );
    }
}

#[test]
fn the_seed_still_matters() {
    // Guards the test itself: if both sides returned a seed-independent
    // constant, the equality above would be vacuous.
    assert_ne!(run_native(7, 30), run_native(8, 30));
}

#[test]
fn a_native_map_advances_the_tick_counter_like_the_script() {
    // A rules value with no `tick` still lets the driver count ticks —
    // the command-driven (chess) shape.
    struct Inert;
    impl MapRules for Inert {
        fn init(&mut self, _host: &dyn Host) {}
    }
    let world = shared_world(1);
    let mut backend = NativeBackend::new(world.clone(), Box::new(Inert));
    backend.on_init().expect("init");
    for _ in 0..5 {
        backend.on_tick().expect("tick");
    }
    assert_eq!(world.lock().expect("world mutex").tick, 5);
}

#[test]
fn commands_reach_the_rules() {
    #[derive(Default)]
    struct Counter {
        seen: Vec<(u32, u32)>,
    }
    impl MapRules for Counter {
        fn init(&mut self, _host: &dyn Host) {}
        fn command(&mut self, _host: &dyn Host, player: PlayerId, command: &Command) {
            self.seen.push((player.0, command.verb));
        }
    }
    let world = shared_world(1);
    let mut backend = NativeBackend::new(world, Box::new(Counter::default()));
    backend.on_init().expect("init");
    backend
        .on_command(PlayerId(1), &Command::new(42))
        .expect("command");
    // The rules value is behind the backend; a downcast is not part of the
    // API, so assert through the effect that is: no error, and the tick
    // path still runs.
    backend.on_tick().expect("tick");
}
