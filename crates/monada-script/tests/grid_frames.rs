//! The `grid_*` frame verbs as a MAP sees them (docs/plans/grid-entities.md
//! §3): spawn a hull, put entities on and off it, read the membership back, and
//! convert points between the hull's frame and the world — all through real
//! Rhai, against the frame table the sim backend owns.
//!
//! The unit tests in `grids.rs` cover the arithmetic; these cover the wiring
//! that arithmetic is useless without: that the verbs resolve, that they shadow
//! the bridge-only ones (so the frame table is the authority and a headless peer
//! answers like a rendering one), and that the local layer gets the reads
//! without the mutators.

use std::sync::{Arc, Mutex};

use monada_fixed::Fixed;
use monada_script::{
    shared_world, LocalBackend, NullBridge, RhaiBackend, ScriptBackend, SharedBridge, SharedWorld,
    TerrainBridge,
};
use monada_sim::{ArchetypeId, EntityId};

const SEED: u64 = 0x6D_6F_6E_61_64_61_00_01;
/// Fixed-point frame conversions are exact to Q32.32 rounding, not bit-exact.
const EPS: f64 = 1e-6;

/// A backend with the map `script` loaded and `init` run, under a bridge whose
/// `grid_spawn` answers `-1` (no renderer). If the frame verbs work here, they
/// work on the oracle and on a joining peer that has not drawn a frame yet.
fn headless(script: &str) -> (SharedWorld, RhaiBackend) {
    let world = shared_world(SEED);
    let mut backend = RhaiBackend::new(world.clone());
    let bridge: SharedBridge = Arc::new(Mutex::new(TerrainBridge::new()));
    backend.set_bridge(&bridge);
    backend.load(script).expect("compile");
    backend.on_init().expect("init");
    (world, backend)
}

fn field(world: &SharedWorld, e: u64, name: &str) -> f64 {
    world
        .lock()
        .expect("world mutex")
        .field(EntityId(e), name)
        .expect("field exists")
        .to_f64()
}

fn near(got: f64, want: f64, what: &str) {
    assert!((got - want).abs() < EPS, "{what}: want {want}, got {got}");
}

/// The map's own preamble: a tumbling hull like the ship demo's, and one crew
/// member standing at a known WORLD point.
const HULL: &str = r#"
fn hull() { 0 }
fn stood() { vec3(fixed(5), fixed(2), fixed(3)) }

fn build() {
    archetype(["wx", "wy", "wz", "handle", "grid", "riders", "attached"]);
    let h = grid_spawn_cubic(4, 3, 0);
    grid_pivot(h, vec3(ratio(19, 2), ratio(19, 2), fixed(2)));
    grid_orient(h, vec3(ratio(3, 10), fixed(0), fixed(1)), ratio(7, 10));
    let crew = entity_create(0);
    entity_set_field(crew, "handle", fixed(h));
    entity_set_position(crew, stood());
    crew
}

/// Record where `e` is in WORLD coordinates, whatever frame it currently rides.
fn note_world(e) {
    let w = grid_world(entity_grid(e), entity_position(e));
    entity_set_field(e, "wx", w.x);
    entity_set_field(e, "wy", w.y);
    entity_set_field(e, "wz", w.z);
}
"#;

