//! [`Host`] — the surface a map's rules call, independent of the language
//! they are written in (docs/plans/desert-game.md §3a, decision L1/L7).
//!
//! Today the same surface exists twice in spirit: once as the closures
//! `monada-script` registers into a Rhai engine, and once as the
//! [`HostBridge`] the native host implements. This trait is the single
//! definition both runtimes call, so a Rust map's rules and a Rhai map's
//! script reach identical engine behaviour by construction rather than by
//! two implementations agreeing.
//!
//! **Why `&self` and not `&mut self`.** A host is a *handle* to the
//! runtime, not the runtime itself: every implementation holds shared
//! state (`Arc<Mutex<World>>`, the bridge) exactly as the Rhai
//! registration already does, and several handles may be live at once
//! (the sim layer and the local layer both hold one). Mutation belongs to
//! the runtime behind the handle, so the methods that change hashed state
//! take `&self` and lock. This also keeps the surface directly
//! expressible as a wasm import table, where a handle is an index and
//! `&mut` has no meaning (§3f).

use monada_fixed::{Fixed, FixedVec3};
use monada_sim::{ArchetypeId, EntityId};

use monada_nav::{MoverProfile, VolumeLimits};

use crate::{MaterialId, SharedBridge, SharedNav, SharedPhysics, SharedTerrain, SharedWorld};

/// What **both** layers may do: read the world, and draw.
///
/// The split below is the determinism wall expressed as types. The Rhai
/// runtime gets the same guarantee by registering a different function
/// list per scope; here the sim layer receives a [`Host`] — which has no
/// input queries at all — and the local layer a [`LocalHost`] — which has
/// no mutators and no RNG. Neither can reach the other's half by
/// accident, and a rules author does not have to remember which is which.
pub trait WorldRead {
    /// An entity's position, or the zero vector.
    fn entity_position(&self, entity: EntityId) -> FixedVec3;
    /// Read a named field, or zero.
    fn entity_field(&self, entity: EntityId, name: &str) -> Fixed;
    /// Every entity, in a defined order.
    fn entities(&self) -> Vec<EntityId>;
    /// The entities of one archetype, ascending.
    fn entities_of(&self, archetype: ArchetypeId) -> Vec<EntityId>;

    /// The render / input seam, when one is attached. `None` on a
    /// headless peer (the oracle, a dedicated server), where presentation
    /// verbs are no-ops by design — rules must therefore treat drawing as
    /// optional and never let a bridge's absence change hashed state.
    fn bridge(&self) -> Option<&SharedBridge>;

    /// The column terrain store — the **runtime's**, not the bridge's, so
    /// what a map may walk on does not depend on whether this peer draws.
    fn terrain(&self) -> Option<&SharedTerrain>;

    // --- collision queries -----------------------------------------------
    //
    // Deterministic reads over the terrain the map painted: a pure
    // function of its own paint calls, so every peer answers identically
    // and a `tick()` may act on them. Available to both layers — the
    // simulation gates movement on them, the local layer aims the cursor
    // with them.

    /// Whether a cell is solid.
    fn voxel_solid(&self, x: i64, y: i64, z: i64) -> bool {
        self.terrain()
            .is_some_and(|t| t.lock().expect("terrain mutex").solid(x, y, z))
    }

    /// The highest solid `z` in a column, or `0`.
    fn ground_height(&self, x: i64, y: i64) -> i64 {
        self.terrain()
            .map_or(0, |t| t.lock().expect("terrain mutex").ground_height(x, y))
    }

    /// A deterministic A\* path as cell-centre waypoints; `[]` when
    /// unreachable (docs/plans/rts-demo.md §1a).
    fn nav_path(&self, from: (i64, i64), to: (i64, i64), max_step: i64) -> Vec<FixedVec3> {
        self.terrain().map_or_else(Vec::new, |t| {
            t.lock()
                .expect("terrain mutex")
                .nav_path(from.0, from.1, to.0, to.1, max_step)
        })
    }

