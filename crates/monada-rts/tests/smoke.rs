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
const BUILDING: ArchetypeId = ArchetypeId(2);
const MINE: ArchetypeId = ArchetypeId(3);
const GAME: ArchetypeId = ArchetypeId(4);
const VERB_MOVE: u32 = 1;
const VERB_HARVEST: u32 = 2;
const VERB_TRAIN: u32 = 3;
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
    world
        .lock()
        .unwrap()
        .position(e)
        .expect("unit has a position")
}

fn field(world: &SharedWorld, e: EntityId, name: &str) -> i64 {
    world
        .lock()
        .unwrap()
        .field(e, name)
        .expect("unit field")
        .to_f64() as i64
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
    assert_eq!(
        owners,
        [0, 0, 0, 1, 1, 1],
        "corners are owned symmetrically"
    );
    for &u in &us {
        let z = pos(&world, u).z.to_f64();
        assert!(
            (z - 12.0).abs() < 0.01,
            "worker starts on plateau (z = {z})"
        );
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
    assert_eq!(
        field(&world, u, "has_dest"),
        0,
        "order settled, not spinning"
    );
}

#[test]
fn ownership_gates_orders() {
    // Player 1 tries to move player 0's worker: the sim-side check drops
    // the command — whatever a hacked local layer submits.
    let (world, mut b) = fresh();
    let u = units(&world)[0]; // owner 0
    let before = pos(&world, u);
    b.on_command(P1, &move_cmd(u, 24, 24))
        .expect("foreign order");
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
    b.on_command(P1, &move_cmd(u, 100, 100))
        .expect("wild order");
    ticks(&mut b, 500);
    let end = pos(&world, u);
    let (ex, ey) = (end.x.to_f64(), end.y.to_f64());
    assert!(
        ex < 46.5 && ey < 46.5,
        "unit stayed inside the rim (at {ex:.2}, {ey:.2})"
    );
    assert_eq!(field(&world, u, "has_dest"), 0, "clamped order completes");
}

/// A HARVEST order: send `unit` to work `mine` (id rides `arg.z`).
fn harvest_cmd(world: &SharedWorld, unit: EntityId, mine_idx: usize) -> Command {
    let w = world.lock().unwrap();
    let m = w.entities(MINE)[mine_idx];
    let p = w.position(m).expect("mine has a position");
    #[allow(clippy::cast_possible_wrap)]
    Command::on(
        VERB_HARVEST,
        unit,
        FixedVec3::new(p.x, p.y, Fixed::from_int(m.0 as i32)),
    )
}

fn train_cmd() -> Command {
    Command::on(VERB_TRAIN, EntityId(0), FixedVec3::ZERO)
}

fn gold(world: &SharedWorld, player: usize) -> i64 {
    let w = world.lock().unwrap();
    let g = w.entities(GAME)[0];
    w.field(g, if player == 0 { "gold0" } else { "gold1" })
        .expect("purse")
        .to_f64() as i64
}

#[test]
fn economy_spawns_at_init() {
    let (world, _b) = fresh();
    let w = world.lock().unwrap();
    assert_eq!(w.count(BUILDING), 2, "a town hall per player");
    assert_eq!(w.count(MINE), 2, "a gold mine per plateau");
    assert_eq!(w.count(GAME), 1, "the game singleton");
    drop(w);
    assert_eq!(gold(&world, 0), 100, "starting purse");
    assert_eq!(gold(&world, 1), 100, "starting purse");
}

#[test]
fn harvest_loop_banks_gold_and_conserves_it() {
    // One worker on the harvest loop: walk to the mine block (nav parks it
    // on the ring), mine for the visit time, carry home, bank, repeat.
    let (world, mut b) = fresh();
    let u = units(&world)[0];
    b.on_command(P0, &harvest_cmd(&world, u, 0)).expect("harvest order");
    ticks(&mut b, 1500);

    let banked = gold(&world, 0) - 100;
    assert!(
        banked >= 30,
        "several round trips banked gold (banked {banked})"
    );
    assert_eq!(banked % 10, 0, "gold moves in whole trips");

    // Conservation: banked + still-carried + left-in-mine = the reserve.
    let w = world.lock().unwrap();
    let m = w.entities(MINE)[0];
    let left = w.field(m, "gold").expect("mine reserve").to_f64() as i64;
    let carried = w.field(u, "carry").expect("carry").to_f64() as i64;
    assert_eq!(banked + carried + left, 500, "no gold minted or lost");
}

#[test]
fn move_order_interrupts_the_harvest() {
    let (world, mut b) = fresh();
    let u = units(&world)[0];
    b.on_command(P0, &harvest_cmd(&world, u, 0)).expect("harvest");
    ticks(&mut b, 400); // deep in the loop by now
    b.on_command(P0, &move_cmd(u, 20, 24)).expect("countermand");
    ticks(&mut b, 400);
    let before = gold(&world, 0);
    ticks(&mut b, 300);
    assert_eq!(
        gold(&world, 0),
        before,
        "harvesting stopped after the explicit MOVE"
    );
    let p = pos(&world, u);
    assert!(
        (p.x.to_f64() - 20.0).abs() < 0.4 && (p.y.to_f64() - 24.0).abs() < 0.4,
        "worker obeyed the countermand (at {:.2}, {:.2})",
        p.x.to_f64(),
        p.y.to_f64()
    );
}

#[test]
fn training_costs_gold_and_stops_at_an_empty_purse() {
    let (world, mut b) = fresh();
    assert_eq!(units(&world).len(), 6);

    // 1st TRAIN: 100 → 50 gold, a 4th worker pops out at the hall ring.
    b.on_command(P0, &train_cmd()).expect("train 1");
    assert_eq!(gold(&world, 0), 50, "cost deducted at command time");
    ticks(&mut b, 150);
    let p0_units = |w: &SharedWorld| {
        let guard = w.lock().unwrap();
        guard
            .entities(UNIT)
            .iter()
            .filter(|&&e| {
                guard.field(e, "owner").expect("owner").to_f64() as i64 == 0
            })
            .count()
    };
    assert_eq!(p0_units(&world), 4, "trained worker delivered");

    // 2nd empties the purse; a 3rd in the same tick must be refused.
    b.on_command(P0, &train_cmd()).expect("train 2");
    b.on_command(P0, &train_cmd()).expect("train 3 (refused)");
    assert_eq!(gold(&world, 0), 0, "second cost deducted, third refused");
    ticks(&mut b, 200);
    assert_eq!(p0_units(&world), 5, "exactly one more worker delivered");
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
        (
            a.x.to_bits(),
            a.y.to_bits(),
            a.z.to_bits(),
            c.x.to_bits(),
            c.y.to_bits(),
            c.z.to_bits(),
        )
    };
    assert_eq!(run(), run(), "identical orders reproduce the march");
}