/// `entity_attach` re-expresses a position in the hull's frame without moving
/// the crew member in the world — and `grid_world` reads that world point back.
#[test]
fn attach_keeps_the_world_pose_and_grid_world_reads_it_back() {
    let script = format!(
        "{HULL}
fn init() {{
    let crew = build();
    entity_attach(crew, hull());
    note_world(crew);
    entity_set_field(crew, \"grid\", fixed(entity_grid(crew)));
    entity_set_field(crew, \"riders\", fixed(grid_riders(hull()).len()));
}}"
    );
    let (world, _b) = headless(&script);

    near(field(&world, 0, "wx"), 5.0, "world x survives the attach");
    near(field(&world, 0, "wy"), 2.0, "world y survives the attach");
    near(field(&world, 0, "wz"), 3.0, "world z survives the attach");
    near(field(&world, 0, "grid"), 0.0, "entity_grid names the hull");
    near(
        field(&world, 0, "riders"),
        1.0,
        "grid_riders lists the crew",
    );

    // The stored position is now hull-LOCAL: the test would prove nothing if the
    // frame happened to be the identity.
    let local = world
        .lock()
        .expect("world mutex")
        .position(EntityId(0))
        .expect("crew alive");
    assert!(
        (local.x.to_f64() - 5.0).abs() > 0.5,
        "a tumbling hull's local frame differs from the world one, got {local:?}"
    );
}

/// The frame table answers with no renderer behind it: `grid_spawn_cubic`
/// returns a real handle where the bare bridge would answer `-1`, because the
/// sim-layer verbs shadow the bridge-only ones. This is what lets the oracle and
/// a headless peer compute the same frames a windowed client does.
#[test]
fn the_frame_table_answers_without_a_renderer() {
    let script = format!(
        "{HULL}
fn init() {{
    let crew = build();
    entity_attach(crew, hull());
    note_world(crew);
}}"
    );
    let (world, _b) = headless(&script);
    near(field(&world, 0, "handle"), 0.0, "a real handle, not -1");
    near(field(&world, 0, "wx"), 5.0, "and a real conversion");
}

/// `entity_detach` puts the crew member back in world coordinates without
/// moving it, and `grid_despawn` does the same for everyone still aboard —
/// leaving them ALIVE (a render frame never kills sim entities).
#[test]
fn detach_and_despawn_return_riders_to_world_coordinates() {
    let script = format!(
        "{HULL}
fn init() {{
    let crew = build();
    entity_attach(crew, hull());
    entity_set_field(crew, \"attached\", fixed(entity_grid(crew)));
    entity_detach(crew);
    note_world(crew);
    entity_set_field(crew, \"grid\", fixed(entity_grid(crew)));

    // A second rider is carried off by the despawn instead.
    let cargo = entity_create(0);
    entity_set_position(cargo, stood());
    entity_attach(cargo, hull());
    grid_despawn(hull());
    note_world(cargo);
    entity_set_field(cargo, \"grid\", fixed(entity_grid(cargo)));
    entity_set_field(cargo, \"riders\", fixed(grid_riders(hull()).len()));
}}"
    );
    let (world, _b) = headless(&script);

    near(field(&world, 0, "attached"), 0.0, "was aboard");
    near(field(&world, 0, "grid"), -1.0, "detached ⇒ no grid");
    near(field(&world, 0, "wx"), 5.0, "detach kept the world pose");

    near(field(&world, 1, "grid"), -1.0, "despawn detached the cargo");
    near(field(&world, 1, "wx"), 5.0, "and kept its world pose");
    near(field(&world, 1, "riders"), 0.0, "a dead hull has no riders");
    assert_eq!(
        world.lock().expect("world mutex").count(ArchetypeId(0)),
        2,
        "both entities are still alive — despawning a grid is not a cull"
    );
}

/// A binding is retired with its entity: the tick that despawns a rider leaves
/// no trace of it in the frame table (a long session churning crew must not
/// grow the map forever).
#[test]
fn a_binding_retires_with_its_entity() {
    let script = format!(
        "{HULL}
fn init() {{
    let crew = build();
    entity_attach(crew, hull());
}}

fn tick() {{
    for e in entities_of(0) {{
        entity_despawn(e);
    }}
}}"
    );
    let (_world, mut backend) = headless(&script);
    assert_eq!(
        backend.grids().lock().expect("grids").riders(0).len(),
        1,
        "aboard after init"
    );
    backend.on_tick().expect("tick");
    assert!(
        backend.grids().lock().expect("grids").riders(0).is_empty(),
        "the despawned rider's binding is gone"
    );
}

/// The sync wall, at the grid verbs: the local layer may READ frames (turning a
/// cursor hit into a hull cell) but the mutators are not registered there at
/// all, so a per-client script physically cannot move a hull or re-seat an
/// entity — the same split `register_world_read_api` draws for entity state.
#[test]
fn the_local_layer_reads_frames_but_cannot_move_them() {
    let world = shared_world(SEED);
    let mut sim = RhaiBackend::new(world.clone());
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    sim.set_bridge(&bridge);
    sim.load(&format!("{HULL}\nfn init() {{ build(); }}"))
        .expect("compile");
    sim.on_init().expect("init");

    let mut local = LocalBackend::new(&world, &bridge);
    local.set_grids(sim.grids());
    local
        .load("fn local_tick(dt) { let p = grid_world(0, vec3(fixed(1), fixed(1), fixed(0))); }")
        .expect("compile local");
    local
        .on_local_tick(Fixed::from_ratio(1, 30))
        .expect("the local layer may convert a point through a grid frame");

    let mut moves = LocalBackend::new(&world, &bridge);
    moves.set_grids(sim.grids());
    moves
        .load("fn local_tick(dt) { entity_attach(0, 0); }")
        .expect("compile local");
    let err = moves
        .on_local_tick(Fixed::from_ratio(1, 30))
        .expect_err("entity_attach must not exist in the local layer");
    assert!(
        format!("{err}").contains("entity_attach"),
        "the raise should name the missing verb, got {err}"
    );
}