    // --- presentation ----------------------------------------------------
    //
    // These forward to the bridge and default to nothing when there is
    // none, which is what makes a headless run of the same rules produce
    // the same hashed state as a rendering one. They carry default bodies
    // so an implementation only has to answer `bridge()`; a runtime that
    // reaches the host another way (the wasm import table) overrides them.

    /// Define a sprite model from a KV6 asset; returns its model id, or
    /// `-1` with no bridge.
    fn model_kv6(&self, asset_path: &str, turns: i64) -> i64 {
        self.bridge().map_or(-1, |b| {
            b.lock().expect("bridge mutex").model_kv6(asset_path, turns)
        })
    }

    /// Define a procedural box sprite; returns its model id, or `-1`.
    fn model_box(&self, w: i64, h: i64, d: i64, color: i64) -> i64 {
        self.bridge().map_or(-1, |b| {
            b.lock().expect("bridge mutex").model_box(w, h, d, color)
        })
    }

    /// Bind an entity to a render model.
    fn entity_set_model(&self, entity: EntityId, model: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .entity_set_model(entity_arg(entity), model);
        }
    }

    /// Aim the camera at a sim-space point.
    fn camera_focus(&self, point: FixedVec3) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").camera_focus(point);
        }
    }

    /// Set the camera's orbit angles (radians).
    fn camera_angle(&self, yaw: Fixed, pitch: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").camera_angle(yaw, pitch);
        }
    }

    /// Set the camera's distance from its focus.
    fn camera_dist(&self, dist: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").camera_dist(dist);
        }
    }

    /// Show only the sim-z band `lo..=hi`, cutting the ceiling above it
    /// away — the depth slider a map with tunnels is viewed through
    /// (docs/plans/desert-game.md §4f).
    fn deck_clip(&self, lo: i64, hi: i64) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").deck_clip(lo, hi);
        }
    }

    /// Set an entity's facing yaw (radians). On a KV6 model this turns
    /// the geometry; on a billboard actor it picks the facing sprite.
    fn entity_set_facing(&self, entity: EntityId, yaw: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .entity_set_facing(entity_arg(entity), yaw);
        }
    }

    /// Declare the directional "sun".
    fn set_light(&self, dir: FixedVec3, intensity: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").set_light(dir, intensity);
        }
    }

    /// Load a sky panorama from an asset.
    fn set_sky(&self, asset_path: &str) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").set_sky(asset_path);
        }
    }

    /// Set the HUD status line.
    fn status(&self, text: &str) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").status(text);
        }
    }
}

/// The **simulation** layer's surface: everything in [`WorldRead`] plus
/// the verbs that change hashed state. Deliberately has no way to observe
/// input — a tick may never depend on where this client's cursor is.
///
/// Grows verb by verb as the native backend's maps need them; the Rhai
/// registration in `monada-script` remains the exhaustive list until the
/// migration finishes (docs/plans/desert-game.md D-0).
pub trait Host: WorldRead {
    /// Declare an archetype with the given field names; returns its id.
    fn archetype(&self, fields: &[&str]) -> ArchetypeId;
    /// Spawn an entity of `archetype`.
    fn entity_create(&self, archetype: ArchetypeId) -> EntityId;
    /// Remove an entity; returns whether it existed.
    fn entity_despawn(&self, entity: EntityId) -> bool;
    /// Set an entity's position (sim cells).
    fn entity_set_position(&self, entity: EntityId, pos: FixedVec3);
    /// Set a named fixed-point field.
    fn entity_set_field(&self, entity: EntityId, name: &str, value: Fixed);
    /// A fixed-point value in `[0, 1)` from the world's seeded generator.
    fn rng01(&self) -> Fixed;
    /// An integer in `0..n` from the world's seeded generator. Unsigned
    /// where the script surface says `i64`: a Rhai map has one numeric
    /// type, compiled rules do not, and the conversion belongs at the
    /// language boundary rather than in the world.
    fn rng_below(&self, n: u64) -> u64;

    // --- world painting ---------------------------------------------------
    //
    // Colour is presentation, but SOLIDITY is not: what these paint is
    // what the collision queries above answer, so they belong to the
    // simulation layer and run headless. Each writes the runtime's store
    // first and then hands the same call to the bridge to draw — one
    // place, so the two can never drift apart.

