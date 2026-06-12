//! monada scripting runtime and the engine-side API surface scripts
//! call into (DESIGN.md §3.3, §5).
//!
//! This crate is the **strict wall** between the script language and sim
//! types: it is the only place that links `rhai` *and* `monada-sim`. The
//! runtime is swappable behind [`ScriptBackend`] so the Rhai -> WASM
//! migration (§5.5) does not cascade into engine code.
//!
//! Determinism: Rhai is built with `no_float`, so scripts cannot do IEEE
//! arithmetic at all — sim math goes through `monada-fixed`. All gameplay
//! state lives in the [`World`](monada_sim::World) (decision A2), so the
//! script keeps no hashed state of its own; it reads/writes entity
//! position and fields through the host API.
#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Arc, Mutex};

use monada_sim::{Command, PlayerId, World};

mod driver;
mod rhai_backend;

pub use driver::RhaiDriver;
pub use rhai_backend::RhaiBackend;

/// The shared, lockable world a [`ScriptBackend`] mutates.
///
/// `sync`-feature Rhai needs `Send + Sync` host functions, so the world
/// is shared as `Arc<Mutex<World>>`. The sim is single-threaded, so the
/// lock never contends — the `Mutex` is just what `Send + Sync` demands.
pub type SharedWorld = Arc<Mutex<World>>;

/// Convenience: a fresh shared world seeded for its RNG.
#[must_use]
pub fn shared_world(seed: u64) -> SharedWorld {
    Arc::new(Mutex::new(World::new(seed)))
}

/// The M2 walk-in-a-circle scenario, as a script (DESIGN.md §7) — the
/// engine knows nothing about circles. Embedded until the map archive
/// format lands (M4).
pub const WALK_CIRCLE_SCRIPT: &str = include_str!("../scripts/walk_circle.rhai");

/// The M3 command-driven demo scenario (DESIGN.md §3.1, §7). Players
/// issue [`Command`]s over the lockstep wire: `verb == 1` spawns a unit
/// at the command's point, `verb == 2` sets a unit's velocity; `tick`
/// integrates position by velocity. The engine knows nothing about the
/// verbs — it is the script that interprets them. Exercises the whole
/// command path end to end (`on_command` -> host API -> `World`).
pub const COMMAND_DEMO_SCRIPT: &str = include_str!("../scripts/command_demo.rhai");

/// Build a seeded world, load `source`, run its `init` trigger then
/// `ticks` `tick` triggers, and return the shared world. The reusable
/// scenario runner for tests and the determinism oracle.
///
/// # Errors
/// Propagates any compile/run [`ScriptError`].
pub fn run_script(seed: u64, source: &str, ticks: u64) -> Result<SharedWorld, ScriptError> {
    let world = shared_world(seed);
    let mut backend = RhaiBackend::new(world.clone());
    backend.load(source)?;
    backend.on_init()?;
    for _ in 0..ticks {
        backend.on_tick()?;
    }
    Ok(world)
}

/// A scripting backend: compile a script, then drive it through the
/// engine's trigger entry points. Implemented by [`RhaiBackend`] in v0;
/// a `WasmBackend` lands behind a feature flag post-v0 (§5.5).
pub trait ScriptBackend {
    /// Compile / prepare `source`. Replaces any previously loaded script.
    ///
    /// # Errors
    /// Returns [`ScriptError::Compile`] on a parse/compile failure.
    fn load(&mut self, source: &str) -> Result<(), ScriptError>;

    /// Run the map's `init` trigger (declare archetypes, spawn entities,
    /// set up initial state).
    ///
    /// # Errors
    /// Returns [`ScriptError::Run`] if the script raises.
    fn on_init(&mut self) -> Result<(), ScriptError>;

    /// Run the map's `command` trigger for one player [`Command`]
    /// (DESIGN.md §3.1, M3). Called by the lockstep session for every
    /// command of a released tick, in canonical player order, *before*
    /// [`on_tick`](Self::on_tick). A script that defines no `command`
    /// handler treats this as a no-op — the engine never interprets the
    /// command itself.
    ///
    /// # Errors
    /// Returns [`ScriptError::Run`] if the handler raises.
    fn on_command(&mut self, player: PlayerId, command: &Command) -> Result<(), ScriptError>;

    /// Advance one simulation tick: bump the world tick, then run the
    /// map's `tick` trigger.
    ///
    /// # Errors
    /// Returns [`ScriptError::Run`] if the script raises.
    fn on_tick(&mut self) -> Result<(), ScriptError>;
}

/// A script compile- or run-time failure (message only; the underlying
/// `rhai` error type is kept out of the public API behind the wall).
#[derive(Debug, Clone)]
pub enum ScriptError {
    Compile(String),
    Run(String),
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptError::Compile(m) => write!(f, "script compile error: {m}"),
            ScriptError::Run(m) => write!(f, "script run error: {m}"),
        }
    }
}

impl std::error::Error for ScriptError {}
