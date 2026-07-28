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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_sim::{Command, PlayerId, World};

mod driver;
mod local_backend;
mod physics;
mod rhai_backend;
mod volume;

pub use driver::RhaiDriver;
pub use local_backend::LocalBackend;
// Re-exported because it is part of VolumeStore's own API surface
// (`set`/`fill`/`get` speak MaterialId) — consumers of the store should
// not need a direct monada-physics edge for the id newtype.
pub use monada_physics::MaterialId;
pub use physics::{shared_physics, DrillToolDef, PhysicsSim, SharedPhysics};
pub use rhai_backend::RhaiBackend;
pub use volume::VolumeStore;

/// The host script-API version this crate registers: the set of
/// functions, their names, and their semantics that map scripts see
/// (both the sim layer, [`RhaiBackend`], and the local layer,
/// [`LocalBackend`]). A map's manifest declares the version it requires
/// (`host_api`); a host runs it only when that requirement falls inside
/// [`HOST_API_OLDEST`]`..=`[`HOST_API_VERSION`] — see [`check_host_api`].
///
/// Bump discipline: *adding* functions bumps `HOST_API_VERSION` only —
/// maps requiring older versions keep working. Renaming, removing, or
/// changing the observable semantics of a registered function is a
/// breaking change: bump **both** constants to the same new value, so
/// maps written against the old surface are refused loudly instead of
/// desyncing or dying mid-game.
/// History: 2 = `camera_pan`; 3 = `nav_path`/`nav_block` (RTS demo,
/// docs/plans/rts-demo.md); 4 = `vision_observer`/`vision_config`/
/// `vision_hear` (ship demo, docs/plans/ship-visibility.md); 5 =
/// `highlight_add`/`highlighted_all`/`drag_begin`/`drag_end` (RTS
/// multi-select); 6 = `voxel_clear` (destructibles — RTS tree felling,
/// ship doors); 7 = `grid_spawn`/`voxel_fill_in` + `vision_observer`'s
/// grid overload (multi-grid ships); 8 = the `phys_*` sim-physics verbs
/// and the material-id overloads of `voxel_fill`/`voxel_set` on volume
/// maps (digger demo, docs/plans/digger-demo.md §1c) — NB on a volume
/// map, paints without a material id write material 0, so the map's
/// first `phys_material` call is its ground material and must precede
/// terrain contact (the material-0 contract on `register_physics_api`);
/// 9 = `phys_drill_tool`/`phys_drill` (the one-call drill loop, digger
/// D3); 10 = `atan2` + `phys_material_color` (digger D4 polish); 11 =
/// `body_deco_box` + `drill_indicator` (digger feel polish — render
/// trim and the spinning-bore telltale on the body mirror) +
/// `phys_solid` (the volume-store solidity read — the "roof over me"
/// predicate); 12 = `entity_set_grid` + `grid_orient` +
/// `camera_focus_entity` (entities ride a grid's transform, which can
/// turn to any 3D orientation — crew stay put on a moving/rotating
/// hull). Additive, deliberately: riding a grid is opt-in through
/// `entity_set_grid` alone, so a grid a map never binds an entity to
/// and never orients stays the static offset v7 gave it, and every verb
/// that reads one — fog, the deck cutaway, picking, the camera —
/// composes against an identity transform exactly as before; 13 =
/// `grid_pivot` (the grid-local point `grid_orient` turns about, so a
/// hull turns in place instead of swinging about a corner).
pub const HOST_API_VERSION: u32 = 13;

/// The oldest declared `host_api` requirement this build still fully
/// honors. Trails [`HOST_API_VERSION`] while growth stays additive; a
/// breaking change catches it up.
pub const HOST_API_OLDEST: u32 = 1;