    /// Fill a solid box of voxels (sim cells). Colour is
    /// `0xBB_RR_GG_BB` — the high byte is brightness, not alpha.
    fn voxel_fill(&self, lo: (i64, i64, i64), hi: (i64, i64, i64), color: i64) {
        if let Some(t) = self.terrain() {
            t.lock()
                .expect("terrain mutex")
                .fill(lo.0, lo.1, lo.2, hi.0, hi.1, hi.2);
        }
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .voxel_fill(lo.0, lo.1, lo.2, hi.0, hi.1, hi.2, color);
        }
    }

    /// Set one voxel.
    fn voxel_set(&self, x: i64, y: i64, z: i64, color: i64) {
        if let Some(t) = self.terrain() {
            t.lock().expect("terrain mutex").set(x, y, z);
        }
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").voxel_set(x, y, z, color);
        }
    }

    /// Cut a column down: everything at and above `z` becomes air.
    fn voxel_clear(&self, x: i64, y: i64, z: i64) {
        if let Some(t) = self.terrain() {
            t.lock().expect("terrain mutex").clear_above(x, y, z);
        }
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").voxel_clear(x, y, z);
        }
    }

    /// Mark or clear a cell as impassable for navigation (building
    /// footprints, props) — an overlay the pathfinder ANDs with the
    /// heightfield walk rule.
    fn nav_block(&self, x: i64, y: i64, on: bool) {
        if let Some(t) = self.terrain() {
            t.lock().expect("terrain mutex").nav_block(x, y, on);
        }
    }

    // --- volume terrain ---------------------------------------------------
    //
    // A `terrain = "volume"` map (the desert game — decision L5) keeps its
    // ground in the chunked 3D store instead of the column heightmap, so
    // tunnels, overhangs and undercuts are first-class rather than
    // unrepresentable. The verbs below are the same three shapes — fill,
    // carve, ask — against that store, and each mirrors its paint to the
    // bridge so the screen keeps up.
    //
    // `None` when the map declared no volume terrain, which is what makes
    // calling these on a column map a quiet no-op rather than a panic.

    /// The volume terrain + physics state, when the map has any.
    fn volume(&self) -> Option<&SharedPhysics> {
        None
    }

    /// Fill a solid box in the volume store with `material`, and paint it
    /// on screen.
    fn volume_fill(
        &self,
        lo: (i64, i64, i64),
        hi: (i64, i64, i64),
        material: MaterialId,
        color: i64,
    ) {
        if let Some(p) = self.volume() {
            let mut sim = p.lock().expect("physics mutex");
            sim.terrain
                .fill(lo.0, lo.1, lo.2, hi.0, hi.1, hi.2, material);
            // P6: the solver caches per-chunk occupancy, so an edit has to
            // announce itself or a sleeping body keeps colliding with
            // terrain that is no longer there.
            sim.world.notify_terrain_edit(lo, hi);
        }
        self.invalidate_nav((lo.0, lo.1), (hi.0, hi.1));
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .voxel_fill(lo.0, lo.1, lo.2, hi.0, hi.1, hi.2, color);
        }
    }

    /// Punch out ONE cell — the tunnel primitive the column store could
    /// only fake by truncating a whole column.
    fn volume_clear(&self, x: i64, y: i64, z: i64) {
        if let Some(p) = self.volume() {
            let mut sim = p.lock().expect("physics mutex");
            sim.terrain.clear(x, y, z);
            sim.world.notify_terrain_edit((x, y, z), (x, y, z));
        }
        self.invalidate_nav((x, y), (x, y));
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").voxel_clear(x, y, z);
        }
    }

    /// Whether the volume store holds a solid cell — the deterministic
    /// terrain read on a volume map, where the column `voxel_solid`
    /// answers an empty world by design.
    fn volume_solid(&self, x: i64, y: i64, z: i64) -> bool {
        self.volume().is_some_and(|p| {
            p.lock()
                .expect("physics mutex")
                .terrain
                .get(x, y, z)
                .is_some()
        })
    }

    // --- navigation -------------------------------------------------------

    /// The stand-graph caches, one per mover profile
    /// (docs/plans/desert-game.md §4c). `None` on a map with no volume
    /// terrain to navigate.
    fn nav(&self) -> Option<&SharedNav> {
        None
    }

    /// Drop the navigation stands of every column in the box.
    ///
    /// Called by the paint verbs themselves, which is the point: whoever
    /// changes the ground invalidates what was derived from it. Left to
    /// the map, every terraforming verb would be a place to forget, and a
    /// stale stand is not a visible bug — it is a unit walking
    /// confidently through a wall raised two seconds ago.
    fn invalidate_nav(&self, lo: (i64, i64), hi: (i64, i64)) {
        if let Some(nav) = self.nav() {
            nav.lock().expect("nav mutex").invalidate(lo, hi);
        }
    }

    /// A deterministic path through the volume world for a mover of this
    /// profile: waypoints after `from`, up to and including the goal, or
    /// the closest reachable stand when the goal is not.
    ///
    /// The cache behind it is the engine's, not the map's, and that is
    /// deliberate: the runtime owns the terrain, so it owns what is
    /// derived from it, and a paint can invalidate the affected columns
    /// without the rules having to remember to.
    fn nav_path3(
        &self,
        from: (i64, i64, i64),
        to: (i64, i64, i64),
        profile: MoverProfile,
        limits: &VolumeLimits,
    ) -> Vec<(i64, i64, i64)> {
        let (Some(nav), Some(phys)) = (self.nav(), self.volume()) else {
            return Vec::new();
        };
        let sim = phys.lock().expect("physics mutex");
        nav.lock()
            .expect("nav mutex")
            .for_profile(profile)
            .path(&sim.terrain, from, to, limits)
    }

    /// The stand a mover of this profile would occupy at `(x, y)` nearest
    /// ground height `z` — how a world position enters the graph.
    fn nav_stand(
        &self,
        x: i64,
        y: i64,
        z: i64,
        profile: MoverProfile,
        z_range: (i64, i64),
    ) -> Option<i64> {
        let (Some(nav), Some(phys)) = (self.nav(), self.volume()) else {
            return None;
        };
        let sim = phys.lock().expect("physics mutex");
        nav.lock()
            .expect("nav mutex")
            .for_profile(profile)
            .stand_at(&sim.terrain, x, y, z, z_range)
            .map(|s| s.z)
    }
}

