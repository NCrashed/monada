//! S-B canary for the ship demo: load the rules from the packed `map/`
//! archive under a [`TerrainBridge`] (render calls — actor, anim, camera, sky —
//! are no-ops), then drive the real-time input + tick path and assert the crew
//! member spawns, walks, is contained by the hull, crosses decks at the stair,
//! and — the point of S-B — collides DECK-RELATIVELY. (Wall collision is a
//! script-side predicate, not `voxel_solid`: monada's heightmap store can't
//! represent stacked decks — see `main.rhai`'s `blocked`.) Mirrors monada-rpg's
//! `gameplay.rs`; the seed of the future `ship@` oracle golden.
//!
//! The navigation tests assume the script's default `cam_yaw` (0.8): with it,
//! `input(1, -1)` is very nearly pure +x (east) and `input(1, 1)` very nearly
//! pure +y (north). If `cam_yaw` changes, revisit the input vectors here.

// The `deck` field is a small integer stored as fixed-point; reading it back
// as i64 through f64 is exact for the 0/1 values here.
#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

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
    // actor / anim / camera / sky calls are no-ops headlessly.
    let bridge: SharedBridge = Arc::new(Mutex::new(TerrainBridge::new()));
    backend.set_bridge(&bridge);
    backend.load(&script()).expect("compile main.rhai");
    backend.on_init().expect("init runs");
    (world, backend)
}

/// One real-time input command: a move axis (buttons unused in S-B).
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

/// Drive one held input for `n` ticks.
fn hold(b: &mut RhaiBackend, mx: i32, my: i32, n: usize) {
    for _ in 0..n {
        step(b, &input(mx, my));
    }
}

/// Walk the crew east from spawn onto the fore staircase and up its steps to the
/// upper deck (the stairs climb east: `input(1, -1)` ≈ east under the default
/// `cam_yaw`). Leaves the crew on deck 1 at the top of the stairs (cx ≈ 18).
fn climb_to_upper(b: &mut RhaiBackend) {
    hold(b, 1, -1, 120);
}

fn count(world: &SharedWorld, arch: ArchetypeId) -> usize {
    world.lock().unwrap().count(arch)
}

fn crew_pos(world: &SharedWorld) -> FixedVec3 {
    let w = world.lock().unwrap();
    let e = w.entities(CREW)[0];
    w.position(e).expect("crew has a position")
}

fn crew_deck(world: &SharedWorld) -> i64 {
    let w = world.lock().unwrap();
    let e = w.entities(CREW)[0];
    w.field(e, "deck").expect("crew has a deck field").to_f64() as i64
}

#[test]
fn crew_spawns_on_first_input() {
    let (world, mut b) = fresh();
    assert_eq!(count(&world, CREW), 0, "no crew before any input");
    step(&mut b, &input(0, 0));
    assert_eq!(count(&world, CREW), 1, "first input spawns the local crew");
    assert_eq!(crew_deck(&world), 0, "crew starts on the lower deck");
}

#[test]
fn crew_walks() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // spawn at (5, 4)
    let start = crew_pos(&world);
    hold(&mut b, 1, 0, 30); // hold a movement axis; the crew should travel
    let end = crew_pos(&world);
    let moved = (end.x - start.x).to_f64().abs() + (end.y - start.y).to_f64().abs();
    assert!(
        moved > 1.0,
        "crew moved under sustained input (was {moved})"
    );
}

#[test]
fn hull_walls_contain_the_crew() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // spawn
                                // Shove around for far longer than it takes to reach any wall.
    hold(&mut b, 1, 0, 400);
    let p = crew_pos(&world);
    // Walkable interior is cells [1, 18]; the footprint radius keeps the centre
    // short of the rim wall on every axis. It must not have escaped the hull.
    let x = p.x.to_f64();
    let y = p.y.to_f64();
    assert!(
        x > 0.5 && x < 18.5,
        "crew stayed within the hull in x (x = {x})"
    );
    assert!(
        y > 0.5 && y < 18.5,
        "crew stayed within the hull in y (y = {y})"
    );
    assert_eq!(count(&world, CREW), 1, "no crew lost");
}

#[test]
fn stairwell_climbs_to_upper_deck() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // spawn on the lower deck (deck 0, z 0)
    assert_eq!(crew_deck(&world), 0);
    climb_to_upper(&mut b);
    // Walking up the steps flipped the deck (z crossed the DECK_STRIDE
    // midpoint) and left the crew standing on the upper floor plate — sim-z 3,
    // since the hull is a CUBIC grid where a storey is 3 whole cells (it was 28
    // unscaled units under the column cell).
    assert_eq!(crew_deck(&world), 1, "climbed the stairwell → upper deck");
    let z = crew_pos(&world).z.to_f64();
    assert!(
        (z - 3.0).abs() < 0.01,
        "reached the upper deck floor (z=3, was {z})"
    );
}

