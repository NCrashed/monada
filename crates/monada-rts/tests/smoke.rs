//! R-B canary for the RTS demo: load the rules from the packed `map/`
//! archive under a [`TerrainBridge`] (render calls are no-ops; the terrain
//! paints fill a real collision store, which now also serves `nav_path`),
//! then drive the order path and assert the walk rule end to end: workers
//! start on their plateaus, a MOVE across the map descends BY THE RAMP
//! (never a cliff jump), tree columns park the walker adjacent, ownership
//! gates foreign orders, and the march is deterministic. Mirrors
//! monada-ship's `smoke.rs`; the seed of the future `rts@` oracle golden.

// Small integer fields (owner / has_dest) read back through f64 exactly.
#![allow(clippy::cast_possible_truncation)]

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_script::{
    shared_world, RhaiBackend, ScriptBackend, SharedBridge, SharedWorld, TerrainBridge,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const UNIT: ArchetypeId = ArchetypeId(0);
const TREE: ArchetypeId = ArchetypeId(1);
const VERB_MOVE: u32 = 1;
const P0: PlayerId = PlayerId(0);
const P1: PlayerId = PlayerId(1);

/// The RTS demo rules, through the real archive path (pack `map/`, read
/// back, take the entry script).
fn script() -> String {
    let map_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack rts map");
    let map = monada_format::Map::read(&bytes).expect("read rts map");
    map.entry_script()
        .expect("rts map has an entry script")
        .to_string()
}

fn fresh() -> (SharedWorld, RhaiBackend) {
    let world = shared_world(SEED);
    let mut backend = RhaiBackend::new(world.clone());
    // TerrainBridge: `init`'s heightfield paints fill a real collision
    // store shared by `ground_height` AND `nav_path`; actor / camera /
    // tint calls are no-ops headlessly.
    let bridge: SharedBridge = Arc::new(Mutex::new(TerrainBridge::new()));
    backend.set_bridge(&bridge);
    backend.load(&script()).expect("compile main.rhai");
    backend.on_init().expect("init runs");
    (world, backend)
}

/// A MOVE order: walk `unit` to `(x, y)`.
fn move_cmd(unit: EntityId, x: i32, y: i32) -> Command {
    Command::on(
        VERB_MOVE,
        unit,
        FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO),
    )
}

fn units(world: &SharedWorld) -> Vec<EntityId> {
    world.lock().unwrap().entities(UNIT).to_vec()
}

fn pos(world: &SharedWorld, e: EntityId) -> FixedVec3 {
    world.lock().unwrap().position(e).expect("unit has a position")
}

fn field(world: &SharedWorld, e: EntityId, name: &str) -> i64 {
    world.lock().unwrap().field(e, name).expect("unit field").to_f64() as i64
}

fn ticks(b: &mut RhaiBackend, n: usize) {
    for _ in 0..n {
        b.on_tick().expect("tick");
    }
}

#[test]
fn workers_spawn_on_their_plateaus() {
    let (world, _b) = fresh();
    let us = units(&world);
    assert_eq!(us.len(), 6, "three starting workers per player");
    let owners: Vec<i64> = us.iter().map(|&e| field(&world, e, "owner")).collect();
    assert_eq!(owners, [0, 0, 0, 1, 1, 1], "corners are owned symmetrically");
    for &u in &us {
        let z = pos(&world, u).z.to_f64();
        assert!((z - 12.0).abs() < 0.01, "worker starts on plateau (z = {z})");
    }
    assert_eq!(
        world.lock().unwrap().count(TREE),
        10,
        "the lowland tree line stands"
    );
}

