//! S-B canary for the ship demo: load the rules from the packed `map/`
//! archive under a [`NullBridge`] (render calls — actor, anim, camera, sky —
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
use monada_net::SimDriver;
use monada_script::{
    shared_physics, shared_world, NullBridge, RhaiDriver, SharedBridge, SharedWorld,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const CREW: ArchetypeId = ArchetypeId(0);
const SHIP: ArchetypeId = ArchetypeId(1);
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

/// The map under a headless driver. `RhaiDriver`, not a bare backend: the
/// hull's pose comes from the dynamics now (docs/plans/ship-physics.md), and
/// the tick order that produces it — script `tick`, physics step, then the
/// grid-frame sync — lives in the driver. A canary that drove the backend
/// directly would be testing a ship whose engines never fired.
fn fresh() -> (SharedWorld, RhaiDriver) {
    let world = shared_world(SEED);
    // NullBridge: `init`'s voxel paints fill a real collision store; the
    // actor / anim / camera / sky calls are no-ops headlessly.
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let phys = shared_physics(30);
    let driver = RhaiDriver::with_physics(world.clone(), &script(), &bridge, &phys)
        .expect("compile main.rhai");
    (world, driver)
}

/// One real-time input command: a move axis (buttons unused in S-B).
fn input(mx: i32, my: i32) -> Command {
    Command::on(
        VERB_INPUT,
        EntityId(0),
        FixedVec3::new(Fixed::from_int(mx), Fixed::from_int(my), Fixed::ZERO),
    )
}

fn step(b: &mut RhaiDriver, cmd: &Command) {
    b.apply_command(P0, cmd);
    b.step();
}

/// Drive one held input for `n` ticks.
fn hold(b: &mut RhaiDriver, mx: i32, my: i32, n: usize) {
    for _ in 0..n {
        step(b, &input(mx, my));
    }
}

/// Walk the crew east from spawn onto the fore staircase and up its steps to the
/// upper deck (the stairs climb east: `input(1, -1)` ≈ east under the default
/// `cam_yaw`). Leaves the crew on deck 1 at the top of the stairs (cx ≈ 18).
fn climb_to_upper(b: &mut RhaiDriver) {
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

/// Where the crew member actually IS — its hull-local seat composed through
/// the hull's frame. The number that moves when the ship flies while
/// `crew_pos` (the seat) stays put.
fn crew_world(world: &SharedWorld, b: &RhaiDriver) -> FixedVec3 {
    let p = crew_pos(world);
    b.grids().lock().unwrap().to_world(0, p)
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
    // NullBridge is deterministic and render calls are no-ops).
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

/// Hold the main drive (input bit 4) for `n` ticks. The hull no longer moves
/// on its own — its pose is an outcome of the dynamics now, so a test that
/// wants the ship to go somewhere has to fly it.
fn burn(b: &mut RhaiDriver, n: usize) {
    for _ in 0..n {
        step(b, &input_btn(0, 0, 4));
    }
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

fn crate_grid(world: &SharedWorld, b: &RhaiDriver, i: usize) -> i64 {
    let k = crate_of(world, i);
    b.grids().lock().unwrap().grid_of(k)
}

/// A crate in WORLD coordinates, composed through whatever frame it rides —
/// exactly what the map's `grid_world` computes.
fn crate_world(world: &SharedWorld, b: &RhaiDriver, i: usize) -> FixedVec3 {
    let k = crate_of(world, i);
    let p = { world.lock().unwrap().position(k).expect("crate exists") };
    let grids = b.grids().lock().unwrap();
    grids.to_world(grids.grid_of(k), p)
}

/// A crate's `dir`/`roll` field (the `Direction`/`Roll` discriminants
/// `entity_set_side` was last called with).
fn crate_field(world: &SharedWorld, i: usize, name: &str) -> i64 {
    let k = crate_of(world, i);
    world
        .lock()
        .unwrap()
        .field(k, name)
        .expect("crate field")
        .to_f64() as i64
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
fn walk_to_airlock(b: &mut RhaiDriver) {
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

    burn(&mut b, 60); // fly the ship out from under it

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
fn keys_1_and_2_turn_a_carried_crate() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0)); // spawn at (5, 2); crate 0 waits at (6, 2)
    assert_eq!(
        crate_field(&world, 0, "dir"),
        2,
        "stowed facing Direction::Y"
    );
    assert_eq!(crate_field(&world, 0, "roll"), 0, "stowed at Roll::Deg0");

    // Bit 32 (key 1) is a no-op with empty hands.
    step(&mut b, &input_btn(0, 0, 32));
    assert_eq!(
        crate_field(&world, 0, "dir"),
        2,
        "nobody is carrying it yet"
    );

    step(&mut b, &input_btn(0, 0, 1)); // F: pick the crate up
    assert!(crew_carry(&world) != 0, "carrying it");

    step(&mut b, &input_btn(0, 0, 32)); // 1: rotate around sim +x
    assert_eq!(
        crate_field(&world, 0, "dir"),
        4,
        "Direction::Y rotated 90° around X lands on Direction::Z"
    );
    // Holding it does not repeat the turn every tick — only the rising edge
    // fires, same as `use`/`door`.
    step(&mut b, &input_btn(0, 0, 32));
    assert_eq!(crate_field(&world, 0, "dir"), 4, "held, not repeated");

    step(&mut b, &input_btn(0, 0, 64)); // 2: roll CW
    assert_eq!(
        crate_field(&world, 0, "roll"),
        1,
        "Roll::Deg0 CW is Roll::Deg90"
    );
    step(&mut b, &input_btn(0, 0, 64)); // held
    assert_eq!(crate_field(&world, 0, "roll"), 1, "held, not repeated");
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

    burn(&mut b, 90); // the ship flies away from it under power

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

// --- the ship flies (docs/plans/ship-physics.md S-5) ---------------------
// The hull is a rigid body with engines bolted to it. What these pin is not
// that physics works — `monada-physics` has its own goldens — but that the
// SHIP is the body: that the map's engines move the frame the crew stand in,
// and that standing in a frame under acceleration is still just standing.

/// The headline of the whole slice: burning moves the ship through the world
/// while changing NOTHING about being aboard it. The crew member's own
/// position is hull-local and untouched; where it is in the world is entirely
/// the hull's doing.
#[test]
fn the_main_drive_moves_the_ship_and_everyone_in_it() {
    let (world, mut b) = fresh();
    step(&mut b, &input(0, 0));
    let local_before = crew_pos(&world);
    let world_before = crew_world(&world, &b);

    burn(&mut b, 60);

    assert!(
        moved(crew_pos(&world), local_before) < 1e-9,
        "the crew member did not walk anywhere — it is standing still aboard"
    );
    assert!(
        moved(crew_world(&world, &b), world_before) > 1.0,
        "yet the ship carried it somewhere else entirely"
    );
}

/// Turning is a reaction wheel, and letting go is a stabiliser: the map's own
/// `τ = −k·ω` bleeds off the tumble it is not being asked for. A ship that
/// kept spinning after the key came up would be unflyable, and there is no
/// engine knob for it — those three lines are in `main.rhai`.
#[test]
fn the_ship_turns_and_then_stops_turning() {
    let (_world, mut b) = fresh();
    step(&mut b, &input(0, 0));
    let heading = |b: &RhaiDriver| {
        let grids = b.grids().lock().unwrap();
        let o = grids.to_world(0, FixedVec3::ZERO);
        let p = grids.to_world(
            0,
            FixedVec3::new(Fixed::from_int(1), Fixed::ZERO, Fixed::ZERO),
        );
        (p.y - o.y).to_f64()
    };
    let straight = heading(&b);

    for _ in 0..40 {
        step(&mut b, &input_btn(0, 0, 8)); // hold the turn
    }
    let turned = heading(&b);
    assert!(
        (turned - straight).abs() > 0.05,
        "the hull came round ({straight} → {turned})"
    );

    // Key up: the stabiliser has a couple of seconds to settle it.
    hold(&mut b, 0, 0, 60);
    let settled = heading(&b);
    hold(&mut b, 0, 0, 30);
    assert!(
        (heading(&b) - settled).abs() < 0.02,
        "and the stabiliser stopped the tumble instead of leaving it spinning"
    );
}

/// The hull weighs its own geometry — the shell `build_hull` paints, not the
/// 20×20×6 block that bounds it. This is what makes `engine_force` mean
/// something: tune the drive against a brick and it would be three times too
/// weak the day the map stopped lying.
#[test]
fn the_hull_weighs_the_shell_it_is() {
    let (world, _b) = fresh();
    let mass = {
        let w = world.lock().unwrap();
        w.field(w.entities(SHIP)[0], "body")
    };
    assert!(mass.is_some(), "the ship entity records its body");
    // The block would be 2400 cells at density 1; the shell is well under it
    // and well over nothing.
    let (_world2, b2) = fresh();
    let hull = {
        let phys = b2.physics().expect("the ship map embeds physics");
        let sim = phys.lock().unwrap();
        sim.world
            .body(monada_physics::BodyId(0))
            .expect("the hull body")
            .mass()
            .to_f64()
    };
    assert!(
        hull > 400.0 && hull < 2000.0,
        "a shell, not the block that bounds it (2400): {hull}"
    );
}

// --- the local layer (the half no canary watched) ------------------------
// `local_tick` turns keys into the one command per tick the sim decodes. It
// runs on the unsynced side of the wall, so nothing above tests it — which is
// exactly where a bug hid: `action_axis` answers with an INT (-1, 0, +1) and
// the ship compared it against a `Fixed`, an operator Rhai has no registration
// for. Rhai answers such a comparison with `false` rather than raising, so the
// turn keys did nothing at all and said nothing about it, while every other
// control worked.

/// A bridge that holds one set of held keys and records what the local layer
/// submits. Everything else is the trait's defaults (headless no-ops).
#[derive(Default)]
struct Keys {
    down: Vec<String>,
    axis: Vec<(String, i64)>,
    sent: Vec<FixedVec3>,
}

impl monada_script::HostBridge for Keys {
    // The three that matter: what is held, and what the map submits.
    fn action_down(&self, id: &str) -> bool {
        self.down.iter().any(|d| d == id)
    }
    fn action_axis(&self, id: &str) -> i64 {
        self.axis
            .iter()
            .find(|(a, _)| a == id)
            .map_or(0, |&(_, v)| v)
    }
    fn submit_command(&mut self, _verb: i64, _target: i64, arg: FixedVec3) {
        self.sent.push(arg);
    }

    // The trait's required rest, headless (`NullBridge`'s answers).
    fn local_player(&self) -> Option<i64> {
        Some(0)
    }
    fn model_box(&mut self, _w: i64, _h: i64, _d: i64, _color: i64) -> i64 {
        -1
    }
    #[allow(clippy::too_many_arguments)]
    fn model_box_sides(
        &mut self,
        _w: i64,
        _h: i64,
        _d: i64,
        _x: i64,
        _neg_x: i64,
        _y: i64,
        _neg_y: i64,
        _z: i64,
        _neg_z: i64,
    ) -> i64 {
        -1
    }
    fn model_kv6(&mut self, _asset_path: &str, _turns: i64) -> i64 {
        -1
    }
    fn entity_set_model(&mut self, _entity: i64, _model: i64) {}
    fn voxel_fill(&mut self, _x0: i64, _y0: i64, _z0: i64, _x1: i64, _y1: i64, _z1: i64, _c: i64) {}
    fn voxel_set(&mut self, _x: i64, _y: i64, _z: i64, _color: i64) {}
    fn highlight(&mut self, _entity: i64) {}
    fn highlight_clear(&mut self) {}
    fn highlighted(&self) -> i64 {
        -1
    }
    fn status(&mut self, _text: &str) {}
    fn camera_focus(&mut self, _point: FixedVec3) {}
    fn camera_angle(&mut self, _yaw: Fixed, _pitch: Fixed) {}
    fn set_light(&mut self, _dir: FixedVec3, _intensity: Fixed) {}
    fn set_sky(&mut self, _asset_path: &str) {}
}

/// Run one `local_tick` with the given keys held, and return the button mask
/// it packed into the command's spare z.
fn local_mask(down: &[&str], axis: &[(&str, i64)]) -> i64 {
    let keys = Arc::new(Mutex::new(Keys {
        down: down.iter().map(|s| (*s).to_string()).collect(),
        axis: axis.iter().map(|&(a, v)| (a.to_string(), v)).collect(),
        sent: Vec::new(),
    }));
    let bridge: SharedBridge = keys.clone();
    let mut local = monada_script::LocalBackend::new(&shared_world(SEED), &bridge);
    local.load(&script()).expect("compile the local layer");
    local.on_local_init().expect("local_init");
    local
        .on_local_tick(Fixed::from_ratio(1, 30))
        .expect("local_tick");
    let sent = &keys.lock().unwrap().sent;
    assert_eq!(sent.len(), 1, "local_tick submits exactly one command");
    sent[0].z.to_f64() as i64
}

#[test]
fn every_control_reaches_the_command() {
    assert_eq!(local_mask(&[], &[]), 0, "hands off, nothing set");
    assert_eq!(local_mask(&["use"], &[]), 1, "F: use");
    assert_eq!(local_mask(&["door"], &[]), 2, "G: airlock");
    assert_eq!(local_mask(&["burn"], &[]), 4, "SPACE: main drive");
    assert_eq!(local_mask(&[], &[("turn", 1)]), 8, "Q: turn one way");
    assert_eq!(local_mask(&[], &[("turn", -1)]), 16, "E: and the other");
    assert_eq!(
        local_mask(&["burn"], &[("turn", 1)]),
        12,
        "burning while turning is one command, not a choice"
    );
    assert_eq!(
        local_mask(&["rotate_x"], &[]),
        32,
        "1: rotate a carried crate"
    );
    assert_eq!(local_mask(&["roll_cw"], &[]), 64, "2: roll it CW");
}