/// The **local** layer's surface: everything in [`WorldRead`] plus input,
/// selection and command submission — the per-client half, none of which
/// is hashed. It cannot mutate the world or draw from the shared RNG,
/// which is what makes "the local layer can never desync a match" a
/// property of the type rather than a review note.
///
/// Selection lives here and nowhere else: what this player has clicked is
/// a fact about this client, so the simulation is not offered it at all.
pub trait LocalHost: WorldRead {
    /// Whether a `button` action is held.
    fn action_down(&self, id: &str) -> bool;
    /// An `axis` action's value: `-1`, `0` or `+1`.
    fn action_axis(&self, id: &str) -> i64;
    /// An `axis2` action's value as `(x, y)`.
    fn action_axis2(&self, id: &str) -> (i64, i64);
    /// The cursor's ground point, or `None` on a miss.
    fn pick_ground(&self) -> Option<FixedVec3>;
    /// The entity under the cursor, or `None`.
    fn pick_entity(&self) -> Option<EntityId>;
    /// The sim-space angle from the local player toward the cursor.
    fn aim_yaw(&self) -> Fixed;
    /// HUD button bits clicked since the last call (take-and-clear).
    fn ui_clicks(&self) -> i64;
    /// The local player's id, or `None` when there is no single one
    /// (hotseat, where one window drives every side).
    fn local_player(&self) -> Option<i64>;
    /// Queue a command for the host to route into the tick stream — the
    /// only channel from this layer into the simulation.
    fn submit_command(&self, verb: u32, target: EntityId, arg: FixedVec3);

