//! S-A canary for the ship demo: load the rules from the packed `map/`
//! archive under a [`TerrainBridge`] (so `voxel_fill` paints collidable hull
//! and `voxel_solid`/`ground_height` answer, while render calls — actor, anim,
//! camera — are no-ops), then drive the real-time input + tick path and assert
//! the crew member spawns, walks, and can't leave the hull. Mirrors
//! monada-rpg's `gameplay.rs`; the seed of the future `ship@` oracle golden.

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_script::{
    shared_world, RhaiBackend, ScriptBackend, SharedBridge, SharedWorld, TerrainBridge,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const CREW: ArchetypeId = ArchetypeId(0);
const VERB_INPUT: u32 = 0;
const P0: PlayerId = PlayerId(0);

/// The ship demo rules, through the real archive path (pack `map/`, read back,
/// take the entry script).
fn script() -> String {
    let map_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack ship map");
    let map = monada_format::Map::read(&bytes).expect("read ship map");
    map.entry_script()
        .expect("ship map has an entry script")
        .to_string()
}

fn fresh() -> (SharedWorld, RhaiBackend) {
    let world = shared_world(SEED);
    let mut backend = RhaiBackend::new(world.clone());
    // TerrainBridge: `init`'s voxel paints fill a real collision store; the
    // actor / anim / camera calls are no-ops headlessly.
    let bridge: SharedBridge = Arc::new(Mutex::new(TerrainBridge::new()));
    backend.set_bridge(&bridge);
    backend.load(&script()).expect("compile main.rhai");
    backend.on_init().expect("init runs");
    (world, backend)
}

/// One real-time input command: a move axis (buttons unused in S-A).
fn input(mx: i32, my: i32) -> Command {
    Command::on(
        VERB_INPUT,
        EntityId(0),
        FixedVec3::new(Fixed::from_int(mx), Fixed::from_int(my), Fixed::ZERO),
    )
}

fn step(b: &mut RhaiBackend, cmd: &Command) {
    b.on_command(P0, cmd).expect("input command");
    b.on_tick().expect("tick");
}

fn count(world: &SharedWorld, arch: ArchetypeId) -> usize {
    world.lock().unwrap().count(arch)
}

fn crew_pos(world: &SharedWorld) -> FixedVec3 {
    let w = world.lock().unwrap();
    let e = w.entities(CREW)[0];
    w.position(e).expect("crew has a position")
}

#[test]
fn crew_spawns_on_first_input() {
    let (world, mut b) = fresh();
    assert_eq!(count(&world, CREW), 0, "no crew before any input");
    step(&mut b, &input(0, 0));
    assert_eq!(count(&world, CREW), 1, "first input spawns the local crew");
}

#[test]
fn crew_walks() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // spawn at (8, 10)
    let start = crew_pos(&world);
    // Hold a movement axis for a while; the crew member should travel.
    for _ in 0..30 {
        step(&mut b, &input(1, 0));
    }
    let end = crew_pos(&world);
    let moved = (end.x - start.x).to_f64().abs() + (end.y - start.y).to_f64().abs();
    assert!(moved > 1.0, "crew moved under sustained input (was {moved})");
}

#[test]
fn hull_walls_contain_the_crew() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // spawn
    // Shove toward the +x wall for far longer than it takes to reach it.
    for _ in 0..400 {
        step(&mut b, &input(1, 0));
    }
    let p = crew_pos(&world);
    // Walkable interior is cells [1, 18]; the footprint radius keeps the
    // centre short of the rim wall. It must not have escaped the hull.
    assert!(
        p.x.to_f64() < 18.5,
        "crew stayed inside the east hull wall (x = {})",
        p.x.to_f64()
    );
    assert_eq!(count(&world, CREW), 1, "no crew lost");
}

#[test]
fn deterministic_walk() {
    // Same seed + same inputs → identical crew position (the lockstep contract;
    // TerrainBridge is deterministic and render calls are no-ops).
    let run = || {
        let (world, mut b) = fresh();
        step(&mut b, &input(0, 0));
        for i in 0..40 {
            step(&mut b, &input(1, i32::from(i % 2 == 0)));
        }
        let p = crew_pos(&world);
        (p.x.to_bits(), p.y.to_bits())
    };
    assert_eq!(run(), run(), "identical inputs reproduce the crew's path");
}