/// Check a map's declared `host_api` requirement against this build's
/// supported range. The one gate every map-loading path shares.
///
/// # Errors
/// A human-readable refusal naming both sides' versions.
pub fn check_host_api(required: u32) -> Result<(), String> {
    if (HOST_API_OLDEST..=HOST_API_VERSION).contains(&required) {
        Ok(())
    } else {
        Err(format!(
            "map requires host API v{required}; this host supports \
             v{HOST_API_OLDEST}..v{HOST_API_VERSION}"
        ))
    }
}

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

    // Input events (`pointer` / `action`) are *not* part of this trigger
    // set: they belong to the map's local, unsynchronized script layer
    // ([`LocalBackend`]) — the sim backend receives player input only as
    // [`Command`]s (docs/plans/input-bindings.md).

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
    /// Bind an entity to a `grid_spawn` grid (by its handle), so it rides
    /// that grid's transform: its sim `position` is read as grid-local and
    /// composed through the grid's origin + rotation when rendered — a crew
    /// stays seated on a hull that moves or turns. Pass `-1` to UNBIND (the
    /// entity returns to the global frame — stepping off the hull). This is the
    /// only verb that binds an entity: naming a grid on
    /// [`vision_observer_in`](Self::vision_observer_in) never binds anything, it
    /// only says which grid the fog rides. Binding the fog OBSERVER also moves
    /// the fog/`deck_clip` onto that grid, so the cone and the crew member can
    /// never disagree about which hull they are on. Render-side, not hashed; an
    /// out-of-range handle is ignored, and a binding is dropped when its entity
    /// despawns. Unbound entities render in the global frame as before
    /// (`host_api` 12).
    fn entity_set_grid(&mut self, _entity: i64, _grid: i64) {}
    /// Paint a solid voxel box into the world grid, in sim coordinates.
    /// (Two corners + colour reads naturally as separate args for scripts.)
    /// `color` is roxlap-packed `0xBB_RR_GG_BB` — the HIGH byte is
    /// brightness, not alpha: `0x0080_8080` is a *black* voxel,
    /// `0x8080_8080` a mid-grey one.
    #[allow(clippy::too_many_arguments)]
    fn voxel_fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64);
    /// Paint a single voxel into the world grid, in sim coordinates.
    fn voxel_set(&mut self, x: i64, y: i64, z: i64, color: i64);
    /// Cut column `(x, y)` down to below sim-`z`: everything at and above
    /// `z` becomes air, in the render grid AND the collision store — so
    /// movers, `nav_path` and the eye agree the space opened (a felled
    /// tree, a breached wall). The heightmap collision store truncates
    /// columns (it cannot hole-punch); repaint the floor voxel afterwards
    /// if the clear reached ground level. Same determinism contract as
    /// [`voxel_fill`](Self::voxel_fill). The default ignores it.
    fn voxel_clear(&mut self, _x: i64, _y: i64, _z: i64) {}

    /// Spawn a new voxel grid offset by SIM cell `(wx, wy, wz)` from the world
    /// origin and return its render-side grid id. Use `(0, 0, 0)` for the world
    /// origin. The offset composes with the mirror/scale/z-down transform the
    /// grid paints with, so `(wx, wy, wz)` lands on the same world voxel a
    /// `voxel_set(wx, wy, wz)` would. The id is render-side only — never put it
    /// into [`World`] state or hashed `tick()` logic. Paint into the grid with
    /// [`voxel_fill_in`](Self::voxel_fill_in). The default returns `-1`
    /// (no grid allocated).
    fn grid_spawn(&mut self, _wx: i64, _wy: i64, _wz: i64) -> i64 {
        -1
    }

    /// Paint a solid voxel box into a specific grid (by id from
    /// [`grid_spawn`](Self::grid_spawn)), in sim coordinates. Same
    /// coordinate convention as [`voxel_fill`](Self::voxel_fill) but
    /// render-side only — does NOT update the collision store. The default
    /// ignores it.
    #[allow(clippy::too_many_arguments)]
    fn voxel_fill_in(
        &mut self,
        _grid: i64,
        _x0: i64,
        _y0: i64,
        _z0: i64,
        _x1: i64,
        _y1: i64,
        _z1: i64,
        _color: i64,
    ) {
    }

    /// Turn a `grid_spawn` grid to a 3D orientation: `angle` radians about the
    /// (unit-normalised) `axis`, replacing the grid's current rotation — so a
    /// hull can pitch, roll and yaw, not merely spin about vertical. Entities
    /// bound to the grid via [`entity_set_grid`](Self::entity_set_grid) and its
    /// fog/`deck_clip` ride the new pose. The `axis` is in SIM coordinates (+z
    /// up, the frame every other verb takes), right-handed about it — the host
    /// maps it through the same sim→world transform the grid's voxels are
    /// painted with. Render-side, not hashed; a zero-length axis or out-of-range
    /// handle is ignored. The turn is about the grid's
    /// [`grid_pivot`](Self::grid_pivot) point. The default ignores it.
    fn grid_orient(&mut self, _grid: i64, _axis: FixedVec3, _angle: Fixed) {}

    /// Name the grid-local point [`grid_orient`](Self::grid_orient) turns a
    /// `grid_spawn` grid about, in SIM cells — the frame `voxel_fill_in` paints
    /// in, so a hull spanning cells `0..=19` turns about its middle at `9.5`.
    /// Without this a grid turns about its own local origin, which for a hull
    /// painted up from cell `(0,0,0)` is a CORNER: the whole hull then swings
    /// through an arc wider than itself instead of turning in place. Sticky —
    /// call it once at spawn; both orders work (`grid_pivot` before or after
    /// `grid_orient` land the same pose). Render-side, not hashed; an
    /// out-of-range handle is ignored. The default ignores it.
    fn grid_pivot(&mut self, _grid: i64, _point: FixedVec3) {}

    /// Mark `entity` as the locally selected one (a highlight overlay),
    /// REPLACING any current selection (single-select semantics — the
    /// chess-era contract).
    fn highlight(&mut self, entity: i64);
    /// ADD `entity` to the local selection (multi-select, e.g. an RTS box
    /// select) without touching what is already selected. The default
    /// ignores it.
    fn highlight_add(&mut self, _entity: i64) {}
    /// Clear the local selection.
    fn highlight_clear(&mut self);
    /// The locally selected entity, or `-1`. With a multi-selection this
    /// is its first (lowest-id) member — single-select maps never see the
    /// difference.
    fn highlighted(&self) -> i64;
    /// Every locally selected entity (ascending id). The group-order side
    /// of multi-select: a map iterates this to submit one command per
    /// selected unit. The default is empty.
    fn highlighted_all(&self) -> Vec<i64> {
        Vec::new()
    }

    /// Begin a pointer drag gesture: the host anchors a ground-space
    /// rectangle at the cursor's CURRENT ground point and keeps its far
    /// corner glued to the cursor until [`drag_end`](Self::drag_end),
    /// drawing the rectangle as a world overlay each frame. Gesture state
    /// lives host-side because the local script layer is stateless (pure
    /// functions — it cannot carry the anchor between ticks). Per-client,
    /// render-side, never hashed. The default ignores it.
    fn drag_begin(&mut self) {}
    /// Finish the drag gesture and take its rectangle: the FOUR sim-space
    /// ground corners of the screen-aligned box (wound around the quad),
    /// or empty if no drag was active (or the anchor never hit the ground).
    /// The corners follow the camera's screen axes at the live yaw — not
    /// world N/S/E/W — so an orbited view still box-selects what the player
    /// framed; opposite corners `[0]`/`[2]` are the press/release diagonal.
    /// The map decides what the rectangle means — box select, formation
    /// line, building footprint. The default is empty.
    fn drag_end(&mut self) -> Vec<FixedVec3> {
        Vec::new()
    }
    /// Set the HUD status line.
    fn status(&mut self, text: &str);
    /// Aim the camera at a point (sim coordinates).
    fn camera_focus(&mut self, point: FixedVec3);
    /// Aim the camera at an entity's `point` (sim coordinates), composed through
    /// the grid the entity rides — so following a crew member on a moving or
    /// rotating hull tracks its true world seat, not the un-transformed cell.
    ///
    /// `point` looks redundant next to `entity` — it is what the map would get
    /// from `entity_position(entity)` — but a bridge is a RENDER-side sink with
    /// no handle on the sim world, so it cannot read the position itself; the
    /// entity is here only to name whose grid to compose through. That also
    /// leaves the door open for aiming somewhere else in the entity's frame (a
    /// point ahead of the crew, a turret mount) without a second verb.
    /// Render-side only; the default no-ops (headless bridges have no camera).
    fn camera_focus_entity(&mut self, _entity: i64, _point: FixedVec3) {}
    /// Orient the camera: `yaw`/`pitch` in radians — the orbit angles the
    /// view should start at. Lets a map face the scene its own way instead
    /// of inheriting the host's default angle.
    fn camera_angle(&mut self, yaw: Fixed, pitch: Fixed);

    /// Set the camera's orbit distance from its focus point (world voxels) —
    /// how close the view sits. The host clamps it to a sane range. The
    /// default ignores it (a map that never calls it keeps the host default).
    fn camera_dist(&mut self, _dist: Fixed) {}

    /// Shift the camera focus by a sim-space delta (cells) — a free RTS-style
    /// camera pan. Unlike `camera_focus` (absolute, needs a point the script
    /// can name), a pan accumulates on the host's stored focus, so a *local*
    /// layer with no persistent state of its own can scroll the view
    /// (script functions are pure — they cannot carry a focus variable
    /// across ticks). Per-client, render-side only; the default ignores it.
    fn camera_pan(&mut self, _dx: Fixed, _dy: Fixed) {}

    /// Dissolve geometry between the camera and its focus point inside a
    /// screen-space "keyhole" (roxlap's `ViewCutout`) — so a third-person crew
    /// member is never hidden behind the wall the camera looks through. `radius`
    /// is the keyhole size in sim cells (the host projects it to pixels at the
    /// focus distance), `feather` the soft-edge band (cells). `radius <= 0`
    /// turns it off. Primary rays only — cut walls still cast shadows, still
    /// block collision + vision. Render-side only; the default ignores it.
    fn camera_cutout(&mut self, _radius: Fixed, _feather: Fixed) {}

    /// Show only the deck band `z_lo..=z_hi` (sim z) of the vision grid (the
    /// grid named by [`vision_observer`](Self::vision_observer), else the world
    /// grid), cutting away everything ABOVE it (a ceiling / upper-deck cutaway)
    /// so the camera
    /// sees into the deck the crew stands on. Maps to roxlap's `Grid::z_clip`
    /// (the engine clips one side — the top of the band). Call it with the
    /// local crew's deck band; a band whose top is the tallest thing in the
    /// world cuts nothing. Render-side only; the default ignores it.
    fn deck_clip(&mut self, _z_lo: i64, _z_hi: i64) {}

    /// Declare the local viewpoint for fog of war: the host maintains a
    /// per-observer visibility mask against the world grid, updated every frame
    /// from this entity's cell, facing, deck and eye height. Unseen cells dim to
    /// a remembered "last seen" look; cells outside the live view hide the
    /// actors standing in them. Pass `-1` to clear. **Per-client, never hashed**
    /// — declare the LOCAL crew member (`local_player()`'s entity), so each peer
    /// sees its own line of sight. Render-side only; the default ignores it.
    fn vision_observer(&mut self, _entity: i64) {}
    /// Like [`vision_observer`](Self::vision_observer) but against a specific
    /// [`grid_spawn`](Self::grid_spawn) grid (`grid` handle) instead of the world
    /// grid — the ship demo's hull, so fog + `deck_clip` ride the crew's own
    /// (movable) grid. The mask is grid-local. This names the fog's grid ONLY —
    /// it does not bind the observer entity to it (bind explicitly with
    /// [`entity_set_grid`](Self::entity_set_grid), which also takes precedence
    /// here: an observer that rides a grid fogs that grid, whatever this named).
    /// `host_api` 7. Render-side only; the default delegates to
    /// [`vision_observer`](Self::vision_observer).
    fn vision_observer_in(&mut self, entity: i64, _grid: i64) {
        self.vision_observer(entity);
    }
    /// Tune the observer's vision: facing-cone half-angle (`cone_deg` degrees),
    /// cone reach and 360° peripheral reach (`range`/`peripheral` cells). Sets
    /// roxlap's `VisionConfig`. Render-side only; the default ignores it.
    fn vision_config(&mut self, _cone_deg: i64, _range: i64, _peripheral: i64) {}
    /// Briefly reveal cell `(x, y, z)` from a heard sound (SS13 "you hear
    /// something" — live data, remembered styling); `loudness` in `0..1`. Pairs
    /// with `play_sound`. Render-side only; the default ignores it.
    fn vision_hear(&mut self, _x: i64, _y: i64, _z: i64, _loudness: Fixed) {}

    /// Bind a render colour (roxlap-packed `0xBB_RR_GG_BB`, like
    /// [`voxel_fill`](Self::voxel_fill)) to a physics material id: the
    /// automatic body mirror blits shape voxels of that material in this
    /// colour instead of the engine's fallback palette. Render-side only
    /// — material ids and their PHYSICAL properties stay hashed sim
    /// state; this is their look. Call at init, before bodies first
    /// appear on screen: a binding made AFTER a body was blitted shows
    /// only on that body's next re-blit (a carve — none exists until
    /// `phys_carve` lands). The default ignores it.
    fn phys_material_color(&mut self, _mat: i64, _color: i64) {}

    /// Paint a render-only decoration box into a body's mirror, in FINE
    /// voxels (16 per sim cell), SHAPE-local coordinates — the box may
    /// extend beyond the shape (skirts under the hull, a cockpit on
    /// top, fenders over the wheels). Rides the physics pose
    /// automatically; never enters the hashed shape. Call at init. The
    /// default ignores it.
    #[allow(clippy::too_many_arguments)]
    fn body_deco_box(
        &mut self,
        _body: i64,
        _x0: i64,
        _y0: i64,
        _z0: i64,
        _x1: i64,
        _y1: i64,
        _z1: i64,
        _color: i64,
    ) {
    }

    /// Drive a body's drill indicator: a cone mirroring the registered
    /// drill tool, tilted by `pitch` (radians, the same value the map
    /// passes to `phys_drill`) and spinning while `spinning` — the
    /// "drilling is working" telltale. Call every tick, like the camera
    /// verbs. Render-side only; the default ignores it.
    fn drill_indicator(&mut self, _body: i64, _pitch: Fixed, _spinning: bool) {}

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

    // --- local-layer input queries (docs/plans/input-bindings.md) ---------
    // Served to the map's *local* script layer only ([`LocalBackend`]) —
    // never registered into the sim backend, so the deterministic sim
    // physically cannot observe raw input. The host resolves physical
    // keys through its binding table and exposes only the map's declared
    // action ids here. Every return type is a sim type (bool / int /
    // fixed-point), so a value can flow straight into a `Command` payload.
    // Defaults = "no input" for headless bridges.

    /// Whether the map-declared `button` action `id` is currently held.
    fn action_down(&self, _id: &str) -> bool {
        false
    }
    /// The map-declared `axis` action's value: `-1`, `0` or `+1`.
    fn action_axis(&self, _id: &str) -> i64 {
        0
    }
    /// The map-declared `axis2` action's value: `(x, y)`, each `-1..=+1`
    /// (x = right − left, y = up − down).
    fn action_axis2(&self, _id: &str) -> (i64, i64) {
        (0, 0)
    }
    /// The cursor's ground-plane point in sim coordinates, or `None` when
    /// the cursor ray misses the world. Quantized fixed-point — safe to
    /// embed in a command payload as-is.
    fn pick_ground(&self) -> Option<FixedVec3> {
        None
    }
    /// The entity under the cursor (nearest model-bound entity within the
    /// host's pick radius), or `-1`. Refreshed by the host each frame —
    /// the WC3 invisible-unit-sphere hack, made a one-call primitive.
    fn pick_entity(&self) -> i64 {
        -1
    }
    /// The sim-space yaw (radians, fixed-point) from the local player /
    /// camera focus toward the cursor's ground point. Holds its last
    /// value while the ray misses (matches the classic twin-stick aim).
    fn aim_yaw(&self) -> Fixed {
        Fixed::ZERO
    }
    /// Take (return **and clear**) the HUD-button bits clicked since the
    /// last call ([`ui_button`](Self::ui_button)'s `button_bit`s, OR-ed).
    /// The local layer folds these into its per-tick input command.
    fn ui_clicks(&mut self) -> i64 {
        0
    }

    /// Declare the map's directional "sun": `dir` is the direction the
    /// light travels, `intensity` its strength. The host shades the map's
    /// sprites and grid from it. Render-side only.
    fn set_light(&mut self, dir: FixedVec3, intensity: Fixed);

    /// Load a sky panorama from an `assets/` image and render it behind the
    /// scene. Render-side only.
    fn set_sky(&mut self, asset_path: &str);

    /// Define an animated, 8-direction billboard "actor" model from GIFs laid
    /// out as `<dir_path>/<state>/<side>.gif` for the 8 compass sides (one
    /// `state` per animation). `height_cells` is the actor's rendered height
    /// in sim cells — the host scales the art to that regardless of its pixel
    /// resolution (so swapping art sizes doesn't change the on-screen size).
    /// Returns a model id to bind with [`entity_set_model`](Self::entity_set_model),
    /// or `-1` if any GIF is missing. The renderer auto-picks the facing
    /// sprite from camera bearing vs the actor's facing yaw. Render-side only;
    /// the default ignores it.
    fn model_actor(&mut self, _dir_path: &str, _states: &[String], _height_cells: Fixed) -> i64 {
        -1
    }

    /// Nudge an actor model's sprites down (`cells` > 0) or up (< 0) by that
    /// many cells, on top of the pivot-computed grounding. Lets a map correct
    /// art whose visible feet aren't at the trimmed opaque bottom (e.g. a baked
    /// shadow) without re-authoring the GIFs. Render-side only.
    fn model_drop(&mut self, _model: i64, _cells: Fixed) {}

    /// Set an actor entity's current animation state by name (one of the
    /// `states` given to [`model_actor`](Self::model_actor)). Render-side only.
    fn entity_set_anim(&mut self, _entity: i64, _state: &str) {}

    /// Set an actor entity's facing yaw in sim radians (`atan2(dy, dx)`); the
    /// renderer picks the matching directional sprite. Render-side only.
    fn entity_set_facing(&mut self, _entity: i64, _yaw: Fixed) {}

    /// Tint an actor entity's sprite by an `0x00RR_GGBB` colour multiply
    /// (`0x00FF_FFFF` = white = no tint; e.g. `0x00FF_4040` = damage red).
    /// Render-side only — flash a hit without touching the hashed sim.
    fn entity_set_tint(&mut self, _entity: i64, _tint: i64) {}

    // --- audio (render-side, never hashed) --------------------------------
    // Sounds are triggered from `tick`, but — like `status`/`entity_set_anim`
    // — they never touch the world hash: the host mixes them, and a headless
    // peer/oracle no-ops them, so a match can't desync on audio. The host
    // COALESCES identical one-shots fired the same frame and rate-limits rapid
    // repeats, so many entities triggering the same sound at once (a wave of
    // attackers) plays it once, not stacked into a roar.

    /// Play a one-shot sound (`assets/…` path). Many different sounds mix in
    /// parallel; identical ones are de-duplicated per frame + debounced.
    fn play_sound(&mut self, _asset_path: &str) {}
    /// [`play_sound`](Self::play_sound) with an explicit gain (`0..1`, clamped).
    fn play_sound_gain(&mut self, _asset_path: &str, _gain: Fixed) {}
    /// Synthesise a short "voice" blip on the fly — the Undertale-style typing
    /// sound. `wave`: 0 square / 1 saw / 2 triangle / 3 sine / 4 noise; `freq`
    /// in Hz (the character's pitch); `dur_ms` length; `gain` 0..1. Mixed in
    /// parallel, no de-dup (fire one per typed glyph). Render-side.
    fn play_blip(&mut self, _wave: i64, _freq: i64, _dur_ms: i64, _gain: Fixed) {}
    /// Keep a looping sound audible: call it every tick the loop should play
    /// (e.g. footsteps while moving). The host starts it on the first request
    /// and stops it shortly after the calls stop — so a *state* (moving) drives
    /// a seamless loop with no restart-per-frame and no per-actor timer. Unlike
    /// [`play_music`](Self::play_music), several loops can overlap.
    fn play_loop(&mut self, _asset_path: &str) {}
    /// Start (or replace) the looping background track. Idempotent for the same
    /// path — re-calling with the current track keeps it playing seamlessly.
    fn play_music(&mut self, _asset_path: &str) {}
    /// Stop the background track started by [`play_music`](Self::play_music).
    fn stop_music(&mut self) {}

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

    /// Mark / clear cell `(x, y)` as explicitly impassable for navigation
    /// (a building footprint, a prop) regardless of its height — the
    /// overlay [`nav_path`](Self::nav_path) ANDs with the heightfield walk
    /// rule. Same determinism contract as [`voxel_fill`](Self::voxel_fill):
    /// fed only by command-driven script calls, so every peer holds the
    /// same set. The default ignores it.
    fn nav_block(&mut self, _x: i64, _y: i64, _on: bool) {}

    /// A deterministic path from cell `(x0, y0)` to `(x1, y1)`: budgeted
    /// integer A* (`monada-nav`) under the shared walk rule — a step
    /// between neighbouring cells passes when |Δ[`ground_height`](Self::ground_height)|
    /// ≤ `max_step` and the target isn't [`nav_block`](Self::nav_block)ed;
    /// diagonals may not cut corners. Waypoints are cell coordinates with
    /// `z` = ground height; empty when already there. An unreachable goal
    /// yields the best-effort path toward the closest reachable cell —
    /// never an error. Same determinism contract as
    /// [`voxel_solid`](Self::voxel_solid), so results may steer hashed
    /// `tick()` movement. The default (no terrain) finds nothing.
    fn nav_path(&self, _x0: i64, _y0: i64, _x1: i64, _y1: i64, _max_step: i64) -> Vec<FixedVec3> {
        Vec::new()
    }

    /// Load a per-cell tile texture from an `assets/` PNG, resampled to the
    /// host's cell resolution. Returns a tile id for [`tile_fill`](Self::tile_fill),
    /// or `-1` if the asset is missing. Render-side only; the default ignores it.
    fn tile(&mut self, _asset_path: &str) -> i64 {
        -1
    }

    /// Paint a cell region (sim coords) from height `z0..z1` with a tile — its
    /// pixels become the cells' voxel colours — feeding collision exactly like
    /// [`voxel_fill`](Self::voxel_fill) (so a textured wall still blocks). The
    /// default does nothing.
    #[allow(clippy::too_many_arguments)]
    fn tile_fill(
        &mut self,
        _x0: i64,
        _y0: i64,
        _z0: i64,
        _x1: i64,
        _y1: i64,
        _z1: i64,
        _tile: i64,
    ) {
    }

    /// Register a marching-squares transition sheet (a 4×4 `.png`) for terrain
    /// type `high` blended over `low` (higher type id = higher priority). Used
    /// by [`terrain_blit`](Self::terrain_blit) to autotile the flat floor.
    /// Render-side only; the default ignores it.
    fn transition(&mut self, _low: i64, _high: i64, _asset_path: &str) {}

    /// Set the flat-floor terrain type of every cell in a region (sim coords).
    /// Render-side only (the floor is walkable; collision is unaffected).
    fn terrain_fill(&mut self, _x0: i64, _y0: i64, _x1: i64, _y1: i64, _type_id: i64) {}

    /// Autotile-paint the flat floor from the terrain types set so far,
    /// blending boundaries with the registered transition sheets. `base_type`
    /// fills cells outside the set region. Render-side only.
    fn terrain_blit(&mut self, _base_type: i64) {}

    // --- HUD / UI (screen-space overlay, render-side only) ----------------
    // The map describes its HUD each `tick` in immediate mode: `ui_clear`
    // then a fresh set of `ui_image` / `ui_text` / `ui_button` calls, in
    // screen points from the top-left (`ui_width`/`ui_height` give the
    // viewport for anchoring). The host draws it over the scene each frame.
    // All render-side; the defaults ignore it (headless / oracle draw no HUD).

    /// Register a UI texture from an `assets/` PNG; returns an id, or `-1`.
    fn ui_texture(&mut self, _asset_path: &str) -> i64 {
        -1
    }
    /// Register an animated HUD image from a `.gif` (a talking portrait);
    /// returns an id (separate space from `ui_texture`), or `-1`. Draw it with
    /// [`ui_anim`](Self::ui_anim); the host cycles its frames by wall-clock.
    fn ui_gif(&mut self, _asset_path: &str) -> i64 {
        -1
    }
    /// Draw animated image `gif` (from [`ui_gif`](Self::ui_gif)) at `(x, y)` —
    /// its current frame this instant.
    fn ui_anim(&mut self, _gif: i64, _x: i64, _y: i64) {}
    /// Viewport width / height in screen points (for anchoring), or `0`.
    fn ui_width(&self) -> i64 {
        0
    }
    fn ui_height(&self) -> i64 {
        0
    }
    /// Uniform scale applied to every HUD texture + text this frame (positions
    /// stay as the map gives them — it lays out at scaled sizes). `1` = native
    /// pixel size; `2` = double. Set per frame before the draws.
    fn ui_scale(&mut self, _factor: Fixed) {}
    /// Begin a fresh HUD frame (drop the previous widget list).
    fn ui_clear(&mut self) {}
    /// Draw texture `tex` with its top-left at `(x, y)`.
    fn ui_image(&mut self, _tex: i64, _x: i64, _y: i64) {}
    /// Draw texture `tex` clipped to the left `frac` (0..1) of its width — the
    /// health-bar-style fill.
    fn ui_image_clip(&mut self, _tex: i64, _x: i64, _y: i64, _frac: Fixed) {}
    /// Draw `text` (white, `size`-pt) with its top-left at `(x, y)`.
    fn ui_text(&mut self, _x: i64, _y: i64, _text: &str, _size: i64) {}
    /// Draw word-wrapped `text` within `width` points, in `0xRRGGBB` `color`
    /// (dialogue paragraphs).
    #[allow(clippy::too_many_arguments)]
    fn ui_text_wrap(
        &mut self,
        _x: i64,
        _y: i64,
        _text: &str,
        _size: i64,
        _width: i64,
        _color: i64,
    ) {
    }
    /// Draw an image button (`tex` normal / `hover` / `pressed`) at `(x, y)`.
    /// When clicked, the host OR-s `button_bit` into the next input command's
    /// button mask, so the map handles it in `command` like any button.
    #[allow(clippy::too_many_arguments)]
    fn ui_button(
        &mut self,
        _tex: i64,
        _hover: i64,
        _pressed: i64,
        _x: i64,
        _y: i64,
        _button_bit: i64,
    ) {
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
    /// Explicit nav blockers (building footprints, props) — an overlay the
    /// pathfinder ANDs with the heightfield walk rule. Deterministic sim
    /// state by the same argument as `tops`: fed only by command-driven
    /// script calls, so every peer holds the same set.
    nav_blocked: BTreeSet<(i64, i64)>,
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

    /// Cut column `(x, y)` down: everything at and above sim-`z` becomes
    /// air (the store is a heightmap — it can truncate a column, never
    /// hole-punch it). Only ever lowers; a column cut below 0 reverts to
    /// the unpainted default. Returns the previous top so a renderer can
    /// clear exactly the span that was solid, or `None` if the column was
    /// never painted.
    pub fn clear_above(&mut self, x: i64, y: i64, z: i64) -> Option<i64> {
        let prev = self.tops.get(&(x, y)).copied();
        if let Some(top) = prev {
            let new_top = top.min(z - 1);
            if new_top < 0 {
                self.tops.remove(&(x, y));
            } else {
                self.tops.insert((x, y), new_top);
            }
        }
        prev
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

    /// Mark / clear cell `(x, y)` as explicitly impassable for navigation
    /// (building footprint, prop) regardless of its height.
    pub fn nav_block(&mut self, x: i64, y: i64, on: bool) {
        if on {
            self.nav_blocked.insert((x, y));
        } else {
            self.nav_blocked.remove(&(x, y));
        }
    }

    /// A deterministic path from cell `(x0, y0)` to `(x1, y1)` under the
    /// shared walk rule (|Δ`ground_height`| ≤ `max_step` per step, nav
    /// blockers impassable, no corner cutting) — `monada-nav`'s budgeted
    /// A*. Waypoints are cell coordinates with `z` = the cell's ground
    /// height; empty when already there. An unreachable goal yields the
    /// best-effort path toward the closest reachable cell (never an
    /// error). Searches inside the painted world's bounding box.
    #[must_use]
    pub fn nav_path(&self, x0: i64, y0: i64, x1: i64, y1: i64, max_step: i64) -> Vec<FixedVec3> {
        let mut keys = self.tops.keys().chain(self.nav_blocked.iter());
        let Some(&first) = keys.next() else {
            return Vec::new(); // nothing painted, nowhere to walk
        };
        let bounds = keys.fold((first.0, first.1, first.0, first.1), |b, &(x, y)| {
            (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y))
        });
        let limits = monada_nav::NavLimits {
            max_step,
            bounds,
            // Generous for RTS-scale maps (a 96×96 field is ~9k cells) yet
            // a hard ceiling on a sim tick's worst case.
            budget: 20_000,
        };
        monada_nav::astar(self, (x0, y0), (x1, y1), &limits)
            .into_iter()
            .map(|(x, y)| {
                FixedVec3::new(
                    Fixed::from_int(i32::try_from(x).unwrap_or(0)),
                    Fixed::from_int(i32::try_from(y).unwrap_or(0)),
                    Fixed::from_int(i32::try_from(self.ground_height(x, y)).unwrap_or(0)),
                )
            })
            .collect()
    }
}

impl monada_nav::NavWorld for VoxelStore {
    fn height(&self, x: i64, y: i64) -> i64 {
        self.ground_height(x, y)
    }
    fn blocked(&self, x: i64, y: i64) -> bool {
        self.nav_blocked.contains(&(x, y))
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
    fn voxel_clear(&mut self, x: i64, y: i64, z: i64) {
        self.terrain.clear_above(x, y, z);
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
    fn nav_block(&mut self, x: i64, y: i64, on: bool) {
        self.terrain.nav_block(x, y, on);
    }
    fn nav_path(&self, x0: i64, y0: i64, x1: i64, y1: i64, max_step: i64) -> Vec<FixedVec3> {
        self.terrain.nav_path(x0, y0, x1, y1, max_step)
    }
    // Tiles are render-only, but `tile_fill` still feeds collision like
    // `voxel_fill`, so headless terrain matches the textured live map.
    fn tile_fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, _tile: i64) {
        self.terrain.fill(x0, y0, z0, x1, y1, z1);
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