#[test]
fn lower_divider_blocks_on_its_deck() {
    // Control: on the lower deck, the y=10 divider blocks a northward walk
    // (except through the doorway) — so the wall genuinely exists.
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // spawn (5, 2), lower deck, away from the doorway
    hold(&mut b, 1, 1, 120); // ≈ +y (north) into the divider
    assert_eq!(crew_deck(&world), 0, "stayed on the lower deck");
    let y = crew_pos(&world).y.to_f64();
    assert!(
        y < 10.0,
        "lower-deck divider stopped the crew short of y=10 (y = {y})"
    );
}

#[test]
fn upper_deck_ignores_the_lower_wall() {
    // The point of deck-relative collision: on the UPPER deck the crew crosses
    // y=10 freely at x≥16, where the LOWER deck has divider walls. A deck-blind
    // check would stop it there.
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // spawn
    climb_to_upper(&mut b); // stairwell → upper deck, at ~(18, 7)
    assert_eq!(crew_deck(&world), 1, "on the upper deck");
    hold(&mut b, 1, 1, 90); // ≈ +y (north) at cx≈18, across the y=10 line
    assert_eq!(crew_deck(&world), 1, "stayed on the upper deck");
    let y = crew_pos(&world).y.to_f64();
    assert!(
        y > 11.0,
        "upper deck crossed y=10 where the lower deck is walled (y = {y})"
    );
}

#[test]
fn deterministic_walk() {
    // Same seed + same inputs → identical crew pose (the lockstep contract;
    // TerrainBridge is deterministic and render calls are no-ops).
    let run = || {
        let (world, mut b) = fresh();
        step(&mut b, &input(0, 0));
        for i in 0..60 {
            step(&mut b, &input(1, i32::from(i % 2 == 0)));
        }
        let p = crew_pos(&world);
        (
            p.x.to_bits(),
            p.y.to_bits(),
            p.z.to_bits(),
            crew_deck(&world),
        )
    };
    assert_eq!(run(), run(), "identical inputs reproduce the crew's path");
}

#[test]
fn stairwell_round_trips_both_ways() {
    // Climb the stairwell UP to the upper deck, then walk back DOWN it — the z
    // ramps continuously (no teleport) and the deck follows z, landing on each
    // deck's floor. Exercises the walkable-ramp transition both directions.
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // deck 0 (lower)
    climb_to_upper(&mut b);
    assert_eq!(crew_deck(&world), 1, "climbed up to the upper deck");
    let z_up = crew_pos(&world).z.to_f64();
    assert!(
        (z_up - 3.0).abs() < 0.01,
        "reached the upper floor (z=3, was {z_up})"
    );

    // Walk back WEST DOWN the steps → lower deck.
    hold(&mut b, -1, 1, 120); // W down the steps and off the bottom (cx < 14)
    assert_eq!(crew_deck(&world), 0, "walked back down to the lower deck");
    let z_dn = crew_pos(&world).z.to_f64();
    assert!(
        z_dn.abs() < 0.01,
        "reached the lower floor (z=0, was {z_dn})"
    );

    // Not soft-locked: it still walks on the lower deck.
    let before = crew_pos(&world);
    hold(&mut b, 1, 1, 20);
    let after = crew_pos(&world);
    let moved = (after.x - before.x).to_f64().abs() + (after.y - before.y).to_f64().abs();
    assert!(moved > 0.5, "crew walks after descending (moved = {moved})");
}

// --- cargo + airlock (grid membership, docs/plans/grid-entities.md S-4) ------
// A crate is an entity whose FRAME is the gameplay: stowed it rides the hull,
// released through the open airlock it is left behind in the world while the
// ship turns away. These tests read the sim's own frame table (the same one
// `grid_world` answers from), so they check the map's semantics, not the
// renderer's.

const CRATE: ArchetypeId = ArchetypeId(2);

/// One command with the button mask the local layer would fold into `arg.z`
/// (1 = use, 2 = airlock). A press is one tick with the bit set: the sim acts
/// on the rising edge, so holding it does not repeat.
fn input_btn(mx: i32, my: i32, btn: i32) -> Command {
    Command::on(
        VERB_INPUT,
        EntityId(0),
        FixedVec3::new(
            Fixed::from_int(mx),
            Fixed::from_int(my),
            Fixed::from_int(btn),
        ),
    )
}

fn crate_of(world: &SharedWorld, i: usize) -> EntityId {
    world.lock().unwrap().entities(CRATE)[i]
}

/// A crate's position in its own frame, and the grid it rides (`-1` = the
/// world frame — it was released).
fn crate_local(world: &SharedWorld, i: usize) -> FixedVec3 {
    let k = crate_of(world, i);
    world.lock().unwrap().position(k).expect("crate exists")
}

fn crate_grid(world: &SharedWorld, b: &RhaiBackend, i: usize) -> i64 {
    let k = crate_of(world, i);
    b.grids().lock().unwrap().grid_of(k)
}

/// A crate in WORLD coordinates, composed through whatever frame it rides —
/// exactly what the map's `grid_world` computes.
fn crate_world(world: &SharedWorld, b: &RhaiBackend, i: usize) -> FixedVec3 {
    let k = crate_of(world, i);
    let p = { world.lock().unwrap().position(k).expect("crate exists") };
    let grids = b.grids().lock().unwrap();
    grids.to_world(grids.grid_of(k), p)
}

