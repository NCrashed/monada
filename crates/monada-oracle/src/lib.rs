//! monada determinism harness (DESIGN.md §3.1, §7).
//!
//! Runs fixed scenarios and records their [`World::state_hash`] at a set
//! of tick checkpoints; CI diffs the result against the committed
//! goldens in `monada-hashes.txt` on every supported platform. A direct
//! lift of `roxlap-oracle`'s hash-and-diff style.
//!
//! Three scenarios gate, by design (decision B + M3):
//! - **`walk`** — the scripted "100 entities walk in a circle"
//!   (`monada-script`'s `WALK_CIRCLE_SCRIPT`). The headline M2 gate; it
//!   exercises the whole Rhai path (compile, host API, fixed-point trig,
//!   seeded RNG) end to end.
//! - **`kernel`** — a tiny pure-Rust scenario on the generic [`World`],
//!   with no scripting at all. A Rhai-independent anchor: it isolates a
//!   sim-kernel regression from a script-layer (e.g. Rhai-version) one.
//! - **`lockstep`** — two `monada-net` sessions, joined by a loopback
//!   transport, run the scripted `command_demo` map from an identical
//!   command stream (M3). It gates the lockstep path: command bundling,
//!   command-delay scheduling, the tick barrier, and the command-driven
//!   sim. The two sessions must also agree at every checkpoint (a built-
//!   in equality assertion), and the recorded replay must reproduce the
//!   final hash.

use std::fmt::Write as _;

use monada_fixed::{Fixed, FixedVec3};
use monada_net::{LockstepSession, LoopbackTransport, MatchInfo, SessionConfig, SimDriver};
use std::sync::{Arc, Mutex};

use monada_format::{pack_dir, Map, SimHz};
use monada_script::{
    run_script, shared_physics, shared_world, LocalBackend, NullBridge, RhaiBackend, RhaiDriver,
    ScriptBackend, SharedBridge, SharedWorld, COMMAND_DEMO_SCRIPT,
    WALK_CIRCLE_SCRIPT,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId, World};
use std::path::Path;

/// Tick counts at which each scenario is hashed. Ascending; `0` captures
/// the seeded post-`init` state before any step.
pub const TICK_CHECKPOINTS: &[u64] = &[0, 1, 30, 150, 600];

/// Shared seed for both scenarios (`MONADA_0`).
const SEED: u64 = 0x4D4F_4E41_4441_5F30;

/// One `(scenario, tick, hash)` checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub scenario: &'static str,
    pub tick: u64,
    pub hash: u64,
}

impl Checkpoint {
    /// The line key used in `monada-hashes.txt` (e.g. `walk@600`).
    #[must_use]
    pub fn key(&self) -> String {
        format!("{}@{}", self.scenario, self.tick)
    }
}

/// The scripted walk-in-circle scenario, hashed at each checkpoint.
///
/// # Panics
/// Panics if the embedded script fails to compile or run (a bug, not a
/// data condition — the script is a fixed asset).
#[must_use]
pub fn walk_checkpoints() -> Vec<Checkpoint> {
    let world = shared_world(SEED);
    let mut backend = RhaiBackend::new(world.clone());
    backend
        .load(WALK_CIRCLE_SCRIPT)
        .expect("compile walk_circle");
    backend.on_init().expect("script init");

    let mut prev = 0;
    let mut out = Vec::with_capacity(TICK_CHECKPOINTS.len());
    for &tick in TICK_CHECKPOINTS {
        for _ in prev..tick {
            backend.on_tick().expect("script tick");
        }
        prev = tick;
        out.push(Checkpoint {
            scenario: "walk",
            tick,
            hash: world.lock().expect("world mutex").state_hash(),
        });
    }
    out
}

/// A pure-Rust scenario on the generic world: 100 entities, each tick
/// shifts every entity's x by its stored `v`. No scripting — the
/// Rhai-independent determinism anchor.
#[must_use]
pub fn kernel_checkpoints() -> Vec<Checkpoint> {
    let mut world = World::new(SEED);
    let arch = world.register_archetype(&["v"]);
    for _ in 0..100 {
        let e = world.spawn(arch);
        let v = world.rng.next_fixed_01();
        world.set_field(e, "v", v);
        world.set_position(e, FixedVec3::new(v, Fixed::ZERO, Fixed::ZERO));
    }

    let mut prev = 0;
    let mut out = Vec::with_capacity(TICK_CHECKPOINTS.len());
    for &tick in TICK_CHECKPOINTS {
        for _ in prev..tick {
            kernel_step(&mut world, arch);
        }
        prev = tick;
        out.push(Checkpoint {
            scenario: "kernel",
            tick,
            hash: world.state_hash(),
        });
    }
    out
}

/// One deterministic tick of the kernel scenario.
fn kernel_step(world: &mut World, _arch: ArchetypeId) {
    world.tick += 1;
    for e in world.all_entities() {
        let v = world.field(e, "v").unwrap_or(Fixed::ZERO);
        let p = world.position(e).unwrap_or(FixedVec3::ZERO);
        world.set_position(e, FixedVec3::new(p.x + v, p.y, p.z));
    }
}

/// The two lockstep players.
const P0: PlayerId = PlayerId(0);
const P1: PlayerId = PlayerId(1);

/// Build one loopback-connected `command_demo` session for `player`.
fn demo_session(
    player: PlayerId,
    transport: LoopbackTransport,
) -> LockstepSession<LoopbackTransport, RhaiDriver> {
    let driver = RhaiDriver::new(shared_world(SEED), COMMAND_DEMO_SCRIPT).expect("compile demo");
    let info = MatchInfo {
        seed: SEED,
        map_hash: monada_format::hash(COMMAND_DEMO_SCRIPT.as_bytes()),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    LockstepSession::new(
        driver,
        transport,
        player,
        &[P0, P1],
        SessionConfig::default(),
        info,
    )
}

/// Player 0's command for the step (tick) at which it is issued; player 1
/// issues nothing. Fully deterministic — fixed verbs, fixed integer
/// vectors, fixed target ids — so the absolute hash is a stable golden.
/// Spawns three units (which become `EntityId` 0/1/2), then steers them
/// and spawns a fourth (`EntityId(3)`); a command issued at step `s`
/// executes at `s + command_delay`, so every steered target exists by the
/// time its steer runs.
fn demo_command(step: u64) -> Vec<Command> {
    let v = |x: i32, y: i32| FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO);
    match step {
        2 => vec![Command::at(1, v(4, 0))],
        3 => vec![Command::at(1, v(-3, 5))],
        4 => vec![Command::at(1, v(7, -2))],
        10 => vec![Command::on(2, EntityId(0), v(1, 1))],
        12 => vec![Command::on(2, EntityId(1), v(-1, 0))],
        20 => vec![Command::on(2, EntityId(2), v(0, 2))],
        50 => vec![Command::at(1, v(0, 9))],
        100 => vec![Command::on(2, EntityId(3), v(2, -1))],
        _ => vec![],
    }
}

