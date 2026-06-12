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
//! unit; voxel `z` is unscaled voxels down from the board surface
//! [`GROUND_Z`] (so the map controls board thickness directly). Local UI
//! (the highlighted entity, the status line, the camera) is per-player
//! and never touches `World` or the desync hash.

use std::collections::BTreeMap;

use glam::{DVec3, IVec3};
use monada_fixed::{Fixed, FixedVec3};
use monada_render::OrbitCamera;
use monada_script::HostBridge;
use monada_sim::{Command, EntityId, World};
use roxlap_core::opticast::OpticastSettings;
use roxlap_core::sky::Sky;
use roxlap_core::sprite::SpriteLighting;
use roxlap_core::{Camera, Engine};
use roxlap_formats::kv6::{self, Kv6};
use roxlap_formats::sprite::{Sprite, SPRITE_FLAG_NO_SHADING};
use roxlap_render::{FrameParams, SceneRenderer, SpriteInstanceDesc, SpriteSet};
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
/// Sprite material (`Engine::kv6col`) — the master brightness for pieces in
/// the lightmode-1 tint path. Below the engine's mid-grey `0x80` default
/// would halve them; pure white `0xFF` blows the lit faces past the colour
/// clamp (flat pure-white). `0xB0` sits just under the clamp so pieces show
/// their own colour with directional form, while highlights still pop.
const SPRITE_MATERIAL: u32 = 0x00B0_B0B0;
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

