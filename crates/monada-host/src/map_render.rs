//! The generic, genre-agnostic render bridge for a scripted map
//! (DESIGN.md §3.3, M4 slice 3). The host knows nothing about chess (or
//! any genre): it renders **every entity the map bound to a model** as a
//! sprite, paints whatever the map painted into its world grid, and
//! forwards raw pointer/key events. [`MapRender`] implements
//! [`HostBridge`] — the script-side calls (`model_box`, `voxel_fill`,
//! `entity_set_model`, `highlight`, `submit_command`, …) land here.
//!
//! Coordinates: the script works in **sim space**; this bridge owns the
//! sim→world mapping. `x`/`y` scale by [`SCALE`] world voxels per sim
//! unit; voxel `z` is unscaled voxels of **height above the floor surface**
//! [`GROUND_Z`] — consistent with `world_of` (entities rise with +z) and the
//! terrain store (collision), so `voxel_fill` paints terrain that stands up,
//! not sinks. Local UI (the highlighted entity, the status line, the camera)
//! is per-player and never touches `World` or the desync hash.

use std::collections::{BTreeMap, BTreeSet};

use glam::{DVec3, IVec3};
use monada_fixed::{Fixed, FixedVec3};
use monada_render::OrbitCamera;
use monada_script::{HostBridge, VoxelStore};
use monada_sim::{Command, EntityId, World};
use roxlap_core::opticast::OpticastSettings;
use roxlap_core::sky::Sky;
use roxlap_core::Camera;
use roxlap_formats::kv6::{self, Kv6};
use roxlap_formats::sprite::{Sprite, SPRITE_FLAG_NO_SHADING};
use roxlap_formats::voxel_clip::DecodedClip;
use roxlap_render::gif_import::{voxel_clip_from_gif, GifImportOpts};
use roxlap_render::{
    ActorState, BillboardActorDef, BillboardActorId, FrameParams, SceneRenderer, SpriteInstanceDesc,
    SpriteSet, VoxelClipId,
};
use roxlap_scene::{GridId, GridTransform, Scene};

/// World voxels per sim unit (x/y). The board's 8 squares span 8·SCALE.
const SCALE: f64 = 16.0;
/// World z of the board surface (z grows downward in voxlap).
const GROUND_Z: f64 = 100.0;
/// Reserved model 0: the selection-highlight marker the host draws on the
/// locally selected entity. Map-defined models start at 1.
const HIGHLIGHT_MODEL: usize = 0;
/// Max world-xy distance from a click to an entity for it to be picked.
const PICK_RADIUS: f64 = 12.0;
/// Strongest per-face grid darkening a sun can apply, to the face pointing
/// fully away from the sun (voxlap side-shade units out of the 0x80
/// brightness reference). Kept gentle so the board reads bright — only
/// shadowed faces darken (see `set_light`), not perpendicular ones.
const MAX_SIDE_SHADE: f32 = 18.0;
/// Outward normals of a grid cube's six faces, in voxlap side-shade order
/// (top/bottom/left/right/up/down). Used to shade the board by sun angle.
const CUBE_FACE_NORMALS: [[f64; 3]; 6] = [
    [0.0, 0.0, -1.0], // top (up, -z)
    [0.0, 0.0, 1.0],  // bot (down, +z)
    [-1.0, 0.0, 0.0], // left
    [1.0, 0.0, 0.0],  // right
    [0.0, -1.0, 0.0], // up (-y)
    [0.0, 1.0, 0.0],  // down (+y)
];

/// The 8 compass sides of an animated actor, in the order they map to a
/// roxlap actor's `dirs` (index 0 = view-from-front, increasing
/// counter-clockwise). The map ships one GIF per side under
/// `<dir>/<state>/<side>.gif`. **This is the single knob for the 8-way
/// facing calibration:** if a walking actor faces the wrong way, rotate or
/// reverse this array (and/or flip the `PI -` in `facing_to_world_yaw`),
/// then re-check visually. Hyphens match the user-facing file names.
const ACTOR_SIDES: [&str; 8] = [
    "south",
    "south-east",
    "east",
    "north-east",
    "north",
    "north-west",
    "west",
    "south-west",
];

/// Convert a map's sim-space facing yaw (radians, `atan2(dy, dx)`) to the
/// world yaw roxlap's directional billboard picker expects. `world_of`
/// mirrors world-X, so a sim heading `(dx, dy)` points to world `(-dx, dy)`;
/// `atan2(dy, -dx) = PI - yaw`.
fn facing_to_world_yaw(sim_yaw: f64) -> f64 {
    std::f64::consts::PI - sim_yaw
}