/// The lockstep scenario: two loopback sessions run `command_demo` from
/// the same command stream, hashed at each tick checkpoint. Asserts the
/// two sessions agree at every checkpoint and that the replay reproduces
/// the final hash — equality is platform-independent, while the absolute
/// hashes gate cross-platform via the goldens.
///
/// # Panics
/// Panics on a script compile/run failure, a session desync, a
/// session/replay hash disagreement (all bugs, not data conditions).
#[must_use]
pub fn lockstep_checkpoints() -> Vec<Checkpoint> {
    let (ta, tb) = LoopbackTransport::pair();
    let mut a = demo_session(P0, ta);
    let mut b = demo_session(P1, tb);

    let mut out = Vec::with_capacity(TICK_CHECKPOINTS.len());
    for &tick in TICK_CHECKPOINTS {
        // Advance both sessions in lockstep until tick `tick` has executed
        // (session tick counter == `tick`). Tick 0 = post-init.
        while a.tick() < tick {
            let step = a.tick();
            assert!(a.step(demo_command(step)).expect("session a"), "a stalled");
            assert!(b.step(Vec::new()).expect("session b"), "b stalled");
        }
        let ha = a.driver().state_hash();
        let hb = b.driver().state_hash();
        assert_eq!(ha, hb, "lockstep sessions diverged at tick {tick}");
        out.push(Checkpoint {
            scenario: "lockstep",
            tick,
            hash: ha,
        });
    }

    // The replay of A reproduces A's final state bit-exactly — through the
    // *verified* path, which also checks the replay's map hash + engine
    // version (DESIGN.md §3.4) against this build.
    let mut fresh = RhaiDriver::new(shared_world(SEED), COMMAND_DEMO_SCRIPT).expect("compile demo");
    let replayed = a
        .replay()
        .playback_verified(
            &mut fresh,
            monada_format::hash(COMMAND_DEMO_SCRIPT.as_bytes()),
            env!("CARGO_PKG_VERSION"),
        )
        .expect("replay identity matches this build");
    assert_eq!(
        replayed,
        a.driver().state_hash(),
        "replay did not reproduce the lockstep final hash"
    );

    out
}

/// The M4 chess demo map, embedded for the golden (read straight from the
/// map's script file — the oracle never bundles the archive).
const CHESS_SCRIPT: &str = include_str!("../../monada-chess/map/scripts/main.rhai");
/// chess piece archetype (declared first by the map).
const CHESS_PIECE: ArchetypeId = ArchetypeId(0);
/// Hash the chess game after this many moves (`chess@0` = post-init).
const CHESS_CHECKPOINTS: &[usize] = &[0, 8, 16];
/// A fixed, subset-legal 16-move opening, ending with two knight captures
/// on e5 — exercises movement, sliding paths, and capture/despawn. Targets
/// are looked up by square each move (the board is deterministic), so the
/// sequence is stable across platforms.
const CHESS_GAME: &[(i32, i32, i32, i32)] = &[
    (4, 1, 4, 3), // 1. e4
    (4, 6, 4, 4), // 1... e5
    (6, 0, 5, 2), // 2. Nf3
    (1, 7, 2, 5), // 2... Nc6
    (5, 0, 2, 3), // 3. Bc4
    (5, 7, 2, 4), // 3... Bc5
    (3, 1, 3, 2), // 4. d3
    (3, 6, 3, 5), // 4... d6
    (1, 0, 2, 2), // 5. Nc3
    (6, 7, 5, 5), // 5... Nf6
    (7, 1, 7, 2), // 6. h3
    (7, 6, 7, 5), // 6... h6
    (0, 1, 0, 2), // 7. a3
    (0, 6, 0, 5), // 7... a6
    (5, 2, 4, 4), // 8. Nxe5  (knight takes the e-pawn)
    (2, 5, 4, 4), // 8... Nxe5 (recapture)
];

fn chess_sq(x: i32, y: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO)
}

fn chess_piece_at(world: &SharedWorld, x: i32, y: i32) -> EntityId {
    let w = world.lock().expect("world mutex");
    *w.entities(CHESS_PIECE)
        .iter()
        .find(|&&e| w.position(e) == Some(chess_sq(x, y)))
        .expect("a piece on the source square")
}

/// The M4 chess golden: a fixed game played through the *map's own script*
/// (the rules live in `monada-chess`, not here) under a headless
/// [`NullBridge`], hashed after a few move counts. Gates cross-platform
/// determinism of the chess ruleset (DESIGN.md §6 oracle row).
///
/// # Panics
/// Panics on a script compile/run failure (a bug, not a data condition).
#[must_use]
pub fn chess_checkpoints() -> Vec<Checkpoint> {
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let mut driver =
        RhaiDriver::with_bridge(shared_world(SEED), CHESS_SCRIPT, &bridge).expect("compile chess");

    let mut out = Vec::new();
    let mut record = |driver: &RhaiDriver, n: usize| {
        if CHESS_CHECKPOINTS.contains(&n) {
            out.push(Checkpoint {
                scenario: "chess",
                tick: n as u64,
                hash: driver.state_hash(),
            });
        }
    };

    record(&driver, 0);
    for (i, &(fx, fy, tx, ty)) in CHESS_GAME.iter().enumerate() {
        let e = chess_piece_at(driver.world(), fx, fy);
        // Colour-enforced: the player id is irrelevant for the golden.
        driver.apply_command(P0, &Command::on(1, e, chess_sq(tx, ty)));
        record(&driver, i + 1);
    }
    out
}

/// The action-RPG demo map, embedded for the golden (read straight from the
/// map's script file). Runs headless under a [`NullBridge`], so its
/// `voxel_fill` terrain answers collision while the actor / sky calls no-op.
const RPG_SCRIPT: &str = include_str!("../../monada-rpg/map/scripts/main.rhai");
/// Hash the demo run at these tick counts (`rpg@0` = post-init, with
/// wave 1 deployed and no hero yet).
const RPG_CHECKPOINTS: &[usize] = &[0, 1, 30, 150, 600];

