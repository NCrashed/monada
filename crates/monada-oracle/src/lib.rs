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
use monada_script::{
    run_script, shared_world, RhaiBackend, RhaiDriver, ScriptBackend, COMMAND_DEMO_SCRIPT,
    WALK_CIRCLE_SCRIPT,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId, World};

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
        map_hash: monada_net::map_hash(COMMAND_DEMO_SCRIPT),
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
            monada_net::map_hash(COMMAND_DEMO_SCRIPT),
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

/// Every gated scenario's checkpoints, in a fixed order.
#[must_use]
pub fn all_checkpoints() -> Vec<Checkpoint> {
    let mut out = walk_checkpoints();
    out.extend(kernel_checkpoints());
    out.extend(lockstep_checkpoints());
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
         lockstep (two-session command demo); seed \"MONADA_0\".\n",
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
