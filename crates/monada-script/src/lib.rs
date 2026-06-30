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

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
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
    /// archive-relative path), turned `turns` quarter-steps clockwise about
    /// the vertical axis (so a map can face asymmetric art whichever way it
    /// needs — e.g. opposing sides facing each other); returns its model id.
    fn model_kv6(&mut self, asset_path: &str, turns: i64) -> i64;
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
    /// Orient the camera: `yaw`/`pitch` in radians — the orbit angles the
    /// view should start at. Lets a map face the scene its own way instead
    /// of inheriting the host's default angle.
    fn camera_angle(&mut self, yaw: Fixed, pitch: Fixed);

    /// Set the camera's orbit distance from its focus point (world voxels) —
    /// how close the view sits. The host clamps it to a sane range. The
    /// default ignores it (a map that never calls it keeps the host default).
    fn camera_dist(&mut self, _dist: Fixed) {}
    /// Queue a sim command for the host to route through the command path
    /// after the current trigger returns (never applied re-entrantly).
    fn submit_command(&mut self, verb: i64, target: i64, arg: FixedVec3);

    /// The local peer's player id, or `None` when there is no single local
    /// player (a single-window "hotseat" session that drives every side).
    /// A turn-based map gates which side this client may submit for by
    /// comparing this against its own side-to-move; the engine attaches no
    /// meaning to it. The script-side sentinel (`None` → a negative id)
    /// lives in exactly one place — the `local_player` host-fn registration.
    fn local_player(&self) -> Option<i64>;

    /// Declare the map's directional "sun": `dir` is the direction the
    /// light travels, `intensity` its strength. The host shades the map's
    /// sprites and grid from it. Render-side only.
    fn set_light(&mut self, dir: FixedVec3, intensity: Fixed);

    /// Load a sky panorama from an `assets/` image and render it behind the
    /// scene. Render-side only.
    fn set_sky(&mut self, asset_path: &str);

    /// Define an animated, 8-direction billboard "actor" model from GIFs laid
    /// out as `<dir_path>/<state>/<side>.gif` for the 8 compass sides (one
    /// `state` per animation). Returns a model id to bind with
    /// [`entity_set_model`](Self::entity_set_model), or `-1` if any GIF is
    /// missing. The renderer auto-picks the facing sprite from camera bearing
    /// vs the actor's facing yaw. Render-side only; the default ignores it.
    fn model_actor(&mut self, _dir_path: &str, _states: &[String]) -> i64 {
        -1
    }

    /// Set an actor entity's current animation state by name (one of the
    /// `states` given to [`model_actor`](Self::model_actor)). Render-side only.
    fn entity_set_anim(&mut self, _entity: i64, _state: &str) {}

    /// Set an actor entity's facing yaw in sim radians (`atan2(dy, dx)`); the
    /// renderer picks the matching directional sprite. Render-side only.
    fn entity_set_facing(&mut self, _entity: i64, _yaw: Fixed) {}

    /// Query whether a voxel is solid, in sim coordinates — the terrain
    /// collision primitive a real-time map needs (the script paints the
    /// world with [`voxel_fill`](Self::voxel_fill) / [`voxel_set`](Self::voxel_set),
    /// then asks this to keep movers out of it).
    ///
    /// **Determinism:** the backing store is a pure function of the
    /// deterministic script's paint calls, so every peer answers identically
    /// — results may feed hashed `tick()` decisions safely. The default
    /// (empty world) lets bridges that paint nothing — and headless callers
    /// that don't need terrain — skip it.
    fn voxel_solid(&self, _x: i64, _y: i64, _z: i64) -> bool {
        false
    }

    /// The highest solid sim-`z` in column `(x, y)`, or `0` for an unpainted
    /// column — the "stand on the ground" primitive (assumes columns are
    /// filled from the floor up). Same determinism contract as
    /// [`voxel_solid`](Self::voxel_solid).
    fn ground_height(&self, _x: i64, _y: i64) -> i64 {
        0
    }
}

/// A deterministic sparse terrain store: the highest solid sim-`z` per
/// `(x, y)` column, fed by [`voxel_fill`](HostBridge::voxel_fill) /
/// [`voxel_set`](HostBridge::voxel_set) and read by
/// [`voxel_solid`](HostBridge::voxel_solid) /
/// [`ground_height`](HostBridge::ground_height). Lives in sim space (no
/// render-side world-X mirror), so a script queries it in the same
/// coordinates it paints. Models terrain as per-column heights — fine for
/// arenas (floor + raised platforms + walls), which fill from the floor up;
/// it does not represent overhangs or holes.
#[derive(Default, Clone)]
pub struct VoxelStore {
    tops: BTreeMap<(i64, i64), i64>,
}