/// Sim position → world-space point (sprite pivot before z-seating).
/// Entities are centred in their unit cell (`+0.5`), so a piece at sim
/// `(sx, sy)` sits in the middle of the voxel square the map painted at
/// `[sx·SCALE, sx·SCALE+SCALE)`. Picking stays corner-based (sim =
/// world/SCALE, floored by the script to the cell index), which is the
/// consistent inverse.
fn world_of(p: FixedVec3) -> DVec3 {
    DVec3::new(
        (p.x.to_f64() + 0.5) * SCALE,
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
    /// Model registry (index 0 = highlight marker) + per-frame instances.
    sprites: SpriteSet,
    /// Entity → base model, set by `entity_set_model`. Render-side, not
    /// hashed. Despawned entities are skipped (positions read live).
    models: BTreeMap<EntityId, usize>,
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
    /// Lighting the map declared via `set_light` — a sprite sun (lightmode
    /// 2 + a `LightSrc`) and grid `side_shades`. Snapshotted per frame.
    engine: Engine,
    /// CPU sky panorama (`FrameParams.sky`), built from the map's image.
    sky: Option<Sky>,
    /// The same panorama as RGBA8 + dims for the GPU backend's separate
    /// sky path; uploaded once.
    sky_panorama: Option<(Vec<u8>, u32, u32)>,
    sky_uploaded: bool,
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
        // Sprite material so unlit maps' pieces aren't halved by the
        // engine's mid-grey `kv6col` default (see `set_light`).
        let mut engine = Engine::new();
        engine.set_kv6col(SPRITE_MATERIAL);
        let sprites = SpriteSet {
            models: vec![marker],
            instances: Vec::new(),
            carve_model: None,
        };
        MapRender {
            scene,
            grid,
            sprites,
            models: BTreeMap::new(),
            highlighted: None,
            status: String::new(),
            camera: OrbitCamera::framing(DVec3::new(0.0, 0.0, GROUND_Z)),
            pending: Vec::new(),
            assets,
            local_player,
            engine,
            sky: None,
            sky_panorama: None,
            sky_uploaded: false,
        }
    }

    /// Rebuild the sprite instances from the live world: one sprite per
    /// entity that has a model binding, seated on the board, plus the
    /// highlight marker on the selected entity.
    pub fn build_instances(&mut self, world: &World) {
        let sprites = &mut self.sprites;
        sprites.instances.clear();
        for (&e, &model) in &self.models {
            let Some(p) = world.position(e) else {
                continue; // despawned (e.g. captured)
            };
            let w = world_of(p);
            let zsiz = sprites
                .models
                .get(model)
                .map_or(SCALE, |m| f64::from(m.kv6.zsiz));
            sprites.instances.push(SpriteInstanceDesc {
                model,
                // Seat the model bottom on the surface (pivot is centre).
                pos: [w.x as f32, w.y as f32, (w.z - zsiz * 0.5) as f32],
            });
        }
        if let Some(h) = self.highlighted {
            if let Some(p) = world.position(h) {
                let w = world_of(p);
                sprites.instances.push(SpriteInstanceDesc {
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
            Fixed::from_f64(hit.x / SCALE),
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

    /// Draw this map: upload its sprites and render its scene, lit by the
    /// map's declared sun (sprite `SpriteLighting` + grid `side_shades`)
    /// and its sky. Disjoint field borrows let the per-frame `FrameParams`
    /// reference `engine`/`sky` while `scene` is borrowed mutably for the
    /// draw — which an accessor returning `&mut Scene` could not.
    pub fn render_into(
        &mut self,
        renderer: &mut SceneRenderer,
        camera: &Camera,
        settings: &OpticastSettings,
        sky_color: u32,
    ) {
        // GPU backend has its own sky path — upload the panorama once.
        if !self.sky_uploaded {
            if let Some((rgba, w, h)) = &self.sky_panorama {
                renderer.set_sky_panorama(rgba, *w, *h);
            }
            self.sky_uploaded = true;
        }
        let lighting = SpriteLighting::from_engine(&self.engine);
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
            sprite_lighting: Some(&lighting),
            side_shades: self.engine.side_shades(),
        };
        renderer.set_sprites(&self.sprites);
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
        (self.sprites.models.len() - 1) as i64
    }

    fn model_kv6(&mut self, asset_path: &str) -> i64 {
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
        self.sprites.models.push(sprite);
        (self.sprites.models.len() - 1) as i64
    }

    fn entity_set_model(&mut self, entity: i64, model: i64) {
        self.models.insert(EntityId(entity as u64), model as usize);
    }

    #[allow(clippy::too_many_arguments)]
    fn voxel_fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64) {
        let s = SCALE as i64;
        let g = GROUND_Z as i64;
        let lo = IVec3::new((x0 * s) as i32, (y0 * s) as i32, (g + z0) as i32);
        let hi = IVec3::new(
            ((x1 + 1) * s - 1) as i32,
            ((y1 + 1) * s - 1) as i32,
            (g + z1) as i32,
        );
        if let Some(grid) = self.scene.grid_mut(self.grid) {
            grid.set_rect(lo, hi, Some(color as u32));
        }
    }

    fn voxel_set(&mut self, x: i64, y: i64, z: i64, color: i64) {
        let scale = SCALE as i64;
        let pos = IVec3::new(
            (x * scale) as i32,
            (y * scale) as i32,
            (GROUND_Z as i64 + z) as i32,
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
                                // Fresh engine each call: deterministic, idempotent.
        let mut engine = Engine::new();
        // Sprites: white material + lightmode 1 (directional surface tint).
        // The default `kv6col` is mid-grey (0x80) which halves every sprite
        // voxel's colour, and lightmode 2's sprite path is ambient-capped —
        // together they rendered pieces at ~⅓ brightness. White + mode 1
        // lights pieces at near-full colour. roxlap fixes the mode-1 tint
        // direction (brightest toward +x/+y for an axis-aligned sprite); the
        // board below follows `dir`, so point the map's light from +x/+y to
        // keep the two consistent. A steerable bright sprite light is a
        // roxlap follow-up.
        engine.set_kv6col(SPRITE_MATERIAL);
        engine.set_lightmode(1);
        // Board grid: darken only faces tilted *away* from the sun (normal
        // along the light's travel, `dot > 0`); faces toward or perpendicular
        // to it keep full brightness, so the lit board reads bright, not
        // grey. `intensity` scales shadow depth (the map's contrast knob).
        let max_shade = (MAX_SIDE_SHADE * intensity.to_f64() as f32).clamp(0.0, MAX_SIDE_SHADE);
        let mut shades = [0i8; 6];
        for (face, normal) in CUBE_FACE_NORMALS.iter().enumerate() {
            let dot = (normal[0] * travel.x + normal[1] * travel.y + normal[2] * travel.z) as f32;
            shades[face] = (max_shade * dot.max(0.0)).clamp(0.0, MAX_SIDE_SHADE) as i8;
        }
        engine.set_side_shades(
            shades[0], shades[1], shades[2], shades[3], shades[4], shades[5],
        );
        self.engine = engine;
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
}