    /// Mark an entity as locally selected (replaces the selection).
    fn highlight(&self, entity: EntityId);
    /// Add an entity to the selection (multi-select).
    fn highlight_add(&self, entity: EntityId);
    /// Clear the local selection.
    fn highlight_clear(&self);
    /// The (first) selected entity, or `None`.
    fn highlighted(&self) -> Option<EntityId>;
}

/// Entity ids cross the bridge as the script surface's `i64`.
#[allow(clippy::cast_possible_wrap)]
fn entity_arg(entity: EntityId) -> i64 {
    entity.0 as i64
}

/// The inverse: a bridge's `i64`, with the script's `-1` sentinel read as
/// "nothing".
#[allow(clippy::cast_sign_loss)]
fn entity_opt(id: i64) -> Option<EntityId> {
    (id >= 0).then_some(EntityId(id as u64))
}

/// The [`Host`] implementation over monada's own runtime state: the
/// shared [`World`](monada_sim::World) plus an optional render bridge.
///
/// Cheap to clone (it is a bundle of handles), which is what lets the
/// Rhai registration capture one per closure and the native backend hold
/// one for the whole match.
#[derive(Clone)]
pub struct RuntimeHost {
    world: SharedWorld,
    bridge: Option<SharedBridge>,
    terrain: SharedTerrain,
    volume: Option<SharedPhysics>,
    /// The stand graphs derived from the volume terrain. Created with the
    /// volume, because the two are one fact seen twice.
    nav: Option<SharedNav>,
}

impl RuntimeHost {
    /// A host over `world`, with no render bridge (headless) and a fresh
    /// terrain store.
    #[must_use]
    pub fn new(world: SharedWorld) -> RuntimeHost {
        RuntimeHost {
            world,
            bridge: None,
            terrain: crate::shared_terrain(),
            volume: None,
            nav: None,
        }
    }

    /// A host sharing an existing terrain store — how the local layer
    /// gets the same ground the simulation walks on.
    #[must_use]
    pub fn with_terrain(world: SharedWorld, terrain: &SharedTerrain) -> RuntimeHost {
        RuntimeHost {
            world,
            bridge: None,
            terrain: terrain.clone(),
            volume: None,
            nav: None,
        }
    }

    /// The terrain store this host reads and paints.
    #[must_use]
    pub fn terrain_store(&self) -> &SharedTerrain {
        &self.terrain
    }

    /// Attach the render / input bridge. Call before `init`, matching
    /// `RhaiBackend::set_bridge`'s contract.
    pub fn set_bridge(&mut self, bridge: &SharedBridge) {
        self.bridge = Some(bridge.clone());
    }

    /// Attach the volume terrain + physics sim a `terrain = "volume"`
    /// map runs on. Call before `init`, like the bridge.
    pub fn set_volume(&mut self, phys: &SharedPhysics) {
        self.volume = Some(phys.clone());
        self.nav.get_or_insert_with(crate::shared_nav);
    }

    /// Share an existing set of stand graphs — how the local layer plans
    /// over the same ground, and the same cache, the simulation does.
    pub fn set_nav(&mut self, nav: &SharedNav) {
        self.nav = Some(nav.clone());
    }

    /// The stand graphs this host plans with.
    #[must_use]
    pub fn nav_cache(&self) -> Option<&SharedNav> {
        self.nav.as_ref()
    }

    /// The shared world this host mutates.
    #[must_use]
    pub fn world(&self) -> &SharedWorld {
        &self.world
    }
}

impl WorldRead for RuntimeHost {
    fn entity_position(&self, entity: EntityId) -> FixedVec3 {
        self.world
            .lock()
            .expect("world mutex")
            .position(entity)
            .unwrap_or(FixedVec3::ZERO)
    }

    fn entity_field(&self, entity: EntityId, name: &str) -> Fixed {
        self.world
            .lock()
            .expect("world mutex")
            .field(entity, name)
            .unwrap_or(Fixed::ZERO)
    }

    fn entities(&self) -> Vec<EntityId> {
        self.world.lock().expect("world mutex").all_entities()
    }