/// A fixed, deterministic per-tick real-time input for the golden: orbit the
/// move axis with periodic attack / dodge (verb 0 = input; `target` = button
/// bitmask; `arg.xy` = move axis).
fn rpg_input(t: usize) -> Command {
    let (mx, my) = match t % 4 {
        0 => (1, 0),
        1 => (0, 1),
        2 => (-1, 0),
        _ => (0, -1),
    };
    let mut btn = 0u64;
    if t % 9 == 0 {
        btn |= 1; // attack
    }
    if t % 23 == 0 {
        btn |= 2; // dodge
    }
    Command::on(
        0,
        EntityId(btn),
        FixedVec3::new(Fixed::from_int(mx), Fixed::from_int(my), Fixed::ZERO),
    )
}

/// The action-RPG golden: the real-time demo map driven through its own
/// script under a headless [`NullBridge`], one input command per tick,
/// hashed at fixed tick counts. Gates cross-platform determinism of the
/// real-time tick + per-tick input + voxel-query + wave-RNG path.
///
/// # Panics
/// Panics on a script compile/run failure (a bug, not a data condition).
#[must_use]
pub fn rpg_checkpoints() -> Vec<Checkpoint> {
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let mut driver =
        RhaiDriver::with_bridge(shared_world(SEED), RPG_SCRIPT, &bridge).expect("compile rpg");

    let mut out = Vec::new();
    let mut record = |driver: &RhaiDriver, n: usize| {
        if RPG_CHECKPOINTS.contains(&n) {
            out.push(Checkpoint {
                scenario: "rpg",
                tick: n as u64,
                hash: driver.state_hash(),
            });
        }
    };

    record(&driver, 0);
    for t in 1..=600usize {
        driver.apply_command(P0, &rpg_input(t));
        driver.step();
        record(&driver, t);
    }
    out
}

/// The spaceship crew-sim demo map, embedded for the golden (read straight
/// from the map's script file). Like the RPG it runs headless under a
/// [`NullBridge`]; its render-side calls (camera, deck clip, fog of war,
/// HUD, actor anim) no-op, leaving only the hashed sim: crew movement, the
/// deck-relative collision, and the stairwell deck-flip.
const SHIP_SCRIPT: &str = include_str!("../../monada-ship/map/scripts/main.rhai");
/// Hash the ship run at these tick counts (`ship@0` = post-init, hull painted
/// but no crew yet — the crew spawns on the first input).
const SHIP_CHECKPOINTS: &[usize] = &[0, 1, 30, 150, 600];
/// The ship's fixed tick rate, from its manifest. The physics `dt` is folded
/// into the digest, so a rate that disagreed with the shipped map would gate a
/// run nobody plays.
const SHIP_HZ: u32 = 30;

/// A fixed, deterministic per-tick input for the ship golden (verb 0,
/// `arg.xy` = the move axis, values in `-1..=1` like the real `action_axis2`;
/// `target` is ignored by the map). Movement is view-relative through
/// `cam_relative(yaw = 0.8)`, so `(1, -1)` resolves to ~due-east: it walks the
/// crew from its fore-port spawn onto the fore-starboard stairwell and climbs
/// it to the upper deck (exercising the deck-flip + stair seating), then orbits
/// to slide along the upper divider.
fn ship_input(t: usize) -> Command {
    let (mx, my) = if t <= 200 {
        (1, -1) // ~east: onto the stairwell, climb to the upper deck
    } else {
        match t % 4 {
            0 => (1, 0),
            1 => (0, 1),
            2 => (-1, 0),
            _ => (0, -1),
        }
    };
    // The button mask the map reads from `arg.z` (1 = use, 2 = airlock,
    // 32 = rotate), pressed on single ticks so the map's rising-edge rule fires
    // exactly once. This is what puts grid MEMBERSHIP and PLACEMENT under the
    // golden: tick 1 takes the crate the cursor is on (see `ship_aim`) and
    // attaches it to the crew's hull, tick 30 turns it a quarter, tick 32 sets
    // it down on the cell the cursor names, and tick 240 cycles the airlock
    // (which changes what the crew can walk through).
    let mut btn = match t {
        1 | 32 => 1,
        30 => 32,
        240 => 2,
        _ => 0,
    };
    // …and the flight controls (docs/plans/ship-physics.md S-5), which are
    // HELD rather than tapped: a burn amidships, then a turn each way. This is
    // what puts the hull's own motion under the golden — its pose is no longer
    // a hashed angle the map advances but an outcome of the physics, so the
    // only way to gate it is to fly the ship and hash where it ends up.
    if (100..=160).contains(&t) {
        btn |= 4; // main drive
    }
    if (400..=430).contains(&t) {
        btn |= 8; // turn one way
    }
    if (500..=520).contains(&t) {
        btn |= 16; // …and back
    }
    Command::on(
        0,
        EntityId(0),
        FixedVec3::new(
            Fixed::from_int(mx),
            Fixed::from_int(my),
            Fixed::from_int(btn),
        ),
    )
}

/// The cursor, on the ticks the schedule above uses one (verb 1, the hull
/// CELL a client's pointer rests on — docs/plans/ship-building.md).
///
/// A cursor is per-client and never hashed; what crosses the wall is the cell
/// it named, exactly as an ordinary command. So the golden sends cells, and
/// what it gates is the half that IS shared: that every peer agrees a crate
/// was taken from cell (4, 3) and put down on (10, 3), turned a quarter.
///
/// The cells are the demo's own: `init` stows a crate at (4, 3) beside the
/// crew's spawn, and (10, 3) is clear deck a third of the way along the walk
/// east. `arg.z` is the deck plate the pointer is on — cell 0 is the lower
/// deck's floor, which is where this whole run happens.
fn ship_aim(t: usize) -> Option<Command> {
    let cell = match t {
        1 => (4, 3),
        30..=32 => (10, 3),
        _ => return None,
    };
    Some(Command::on(
        1,
        EntityId(0),
        FixedVec3::new(
            Fixed::from_int(cell.0),
            Fixed::from_int(cell.1),
            Fixed::ZERO,
        ),
    ))
}