/// A model the script bound via `entity_set_model`: either a static sprite
/// (box / kv6) or an animated billboard [`ActorModel`]. Unifies the public
/// model-id space the script sees.
enum ModelRef {
    /// Index into [`SpriteSet::models`].
    Sprite(usize),
    /// Index into [`MapRender::actors`].
    Actor(usize),
}

/// An animated billboard actor model: each animation state's 8 decoded
/// directional GIF clips. Decode happens at `init` (renderer-free); the
/// renderer-side [`BillboardActorDef`] (with `VoxelClipId`s) is assembled
/// once, on the first frame that has a [`SceneRenderer`].
struct ActorModel {
    /// `(state name, 8 directional clips)`; the name is interned `'static`
    /// because roxlap's [`ActorState`] holds `&'static str`.
    states: Vec<(&'static str, Vec<DecodedClip>)>,
    /// `(state name, 8 registered clip ids)`, filled once the renderer
    /// exists. A fresh [`BillboardActorDef`] is rebuilt from these per entity
    /// instance (the def isn't `Clone`, but `VoxelClipId` is `Copy`).
    registered: Option<Vec<(&'static str, Vec<VoxelClipId>)>>,
}

/// Build a fresh actor recipe from registered directional clip ids.
fn actor_def(registered: &[(&'static str, Vec<VoxelClipId>)]) -> BillboardActorDef {
    BillboardActorDef {
        states: registered
            .iter()
            .map(|(name, ids)| ActorState {
                name,
                dirs: ids.clone(),
            })
            .collect(),
        ..BillboardActorDef::default()
    }
}

/// Per-entity live actor state: the renderer handle (once created), the
/// model it draws, and the desired animation/facing the script set.
struct ActorInst {
    /// Index into [`MapRender::actors`].
    model: usize,
    /// The live renderer actor, created lazily on the first frame it renders.
    id: Option<BillboardActorId>,
    /// Desired animation state (script-set, interned `'static`).
    anim: &'static str,
    /// The state last pushed to the renderer — only re-set on change, so the
    /// animation clock isn't reset every frame.
    applied_anim: &'static str,
    /// Desired facing yaw in sim radians (script-set).
    facing: f64,
}

/// A box sprite model. `shaded` keeps roxlap's per-face directional
/// shading on (pieces, lit by the map's sun); `false` flags it flat (UI
/// markers that should read at constant brightness).
fn sprite_box(w: u32, h: u32, d: u32, color: u32, shaded: bool) -> Sprite {
    let mut s = Sprite::axis_aligned(Kv6::solid_box(w, h, d, color), [0.0, 0.0, 0.0]);
    if !shaded {
        s.flags = SPRITE_FLAG_NO_SHADING;
    }
    s
}

/// One 90°-clockwise quarter-turn of a sprite about the vertical axis,
/// applied `turns` times by `model_kv6` so a map can face its asymmetric art
/// (e.g. opposing sides facing each other). Rotating the sprite *basis*
/// avoids re-baking the assets.
fn rot90_cw(mut sprite: Sprite) -> Sprite {
    // Rotate the horizontal basis vectors about +z: (x, y) -> (y, -x). The
    // vertical axis `f` is unchanged, so the piece still stands upright.
    let (s, h) = (sprite.s, sprite.h);
    sprite.s = [s[1], -s[0], s[2]];
    sprite.h = [h[1], -h[0], h[2]];
    sprite
}

/// Sim position → world-space point (sprite pivot before z-seating).
/// Entities are centred in their unit cell (`+0.5`).
///
/// **World X is mirrored** (`-`): roxlap's right-handed camera renders
/// `screen-right = -world_x` from a low-Y viewpoint, so without this a map
/// viewed from its near (low-Y) side comes out left-right flipped (files
/// reversed, board colours inverted — see chess). Negating world X cancels
/// that, so the natural viewing side reads un-mirrored. It's a pure render
/// placement (the sim is untouched) and keeps the camera basis right-handed
/// (only positions move, so the sprite frustum-cull is unaffected); the
/// grid accepts negative coords (`roxlap-scene` addr is `div_euclid`-based).
/// `voxel_fill` and the pick inverse mirror X to match.
fn world_of(p: FixedVec3) -> DVec3 {
    DVec3::new(
        -(p.x.to_f64() + 0.5) * SCALE,
        (p.y.to_f64() + 0.5) * SCALE,
        // Smaller z is up: sim z lifts above the board surface.
        GROUND_Z - p.z.to_f64() * SCALE,
    )
}

/// Intersect a world ray with the board plane `z = GROUND_Z`.
fn ground_hit(origin: DVec3, dir: DVec3) -> Option<DVec3> {
    if dir.z.abs() < 1e-9 {
        return None;
    }
    let t = (GROUND_Z - origin.z) / dir.z;
    (t > 0.0).then(|| origin + dir * t)
}

/// The render + bridge state for one scripted map. Owned by the host
/// behind `Arc<Mutex<_>>`; the same handle is coerced to a
/// [`SharedBridge`](monada_script::SharedBridge) for the Rhai engine.
pub struct MapRender {
    scene: Scene,
    /// The world grid the map paints (board / terrain).
    grid: GridId,
    /// Sprite model registry (index 0 = highlight marker) + per-frame
    /// instances. Holds the box/kv6 models; actor models live in `actors`.
    sprites: SpriteSet,
    /// Public model-id registry: each `entity_set_model` id resolves here to
    /// a static sprite or an animated actor (unifies the two id spaces).
    model_refs: Vec<ModelRef>,
    /// Animated billboard actor models (decoded GIF clips per state).
    actors: Vec<ActorModel>,
    /// Entity → public model id, set by `entity_set_model`. Render-side, not
    /// hashed. Despawned entities are skipped (positions read live).
    models: BTreeMap<EntityId, usize>,
    /// Per-entity live actor state (only for entities bound to an actor
    /// model). Created on bind, driven by `entity_set_anim` / `_facing`.
    entity_actors: BTreeMap<EntityId, ActorInst>,
    /// Actor render targets computed by `build_instances` (which has the
    /// world) for `render_into` (which has the renderer) to apply:
    /// `(entity, model index, world pos, world yaw)`.
    actor_targets: Vec<(EntityId, usize, [f32; 3], f64)>,
    /// Whether the actor clips have been registered with the renderer (done
    /// once, on the first frame a renderer is available — like `sky_uploaded`).
    clips_registered: bool,
    /// Locally selected entity (per-player UI, never networked/hashed).
    highlighted: Option<EntityId>,
    /// HUD status line, set by the map's `status(...)`.
    status: String,
    camera: OrbitCamera,
    /// Commands the map queued via `submit_command`, drained by the host.
    pending: Vec<Command>,
    /// The map's `assets/` (for `model_kv6` path resolution).
    assets: BTreeMap<String, Vec<u8>>,
    /// The local peer's player id (`None` = hotseat / all sides), exposed
    /// to the map via `local_player()` for turn gating.
    local_player: Option<i64>,
    /// Grid per-face shading the map declared via `set_light`, in voxlap
    /// side-shade order (top/bottom/left/right/up/down). Passed straight to
    /// `FrameParams.side_shades` each frame. Sprites are flat-lit in roxlap
    /// 0.19, so there is no separate sprite-sun state any more.
    side_shades: [i8; 6],
    /// Deterministic terrain heights (sim space) fed by `voxel_fill` /
    /// `voxel_set`, answering the map's `voxel_solid` / `ground_height`
    /// collision queries. Mirrors what the roxlap grid holds, but in sim
    /// coords and cheap to query.
    terrain: VoxelStore,
    /// CPU sky panorama (`FrameParams.sky`), built from the map's image.
    sky: Option<Sky>,
    /// The same panorama as RGBA8 + dims for the GPU backend's separate
    /// sky path; uploaded once.
    sky_panorama: Option<(Vec<u8>, u32, u32)>,
    sky_uploaded: bool,
    /// Whether the static sprite set was uploaded for an actor map. roxlap's
    /// `set_sprites` RESETS the dynamic layer (clips + actors), so an actor
    /// map must upload its static set exactly once — before any actor exists
    /// — or each frame's `set_sprites` would wipe the actors just created.
    sprites_uploaded: bool,
}

impl MapRender {
    /// A fresh bridge: one identity world grid + the reserved highlight
    /// marker model. (Identity grid so the GPU sprite pass projects the
    /// world camera correctly — see `monada_render`'s circle ground.)
    #[must_use]
    pub fn new(assets: BTreeMap<String, Vec<u8>>, local_player: Option<i64>) -> MapRender {
        let mut scene = Scene::new();
        let grid = scene.add_grid(GridTransform::identity());
        // Model 0: a flat amber tile the size of one cell — highlights the
        // selected entity's *square*, sitting on the board surface under
        // the sprite (rather than a marker floating in the entity).
        let marker = sprite_box(SCALE as u32, SCALE as u32, 2, 0x80FF_E000, false);
        let sprites = SpriteSet {
            models: vec![marker],
            instances: Vec::new(),
            carve_model: None,
        };
        MapRender {
            scene,
            grid,
            sprites,
            model_refs: Vec::new(),
            actors: Vec::new(),
            models: BTreeMap::new(),
            entity_actors: BTreeMap::new(),
            actor_targets: Vec::new(),
            clips_registered: false,
            highlighted: None,
            status: String::new(),
            camera: OrbitCamera::framing(DVec3::new(0.0, 0.0, GROUND_Z)),
            pending: Vec::new(),
            assets,
            local_player,
            side_shades: [0; 6],
            terrain: VoxelStore::new(),
            sky: None,
            sky_panorama: None,
            sky_uploaded: false,
            sprites_uploaded: false,
        }
    }

    /// Register a static sprite model (by its [`SpriteSet::models`] index)
    /// in the public model-id registry; returns the public id.
    fn push_sprite_model(&mut self, sprite_idx: usize) -> i64 {
        self.push_model_ref(ModelRef::Sprite(sprite_idx))
    }

    /// Append a [`ModelRef`] and return its public model id.
    fn push_model_ref(&mut self, r: ModelRef) -> i64 {
        self.model_refs.push(r);
        (self.model_refs.len() - 1) as i64
    }

    /// Rebuild the sprite instances from the live world: one sprite per
    /// entity that has a model binding, seated on the board, plus the
    /// highlight marker on the selected entity.
    pub fn build_instances(&mut self, world: &World) {
        self.sprites.instances.clear();
        self.actor_targets.clear();
        // Snapshot the bindings so the loop can mutate the disjoint sprite /
        // actor-target fields freely (the map is small — per-entity).
        let bindings: Vec<(EntityId, usize)> =
            self.models.iter().map(|(&e, &m)| (e, m)).collect();
        for (e, model_id) in bindings {
            let Some(p) = world.position(e) else {
                continue; // despawned (e.g. captured / killed)
            };
            let w = world_of(p);
            match self.model_refs.get(model_id) {
                Some(&ModelRef::Sprite(si)) => {
                    // roxlap anchors the kv6's stored pivot at the sprite
                    // `pos`, so seat by the pivot, not an assumed centre: the
                    // model's bottom face sits `(zsiz - zpiv)` below the pivot
                    // (z grows down). For a centre-pivot box this is the old
                    // `w.z - zsiz/2`; an off-centre piece no longer sinks.
                    let drop = self
                        .sprites
                        .models
                        .get(si)
                        .map_or(SCALE * 0.5, |m| f64::from(m.kv6.zsiz) - f64::from(m.kv6.zpiv));
                    self.sprites.instances.push(SpriteInstanceDesc {
                        model: si,
                        pos: [w.x as f32, w.y as f32, (w.z - drop) as f32],
                    });
                }
                Some(&ModelRef::Actor(ai)) => {
                    // A directional billboard actor: seat its bottom-centre
                    // pivot on the surface; facing comes from the script.
                    let yaw = self
                        .entity_actors
                        .get(&e)
                        .map_or(0.0, |a| facing_to_world_yaw(a.facing));
                    self.actor_targets
                        .push((e, ai, [w.x as f32, w.y as f32, w.z as f32], yaw));
                }
                None => {}
            }
        }
        if let Some(h) = self.highlighted {
            if let Some(p) = world.position(h) {
                let w = world_of(p);
                self.sprites.instances.push(SpriteInstanceDesc {
                    // Seat the tile flush on the board surface, centred on
                    // the entity's square (x/y already cell-centred).
                    model: HIGHLIGHT_MODEL,
                    pos: [w.x as f32, w.y as f32, (GROUND_Z - 1.0) as f32],
                });
            }
        }
    }

    /// Pick under a world ray: the sim-space point on the board plane, and
    /// the nearest model-bound entity within [`PICK_RADIUS`] (`-1` none).
    pub fn pick(&self, world: &World, origin: DVec3, dir: DVec3) -> (FixedVec3, i64) {
        let Some(hit) = ground_hit(origin, dir) else {
            return (FixedVec3::ZERO, -1);
        };
        let point = FixedVec3::new(
            Fixed::from_f64(-hit.x / SCALE), // world X is mirrored (see world_of)
            Fixed::from_f64(hit.y / SCALE),
            Fixed::ZERO,
        );
        let mut best: Option<(EntityId, f64)> = None;
        for &e in self.models.keys() {
            let Some(p) = world.position(e) else { continue };
            let w = world_of(p);
            let d2 = (w.x - hit.x).powi(2) + (w.y - hit.y).powi(2);
            if best.map_or(true, |(_, b)| d2 < b) {
                best = Some((e, d2));
            }
        }
        let entity = best
            .filter(|&(_, d2)| d2 <= PICK_RADIUS * PICK_RADIUS)
            .map_or(-1, |(e, _)| e.0 as i64);
        (point, entity)
    }

    /// Commands the map queued this trigger, for the host to route.
    pub fn drain_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.pending)
    }

    pub fn camera(&self) -> Camera {
        self.camera.to_roxlap()
    }
    pub fn orbit(&mut self, dyaw: f64, dpitch: f64, ddist: f64) {
        self.camera.orbit(dyaw, dpitch, ddist);
    }
    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// Drive the animated billboard actors for this frame: register their
    /// GIF clips with the renderer the first time one is available, then
    /// create / move / restate / retire one [`BillboardActorId`] per
    /// actor-bound entity from the targets `build_instances` computed, and
    /// advance the renderer's animation + facing. Render-side only.
    fn update_actors(&mut self, renderer: &mut SceneRenderer, camera: &Camera, dt: f64) {
        if self.actors.is_empty() {
            return;
        }
        // Register each actor model's directional clips once (needs the
        // renderer, which doesn't exist until the first frame).
        if !self.clips_registered {
            for am in &mut self.actors {
                let mut reg = Vec::with_capacity(am.states.len());
                for (name, clips) in &am.states {
                    let ids: Vec<VoxelClipId> =
                        clips.iter().map(|c| renderer.add_voxel_clip(c)).collect();
                    reg.push((*name, ids));
                }
                am.registered = Some(reg);
            }
            self.clips_registered = true;
        }

        // Create / update each present actor entity from this frame's targets.
        let present: BTreeSet<EntityId> = self.actor_targets.iter().map(|t| t.0).collect();
        for &(e, ai, pos, yaw) in &self.actor_targets {
            let Some(inst) = self.entity_actors.get_mut(&e) else {
                continue;
            };
            match inst.id {
                Some(id) => {
                    renderer.set_actor_transform(id, pos, yaw);
                    if inst.anim != inst.applied_anim {
                        renderer.set_actor_state(id, inst.anim);
                        inst.applied_anim = inst.anim;
                    }
                }
                None => {
                    if let Some(reg) = self.actors.get(ai).and_then(|a| a.registered.as_ref()) {
                        let id = renderer.add_billboard_actor(actor_def(reg), pos, yaw);
                        renderer.set_actor_state(id, inst.anim);
                        inst.id = Some(id);
                        inst.applied_anim = inst.anim;
                    }
                }
            }
        }

        // Retire actors whose entity is gone this frame (despawned).
        let gone: Vec<EntityId> = self
            .entity_actors
            .iter()
            .filter(|(e, a)| a.id.is_some() && !present.contains(e))
            .map(|(&e, _)| e)
            .collect();
        for e in gone {
            if let Some(inst) = self.entity_actors.remove(&e) {
                if let Some(id) = inst.id {
                    renderer.remove_billboard_actor(id);
                }
            }
        }

        // Advance animation clocks + pick the directional clip / orient each
        // slab to the camera.
        renderer.update_billboard_actors(camera, dt);
        renderer.face_billboards_to(camera);
    }

    /// Draw this map: upload its sprites and render its scene, lit by the
    /// map's declared sun (grid `side_shades`; sprites are flat-lit in
    /// roxlap 0.19) and its sky. Disjoint field borrows let the per-frame
    /// `FrameParams` reference `sky` while `scene` is borrowed mutably for
    /// the draw — which an accessor returning `&mut Scene` could not.
    pub fn render_into(
        &mut self,
        renderer: &mut SceneRenderer,
        camera: &Camera,
        settings: &OpticastSettings,
        sky_color: u32,
        dt: f64,
    ) {
        // GPU backend has its own sky path — upload the panorama once.
        if !self.sky_uploaded {
            if let Some((rgba, w, h)) = &self.sky_panorama {
                renderer.set_sky_panorama(rgba, *w, *h);
            }
            self.sky_uploaded = true;
        }
        // Upload the static sprite set *before* driving the actors:
        // `set_sprites` resets the dynamic layer (clips + actors), so a map
        // with animated actors uploads its static set exactly once (before any
        // actor is registered), while a static map (chess) rebuilds the set
        // each frame (it has no actors to clobber).
        if self.actors.is_empty() {
            renderer.set_sprites(&self.sprites);
        } else if !self.sprites_uploaded {
            renderer.set_sprites(&self.sprites);
            self.sprites_uploaded = true;
        }
        self.update_actors(renderer, camera, dt);
        let frame = FrameParams {
            settings,
            sky_color,
            sky: self.sky.as_ref(), // CPU backend sky panorama
            fog_color: 0,
            fog_max_scan_dist: 0,
            treat_z_max_as_air: true,
            gpu_mip_scan_dist: 64.0,
            gpu_max_outer_steps: 64,
            gpu_fov_y_rad: 1.2,
            // Sprites are flat-lit on both backends in roxlap 0.19; this is
            // just the on/off opt-in.
            draw_sprites: true,
            side_shades: self.side_shades,
            // Dynamic lighting (GPU-only sun + point lights) — unused by the
            // static-sprite maps; the map's sun is expressed via side_shades.
            lights: None,
        };
        renderer.render(&mut self.scene, camera, &frame);
    }
}

// All-integer / FixedVec3 signatures — no roxlap types cross into
// monada-script; this impl is the host side of the wall.
impl HostBridge for MapRender {
    fn model_box(&mut self, w: i64, h: i64, d: i64, color: i64) -> i64 {
        self.sprites
            .models
            .push(sprite_box(w as u32, h as u32, d as u32, color as u32, true));
        self.push_sprite_model(self.sprites.models.len() - 1)
    }

    fn model_kv6(&mut self, asset_path: &str, turns: i64) -> i64 {
        let sprite = self
            .assets
            .get(asset_path)
            .and_then(|bytes| kv6::parse(bytes).ok())
            .map_or_else(
                || {
                    eprintln!("monada-host: model_kv6: missing/invalid asset {asset_path:?}");
                    sprite_box(8, 8, 8, 0x80FF_00FF, true) // magenta "missing" box
                },
                // Shaded (no NO_SHADING flag) so the map's sun lights it.
                |kv6| Sprite::axis_aligned(kv6, [0.0, 0.0, 0.0]),
            );
        // Face it the way the map asked (quarter-turns CW about vertical).
        let mut sprite = sprite;
        for _ in 0..turns.rem_euclid(4) {
            sprite = rot90_cw(sprite);
        }
        self.sprites.models.push(sprite);
        self.push_sprite_model(self.sprites.models.len() - 1)
    }

    fn model_actor(&mut self, dir_path: &str, states: &[String]) -> i64 {
        // Bottom-centre pivot (feet on the ground), looping, 1 voxel/world.
        let opts = GifImportOpts::default();
        let mut actor_states = Vec::with_capacity(states.len());
        for state in states {
            let mut clips = Vec::with_capacity(ACTOR_SIDES.len());
            for side in ACTOR_SIDES {
                let path = format!("{dir_path}/{state}/{side}.gif");
                let decoded = self
                    .assets
                    .get(&path)
                    .and_then(|bytes| voxel_clip_from_gif(bytes, &opts).ok())
                    .and_then(|clip| clip.decode().ok());
                let Some(c) = decoded else {
                    eprintln!("monada-host: model_actor: missing/invalid GIF {path:?}");
                    return -1;
                };
                clips.push(c);
            }
            // roxlap's `ActorState` holds `&'static str`; intern the script's
            // state name (actor models are defined once, at `init`).
            let name: &'static str = Box::leak(state.clone().into_boxed_str());
            actor_states.push((name, clips));
        }
        if actor_states.is_empty() {
            return -1;
        }
        self.actors.push(ActorModel {
            states: actor_states,
            registered: None,
        });
        self.push_model_ref(ModelRef::Actor(self.actors.len() - 1))
    }

    fn entity_set_model(&mut self, entity: i64, model: i64) {
        let e = EntityId(entity as u64);
        let id = model as usize;
        self.models.insert(e, id);
        // Binding an actor model sets up the per-entity actor state (initial
        // animation = the model's first state).
        if let Some(&ModelRef::Actor(ai)) = self.model_refs.get(id) {
            let initial = self
                .actors
                .get(ai)
                .and_then(|a| a.states.first())
                .map_or("", |(n, _)| *n);
            self.entity_actors.insert(
                e,
                ActorInst {
                    model: ai,
                    id: None,
                    anim: initial,
                    applied_anim: "",
                    facing: 0.0,
                },
            );
        }
    }

    fn entity_set_anim(&mut self, entity: i64, state: &str) {
        let e = EntityId(entity as u64);
        let Some(&ActorInst { model, .. }) = self.entity_actors.get(&e) else {
            return;
        };
        // Reuse the model's interned `'static` name so the renderer state and
        // the change-detection compare cheaply.
        let interned = self
            .actors
            .get(model)
            .and_then(|a| a.states.iter().find(|(n, _)| *n == state))
            .map(|(n, _)| *n);
        if let (Some(name), Some(inst)) = (interned, self.entity_actors.get_mut(&e)) {
            inst.anim = name;
        }
    }

    fn entity_set_facing(&mut self, entity: i64, yaw: Fixed) {
        if let Some(inst) = self.entity_actors.get_mut(&EntityId(entity as u64)) {
            inst.facing = yaw.to_f64();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn voxel_fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64) {
        // Mirror into the sim-space terrain store for collision queries.
        self.terrain.fill(x0, y0, z0, x1, y1, z1);
        let s = SCALE as i64;
        let g = GROUND_Z as i64;
        // World X is mirrored (see `world_of`): sim cell x occupies world X
        // in [-(x+1)·s, -x·s), so the rect flips and swaps its X bounds.
        // World z grows DOWN, but sim z is height ABOVE the floor (matching
        // `world_of` and the terrain store), so height `z` sits at world
        // `g - z`: a taller fill (`z1 > z0`) reaches further up (smaller z).
        let lo = IVec3::new((-(x1 + 1) * s) as i32, (y0 * s) as i32, (g - z1) as i32);
        let hi = IVec3::new(
            (-x0 * s - 1) as i32,
            ((y1 + 1) * s - 1) as i32,
            (g - z0) as i32,
        );
        if let Some(grid) = self.scene.grid_mut(self.grid) {
            grid.set_rect(lo, hi, Some(color as u32));
        }
    }

    fn voxel_set(&mut self, x: i64, y: i64, z: i64, color: i64) {
        self.terrain.set(x, y, z);
        let scale = SCALE as i64;
        let pos = IVec3::new(
            (x * scale) as i32,
            (y * scale) as i32,
            // Height above the floor → world z `g - z` (world z grows down).
            (GROUND_Z as i64 - z) as i32,
        );
        if let Some(grid) = self.scene.grid_mut(self.grid) {
            grid.set_voxel(pos, Some(color as u32));
        }
    }

    fn highlight(&mut self, entity: i64) {
        self.highlighted = Some(EntityId(entity as u64));
    }
    fn highlight_clear(&mut self) {
        self.highlighted = None;
    }
    fn highlighted(&self) -> i64 {
        self.highlighted.map_or(-1, |e| e.0 as i64)
    }

    fn status(&mut self, text: &str) {
        self.status = text.to_string();
    }

    fn camera_focus(&mut self, point: FixedVec3) {
        self.camera.center = world_of(point);
    }

    fn camera_angle(&mut self, yaw: Fixed, pitch: Fixed) {
        self.camera.yaw = yaw.to_f64();
        self.camera.pitch = pitch.to_f64();
    }

    fn camera_dist(&mut self, dist: Fixed) {
        // Clamp to the same range the orbit nudge uses (world voxels).
        self.camera.dist = dist.to_f64().clamp(60.0, 2000.0);
    }

    fn submit_command(&mut self, verb: i64, target: i64, arg: FixedVec3) {
        self.pending
            .push(Command::on(verb as u32, EntityId(target as u64), arg));
    }

    fn local_player(&self) -> Option<i64> {
        self.local_player
    }

    fn set_light(&mut self, dir: FixedVec3, intensity: Fixed) {
        let raw = DVec3::new(dir.x.to_f64(), dir.y.to_f64(), dir.z.to_f64());
        let len = raw.length();
        if len < 1e-9 {
            return;
        }
        let travel = raw / len; // unit direction the light travels
                                // Board grid: darken only faces tilted *away* from the sun (normal
                                // along the light's travel, `dot > 0`); faces toward or perpendicular
                                // to it keep full brightness, so the lit board reads bright, not
                                // grey. `intensity` scales shadow depth (the map's contrast knob).
                                // Sprites are flat-lit in roxlap 0.19, so this no longer touches them.
        let max_shade = (MAX_SIDE_SHADE * intensity.to_f64() as f32).clamp(0.0, MAX_SIDE_SHADE);
        let mut shades = [0i8; 6];
        for (face, normal) in CUBE_FACE_NORMALS.iter().enumerate() {
            let dot = (normal[0] * travel.x + normal[1] * travel.y + normal[2] * travel.z) as f32;
            shades[face] = (max_shade * dot.max(0.0)).clamp(0.0, MAX_SIDE_SHADE) as i8;
        }
        self.side_shades = shades;
    }

    fn set_sky(&mut self, asset_path: &str) {
        let Some(bytes) = self.assets.get(asset_path) else {
            eprintln!("monada-host: set_sky: missing asset {asset_path:?}");
            return;
        };
        let rgba = match image::load_from_memory(bytes) {
            Ok(img) => img.to_rgba8(),
            Err(e) => {
                eprintln!("monada-host: set_sky: {asset_path:?}: {e}");
                return;
            }
        };
        let (width, height) = rgba.dimensions();
        // CPU `Sky`: voxlap BGRA i32 (low byte blue), brightness high byte
        // 0x80 to match the scene's voxel colours.
        let pixels: Vec<i32> = rgba
            .pixels()
            .map(|px| {
                let [r, g, b, _a] = px.0;
                ((0x80u32 << 24) | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)) as i32
            })
            .collect();
        self.sky = Some(Sky::from_pixels(pixels, width, height));
        self.sky_panorama = Some((rgba.into_raw(), width, height));
        self.sky_uploaded = false;
    }

    fn voxel_solid(&self, x: i64, y: i64, z: i64) -> bool {
        self.terrain.solid(x, y, z)
    }

    fn ground_height(&self, x: i64, y: i64) -> i64 {
        self.terrain.ground_height(x, y)
    }
}

#[cfg(test)]
mod tests {
    //! The renderer-free half of the actor bridge (the GPU/clip half needs a
    //! window): GIF decode, per-entity binding, and the actor-target the
    //! frame computes for `update_actors` to apply.
    use super::*;
    use monada_sim::World;

    /// A small solid-colour single-frame GIF the importer can voxelize.
    fn tiny_gif() -> Vec<u8> {
        let (w, h) = (4u16, 6u16);
        let mut rgba = vec![0u8; usize::from(w) * usize::from(h) * 4];
        for px in rgba.chunks_mut(4) {
            px.copy_from_slice(&[200, 80, 60, 255]); // opaque → voxels
        }
        let mut out = Vec::new();
        {
            let mut enc = gif::Encoder::new(&mut out, w, h, &[]).expect("gif encoder");
            let frame = gif::Frame::from_rgba(w, h, &mut rgba);
            enc.write_frame(&frame).expect("write frame");
        }
        out
    }

    /// `char/hero/<state>/<side>.gif` for two states × the 8 compass sides.
    fn hero_assets() -> BTreeMap<String, Vec<u8>> {
        let mut a = BTreeMap::new();
        for state in ["idle", "run"] {
            for side in ACTOR_SIDES {
                a.insert(format!("char/hero/{state}/{side}.gif"), tiny_gif());
            }
        }
        a
    }

    #[test]
    fn model_actor_decodes_binds_and_targets() {
        let mut r = MapRender::new(hero_assets(), Some(0));
        let model = r.model_actor("char/hero", &["idle".to_string(), "run".to_string()]);
        assert!(model >= 0, "actor model registered");
        assert_eq!(r.actors.len(), 1);
        assert_eq!(r.actors[0].states.len(), 2, "two animation states");
        assert_eq!(
            r.actors[0].states[0].1.len(),
            ACTOR_SIDES.len(),
            "8 directional clips per state"
        );

        // Bind to a live entity and set its render-side anim / facing.
        let mut world = World::new(0);
        let arch = world.register_archetype(&["hp"]);
        let e = world.spawn(arch);
        world.set_position(
            e,
            FixedVec3::new(Fixed::from_int(2), Fixed::from_int(3), Fixed::ZERO),
        );
        r.entity_set_model(e.0 as i64, model);
        r.entity_set_anim(e.0 as i64, "run");
        r.entity_set_facing(e.0 as i64, Fixed::ZERO);
        assert_eq!(
            r.entity_actors.get(&e).map(|a| a.anim),
            Some("run"),
            "anim state stored (interned)"
        );

        // The frame produces one actor target, not a sprite instance.
        r.build_instances(&world);
        assert_eq!(r.actor_targets.len(), 1, "one actor target");
        assert_eq!(r.actor_targets[0].0, e);
        assert!(
            r.sprites.instances.is_empty(),
            "an actor is not a static sprite instance"
        );
    }

    #[test]
    fn model_actor_missing_gif_is_minus_one() {
        let mut r = MapRender::new(BTreeMap::new(), None);
        assert_eq!(
            r.model_actor("char/hero", &["idle".to_string()]),
            -1,
            "a missing GIF aborts the actor model"
        );
    }
}
