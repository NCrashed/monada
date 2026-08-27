//! The monada script **runtime**: the engine-side API surface map rules
//! call into (DESIGN.md §3.3, §5), the world services behind it, and the
//! contracts every runtime implements.
//!
//! This crate deliberately knows no scripting language. It holds the
//! [`ScriptBackend`] contract, the [`HostBridge`] render/input seam, and
//! the deterministic world services a map's rules query
//! ([`VoxelStore`], [`VolumeStore`]) — so a second backend (native Rust
//! rules today, wasm next) shares one definition of the host API with
//! `monada-script`'s Rhai backend instead of reimplementing it
//! (docs/plans/desert-game.md §3a, decision L7).
//!
//! Determinism: all gameplay state lives in the
//! [`World`](monada_sim::World) and these services (decision A2), every
//! one of which is a pure function of the map's own deterministic calls.
//! Sim math is `monada-fixed`; no IEEE arithmetic reaches hashed state.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedQuat, FixedVec3};
use monada_sim::{Command, PlayerId, World};

mod granular;
mod host;
mod native;
mod nav;
mod physics;
mod snapshot;
mod volume;

pub use granular::{Granular, Repose, Slide};
pub use host::{Host, LocalHost, RuntimeHost, WorldRead};
pub use native::{LocalLayer, LocalRules, MapRules, NativeBackend, NativeLocalBackend};
pub use nav::{shared_nav, NavCache, SharedNav};
pub use physics::{shared_physics, DrillToolDef, PhysicsSim, SharedPhysics};
// The navigation vocabulary a map speaks, re-exported so a rules crate
// needs no direct monada-nav edge.
pub use monada_nav::{MoverProfile, Stand, VolumeLimits};
pub use snapshot::SNAPSHOT_VERSION;
// Re-exported because it is part of VolumeStore's own API surface
// (`set`/`fill`/`get` speak MaterialId) — consumers of the store should
// not need a direct monada-physics edge for the id newtype.
pub use monada_physics::MaterialId;
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
/// hull turns in place instead of swinging about a corner); 14 =
/// `model_character` (rigged `.rkc` voxel characters — real geometry
/// with a skeleton and named clips, the 3D counterpart of
/// `model_actor`'s billboards; it reuses `entity_set_anim` /
/// `entity_set_facing` / `model_drop`, so nothing else moved); 15 =
/// `grid_spawn_cubic` (a `grid_spawn` grid whose CELLS ARE CUBES —
/// `SCALE³` world voxels — instead of the column convention's
/// `SCALE×SCALE×1`, so sim z scales like x/y inside it). Additive: the
/// 3-arg `grid_spawn` keeps the column cell exactly, and the cell shape
/// is per-grid, so every verb that reads a grid (`voxel_fill_in`,
/// `grid_pivot`, the seats of entities bound with `entity_set_grid`,
/// `deck_clip`, the fog) follows the grid it was given and a map written
/// against v7..v14 is byte-unaffected. Why it exists: sim→world on a
/// column cell is the ANISOTROPIC `diag(-SCALE, SCALE, -1)`, and
/// conjugating a rotation by a non-uniform diagonal yields a rotation
/// only about z — so on a column grid only yaw is exact, and a map
/// cannot convert coordinates between a tilted hull and the world
/// (docs/plans/grid-entities.md §6). A cubic cell makes that map a
/// similarity transform, so any 3D orientation is exact; 16 = dynamic
/// grid membership (docs/plans/grid-entities.md §3): `grid_move` /
/// `grid_despawn` complete a grid's lifecycle, `grid_world` /
/// `grid_local` convert a point between a grid's frame and the world,
/// `entity_attach` / `entity_detach` move an entity between those frames
/// WITHOUT moving it in the world, `entity_grid` / `grid_riders` read
/// the membership back, and `voxel_set_in` / `voxel_clear_in` give
/// `voxel_fill_in` its one-cell and its inverse. Additive: the raw
/// `entity_set_grid` keeps its v12 meaning (bind, re-reading the
/// position in the new frame), because changing it would be a breaking
/// bump — the converting pair is new verbs beside it; 17 = `camera_grid`
/// (the camera rides a grid's rotation). Found by playing the ship demo:
/// a crew member bound to a hull has a grid-LOCAL position, so with a
/// world-fixed camera the map's view-relative input steered in the
/// ship's frame while the player looked at the world's — "forward"
/// changed direction every tick as the hull turned. Riding the grid
/// aligns the two and costs the map nothing but the one call; 18 = the
/// granular and volume-read verbs of the desert game's D-3 slice
/// (docs/plans/desert-game.md §4d): `granular_register` / `settle` /
/// `settling` declare a material's angle of repose and let the map pace
/// its collapse, and `volume_material` / `volume_top` read back *what* a
/// cell is made of and where a column ends. Native-surface only so far —
/// no Rhai verb exposes them yet — but the manifest contract is the
/// map's, not the language's, so the number moves. Additive twice over:
/// a map that declares nothing granular leaves the automaton inert, and
/// an inert automaton contributes nothing to the digest, so every
/// existing golden holds; 19 = the HUD canvas on the shared host surface
/// — `ui_size` / `ui_clear` / `ui_text` / `ui_button` forward to the
/// bridge's immediate-mode canvas, so a native map draws a sidebar
/// (docs/plans/desert-game.md §7, D-5) the same way a Rhai map already
/// could. Additive and presentation-only: they are no-ops without a
/// bridge, so a headless peer runs the identical rules and draws
/// nothing; 20 = what playing D-5 asked for. The volume READS —
/// `volume` / `volume_solid` / `volume_material` / `volume_top` — moved
/// from `Host` down to `WorldRead`, so the local layer can resolve the
/// cursor against the same ground the simulation walks on; a placement
/// preview that judged the terrain by its own copy of the rules would
/// eventually disagree with the answer. The verbs that CHANGE the store
/// stayed on `Host`, so the determinism wall is where it was. Beside
/// them, `grid_overlay` / `overlay_fill` / `overlay_clear`: a grid of
/// the map's own for things that are shown rather than simulated — a
/// build ghost, a range ring, a rally marker — real geometry rather
/// than HUD pixels, because "where will this go" is a question about
/// the ground. Additive: the reads gained a caller, not a meaning; 21 =
/// freeform body shapes (docs/plans/ship-physics.md S-2). `phys_box`
/// spawns the one body a map could describe in a call — a solid block —
/// and a hull is a shell, whose inertia tensor is not a block's at all.
/// `phys_shape` / `phys_shape_fill` / `phys_shape_clear` / `phys_body`
/// let a map author a body out of the same cell boxes it paints the
/// hull's voxels with, so mass, centre of mass, inertia and collision
/// skin all derive from the geometry the player can see; `phys_mass`
/// reads the result back. Additive: the shape table is authoring
/// scratch outside the sim, and a map that never opens one is
/// bit-unaffected; 22 = `grid_body` (docs/plans/ship-physics.md D2–D4):
/// a `grid_spawn` grid can be BOUND to a physics body, after which its
/// frame is no longer something the map computes — the engine copies
/// the body's pose into the sim-side frame table and the render mirror
/// after every physics step, the grid's pivot becomes the body's centre
/// of mass, and the body stops being auto-mirrored because the painted
/// grid is already its picture. Everything that rides a frame — riders,
/// props, actor facings, fog, the deck cutaway, the camera — follows
/// with no change at all, which is the whole point: a ship becomes a
/// rigid body without touching what it means to walk around inside one.
/// Additive: a grid nobody binds is posed by the map exactly as before;
/// 23 = engines (docs/plans/ship-physics.md D6/D7). `phys_thrust` fires
/// a force for one tick at a point in the hull's OWN frame — so an
/// off-centre mount turns the ship, and a thruster keeps pushing along
/// the hull as the hull turns — `phys_torque` applies the pure couple
/// an off-centre impulse cannot express (a gyro, an RCS quad), and
/// `phys_angvel` reads the tumble back so a map can write its own
/// stabiliser as `τ = −k·ω`. No new hashed state and no engine-side
/// notion of a thruster: fuel, throttle, gimbal and which key fires it
/// stay in the map; 24 = the cursor learns about grids, and a map learns
/// to draw a line (docs/plans/ship-building.md). `pick_grid` /
/// `pick_cell` / `pick_face` resolve the cursor ray against the SCENE's
/// voxels rather than a ground plane, and answer in the cells of the
/// grid that was hit — so a map whose whole world is a moving hull can
/// name the deck cell under the pointer at any attitude, and the deck
/// cutaway redirects the hit to the deck the player can actually see.
/// `gizmo_style` / `gizmo_box` / `gizmo_line` draw alpha-blended
/// world-space outlines in a grid's frame: a placement ghost, a snap
/// lattice, a range ring — things that are shown rather than simulated,
/// in three dimensions, where `ui_*` is flat and a voxel is opaque and
/// cell-sized. Additive and local-layer only: the simulation is never
/// offered any of it, and a headless peer no-ops the lot;
/// 25 = `entity_set_side` (box/door/engine orientation).
/// `entity_set_facing`'s yaw only turns a model about the vertical axis —
/// enough for a walking crew member, not for an object that can point
/// along any of a hull's 6 grid faces. `entity_set_side` takes a discrete
/// face plus a quarter-turn roll around it instead, 24 orientations
/// total rather than a continuous angle, so a script can snap a placed
/// object to the grid the way it already snaps position. Scoped to the
/// geometry-turning KV6/box path `entity_set_facing` uses for a plain
/// model; a billboard actor still only answers to yaw. Additive: an
/// entity nobody sides keeps whatever facing (or none) it already had;
/// 26 = `model_box_sides` (orientation debugging). A box built by
/// `model_box` is one flat colour, so a script that turns one with
/// `entity_set_side` cannot tell from the render alone whether the turn
/// actually happened — only that *something* did or didn't get posed.
/// `model_box_sides` paints each local face its own colour so a rotation
/// is visible on sight, the same way an authored `.kv6` would be.
/// Debug/demo tooling, not a new render capability: identical geometry to
/// `model_box`, just per-face colour instead of one colour throughout.
/// 27 = the tileset, actor and audio verbs reach a NATIVE map. They have
/// existed on `HostBridge` since the Rhai days, but were never lifted into
/// `WorldRead`/`Host` when `monada-runtime` was split out, so a compiled
/// map could paint a tileset or animate a billboard only by locking
/// `bridge()` and going round the typed surface the split exists to
/// provide. Rhai-scripted maps could do both all along — which made
/// "the runtime is swappable" untrue in the one direction that mattered.
/// Additive: every method carries a default over `bridge()`, the Rhai
/// registrations are untouched, and no existing map's rendering changes.
/// `tile_fill` lands on `Host` rather than `WorldRead` because it feeds
/// collision as well as the eye, so it writes the terrain store first,
/// exactly like `voxel_fill`.
/// 28 = `set_shadows`: real cast shadows on a COLUMN map. The dynamic
/// light rig (sun + baked ambient + stylized shadows) was gated on
/// `terrain = "volume"`, so a heightmap map got per-face `side_shades`
/// only, where a shape reads by its own facets and nothing casts onto
/// anything else. Opt-in rather than a default switch, because chess, the
/// RPG and the RTS were all tuned against `side_shades`.
/// 29 = `set_sprite_facing`: billboard actors aligned to the VIEW PLANE
/// rather than each aimed at the camera position. Eye-facing turns a card
/// as it drifts off the middle of the screen -- under a 90 degree field of
/// view one at the edge stands about 45 degrees away from one in the
/// centre -- which is right for a card standing in for a volume and wrong
/// for art drawn to be seen flat. Needs roxlap's
/// `BillboardMode::CylindricalViewPlane` + `set_actor_mode`. Off by
/// default: every map before this was drawn against the eye-facing look.
/// 30 = `camera_fov`: the horizontal field of view, fixed at 90 degrees
/// until now (`OpticastSettings::hz = xres/2`). Narrowing it and pulling
/// the camera back by the same factor is how a perspective raycaster
/// imitates an orthographic one, which is the look a 2D-sprite game wants.
/// The default is unchanged, so no existing map's framing moves.
/// 31 = fog of war for a PARTY. `FogOfWar` took one observer with a facing
/// cone -- a first-person model, a crew member looking down a corridor.
/// A strategy map is the other shape: several units, each seeing all round
/// itself, ground staying explored once anyone walked it. roxlap grows
/// `update_many`; here the observer becomes a list
/// (`vision_observer_add` / `vision_observer_clear`), and the whole vision
/// family is lifted into `WorldRead`, which a native map could not reach
/// at all before.
/// 32 = `vision_shroud`: never-seen ground drawn opaque black rather than
/// treated as air. Transparent is the first-person reading -- inside a
/// hull, unexplored floor is a hole and the ray hits the wall behind it.
/// Outdoors that hole shows the SKY through the ground, which reads as
/// missing terrain rather than unknown terrain. Both render backends;
/// off by default.
/// 33 = selection reaches a native map: `highlighted_all`, `drag_begin`
/// and `drag_end` lifted into `LocalHost`. A compiled map could select one
/// entity and read one back, so a drag box and multi-select -- the whole
/// Warcraft III control scheme -- were script-only.
/// 34 = `set_sky_color`: the flat background a ray that hits nothing lands
/// on. Fixed at a daylight blue until now, which shows through unexplored
/// ground on a fogged outdoor map -- the fog's known twin only holds
/// chunks something has been seen in, so ground nobody has been near has
/// no geometry to shroud.
/// 35 = `tile_relief` + `cell_voxels`: a column map may paint a cell whose
/// surface is not flat. One height per cell is a sixteen-voxel step, so a
/// gentle hill still read as a ziggurat; now the eye gets a sub-column
/// height per voxel while the feet keep the cell the heightfield store,
/// `ground_height` and `nav_path` all speak. Only the MAP can interpolate
/// that surface, since only it knows which steps are cliffs to keep sharp.
pub const HOST_API_VERSION: u32 = 35;

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