/// The ship golden: the crew-sim demo driven through its own script under a
/// headless [`NullBridge`], one input command per tick, hashed at fixed
/// tick counts. Gates cross-platform determinism of the two-deck movement,
/// deck-relative collision, the stairwell deck-flip, and cursor-directed
/// cargo placement — the ship's sim half (its visibility/camera work, and the
/// ghost that previews a placement, are render-side and unhashed by design).
///
/// # Panics
/// Panics on a script compile/run failure (a bug, not a data condition).
#[must_use]
pub fn ship_checkpoints() -> Vec<Checkpoint> {
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    // The ship declares `terrain = "volume"` for the physics it is about to
    // grow (docs/plans/ship-physics.md D1), so the golden runs the map the way
    // the host does: a `PhysicsWorld` stepped after each tick and folded into
    // the combined digest. It carries no bodies yet — this slice is the seam,
    // not the dynamics — so what moved in `ship@` is the digest's SHAPE.
    let phys = shared_physics(SHIP_HZ);
    let mut driver = RhaiDriver::with_physics(shared_world(SEED), SHIP_SCRIPT, &bridge, &phys)
        .expect("compile ship");

    let mut out = Vec::new();
    let mut record = |driver: &RhaiDriver, n: usize| {
        if SHIP_CHECKPOINTS.contains(&n) {
            out.push(Checkpoint {
                scenario: "ship",
                tick: n as u64,
                hash: driver.state_hash(),
            });
        }
    };

    record(&driver, 0);
    for t in 1..=600usize {
        driver.apply_command(P0, &ship_input(t));
        // A client with a pointer sends two commands a tick, not one.
        if let Some(aim) = ship_aim(t) {
            driver.apply_command(P0, &aim);
        }
        driver.step();
        record(&driver, t);
    }
    out
}

/// The RTS demo map, embedded for the golden (read straight from the
/// map's script file). Runs headless under a [`NullBridge`]: the
/// heightfield paints fill the shared collision store `nav_path` also
/// reads, while render/selection calls no-op — leaving the hashed sim:
/// nav-routed movement over cliffs/ramps, the worker harvest economy,
/// training, combat, and a tree felled by `voxel_clear`.
const RTS_SCRIPT: &str = include_str!("../../monada-rts/map/scripts/main.rhai");
/// Hash the RTS run at these tick counts (`rts@0` = post-init: terrain
/// painted, bases + starting squads spawned).
const RTS_CHECKPOINTS: &[usize] = &[0, 1, 30, 150, 600];

/// An RTS order (event-driven — unlike the RPG/ship goldens most ticks
/// carry no command at all): `verb` per the map's contract (1 MOVE,
/// 2 HARVEST, 3 TRAIN worker, 4 TRAIN soldier, 5 ATTACK), the acting
/// unit in `target`, a point in `arg.xy`, an entity id in `arg.z`.
// `-1 as u64` round-trips back to the script's `-1` sentinel by design.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rts_cmd(verb: u32, unit: i64, x: i64, y: i64, z: i64) -> Command {
    Command::on(
        verb,
        EntityId(unit as u64),
        FixedVec3::new(
            Fixed::from_int(x as i32),
            Fixed::from_int(y as i32),
            Fixed::from_int(z as i32),
        ),
    )
}

/// The RTS golden: a fixed 1v1 order schedule driven through the map's
/// own script — both sides put a worker on the gold loop, P0 group-moves
/// two more (a two-command burst in one tick), P1 trains a worker, P0
/// trains a soldier and sends it to fell a lowland tree (`voxel_clear`
/// reshapes collision + nav mid-run). Gates cross-platform determinism
/// of the whole strategic sim half; selection/camera/HUD are render-side
/// and unhashed by design.
///
/// # Panics
/// Panics on a script compile/run failure (a bug, not a data condition).
#[must_use]
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
pub fn rts_checkpoints() -> Vec<Checkpoint> {
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let mut driver =
        RhaiDriver::with_bridge(shared_world(SEED), RTS_SCRIPT, &bridge).expect("compile rts");

    // Init is deterministic, so ids and positions read back identically on
    // every platform; deriving them here keeps the schedule robust to map
    // spawn-order tweaks (no hardcoded entity ids).
    let world = driver.world().clone();
    let (units, mines, tree) = {
        let w = world.lock().expect("world mutex");
        let units: Vec<i64> = w
            .entities(ArchetypeId(0))
            .iter()
            .map(|e| e.0 as i64)
            .collect();
        let mines: Vec<(i64, i64, i64)> = w
            .entities(ArchetypeId(3))
            .iter()
            .map(|&m| {
                let p = w.position(m).expect("mine pos");
                (m.0 as i64, p.x.to_f64() as i64, p.y.to_f64() as i64)
            })
            .collect();
        // The lowland tree at cell (24, 22) — the soldier's chopping target.
        let tree = w
            .entities(ArchetypeId(1))
            .iter()
            .find(|&&t| {
                let p = w.position(t).expect("tree pos");
                (p.x.to_f64() - 24.0).abs() < 0.1 && (p.y.to_f64() - 22.0).abs() < 0.1
            })
            .map(|t| t.0 as i64)
            .expect("the (24, 22) tree stands");
        (units, mines, tree)
    };

    let mut out = Vec::new();
    let mut record = |driver: &RhaiDriver, n: usize| {
        if RTS_CHECKPOINTS.contains(&n) {
            out.push(Checkpoint {
                scenario: "rts",
                tick: n as u64,
                hash: driver.state_hash(),
            });
        }
    };

    record(&driver, 0);
    let mut soldier: Option<i64> = None;
    for t in 1..=600usize {
        match t {
            // Both sides put their first worker on the gold loop.
            2 => {
                let (m0, x0, y0) = mines[0];
                let (m1, x1, y1) = mines[1];
                driver.apply_command(P0, &rts_cmd(2, units[0], x0, y0, m0));
                driver.apply_command(P1, &rts_cmd(2, units[3], x1, y1, m1));
            }
            // A two-command group burst in one tick (the box-select shape).
            10 => {
                driver.apply_command(P0, &rts_cmd(1, units[1], 22, 24, 0));
                driver.apply_command(P0, &rts_cmd(1, units[2], 24, 24, 0));
            }
            12 => driver.apply_command(P1, &rts_cmd(3, -1, 0, 0, 0)),
            15 => driver.apply_command(P0, &rts_cmd(4, -1, 0, 0, 0)),
            // The trained soldier (delivered ~tick 105) fells a tree:
            // voxel_clear reshapes collision + nav inside the hashed run.
            150 => {
                let w = world.lock().expect("world mutex");
                let s = w
                    .entities(ArchetypeId(0))
                    .iter()
                    .find(|&&e| {
                        w.field(e, "owner").expect("owner").to_f64() as i64 == 0
                            && w.field(e, "kind").expect("kind").to_f64() as i64 == 1
                    })
                    .map(|e| e.0 as i64)
                    .expect("P0's soldier was delivered");
                drop(w);
                soldier = Some(s);
                driver.apply_command(P0, &rts_cmd(5, s, 24, 22, tree));
            }
            // Post-felling: walk the soldier onto the opened stump cell.
            520 => {
                if let Some(s) = soldier {
                    driver.apply_command(P0, &rts_cmd(1, s, 24, 22, 0));
                }
            }
            _ => {}
        }
        driver.step();
        record(&driver, t);
    }
    out
}