impl VoxelStore {
    /// A fresh, empty store (flat world at height 0).
    #[must_use]
    pub fn new() -> VoxelStore {
        VoxelStore::default()
    }

    /// Raise each column in the box to at least `max(z0, z1)`.
    #[allow(clippy::too_many_arguments)]
    pub fn fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64) {
        let (xa, xb) = (x0.min(x1), x0.max(x1));
        let (ya, yb) = (y0.min(y1), y0.max(y1));
        let top = z0.max(z1);
        for x in xa..=xb {
            for y in ya..=yb {
                let e = self.tops.entry((x, y)).or_insert(i64::MIN);
                *e = (*e).max(top);
            }
        }
    }

    /// Raise a single column to at least `z`.
    pub fn set(&mut self, x: i64, y: i64, z: i64) {
        let e = self.tops.entry((x, y)).or_insert(i64::MIN);
        *e = (*e).max(z);
    }

    /// Whether `(x, y, z)` is at or below the column's top solid voxel.
    #[must_use]
    pub fn solid(&self, x: i64, y: i64, z: i64) -> bool {
        self.tops.get(&(x, y)).is_some_and(|&top| z <= top)
    }

    /// The column's top solid sim-`z`, or `0` if unpainted.
    #[must_use]
    pub fn ground_height(&self, x: i64, y: i64) -> i64 {
        self.tops.get(&(x, y)).map_or(0, |&top| top)
    }
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
    fn model_kv6(&mut self, _asset_path: &str, _turns: i64) -> i64 {
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
    fn camera_angle(&mut self, _yaw: Fixed, _pitch: Fixed) {}
    fn submit_command(&mut self, _verb: i64, _target: i64, _arg: FixedVec3) {}
    fn local_player(&self) -> Option<i64> {
        None
    }
    fn set_light(&mut self, _dir: FixedVec3, _intensity: Fixed) {}
    fn set_sky(&mut self, _asset_path: &str) {}
}

/// A headless [`HostBridge`] that maintains a real [`VoxelStore`] — the
/// terrain a real-time map collides against — while no-opping all render /
/// input calls. The determinism-relevant counterpart to [`NullBridge`]:
/// used by headless tests and the oracle for maps whose `tick()` queries
/// [`voxel_solid`](HostBridge::voxel_solid) / [`ground_height`](HostBridge::ground_height),
/// so the collision the goldens hash matches the map's painted terrain.
#[derive(Default)]
pub struct TerrainBridge {
    terrain: VoxelStore,
}

impl TerrainBridge {
    #[must_use]
    pub fn new() -> TerrainBridge {
        TerrainBridge::default()
    }
}

impl HostBridge for TerrainBridge {
    fn model_box(&mut self, _w: i64, _h: i64, _d: i64, _color: i64) -> i64 {
        0
    }
    fn model_kv6(&mut self, _asset_path: &str, _turns: i64) -> i64 {
        0
    }
    fn entity_set_model(&mut self, _entity: i64, _model: i64) {}
    #[allow(clippy::too_many_arguments)]
    fn voxel_fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, _c: i64) {
        self.terrain.fill(x0, y0, z0, x1, y1, z1);
    }
    fn voxel_set(&mut self, x: i64, y: i64, z: i64, _color: i64) {
        self.terrain.set(x, y, z);
    }
    fn highlight(&mut self, _entity: i64) {}
    fn highlight_clear(&mut self) {}
    fn highlighted(&self) -> i64 {
        -1
    }
    fn status(&mut self, _text: &str) {}
    fn camera_focus(&mut self, _point: FixedVec3) {}
    fn camera_angle(&mut self, _yaw: Fixed, _pitch: Fixed) {}
    fn submit_command(&mut self, _verb: i64, _target: i64, _arg: FixedVec3) {}
    fn local_player(&self) -> Option<i64> {
        None
    }
    fn set_light(&mut self, _dir: FixedVec3, _intensity: Fixed) {}
    fn set_sky(&mut self, _asset_path: &str) {}
    fn voxel_solid(&self, x: i64, y: i64, z: i64) -> bool {
        self.terrain.solid(x, y, z)
    }
    fn ground_height(&self, x: i64, y: i64) -> i64 {
        self.terrain.ground_height(x, y)
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
