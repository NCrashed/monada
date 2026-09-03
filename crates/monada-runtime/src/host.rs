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

use crate::{
    MaterialId, PhysicsSim, Repose, SharedBridge, SharedNav, SharedPhysics, SharedTerrain,
    SharedWorld,
};

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

    /// The volume terrain + physics state, when the map has any.
    ///
    /// A **read** available to both layers, for the same reason the
    /// column queries below are: the cursor has to resolve against the
    /// same ground the simulation walks on, or a placement preview shows
    /// the player one thing and the sim does another. The verbs that
    /// *change* it stay on [`Host`].
    fn volume(&self) -> Option<&SharedPhysics> {
        None
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

    /// What a cell is *made of*, or `None` for air.
    ///
    /// The solidity read answers "is there ground"; this answers "whose
    /// ground" — which is the question the transmutative verbs are built
    /// on (§6c): a Binder sinters an enemy's packed fill and leaves raw
    /// sand alone, and a Dweller's spoil has to be the material that came
    /// out of the hole.
    fn volume_material(&self, x: i64, y: i64, z: i64) -> Option<MaterialId> {
        self.volume()
            .and_then(|p| p.lock().expect("physics mutex").terrain.get(x, y, z))
    }

    /// The topmost solid cell of a column and its material.
    ///
    /// One call for what the rules would otherwise ask sixty-four times
    /// scanning down from the sky — and the store answers it by walking
    /// its own chunks, so the cost is the column's height rather than the
    /// world's.
    fn volume_top(&self, x: i64, y: i64) -> Option<(i64, MaterialId)> {
        self.volume().and_then(|p| {
            p.lock()
                .expect("physics mutex")
                .terrain
                .column_top(x, y)
                .map(|(z, mat)| (z, MaterialId(mat)))
        })
    }

    /// The first solid cell the segment `from → to` enters, or `None` if
    /// nothing is in the way.
    ///
    /// **Line of fire, and the reason a berm is worth building** (§7). On
    /// a flat map "can this gun see that tank" is a distance; on a
    /// volumetric one it is a question about the ground between them, and
    /// answering it is what makes a Surfling rampart a firing position, a
    /// Dweller trench cover, and artillery the counter to both.
    ///
    /// Marches the same integer DDA the physics solver uses rather than a
    /// second one of its own — two ray marchers over the same store would
    /// agree everywhere except the corners, and the corners are where a
    /// shot grazes a berm.
    ///
    /// Endpoints are cell centres. A shot from inside solid rock hits at
    /// once, which is the honest answer.
    fn volume_ray(&self, from: (i64, i64, i64), to: (i64, i64, i64)) -> Option<(i64, i64, i64)> {
        let p = self.volume()?;
        let sim = p.lock().expect("physics mutex");
        let centre = |c: (i64, i64, i64)| {
            FixedVec3::new(
                Fixed::from_int(i32::try_from(c.0).unwrap_or(0)) + Fixed::from_ratio(1, 2),
                Fixed::from_int(i32::try_from(c.1).unwrap_or(0)) + Fixed::from_ratio(1, 2),
                Fixed::from_int(i32::try_from(c.2).unwrap_or(0)) + Fixed::from_ratio(1, 2),
            )
        };
        let (a, b) = (centre(from), centre(to));
        let span = b - a;
        let len = span.length();
        if len <= Fixed::ZERO {
            return None;
        }
        let dir = span.scale(Fixed::ONE / len);
        monada_physics::raycast::cast(&sim.terrain, a, dir, len).map(|hit| hit.cell)
    }

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
        self.nav_path_drop(from, to, max_step, max_step)
    }

    /// …and the same for a walker that may drop further than it climbs,
    /// which makes a ledge one-way: off it the short way, back round the
    /// long way.
    fn nav_path_drop(
        &self,
        from: (i64, i64),
        to: (i64, i64),
        max_step: i64,
        max_drop: i64,
    ) -> Vec<FixedVec3> {
        self.terrain().map_or_else(Vec::new, |t| {
            t.lock()
                .expect("terrain mutex")
                .nav_path_drop(from.0, from.1, to.0, to.1, max_step, max_drop)
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

    /// Define a procedural box sprite with each of its 6 local faces a
    /// distinct colour; returns its model id, or `-1`.
    #[allow(clippy::too_many_arguments)]
    fn model_box_sides(
        &self,
        w: i64,
        h: i64,
        d: i64,
        x: i64,
        neg_x: i64,
        y: i64,
        neg_y: i64,
        z: i64,
        neg_z: i64,
    ) -> i64 {
        self.bridge().map_or(-1, |b| {
            b.lock()
                .expect("bridge mutex")
                .model_box_sides(w, h, d, x, neg_x, y, neg_y, z, neg_z)
        })
    }

    /// Define an animated 8-direction billboard actor from GIFs laid out
    /// as `<dir_path>/<state>/<side>.gif`; returns its model id, or `-1`.
    /// `height_cells` is the rendered height in sim cells, so swapping art
    /// resolutions does not change on-screen size.
    ///
    /// Takes `&[&str]` where the bridge takes `&[String]`: naming states is
    /// a literal at every call site, and the one allocation happens once
    /// per model definition rather than per frame.
    fn model_actor(&self, dir_path: &str, states: &[&str], height_cells: Fixed) -> i64 {
        let owned: Vec<String> = states.iter().map(|s| (*s).to_string()).collect();
        self.bridge().map_or(-1, |b| {
            b.lock()
                .expect("bridge mutex")
                .model_actor(dir_path, &owned, height_cells)
        })
    }

    /// Define a rigged `.rkc` character model — real geometry that turns in
    /// world space rather than a pre-drawn facing. Returns its model id, or
    /// `-1`. `height_cells <= 0` keeps the artist's scale.
    fn model_character(&self, asset_path: &str, height_cells: Fixed) -> i64 {
        self.bridge().map_or(-1, |b| {
            b.lock()
                .expect("bridge mutex")
                .model_character(asset_path, height_cells)
        })
    }

    /// Float ONE body above where it stands, in cells. Positive lifts.
    ///
    /// **Per entity, where [`model_drop`](Self::model_drop) is per kind.**
    /// A kind's drop says how its art sits on the ground and is the same
    /// for every one of them; this says where a single body is drawn
    /// relative to its own feet, which is what a hover, a bob or a hop
    /// needs — the same thing in unison across a swarm reads as
    /// clockwork.
    ///
    /// Drawing only. It moves nothing: the body stands, walks, is picked
    /// and is reached exactly where it did, so a map may drive this from
    /// its local layer per client without touching a hashed byte.
    fn entity_set_lift(&self, entity: EntityId, cells: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .entity_set_lift(entity_arg(entity), cells);
        }
    }

    /// Nudge a model's sprites down (`cells > 0`) or up, on top of the
    /// pivot-computed grounding — art whose visible feet are not at its
    /// trimmed bottom, corrected without re-authoring the GIFs.
    fn model_drop(&self, model: i64, cells: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").model_drop(model, cells);
        }
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

    /// Snap an entity to one of the 6 axis-aligned grid faces, with one of
    /// 4 quarter-turns around it. `dir`/`roll` are a `Direction`/`Roll`
    /// discriminant each — this trait stays in `monada-runtime`, so it
    /// takes the plain ints the bridge does, not the script-side enums.
    fn entity_set_side(&self, entity: EntityId, dir: i64, roll: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .entity_set_side(entity_arg(entity), dir, roll);
        }
    }

    /// Select an entity's animation state by name — one of the `states`
    /// given to [`model_actor`](Self::model_actor), or a clip baked into a
    /// [`model_character`](Self::model_character). An unknown name leaves
    /// the current animation playing.
    fn entity_set_anim(&self, entity: EntityId, state: &str) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .entity_set_anim(entity_arg(entity), state);
        }
    }

    /// Tint an actor's sprite by an `0x00RR_GGBB` colour multiply
    /// (`0x00FF_FFFF` = no tint) — a damage flash without touching the
    /// hashed sim. Billboard actors only.
    fn entity_set_tint(&self, entity: EntityId, tint: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .entity_set_tint(entity_arg(entity), tint);
        }
    }

    /// Declare the directional "sun".
    fn set_light(&self, dir: FixedVec3, intensity: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").set_light(dir, intensity);
        }
    }

    /// Ask for real cast shadows from the sun, `strength` deep (`0..1`;
    /// `0` turns them off). A volume map lights through the shadow rig
    /// already and only tunes depth here; a column map takes per-face
    /// shading until it calls this.
    fn set_shadows(&self, strength: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").set_shadows(strength);
        }
    }

    /// Align billboard actors to the view plane rather than aiming each at
    /// the camera position, so a sprite stops turning as it drifts off the
    /// middle of the screen. Off by default.
    fn set_sprite_facing(&self, view_plane: bool) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").set_sprite_facing(view_plane);
        }
    }

    /// Set the horizontal field of view in degrees (default 90). Narrow it
    /// and pull `camera_dist` back by the same factor to approach an
    /// orthographic look at unchanged framing.
    fn camera_fov(&self, degrees: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").camera_fov(degrees);
        }
    }

    /// Set the flat background colour (`0x00RR_GGBB`) a ray that hits
    /// nothing lands on. Black is what a fogged outdoor map wants: the
    /// fog's known twin has no geometry where nobody has been, so the
    /// background shows through unexplored ground.
    fn set_sky_color(&self, color: i64) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").set_sky_color(color);
        }
    }

    /// Load a sky panorama from an asset.
    fn set_sky(&self, asset_path: &str) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").set_sky(asset_path);
        }
    }

    // --- fog of war (per client, never hashed) -----------------------------
    //
    // What one player can see is not simulation state: two peers on the
    // same lockstep stream see different fog and are not desynced, which
    // is the whole reason this lives on the render side.

    /// Declare an entity the fog sees through, replacing whichever were
    /// declared before. Pass a despawned or negative id to clear.
    fn vision_observer(&self, entity: EntityId) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .vision_observer(entity_arg(entity));
        }
    }

    /// [`vision_observer`](Self::vision_observer) with the mask riding a
    /// `grid_spawn` grid (a movable hull) instead of the world grid.
    fn vision_observer_in(&self, entity: EntityId, grid: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .vision_observer_in(entity_arg(entity), grid);
        }
    }

    /// Add another entity the fog sees through: what is visible becomes
    /// the union of them all. Adding the same one twice is one observer.
    fn vision_observer_add(&self, entity: EntityId) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .vision_observer_add(entity_arg(entity));
        }
    }

    /// Nobody sees; everything visible demotes to memory.
    fn vision_observer_clear(&self) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").vision_observer_clear();
        }
    }

    /// Draw never-seen ground as opaque black instead of as air — what an
    /// outdoor map wants, where transparent unexplored ground shows the
    /// sky through it. Off by default.
    fn vision_shroud(&self, opaque: bool) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").vision_shroud(opaque);
        }
    }

    /// Tune the observers' cone, reach and peripheral radius, in cells. A
    /// cone of 360 is an observer who sees all round, which is what a
    /// strategy unit is.
    fn vision_config(&self, cone_deg: i64, range: i64, peripheral: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .vision_config(cone_deg, range, peripheral);
        }
    }

    /// Briefly reveal a cell from a heard sound (`loudness` in `0..1`).
    fn vision_hear(&self, x: i64, y: i64, z: i64, loudness: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .vision_hear(x, y, z, loudness);
        }
    }

    /// Set the HUD status line.
    fn status(&self, text: &str) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").status(text);
        }
    }

    // --- terrain tiles (render-side) --------------------------------------
    //
    // The tileset half of a Warcraft-III-shaped map: per-cell PNG tiles for
    // painted geometry, and a marching-squares autotiled flat floor under
    // it. Everything here is render-only — the one verb that also feeds
    // collision, `tile_fill`, lives on `Host` beside `voxel_fill` for
    // exactly that reason.

    /// Load a per-cell tile texture from an `assets/` PNG; returns its id
    /// for [`tile_fill`](Host::tile_fill), or `-1`.
    fn tile(&self, asset_path: &str) -> i64 {
        self.bridge()
            .map_or(-1, |b| b.lock().expect("bridge mutex").tile(asset_path))
    }

    /// Bake ambient occlusion into the world grid, once the terrain is
    /// painted. `strength` in hundredths (`0` off, `100` black crevices),
    /// `radius` the sampling reach in voxels.
    ///
    /// Under a runtime light rig the baked byte IS the ambient fill, so
    /// without this every surface takes the same flat ambient and the
    /// places where terrain meets terrain have nothing to mark them. A
    /// cast shadow only helps where the sun happens to fall; occlusion
    /// draws the shape.
    fn bake_ao(&self, strength: i64, radius: i64) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").bake_ao(strength, radius);
        }
    }

    /// [`bake_ao`](Self::bake_ao) over one inclusive cell rectangle.
    ///
    /// The bake is written into the voxel colours, so ground repainted
    /// after one comes out unshaded against neighbours that still carry
    /// theirs. On flat terrain there is no occlusion to lose and nothing
    /// shows; on a hill the patch goes flat and bright, which reads as the
    /// paint having failed rather than as the light being stale. Editing
    /// terrain means relighting it, and relighting the whole grid for one
    /// stroke is seconds of work.
    fn bake_ao_in(&self, lo: (i64, i64), hi: (i64, i64), strength: i64, radius: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .bake_ao_in(lo, hi, strength, radius);
        }
    }

    /// World voxels across one sim cell in x/y, or `0` headless. What a
    /// column map needs to say anything finer than a cell.
    fn cell_voxels(&self) -> i64 {
        self.bridge()
            .map_or(0, |b| b.lock().expect("bridge mutex").cell_voxels())
    }

    /// Register a marching-squares transition sheet (a 4×4 PNG) blending
    /// terrain type `high` over `low`; higher id wins. Read by
    /// [`terrain_blit`](Self::terrain_blit).
    fn transition(&self, low: i64, high: i64, asset_path: &str) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .transition(low, high, asset_path);
        }
    }

    /// Set the flat-floor terrain type over a cell region. The floor is
    /// walkable, so this never touches collision.
    fn terrain_fill(&self, lo: (i64, i64), hi: (i64, i64), type_id: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .terrain_fill(lo.0, lo.1, hi.0, hi.1, type_id);
        }
    }

    /// Autotile the flat floor from the types set so far, blending
    /// boundaries with the registered sheets. `base_type` fills everything
    /// outside the region.
    fn terrain_blit(&self, base_type: i64) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").terrain_blit(base_type);
        }
    }

    // --- audio (render-side, never hashed) --------------------------------
    //
    // Triggered from `tick` like `status` and `entity_set_anim`, and like
    // them never part of the world hash: a headless peer no-ops the lot, so
    // a match cannot desync on sound.

    /// How big a HUD texture is, in pixels — `(0, 0)` for an id that names
    /// nothing, and on a headless peer.
    ///
    /// `ui_image` draws at native size, so centring a picture in a frame
    /// takes both sizes. A map that had to hard-code them could not have
    /// its art swapped without a rebuild, which is the whole point of the
    /// asset indirection.
    fn ui_texture_size(&self, tex: i64) -> (i64, i64) {
        self.bridge().map_or((0, 0), |b| {
            b.lock().expect("bridge mutex").ui_texture_size(tex)
        })
    }

    /// Scatter a one-shot burst of particles at `at` — a spell going off,
    /// something breaking.
    ///
    /// `count` pieces leaving at `speed` **cells per second**, each
    /// lasting about `life` seconds, tinted `0xRRGGBB`.
    ///
    /// **Drawing only, like a sound.** Nothing it scatters is hashed, a
    /// headless peer makes none, and no two peers compare them — so a map
    /// may call it from its tick without leaving the determinism contract,
    /// exactly as it calls `play_sound` and `entity_set_anim`.
    fn burst(&self, at: FixedVec3, count: i64, speed: Fixed, life: Fixed, color: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .burst(at, count, speed, life, color);
        }
    }

    /// Play a one-shot sound. Identical sounds fired the same frame are
    /// de-duplicated, so a wave of attackers plays it once, not stacked.
    fn play_sound(&self, asset_path: &str) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").play_sound(asset_path);
        }
    }

    /// [`play_sound`](Self::play_sound) with an explicit gain (`0..1`).
    fn play_sound_gain(&self, asset_path: &str, gain: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .play_sound_gain(asset_path, gain);
        }
    }

    /// Synthesise a short voice blip — the Undertale-style typing sound.
    /// `wave`: 0 square / 1 saw / 2 triangle / 3 sine / 4 noise. No de-dup;
    /// fire one per typed glyph.
    fn play_blip(&self, wave: i64, freq: i64, dur_ms: i64, gain: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .play_blip(wave, freq, dur_ms, gain);
        }
    }

    /// Keep a looping sound audible: call it every tick the loop should
    /// play. A *state* (moving) drives a seamless loop with no per-actor
    /// timer and no restart per frame.
    fn play_loop(&self, asset_path: &str) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").play_loop(asset_path);
        }
    }

    /// Start or replace the background track. Idempotent for the same path.
    fn play_music(&self, asset_path: &str) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").play_music(asset_path);
        }
    }

    /// Stop the background track.
    fn stop_music(&self) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").stop_music();
        }
    }

    // --- the HUD canvas ---------------------------------------------------
    //
    // Immediate mode, redrawn every frame: `ui_clear`, then a fresh set of
    // images, labels and buttons. A sidebar is not state the engine keeps
    // for a map (docs/plans/desert-game.md §7) — it is what the map draws
    // this frame, which is what makes a build list that greys out what you
    // cannot afford a two-line change rather than a widget tree.
    //
    // These belong to the LOCAL layer in practice, but they live here for
    // the same reason the rest of the presentation verbs do: they are
    // no-ops without a bridge, so a headless peer runs the identical rules
    // and draws nothing.

    /// The HUD canvas size, in its own units.
    fn ui_size(&self) -> (i64, i64) {
        self.bridge().map_or((0, 0), |b| {
            let b = b.lock().expect("bridge mutex");
            (b.ui_width(), b.ui_height())
        })
    }

    /// Start a frame's HUD: everything drawn before this is gone.
    fn ui_clear(&self) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").ui_clear();
        }
    }

    /// Pin the next widget over a world point. See [`HostBridge::ui_pin`].
    fn ui_pin(&self, at: FixedVec3) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").ui_pin(at);
        }
    }

    /// Draw a label.
    fn ui_text(&self, x: i64, y: i64, text: &str, size: i64) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").ui_text(x, y, text, size);
        }
    }

    /// …and the same line in `0xAARRGGBB`, alpha and all: a word that
    /// means something by its colour, and fades rather than blinking off.
    fn ui_text_tint(&self, x: i64, y: i64, text: &str, size: i64, tint: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .ui_text_tint(x, y, text, size, tint);
        }
    }

    /// Draw word-wrapped `text` within `width` points, in `0xRRGGBB` —
    /// the dialogue paragraph.
    fn ui_text_wrap(&self, x: i64, y: i64, text: &str, size: i64, width: i64, color: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .ui_text_wrap(x, y, text, size, width, color);
        }
    }

    /// Register a HUD texture from an `assets/` PNG; returns its id, or `-1`.
    fn ui_texture(&self, asset_path: &str) -> i64 {
        self.bridge().map_or(-1, |b| {
            b.lock().expect("bridge mutex").ui_texture(asset_path)
        })
    }

    /// Register an animated HUD image from a `.gif` — a talking portrait.
    /// Its own id space, separate from [`ui_texture`](Self::ui_texture);
    /// returns `-1` on a missing asset.
    fn ui_gif(&self, asset_path: &str) -> i64 {
        self.bridge()
            .map_or(-1, |b| b.lock().expect("bridge mutex").ui_gif(asset_path))
    }

    /// Draw animated image `gif`'s current frame at `(x, y)`.
    fn ui_anim(&self, gif: i64, x: i64, y: i64) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").ui_anim(gif, x, y);
        }
    }

    /// Draw texture `tex` with its top-left at `(x, y)`.
    fn ui_image(&self, tex: i64, x: i64, y: i64) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").ui_image(tex, x, y);
        }
    }

    /// …and the same tinted `0xAARRGGBB` and turned `turn` radians
    /// clockwise about its own centre: one arrow in the assets, painted
    /// and pointed per meaning.
    fn ui_mark(&self, tex: i64, x: i64, y: i64, tint: i64, turn: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").ui_mark(tex, x, y, tint, turn);
        }
    }

    /// Draw `tex` clipped to the left `frac` (`0..1`) of its width — the
    /// health-bar fill.
    fn ui_image_clip(&self, tex: i64, x: i64, y: i64, frac: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .ui_image_clip(tex, x, y, frac);
        }
    }

    /// Uniform scale over every HUD texture and label this frame; positions
    /// stay as given. `1` is native pixel size. Set it before the draws.
    fn ui_scale(&self, factor: Fixed) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").ui_scale(factor);
        }
    }

    // --- overlay geometry -------------------------------------------------
    //
    // A grid of the map's own, painted and repainted freely, for things
    // that are *shown* rather than simulated: a placement ghost, a range
    // ring, a rally marker. Real geometry rather than HUD pixels, because
    // an RTS's "where will this go" has to sit on the ground in three
    // dimensions — a coordinate readout is not an answer.
    //
    // Cubic cells (`grid_spawn_cubic`) so an overlay lines up with a
    // volume map's isotropic ground. Nothing here touches hashed state;
    // on a headless peer it is all no-ops.

    /// Make an overlay grid whose cells are cubes, returning its id.
    fn grid_overlay(&self) -> i64 {
        self.bridge().map_or(-1, |b| {
            b.lock().expect("bridge mutex").grid_spawn_cubic(0, 0, 0)
        })
    }

    /// Paint a box into an overlay grid.
    fn overlay_fill(&self, grid: i64, lo: (i64, i64, i64), hi: (i64, i64, i64), color: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .voxel_fill_in(grid, lo.0, lo.1, lo.2, hi.0, hi.1, hi.2, color);
        }
    }

    /// Rub one cell out of an overlay grid.
    fn overlay_clear(&self, grid: i64, x: i64, y: i64, z: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .voxel_clear_in(grid, x, y, z);
        }
    }

    /// Draw a clickable button from three textures (idle, hover,
    /// pressed); its `bit` comes back through
    /// [`ui_clicks`](LocalHost::ui_clicks).
    fn ui_button(&self, tex: (i64, i64, i64), x: i64, y: i64, bit: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .ui_button(tex.0, tex.1, tex.2, x, y, bit);
        }
    }

    // --- overlay gizmos ---------------------------------------------------
    //
    // The overlay grid above answers "which cells", in whole cells of
    // paint. These answer "what shape", in lines: alpha-blended
    // world-space outlines drawn over the frame, in a grid's own frame,
    // cleared every frame like the HUD. A placement ghost on a *moving*
    // hull is theirs and not the overlay grid's — a grid of voxels has
    // no pose but its own, and a cell is opaque and a whole cell across.

    /// Start a fresh set of gizmos; the map's to call, like `ui_clear`.
    fn gizmo_clear(&self) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").gizmo_clear();
        }
    }

    /// Line width (pixels) and depth behaviour for the gizmos that
    /// follow; state, like `ui_scale`.
    fn gizmo_style(&self, width_px: i64, on_top: bool) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").gizmo_style(width_px, on_top);
        }
    }

    /// Outline an inclusive cell box in `grid`'s frame (`-1` = the world
    /// frame). `color` is `0xAA_RR_GG_BB`, alpha in the high byte.
    fn gizmo_box(&self, grid: i64, lo: (i64, i64, i64), hi: (i64, i64, i64), color: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .gizmo_box(grid, lo.0, lo.1, lo.2, hi.0, hi.1, hi.2, color);
        }
    }

    /// One overlay segment between two sim points of `grid`'s frame.
    fn gizmo_line(&self, grid: i64, a: FixedVec3, b: FixedVec3, color: i64) {
        if let Some(bridge) = self.bridge() {
            bridge
                .lock()
                .expect("bridge mutex")
                .gizmo_line(grid, a, b, color);
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

    /// Fill a box of cells with a tile — the texture's pixels become the
    /// cells' voxel colours.
    ///
    /// This sits here rather than beside [`WorldRead::tile`] because it is
    /// **not** render-only: a textured wall blocks exactly like a painted
    /// one, so it writes the terrain store first and only then hands the
    /// call to the bridge, the same order [`voxel_fill`](Self::voxel_fill)
    /// keeps. Skipping the store would let the eye and the pathfinder
    /// disagree about a wall — the drift that ordering exists to prevent.
    fn tile_fill(&self, lo: (i64, i64, i64), hi: (i64, i64, i64), tile: i64) {
        if let Some(t) = self.terrain() {
            t.lock()
                .expect("terrain mutex")
                .fill(lo.0, lo.1, lo.2, hi.0, hi.1, hi.2);
        }
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .tile_fill(lo.0, lo.1, lo.2, hi.0, hi.1, hi.2, tile);
        }
    }

    /// Paint one cell with a surface that is not flat: each sub-column
    /// runs from `floor` to `tops[ly * s + lx]`, `s` being
    /// [`WorldRead::cell_voxels`].
    ///
    /// `floor` is where the columns START, and a map that digs below its
    /// datum needs it lower still — a top alone leaves a hollow with
    /// nothing under it, which is a hole rather than a dip.
    ///
    /// `walkable` is the single height the store, `ground_height` and
    /// `nav_path` see — the feet keep a cell, only the eye gets the
    /// relief. Which is the trade that makes smooth ground affordable: a
    /// pathfinder over sub-columns would be a different engine.
    fn tile_relief(&self, x: i64, y: i64, floor: i64, walkable: i64, tops: &[i64], tile: i64) {
        if let Some(t) = self.terrain() {
            t.lock()
                .expect("terrain mutex")
                .fill(x, y, floor, x, y, walkable);
        }
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .tile_relief(x, y, floor, walkable, tops, tile);
        }
    }

    /// [`tile_relief`](Self::tile_relief) with a tile per sub-column.
    ///
    /// One tile per cell makes every border between two grounds a
    /// staircase of whole cells. A tile per sub-column puts the border
    /// where the map wants it, sixteen times finer, so a brush can paint a
    /// join instead of snapping it to the grid. `tiles` is indexed like
    /// `tops`; a one-element slice paints the whole cell with it.
    fn tile_relief_mixed(
        &self,
        x: i64,
        y: i64,
        floor: i64,
        walkable: i64,
        tops: &[i64],
        tiles: &[i64],
    ) {
        if let Some(t) = self.terrain() {
            t.lock()
                .expect("terrain mutex")
                .fill(x, y, floor, x, y, walkable);
        }
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .tile_relief_mixed(x, y, floor, walkable, tops, tiles);
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
        self.disturb_terrain((lo.0, lo.1), (hi.0, hi.1));
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
        self.disturb_terrain((x, y), (x, y));
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").voxel_clear(x, y, z);
        }
    }

    // --- granular terrain -------------------------------------------------

    /// Declare a material granular: a slope of it steeper than `repose`
    /// collapses (docs/plans/desert-game.md §4d). A material never
    /// declared is stable at any slope — which is what makes a Surfling's
    /// packed fill and a Binder's glass worth making.
    fn granular_register(&self, material: MaterialId, repose: Repose) {
        if let Some(p) = self.volume() {
            p.lock()
                .expect("physics mutex")
                .granular
                .register(material, repose);
        }
    }

    /// Let disturbed terrain settle, moving at most `budget` cells;
    /// returns how many moved.
    ///
    /// The map decides when and how much, because the answer belongs to
    /// its terraform budget (§4e) rather than to the engine: one knob
    /// paces the collapse, the render re-uploads, and the navigation
    /// invalidation together.
    fn settle(&self, budget: u32) -> u32 {
        let Some(p) = self.volume() else {
            return 0;
        };
        let slides = {
            let mut sim = p.lock().expect("physics mutex");
            let PhysicsSim {
                granular,
                terrain,
                world,
                ..
            } = &mut *sim;
            let slides = granular.settle(terrain, budget);
            // The solver caches per-chunk occupancy, so a sleeping body
            // has to be told the ground moved. One box around the whole
            // slump rather than one call per cell: the call walks every
            // body, and a thousand walks a tick is a worse trade than
            // waking a few bodies that did not strictly need it.
            if let Some((lo, hi)) = bounds(&slides) {
                world.notify_terrain_edit(lo, hi);
            }
            slides
        };
        if slides.is_empty() {
            return 0;
        }

        // Settling is the one thing that reshapes the ground WITHOUT
        // going through a paint verb, so it has to do by hand what
        // `disturb_terrain` does for everything else — and it must not
        // re-disturb, or a slump would keep re-dirtying its own columns
        // and never come to rest.
        if let Some(nav) = self.nav() {
            let mut nav = nav.lock().expect("nav mutex");
            let mut seen = std::collections::BTreeSet::new();
            for slide in &slides {
                for column in [(slide.from.0, slide.from.1), (slide.to.0, slide.to.1)] {
                    if seen.insert(column) {
                        nav.invalidate(column, column);
                    }
                }
            }
        }
        if let Some(b) = self.bridge() {
            let mut b = b.lock().expect("bridge mutex");
            for slide in &slides {
                b.voxel_slide(slide.from, slide.to);
            }
        }
        u32::try_from(slides.len()).unwrap_or(u32::MAX)
    }

    /// How many columns are still slumping — the map's cue that a slope
    /// has not come to rest.
    fn settling(&self) -> usize {
        self.volume()
            .map_or(0, |p| p.lock().expect("physics mutex").granular.pending())
    }

    // --- navigation -------------------------------------------------------

    /// The stand-graph caches, one per mover profile
    /// (docs/plans/desert-game.md §4c). `None` on a map with no volume
    /// terrain to navigate.
    fn nav(&self) -> Option<&SharedNav> {
        None
    }

    /// Tell everything derived from the ground that the ground moved:
    /// the navigation stands go stale, and the columns start slumping.
    ///
    /// Called by the paint verbs themselves, which is the point: whoever
    /// changes the ground invalidates what was derived from it. Left to
    /// the map, every terraforming verb would be a place to forget, and a
    /// stale stand is not a visible bug — it is a unit walking
    /// confidently through a wall raised two seconds ago.
    fn disturb_terrain(&self, lo: (i64, i64), hi: (i64, i64)) {
        if let Some(nav) = self.nav() {
            nav.lock().expect("nav mutex").invalidate(lo, hi);
        }
        if let Some(p) = self.volume() {
            p.lock().expect("physics mutex").granular.disturb(lo, hi);
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
        let mut cache = nav.lock().expect("nav mutex");
        cache.route(profile, &sim.terrain, from, to, limits)
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

    /// Where `entity` is DRAWN this frame, rather than where the tick left
    /// it: the position smoothed across the tick it arrived on.
    ///
    /// **Local only, and that is the point.** A drawn position depends on
    /// how far into a tick this client's frame fell, so two peers disagree
    /// about it by design — which is why it is here and not on
    /// [`WorldRead`], where a `tick` could reach it and desync the match.
    /// What it is for is presentation that has to AGREE with the picture:
    /// a camera easing toward a hero it follows must ease toward the hero
    /// on screen, or the body shakes against a world sliding smoothly
    /// behind it.
    ///
    /// Falls back to [`entity_position`](WorldRead::entity_position) where
    /// there is nothing smoothed to report — a headless peer, a map with no
    /// declared tick rate, an entity nothing draws — so a caller always gets
    /// a usable point.
    fn entity_drawn_position(&self, entity: EntityId) -> FixedVec3 {
        i64::try_from(entity.0)
            .ok()
            .and_then(|id| {
                self.bridge()
                    .and_then(|b| b.lock().expect("bridge mutex").entity_drawn(id))
            })
            .unwrap_or_else(|| self.entity_position(entity))
    }

    /// The grid the cursor ray first meets, or `None` — the question a
    /// map asks when its world is geometry rather than a ground plane.
    fn pick_grid(&self) -> Option<i64>;
    /// The sim cell of that hit inside `grid` (`-1` = the world grid),
    /// or `None` when the cursor misses it. Deck-clip aware.
    fn pick_cell(&self, grid: i64) -> Option<FixedVec3>;
    /// The outward face normal of the same hit, in `grid`'s sim axes.
    fn pick_face(&self, grid: i64) -> Option<FixedVec3>;
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

    /// The whole selection, in the order it was built.
    ///
    /// The default reads only the first, so a runtime that has no
    /// multi-select still answers something sane rather than nothing.
    fn highlighted_all(&self) -> Vec<EntityId> {
        self.bridge().map_or_else(
            || self.highlighted().into_iter().collect(),
            |b| {
                b.lock()
                    .expect("bridge mutex")
                    .highlighted_all()
                    .into_iter()
                    .filter_map(entity_opt)
                    .collect()
            },
        )
    }

    /// Anchor a drag rectangle at the cursor.
    ///
    /// The gesture's anchor lives on the HOST, not here: a Rhai local
    /// layer is stateless and could not carry it between the press and the
    /// release, and the host has to draw the rectangle anyway.
    fn drag_begin(&self) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").drag_begin();
        }
    }

    /// Finish the drag and take its ground quad: four sim-ground corners
    /// wound around the screen-aligned rectangle, so a selection test is
    /// against the box the player actually drew whatever the camera yaw.
    /// Empty if no drag was in progress.
    ///
    /// A quad whose diagonal is tiny is a CLICK rather than a drag — the
    /// caller decides where that threshold sits, since it depends on how
    /// big a cell is on screen.
    fn drag_end(&self) -> Vec<FixedVec3> {
        self.bridge()
            .map(|b| b.lock().expect("bridge mutex").drag_end())
            .unwrap_or_default()
    }

    // --- placement ghosts --------------------------------------------
    //
    // Here rather than on `WorldRead` beside the gizmos, which are local
    // by convention and by nothing stronger. A preview belongs to the eye
    // that is about to click, so the simulation is given no way to reach
    // it at all.

    /// Start a fresh set of ghosts; what was drawn before this is gone.
    /// Immediate mode, on the HUD's contract: clear and reissue.
    fn ghost_clear(&self) {
        if let Some(b) = self.bridge() {
            b.lock().expect("bridge mutex").ghost_clear();
        }
    }

    /// Draw `model` translucently at `pos`, turned to `yaw` radians, at
    /// `alpha` in `0..=255`.
    ///
    /// The model id is the one [`model_kv6`](WorldRead::model_kv6) or
    /// [`model_actor`](WorldRead::model_actor) gave, so a preview draws
    /// exactly what the placement will -- which is the whole point of it,
    /// and what an outline of a bounding box can never be. Ghosts cast no
    /// shadow and cannot be picked.
    ///
    /// An actor preview animates and picks its card from the camera the way
    /// a placed one does, and there is **one at a time**: a cursor previews
    /// one thing, so a second call in a frame replaces the first. Sprites
    /// are not limited that way. A rigged
    /// [`model_character`](WorldRead::model_character) has no preview --
    /// posing a clip is a question nothing has asked.
    fn ghost_model(&self, model: i64, pos: FixedVec3, yaw: Fixed, alpha: i64) {
        if let Some(b) = self.bridge() {
            b.lock()
                .expect("bridge mutex")
                .ghost_model(model, pos, yaw, alpha);
        }
    }

    /// Whether the fog of war is showing `at` to THIS client right now.
    ///
    /// `true` where there is no fog, where the point is outside the fog
    /// grid, and where the cell is lit -- so a map that never declared an
    /// observer sees everything, which is what it drew before this
    /// existed.
    ///
    /// **Here rather than on [`WorldRead`], and that is the whole point.**
    /// Two players see different fog by design (`docs/plan.md` N3), so a
    /// simulation that could ask this would fold a per-client answer into
    /// a shared state -- the exact shape of a desync. The gesture layer
    /// may ask because nothing it does with the answer is hashed.
    ///
    /// What it is for is everything a map draws *about* a body rather than
    /// as one: a health bar, a name, a threat marker. The renderer culls
    /// the body itself, and an overlay that did not ask would hang in the
    /// dark pointing at what the fog is hiding.
    fn point_visible(&self, at: FixedVec3) -> bool {
        self.bridge()
            .map_or(true, |b| b.lock().expect("bridge mutex").point_visible(at))
    }
}