/// The physics-crate golden: a [`PhysicsWorld`] at the engine-default
/// 25 Hz under gravity, over bumpy voxel terrain — a flat floor with a
/// deterministic 0–2 voxel bump field for x > 40 and a staircase for
/// x < −40. Bodies: the two free P1 ghosts — a spinning ballistic one
/// carrying a *rotated* (non-diagonal) inertia tensor so the
/// `FixedMat3` hash fold is warmed by real data, and a drifting one
/// (ghosts have no skin, so they sail through the floor by design) —
/// two P2 voxel bodies exercising the contact stack (a 3³ cube dropped
/// from z = 30, at rest well before tick 600 with a live warm-start
/// cache; a 4×4×2 slab shoved sideways, sliding to a frictional stop)
/// — a P3 four-wheel vehicle driving a scripted schedule (wind-up
/// straight, a steered arc over the bump field, brake to a stop) —
/// and a P4 destruction beat at tick 300: the resting cube is cut in
/// half (a split into two live bodies) and the slab loses its corner
/// column to a debris cluster, mid-slide.
/// Like `kernel@` it is pure Rust with no scripting: the anchor that
/// gates `monada-physics`'s state layout, canonical hash, and solver +
/// wheel + destruction arithmetic cross-platform
/// (docs/plans/voxel-physics.md §5). Later milestones grow this
/// scenario, each growth re-blessed explicitly.
///
/// [`PhysicsWorld`]: monada_physics::PhysicsWorld
///
/// # Panics
/// Panics if the scenario's fixed spawns fail (a bug, not a data
/// condition — every input here is a compile-time constant).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn phys_checkpoints() -> Vec<Checkpoint> {
    use monada_fixed::{FixedMat3, FixedQuat};
    use monada_physics::{
        BodyDef, Material, MaterialId, PhysicsWorld, VoxelBodyDef, VoxelField, VoxelShape,
        WheelDef, WheelInput,
    };

    /// Flat floor; bumps (height 0–2, deterministic in the cell
    /// coords) beyond x > 40; stairs descending for x < −40.
    // P6: the terrain is no longer a pure function of coordinates —
    // the drill beat carves cells at ticks 400..=429, so the field
    // reads a mutable carved-set held beside it (deterministic and
    // cheap: BTreeSet lookups, mutated only between steps).
    struct Terrain<'a> {
        carved: &'a std::collections::BTreeSet<(i64, i64, i64)>,
    }
    impl VoxelField for Terrain<'_> {
        fn occupied(&self, x: i64, y: i64, z: i64) -> bool {
            if self.carved.contains(&(x, y, z)) {
                return false;
            }
            if x > 40 {
                let bump = (x.div_euclid(3).wrapping_mul(7) + y.div_euclid(3).wrapping_mul(5))
                    .rem_euclid(3);
                return z < bump;
            }
            if x < -40 {
                return z < ((-40 - x).div_euclid(2)).clamp(0, 20);
            }
            z < 0
        }
        fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
            MaterialId(0)
        }
    }

    let fx = Fixed::from_int;
    let v3 = |x: i32, y: i32, z: i32| FixedVec3::new(fx(x), fx(y), fx(z));

    let mut world = PhysicsWorld::new(25);
    world.set_gravity(v3(0, 0, -10));
    let mat = world.register_material(Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(50),
    });

    // A box-ish inertia diag(2, 3, 4) rotated off-axis: R · D · Rᵀ.
    let r = FixedMat3::from_quat(FixedQuat::from_axis_angle(
        v3(1, 1, 0),
        Fixed::from_ratio(1, 2),
    ));
    let rotated_inertia = r * FixedMat3::from_diagonal(v3(2, 3, 4)) * r.transpose();
    world.spawn(&BodyDef {
        position: v3(0, 0, 100),
        linear_velocity: v3(5, 3, 20),
        angular_velocity: v3(1, 2, -1),
        mass: fx(3),
        inertia_body: rotated_inertia,
        orientation: FixedQuat::IDENTITY,
    });
    world.spawn(&BodyDef {
        position: v3(-40, 25, 200),
        linear_velocity: v3(-1, 1, 0),
        ..BodyDef::default()
    });

    // P2: a dropped cube (rests by ~tick 80)…
    let mut cube = VoxelShape::new(3, 3, 3);
    cube.fill_box((0, 0, 0), (2, 2, 2), mat);
    let cube_body = world.spawn_voxels(&VoxelBodyDef {
        shape: cube,
        position: v3(20, 0, 30),
        orientation: FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    // …and a shoved slab sliding to a frictional stop.
    let mut slab = VoxelShape::new(4, 4, 2);
    slab.fill_box((0, 0, 0), (3, 3, 1), mat);
    let slab_body = world.spawn_voxels(&VoxelBodyDef {
        shape: slab,
        position: v3(-20, 0, 1),
        orientation: FixedQuat::IDENTITY,
        linear_velocity: v3(12, 0, 0),
        angular_velocity: FixedVec3::ZERO,
    });

    // P3: a four-wheel vehicle (the test-suite stance: wheelbase ±3.5,
    // track ±2.5, k = 240, c = 80).
    let mut chassis = VoxelShape::new(6, 4, 2);
    chassis.fill_box((0, 0, 0), (5, 3, 1), mat);
    let vehicle = world.spawn_voxels(&VoxelBodyDef {
        shape: chassis,
        position: v3(0, 15, 3),
        orientation: FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let com = world.body(vehicle).expect("vehicle").com_in_shape();
    let corners = [
        (Fixed::from_ratio(13, 2), Fixed::from_ratio(9, 2)),
        (Fixed::from_ratio(13, 2), Fixed::from_ratio(-1, 2)),
        (Fixed::from_ratio(-1, 2), Fixed::from_ratio(9, 2)),
        (Fixed::from_ratio(-1, 2), Fixed::from_ratio(-1, 2)),
    ];
    let wheel_ids = corners.map(|(sx, sy)| {
        world.attach_wheel(
            vehicle,
            &WheelDef {
                anchor: FixedVec3::new(sx, sy, Fixed::ZERO) - com,
                rest_length: Fixed::from_ratio(3, 2),
                radius: Fixed::HALF,
                stiffness: fx(240),
                damping: fx(80),
                friction: Fixed::from_ratio(4, 5),
            },
        )
    });
    // Scripted schedule: settle → wind up straight → steered arc onto
    // the bump field → brake to a stop.
    let input = |steer_front: bool, steer: Fixed, drive: Fixed, brake: Fixed| {
        move |world: &mut PhysicsWorld, ids: &[monada_physics::WheelId; 4]| {
            for (i, id) in ids.iter().enumerate() {
                world.set_wheel_input(
                    vehicle,
                    *id,
                    WheelInput {
                        steer: if !steer_front || i < 2 {
                            steer
                        } else {
                            Fixed::ZERO
                        },
                        drive,
                        brake,
                    },
                );
            }
        }
    };

    let mut carved: std::collections::BTreeSet<(i64, i64, i64)> = std::collections::BTreeSet::new();
    let drill = monada_physics::DrillTool {
        anchor: v3(4, 0, -1),
        half_extents: FixedVec3::new(Fixed::ONE, fx(2), Fixed::ONE),
        orientation: FixedQuat::IDENTITY,
    };

    let mut prev = 0;
    let mut out = Vec::with_capacity(TICK_CHECKPOINTS.len());
    for &tick in TICK_CHECKPOINTS {
        for t in prev..tick {
            match t {
                50 => input(false, Fixed::ZERO, fx(25), Fixed::ZERO)(&mut world, &wheel_ids),
                200 => {
                    input(true, Fixed::from_ratio(3, 10), fx(25), Fixed::ZERO)(
                        &mut world, &wheel_ids,
                    );
                }
                // P4: cut the resting cube in half (both halves ≥ the
                // debris threshold → a split) and blow the slab's
                // corner column off (2 voxels → a debris cluster).
                300 => {
                    let plane: Vec<(i32, i32, i32)> = (0..3)
                        .flat_map(|y| (0..3).map(move |z| (1, y, z)))
                        .collect();
                    let _ = world.remove_voxels(cube_body, &plane);
                    let _ = world
                        .remove_voxels(slab_body, &[(1, 0, 0), (1, 0, 1), (0, 1, 0), (0, 1, 1)]);
                }
                // P6: a short tunnel drilled through the bump field
                // while edits stream in — one column of cells per
                // tick, wake/invalidate notify, hardness reaction on
                // the vehicle. The two-material cut list is a
                // STAND-IN for an engine policy (six cells are carved
                // per tick) — the mismatch is deliberate: the golden
                // gates the seam's arithmetic, not a cutting policy.
                400..=429 => {
                    let x = 45 + i64::try_from(t - 400).expect("small");
                    for y in -1..=1i64 {
                        for z in 0..=1i64 {
                            carved.insert((x, y, z));
                        }
                    }
                    world.notify_terrain_edit((x, -1, 0), (x, 1, 1));
                    let _ = world.drill_reaction(vehicle, &drill, &[MaterialId(0), MaterialId(0)]);
                }
                450 => input(false, Fixed::ZERO, Fixed::ZERO, fx(100))(&mut world, &wheel_ids),
                _ => {}
            }
            world.step(&Terrain { carved: &carved });
        }
        prev = tick;
        out.push(Checkpoint {
            scenario: "phys",
            tick,
            hash: world.state_hash(),
        });
    }
    out
}

/// The digger demo map, embedded for the golden (read straight from the
/// map's script file). Runs headless under a [`NullBridge`] with the
/// embedded physics sim — the FULL `terrain = "volume"` driver stack:
/// script `tick` → `PhysicsWorld::step` against the [`VolumeStore`]
/// (docs/plans/digger-demo.md §1b) → the combined entity ⊕ physics ⊕
/// terrain digest.
///
/// [`VolumeStore`]: monada_script::VolumeStore
const DIGGER_SCRIPT: &str = include_str!("../../monada-digger/map/scripts/main.rhai");
/// The digger map's fixed tick rate — a DUPLICATE of the manifest's
/// `sim_hz` (the oracle embeds the script, not the archive). Guarded by
/// `manifest_declares_the_volume_map` in the digger crate, which pins
/// the manifest to 30 Hz.
const DIGGER_HZ: u32 = 30;
/// Hash the demo run at these tick counts (`digger@0` = post-init: apron
/// painted, vehicle spawned, suspension not yet settled; `digger@900` =
/// parked inside the basement vault).
const DIGGER_CHECKPOINTS: &[usize] = &[0, 1, 30, 150, 600, 900];

/// The fixed drive schedule (docs/plans/digger-demo.md §4): settle → a
/// steer S at low speed → straight run over the jump ramp (launch ~t165,
/// land ~t210) → brake at the mountain → bore level through the granite
/// vein into the crystal chamber → pitch down ON THE MOVE → ride the
/// descending bore through the apron slab into the basement vault →
/// brake underground. Verb 0, `arg.x` = drive, `arg.y` = steer (+1 =
/// screen right; the script negates into physics yaw), `arg.z` = pitch
/// nudge, `target` bit 0 = brake, bit 1 = drill — one packed command per
/// tick, the rpg pattern. KEPT IN SYNC with the behaviour test in
/// `crates/monada-digger/tests/gameplay.rs`.
fn digger_input(t: usize) -> Command {
    // Identical arm bodies are distinct BEATS — merging them would
    // scramble the schedule's story.
    #[allow(clippy::match_same_arms)]
    let (drive, steer, pitch, brake, drill) = match t {
        0..=29 => (0, 0, 0, 0, 0),
        30..=35 => (1, 1, 0, 0, 0),
        36..=41 => (1, -1, 0, 0, 0),
        42..=209 => (1, 0, 0, 0, 0),
        // Reverse torque brakes (S is the service brake now); the tail
        // coast settles the nose at the face.
        210..=242 => (-1, 0, 0, 0, 0),
        243..=259 => (0, 0, 0, 0, 0),
        260..=454 => (1, 0, 0, 0, 1),
        455..=469 => (1, 0, -1, 0, 1),
        470..=599 => (1, 0, 0, 0, 1),
        600..=819 => (1, 0, 0, 0, 0),
        // Park on the handbrake.
        _ => (0, 0, 0, 1, 0),
    };
    Command::on(
        0,
        EntityId(brake | (drill << 1)),
        FixedVec3::new(
            Fixed::from_int(drive),
            Fixed::from_int(steer),
            Fixed::from_int(pitch),
        ),
    )
}

/// The digger golden: the volume-terrain demo driven through its own
/// script with physics embedded in the sim, one input command per tick,
/// hashed at fixed tick counts. Gates cross-platform determinism of the
/// physics-in-sim seam: the chunk-hash-cached volume store, the wheel
/// drive-train through the `phys_*` verbs, and the combined digest.
///
/// # Panics
/// Panics on a script compile/run failure (a bug, not a data condition).
#[must_use]
pub fn digger_checkpoints() -> Vec<Checkpoint> {
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let phys = shared_physics(DIGGER_HZ);
    let mut driver = RhaiDriver::with_physics(shared_world(SEED), DIGGER_SCRIPT, &bridge, &phys)
        .expect("compile digger");
    driver.set_tick_hz(DIGGER_HZ);

    let mut out = Vec::new();
    let mut record = |driver: &RhaiDriver, n: usize| {
        if DIGGER_CHECKPOINTS.contains(&n) {
            out.push(Checkpoint {
                scenario: "digger",
                tick: n as u64,
                hash: driver.state_hash(),
            });
        }
    };

    record(&driver, 0);
    for t in 1..=900usize {
        driver.apply_command(P0, &digger_input(t));
        driver.step();
        record(&driver, t);
    }
    out
}

/// Every gated scenario's checkpoints, in a fixed order.
#[must_use]
pub fn all_checkpoints() -> Vec<Checkpoint> {
    let mut out = walk_checkpoints();
    out.extend(kernel_checkpoints());
    out.extend(lockstep_checkpoints());
    out.extend(chess_checkpoints());
    out.extend(rpg_checkpoints());
    out.extend(ship_checkpoints());
    out.extend(rts_checkpoints());
    out.extend(phys_checkpoints());
    out.extend(digger_checkpoints());
    out
}

/// The headline scripted golden: `walk@600`. Exposed for cross-checks.
///
/// # Panics
/// Panics if the embedded script fails to compile or run.
#[must_use]
pub fn walk_final_hash() -> u64 {
    let world = run_script(SEED, WALK_CIRCLE_SCRIPT, 600).expect("run walk_circle");
    let hash = world.lock().expect("world mutex").state_hash();
    hash
}

/// Render checkpoints as the on-disk goldens file. Inverse of
/// [`parse_goldens`].
#[must_use]
pub fn render_goldens(checkpoints: &[Checkpoint]) -> String {
    let mut s = String::new();
    s.push_str("# monada determinism goldens — @generated, do not hand-edit.\n");
    s.push_str(
        "# scenarios: walk (scripted circle), kernel (pure-Rust anchor), \
         lockstep (two-session command demo), chess (turn-based rules), \
         rpg (real-time action-RPG: per-tick input + voxel-query + wave \
         RNG), ship (two-deck crew sim aboard a rigid-body hull: \
         deck-relative collision, stairwell deck-flip, grid membership, \
         cursor-directed cargo placement, and a ship flown under its own \
         engines), rts (1v1 strategy: nav-routed \
         orders + economy + \
         combat + voxel_clear tree felling), phys (pure-Rust physics-crate \
         anchor: PhysicsWorld fixed-timestep shell), digger (volume-terrain \
         demo: physics-in-sim drive-train + chunk-hashed VolumeStore + \
         combined digest); seed \"MONADA_0\".\n",
    );
    s.push_str("# Regenerate with `cargo run -p monada-oracle -- --bless`.\n");
    for c in checkpoints {
        let _ = writeln!(s, "{} = {}", c.key(), c.hash);
    }
    s
}

/// Parse a goldens file into `(key, hash)` pairs, ignoring blank and
/// `#`-comment lines.
///
/// # Errors
/// Returns the offending line on a malformed entry.
pub fn parse_goldens(text: &str) -> Result<Vec<(String, u64)>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed line (no '='): {line:?}"))?;
        let hash = value
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("bad hash in {line:?}: {e}"))?;
        out.push((key.trim().to_string(), hash));
    }
    Ok(out)
}