/// The shared column terrain store: the highest solid sim-z per column
/// plus the navigation blocker overlay. Owned by the RUNTIME, not by the
/// render bridge — what a map may walk on is simulation state, and a
/// headless peer must answer it identically to a drawing one
/// (docs/plans/desert-game.md §3a; the smell docs/plans/rts-demo.md
/// flagged as "bridge-owned determinism state").
pub type SharedTerrain = Arc<Mutex<VoxelStore>>;

/// Convenience: a fresh shared terrain store.
#[must_use]
pub fn shared_terrain() -> SharedTerrain {
    Arc::new(Mutex::new(VoxelStore::new()))
}

/// Convenience: a fresh shared world seeded for its RNG.
#[must_use]
pub fn shared_world(seed: u64) -> SharedWorld {
    Arc::new(Mutex::new(World::new(seed)))
}

/// A scripting backend: compile a script, then drive it through the
/// engine's trigger entry points. Implemented by `RhaiBackend`
/// (`monada-script`) in v0; a native-Rust and then a wasm backend land
/// beside it (§5.5, docs/plans/desert-game.md §3b).
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

    /// Carry the pose of every body-bound grid (`grid_body`) into the
    /// frame table and the render mirror (docs/plans/ship-physics.md
    /// D2). Called by whoever steps physics, immediately AFTER the step
    /// and before anything reads a frame — a tick order that matters:
    /// the script's `tick` runs first and may push a hull about, physics
    /// integrates that, and only then is the frame the drawn world hangs
    /// off it true.
    ///
    /// The default does nothing, which is right for a backend with no
    /// physics and for one where no grid is bound.
    fn sync_grid_bodies(&mut self) {}
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
    /// Define a procedural box sprite model with each of its 6 local
    /// faces painted a distinct colour (`x`/`neg_x`/`y`/`neg_y`/`z`/
    /// `neg_z`, in the box's own local axes — the same local X/Y/Z
    /// [`entity_set_side`](Self::entity_set_side) rotates). A debug/demo
    /// aid: it makes an orientation visible without relying on
    /// directional shading alone to tell one side from another. Returns
    /// its model id.
    #[allow(clippy::too_many_arguments)]
    fn model_box_sides(
        &mut self,
        w: i64,
        h: i64,
        d: i64,
        x: i64,
        neg_x: i64,
        y: i64,
        neg_y: i64,
        z: i64,
        neg_z: i64,
    ) -> i64;
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

    /// Move one already-painted voxel from one sim cell to another — the
    /// render half of a settling step (docs/plans/desert-game.md §4d).
    ///
    /// Not a clear plus a fill, for two reasons. The colour is not the
    /// caller's to supply: the automaton moved a cell of *whatever was
    /// there*, and the render grid is the thing that knows what colour
    /// that was. And a clear would seed the debris puff a carve does,
    /// which is right for a drill and wrong for a dune finding its
    /// angle — sand sliding is not sand exploding. The default ignores
    /// it, so a headless peer settles identically and draws nothing.
    fn voxel_slide(&mut self, _from: (i64, i64, i64), _to: (i64, i64, i64)) {}

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

    /// Like [`grid_spawn`](Self::grid_spawn), but the grid's CELLS ARE CUBES:
    /// one sim cell is `SCALE³` world voxels, so sim z scales exactly like x/y
    /// inside it (a plain `grid_spawn` grid keeps the column convention's
    /// `SCALE×SCALE×1` cell, whose z is unscaled). The spawn offset `(wx, wy,
    /// wz)` is in sim cells, as before — only its z now scales too.
    ///
    /// Use this whenever the map converts coordinates between the grid and the
    /// world, or turns the grid about anything but the vertical: sim→world on a
    /// column cell is the anisotropic `diag(-SCALE, SCALE, -1)`, so a sim-space
    /// rotation survives it only about z. On a cubic grid the map is a
    /// similarity transform, so ANY orientation is exact and a grid-local point
    /// and a world point are the same point (docs/plans/grid-entities.md §6).
    ///
    /// The cell shape is a property of the GRID, so every verb that reads one
    /// follows it: [`voxel_fill_in`](Self::voxel_fill_in) paints cubes,
    /// [`grid_pivot`](Self::grid_pivot) reads its point in the cubic frame,
    /// entities bound with [`entity_set_grid`](Self::entity_set_grid) seat with
    /// scaled z, and [`deck_clip`](Self::deck_clip) / the fog cut on cell
    /// boundaries. Vertical geometry is therefore CELL-QUANTISED here: a wall is
    /// a whole number of cells tall, not an arbitrary number of voxels.
    ///
    /// Render-side, like `grid_spawn`. The default returns `-1` (no grid
    /// allocated), so a map must check the handle exactly as it does there
    /// (`host_api` 15).
    fn grid_spawn_cubic(&mut self, _wx: i64, _wy: i64, _wz: i64) -> i64 {
        -1
    }

    /// Paint a solid voxel box into a specific grid (by id from
    /// [`grid_spawn`](Self::grid_spawn)), in sim coordinates. Same
    /// coordinate convention as [`voxel_fill`](Self::voxel_fill) but
    /// render-side only — does NOT update the collision store.
    ///
    /// The cell's SHAPE follows the grid: a `grid_spawn` grid keeps the column
    /// convention (`SCALE×SCALE×1` voxels — z unscaled, so a cell is a thin
    /// slab), a [`grid_spawn_cubic`](Self::grid_spawn_cubic) grid makes it a
    /// `SCALE³` cube. The default ignores it.
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
    /// [`grid_pivot`](Self::grid_pivot) point.
    ///
    /// EXACTNESS: on a [`grid_spawn_cubic`](Self::grid_spawn_cubic) grid that
    /// sim→world map is a similarity transform, so the rendered turn IS the
    /// sim-space turn the script asked for, whatever the axis. On a column-cell
    /// `grid_spawn` grid only the yaw part is scale-exact (z is unscaled there,
    /// so the map is anisotropic): a tilted axis renders as an honest world
    /// rotation about the mapped axis, but it is not the sim rotation, and a map
    /// cannot convert coordinates through it. The default ignores it.
    fn grid_orient(&mut self, _grid: i64, _axis: FixedVec3, _angle: Fixed) {}

    /// Move a `grid_spawn` grid to sim-space `origin` — a hull under way, a
    /// platform on its track. Replaces the offset `grid_spawn` placed it at;
    /// unlike that verb's integer cells this is fixed-point, so a hull can
    /// drift a fraction of a cell per tick. Entities bound to the grid, its fog
    /// and its `deck_clip` ride the new placement, exactly as they ride
    /// [`grid_orient`](Self::grid_orient). Render-side, not hashed; an
    /// out-of-range or despawned handle is ignored (`host_api` 16). The default
    /// ignores it.
    fn grid_move(&mut self, _grid: i64, _origin: FixedVec3) {}

    /// Bind a `grid_spawn` grid to a physics body (`grid_body`,
    /// docs/plans/ship-physics.md D2/D4), or release it with a negative body.
    ///
    /// Two things follow render-side. The grid's pose stops being something
    /// the map writes and starts arriving from [`grid_pose`](Self::grid_pose)
    /// after every physics step. And the body stops being mirrored
    /// automatically: a bound body's picture is the grid the map painted —
    /// its plating, its doors, its stair brass — not the material-coloured
    /// block the mirror would draw over it. Render-side, not hashed
    /// (`host_api` 22); the default ignores it.
    fn grid_body(&mut self, _grid: i64, _body: i64) {}

    /// Set a `grid_spawn` grid's whole pose at once: `origin` in sim cells
    /// (like [`grid_move`](Self::grid_move)) and `rot` as a quaternion.
    ///
    /// The engine calls this for a [`grid_body`](Self::grid_body)-bound grid
    /// once per tick, carrying the body's pose. A quaternion rather than
    /// `grid_orient`'s axis/angle because the pose already IS one: routing a
    /// solver-exact attitude through `from_axis_angle` and back would round it
    /// twice every tick, and the sim-side frame table would drift from the
    /// drawn one. Not part of a map's own vocabulary — a script poses a grid
    /// with `grid_move` / `grid_orient` (`host_api` 22). The default ignores
    /// it.
    fn grid_pose(&mut self, _grid: i64, _origin: FixedVec3, _rot: FixedQuat) {}

    /// Retire a `grid_spawn` grid: its voxels leave the scene and its handle
    /// dies. Handles are never reused, so a stale one stays inert rather than
    /// addressing a later grid.
    ///
    /// Riders are NOT despawned — entity lifetime is hashed sim state and
    /// belongs to the map, so a vanishing render frame must never kill crew.
    /// They are detached keeping their world pose, as if the map had called
    /// [`entity_detach`] on each; a map that wants them to go down with the
    /// ship despawns them itself. Render-side (`host_api` 16); the default
    /// ignores it.
    ///
    /// [`entity_detach`]: Self::entity_set_grid
    fn grid_despawn(&mut self, _grid: i64) {}

    /// Make the camera RIDE a `grid_spawn` grid: its whole orbit frame — the eye
    /// offset and the view basis — is turned by that grid's rotation, so the
    /// grid holds still on screen and the world turns around it. `-1` (or a dead
    /// handle) puts it back in the world frame.
    ///
    /// A map with a moving hull wants this for a reason beyond the view: an
    /// entity bound to a grid has a grid-LOCAL position, so a map that reads
    /// input relative to the camera is steering in the ship's frame while
    /// looking at the world's — and "forward" becomes a different direction
    /// every tick as the hull turns. Riding the grid re-aligns the two, and the
    /// map's own movement math needs no change. Render-side, never hashed
    /// (`host_api` 17). The default ignores it.
    fn camera_grid(&mut self, _grid: i64) {}

    /// Paint ONE cell of a specific grid — [`voxel_fill_in`](Self::voxel_fill_in)
    /// with both corners at the same place, spelled out because a single cell is
    /// what a door or a prop edits (`host_api` 16). The default ignores it.
    fn voxel_set_in(&mut self, _grid: i64, _x: i64, _y: i64, _z: i64, _color: i64) {}

    /// Erase one cell of a specific grid — `voxel_fill_in`'s inverse, and the
    /// door / hull-breach primitive: until this existed a grid's voxels could be
    /// painted but never taken back. Render-side only, like every `*_in` verb: a
    /// dynamic grid still feeds no collision store, so a map that opens a door
    /// must also open its own passability rule (`host_api` 16). The default
    /// ignores it.
    fn voxel_clear_in(&mut self, _grid: i64, _x: i64, _y: i64, _z: i64) {}

    /// Name the grid-local point [`grid_orient`](Self::grid_orient) turns a
    /// `grid_spawn` grid about, in SIM cells — the frame `voxel_fill_in` paints
    /// in (so on a [`grid_spawn_cubic`](Self::grid_spawn_cubic) grid the point's
    /// z scales with x/y, like everything else there), so a hull spanning cells
    /// `0..=19` turns about its middle at `9.5`.
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
    /// Add another entity the fog sees through, so what is visible is the
    /// union of them all. Adding the same entity twice is one observer.
    ///
    /// [`vision_observer`](Self::vision_observer) still means "these are
    /// the observers", replacing the list, so a map that names one keeps
    /// today's behaviour. A party or a side adds the rest.
    ///
    /// All observers share one mask, and a mask belongs to one grid: the
    /// grid is derived from the FIRST observer. Render-side only.
    fn vision_observer_add(&mut self, _entity: i64) {}

    /// Nobody sees. Everything visible demotes to memory, which is what a
    /// side with no units left should be looking at. Render-side only.
    fn vision_observer_clear(&mut self) {}

    /// Draw never-seen ground as opaque black rather than treating it as
    /// air. Off by default.
    ///
    /// Off is the first-person reading: you are inside a hull, unexplored
    /// floor is a hole, and the ray carries on to the wall behind it. Seen
    /// from above and outdoors that same hole shows the **sky** through
    /// the ground, which reads as missing terrain rather than as unknown
    /// terrain — so an outdoor map wants Warcraft III's black mask, and
    /// says so here. Render-side only.
    fn vision_shroud(&mut self, _opaque: bool) {}

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
    /// The script handle of the grid whose voxels the cursor ray meets
    /// first, or `-1` for a miss (`grid_spawn` handles; the world grid a
    /// map paints terrain into answers `-1` — ask for it as `-1` below).
    ///
    /// Unlike [`pick_ground`](Self::pick_ground) this is a question about
    /// the geometry that exists rather than about a plane: a map whose
    /// world is one moving hull has no ground plane to speak of.
    fn pick_grid(&self) -> i64 {
        -1
    }
    /// The **sim cell** of the cursor's first solid hit inside `grid`, in
    /// that grid's own cells (cube or column, whichever it was spawned
    /// with); `-1` names the world grid. `None` when the cursor misses,
    /// or hits some other grid — a map asks about the hull it cares
    /// about and gets a straight answer about that hull.
    ///
    /// Clip-aware: voxels the grid's deck cutaway
    /// ([`deck_clip`](Self::deck_clip)) hides read as air, so the cursor
    /// lands on the deck the player is looking into rather than on the
    /// roof that was cut away.
    fn pick_cell(&self, _grid: i64) -> Option<FixedVec3> {
        None
    }
    /// The outward face normal of that same hit, in `grid`'s sim axes: a
    /// unit vector along one axis, so `pick_cell + pick_face` is the
    /// empty cell in front of the surface — the difference between
    /// putting a crate ON the floor and INTO the wall.
    fn pick_face(&self, _grid: i64) -> Option<FixedVec3> {
        None
    }

    // --- overlay gizmos ---------------------------------------------------
    //
    // World-space outlines drawn over the frame, in a grid's own frame
    // and its own cells: a placement ghost, a snap lattice, a range ring.
    // Immediate mode — `gizmo_clear`, then a fresh set, exactly like the
    // HUD canvas — and alpha-blended, which neither of the map's other
    // drawing surfaces can be: `ui_*` is flat HUD pixels, and a voxel is
    // opaque (its high byte is brightness) and a whole cell across.
    //
    // Belong to the local layer in practice, like `ui_*`, and are
    // registered only there; they live on the bridge with the rest of
    // the presentation verbs because they are no-ops without one.

    /// Start a fresh set of gizmos: everything drawn before this is
    /// gone, and the style resets. The map's to call — what it drew last
    /// stays on screen through the frames between its ticks, which is
    /// the same contract [`ui_clear`](Self::ui_clear) has.
    fn gizmo_clear(&mut self) {}
    /// Line width in pixels and depth behaviour for the gizmos drawn
    /// after it: `on_top` segments ignore the depth buffer (a highlight
    /// that shows through the hull), the rest are occluded by nearer
    /// geometry. State, like `ui_scale`.
    fn gizmo_style(&mut self, _width_px: i64, _on_top: bool) {}
    /// Outline the inclusive cell box `(x0,y0,z0)..=(x1,y1,z1)` of
    /// `grid` (`-1` = the world frame). `color` is `0xAA_RR_GG_BB` —
    /// here the high byte really is **alpha**, unlike a voxel's.
    #[allow(clippy::too_many_arguments)] // a cell box is six numbers
    fn gizmo_box(
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
    /// One segment between two sim-space points of `grid`'s frame
    /// (`-1` = the world frame), same colour packing as
    /// [`gizmo_box`](Self::gizmo_box).
    fn gizmo_line(&mut self, _grid: i64, _a: FixedVec3, _b: FixedVec3, _color: i64) {}

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

    /// Ask for real cast shadows from the `set_light` sun, `strength` deep
    /// (`0..1`; `0` turns them off again). Off by default.
    ///
    /// A `terrain = "volume"` map already lights through the dynamic rig
    /// and only tunes the depth here. A column map is the reason this verb
    /// exists: it has always taken the legacy per-face shading, where a
    /// shape reads by its own facets but nothing casts onto anything else.
    /// Opting in rather than switching by default keeps chess, the RPG and
    /// the RTS looking as they were tuned to look. Render-side only; the
    /// default ignores it.
    fn set_shadows(&mut self, _strength: Fixed) {}

    /// Align billboard actors to the **view plane** instead of aiming each
    /// at the camera position. Off by default.
    ///
    /// Aiming at the eye turns a card as it drifts off the middle of the
    /// screen: under a 90° field of view one at the edge stands about 45°
    /// away from one in the centre, so a sprite that looked square-on
    /// stands part side-on once the camera pans. A card standing in for a
    /// volume should turn -- it really is at that bearing. A sprite drawn
    /// to be seen flat should not, which is what Doom and Build did.
    ///
    /// Off by default because every map before `host_api` 29 was drawn
    /// against the eye-facing look. Render-side only.
    fn set_sprite_facing(&mut self, _view_plane: bool) {}

    /// Set the horizontal field of view in degrees. The host's default is
    /// 90; values outside `1..=170` are ignored.
    ///
    /// Narrowing this and pulling [`camera_dist`](Self::camera_dist) back
    /// by the same factor is how a perspective renderer imitates an
    /// orthographic one: the tighter and further the cone, the less an
    /// object's on-screen size depends on its depth, until the scene reads
    /// flat. The framing is unchanged if the two move together, since what
    /// fills the frame is `dist · tan(fov/2)`.
    ///
    /// It stays an imitation. True orthographic projection means parallel
    /// primary rays, which is a change to how both backends build a ray,
    /// not a projection constant. Render-side only.
    fn camera_fov(&mut self, _degrees: Fixed) {}

    /// Load a sky panorama from an `assets/` image and render it behind the
    /// scene. Render-side only.
    fn set_sky(&mut self, asset_path: &str);

    /// Set the flat background colour a ray that hits nothing lands on,
    /// as `0x00RR_GGBB`. The host's default is a daylight blue.
    ///
    /// Worth setting to black on a fogged outdoor map. Where fog of war is
    /// on, the renderer draws a *known twin* of the terrain — a copy that
    /// gains a chunk only once some of it has been seen — so ground nobody
    /// has been near yet has no geometry to shroud, and the background
    /// shows through it. A background the same colour as the shroud makes
    /// the twin's edge invisible instead of a moving hole.
    ///
    /// A panorama from [`set_sky`](Self::set_sky) covers this whenever one
    /// is loaded; this is the colour behind it. Render-side only.
    fn set_sky_color(&mut self, _color: i64) {}

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

    /// Define a rigged, animated **character** model from an `.rkc` asset in
    /// the map archive (`roxlap-formats`' character container: voxel meshes +
    /// a skeleton + named animation clips). Real geometry, not a billboard —
    /// it turns in world space instead of swapping a pre-drawn facing, so a
    /// steep camera sees it in proper perspective (`host_api` 14).
    ///
    /// `height_cells` is the rendered height in sim cells, measured over the
    /// character's **first clip** (its idle) so a sprawling death pose can't
    /// shrink the walking character; pass `0` (or less) to keep the artist's
    /// scale, one model voxel per world voxel. Returns a model id to bind
    /// with [`entity_set_model`](Self::entity_set_model), or `-1` if the asset
    /// is missing or unparsable.
    ///
    /// The animation states are the character's own clip names, selected per
    /// entity with [`entity_set_anim`](Self::entity_set_anim); facing comes
    /// from [`entity_set_facing`](Self::entity_set_facing), grounding is
    /// nudged with [`model_drop`](Self::model_drop). Whether a clip loops or
    /// holds its last frame is baked into the clip's own keyframe sequence —
    /// unlike [`model_actor`](Self::model_actor), where the host picks the
    /// loop mode by state name — so a death that replays forever is an
    /// authoring fix, not a script one. Render-side only; the default
    /// ignores it.
    fn model_character(&mut self, _asset_path: &str, _height_cells: Fixed) -> i64 {
        -1
    }

    /// Nudge a model's sprites down (`cells` > 0) or up (< 0) by that many
    /// cells, on top of the pivot-computed grounding. Lets a map correct art
    /// whose visible feet aren't at the trimmed opaque bottom (e.g. a baked
    /// shadow) without re-authoring the GIFs — or lift a hovering `.rkc`
    /// character off the floor. Applies to actor and character models.
    /// Render-side only.
    fn model_drop(&mut self, _model: i64, _cells: Fixed) {}

    /// Set an entity's current animation state by name: one of the `states`
    /// given to [`model_actor`](Self::model_actor), or one of the clip names
    /// baked into the [`model_character`](Self::model_character) `.rkc`. An
    /// unknown name leaves the current animation playing. Render-side only.
    fn entity_set_anim(&mut self, _entity: i64, _state: &str) {}

    /// Set an entity's facing yaw in sim radians (`atan2(dy, dx)`): an actor
    /// picks the matching directional sprite, a character turns its geometry.
    /// Render-side only.
    fn entity_set_facing(&mut self, _entity: i64, _yaw: Fixed) {}

    /// Set an entity's axis-aligned side and roll (`host_api` 24): a
    /// discrete alternative to [`entity_set_facing`](Self::entity_set_facing)
    /// for a model that should snap to a grid face rather than turn
    /// continuously — a crate, a door, an engine bell. `dir` picks which of
    /// the 6 faces points forward, `roll` turns that face's own axis in one
    /// of 4 quarter-turns, 24 orientations total. `dir`/`roll` are the
    /// `monada_script::Direction`/`Roll` enums' `u8` discriminants crossing
    /// the host-API wall as plain ints, since this crate does not depend on
    /// `monada-script` and cannot name the types themselves. Turns the KV6/
    /// box model's geometry the same way `entity_set_facing` does; a
    /// billboard actor has no roll to show and ignores it. Render-side only.
    fn entity_set_side(&mut self, _entity: i64, _dir: i64, _roll: i64) {}

    /// Tint an actor entity's sprite by an `0x00RR_GGBB` colour multiply
    /// (`0x00FF_FFFF` = white = no tint; e.g. `0x00FF_4040` = damage red).
    /// Render-side only — flash a hit without touching the hashed sim.
    /// Billboard actors only: roxlap has no per-character tint, so a
    /// [`model_character`](Self::model_character) entity ignores it.
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

    /// World voxels across one sim cell in x/y — the host's sim→world
    /// scale. `0` where there is no renderer to ask.
    ///
    /// A column map needs this to talk about anything finer than a cell:
    /// it is the length of the `tops` slice
    /// [`tile_relief`](Self::tile_relief) wants, squared.
    fn cell_voxels(&self) -> i64 {
        0
    }

    /// Paint ONE cell whose surface is not flat: each of the cell's `s × s`
    /// sub-columns runs from `floor` up to `tops[ly * s + lx]`, where `s`
    /// is [`cell_voxels`](Self::cell_voxels). Shorter slices leave the rest
    /// of the cell unpainted.
    ///
    /// **`floor` is not decoration.** A top is where a column ENDS, so a
    /// map that digs a hollow below its datum has columns that must start
    /// lower still or there is nothing under them — a hole through the
    /// world rather than a dip in it. The map picks how deep its basement
    /// goes; assuming zero here would make every hollow a hole.
    ///
    /// **Collision stays a cell.** `walkable` is the one height the
    /// heightfield store, `ground_height`, `nav_path` and the walk rule all
    /// see, exactly as [`tile_fill`](Self::tile_fill) would set it. The
    /// relief is what the eye gets, and it is deliberately allowed to
    /// disagree with what the feet get by a voxel or two — the alternative
    /// is a pathfinder over a million columns.
    ///
    /// This is how a column map stops looking like a staircase. A cell is
    /// sixteen voxels across, so one height per cell is a sixteen-voxel
    /// step, and a hill built out of those reads as a ziggurat however
    /// gentle its slope. Interpolating the surface across the cell — which
    /// only the MAP can do, since only it knows which steps are cliffs it
    /// must keep sharp — costs nothing here: `tile_fill` already walks
    /// these sub-columns to place the tile's colours.
    #[allow(clippy::too_many_arguments)]
    fn tile_relief(
        &mut self,
        _x: i64,
        _y: i64,
        _floor: i64,
        _walkable: i64,
        _tops: &[i64],
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
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
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
    /// A saved game could not be written or read — a corrupt blob, or one
    /// from a build whose [`SNAPSHOT_VERSION`] differs. Distinct from
    /// [`Run`](ScriptError::Run) because nothing about the running map is
    /// wrong: the *file* is.
    Snapshot(String),
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScriptError::Compile(m) => write!(f, "script compile error: {m}"),
            ScriptError::Run(m) => write!(f, "script run error: {m}"),
            ScriptError::Snapshot(m) => write!(f, "snapshot error: {m}"),
        }
    }
}

impl std::error::Error for ScriptError {}