#[test]
fn march_descends_by_the_ramp_only() {
    // Plateau (5,5, z 12) → lowland (24,24, z 0). A straight line falls
    // off a 12-high cliff; the walk rule forbids it, so `nav_path` must
    // route through the 2-per-cell ramp — observable as the unit's height
    // never changing by more than the walk tolerance in one tick.
    let (world, mut b) = fresh();
    let u = units(&world)[0];
    b.on_command(P0, &move_cmd(u, 24, 24)).expect("move order");
    let mut prev_z = pos(&world, u).z.to_f64();
    let mut max_drop = 0.0_f64;
    for _ in 0..500 {
        b.on_tick().expect("tick");
        let z = pos(&world, u).z.to_f64();
        max_drop = max_drop.max((z - prev_z).abs());
        prev_z = z;
    }
    let p = pos(&world, u);
    assert!(
        (p.x.to_f64() - 24.0).abs() < 0.4 && (p.y.to_f64() - 24.0).abs() < 0.4,
        "unit reached the lowland goal (at {:.2}, {:.2})",
        p.x.to_f64(),
        p.y.to_f64()
    );
    assert!((p.z.to_f64()).abs() < 0.01, "stands on the lowland floor");
    assert!(
        max_drop <= 2.0 + 1e-9,
        "every height change obeyed the walk rule (max step {max_drop})"
    );
    assert_eq!(field(&world, u, "has_dest"), 0, "order completed");
}

#[test]
fn tree_column_parks_the_walker_adjacent() {
    // Order a worker INTO a tree cell (24, 14): unreachable, so the
    // closest-approach path parks it next to the trunk, never on it.
    let (world, mut b) = fresh();
    let u = units(&world)[0];
    b.on_command(P0, &move_cmd(u, 24, 14)).expect("tree order");
    ticks(&mut b, 500);
    let p = pos(&world, u);
    let (cx, cy) = (
        (p.x.to_f64() + 0.5).floor() as i64,
        (p.y.to_f64() + 0.5).floor() as i64,
    );
    assert_ne!((cx, cy), (24, 14), "never stands in the tree");
    let d = (cx - 24).abs().max((cy - 14).abs());
    assert!(d <= 2, "parked beside the tree (at cell {cx}, {cy})");
    assert_eq!(field(&world, u, "has_dest"), 0, "order settled, not spinning");
}

#[test]
fn ownership_gates_orders() {
    // Player 1 tries to move player 0's worker: the sim-side check drops
    // the command — whatever a hacked local layer submits.
    let (world, mut b) = fresh();
    let u = units(&world)[0]; // owner 0
    let before = pos(&world, u);
    b.on_command(P1, &move_cmd(u, 24, 24)).expect("foreign order");
    ticks(&mut b, 60);
    let after = pos(&world, u);
    assert_eq!(
        (before.x.to_bits(), before.y.to_bits()),
        (after.x.to_bits(), after.y.to_bits()),
        "a foreign MOVE order must not move the unit"
    );
    assert_eq!(field(&world, u, "has_dest"), 0, "no order was accepted");
}

#[test]
fn destination_clamps_to_the_field() {
    // An order beyond the rim clamps into the walkable field and still
    // completes (no forever-marching unit against the rim band).
    let (world, mut b) = fresh();
    let u = units(&world)[3]; // player 1's worker, at (42, 42)
    b.on_command(P1, &move_cmd(u, 100, 100)).expect("wild order");
    ticks(&mut b, 500);
    let end = pos(&world, u);
    let (ex, ey) = (end.x.to_f64(), end.y.to_f64());
    assert!(
        ex < 46.5 && ey < 46.5,
        "unit stayed inside the rim (at {ex:.2}, {ey:.2})"
    );
    assert_eq!(field(&world, u, "has_dest"), 0, "clamped order completes");
}

#[test]
fn deterministic_march() {
    // Same seed + same orders → bit-identical positions (the lockstep
    // contract; TerrainBridge + nav are deterministic, render is no-op).
    let run = || {
        let (world, mut b) = fresh();
        let us = units(&world);
        b.on_command(P0, &move_cmd(us[0], 24, 24)).expect("order 0");
        b.on_command(P1, &move_cmd(us[3], 30, 20)).expect("order 1");
        ticks(&mut b, 400);
        let a = pos(&world, us[0]);
        let c = pos(&world, us[3]);
        (a.x.to_bits(), a.y.to_bits(), a.z.to_bits(), c.x.to_bits(), c.y.to_bits(), c.z.to_bits())
    };
    assert_eq!(run(), run(), "identical orders reproduce the march");
}