    fn entities_of(&self, archetype: ArchetypeId) -> Vec<EntityId> {
        self.world
            .lock()
            .expect("world mutex")
            .entities(archetype)
            .to_vec()
    }

    fn bridge(&self) -> Option<&SharedBridge> {
        self.bridge.as_ref()
    }

    fn terrain(&self) -> Option<&SharedTerrain> {
        Some(&self.terrain)
    }
}

impl Host for RuntimeHost {
    fn volume(&self) -> Option<&SharedPhysics> {
        self.volume.as_ref()
    }

    fn nav(&self) -> Option<&SharedNav> {
        self.nav.as_ref()
    }

    fn archetype(&self, fields: &[&str]) -> ArchetypeId {
        self.world
            .lock()
            .expect("world mutex")
            .register_archetype(fields)
    }

    fn entity_create(&self, archetype: ArchetypeId) -> EntityId {
        self.world.lock().expect("world mutex").spawn(archetype)
    }

    fn entity_despawn(&self, entity: EntityId) -> bool {
        self.world.lock().expect("world mutex").despawn(entity)
    }

    fn entity_set_position(&self, entity: EntityId, pos: FixedVec3) {
        self.world
            .lock()
            .expect("world mutex")
            .set_position(entity, pos);
    }

    fn entity_set_field(&self, entity: EntityId, name: &str, value: Fixed) {
        self.world
            .lock()
            .expect("world mutex")
            .set_field(entity, name, value);
    }

    fn rng01(&self) -> Fixed {
        self.world.lock().expect("world mutex").rng.next_fixed_01()
    }

    fn rng_below(&self, n: u64) -> u64 {
        self.world.lock().expect("world mutex").rng.gen_below(n)
    }
}

/// Every local verb forwards to the bridge, because every one of them is
/// about *this client*: what it is holding down, where its cursor is,
/// what it has selected. With no bridge there is no client, so the reads
/// answer "nothing" and the writes are dropped — which is exactly what a
/// headless peer should observe.
impl LocalHost for RuntimeHost {
    fn action_down(&self, id: &str) -> bool {
        self.bridge()
            .is_some_and(|b| b.lock().expect("bridge mutex").action_down(id))
    }

    fn action_axis(&self, id: &str) -> i64 {
        self.bridge()
            .map_or(0, |b| b.lock().expect("bridge mutex").action_axis(id))
    }

    fn action_axis2(&self, id: &str) -> (i64, i64) {
        self.bridge()
            .map_or((0, 0), |b| b.lock().expect("bridge mutex").action_axis2(id))
    }

    fn pick_ground(&self) -> Option<FixedVec3> {
        self.bridge()
            .and_then(|b| b.lock().expect("bridge mutex").pick_ground())
    }

    fn pick_entity(&self) -> Option<EntityId> {
        self.bridge()
            .and_then(|b| entity_opt(b.lock().expect("bridge mutex").pick_entity()))
    }

    fn aim_yaw(&self) -> Fixed {
        self.bridge().map_or(Fixed::ZERO, |b| {
            b.lock().expect("bridge mutex").aim_yaw()
        })
    }

    fn ui_clicks(&self) -> i64 {
        self.bridge()
            .map_or(0, |b| b.lock().expect("bridge mutex").ui_clicks())
    }

    fn local_player(&self) -> Option<i64> {
        self.bridge()
            .and_then(|b| b.lock().expect("bridge mutex").local_player())
    }

    fn submit_command(&self, verb: u32, target: EntityId, arg: FixedVec3) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").submit_command(
                i64::from(verb),
                entity_arg(target),
                arg,
            );
        }
    }

    fn highlight(&self, entity: EntityId) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .highlight(entity_arg(entity));
        }
    }

    fn highlight_add(&self, entity: EntityId) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .highlight_add(entity_arg(entity));
        }
    }

    fn highlight_clear(&self) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").highlight_clear();
        }
    }

    fn highlighted(&self) -> Option<EntityId> {
        self.bridge()
            .and_then(|b| entity_opt(b.lock().expect("bridge mutex").highlighted()))
    }
}
