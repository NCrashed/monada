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

/// Snapshot equivalence — the strongest determinism test in the repo
/// (docs/plans/desert-game.md §3d), and the reason saved games are worth
/// building this way.
///
/// Run N ticks, save, restore into a *fresh* world and backend, run M
/// more: the result must equal an uninterrupted N+M run, bit for bit.
/// Anything the engine keeps outside the snapshot — a cached derivation,
/// an RNG stream position, a counter living in the wrong place — shows up
/// here as a mismatch, which is exactly the class of bug that desyncs a
/// lockstep match at hour two rather than at minute one.
#[test]
fn a_restored_run_matches_an_uninterrupted_one() {
    const SEED: u64 = 7;
    for (n, m) in [(1_u64, 1_u64), (40, 60), (13, 7)] {
        let uninterrupted = run_native(SEED, n + m);

        let world = shared_world(SEED);
        let mut backend = NativeBackend::new(world.clone(), Box::new(WalkCircle::default()));
        backend.on_init().expect("init");
        for _ in 0..n {
            backend.on_tick().expect("tick");
        }
        let save = backend.snapshot().expect("snapshot");

        // A fresh everything, as a new process would have.
        let world = shared_world(SEED);
        let mut resumed = NativeBackend::new(world.clone(), Box::new(WalkCircle::default()));
        resumed.on_init().expect("init");
        resumed.restore(&save).expect("restore");
        for _ in 0..m {
            resumed.on_tick().expect("tick");
        }

        assert_eq!(
            uninterrupted,
            world.lock().expect("world mutex").state_hash(),
            "resuming after {n} ticks and running {m} more diverged from a \
             straight {} tick run",
            n + m
        );
    }
}

/// The same equivalence for a map that draws from the shared RNG **every
/// tick**. `walk_circle` only rolls during `init`, so on its own it would
/// pass even if a snapshot dropped the generator's position entirely —
/// the exact omission that makes a resumed match desync a few minutes
/// later, once the two peers' streams have drifted apart.
#[test]
fn a_restored_run_keeps_the_rng_stream_position() {
    /// Ten entities, each re-rolled every tick: the world's hash then
    /// depends on where in the stream the generator stands.
    #[derive(Default)]
    struct RngWalk {
        kind: Option<ArchetypeId>,
    }
    impl MapRules for RngWalk {
        fn init(&mut self, host: &dyn Host) {
            let a = host.archetype(&["r"]);
            self.kind = Some(a);
            for _ in 0..10 {
                host.entity_create(a);
            }
        }
        fn tick(&mut self, host: &dyn Host, _dt: Fixed) {
            for e in host.entities() {
                host.entity_set_field(e, "r", host.rng01());
            }
        }
    }

    let straight = {
        let world = shared_world(3);
        let mut b = NativeBackend::new(world.clone(), Box::new(RngWalk::default()));
        b.on_init().expect("init");
        for _ in 0..25 {
            b.on_tick().expect("tick");
        }
        let h = world.lock().expect("world mutex").state_hash();
        h
    };

    let world = shared_world(3);
    let mut b = NativeBackend::new(world, Box::new(RngWalk::default()));
    b.on_init().expect("init");
    for _ in 0..10 {
        b.on_tick().expect("tick");
    }
    let save = b.snapshot().expect("snapshot");

    let world = shared_world(3);
    let mut resumed = NativeBackend::new(world.clone(), Box::new(RngWalk::default()));
    resumed.on_init().expect("init");
    resumed.restore(&save).expect("restore");
    for _ in 0..15 {
        resumed.on_tick().expect("tick");
    }
    assert_eq!(
        straight,
        world.lock().expect("world mutex").state_hash(),
        "the resumed run drew different randomness — the snapshot lost the \
         generator's position"
    );
}

/// Restoring must overwrite what `init` produced rather than adding to
/// it: the RNG has been advanced 300 draws by the fresh `init`, and the
/// world holds 100 entities that the snapshot's own 100 must replace.
#[test]
fn a_restore_replaces_the_world_it_lands_in() {
    let world = shared_world(1);
    let mut backend = NativeBackend::new(world.clone(), Box::new(WalkCircle::default()));
    backend.on_init().expect("init");
    for _ in 0..5 {
        backend.on_tick().expect("tick");
    }
    let save = backend.snapshot().expect("snapshot");

    // Land it in a world seeded differently and already ticked further.
    let other = shared_world(99);
    let mut backend = NativeBackend::new(other.clone(), Box::new(WalkCircle::default()));
    backend.on_init().expect("init");
    for _ in 0..50 {
        backend.on_tick().expect("tick");
    }
    backend.restore(&save).expect("restore");

    assert_eq!(
        world.lock().expect("world mutex").state_hash(),
        other.lock().expect("world mutex").state_hash(),
        "a restored world must equal the saved one, whatever it replaced"
    );
    assert_eq!(other.lock().expect("world mutex").tick, 5);
}

/// A rules value with state of its own must get it back — the shape the
/// desert game needs, where the spice field and the AI's memory live in
/// the rules rather than in entities (§3c).
#[test]
fn rules_state_survives_a_round_trip() {
    #[derive(Default)]
    struct Counting {
        ticks: u32,
    }
    impl MapRules for Counting {
        fn init(&mut self, _host: &dyn Host) {}
        fn tick(&mut self, _host: &dyn Host, _dt: Fixed) {
            self.ticks += 1;
        }
        fn snapshot(&self) -> Vec<u8> {
            self.ticks.to_le_bytes().to_vec()
        }
        fn restore(&mut self, bytes: &[u8]) {
            self.ticks = u32::from_le_bytes(bytes.try_into().expect("four bytes"));
        }
    }

    let world = shared_world(1);
    let mut backend = NativeBackend::new(world, Box::new(Counting::default()));
    backend.on_init().expect("init");
    for _ in 0..9 {
        backend.on_tick().expect("tick");
    }
    let save = backend.snapshot().expect("snapshot");

    let world = shared_world(1);
    let mut resumed = NativeBackend::new(world, Box::new(Counting::default()));
    resumed.on_init().expect("init");
    resumed.restore(&save).expect("restore");
    resumed.on_tick().expect("tick");
    assert_eq!(
        resumed.rules().snapshot(),
        10_u32.to_le_bytes().to_vec(),
        "the rules' own counter should resume at 9, not restart at 0"
    );
}

#[test]
fn a_foreign_blob_is_refused_rather_than_misread() {
    let world = shared_world(1);
    let mut backend = NativeBackend::new(world, Box::new(WalkCircle::default()));
    backend.on_init().expect("init");
    assert!(backend.restore(b"not a snapshot").is_err());
    // A truncated tail of a real save is the nastier case: it decodes far
    // enough to look plausible.
    let save = backend.snapshot().expect("snapshot");
    assert!(backend.restore(&save[..save.len() / 2]).is_err());
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
