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
use std::io::Cursor;

use image::AnimationDecoder;

use crate::autotile;
use crate::bindings::{MapActionStates, MapActionValue, Part};
use glam::{DVec3, IVec3};
use monada_fixed::{Fixed, FixedVec3};
use monada_format::ActionDecl;
use monada_render::OrbitCamera;
use monada_script::{HostBridge, VoxelStore};
use monada_sim::{Command, EntityId, World};
use roxlap_core::opticast::OpticastSettings;
use roxlap_core::sky::Sky;
use roxlap_core::Camera;
use roxlap_formats::kv6::{self, Kv6};
use roxlap_formats::sprite::{Sprite, SPRITE_FLAG_NO_SHADING};
use roxlap_formats::voxel_clip::{DecodedClip, LoopMode};
use roxlap_formats::{OverlayColor, Rgb, VoxColor};
use roxlap_render::gif_import::{voxel_clip_from_gif, GifImportOpts};
use roxlap_render::{
    ActorState, BillboardActorDef, BillboardActorId, BillboardMode, FrameParams, Line3,
    SceneRenderer, SpriteInstanceDesc, SpriteSet, ViewCutout, VoxelClipId,
};
use roxlap_scene::{GridId, GridTransform, Scene};

/// World voxels per sim unit (x/y). The board's 8 squares span 8·SCALE.
const SCALE: f64 = 16.0;
/// World z of the board surface (z grows downward in voxlap).
const GROUND_Z: f64 = 100.0;
/// The no-op actor tint (`0x00RR_GGBB` colour multiply): white leaves the art
/// unchanged. `entity_set_tint` overrides it (e.g. red for a damage flash).
const WHITE_TINT: u32 = 0x00FF_FFFF;
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

/// A pending background-music change the map requested (`play_music` /
/// `stop_music`), drained by the host each frame.
pub enum MusicCmd {
    /// Stop the current track.
    Stop,
    /// Start (or keep, if already this track) the looping track at this path.
    Play(String),
}