/// Pack, load, and smoke-run one book example map (`book/examples/*`).
///
/// The book's examples are real, runnable maps rather than inline
/// snippets — this is what keeps them honest: a snippet the harness never
/// executes rots into a lie (docs/plans/mapmakers-book.md §0). It packs
/// the directory into a `.monada` archive, reads it back (which validates
/// the manifest), compiles *both* script layers, and drives `ticks`
/// simulation steps under a headless [`NullBridge`] — the same path
/// the real oracle scenarios use. Returns the final [`World::state_hash`];
/// any pack / load / compile / run failure surfaces as `Err`.
///
/// # Errors
/// A human-readable description of the first failing stage.
pub fn run_example_map(dir: &Path, ticks: u64) -> Result<u64, String> {
    let bytes = pack_dir(dir).map_err(|e| format!("pack {}: {e}", dir.display()))?;
    let map = Map::read(&bytes).map_err(|e| format!("read {}: {e}", dir.display()))?;
    // The same gate the interactive host applies in `config_for_map` —
    // an example must not claim an API the shipped host would refuse.
    monada_script::check_host_api(map.manifest.host_api)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let script = map
        .entry_script()
        .ok_or("manifest `entry` names no packed script")?;
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let mut driver = RhaiDriver::with_bridge(shared_world(SEED), script, &bridge)
        .map_err(|e| format!("sim script: {e:?}"))?;
    if let SimHz::Fixed(hz) = map.manifest.sim_hz {
        driver.set_tick_hz(hz);
    }
    // Compile the local layer too, so a broken local script fails loudly.
    // It is not driven here — the sim layer is what a golden would cover;
    // this only proves both halves load.
    let local_src = map.local_script().ok_or("manifest names no local script")?;
    LocalBackend::new(driver.world(), &bridge)
        .load(local_src)
        .map_err(|e| format!("local script: {e:?}"))?;
    for _ in 0..ticks {
        driver.step();
    }
    Ok(driver.state_hash())
}

