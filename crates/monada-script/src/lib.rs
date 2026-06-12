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

/// The M4 chess map (DESIGN.md §6) — a legal-move subset: piece moves +
/// capture (`despawn`) + win-on-king-capture. Castling / en passant /
/// promotion / check-mate detection are a later slice. Every rule lives
/// in the script; the engine knows nothing of chess (DESIGN.md §4). It
/// keeps authoritative game state (`to_move`, `winner`) in a singleton
/// `game` entity and reports turn / capture / illegal-move / game-over to
/// the host via [`ScriptBackend::drain_ui_events`]. Embedded until the
/// map archive format lands (M4 slice 2).
pub const CHESS_SCRIPT: &str = include_str!("../scripts/chess.rhai");

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

    /// Drain the UI/HUD events the script emitted via `ui_emit_event`
    /// since the last drain (DESIGN.md §3.3). These live strictly on the
    /// render side of the determinism wall — the host reads them for
    /// display, they never enter [`World`] state or the desync hash. A
    /// backend that emits none returns empty (the default).
    fn drain_ui_events(&mut self) -> Vec<UiEvent> {
        Vec::new()
    }
}

/// A UI/HUD-side event a script pushes via `ui_emit_event` (DESIGN.md
/// §3.3). Render-side only: the host drains it for display, it never
/// enters [`World`] state or the desync hash, so it can never desync a
/// peer. The payload is all-integer (no float crosses the wall); its
/// field meanings are a script↔host convention (see the chess map's
/// event codes), opaque to the engine itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiEvent {
    pub code: u32,
    pub a: i64,
    pub b: i64,
    pub c: i64,
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