/// One HUD widget the map described this frame (screen points, top-left
/// origin). Texture fields index [`MapRender::ui_textures`]. Each widget
/// captures the `ui_scale` in effect when it was pushed, so a map can mix sizes
/// in one frame (a big portrait next to a smaller panel).
#[derive(Clone)]
pub enum UiWidget {
    Image {
        tex: usize,
        x: i32,
        y: i32,
        scale: f32,
    },
    /// A texture clipped to the left `frac` (0..1) of its width (bar fill).
    ImageClip {
        tex: usize,
        x: i32,
        y: i32,
        frac: f32,
        scale: f32,
    },
    Text {
        x: i32,
        y: i32,
        text: String,
        size: f32,
        scale: f32,
    },
    /// Word-wrapped text within `width` points, `0x00RR_GGBB` (dialogue).
    TextWrap {
        x: i32,
        y: i32,
        text: String,
        size: f32,
        width: f32,
        color: u32,
        scale: f32,
    },
    /// An animated image (`ui_gif` id); the host draws the wall-clock-current
    /// frame (a talking portrait).
    Anim {
        gif: usize,
        x: i32,
        y: i32,
        scale: f32,
    },
    /// An image button (normal / hover / pressed textures); a click OR-s
    /// `bit` into the next input command's button mask.
    Button {
        tex: usize,
        hover: usize,
        pressed: usize,
        x: i32,
        y: i32,
        bit: u64,
        scale: f32,
    },
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
    /// here and cloned into the owned `String` roxlap's [`ActorState`] holds
    /// (since roxlap 0.30 — `ActorState.name` was `&'static str` before).
    states: Vec<(&'static str, Vec<DecodedClip>)>,
    /// `(state name, 8 registered clip ids)`, filled once the renderer
    /// exists. A fresh [`BillboardActorDef`] is rebuilt from these per entity
    /// instance (the def isn't `Clone`, but `VoxelClipId` is `Copy`).
    registered: Option<Vec<(&'static str, Vec<VoxelClipId>)>>,
    /// Extra world-space vertical offset (`model_drop`), added when seating the
    /// actor: positive lowers the sprite (world +z is down). Corrects art whose
    /// visible feet aren't at the trimmed opaque bottom.
    drop: f32,
}

/// Build a fresh actor recipe from registered directional clip ids.
fn actor_def(registered: &[(&'static str, Vec<VoxelClipId>)]) -> BillboardActorDef {
    BillboardActorDef {
        states: registered
            .iter()
            .map(|(name, ids)| ActorState {
                name: (*name).to_string(),
                dirs: ids.clone(),
            })
            .collect(),
        // Cylindrical: the card only yaws to face the camera, staying upright
        // (its vertical axis is exactly world-up). A grounded character's feet
        // stay planted on its pivot. Spherical tilts the whole card to face the
        // camera *including pitch*, which at this steep view leans the body up-
        // and-back off its ground anchor — the sprite reads as floating above
        // its collision box even though the feet-pivot is correctly on the
        // ground. Feet-planted wins for a top-down ARPG.
        mode: BillboardMode::Cylindrical,
        ..BillboardActorDef::default()
    }
}

/// The opaque (non-air) voxel bounding box of a clip — `(min_x, max_x, min_z,
/// max_z)` across all its frames — or `None` if the clip is fully transparent.
/// Lets `model_actor` size and ground by the actual art, not the padded frame.
/// Occupancy layout (roxlap `VoxelFrame`): `col = x + y*dims[0]`, with
/// `occ_words_per_col` u32 words per column; bit `z & 31` of word `z >> 5`.
fn opaque_box(clip: &DecodedClip) -> Option<(u32, u32, u32, u32)> {
    let w = clip.dims[0];
    let cols = (clip.dims[0] * clip.dims[1]) as usize;
    let owpc = clip.occ_words_per_col as usize;
    let (mut min_x, mut max_x, mut min_z, mut max_z) = (u32::MAX, 0u32, u32::MAX, 0u32);
    let mut any = false;
    for frame in &clip.frames {
        if owpc == 0 || frame.occupancy.len() < cols * owpc {
            continue;
        }
        for col in 0..cols {
            let words = &frame.occupancy[col * owpc..(col + 1) * owpc];
            let (mut cmin, mut cmax, mut col_any) = (u32::MAX, 0u32, false);
            for (wi, &word) in words.iter().enumerate() {
                if word != 0 {
                    col_any = true;
                    let base = wi as u32 * 32;
                    cmin = cmin.min(base + word.trailing_zeros());
                    cmax = cmax.max(base + 31 - word.leading_zeros());
                }
            }
            if col_any {
                any = true;
                let x = col as u32 % w;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_z = min_z.min(cmin);
                max_z = max_z.max(cmax);
            }
        }
    }
    any.then_some((min_x, max_x, min_z, max_z))
}

/// Union of two opaque boxes (`None` = empty).
fn merge_box(
    a: Option<(u32, u32, u32, u32)>,
    b: Option<(u32, u32, u32, u32)>,
) -> Option<(u32, u32, u32, u32)> {
    match (a, b) {
        (Some((ax0, ax1, az0, az1)), Some((bx0, bx1, bz0, bz1))) => {
            Some((ax0.min(bx0), ax1.max(bx1), az0.min(bz0), az1.max(bz1)))
        }
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
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
    /// Desired `0x00RR_GGBB` colour multiply (script-set; `0x00FF_FFFF` =
    /// white = no tint). Used for the damage-flash.
    tint: u32,
    /// The tint last pushed to the renderer — only re-set on change.
    applied_tint: u32,
}

/// A box sprite model. `shaded` keeps roxlap's per-face directional
/// shading on (pieces, lit by the map's sun); `false` flags it flat (UI
/// markers that should read at constant brightness).
fn sprite_box(w: u32, h: u32, d: u32, color: u32, shaded: bool) -> Sprite {
    let mut s = Sprite::axis_aligned(Kv6::solid_box(w, h, d, VoxColor(color)), [0.0, 0.0, 0.0]);
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
        // Smaller z is up: sim z lifts above the board surface. z is UNSCALED
        // (1 world unit per sim-z), matching where `voxel_fill`/`voxel_set` put
        // grid voxels (`GROUND_Z - sim_z`) — so a sprite seated at sim-z N sits
        // ON its grid voxels, not `SCALE·N` above them. (Only x/y scale by
        // `SCALE`.) chess/RPG live at sim-z 0 where this was already a no-op;
        // the ship's upper deck needs the two z systems to agree. A map that
        // wants tall verticality stacks more sim-z layers (each 1 unit), the
        // same convention `voxel_fill` already uses.
        GROUND_Z - p.z.to_f64(),
    )
}

/// The `Grid::z_clip` value that cuts everything ABOVE sim band top `z_hi` (a
/// deck cutaway). CRITICAL: `z_clip` is in **grid-local voxel z** — where
/// `voxel_fill`/`voxel_set` actually place voxels, `GROUND_Z - sim_z`,
/// **UNSCALED** in z (only x/y scale by `SCALE`; see `voxel_set`). It is NOT
/// the `world_of` scaled z the camera/sprites use — the two coordinate systems
/// only coincide at `sim_z = 0` (a known monada wart; see the plan). Grid z is
/// z-DOWN (smaller = higher up), so voxels at sim-z > `z_hi` sit at grid-z below
/// this threshold and roxlap clips them (`z < z_clip` reads as air). Band top
/// sim-z `z_hi` maps to grid-z `GROUND_Z - z_hi`, kept; the layer above it
/// (sim-z `z_hi+1`) is one lower and cut. Unit-tested against a REAL grid.
fn deck_clip_world_z(z_hi: i64) -> i32 {
    (GROUND_Z as i64 - z_hi) as i32
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
    /// Per-cell tile textures (`tile`/`tile_fill`): each is a `SCALE×SCALE`
    /// row-major grid of voxlap colours, painted onto a cell's footprint.
    tiles: Vec<Vec<u32>>,
    /// Autotiled flat-floor terrain (`transition`/`terrain_fill`/`terrain_blit`):
    /// per-cell types blended at boundaries via marching-squares sheets.
    autotiler: autotile::Terrain,
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
    /// HUD textures the map loaded via `ui_texture` (RGBA8 + dims), indexed by
    /// the id returned to the script. Render-side, never hashed.
    ui_textures: Vec<(Vec<u8>, u32, u32)>,
    /// The HUD widget list the map rebuilt this tick (immediate mode: cleared
    /// by `ui_clear`, appended by `ui_image`/`ui_text`/`ui_button`). The host
    /// paints it via egui each frame and reports button clicks back.
    ui_widgets: Vec<UiWidget>,
    /// The viewport size (screen points) the host last rendered at, so the map
    /// can lay the HUD out relative to the window (`ui_width`/`ui_height`).
    ui_viewport: (i64, i64),
    /// Uniform scale applied to every HUD texture + text (`ui_scale`); the map
    /// lays out at scaled sizes, the host multiplies each drawn size by this.
    ui_scale: f32,
    /// One-shot sound requests (`(asset path, gain)`) the map fired since the
    /// host last drained them, DE-DUPLICATED by path: many entities triggering
    /// the same sound in one frame enqueue it once. Render-side; the host owns
    /// the mixer (rodio is `!Send`, this bridge is `Send`).
    sounds_pending: Vec<(String, f32)>,
    /// Synthesised voice blips queued this frame (`(wave, freq_hz, dur_ms,
    /// gain)`), NOT de-duplicated — one per typed glyph. Drained with the rest.
    blips_pending: Vec<(i64, i64, i64, f32)>,
    /// Looping sounds the map requested (via `play_loop`) since the last drain,
    /// de-duplicated by path. A "should this loop play now" snapshot: the host
    /// starts requested loops and stops ones that go unrequested (footsteps).
    loops_pending: Vec<String>,
    /// Background-music change since the last drain (`None` = unchanged).
    music_change: Option<MusicCmd>,
    /// Animated HUD images (`ui_gif`): decoded RGBA frames + per-frame delay
    /// (ms) + dims, indexed by the id returned to the script. The host cycles
    /// frames by wall-clock and draws the current one for a `UiWidget::Anim`.
    ui_gifs: Vec<UiGif>,
    /// Ids of the map's declared `[[action]]`s, index-aligned with
    /// `action_states` (and the binding table's `ActionRef::Map` indexes).
    action_ids: Vec<String>,
    /// Live action values, written by the host's input dispatch and read
    /// by the local script layer via the `action_*` bridge queries.
    action_states: MapActionStates,
    /// HUD-button bits clicked since the local layer last took them
    /// (`ui_clicks` bridge query). Latched by the host's egui pass.
    ui_click_bits: i64,
    /// The cursor's ground point in sim coords (`pick_ground`), refreshed
    /// by the host each frame; `None` while the ray misses.
    cursor_ground: Option<(f64, f64)>,
    /// Sim-space aim yaw toward the cursor (`aim_yaw`), holding its last
    /// value while the ray misses — the twin-stick aim convention.
    cursor_aim: f64,
    /// The entity under the cursor (`pick_entity`), refreshed per frame
    /// by the host alongside the ray; `-1` = none.
    cursor_entity: i64,
    /// Third-person wall cutout (`camera_cutout`): keyhole `(radius, feather)`
    /// in sim cells, projected to pixels each frame around the camera focus.
    /// `None` = no cutout. Render-side, never hashed.
    cutout: Option<(f64, f64)>,
    /// Deck cutaway (`deck_clip`): the grid-local z threshold for the grid's
    /// `z_clip` (voxels above it — smaller grid-z — are cut). `None` = show the
    /// whole grid. Render-side, never hashed.
    deck_clip: Option<i32>,
}

/// A decoded animated HUD image (a portrait): its frames as `(RGBA, delay_ms)`
/// plus the shared dimensions.
pub struct UiGif {
    pub frames: Vec<(Vec<u8>, u32)>,
    pub width: u32,
    pub height: u32,
}

impl MapRender {
    /// A fresh bridge: one identity world grid + the reserved highlight
    /// marker model. (Identity grid so the GPU sprite pass projects the
    /// world camera correctly — see `monada_render`'s circle ground.)
    #[must_use]
    pub fn new(
        assets: BTreeMap<String, Vec<u8>>,
        local_player: Option<i64>,
        actions: &[ActionDecl],
    ) -> MapRender {
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
            tiles: Vec::new(),
            autotiler: autotile::Terrain::new(SCALE as usize),
            sky: None,
            sky_panorama: None,
            sky_uploaded: false,
            sprites_uploaded: false,
            ui_textures: Vec::new(),
            ui_widgets: Vec::new(),
            ui_viewport: (0, 0),
            ui_scale: 1.0,
            sounds_pending: Vec::new(),
            blips_pending: Vec::new(),
            loops_pending: Vec::new(),
            music_change: None,
            ui_gifs: Vec::new(),
            action_ids: actions.iter().map(|a| a.id.clone()).collect(),
            action_states: MapActionStates::new(actions),
            ui_click_bits: 0,
            cursor_ground: None,
            cursor_aim: 0.0,
            cursor_entity: -1,
            cutout: None,
            deck_clip: None,
        }
    }

    /// Push the current deck cutaway onto the grid's `z_clip` (the render + the
    /// unit test share this one apply path, so the test exercises exactly what
    /// `render_into` does). `None` clears the clip (whole grid shown).
    fn apply_deck_clip(&mut self) {
        if let Some(grid) = self.scene.grid_mut(self.grid) {
            grid.z_clip = self.deck_clip;
        }
    }

    /// Apply one input edge to a declared action's live value (the host's
    /// binding dispatch; `index` is the manifest declaration order).
    pub fn action_set(&mut self, index: usize, part: Part, down: bool) {
        self.action_states.set(index, part, down);
    }

    /// `(id, value)` debug lines for the F1 HUD — a map author watches
    /// bindings land without wiring any UI.
    #[must_use]
    pub fn action_lines(&self) -> Vec<(String, String)> {
        self.action_ids
            .iter()
            .enumerate()
            .filter_map(|(i, id)| {
                self.action_states
                    .get(i)
                    .map(|v| (id.clone(), v.describe()))
            })
            .collect()
    }

    /// OR HUD-button bits clicked this frame into the latch `ui_clicks`
    /// hands to the local layer.
    pub fn add_ui_clicks(&mut self, bits: i64) {
        self.ui_click_bits |= bits;
    }

    /// Refresh the cursor-derived state (`pick_ground` / `pick_entity` /
    /// `aim_yaw`) from this frame's view ray. Aim holds its last value on
    /// a miss.
    pub fn set_cursor_ray(&mut self, world: &World, origin: DVec3, dir: DVec3) {
        self.cursor_ground = self.ground_sim(origin, dir);
        self.cursor_entity = self.pick(world, origin, dir).1;
        if let Some((mx, my)) = self.cursor_ground {
            let (px, py) = self.camera_center_sim();
            let (dx, dy) = (mx - px, my - py);
            if dx.mul_add(dx, dy * dy) > 1e-6 {
                self.cursor_aim = dy.atan2(dx);
            }
        }
    }

    /// The current aim yaw as the host's float (for the legacy input
    /// snapshot path; the bridge's `aim_yaw` serves the local layer).
    #[must_use]
    pub fn aim_f64(&self) -> f64 {
        self.cursor_aim
    }

    /// A declared action's live value, by its manifest id.
    fn action_value(&self, id: &str) -> Option<MapActionValue> {
        let index = self.action_ids.iter().position(|a| a == id)?;
        self.action_states.get(index)
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
        let bindings: Vec<(EntityId, usize)> = self.models.iter().map(|(&e, &m)| (e, m)).collect();
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
                    let drop = self.sprites.models.get(si).map_or(SCALE * 0.5, |m| {
                        f64::from(m.kv6.zsiz) - f64::from(m.kv6.zpiv)
                    });
                    self.sprites.instances.push(SpriteInstanceDesc {
                        model: si,
                        pos: [w.x as f32, w.y as f32, (w.z - drop) as f32],
                    });
                }
                Some(&ModelRef::Actor(ai)) => {
                    // A directional billboard actor: seat its bottom-centre
                    // pivot on the surface (plus the model's `model_drop`
                    // offset, world +z = down); facing comes from the script.
                    let yaw = self
                        .entity_actors
                        .get(&e)
                        .map_or(0.0, |a| facing_to_world_yaw(a.facing));
                    let drop = self.actors.get(ai).map_or(0.0, |a| a.drop);
                    self.actor_targets.push((
                        e,
                        ai,
                        [w.x as f32, w.y as f32, w.z as f32 + drop],
                        yaw,
                    ));
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

    /// The sim-space ground point under a world ray (cursor → aim), in the
    /// same un-mirrored convention as [`pick`](Self::pick). `None` if the ray
    /// misses the ground plane.
    #[must_use]
    #[allow(clippy::unused_self)] // a coordinate transform of this map's space
    pub fn ground_sim(&self, origin: DVec3, dir: DVec3) -> Option<(f64, f64)> {
        let hit = ground_hit(origin, dir)?;
        Some((-hit.x / SCALE, hit.y / SCALE))
    }

    /// The camera's focus point in the same sim convention as
    /// [`ground_sim`](Self::ground_sim). Maps follow the local player with
    /// `camera_focus`, so this is effectively the local player's position —
    /// the host derives the mouse-aim direction from it without the genre.
    #[must_use]
    pub fn camera_center_sim(&self) -> (f64, f64) {
        (-self.camera.center.x / SCALE, self.camera.center.y / SCALE)
    }
    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// The HUD widget list the map rebuilt this tick, for the host's egui pass.
    #[must_use]
    pub fn ui_widgets(&self) -> &[UiWidget] {
        &self.ui_widgets
    }

    /// A HUD texture's RGBA8 pixels + dimensions, by the id `ui_texture`
    /// returned to the map. `None` for an out-of-range id.
    #[must_use]
    pub fn ui_texture_data(&self, id: usize) -> Option<&(Vec<u8>, u32, u32)> {
        self.ui_textures.get(id)
    }

    /// A HUD animation's decoded frames, by the id `ui_gif` returned to the map.
    #[must_use]
    pub fn ui_gif_data(&self, id: usize) -> Option<&UiGif> {
        self.ui_gifs.get(id)
    }

    /// Record the current viewport (screen points) so the map can lay the HUD
    /// out relative to the window via `ui_width` / `ui_height`.
    pub fn set_ui_viewport(&mut self, width: i64, height: i64) {
        self.ui_viewport = (width, height);
    }

    /// Take the audio the map queued since the last call: the de-duplicated
    /// one-shot `(path, gain)` requests, the synth `(wave, freq, dur, gain)`
    /// blips, the loops that should be audible this frame, and any [`MusicCmd`]
    /// (`None` = unchanged). The host owns the mixer (rodio is `!Send`).
    #[allow(clippy::type_complexity)]
    pub fn drain_audio(
        &mut self,
    ) -> (
        Vec<(String, f32)>,
        Vec<(i64, i64, i64, f32)>,
        Vec<String>,
        Option<MusicCmd>,
    ) {
        (
            std::mem::take(&mut self.sounds_pending),
            std::mem::take(&mut self.blips_pending),
            std::mem::take(&mut self.loops_pending),
            self.music_change.take(),
        )
    }

    /// A clone of the map's sound assets (`assets/sounds/*`), for the host to
    /// hand the mixer once at startup — keyed by the same path the map plays.
    #[must_use]
    pub fn sound_assets(&self) -> Vec<(String, Vec<u8>)> {
        self.assets
            .iter()
            .filter(|(k, _)| k.starts_with("assets/sounds/"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
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
                    if inst.tint != inst.applied_tint {
                        renderer.set_actor_tint(id, Rgb(inst.tint));
                        inst.applied_tint = inst.tint;
                    }
                }
                None => {
                    if let Some(reg) = self.actors.get(ai).and_then(|a| a.registered.as_ref()) {
                        // roxlap 0.30: `add_billboard_actor` returns `None` for a
                        // malformed def (no states / empty dirs); skip if so.
                        if let Some(id) = renderer.add_billboard_actor(actor_def(reg), pos, yaw) {
                            renderer.set_actor_state(id, inst.anim);
                            if inst.tint != WHITE_TINT {
                                renderer.set_actor_tint(id, Rgb(inst.tint));
                            }
                            inst.id = Some(id);
                            inst.applied_anim = inst.anim;
                            inst.applied_tint = inst.tint;
                        }
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
        sky_color: Rgb,
        dt: f64,
        debug: bool,
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
        // Deck cutaway: clip the grid above the local crew's deck so the camera
        // sees inside (set before building `frame`, which doesn't touch scene).
        self.apply_deck_clip();
        // roxlap 0.30: `FrameParams` is `#[non_exhaustive]` — build from
        // `new` and override. The GPU mip/step/FOV knobs moved off the frame
        // (mip-scan → `RenderOptions`; step budget + FOV are now derived from
        // the scan distance + projection so the backends can't disagree).
        let mut frame = FrameParams::new(settings);
        frame.sky_color = sky_color;
        frame.sky = self.sky.as_ref(); // CPU backend sky panorama
                                       // Sprites are flat-lit on both backends; this is just the on/off opt-in.
        frame.draw_sprites = true;
        frame.side_shades = self.side_shades;
        // Third-person wall cutout: a keyhole around the camera focus (the crew
        // member) so front geometry between eye and focus dissolves. Project the
        // sim-cell radius to logical pixels at the focus distance
        // (`px ≈ world_radius / dist · hz`). `margin` is an xy shell round the
        // body column (xy is SCALEd, so a half-cell = `SCALE/2`). `z_bias`:
        // roxlap cuts cells whose grid-z < `floor(focus_z + z_bias/vws)` (vws=1
        // here). The focus is at the crew's feet, which sit at the floor voxel's
        // grid-z EXACTLY (unscaled z: `world_of(z)=GROUND_Z-z`, floor voxel at
        // the same grid-z), so `z_bias` must stay in `[0, 1)` to land the plane
        // AT the floor — cutting the walls above while the floor + feet stay.
        // `1.0` tipped `floor()` one voxel BELOW the floor and cut it out from
        // under the character (front floor + boots gone); `0.5` keeps it.
        frame.view_cutout = self.cutout.map(|(r_cells, f_cells)| {
            let dist = self.camera.dist.max(1.0);
            let to_px = |cells: f64| (cells * SCALE / dist * f64::from(settings.hz)) as f32;
            let c = self.camera.center;
            ViewCutout {
                focus_world: [c.x as f32, c.y as f32, c.z as f32],
                radius_px: to_px(r_cells),
                feather_px: to_px(f_cells),
                margin: (0.5 * SCALE) as f32, // half-cell xy shell round the body
                z_bias: 0.5,                  // plane AT the floor (feet), walls above cut
            }
        });
        renderer.render(&mut self.scene, camera, &frame);

        if debug {
            self.draw_debug_footprints(renderer, camera);
        }
    }

    /// Debug overlay (F1): draw each animated actor's collision footprint — the
    /// ground square the sim's `clear()` checks — plus a vertical anchor stalk,
    /// so the sprite's drawn feet can be compared against the position collision
    /// actually uses. World-space lines, always on top (no depth test).
    fn draw_debug_footprints(&self, renderer: &mut SceneRenderer, camera: &Camera) {
        // Half-extent of the footprint, in world units. Mirrors the monada-rpg
        // map's `clear()` radius (`ratio(40, 100)` = 0.4 cell). Debug-only: the
        // engine can't know a map's collision shape, so this matches the demo.
        const R: f64 = 0.4 * SCALE;
        let box_col = OverlayColor(0xFF00_FF00); // green footprint
        let stalk_col = OverlayColor(0xFF00_FFFF); // cyan anchor stalk
        let mut lines = Vec::with_capacity(self.actor_targets.len() * 5);
        for &(_, ai, pos, _) in &self.actor_targets {
            // The target pos includes the model's `model_drop`; the collision
            // ground is the un-dropped z, so the box shows the true footprint.
            let drop = f64::from(self.actors.get(ai).map_or(0.0, |a| a.drop));
            let (cx, cy, cz) = (
                f64::from(pos[0]),
                f64::from(pos[1]),
                f64::from(pos[2]) - drop,
            );
            let corners = [
                [cx - R, cy - R, cz],
                [cx + R, cy - R, cz],
                [cx + R, cy + R, cz],
                [cx - R, cy + R, cz],
            ];
            for i in 0..4 {
                lines.push(Line3 {
                    a: corners[i],
                    b: corners[(i + 1) % 4],
                    color: box_col,
                    width_px: 2.0,
                    depth_test: false,
                });
            }
            // Anchor stalk: up is smaller world z, one cell tall.
            lines.push(Line3 {
                a: [cx, cy, cz],
                b: [cx, cy, cz - SCALE],
                color: stalk_col,
                width_px: 2.0,
                depth_test: false,
            });
        }
        renderer.draw_lines(camera, &lines);
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

    #[allow(clippy::case_sensitive_file_extension_comparisons)] // assets are lowercase .gif
    fn model_actor(&mut self, dir_path: &str, states: &[String], height_cells: Fixed) -> i64 {
        let mut actor_states = Vec::with_capacity(states.len());
        for state in states {
            // One-shot states hold their last frame instead of looping (a
            // death pose stays a corpse; a swing doesn't restart mid-play).
            let mut opts = GifImportOpts::default();
            if matches!(state.as_str(), "death" | "attack" | "dodge" | "hurt") {
                opts.loop_mode = LoopMode::Once;
            }
            // Directional: all 8 compass GIFs present → one clip per facing.
            let prefix = format!("{dir_path}/{state}/");
            let sides: Option<Vec<DecodedClip>> = ACTOR_SIDES
                .iter()
                .map(|side| {
                    self.assets
                        .get(&format!("{prefix}{side}.gif"))
                        .and_then(|bytes| voxel_clip_from_gif(bytes, &opts).ok())
                        .and_then(|clip| clip.decode().ok())
                })
                .collect();
            let clips = if let Some(dir_clips) = sides {
                dir_clips
            } else {
                // Not the 8 compass sides — fall back to a SINGLE non-directional
                // GIF anywhere in the state dir (a view-independent effect: an
                // impact flash, a summon circle). roxlap shows the one clip from
                // every angle (`dirs.len() == 1`). Assets are lowercase `.gif`
                // (build.rs packs the map verbatim), so a plain suffix match is
                // exact.
                let single = self
                    .assets
                    .keys()
                    .filter(|k| k.starts_with(&prefix) && k.ends_with(".gif"))
                    .min()
                    .and_then(|k| self.assets.get(k))
                    .and_then(|bytes| voxel_clip_from_gif(bytes, &opts).ok())
                    .and_then(|clip| clip.decode().ok());
                let Some(c) = single else {
                    eprintln!("monada-host: model_actor: no usable GIF under {prefix:?}");
                    return -1;
                };
                vec![c]
            };
            // roxlap's `ActorState` holds `&'static str`; intern the script's
            // state name (actor models are defined once, at `init`).
            let name: &'static str = Box::leak(state.clone().into_boxed_str());
            actor_states.push((name, clips));
        }
        if actor_states.is_empty() {
            return -1;
        }

        // Measure the *opaque* bounding box across every frame of every state /
        // side, so transparent padding around the art doesn't shrink the
        // character or lift it off the ground. Size and ground from that box,
        // uniformly (so the character stays one size and grounded as it
        // animates), instead of the raw frame `dims`.
        let mut bb: Option<(u32, u32, u32, u32)> = None; // (min_x, max_x, min_z, max_z)
        for (_, clips) in &actor_states {
            for c in clips {
                bb = merge_box(bb, opaque_box(c));
            }
        }
        let target_h = height_cells.to_f64() * SCALE;
        if let Some((_min_x, _max_x, min_z, max_z)) = bb {
            let opaque_h = f64::from(max_z - min_z + 1);
            let vws = (target_h / opaque_h) as f32;
            for (_, clips) in &mut actor_states {
                for c in clips {
                    c.voxel_world_size = vws;
                    // Horizontal pivot = the frame's own centre (the padded
                    // canvas centre), NOT the trimmed opaque box. The artist
                    // positions the character within the canvas, so the canvas
                    // centre is the stable anchor across all 8 sides. Centering
                    // on the opaque box instead lets each side's / pose's
                    // differing silhouette extent move the anchor, so the sprite
                    // drifts from its collision position by a different amount
                    // per facing (the directional gap). Vertical still uses the
                    // opaque box: feet (lowest z) on the ground, sized to the
                    // visible height so padding doesn't shrink/lift the art.
                    c.pivot = [
                        f64::from(c.dims[0]) as f32 * 0.5,
                        f64::from(c.dims[1]) as f32 * 0.5,
                        min_z as f32,
                    ];
                }
            }
        } else {
            // Fully transparent (degenerate) — fall back to the frame height.
            for (_, clips) in &mut actor_states {
                for c in clips {
                    let px_h = f64::from(c.dims[2].max(1));
                    c.voxel_world_size = (target_h / px_h) as f32;
                }
            }
        }

        self.actors.push(ActorModel {
            states: actor_states,
            registered: None,
            drop: 0.0,
        });
        self.push_model_ref(ModelRef::Actor(self.actors.len() - 1))
    }

    fn model_drop(&mut self, model: i64, cells: Fixed) {
        // Resolve the public model id to its actor slot, then store the offset
        // in world units (down = +z). No-op for a non-actor / bad id.
        if let Some(&ModelRef::Actor(ai)) = usize::try_from(model)
            .ok()
            .and_then(|m| self.model_refs.get(m))
        {
            if let Some(actor) = self.actors.get_mut(ai) {
                actor.drop = (cells.to_f64() * SCALE) as f32;
            }
        }
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
                    tint: WHITE_TINT,
                    applied_tint: WHITE_TINT,
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

    fn entity_set_tint(&mut self, entity: i64, tint: i64) {
        if let Some(inst) = self.entity_actors.get_mut(&EntityId(entity as u64)) {
            // Keep only the low 24 bits (`0x00RR_GGBB`); the renderer's tint is
            // a colour multiply, white = no-op.
            inst.tint = (tint as u32) & 0x00FF_FFFF;
        }
    }

    fn play_sound(&mut self, asset_path: &str) {
        self.play_sound_gain(asset_path, Fixed::from_int(1));
    }

    fn play_sound_gain(&mut self, asset_path: &str, gain: Fixed) {
        // De-duplicate by path within this batch: many entities firing the same
        // sound this frame enqueue it once (the mass-repeat guard). The host
        // adds a time-debounce across frames.
        if self.sounds_pending.iter().any(|(p, _)| p == asset_path) {
            return;
        }
        let g = gain.to_f64().clamp(0.0, 1.0) as f32;
        self.sounds_pending.push((asset_path.to_string(), g));
    }

    fn play_blip(&mut self, wave: i64, freq: i64, dur_ms: i64, gain: Fixed) {
        let g = gain.to_f64().clamp(0.0, 1.0) as f32;
        self.blips_pending.push((wave, freq, dur_ms, g));
    }

    fn play_loop(&mut self, asset_path: &str) {
        if !self.loops_pending.iter().any(|p| p == asset_path) {
            self.loops_pending.push(asset_path.to_string());
        }
    }

    fn play_music(&mut self, asset_path: &str) {
        self.music_change = Some(MusicCmd::Play(asset_path.to_string()));
    }

    fn stop_music(&mut self) {
        self.music_change = Some(MusicCmd::Stop);
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
            grid.set_rect(lo, hi, Some(VoxColor(color as u32)));
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
            grid.set_voxel(pos, Some(VoxColor(color as u32)));
        }
    }

    #[allow(clippy::many_single_char_names)]
    fn tile(&mut self, asset_path: &str) -> i64 {
        let Some(bytes) = self.assets.get(asset_path) else {
            eprintln!("monada-host: tile: missing asset {asset_path:?}");
            return -1;
        };
        let img = match image::load_from_memory(bytes) {
            Ok(img) => img.to_rgba8(),
            Err(e) => {
                eprintln!("monada-host: tile: {asset_path:?}: {e}");
                return -1;
            }
        };
        let (w, h) = img.dimensions();
        if w == 0 || h == 0 {
            return -1;
        }
        // Nearest-neighbour resample to one cell (SCALE×SCALE voxels); each
        // pixel becomes a voxlap colour (high byte = 0x80 brightness).
        let s = SCALE as u32;
        let mut cells = Vec::with_capacity((s * s) as usize);
        for ty in 0..s {
            for tx in 0..s {
                let px = img
                    .get_pixel((tx * w / s).min(w - 1), (ty * h / s).min(h - 1))
                    .0;
                let [r, g, b, _a] = px;
                cells.push(0x8000_0000 | (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b));
            }
        }
        self.tiles.push(cells);
        (self.tiles.len() - 1) as i64
    }

    #[allow(clippy::too_many_arguments, clippy::many_single_char_names)]
    fn tile_fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, tile: i64) {
        // Collision is identical to `voxel_fill`; only the colours differ.
        self.terrain.fill(x0, y0, z0, x1, y1, z1);
        let Some(cells) = self.tiles.get(tile as usize).cloned() else {
            return;
        };
        let s = SCALE as i64;
        let g = GROUND_Z as i64;
        let Some(grid) = self.scene.grid_mut(self.grid) else {
            return;
        };
        for cy in y0.min(y1)..=y0.max(y1) {
            for cx in x0.min(x1)..=x0.max(x1) {
                for ly in 0..s {
                    for lx in 0..s {
                        let color = cells[(ly * s + lx) as usize];
                        // World X is mirrored (see `world_of`); tile column `lx`
                        // maps across the cell's mirrored X span, row `ly` along Y.
                        let wx = (-cx * s - 1 - lx) as i32;
                        let wy = (cy * s + ly) as i32;
                        for z in z0.min(z1)..=z0.max(z1) {
                            grid.set_voxel(
                                IVec3::new(wx, wy, (g - z) as i32),
                                Some(VoxColor(color)),
                            );
                        }
                    }
                }
            }
        }
    }

    fn transition(&mut self, low: i64, high: i64, asset_path: &str) {
        let Some(bytes) = self.assets.get(asset_path) else {
            eprintln!("monada-host: transition: missing asset {asset_path:?}");
            return;
        };
        let rgba = match image::load_from_memory(bytes) {
            Ok(img) => img.to_rgba8(),
            Err(e) => {
                eprintln!("monada-host: transition: {asset_path:?}: {e}");
                return;
            }
        };
        let (w, h) = rgba.dimensions();
        let t = autotile::Transition::from_sheet(&rgba, w, h, SCALE as usize);
        self.autotiler.add_transition(low, high, t);
    }

    fn terrain_fill(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, type_id: i64) {
        for cy in y0.min(y1)..=y0.max(y1) {
            for cx in x0.min(x1)..=x0.max(x1) {
                self.autotiler.cells.insert((cx, cy), type_id);
            }
        }
    }

    fn terrain_blit(&mut self, base_type: i64) {
        // Bounding box of the set cells.
        let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
        for &(cx, cy) in self.autotiler.cells.keys() {
            x0 = x0.min(cx);
            y0 = y0.min(cy);
            x1 = x1.max(cx);
            y1 = y1.max(cy);
        }
        if x0 > x1 {
            return; // no terrain set
        }
        let g = GROUND_Z as i64;
        let autotiler = &self.autotiler;
        let Some(grid) = self.scene.grid_mut(self.grid) else {
            return;
        };
        // Floor pixel (fx, fy) is sim-aligned; world X is mirrored (`-fx - 1`,
        // matching `tile_fill`/`world_of`), Y direct, at the floor surface.
        autotiler.paint(x0, y0, x1, y1, base_type, |fx, fy, color| {
            grid.set_voxel(
                IVec3::new((-fx - 1) as i32, fy as i32, g as i32),
                Some(VoxColor(color)),
            );
        });
    }

    // --- HUD / UI overlay (screen-space, render-side only) ----------------

    fn ui_texture(&mut self, asset_path: &str) -> i64 {
        let Some(bytes) = self.assets.get(asset_path) else {
            eprintln!("monada-host: ui_texture: missing asset {asset_path:?}");
            return -1;
        };
        let img = match image::load_from_memory(bytes) {
            Ok(img) => img.to_rgba8(),
            Err(e) => {
                eprintln!("monada-host: ui_texture: {asset_path:?}: {e}");
                return -1;
            }
        };
        let (w, h) = img.dimensions();
        self.ui_textures.push((img.into_raw(), w, h));
        (self.ui_textures.len() - 1) as i64
    }

    fn ui_gif(&mut self, asset_path: &str) -> i64 {
        let Some(bytes) = self.assets.get(asset_path) else {
            eprintln!("monada-host: ui_gif: missing asset {asset_path:?}");
            return -1;
        };
        let frames = image::codecs::gif::GifDecoder::new(Cursor::new(bytes.clone()))
            .and_then(|d| d.into_frames().collect_frames());
        let frames = match frames {
            Ok(f) if !f.is_empty() => f,
            Ok(_) => return -1,
            Err(e) => {
                eprintln!("monada-host: ui_gif: {asset_path:?}: {e}");
                return -1;
            }
        };
        let (mut w, mut h) = (0u32, 0u32);
        let mut out = Vec::with_capacity(frames.len());
        for f in frames {
            let (num, den) = f.delay().numer_denom_ms();
            let delay = num.checked_div(den).map_or(100, |d| d.max(10));
            let img = f.into_buffer();
            w = img.width();
            h = img.height();
            out.push((img.into_raw(), delay));
        }
        self.ui_gifs.push(UiGif {
            frames: out,
            width: w,
            height: h,
        });
        (self.ui_gifs.len() - 1) as i64
    }

    fn ui_anim(&mut self, gif: i64, x: i64, y: i64) {
        if let Ok(gif) = usize::try_from(gif) {
            self.ui_widgets.push(UiWidget::Anim {
                gif,
                x: x as i32,
                y: y as i32,
                scale: self.ui_scale,
            });
        }
    }

    fn ui_width(&self) -> i64 {
        self.ui_viewport.0
    }
    fn ui_height(&self) -> i64 {
        self.ui_viewport.1
    }

    fn ui_scale(&mut self, factor: Fixed) {
        self.ui_scale = (factor.to_f64() as f32).max(0.1);
    }

    fn ui_clear(&mut self) {
        self.ui_widgets.clear();
    }

    fn ui_image(&mut self, tex: i64, x: i64, y: i64) {
        if let Ok(tex) = usize::try_from(tex) {
            self.ui_widgets.push(UiWidget::Image {
                tex,
                x: x as i32,
                y: y as i32,
                scale: self.ui_scale,
            });
        }
    }

    fn ui_image_clip(&mut self, tex: i64, x: i64, y: i64, frac: Fixed) {
        if let Ok(tex) = usize::try_from(tex) {
            let frac = frac.to_f64().clamp(0.0, 1.0) as f32;
            self.ui_widgets.push(UiWidget::ImageClip {
                tex,
                x: x as i32,
                y: y as i32,
                frac,
                scale: self.ui_scale,
            });
        }
    }

    fn ui_text(&mut self, x: i64, y: i64, text: &str, size: i64) {
        self.ui_widgets.push(UiWidget::Text {
            x: x as i32,
            y: y as i32,
            text: text.to_string(),
            size: size.max(1) as f32,
            scale: self.ui_scale,
        });
    }

    fn ui_text_wrap(&mut self, x: i64, y: i64, text: &str, size: i64, width: i64, color: i64) {
        self.ui_widgets.push(UiWidget::TextWrap {
            x: x as i32,
            y: y as i32,
            text: text.to_string(),
            size: size.max(1) as f32,
            width: width.max(1) as f32,
            color: (color as u32) & 0x00FF_FFFF,
            scale: self.ui_scale,
        });
    }

    fn ui_button(&mut self, tex: i64, hover: i64, pressed: i64, x: i64, y: i64, button_bit: i64) {
        let (Ok(tex), Ok(hover), Ok(pressed), Ok(bit)) = (
            usize::try_from(tex),
            usize::try_from(hover),
            usize::try_from(pressed),
            u64::try_from(button_bit),
        ) else {
            return;
        };
        self.ui_widgets.push(UiWidget::Button {
            tex,
            hover,
            pressed,
            x: x as i32,
            y: y as i32,
            bit,
            scale: self.ui_scale,
        });
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

    fn camera_cutout(&mut self, radius: Fixed, feather: Fixed) {
        let r = radius.to_f64();
        self.cutout = (r > 0.0).then(|| (r, feather.to_f64().max(0.0)));
    }

    fn deck_clip(&mut self, _z_lo: i64, z_hi: i64) {
        // roxlap's `Grid::z_clip` cuts one side: voxels with grid-z BELOW the
        // threshold (smaller grid-z = higher up, z-down) become air. So clip at
        // the band top (sim `z_hi`) to cut everything above it. The band floor
        // (`z_lo`) is reserved — the engine can't cut the underside with a
        // single threshold yet. A band whose top is the world's tallest voxel
        // yields a threshold at/below all geometry, cutting nothing.
        self.deck_clip = Some(deck_clip_world_z(z_hi));
    }

    fn submit_command(&mut self, verb: i64, target: i64, arg: FixedVec3) {
        self.pending
            .push(Command::on(verb as u32, EntityId(target as u64), arg));
    }

    fn local_player(&self) -> Option<i64> {
        self.local_player
    }

    // --- local-layer input queries (docs/plans/input-bindings.md §3) ------
    // Backed by the action states the host's binding dispatch writes and
    // the per-frame cursor refresh (`set_cursor_ray`). Registered only
    // into the local backend; the sim backend never sees these.

    fn action_down(&self, id: &str) -> bool {
        self.action_value(id)
            .is_some_and(|v| matches!(v, MapActionValue::Button(true)))
    }

    fn action_axis(&self, id: &str) -> i64 {
        match self.action_value(id) {
            Some(MapActionValue::Axis { pos, neg }) => i64::from(pos) - i64::from(neg),
            _ => 0,
        }
    }

    fn action_axis2(&self, id: &str) -> (i64, i64) {
        match self.action_value(id) {
            Some(MapActionValue::Axis2 {
                up,
                down,
                left,
                right,
            }) => (
                i64::from(right) - i64::from(left),
                i64::from(up) - i64::from(down),
            ),
            _ => (0, 0),
        }
    }

    fn pick_ground(&self) -> Option<FixedVec3> {
        self.cursor_ground
            .map(|(x, y)| FixedVec3::new(Fixed::from_f64(x), Fixed::from_f64(y), Fixed::ZERO))
    }

    fn pick_entity(&self) -> i64 {
        self.cursor_entity
    }

    fn aim_yaw(&self) -> Fixed {
        Fixed::from_f64(self.cursor_aim)
    }

    fn ui_clicks(&mut self) -> i64 {
        std::mem::take(&mut self.ui_click_bits)
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

    /// The deck cutaway, end-to-end against the REAL roxlap grid: paint two
    /// stacked deck floors with the actual `voxel_set` the map uses, apply the
    /// deck clip, and raycast straight down the column. The clip must turn the
    /// upper floor to air so the ray reaches the lower one — proving the
    /// threshold matches where `voxel_set` really places voxels (grid-z,
    /// UNSCALED), not a hand model in the wrong coordinate system.
    #[test]
    fn deck_clip_cuts_the_deck_above_via_a_real_grid() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let (cx, cy) = (2_i64, 2_i64);
        r.voxel_set(cx, cy, 0, 0x80AA_AAAA); // lower deck floor (sim-z 0)
        r.voxel_set(cx, cy, 4, 0x8055_5555); // upper deck floor (sim-z 4)

        // `voxel_set` places cell (x, y) at world (x·SCALE, y·SCALE, …); ray
        // straight down the column (+z is "down", world z-down) from above both.
        let col = DVec3::new(cx as f64 * SCALE + 0.5, cy as f64 * SCALE + 0.5, 0.0);
        let down = DVec3::new(0.0, 0.0, 1.0);
        let hit_z = |r: &MapRender| {
            r.scene
                .raycast_clipped(col, down, 4096.0)
                .map(|h| h.voxel.z)
        };

        // No clip → the ray meets the upper floor first (higher = smaller grid-z).
        r.apply_deck_clip(); // deck_clip is None here
        let upper = hit_z(&r).expect("hits the upper floor");
        assert_eq!(
            upper,
            GROUND_Z as i32 - 4,
            "upper floor at grid-z GROUND_Z-4"
        );

        // Clip to the lower deck band (sim-z 0..3) → the upper floor is cut and
        // the ray reaches the lower floor (a larger grid-z, deeper down).
        r.deck_clip(0, 3);
        r.apply_deck_clip();
        let lower = hit_z(&r).expect("hits the lower floor after the cut");
        assert_eq!(lower, GROUND_Z as i32, "lower floor at grid-z GROUND_Z");
        assert!(lower > upper, "the clip exposed the lower deck");

        // The upper deck's own band (top sim-z 7) is the tallest thing → cuts
        // nothing: the ray still meets the upper floor.
        r.deck_clip(4, 7);
        r.apply_deck_clip();
        assert_eq!(
            hit_z(&r),
            Some(GROUND_Z as i32 - 4),
            "top band cuts nothing"
        );
    }

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

    /// An 8×8 GIF whose only opaque content is a full-width band in image rows
    /// 3..=5 — transparent padding above and below. Voxel `z = 7 - row`, so the
    /// band occupies `z 2..=4` (feet at z=2, not the frame bottom z=0).
    fn padded_gif() -> Vec<u8> {
        let (w, h) = (8u16, 8u16);
        let mut rgba = vec![0u8; usize::from(w) * usize::from(h) * 4];
        for row in 3..=5u16 {
            for col in 0..w {
                let i = (usize::from(row) * usize::from(w) + usize::from(col)) * 4;
                rgba[i..i + 4].copy_from_slice(&[200, 80, 60, 255]); // opaque band
            }
        }
        let mut out = Vec::new();
        {
            let mut enc = gif::Encoder::new(&mut out, w, h, &[]).expect("gif encoder");
            let frame = gif::Frame::from_rgba(w, h, &mut rgba);
            enc.write_frame(&frame).expect("write frame");
        }
        out
    }

    #[test]
    fn alpha_padding_is_trimmed() {
        let mut a = BTreeMap::new();
        for side in ACTOR_SIDES {
            a.insert(format!("char/hero/idle/{side}.gif"), padded_gif());
        }
        let mut r = MapRender::new(a, None, &[]);
        let model = r.model_actor("char/hero", &["idle".to_string()], Fixed::from_int(3));
        assert!(model >= 0, "actor registered");
        let clip = &r.actors[0].states[0].1[0];
        // Feet at the opaque band (z=2), not the padded frame bottom (z=0), so
        // the actor isn't lifted off the ground.
        assert!(
            (clip.pivot[2] - 2.0).abs() < 1e-3,
            "pivot at the opaque feet (z=2), got {}",
            clip.pivot[2]
        );
        // 3 cells × SCALE(16) over the 3-voxel opaque band → 16 wsu/voxel —
        // sized by the band, not the 8px padded frame (which would give 6).
        assert!(
            (clip.voxel_world_size - 16.0).abs() < 1e-3,
            "sized by the opaque band, got {}",
            clip.voxel_world_size
        );
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
        let mut r = MapRender::new(hero_assets(), Some(0), &[]);
        let model = r.model_actor(
            "char/hero",
            &["idle".to_string(), "run".to_string()],
            Fixed::from_int(2),
        );
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
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        assert_eq!(
            r.model_actor("char/hero", &["idle".to_string()], Fixed::from_int(2)),
            -1,
            "a missing GIF aborts the actor model"
        );
    }
}
