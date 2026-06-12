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

use monada_fixed::FixedVec3;
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

    /// Run the map's `pointer` trigger for a pointer event (DESIGN.md
    /// §3.3 input events): a button at sim-space `point` over the picked
    /// `entity` (or `-1` for none). The map owns the gesture state machine
    /// (select → act); the host only forwards the raw event. A map with no
    /// `pointer` handler ignores it (the default).
    ///
    /// # Errors
    /// Returns [`ScriptError::Run`] if the handler raises.
    fn on_pointer(
        &mut self,
        _button: i64,
        _point: FixedVec3,
        _entity: i64,
    ) -> Result<(), ScriptError> {
        Ok(())
    }

    /// Run the map's `key` trigger for a keyboard event. A map with no
    /// `key` handler ignores it (the default).
    ///
    /// # Errors
    /// Returns [`ScriptError::Run`] if the handler raises.
    fn on_key(&mut self, _code: i64, _down: bool) -> Result<(), ScriptError> {
        Ok(())
    }

    /// Drain the UI/HUD events the script emitted via `ui_emit_event`
    /// since the last drain (DESIGN.md §3.3). These live strictly on the
    /// render side of the determinism wall — the host reads them for
    /// display, they never enter [`World`] state or the desync hash. A
    /// backend that emits none returns empty (the default).
    fn drain_ui_events(&mut self) -> Vec<UiEvent> {
        Vec::new()
    }
}

/// The render / input / command host-API surface (DESIGN.md §3.3) that
/// lives on the **host** side of the wall. `monada-script` defines only
/// these primitive signatures — no roxlap render types — so the sim /
/// script wall holds; the host ([`monada-host`]) implements them (the
/// sprite-model registry, the voxel world grid, local selection, command
/// routing). A [`RhaiBackend`] with no bridge set treats every render/
/// input call as a no-op, so headless tests and the determinism oracle
/// need no host (use [`NullBridge`]).
///
/// Coordinates are **sim space** (the same the script uses for entity
/// positions); the host owns the sim→world scale, the camera, and the
/// z-convention. Local UI state (selection) is per-player and **never**
/// enters [`World`] or the desync hash.
pub trait HostBridge: Send {
    /// Define a procedural box sprite model; returns its model id.
    fn model_box(&mut self, w: i64, h: i64, d: i64, color: i64) -> i64;
    /// Define a sprite model from a KV6 asset in the map archive (by its
    /// archive-relative path); returns its model id.
    fn model_kv6(&mut self, asset_path: &str) -> i64;
    /// Bind an entity to a base render model (render-side, not hashed).
    fn entity_set_model(&mut self, entity: i64, model: i64);
    /// Paint a solid voxel box into the world grid, in sim coordinates.
    /// (Two corners + colour reads naturally as separate args for scripts.)
    #[allow(clippy::too_many_arguments)]
    fn voxel_fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64);
    /// Paint a single voxel into the world grid, in sim coordinates.
    fn voxel_set(&mut self, x: i64, y: i64, z: i64, color: i64);
    /// Mark `entity` as the locally selected one (a highlight overlay).
    fn highlight(&mut self, entity: i64);
    /// Clear the local selection.
    fn highlight_clear(&mut self);
    /// The locally selected entity, or `-1`.
    fn highlighted(&self) -> i64;
    /// Set the HUD status line.
    fn status(&mut self, text: &str);
    /// Aim the camera at a point (sim coordinates).
    fn camera_focus(&mut self, point: FixedVec3);
    /// Queue a sim command for the host to route through the command path
    /// after the current trigger returns (never applied re-entrantly).
    fn submit_command(&mut self, verb: i64, target: i64, arg: FixedVec3);
}

/// A shared host bridge handle: the host owns the concrete render state
/// and hands a coerced clone to the [`RhaiBackend`].
pub type SharedBridge = Arc<Mutex<dyn HostBridge + Send>>;

/// A do-nothing [`HostBridge`] for headless runs (tests, oracle): render
/// and input calls are no-ops, `highlighted` is empty. Lets a map whose
/// `init` paints a board / defines models run with no window.
pub struct NullBridge;

impl HostBridge for NullBridge {
    fn model_box(&mut self, _w: i64, _h: i64, _d: i64, _color: i64) -> i64 {
        0
    }
    fn model_kv6(&mut self, _asset_path: &str) -> i64 {
        0
    }
    fn entity_set_model(&mut self, _entity: i64, _model: i64) {}
    #[allow(clippy::too_many_arguments)]
    fn voxel_fill(&mut self, _x0: i64, _y0: i64, _z0: i64, _x1: i64, _y1: i64, _z1: i64, _c: i64) {}
    fn voxel_set(&mut self, _x: i64, _y: i64, _z: i64, _color: i64) {}
    fn highlight(&mut self, _entity: i64) {}
    fn highlight_clear(&mut self) {}
    fn highlighted(&self) -> i64 {
        -1
    }
    fn status(&mut self, _text: &str) {}
    fn camera_focus(&mut self, _point: FixedVec3) {}
    fn submit_command(&mut self, _verb: i64, _target: i64, _arg: FixedVec3) {}
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