/// A single checkpoint's verdict against the goldens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Match,
    Mismatch { golden: u64, got: u64 },
    MissingGolden,
}

/// Diff freshly-computed checkpoints against parsed goldens, in order.
#[must_use]
pub fn diff(checkpoints: &[Checkpoint], goldens: &[(String, u64)]) -> Vec<(Checkpoint, Verdict)> {
    checkpoints
        .iter()
        .map(|c| {
            let verdict = match goldens.iter().find(|(k, _)| *k == c.key()) {
                None => Verdict::MissingGolden,
                Some((_, g)) if *g == c.hash => Verdict::Match,
                Some((_, g)) => Verdict::Mismatch {
                    golden: *g,
                    got: c.hash,
                },
            };
            (*c, verdict)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The ship schedule really does what its comments claim.
    ///
    /// A golden hash gates that a run is *reproducible*, not that it is
    /// interesting: a placement the map silently refuses hashes just as
    /// happily as one it accepts, and the schedule would go on claiming to
    /// cover cargo while covering nothing. So replay the same beats and look
    /// at the crate — it must have been taken from the cell the cursor named,
    /// turned, and put down on the other one.
    // Small integers stored as fixed-point; reading them back through f64 is
    // exact for the cell coordinates and flags here.
    #[allow(clippy::cast_possible_truncation)]
    #[test]
    fn the_ship_schedule_actually_places_the_crate() {
        let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
        let world = shared_world(SEED);
        let phys = shared_physics(SHIP_HZ);
        let mut driver = RhaiDriver::with_physics(world.clone(), SHIP_SCRIPT, &bridge, &phys)
            .expect("compile ship");
        for t in 1..=40usize {
            driver.apply_command(P0, &ship_input(t));
            if let Some(aim) = ship_aim(t) {
                driver.apply_command(P0, &aim);
            }
            driver.step();
        }
        let w = world.lock().expect("world mutex");
        let crew = w.entities(monada_sim::ArchetypeId(0))[0];
        let crate0 = w.entities(monada_sim::ArchetypeId(2))[0];
        let carry = w.field(crew, "carry").expect("crew carries a field");
        assert_eq!(carry.to_f64() as i64, 0, "the crate was set down again");
        let p = w.position(crate0).expect("the crate exists");
        assert_eq!(
            ((p.x.to_f64() + 0.5).floor() as i64, (p.y.to_f64() + 0.5).floor() as i64),
            (10, 3),
            "…on the cell the cursor named, not where the crew stood"
        );
        assert_eq!(
            w.field(crate0, "dir").expect("props carry a facing").to_f64() as i64,
            2,
            "…and kept the quarter turn tick 30 gave it (+y = Direction::Y)"
        );
    }

    /// Every `book/examples/*` map packs, loads, and runs headless. This
    /// is the book's "examples don't rot" gate — it runs under the normal
    /// `cargo test` matrix, so a broken tutorial map fails CI on every
    /// platform.
    #[test]
    fn book_examples_run_headless() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../book/examples");
        let mut ran = 0;
        for entry in std::fs::read_dir(&root).expect("book/examples exists") {
            let dir = entry.expect("dir entry").path();
            if !dir.join("manifest.toml").exists() {
                continue; // not a map directory
            }
            run_example_map(&dir, 60).unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
            ran += 1;
        }
        assert!(ran > 0, "no book examples under {}", root.display());
    }

    /// Every map shipped in this repo — the demo crates' `map/` dirs and
    /// the book's examples — declares a `host_api` the shipped host still
    /// runs. A breaking bump moves `HOST_API_OLDEST`, which silently
    /// strands every manifest left behind; this is what turns that into a
    /// failing test instead of a map that refuses to load at runtime.
    #[test]
    fn shipped_manifests_declare_a_supported_host_api() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut dirs: Vec<PathBuf> = Vec::new();
        for parent in [root.join("crates"), root.join("book/examples")] {
            for entry in std::fs::read_dir(&parent).expect("crate / example dir") {
                let path = entry.expect("dir entry").path();
                // A demo crate keeps its map under `map/`; a book example
                // IS the map directory.
                for candidate in [path.join("map"), path] {
                    if candidate.join("manifest.toml").exists() {
                        dirs.push(candidate);
                    }
                }
            }
        }
        assert!(dirs.len() >= 6, "found only {} shipped maps", dirs.len());
        for dir in dirs {
            let bytes = pack_dir(&dir).unwrap_or_else(|e| panic!("pack {}: {e}", dir.display()));
            let map = Map::read(&bytes).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
            monada_script::check_host_api(map.manifest.host_api)
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()));
        }
    }

    /// A script-facing name: lowercase / digits / underscore, leading
    /// alpha or `_`. Excludes the operator overloads (`+`, `<`, …) that
    /// are also registered via `register_fn` but aren't documented API.
    fn is_api_ident(s: &str) -> bool {
        let mut chars = s.chars();
        matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_lowercase())
            && s.chars()
                .all(|c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
    }

    /// The identifier-named functions monada-script registers, scraped
    /// from `register_fn("name"` in the given source (whitespace/newlines
    /// between `(` and the literal are tolerated — rustfmt wraps them).
    fn registered_fn_names(src: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let marker = "register_fn(";
        let mut rest = src;
        while let Some(i) = rest.find(marker) {
            let after = rest[i + marker.len()..].trim_start();
            if let Some(quoted) = after.strip_prefix('"') {
                if let Some(end) = quoted.find('"') {
                    let name = &quoted[..end];
                    if is_api_ident(name) {
                        out.insert(name.to_owned());
                    }
                }
            }
            rest = &rest[i + marker.len()..];
        }
        out
    }

    /// The function names the reference chapter documents: the leading
    /// backtick-quoted identifier of each table row's first cell.
    fn documented_fn_names(md: &str) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        for line in md.lines() {
            let line = line.trim_start();
            let Some(cell) = line.strip_prefix('|') else {
                continue;
            };
            if let Some(code) = cell.trim_start().strip_prefix('`') {
                let name: String = code
                    .chars()
                    .take_while(|&c| c == '_' || c.is_ascii_lowercase() || c.is_ascii_digit())
                    .collect();
                if is_api_ident(&name) {
                    out.insert(name);
                }
            }
        }
        out
    }

    /// The API reference (`book/src/reference.md`) documents exactly the
    /// host functions monada-script registers — no more, no less. This is
    /// the book's "reference can't drift" gate: adding or removing a host
    /// function fails CI until the reference is updated to match.
    #[test]
    fn api_reference_matches_registered_functions() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let read = |rel: &str| {
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
        };
        let sources = format!(
            "{}\n{}\n{}\n{}",
            read("crates/monada-script/src/rhai_backend.rs"),
            read("crates/monada-script/src/local_backend.rs"),
            // The volume-map physics verbs (digger demo, host_api 8).
            read("crates/monada-script/src/physics.rs"),
            // The grid frame verbs (grid-entities, host_api 16). Registered
            // beside the store they write, so this file has to be scraped too —
            // without it the gate passed vacuously for every `grid_*` verb.
            read("crates/monada-script/src/grids.rs"),
        );
        let registered = registered_fn_names(&sources);
        let documented = documented_fn_names(&read("book/src/reference.md"));

        // Guard against a parser regression silently emptying both sides
        // (empty == empty would pass vacuously).
        assert!(
            registered.len() > 50 && registered.contains("voxel_fill"),
            "scraper found too few registered functions ({})",
            registered.len()
        );
        assert!(
            documented.contains("voxel_fill"),
            "reference parser found no known function"
        );

        let undocumented: Vec<_> = registered.difference(&documented).collect();
        let phantom: Vec<_> = documented.difference(&registered).collect();
        assert!(
            undocumented.is_empty() && phantom.is_empty(),
            "API reference out of sync with monada-script.\n  \
             registered but undocumented: {undocumented:?}\n  \
             documented but not registered: {phantom:?}"
        );
    }
}
