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
#![allow(clippy::cast_possible_truncation)]

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
    // Walking up the ramp flipped the deck (z crossed the DECK_STRIDE midpoint)
    // and left the crew standing on the upper floor (sim-z 28).
    assert_eq!(crew_deck(&world), 1, "climbed the stairwell → upper deck");
    let z = crew_pos(&world).z.to_f64();
    assert!(
        (z - 28.0).abs() < 0.01,
        "reached the upper deck floor (z=28, was {z})"
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
        (z_up - 28.0).abs() < 0.01,
        "reached the upper floor (z=28, was {z_up})"
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