/// An inclusive box of sim cells.
type CellBox = ((i64, i64, i64), (i64, i64, i64));

/// The inclusive cell box a set of slides covers, or `None` for none.
fn bounds(slides: &[crate::Slide]) -> Option<CellBox> {
    let mut it = slides.iter().flat_map(|s| [s.from, s.to]).map(|c| (c, c));
    let (mut lo, mut hi) = it.next()?;
    for (a, b) in it {
        lo = (lo.0.min(a.0), lo.1.min(a.1), lo.2.min(a.2));
        hi = (hi.0.max(b.0), hi.1.max(b.1), hi.2.max(b.2));
    }
    Some((lo, hi))
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

    fn volume(&self) -> Option<&SharedPhysics> {
        self.volume.as_ref()
    }
}

impl Host for RuntimeHost {
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

    fn pick_grid(&self) -> Option<i64> {
        self.bridge()
            .map(|b| b.lock().expect("bridge mutex").pick_grid())
            .filter(|&g| g >= 0)
    }

    fn pick_cell(&self, grid: i64) -> Option<FixedVec3> {
        self.bridge()
            .and_then(|b| b.lock().expect("bridge mutex").pick_cell(grid))
    }

    fn pick_face(&self, grid: i64) -> Option<FixedVec3> {
        self.bridge()
            .and_then(|b| b.lock().expect("bridge mutex").pick_face(grid))
    }

    fn aim_yaw(&self) -> Fixed {
        self.bridge()
            .map_or(Fixed::ZERO, |b| b.lock().expect("bridge mutex").aim_yaw())
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