fn crew_carry(world: &SharedWorld) -> i64 {
    let w = world.lock().unwrap();
    let e = w.entities(CREW)[0];
    w.field(e, "carry")
        .expect("crew has a carry field")
        .to_f64() as i64
}

fn moved(a: FixedVec3, b: FixedVec3) -> f64 {
    (a.x - b.x).to_f64().abs() + (a.y - b.y).to_f64().abs() + (a.z - b.z).to_f64().abs()
}

/// Walk the crew to the starboard airlock at hull cell (19, 9): north until the
/// y=10 divider parks it in the door's row, then east into the doorway.
fn walk_to_airlock(b: &mut RhaiBackend) {
    hold(b, 1, 1, 80); // north until the y=10 divider parks it
    hold(b, 1, -1, 140); // east along the divider to the starboard rim
                         // The east run drifts a third of a cell south (the input is not exactly
                         // axis-aligned), which puts a footprint corner in row 8 — rim wall beside
                         // the doorway. Re-seat in the door row, then step in.
    hold(b, 1, 1, 12);
    hold(b, 1, -1, 30);
}

#[test]
fn a_stowed_crate_rides_the_hull() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0));
    let local_before = crate_local(&world, 0);
    let world_before = crate_world(&world, &b, 0);

    hold(&mut b, 0, 0, 60); // the hull turns and sways under it

    assert_eq!(crate_grid(&world, &b, 0), 0, "still aboard the hull");
    assert!(
        moved(crate_local(&world, 0), local_before) < 1e-9,
        "a stowed crate does not move in the ship's own frame"
    );
    assert!(
        moved(crate_world(&world, &b, 0), world_before) > 1.0,
        "but the hull carried it somewhere else in the world"
    );
}

#[test]
fn use_picks_a_crate_up_and_sets_it_down() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // spawn at (5, 2); crate 0 waits at (6, 2)
    assert_eq!(crew_carry(&world), 0, "hands free");

    step(&mut b, &input_btn(0, 0, 1)); // press E
    assert_eq!(
        crew_carry(&world),
        crate_of(&world, 0).0 as i64 + 1,
        "picked up the nearest crate (its id + 1, since 0 means empty hands)"
    );

    hold(&mut b, 1, 1, 20); // carry it a few cells
    assert!(
        moved(crate_local(&world, 0), crew_pos(&world)) < 1e-9,
        "the carried crate sits on its crew member's cell"
    );

    step(&mut b, &input_btn(0, 0, 1)); // press E again, on the deck
    assert_eq!(crew_carry(&world), 0, "hands free again");
    assert_eq!(
        crate_grid(&world, &b, 0),
        0,
        "set down INSIDE the ship, so it stays in the ship's frame"
    );
}

#[test]
fn a_crate_released_through_the_airlock_stays_in_space() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0));
    step(&mut b, &input_btn(0, 0, 2)); // F: cycle the airlock open
    step(&mut b, &input_btn(0, 0, 1)); // E: pick the crate up
    assert!(crew_carry(&world) != 0, "carrying it to the airlock");

    walk_to_airlock(&mut b);
    let x = crew_pos(&world).x.to_f64();
    assert!(x > 18.6, "the crew reached the open doorway (x = {x})");

    step(&mut b, &input_btn(0, 0, 1)); // E: let go, in the doorway
    assert_eq!(
        crate_grid(&world, &b, 0),
        -1,
        "released into the WORLD frame, not stowed"
    );
    let released_at = crate_world(&world, &b, 0);
    let crew_before = crew_pos(&world);

    hold(&mut b, 0, 0, 90); // the ship turns and sways away from it

    assert!(
        moved(crate_world(&world, &b, 0), released_at) < 1e-9,
        "the crate stays exactly where it was let go"
    );
    assert!(
        moved(crew_pos(&world), crew_before) < 1e-9,
        "the crew has not walked — its own cell is unchanged…"
    );
    let crew_world = {
        let p = crew_pos(&world);
        let grids = b.grids().lock().unwrap();
        grids.to_world(0, p)
    };
    assert!(
        moved(crew_world, released_at) > 1.0,
        "…yet the hull carried the CREW away from the crate it left behind"
    );
    assert_eq!(
        count(&world, CRATE),
        2,
        "releasing a crate does not destroy it"
    );
}

#[test]
fn the_airlock_gates_its_doorway() {
    // Closed, the doorway is rim wall like the rest of the hull.
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0));
    walk_to_airlock(&mut b);
    let shut = crew_pos(&world).x.to_f64();
    assert!(shut < 18.6, "a closed airlock stops the crew (x = {shut})");

    // Open, the same walk carries the crew into it.
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0));
    step(&mut b, &input_btn(0, 0, 2));
    walk_to_airlock(&mut b);
    let open = crew_pos(&world).x.to_f64();
    assert!(open > 18.6, "an open airlock lets the crew in (x = {open})");
}
