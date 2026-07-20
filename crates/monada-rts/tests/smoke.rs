//! R-A canary for the RTS demo: load the rules from the packed `map/`
//! archive under a [`TerrainBridge`] (render calls — actor, anim, camera,
//! tint — are no-ops), then drive the event-driven order path and assert the
//! starting workers exist, a MOVE command walks its unit to the destination,
//! ownership gates foreign orders, and the march is deterministic. Mirrors
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
    // TerrainBridge: `init`'s rim `tile_fill` fills a real collision store;
    // the actor / anim / camera / tint calls are no-ops headlessly.
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
fn workers_spawn_at_init() {
    let (world, _b) = fresh();
    let us = units(&world);
    assert_eq!(us.len(), 6, "three starting workers per player");
    let owners: Vec<i64> = us.iter().map(|&e| field(&world, e, "owner")).collect();
    assert_eq!(owners, [0, 0, 0, 1, 1, 1], "corners are owned symmetrically");
}

#[test]
fn move_command_walks_the_unit() {
    let (world, mut b) = fresh();
    let u = units(&world)[0]; // player 0's first worker, at (6, 6)
    b.on_command(P0, &move_cmd(u, 20, 20)).expect("move order");
    ticks(&mut b, 200); // plenty for ~14 diagonal cells at 0.14/tick
    let p = pos(&world, u);
    assert!(
        (p.x.to_f64() - 20.0).abs() < 0.3 && (p.y.to_f64() - 20.0).abs() < 0.3,
        "unit reached its destination (at {:.2}, {:.2})",
        p.x.to_f64(),
        p.y.to_f64()
    );
    assert_eq!(field(&world, u, "has_dest"), 0, "order completed and cleared");
}

#[test]
fn ownership_gates_orders() {
    // Player 1 tries to move player 0's worker: the sim-side check drops the
    // command — whatever a hacked local layer submits, foreign units ignore it.
    let (world, mut b) = fresh();
    let u = units(&world)[0]; // owner 0
    let before = pos(&world, u);
    b.on_command(P1, &move_cmd(u, 20, 20)).expect("foreign order");
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
    // An order into / beyond the rim parks the unit at the field edge
    // instead of grinding against the wall forever.
    let (world, mut b) = fresh();
    let u = units(&world)[0];
    b.on_command(P0, &move_cmd(u, 100, 100)).expect("wild order");
    ticks(&mut b, 500);
    let end = pos(&world, u);
    let (ex, ey) = (end.x.to_f64(), end.y.to_f64());
    assert!(
        ex < 46.5 && ey < 46.5,
        "unit stayed inside the rim (at {ex:.2}, {ey:.2})"
    );
    assert_eq!(
        field(&world, u, "has_dest"),
        0,
        "clamped order still completes (no forever-marching unit)"
    );
}

#[test]
fn deterministic_march() {
    // Same seed + same orders → bit-identical positions (the lockstep
    // contract; TerrainBridge is deterministic, render calls are no-ops).
    let run = || {
        let (world, mut b) = fresh();
        let us = units(&world);
        b.on_command(P0, &move_cmd(us[0], 30, 12)).expect("order 0");
        b.on_command(P1, &move_cmd(us[3], 14, 33)).expect("order 1");
        ticks(&mut b, 150);
        let a = pos(&world, us[0]);
        let c = pos(&world, us[3]);
        (a.x.to_bits(), a.y.to_bits(), c.x.to_bits(), c.y.to_bits())
    };
    assert_eq!(run(), run(), "identical orders reproduce the march");
}
