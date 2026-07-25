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
use glam::{DQuat, DVec3, IVec2, IVec3, Vec2};
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
use roxlap_scene::fow::{DeckBand, FogOfWar, FowObserver, FowTwin, VisionConfig};
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

/// Map an inclusive sim-cell region `(x0,y0,z0)..=(x1,y1,z1)` to its world
/// voxel box `(lo, hi)` for `Grid::set_rect`. World X is mirrored (see
/// `world_of`): sim cell x occupies world X `[-(x+1)·SCALE, -x·SCALE)`, so the
/// box flips and swaps its X bounds. World z grows DOWN while sim z is height
/// above the floor, so sim height z sits at world `GROUND_Z - z` — a taller
/// fill (larger `z1`) reaches further up (smaller world z), hence `z1` feeds
/// `lo.z` and `z0` feeds `hi.z`. `voxel_fill`/`voxel_fill_in`/`voxel_set`/
/// `voxel_clear` all share this transform so it lives in exactly one place
/// (`voxel_set` passes a degenerate 1-cell region; `voxel_clear` a 1-column,
/// z-spanning one).
fn sim_box_to_world(x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64) -> (IVec3, IVec3) {
    let s = SCALE as i64;
    let gz = GROUND_Z as i64;
    let lo = IVec3::new((-(x1 + 1) * s) as i32, (y0 * s) as i32, (gz - z1) as i32);
    let hi = IVec3::new(
        (-x0 * s - 1) as i32,
        ((y1 + 1) * s - 1) as i32,
        (gz - z0) as i32,
    );
    (lo, hi)
}

/// Map an inclusive sim-cell region to its grid-voxel box on the ISOTROPIC
/// volume world grid (one grid voxel per cell, `voxel_world_size = SCALE`,
/// origin `(0, 0, GROUND_Z)`): cell `(x, y, z)` is grid voxel
/// `(-x-1, y, -z-1)` — the same world-X mirror and z-down flip as
/// `sim_box_to_world`, but in cells, not world voxels.
#[allow(clippy::cast_possible_truncation)]
fn cell_box_to_volume_grid(
    x0: i64,
    y0: i64,
    z0: i64,
    x1: i64,
    y1: i64,
    z1: i64,
) -> (IVec3, IVec3) {
    let (xa, xb) = (x0.min(x1), x0.max(x1));
    let (ya, yb) = (y0.min(y1), y0.max(y1));
    let (za, zb) = (z0.min(z1), z0.max(z1));
    let lo = IVec3::new((-xb - 1) as i32, ya as i32, (-zb - 1) as i32);
    let hi = IVec3::new((-xa - 1) as i32, yb as i32, (-za - 1) as i32);
    (lo, hi)
}

/// The continuous sim→world map of a volume map (see the `volume` field):
/// `(0, 0, GROUND_Z) + SCALE · R_y(π) · p`. Agrees with
/// `cell_box_to_volume_grid` on cell corners and with `world_of` on x/y —
/// only z differs (scaled by `SCALE` instead of unscaled).
fn volume_world_of(p: DVec3) -> DVec3 {
    DVec3::new(-SCALE * p.x, SCALE * p.y, GROUND_Z - SCALE * p.z)
}

/// The world-frame pose of a physics body's render grid. The grid's voxels
/// are the body's SHAPE cells 1:1 (`voxel_world_size = SCALE`), so the pose
/// must compose the shape→body rebase (the derived CoM, physics P3's
/// `com_in_shape` seam), the body's sim orientation, and the sim→world map:
/// `rotation = R_y(π) ∘ q` (the mirror half-turn is a proper rotation, so a
/// full 3D orientation survives), `origin = world(position) − rotation ·
/// (SCALE · com)`. Free function so the math is unit-testable without a
/// physics world.
fn body_grid_pose(
    position: FixedVec3,
    orientation: monada_fixed::FixedQuat,
    com_in_shape: FixedVec3,
) -> (DVec3, glam::DQuat) {
    let rot = mirror_half_turn() * dquat(orientation);
    let origin = volume_world_of(dvec3(position)) - rot * (dvec3(com_in_shape) * SCALE);
    (origin, rot)
}

/// The sim→world mirror as a rotation: `diag(-1, 1, -1)` = a half-turn
/// about +Y (`det = +1`).
fn mirror_half_turn() -> glam::DQuat {
    glam::DQuat::from_rotation_y(std::f64::consts::PI)
}

fn dvec3(v: FixedVec3) -> DVec3 {
    DVec3::new(v.x.to_f64(), v.y.to_f64(), v.z.to_f64())
}

fn dquat(q: monada_fixed::FixedQuat) -> glam::DQuat {
    glam::DQuat::from_xyzw(q.x.to_f64(), q.y.to_f64(), q.z.to_f64(), q.w.to_f64())
}

/// Render colour for a physics material id — a fixed, engine-side palette
/// (the map gets a real colour pass in D4; until then distinct materials
/// just need to READ as distinct). High byte = roxlap brightness.
fn material_color(mat: u16) -> u32 {
    const PALETTE: [u32; 8] = [
        0x80c8_6a32, // 0: rust orange (the un-materialed default — terrain)
        0x8078_8290, // 1: steel grey (the digger hull)
        0x80b8_9850, // 2: sandstone
        0x8060_6a74, // 3: granite
        0x8068_c8d8, // 4: crystal
        0x80a0_5048, // 5: brick red
        0x8058_a058, // 6: moss green
        0x8090_78b0, // 7: violet
    ];
    PALETTE[usize::from(mat) % PALETTE.len()]
}

/// Render mirror of one physics body (plan §1d): its shape grid plus one
/// small grid per wheel. Grids are blitted once and re-posed per frame —
/// transform updates never touch voxel data. When the body disappears
/// from the sim (D3: fully drilled away, or a destruction split retiring
/// an id) the mirror's voxels are cleared and the entry dropped.
struct BodyMirror {
    grid: GridId,
    /// Occupied-cell count at the last blit; a difference re-blits (the D3
    /// carve seam — `remove_voxels` shrinks the count).
    blitted: usize,
    /// Shape dims at the last blit — the box to clear on re-blit/removal.
    dims: IVec3,
    wheels: Vec<WheelMirror>,
}

/// One wheel's render state: the cylinder grid and its accumulated spin
/// angle (radians). Spin is derived render-side from ground speed — the
/// stateless-wheel dividend; it exists nowhere in the hashed sim.
struct WheelMirror {
    wheel: u32,
    grid: GridId,
    spin: f64,
    /// Cylinder blit extent (radius, half-width) — the box to clear on
    /// removal.
    extent: (i32, i32),
}

/// Wheel cylinder half-width along its axle, world voxels.
const WHEEL_HALF_WIDTH: i32 = 3;

/// One debris puff: a dust sprite rising from a carved cell for
/// [`PUFF_TTL`] seconds. World position is the cell centre at spawn; the
/// rise is derived from `age` at draw time.
struct Puff {
    pos: DVec3,
    color: u32,
    age: f64,
}

/// Puff lifetime, seconds.
const PUFF_TTL: f64 = 0.45;
/// Puff rise rate, world voxels per second (upward = −z).
const PUFF_RISE: f64 = 24.0;

/// The render-side suspension length of a stateless wheel: march the
/// solver's own ray — from the anchor along body-down through the volume
/// terrain — and clamp the surface distance minus the wheel radius to
/// `[0, rest]`. At equilibrium the chassis sits ~`mg/(4k)` lower than
/// spawn, so a rest-length wheel would bury itself by that much;
/// airborne (no hit) → full extension. Presentation only.
#[allow(clippy::cast_possible_truncation)]
fn wheel_travel(
    terrain: &monada_script::VolumeStore,
    anchor: DVec3,
    down: DVec3,
    rest: f64,
    radius: f64,
) -> f64 {
    let mut dist = 0.0;
    while dist <= rest + radius {
        let probe = anchor + down * dist;
        let cell_x = probe.x.floor() as i64;
        let cell_y = probe.y.floor() as i64;
        let cell_z = probe.z.floor() as i64;
        if terrain.get(cell_x, cell_y, cell_z).is_some() {
            return (dist - radius).clamp(0.0, rest);
        }
        dist += 1.0 / 16.0;
    }
    rest
}

/// A grid for a small dynamic prop (a physics body, a wheel): rotates every
/// frame, so apply roxlap's rotating-grid guidance — one mip level (the
/// near-axis-aligned cf-cancellation artifact) and no grid-local sky (it
/// would rotate with the prop and fight the world's).
fn new_prop_grid(scene: &mut Scene, voxel_world_size: f64) -> GridId {
    let id = scene.add_grid(GridTransform::at_scale(DVec3::ZERO, voxel_world_size));
    if let Some(grid) = scene.grid_mut(id) {
        grid.mip_levels_override = Some(1);
        grid.render_sky = false;
    }
    id
}

/// Blit a wheel cylinder into its (world-voxel) grid: axis along local Y
/// (the axle), radius `r` world voxels, centred on the grid origin. A
/// lighter spoke arm makes the render-side spin readable.
#[allow(clippy::cast_possible_truncation)]
fn blit_wheel_cylinder(scene: &mut Scene, id: GridId, r: f64, half_width: i32) {
    let Some(grid) = scene.grid_mut(id) else {
        return;
    };
    let ri = r.ceil() as i32;
    for x in -ri..=ri {
        for z in -ri..=ri {
            // Voxel centre vs the axle at the grid origin.
            let (cx, cz) = (f64::from(x) + 0.5, f64::from(z) + 0.5);
            if cx.mul_add(cx, cz * cz) > r * r {
                continue;
            }
            let spoke = z >= 0 && x.abs() <= 1;
            let color = if spoke { 0x8090_9098 } else { 0x8030_3038 };
            for y in -half_width..half_width {
                let c = IVec3::new(x, y, z);
                grid.set_rect(c, c, Some(VoxColor(color)));
            }
        }
    }
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

/// The four sim-cell corners of a box-select drag rectangle, aligned to the
/// SCREEN rather than to world north/south. The drag's two ground points
/// `a`/`b` (press and release) are opposite corners; the other two follow the
/// camera's ground-projected screen axes at the live `yaw`, so the box rotates
/// with the view instead of sticking to world X/Y (which reads as skewed the
/// moment the camera is orbited). The host owns this because only it knows the
/// live camera angle — the map's `cam_yaw` is a frozen init constant.
///
/// Sim-space camera basis (world X is mirrored, see `world_of`, so a world dir
/// `(wx, wy)` is sim dir `(-wx, wy)`): roxlap's `right = (-sin y, cos y)` →
/// sim screen-right `u = (sin y, cos y)`; the forward's ground component
/// `(cos y, sin y)` → sim screen-up `v = (-cos y, sin y)`. `u`/`v` are
/// orthonormal, so decomposing `b - a` onto them and rebuilding is exact (the
/// third corner comes back to `b`). Corners are wound around the quad. At
/// `yaw = 0` this collapses to the old world-axis rectangle (screen == world).
fn drag_quad_sim(yaw: f64, a: (f64, f64), b: (f64, f64)) -> [(f64, f64); 4] {
    let (sy, cy) = yaw.sin_cos();
    let (ux, uy) = (sy, cy); // screen-right, on the ground, in sim cells
    let (vx, vy) = (-cy, sy); // screen-up, on the ground, in sim cells
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let du = dx * ux + dy * uy; // (b − a) projected on screen-right
    let dv = dx * vx + dy * vy; // (b − a) projected on screen-up
    [
        a,
        (a.0 + ux * du, a.1 + uy * du),
        (a.0 + ux * du + vx * dv, a.1 + uy * du + vy * dv), // == b (exact)
        (a.0 + vx * dv, a.1 + vy * dv),
    ]
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
// Independent renderer facts (upload latches, terrain mode), not a state
// machine — the same stance as RhaiBackend's handler flags.
#[allow(clippy::struct_excessive_bools)]
pub struct MapRender {
    scene: Scene,
    /// The world / terrain grid, painted by `voxel_fill`/`voxel_set`/`tile_fill`/
    /// autotile. Lazily created at the world origin on the first paint call (see
    /// [`world_grid`](Self::world_grid)) so a map that paints terrain never needs
    /// to spawn a grid explicitly, and old maps keep working. `None` until then.
    world_grid: Option<GridId>,
    /// `terrain = "volume"` mode (docs/plans/digger-demo.md §1d): the world
    /// grid is ISOTROPIC — `voxel_world_size = SCALE`, ONE grid voxel per sim
    /// cell on all three axes — instead of the column convention's
    /// `SCALE×SCALE×1` world voxels per cell. Rotating physics bodies force
    /// this: a roxlap grid supports rotation + uniform scale only, and the
    /// column convention's anisotropic z (unscaled) cannot rotate. The world-X
    /// mirror + z-down flip compose to `diag(-1, 1, -1)` = a half-turn about
    /// +Y — a PROPER rotation — so sim→world stays exact: `world = (0, 0,
    /// GROUND_Z) + SCALE · R_y(π) · sim`. Set by the host from the manifest
    /// BEFORE the first paint.
    volume: bool,
    /// Per-`BodyId` render mirrors of the embedded physics sim (volume maps
    /// only), fed by [`sync_physics`](Self::sync_physics). Map scripts never
    /// hand-mirror a body (plan §1d locked decision).
    body_mirrors: BTreeMap<u64, BodyMirror>,
    /// Live debris puffs (plan §1d): every volume-map carve spawns a
    /// short-lived dust sprite at the cell, coloured from the voxel it
    /// replaced. Render-side only — nothing debris-shaped enters the sim
    /// (the falling-sand layer stays future work, physics plan §8).
    puffs: Vec<Puff>,
    /// Puff sprite models by colour, lazily registered into the STATIC
    /// sprite set — actor-less maps re-upload that set every frame, so
    /// static instances are the layer that actually shows there (the
    /// dynamic ring layer gets reset by that same upload). NB an
    /// actor-ful volume map would need the dynamic-layer route instead;
    /// no such map exists yet.
    puff_models: BTreeMap<u32, usize>,
    /// Map-declared physics-material colours (`phys_material_color`,
    /// D4): the body mirror consults this before the engine's fallback
    /// palette. Render-side only.
    phys_colors: BTreeMap<u16, u32>,
    /// Grids spawned by the script via `grid_spawn` (e.g. the ship demo's hull).
    /// The script's i64 handle is the index into this Vec; never reordered or
    /// compacted so handles stay stable. Painted by `voxel_fill_in`. Kept
    /// separate from [`world_grid`](Self::world_grid) so terrain/fog never attach
    /// to a decorative grid.
    grids: Vec<GridId>,
    /// The grid the fog of war and `deck_clip` cutaway attach to — the local
    /// observer's hull (a `grids` entry the ship names via `vision_observer`), or
    /// `world_grid` for the legacy single-grid maps. `None` ⇒ no fog / no clip.
    vision_grid: Option<GridId>,
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
    /// Entity → the `grids` grid it rides, set by `entity_set_grid`. An entity
    /// here has its sim `position` read as grid-local and composed through the
    /// grid's transform (origin + rotation) when seated — so crew stay put on a
    /// moving/rotating hull. Unbound entities render in the global frame
    /// (`world_of` directly). Render-side, not hashed.
    entity_grid: BTreeMap<EntityId, GridId>,
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
    /// Locally selected entities (per-player UI, never networked/hashed).
    /// `highlight` replaces the set (single-select), `highlight_add` grows
    /// it (RTS box select); ascending-id order is the `highlighted_all`
    /// contract.
    highlighted: BTreeSet<EntityId>,
    /// The selection-ring marker on the renderer's DYNAMIC sprite layer,
    /// for actor maps: their static sprite set uploads exactly once
    /// (re-uploading resets the actor layer), so marker instances frozen
    /// in it would never appear — instead `sync_rings` mirrors the
    /// per-frame marker instances through add/remove_sprite_instance.
    /// Actor-less maps (chess) keep the static path (`set_sprites` runs
    /// every frame there and carries the marker itself).
    ring_model: Option<roxlap_render::SpriteModelId>,
    /// Live ring instance ids, torn down and re-issued each frame.
    ring_ids: Vec<roxlap_render::SpriteInstanceId>,
    /// An active pointer drag's anchor (sim-space ground point), set by
    /// `drag_begin`; the far corner rides `cursor_ground`. Render-side
    /// gesture state — the stateless local script layer cannot hold it.
    drag_anchor: Option<(f64, f64)>,
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
    /// The current deck band (`deck_clip`'s sim `z_lo..=z_hi`), kept so the fog
    /// of war can build its `DeckBand`. `None` = no deck declared.
    deck_band: Option<(i64, i64)>,
    /// Fog of war (`vision_observer`): the local observer entity, or `None`.
    /// Per-client, never hashed.
    vision_entity: Option<EntityId>,
    /// Fog-of-war tuning (`vision_config`): `(cone_deg, range_cells, peripheral_cells)`.
    vision_cfg: (i64, i64, i64),
    /// The live fog mask + its dimmed "known twin" grid, lazily built for the
    /// observer once a deck band is known; rebuilt when the band changes (deck
    /// change). `fow_band` is the sim band the current mask was built for.
    fow: Option<FogOfWar>,
    fow_twin: Option<FowTwin>,
    fow_band: Option<(i64, i64)>,
    /// The observer's world feet-position + facing yaw, captured in
    /// `build_instances` (which has the `World`) for `render_into` to build the
    /// `FowObserver` from.
    observer_pose: Option<(DVec3, f64)>,
    /// Queued `vision_hear` reveals `(grid cell x, y, deck z sim, loudness)`,
    /// drained into the mask each frame.
    vision_hears: Vec<(i64, i64, i64, f32)>,
}

/// A decoded animated HUD image (a portrait): its frames as `(RGBA, delay_ms)`
/// plus the shared dimensions.
pub struct UiGif {
    pub frames: Vec<(Vec<u8>, u32)>,
    pub width: u32,
    pub height: u32,
}

impl MapRender {
    /// A fresh bridge: no grids yet (scripts spawn them via `grid_spawn`) +
    /// the reserved highlight marker model.
    #[must_use]
    pub fn new(
        assets: BTreeMap<String, Vec<u8>>,
        local_player: Option<i64>,
        actions: &[ActionDecl],
    ) -> MapRender {
        let scene = Scene::new();
        // Model 0: a flat amber selection RING circling the selected
        // entity's footprint on the ground under the sprite — the classic
        // RTS read (a multi-selected squad shows one ring per unit). Same
        // placement contract as the old filled tile (`from_fn` and
        // `solid_box` share the pivot path), so chess's square highlight
        // simply became a circle on its square.
        let marker = {
            let d = SCALE as u32 + 4; // a hair wider than the cell
            let c = f64::from(d - 1) * 0.5;
            let (r_out, r_in) = (c, c - 2.5);
            let kv6 = Kv6::from_fn(d, d, 2, |x, y, _z| {
                let (dx, dy) = (f64::from(x) - c, f64::from(y) - c);
                let d2 = dx.mul_add(dx, dy * dy);
                (d2 <= r_out * r_out && d2 >= r_in * r_in).then_some(VoxColor(0x80FF_E000))
            });
            let mut s = Sprite::axis_aligned(kv6, [0.0, 0.0, 0.0]);
            s.flags = SPRITE_FLAG_NO_SHADING;
            s
        };
        let sprites = SpriteSet {
            models: vec![marker],
            instances: Vec::new(),
            carve_model: None,
        };
        MapRender {
            scene,
            world_grid: None,
            volume: false,
            body_mirrors: BTreeMap::new(),
            puffs: Vec::new(),
            puff_models: BTreeMap::new(),
            phys_colors: BTreeMap::new(),
            grids: Vec::new(),
            vision_grid: None,
            sprites,
            model_refs: Vec::new(),
            actors: Vec::new(),
            models: BTreeMap::new(),
            entity_grid: BTreeMap::new(),
            entity_actors: BTreeMap::new(),
            actor_targets: Vec::new(),
            clips_registered: false,
            highlighted: BTreeSet::new(),
            ring_model: None,
            ring_ids: Vec::new(),
            drag_anchor: None,
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
            deck_band: None,
            vision_entity: None,
            vision_cfg: (100, 8, 3),
            fow: None,
            fow_twin: None,
            fow_band: None,
            observer_pose: None,
            vision_hears: Vec::new(),
        }
    }

    /// The world / terrain grid, creating it at the world origin on first use.
    /// Terrain paints (`voxel_fill`/`voxel_set`/`tile_fill`/autotile) go through
    /// here so a map never has to spawn a grid to paint a board, and old maps
    /// keep working (an identity grid so the GPU sprite pass projects the world
    /// camera correctly — see `monada_render`'s circle ground).
    fn world_grid(&mut self) -> GridId {
        *self.world_grid.get_or_insert_with(|| {
            if self.volume {
                // Isotropic cell grid (see the `volume` field): origin at
                // GROUND_Z so cell z 0 tops out exactly where the column
                // convention's floor surface sits.
                self.scene.add_grid(GridTransform::at_scale(
                    DVec3::new(0.0, 0.0, GROUND_Z),
                    SCALE,
                ))
            } else {
                self.scene.add_grid(GridTransform::identity())
            }
        })
    }

    /// Switch this map to `terrain = "volume"` rendering (isotropic world
    /// grid + physics body mirrors — see the `volume` field). The host calls
    /// this from the manifest BEFORE the script's `init` paints anything;
    /// flipping it after the world grid exists would leave voxels painted
    /// under the other convention, so it panics on a late call.
    pub fn set_volume_terrain(&mut self) {
        assert!(
            self.world_grid.is_none(),
            "set_volume_terrain must precede the first terrain paint"
        );
        self.volume = true;
    }

    /// The grid the fog / deck cutaway attach to: the observer's hull if the map
    /// named one via `vision_observer`, else the world grid. `None` only when a
    /// map has neither — i.e. never painted terrain nor named a vision grid.
    fn vision_grid(&self) -> Option<GridId> {
        self.vision_grid.or(self.world_grid)
    }

    /// Set the fog observer entity and the grid its mask rides. Rebuilds the mask
    /// (drops it, so it re-arms next frame) whenever either changes — a new
    /// observer or a switch to another hull grid both invalidate the old mask.
    fn set_observer(&mut self, entity: i64, grid: Option<GridId>) {
        let e = (entity >= 0).then_some(EntityId(entity as u64));
        if e != self.vision_entity || grid != self.vision_grid {
            self.vision_entity = e;
            self.vision_grid = grid;
            self.drop_fow();
        }
        // The fog mask re-bases the observer through `vision_grid` while `place`
        // seats the same crew through `entity_grid` — if a map bound the observer
        // to a different hull than the crew rides, the cone and the sprite would
        // silently disagree. Derive one from the other rather than trusting the
        // author: the observer entity always rides its own vision grid. A `None`
        // (legacy world) grid leaves the binding to the identity world transform,
        // which `place` already assumes for an unbound entity.
        if let (Some(e), Some(g)) = (e, grid) {
            self.entity_grid.insert(e, g);
        }
    }

    /// Push the current deck cutaway onto the vision grid's `z_clip` (the render
    /// and the unit test share this one apply path, so the test exercises exactly
    /// what `render_into` does). `None` clears the clip (whole grid shown). Only
    /// the vision grid is clipped, because the band is grid-local and applying it
    /// to a grid at another origin would cut that grid at the wrong world height.
    fn apply_deck_clip(&mut self) {
        if let Some(id) = self.vision_grid() {
            if let Some(grid) = self.scene.grid_mut(id) {
                grid.z_clip = self.deck_clip;
            }
        }
    }

    /// Drop the fog mask + detach its twin grid from the scene (so the real
    /// grid draws normally again). Called when the observer / config changes.
    fn drop_fow(&mut self) {
        if let Some(twin) = self.fow_twin.take() {
            twin.detach(&mut self.scene);
        }
        self.fow = None;
        self.fow_band = None;
    }

    /// Update the fog of war for the local observer (captured in
    /// `build_instances`) and return the twin grid to style via
    /// `FrameParams.fow`, or `None` if vision is off / not ready. Mirrors the
    /// roxlap boarding demo's FW.5 loop (build → update → sync twin).
    fn update_fow(&mut self, dt: f64) -> Option<GridId> {
        self.vision_entity?; // no observer ⇒ no fog (guards a stale pose)
        let (feet, yaw) = self.observer_pose?;
        self.deck_band?; // a deck was declared (deck_clip ran) ⇒ vision is ready
        let main_grid = self.vision_grid()?; // no grid yet ⇒ no fog
                                             // Re-base the world-space observer into the vision grid's local voxel
                                             // space: the fog mask is grid-local, so a grid spawned off the world
                                             // origin (or a moving/turning hull) must have its viewpoint expressed
                                             // in the grid's own frame. This is roxlap's world→grid-local map,
                                             // `rotation.inverse() * (world - origin) / voxel_world_size`. Every
                                             // grid we make has voxel_world_size 1, so the scale drops out; we read
                                             // the live origin AND rotation each frame, so a hull that translates or
                                             // yaws is tracked automatically. The identity world grid has origin 0
                                             // and no rotation, so this is a no-op there.
        let (grid_origin, grid_rot) = self
            .scene
            .grid(main_grid)
            .map_or((DVec3::ZERO, DQuat::IDENTITY), |g| {
                (g.transform.origin, g.transform.rotation)
            });
        let grid_rot_inv = grid_rot.inverse();
        let feet = grid_rot_inv * (feet - grid_origin);
        // Build the mask once. The fog rides ONE fixed grid-local band spanning
        // the whole hull, NOT the crew's current `deck_clip` band. A staircase
        // BRIDGES two decks — its columns run from the lower floor up past the
        // upper one — so any per-deck band never contains the whole run and
        // leaves it permanently fogged. `deck_clip` still hides the non-current
        // deck visually; the fog just tracks every column and lets LOS (blocked
        // within `EYE_HALF` of the eye) do the between-deck occlusion. A fixed
        // band also means the mask never rebuilds on a deck flip, so remembered
        // cells survive a climb (vision_observer/vision_config drop it explicitly
        // when the viewpoint or tuning actually changes).
        if self.fow.is_none() {
            let (cone_deg, range, peripheral) = self.vision_cfg;
            // z-down: the floor is the LARGER grid-z (the lowest deck sits at
            // GROUND_Z, sim_z 0), the ceiling the smaller. HULL_SPAN clears the
            // tallest deck + its walls with margin.
            let hull_span: i32 = 64;
            let z_bottom = GROUND_Z as i32; // lowest floor
            let z_top = GROUND_Z as i32 - hull_span; // generous ceiling
            let mut cfg = VisionConfig::for_decks(vec![DeckBand { z_top, z_bottom }]);
            cfg.cone_half_angle = (cone_deg as f32).to_radians() * 0.5;
            // Ranges are sim cells; grid columns are SCALE finer.
            cfg.range = range as f32 * SCALE as f32;
            cfg.peripheral_range = peripheral as f32 * SCALE as f32;
            cfg.memory_decay = 2.0;
            self.fow = Some(FogOfWar::new(cfg));
            self.fow_twin = Some(FowTwin::attach(&mut self.scene, main_grid));
            self.fow_band = self.deck_band;
        }
        // The observer, in the vision grid's local voxels (`feet` was re-based
        // above). The crew's `facing` yaw is HULL-RELATIVE: it is authored the
        // same way the sprite's is, and the sprite turns it to world by the grid
        // rotation (see `grid_facing_yaw`). The fog mask is built in grid-local
        // space and the twin grid re-applies the grid rotation when it renders,
        // so we feed the hull-relative facing straight in — the twin then turns
        // the cone to world and it tracks a spinning hull exactly as the sprite
        // does. De-rotating here by `grid_rot_inv` (as this used to) cancels the
        // twin's rotation and pins the cone to one world direction while the hull
        // spins under it. `world_of` mirrors sim +x → world -x (hence `-cos`).
        let facing_local = DVec3::new(-(yaw.cos()), yaw.sin(), 0.0);
        let observer = FowObserver {
            cell: IVec2::new(feet.x.floor() as i32, feet.y.floor() as i32),
            facing: Vec2::new(facing_local.x as f32, facing_local.y as f32),
            deck: 0,
            // Eye near HEAD height above the feet (z-down ⇒ a smaller grid-z).
            // Two forces set this:
            //  - roxlap blocks LOS with any voxel within `EYE_HALF` (2) of the
            //    eye. The crew stands ON the 1-voxel floor slab (at `feet.z`), so
            //    a low eye sits in the floor's opacity band and goes blind to a
            //    patch underfoot.
            //  - Each staircase riser (7 tall) OCCLUDES the tread behind it: an
            //    eye that barely clears one step sees risers, not treads, so the
            //    step tops stay fogged from below (correct LOS, but too low).
            // The roxlap boarding demo rides the eye at ~83% of body height
            // (EYE_HEIGHT 10 of a 12-tall body) so the crew looks OVER the near
            // steps onto the treads — `-16` (≈head height on the ~22-tall crew,
            // clears two 7-risers) matches that and reveals the run from below.
            eye_z: feet.z as i32 - 16,
        };
        // Take the mask + twin out to keep `self.scene` borrows disjoint.
        let mut fow = self.fow.take()?;
        let mut twin = self.fow_twin.take()?;
        if let Some(grid) = self.scene.grid(main_grid) {
            fow.update(grid, &observer, dt as f32);
        }
        for (hx, hy, _hz, loud) in std::mem::take(&mut self.vision_hears) {
            // Same world→grid-local re-basing as the observer above (heard
            // cells are grid-local too).
            let w = grid_rot_inv
                * (world_of(FixedVec3::new(
                    Fixed::from_int(hx as i32),
                    Fixed::from_int(hy as i32),
                    Fixed::ZERO,
                )) - grid_origin);
            fow.hear(0, IVec2::new(w.x.floor() as i32, w.y.floor() as i32), loud);
        }
        let out = if twin.sync(&mut self.scene, &fow) {
            let id = twin.twin();
            self.fow_twin = Some(twin);
            Some(id)
        } else {
            // Twin lost (snapshot / rollback) — re-arm AND populate the fresh twin
            // THIS frame. A newly attached twin holds no geometry until its first
            // `sync`, so deferring to next frame would blank the hull for a frame;
            // syncing now (a full first-seen scan) also lets a rollback that lands
            // mid-rotation render at the live transform instead of jittering.
            let mut fresh = FowTwin::attach(&mut self.scene, main_grid);
            let out = fresh.sync(&mut self.scene, &fow).then(|| fresh.twin());
            self.fow_twin = Some(fresh);
            out
        };
        // The twin renders the hull (the real grid is `render_excluded`), but
        // `sync` only mirrors the real grid's transform in its Phase 2 — which it
        // SKIPS on a "quiet" frame (mask version + real mutation counter
        // unchanged). A hull that merely ROTATES each tick bumps neither:
        // `grid_orient` is not a voxel edit (no mutation) and turns no cell
        // visible/dark (no mask change). So on an idle frame `sync` early-outs and
        // the twin's transform freezes while the real grid keeps turning — the
        // rendered hull stalls, and the crew (placed from the live real transform
        // in `place`) visibly slides off it. Force the twin to track the real
        // transform every frame — on both the synced and the just-re-armed twin —
        // so the hull turns smoothly regardless of fog activity or rollback.
        if let Some(id) = out {
            if let Some(t) = self.scene.grid(main_grid).map(|g| g.transform) {
                if let Some(tw) = self.scene.grid_mut(id) {
                    tw.transform = t;
                }
            }
        }
        self.fow = Some(fow);
        out
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

    /// [`world_of`] with this map's z convention: volume maps scale z by
    /// `SCALE` (isotropic cells — see the `volume` field), column maps keep
    /// the unscaled-z convention. Everything that seats render state on an
    /// entity/sim position goes through here.
    fn entity_world_of(&self, p: FixedVec3) -> DVec3 {
        let mut w = world_of(p);
        if self.volume {
            w.z = GROUND_Z - p.z.to_f64() * SCALE;
        }
        w
    }

    /// Seat sim position `p` in world space, composed through the grid the
    /// entity rides (if any). [`Self::entity_world_of`] maps sim → the grid's
    /// LOCAL world frame (the same frame `voxel_fill_in`'s voxels live in); a
    /// bound grid's transform (rotation then origin) then carries it into the
    /// world — so a crew member tracks its hull as it moves or turns. Unbound
    /// entities have an identity transform, i.e. `entity_world_of(p)`
    /// unchanged (chess/RPG/RTS).
    fn place(&self, e: EntityId, p: FixedVec3) -> DVec3 {
        let local = self.entity_world_of(p);
        match self.entity_grid.get(&e).and_then(|&g| self.scene.grid(g)) {
            Some(grid) => grid.transform.rotation * local + grid.transform.origin,
            None => local,
        }
    }

    /// A bound entity's billboard facing turned by its grid's *full* 3D
    /// rotation: the billboard's world facing direction rotated by the grid
    /// quaternion, then re-projected onto the horizontal plane. Honest for any
    /// orientation — a cylindrical billboard can only yaw, so a hull's pitch and
    /// roll fold into the projected heading rather than being silently dropped
    /// (the old `grid_yaw` collapsed the quaternion to a z-only scalar, which was
    /// wrong the moment the hull left pure-yaw). Returns `world_yaw` unchanged
    /// for an unbound entity or an un-rotated grid.
    fn grid_facing_yaw(&self, e: EntityId, world_yaw: f64) -> f64 {
        match self.entity_grid.get(&e).and_then(|&g| self.scene.grid(g)) {
            Some(grid) => {
                let dir =
                    grid.transform.rotation * DVec3::new(world_yaw.cos(), world_yaw.sin(), 0.0);
                // A hull pitched toward vertical projects the facing onto a
                // near-zero horizontal vector; `atan2(0, 0)` is a meaningless 0
                // that would snap the billboard to +x. Below that floor the
                // heading is undefined, so hold the incoming world yaw.
                if dir.x.hypot(dir.y) < 1e-6 {
                    world_yaw
                } else {
                    dir.y.atan2(dir.x)
                }
            }
            None => world_yaw,
        }
    }

    /// Rebuild the sprite instances from the live world: one sprite per
    /// entity that has a model binding, seated on the board, plus the
    /// highlight marker on the selected entity.
    pub fn build_instances(&mut self, world: &World) {
        self.sprites.instances.clear();
        self.actor_targets.clear();
        // Capture the fog-of-war observer's world pose (feet + facing yaw) while
        // we have the World; `render_into` builds the `FowObserver` from it.
        self.observer_pose = self.vision_entity.and_then(|e| {
            let p = world.position(e)?;
            let yaw = self.entity_actors.get(&e).map_or(0.0, |a| a.facing);
            Some((self.place(e, p), yaw))
        });
        // Snapshot the bindings so the loop can mutate the disjoint sprite /
        // actor-target fields freely (the map is small — per-entity).
        let bindings: Vec<(EntityId, usize)> = self.models.iter().map(|(&e, &m)| (e, m)).collect();
        for (e, model_id) in bindings {
            let Some(p) = world.position(e) else {
                continue; // despawned (e.g. captured / killed)
            };
            // The observer entity is usually model-bound too, so reuse the
            // seat we already composed for `observer_pose` above instead of
            // running `place` (a quaternion rotate) a second time this frame.
            let w = if Some(e) == self.vision_entity {
                self.observer_pose
                    .map_or_else(|| self.place(e, p), |(pos, _)| pos)
            } else {
                self.place(e, p)
            };
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
                    let yaw = self.grid_facing_yaw(
                        e,
                        self.entity_actors
                            .get(&e)
                            .map_or(0.0, |a| facing_to_world_yaw(a.facing)),
                    );
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
        // Selection markers: one per selected entity. Despawned entities
        // (killed / captured since selection) silently drop out of the set,
        // so `highlighted_all` never hands the map a stale id.
        self.highlighted.retain(|e| world.position(*e).is_some());
        // Compose each marker's seat inline rather than snapshotting the set into
        // a fresh `Vec` every frame: `place` borrows the whole `&self`, which
        // would clash with pushing into `self.sprites`, but inlining its grid
        // composition touches only the disjoint `entity_grid` / `scene` fields the
        // borrow checker can split from `sprites`.
        for &h in &self.highlighted {
            if let Some(p) = world.position(h) {
                let local = world_of(p);
                let w = match self.entity_grid.get(&h).and_then(|&g| self.scene.grid(g)) {
                    Some(grid) => grid.transform.rotation * local + grid.transform.origin,
                    None => local,
                };
                self.sprites.instances.push(SpriteInstanceDesc {
                    // Seat the tile flush on the ground the entity stands
                    // on (its own w.z, not the z=0 board plane — a unit up
                    // on a plateau keeps its marker underfoot), centred on
                    // its cell (x/y already cell-centred).
                    model: HIGHLIGHT_MODEL,
                    pos: [w.x as f32, w.y as f32, (w.z - 1.0) as f32],
                });
            }
        }
    }

    /// Mirror the embedded physics sim into render grids (plan §1d): one
    /// isotropic grid per body (shape cells 1:1, blitted once, re-posed each
    /// frame — transform updates never touch voxel data) plus a small
    /// world-voxel cylinder grid per wheel. `dt` drives the render-side
    /// wheel-spin accumulator (the stateless-wheel dividend: spin exists
    /// nowhere in the hashed sim). Ghost bodies (no shape) have nothing to
    /// draw and are skipped.
    pub fn sync_physics(&mut self, sim: &monada_script::PhysicsSim, dt: f64) {
        self.retire_dead_mirrors(sim);
        for body in sim.world.bodies() {
            let Some(shape) = body.shape() else { continue };
            let (origin, rot) = body_grid_pose(
                body.position(),
                body.orientation(),
                body.com_in_shape(),
            );
            let mirror = self.body_mirrors.entry(body.id().0).or_insert_with(|| {
                let grid = new_prop_grid(&mut self.scene, SCALE);
                BodyMirror {
                    grid,
                    blitted: usize::MAX,
                    dims: IVec3::ONE,
                    wheels: Vec::new(),
                }
            });

            // Blit the shape once; re-blit when the occupied count changes
            // (the D3 carve seam — remove_voxels shrinks it).
            let (dx, dy, dz) = shape.dims();
            let mut filled = 0usize;
            for z in 0..dz {
                for y in 0..dy {
                    for x in 0..dx {
                        filled += usize::from(shape.get(x, y, z).is_some());
                    }
                }
            }
            if mirror.blitted != filled {
                mirror.dims = IVec3::new(dx, dy, dz);
                if let Some(grid) = self.scene.grid_mut(mirror.grid) {
                    grid.set_rect(
                        IVec3::ZERO,
                        IVec3::new(dx - 1, dy - 1, dz - 1),
                        None,
                    );
                    for z in 0..dz {
                        for y in 0..dy {
                            for x in 0..dx {
                                if let Some(mat) = shape.get(x, y, z) {
                                    let c = IVec3::new(x, y, z);
                                    let color = self
                                        .phys_colors
                                        .get(&mat.0)
                                        .copied()
                                        .unwrap_or_else(|| material_color(mat.0));
                                    grid.set_rect(c, c, Some(VoxColor(color)));
                                }
                            }
                        }
                    }
                }
                mirror.blitted = filled;
            }

            if let Some(grid) = self.scene.grid_mut(mirror.grid) {
                grid.transform.origin = origin;
                grid.transform.rotation = rot;
            }

            // Wheels: cylinder grids on their anchors, dropped rest_length
            // down the body's suspension axis; steer comes from the retained
            // input, spin accumulates from ground speed.
            let q = dquat(body.orientation());
            let fwd_sim = q * DVec3::X;
            let speed = dvec3(body.linear_velocity()).dot(fwd_sim);
            for wheel in body.wheels() {
                let radius = wheel.def().radius.to_f64();
                let found = mirror.wheels.iter().position(|w| w.wheel == wheel.id().0);
                let idx = if let Some(i) = found {
                    i
                } else {
                    let grid = new_prop_grid(&mut self.scene, 1.0);
                    let r_world = radius * SCALE;
                    blit_wheel_cylinder(&mut self.scene, grid, r_world, WHEEL_HALF_WIDTH);
                    #[allow(clippy::cast_possible_truncation)]
                    mirror.wheels.push(WheelMirror {
                        wheel: wheel.id().0,
                        grid,
                        spin: 0.0,
                        extent: (r_world.ceil() as i32, WHEEL_HALF_WIDTH),
                    });
                    mirror.wheels.len() - 1
                };
                let wm = &mut mirror.wheels[idx];
                if radius > 1e-6 {
                    wm.spin = (wm.spin + speed / radius * dt) % std::f64::consts::TAU;
                }
                let steer = wheel.input().steer.to_f64();
                // Seat the wheel on the ACTUAL suspension length (see
                // `wheel_travel`), not the rest length.
                let anchor_sim = dvec3(body.position()) + q * dvec3(wheel.def().anchor);
                let down_sim = q * DVec3::NEG_Z;
                let rest = wheel.def().rest_length.to_f64();
                let travel = wheel_travel(&sim.terrain, anchor_sim, down_sim, rest, radius);
                let center_sim = anchor_sim + down_sim * travel;
                let wrot = mirror_half_turn()
                    * q
                    * DQuat::from_rotation_z(steer)
                    * DQuat::from_rotation_y(wm.spin);
                if let Some(grid) = self.scene.grid_mut(wm.grid) {
                    grid.transform.origin = volume_world_of(center_sim);
                    grid.transform.rotation = wrot;
                }
            }
        }
    }

    /// Retire mirrors of bodies the sim no longer has (D3: fully drilled
    /// away / a destruction split retiring an id): clear their voxels so
    /// the grids go empty (roxlap skips empty grids), then drop the
    /// entry. Body ids are never reused, so an id that vanished is gone
    /// for good.
    fn retire_dead_mirrors(&mut self, sim: &monada_script::PhysicsSim) {
        let live: BTreeSet<u64> = sim.world.bodies().iter().map(|b| b.id().0).collect();
        self.body_mirrors.retain(|id, mirror| {
            if live.contains(id) {
                return true;
            }
            if let Some(grid) = self.scene.grid_mut(mirror.grid) {
                grid.set_rect(IVec3::ZERO, mirror.dims - IVec3::ONE, None);
            }
            for wm in &mirror.wheels {
                let (r, hw) = wm.extent;
                if let Some(grid) = self.scene.grid_mut(wm.grid) {
                    grid.set_rect(IVec3::new(-r, -hw, -r), IVec3::new(r, hw, r), None);
                }
            }
            false
        });
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
            // Compose through the grid the entity rides (rotation + origin), the
            // same seat `build_instances` renders it at — hit-testing against the
            // bare `world_of(p)` mis-picks on a moved/rotated hull.
            let w = self.place(e, p);
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
    pub fn ground_sim(&self, origin: DVec3, dir: DVec3) -> Option<(f64, f64)> {
        // Heightfield-aware pick: march the ray against the sim collision
        // store, so a click on a plateau / ramp lands on the cell actually
        // under the cursor instead of its z=0 shadow (docs/plans/rts-demo.md
        // §1b). Maps whose terrain is all at z 0 (chess, RPG floor) hit at
        // exactly the old plane intersection; the plane stays the fallback
        // for rays that never meet painted terrain. Render-side only.
        let plane = ground_hit(origin, dir)?;
        let t_plane = (plane - origin).length();
        let below = |t: f64| {
            let p = origin + dir * t;
            let (sx, sy) = (-p.x / SCALE, p.y / SCALE);
            // The same nearest-cell rule the scripts' `cell()` uses.
            let (cx, cy) = ((sx + 0.5).floor() as i64, (sy + 0.5).floor() as i64);
            let surface = self.terrain.ground_height(cx, cy) as f64;
            GROUND_Z - p.z <= surface
        };
        // Coarse march (1/8 cell), then bisect the crossing step.
        let step = SCALE / 8.0;
        let mut t = 0.0;
        while t < t_plane && !below(t) {
            t += step;
        }
        if t >= t_plane {
            return Some((-plane.x / SCALE, plane.y / SCALE));
        }
        let (mut lo, mut hi) = ((t - step).max(0.0), t);
        for _ in 0..12 {
            let mid = (lo + hi) * 0.5;
            if below(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        let p = origin + dir * hi;
        Some((-p.x / SCALE, p.y / SCALE))
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
        // Debris puffs ride the static instance list — append before the
        // upload below.
        self.sync_puffs(dt);
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
        self.sync_rings(renderer);
        // Deck cutaway: clip the grid above the local crew's deck so the camera
        // sees inside (set before building `frame`, which doesn't touch scene).
        self.apply_deck_clip();
        // Fog of war: update the observer's mask + its twin grid (mutates the
        // scene), then style the twin below. `None` when vision is off.
        let fow_twin = self.update_fow(dt);
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
        // Style the fog twin (the dimmed last-seen grid the mask paints over).
        frame.fow = fow_twin.map(|g| (g, self.fow.as_ref().expect("fow mask present")));
        renderer.render(&mut self.scene, camera, &frame);

        self.draw_drag_rect(renderer, camera);
        if debug {
            self.draw_debug_footprints(renderer, camera);
        }
    }

    /// Mirror this frame's selection-marker instances onto the renderer's
    /// dynamic sprite layer (see the `ring_model` field for why the static
    /// path can't carry them on actor maps). Tear-down + re-add per frame:
    /// selection counts are tiny and `remove_sprite_instance` is an O(1)
    /// swap, so reconciliation would be complexity for nothing.
    /// Age, cull and draw the debris puffs: each is a static-set sprite
    /// instance (see `puff_models` on why not the dynamic layer), rising
    /// from its cell for [`PUFF_TTL`] seconds. Runs before the sprite-set
    /// upload each frame; `build_instances` has already rebuilt the
    /// instance list, so appending here survives exactly one frame — the
    /// immediate-mode contract the rest of the sprite path already lives
    /// by.
    fn sync_puffs(&mut self, dt: f64) {
        if self.puffs.is_empty() {
            return;
        }
        for p in &mut self.puffs {
            p.age += dt;
        }
        self.puffs.retain(|p| p.age < PUFF_TTL);
        for i in 0..self.puffs.len() {
            let (color, pos, age) = {
                let p = &self.puffs[i];
                (p.color, p.pos, p.age)
            };
            let model = if let Some(&m) = self.puff_models.get(&color) {
                m
            } else {
                // A chunky dust ball in the carved voxel's colour, unlit.
                let c = 3.0;
                let kv6 = Kv6::from_fn(7, 7, 7, |x, y, z| {
                    let (dx, dy, dz) = (
                        f64::from(x) - c,
                        f64::from(y) - c,
                        f64::from(z) - c,
                    );
                    (dx * dx + dy * dy + dz * dz <= c * c).then_some(VoxColor(color))
                });
                let mut s = Sprite::axis_aligned(kv6, [0.0, 0.0, 0.0]);
                s.flags = SPRITE_FLAG_NO_SHADING;
                self.sprites.models.push(s);
                let m = self.sprites.models.len() - 1;
                self.puff_models.insert(color, m);
                m
            };
            self.sprites.instances.push(SpriteInstanceDesc {
                model,
                pos: [
                    pos.x as f32,
                    pos.y as f32,
                    (pos.z - age * PUFF_RISE) as f32,
                ],
            });
        }
    }

    fn sync_rings(&mut self, renderer: &mut SceneRenderer) {
        if self.actors.is_empty() {
            return; // static path (chess) draws the marker itself
        }
        if self.ring_model.is_none() {
            self.ring_model =
                Some(renderer.add_sprite_model(&self.sprites.models[HIGHLIGHT_MODEL].kv6));
        }
        let Some(model) = self.ring_model else {
            return;
        };
        for id in self.ring_ids.drain(..) {
            renderer.remove_sprite_instance(id);
        }
        for inst in &self.sprites.instances {
            if inst.model == HIGHLIGHT_MODEL {
                if let Some(id) = renderer.add_sprite_instance(model, inst.pos) {
                    self.ring_ids.push(id);
                }
            }
        }
    }

    /// The active pointer-drag rectangle (`drag_begin` … `drag_end`), as a
    /// ground-space outline glued to the terrain — WYSIWYG for a box
    /// select: the drawn region IS the sim region the map will query.
    /// Corners ride each cell's own ground height, lifted a hair so the
    /// lines never z-fight the floor. No-op when no drag is active.
    fn draw_drag_rect(&self, renderer: &mut SceneRenderer, camera: &Camera) {
        let Some((ax, ay)) = self.drag_anchor else {
            return;
        };
        let Some((bx, by)) = self.cursor_ground else {
            return;
        };
        let col = OverlayColor(0xFFE8_F4A0); // pale WC3-ish selection green
        let ground = |sx: f64, sy: f64| {
            let (cx, cy) = ((sx + 0.5).floor() as i64, (sy + 0.5).floor() as i64);
            GROUND_Z - self.terrain.ground_height(cx, cy) as f64 - 2.0
        };
        // Sim rect corners → world (world X is mirrored; see world_of). The
        // rectangle is screen-aligned (`drag_quad_sim` at the live camera yaw),
        // so the drawn outline is exactly the region `drag_end` hands the map.
        let world = |sx: f64, sy: f64| [-sx * SCALE, sy * SCALE, ground(sx, sy)];
        let q = drag_quad_sim(self.camera.yaw, (ax, ay), (bx, by));
        let corners = q.map(|(sx, sy)| world(sx, sy));
        let mut lines = Vec::with_capacity(4);
        for i in 0..4 {
            lines.push(Line3 {
                a: corners[i],
                b: corners[(i + 1) % 4],
                color: col,
                width_px: 2.0,
                depth_test: false,
            });
        }
        renderer.draw_lines(camera, &lines);
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

    fn entity_set_grid(&mut self, entity: i64, grid: i64) {
        let e = EntityId(entity as u64);
        // Resolve the script's grid handle (index into `grids`) to a GridId;
        // an out-of-range handle is ignored, matching `voxel_fill_in`.
        if let Some(&id) = usize::try_from(grid).ok().and_then(|i| self.grids.get(i)) {
            self.entity_grid.insert(e, id);
        }
    }

    fn grid_orient(&mut self, grid: i64, axis: FixedVec3, angle: Fixed) {
        // Resolve the handle as `voxel_fill_in`/`entity_set_grid` do.
        let Some(&id) = usize::try_from(grid).ok().and_then(|i| self.grids.get(i)) else {
            return;
        };
        // Build a full 3D rotation from axis + angle. The axis is in the grid's
        // LOCAL frame (the same frame `voxel_fill_in` paints in); glam requires a
        // unit axis, so normalise here and drop a zero-length one (leaves the
        // pose unchanged). The whole pose is replaced each call, so the script
        // drives orientation from hashed sim state and never accumulates float
        // drift render-side. Origin is untouched — the grid still turns about its
        // local origin (pivot compensation is a later concern).
        let a = DVec3::new(axis.x.to_f64(), axis.y.to_f64(), axis.z.to_f64());
        let Some(unit) = a.try_normalize() else {
            return;
        };
        if let Some(g) = self.scene.grid_mut(id) {
            g.transform.rotation = DQuat::from_axis_angle(unit, angle.to_f64());
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
        // Column maps mirror paints into the sim-space column store for
        // the voxel_solid / ground_height / nav queries. Volume maps do
        // NOT feed it at all: their hashed store is the sim-side
        // VolumeStore, the column model cannot represent their clears
        // (hole punches), and a half-fed copy would answer those queries
        // with silently divergent state. On a volume map they read an
        // empty world — by design, not by accident.
        if !self.volume {
            self.terrain.fill(x0, y0, z0, x1, y1, z1);
        }
        let (lo, hi) = if self.volume {
            cell_box_to_volume_grid(x0, y0, z0, x1, y1, z1)
        } else {
            sim_box_to_world(x0, y0, z0, x1, y1, z1)
        };
        let id = self.world_grid();
        if let Some(grid) = self.scene.grid_mut(id) {
            grid.set_rect(lo, hi, Some(VoxColor(color as u32)));
        }
    }

    fn grid_spawn(&mut self, wx: i64, wy: i64, wz: i64) -> i64 {
        // `(wx, wy, wz)` is a SIM cell offset, so the grid composes with the
        // mirrored/scaled voxels `voxel_fill_in` paints inside it: a spawn at
        // sim cell `wx` shifts world X by `-wx·SCALE` (world X is mirrored — see
        // `world_of`), Y by `+wy·SCALE`, and z by `-wz` (z unscaled, z-down). At
        // `(0, 0, 0)` this is the identity, the common world-origin case.
        let pos = glam::DVec3::new(-(wx as f64) * SCALE, wy as f64 * SCALE, -(wz as f64));
        let id = self.scene.add_grid(GridTransform::at(pos));
        let idx = self.grids.len() as i64;
        self.grids.push(id);
        idx
    }

    #[allow(clippy::too_many_arguments)]
    fn voxel_fill_in(
        &mut self,
        grid: i64,
        x0: i64,
        y0: i64,
        z0: i64,
        x1: i64,
        y1: i64,
        z1: i64,
        color: i64,
    ) {
        // Render-side only — does NOT update self.terrain. Same sim→world
        // coordinate transform as voxel_fill (world X mirrored, z unscaled).
        let Some(&id) = self.grids.get(grid as usize) else {
            return;
        };
        let (lo, hi) = sim_box_to_world(x0, y0, z0, x1, y1, z1);
        if let Some(g) = self.scene.grid_mut(id) {
            g.set_rect(lo, hi, Some(VoxColor(color as u32)));
        }
    }

    fn voxel_clear(&mut self, x: i64, y: i64, z: i64) {
        if self.volume {
            // A true one-cell hole punch — the tunnel primitive. Matches the
            // sim-side VolumeStore semantics exactly (the D1 column-clear
            // mismatch, retired). The carved voxel's colour seeds a debris
            // puff at the cell (plan §1d) — read it BEFORE the punch.
            if let Some(id) = self.world_grid {
                let (lo, hi) = cell_box_to_volume_grid(x, y, z, x, y, z);
                if let Some(grid) = self.scene.grid_mut(id) {
                    if let Some(color) = grid.voxel_color(lo) {
                        let center = volume_world_of(DVec3::new(
                            x as f64 + 0.5,
                            y as f64 + 0.5,
                            z as f64 + 0.5,
                        ));
                        self.puffs.push(Puff {
                            pos: center,
                            color: color.0,
                            age: 0.0,
                        });
                    }
                    grid.set_rect(lo, hi, None);
                }
            }
            return;
        }
        // Truncate the collision column; the previous top bounds the render
        // span, so the clear erases exactly what was solid (an unpainted
        // column is a no-op — nothing to erase, nothing to collide with).
        let Some(prev_top) = self.terrain.clear_above(x, y, z) else {
            return;
        };
        if prev_top < z {
            return; // already clear at and above z
        }
        // One column (x, y), clearing sim heights z..=prev_top — the same
        // sim→world mapping as voxel_fill.
        let (lo, hi) = sim_box_to_world(x, y, z, x, y, prev_top);
        // Only clears an existing world grid — a clear before any paint is a
        // no-op, no need to materialize an empty grid for it.
        if let Some(id) = self.world_grid {
            if let Some(grid) = self.scene.grid_mut(id) {
                grid.set_rect(lo, hi, None);
            }
        }
    }

    fn voxel_set(&mut self, x: i64, y: i64, z: i64, color: i64) {
        // Column store: fed on column maps only — see voxel_fill.
        if !self.volume {
            self.terrain.set(x, y, z);
        }
        // One sim CELL layer (SCALE×SCALE×1 world voxels), with the same
        // world-X mirror as voxel_fill — this used to paint a single
        // world voxel at UNMIRRORED +x, i.e. a speck in the void on the
        // wrong side of the map, silently diverging from the collision
        // store's full-cell semantics.
        let (lo, hi) = if self.volume {
            cell_box_to_volume_grid(x, y, z, x, y, z)
        } else {
            sim_box_to_world(x, y, z, x, y, z)
        };
        let id = self.world_grid();
        if let Some(grid) = self.scene.grid_mut(id) {
            grid.set_rect(lo, hi, Some(VoxColor(color as u32)));
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
        let world = self.world_grid();
        let Some(grid) = self.scene.grid_mut(world) else {
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
        let world = self.world_grid(); // before the `autotiler` borrow below
        let autotiler = &self.autotiler;
        let Some(grid) = self.scene.grid_mut(world) else {
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
        self.highlighted.clear();
        self.highlighted.insert(EntityId(entity as u64));
    }
    fn highlight_add(&mut self, entity: i64) {
        self.highlighted.insert(EntityId(entity as u64));
    }
    fn highlight_clear(&mut self) {
        self.highlighted.clear();
    }
    fn highlighted_all(&self) -> Vec<i64> {
        self.highlighted.iter().map(|e| e.0 as i64).collect()
    }
    fn drag_begin(&mut self) {
        self.drag_anchor = self.cursor_ground;
    }
    fn drag_end(&mut self) -> Vec<FixedVec3> {
        let Some((ax, ay)) = self.drag_anchor.take() else {
            return Vec::new();
        };
        // A release off-world (cursor over the sky) collapses to a click.
        let (bx, by) = self.cursor_ground.unwrap_or((ax, ay));
        // The screen-aligned quad (matches what `draw_drag_rect` drew): four
        // corners wound around the rect, so the map can point-test units in the
        // rotated box instead of a world-axis bbox.
        let corner =
            |x: f64, y: f64| FixedVec3::new(Fixed::from_f64(x), Fixed::from_f64(y), Fixed::ZERO);
        drag_quad_sim(self.camera.yaw, (ax, ay), (bx, by))
            .iter()
            .map(|&(x, y)| corner(x, y))
            .collect()
    }
    fn highlighted(&self) -> i64 {
        self.highlighted.iter().next().map_or(-1, |e| e.0 as i64)
    }

    fn status(&mut self, text: &str) {
        self.status = text.to_string();
    }

    fn camera_focus(&mut self, point: FixedVec3) {
        self.camera.center = self.entity_world_of(point);
    }

    fn camera_focus_entity(&mut self, entity: i64, point: FixedVec3) {
        // Compose through the grid the entity rides (the same seat
        // `build_instances` renders it at), so following a crew member on a
        // rotating hull tracks it instead of aiming at the un-transformed cell.
        self.camera.center = self.place(EntityId(entity as u64), point);
    }

    fn camera_angle(&mut self, yaw: Fixed, pitch: Fixed) {
        self.camera.yaw = yaw.to_f64();
        self.camera.pitch = pitch.to_f64();
    }

    fn camera_dist(&mut self, dist: Fixed) {
        // Clamp to the same range the orbit nudge uses (world voxels).
        self.camera.dist = dist.to_f64().clamp(60.0, 2000.0);
    }

    fn camera_pan(&mut self, dx: Fixed, dy: Fixed) {
        // A sim-space delta through `world_of`'s linear part: world-X is
        // mirrored, x/y scale by SCALE, z untouched. Accumulates on the
        // stored focus, so a stateless local layer can scroll the view.
        self.camera.center.x -= dx.to_f64() * SCALE;
        self.camera.center.y += dy.to_f64() * SCALE;
    }

    fn camera_cutout(&mut self, radius: Fixed, feather: Fixed) {
        let r = radius.to_f64();
        self.cutout = (r > 0.0).then(|| (r, feather.to_f64().max(0.0)));
    }

    fn deck_clip(&mut self, z_lo: i64, z_hi: i64) {
        // roxlap's `Grid::z_clip` cuts one side: voxels with grid-z BELOW the
        // threshold (smaller grid-z = higher up, z-down) become air. So clip at
        // the band top (sim `z_hi`) to cut everything above it. A band whose top
        // is the world's tallest voxel yields a threshold at/below all geometry,
        // cutting nothing.
        self.deck_clip = Some(deck_clip_world_z(z_hi));
        // The full sim band drives the fog of war's `DeckBand` too.
        self.deck_band = Some((z_lo, z_hi));
    }

    fn vision_observer(&mut self, entity: i64) {
        // Legacy 1-arg form: fog rides the world grid.
        self.set_observer(entity, None);
    }

    fn vision_observer_in(&mut self, entity: i64, grid: i64) {
        // Fog rides the named `grid_spawn` grid (the crew's hull); an out-of-range
        // handle leaves it on the world grid rather than blindly picking one.
        let g = self.grids.get(grid as usize).copied();
        self.set_observer(entity, g);
    }

    fn vision_config(&mut self, cone_deg: i64, range: i64, peripheral: i64) {
        let cfg = (cone_deg, range, peripheral);
        if cfg != self.vision_cfg {
            self.vision_cfg = cfg;
            self.drop_fow(); // rebuild with the new tuning
        }
    }

    fn vision_hear(&mut self, x: i64, y: i64, z: i64, loudness: Fixed) {
        self.vision_hears
            .push((x, y, z, loudness.to_f64().clamp(0.0, 1.0) as f32));
    }

    fn phys_material_color(&mut self, mat: i64, color: i64) {
        self.phys_colors.insert(mat as u16, color as u32);
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

    fn nav_block(&mut self, x: i64, y: i64, on: bool) {
        self.terrain.nav_block(x, y, on);
    }

    fn nav_path(&self, x0: i64, y0: i64, x1: i64, y1: i64, max_step: i64) -> Vec<FixedVec3> {
        self.terrain.nav_path(x0, y0, x1, y1, max_step)
    }
}

#[cfg(test)]
mod tests {
    //! The renderer-free half of the actor bridge (the GPU/clip half needs a
    //! window): GIF decode, per-entity binding, and the actor-target the
    //! frame computes for `update_actors` to apply.
    use super::*;
    use monada_sim::World;

    /// A volume-map carve spawns a debris puff carrying the carved
    /// voxel's colour; clearing air spawns nothing; the puff dies after
    /// [`PUFF_TTL`].
    #[test]
    fn volume_carve_spawns_and_retires_a_puff() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        r.set_volume_terrain();
        r.voxel_fill(0, 0, 0, 3, 3, 1, 0x8070_5838);
        r.voxel_clear(1, 1, 1);
        assert_eq!(r.puffs.len(), 1, "one carve, one puff");
        assert_eq!(r.puffs[0].color, 0x8070_5838, "puff wears the voxel's colour");
        // Clearing already-empty cells is silent.
        r.voxel_clear(1, 1, 1);
        r.voxel_clear(50, 50, 50);
        assert_eq!(r.puffs.len(), 1);
        // Age it out through the draw path.
        r.sync_puffs(PUFF_TTL + 0.01);
        assert!(r.puffs.is_empty(), "puffs die after PUFF_TTL");
    }

    /// The body-mirror pose math: grid local = shape cells, so `origin +
    /// rot · (SCALE · l)` must land shape point `l` exactly where the sim
    /// says — through the CoM rebase, the body orientation, and the
    /// mirror half-turn.
    #[test]
    fn body_grid_pose_composes_mirror_com_and_rotation() {
        let fx = Fixed::from_int;
        let pos = FixedVec3::new(fx(10), fx(20), fx(5));
        let com = FixedVec3::new(Fixed::from_ratio(3, 2), fx(2), Fixed::ONE);

        // Identity orientation: the shape's CoM cell lands on the body
        // position.
        let (origin, rot) = body_grid_pose(pos, monada_fixed::FixedQuat::IDENTITY, com);
        let w = origin + rot * (dvec3(com) * SCALE);
        assert!((w - volume_world_of(dvec3(pos))).length() < 1e-6);

        // A quarter-turn about +z: one cell nose-ward of the CoM in shape
        // space must land one cell +y of the position in sim space.
        let q = monada_fixed::FixedQuat::from_axis_angle(
            FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE),
            monada_fixed::trig::FRAC_PI_2,
        );
        let (origin, rot) = body_grid_pose(pos, q, com);
        let l = dvec3(com) + DVec3::X;
        let w = origin + rot * (l * SCALE);
        let expect = volume_world_of(dvec3(pos) + DVec3::Y);
        assert!(
            (w - expect).length() < 0.05,
            "rotated shape point off by {:?}",
            w - expect
        );
    }

    /// The render-side wheel seat: on flat terrain the travel is the
    /// surface distance minus the radius (clamped to rest); airborne it
    /// is full extension; bottomed out it clamps to zero.
    #[test]
    fn wheel_travel_seats_on_terrain() {
        let mut terrain = monada_script::VolumeStore::new();
        // A slab: cells z 0..1 solid, surface at z = 2.
        terrain.fill(0, 0, 0, 7, 7, 1, monada_script::MaterialId(0));
        let down = DVec3::NEG_Z;
        // Anchor 2.0 above the surface: hit at t = 2.0, travel 1.5 → but
        // radius 0.5 gives 1.5 == rest — full extension, wheel bottom
        // exactly on the surface.
        let t = wheel_travel(&terrain, DVec3::new(4.0, 4.0, 4.0), down, 1.5, 0.5);
        assert!((t - 1.5).abs() < 0.1, "flush contact, got {t}");
        // The equilibrium stance: chassis sank 0.5, anchor at 3.5 → hit
        // at 1.5, travel 1.0 — the compression the sim implies.
        let t = wheel_travel(&terrain, DVec3::new(4.0, 4.0, 3.5), down, 1.5, 0.5);
        assert!((t - 1.0).abs() < 0.1, "half-compressed, got {t}");
        // Airborne (off the slab): full extension.
        let t = wheel_travel(&terrain, DVec3::new(40.0, 40.0, 4.0), down, 1.5, 0.5);
        assert!((t - 1.5).abs() < 1e-9, "airborne, got {t}");
        // Bottomed out (anchor at the surface): clamps to zero.
        let t = wheel_travel(&terrain, DVec3::new(4.0, 4.0, 2.0), down, 1.5, 0.5);
        assert!(t.abs() < 0.1, "bottomed out, got {t}");
    }

    /// The automatic body mirror, end-to-end through the REAL digger map:
    /// init spawns the vehicle, `sync_physics` builds its grid, and a ray
    /// down the spawn column meets the body's voxels well above the
    /// terrain slab — proving the mirror grid exists, is posed by the
    /// isotropic sim→world map, and carries the shape blit.
    #[test]
    fn digger_body_mirror_renders_above_the_terrain() {
        use std::sync::{Arc, Mutex};

        use monada_script::{RhaiBackend, ScriptBackend as _, SharedBridge};

        let mut mr = MapRender::new(BTreeMap::new(), None, &[]);
        mr.set_volume_terrain();
        let render = Arc::new(Mutex::new(mr));
        let bridge: SharedBridge = render.clone();
        let world = monada_script::shared_world(1);
        let mut backend = RhaiBackend::new(world);
        backend.set_bridge(&bridge);
        let phys = monada_script::shared_physics(30);
        backend.set_physics(&phys);
        backend.set_tick_hz(30);
        backend
            .load(include_str!("../../monada-digger/map/scripts/main.rhai"))
            .expect("compile digger");
        backend.on_init().expect("digger init");

        let mut r = render.lock().unwrap();
        r.sync_physics(&phys.lock().expect("physics mutex"), 1.0 / 60.0);

        // Straight down the spawn cell (sim 25, 120). The chassis spans
        // sim z 4..6 → WORLD z 4..36 (isotropic: 100 − 16·z); the slab
        // top surface is at world z 68. A second sync must not disturb
        // the pose (blit-once contract).
        let col = DVec3::new(-25.5 * SCALE, 120.5 * SCALE, 0.0);
        let down = DVec3::new(0.0, 0.0, 1.0);
        let body_hit = r
            .scene
            .raycast_clipped(col, down, 4096.0)
            .expect("ray should meet the mirrored chassis");
        assert!(
            body_hit.world.z < 40.0,
            "first hit should be the body mirror (world z 4..36), got z {}",
            body_hit.world.z
        );
        r.sync_physics(&phys.lock().expect("physics mutex"), 1.0 / 60.0);
        let again = r
            .scene
            .raycast_clipped(col, down, 4096.0)
            .expect("mirror survives a re-sync");
        assert_eq!((body_hit.grid, body_hit.voxel), (again.grid, again.voxel));

        // Off the vehicle, the same ray reaches the terrain slab instead.
        let apron = DVec3::new(-100.5 * SCALE, 20.5 * SCALE, 0.0);
        let terrain_hit = r
            .scene
            .raycast_clipped(apron, down, 4096.0)
            .expect("apron ray hits the slab");
        assert!(
            (terrain_hit.world.z - 68.0).abs() < 0.01,
            "slab top surface at world z 68, got {}",
            terrain_hit.world.z
        );
    }

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
        r.voxel_set(cx, cy, 0, 0x80AA_AAAA); // lower deck floor (sim-z 0) — auto world grid
        r.voxel_set(cx, cy, 4, 0x8055_5555); // upper deck floor (sim-z 4)

        // `voxel_set` places cell (x, y) with the same world-X mirror as
        // `voxel_fill` (world x ∈ [-(x+1)·S, -x·S)); ray straight down the
        // mirrored cell centre (+z is "down", world z-down) from above both.
        let col = DVec3::new(-(cx as f64 + 0.5) * SCALE, (cy as f64 + 0.5) * SCALE, 0.0);
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

    /// An off-origin `grid_spawn` composes its offset with the mirror + SCALE +
    /// z-down transform `voxel_fill_in` paints inside it: the grid's LOCAL origin
    /// cell must land on the exact world voxel of the sim cell it was spawned at,
    /// not a raw-unit, unmirrored offset (the pre-fix bug rendered it elsewhere).
    #[test]
    fn grid_spawn_off_origin_composes_the_mirror_transform() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let (wx, wy, wz) = (3_i64, 2_i64, 5_i64);
        // A grid offset by sim cell (wx, wy, wz), painting only its local origin
        // cell (0,0,0). No world grid painted — the ray can only hit this grid.
        let g = r.grid_spawn(wx, wy, wz);
        assert!(g >= 0, "grid handle allocated");
        r.voxel_fill_in(g, 0, 0, 0, 0, 0, 0, 0x8055_5555);

        // Ray straight down (+z, z-down) the MIRRORED centre of world cell
        // (wx, wy). It reaches the voxel only if the offset was mirrored/scaled
        // like `voxel_set(wx, wy, wz)` would place it.
        let col = DVec3::new(-(wx as f64 + 0.5) * SCALE, (wy as f64 + 0.5) * SCALE, 0.0);
        let down = DVec3::new(0.0, 0.0, 1.0);
        let hit = r
            .scene
            .raycast_clipped(col, down, 4096.0)
            .expect("ray hits the off-origin grid at the mirrored column");
        assert_eq!(hit.grid, r.grids[g as usize], "hit the spawned grid");
        // World z-down: sim height wz sits at world z GROUND_Z - wz (unscaled).
        assert!(
            (hit.world.z - (GROUND_Z - wz as f64)).abs() < 1.0,
            "off-origin cell at world z GROUND_Z - wz, got {}",
            hit.world.z
        );
    }

    /// An entity bound with `entity_set_grid` rides its grid's transform: its
    /// sprite seats at `rotation · world_of(p) + origin`, NOT the bare global
    /// `world_of(p)`. Spawn a grid off-origin AND turned 90° about z, bind an
    /// entity to it, and prove the built instance lands at the composed column —
    /// while a second, UNBOUND entity still seats in the global frame.
    #[test]
    fn bound_entity_rides_its_grids_transform() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let (wx, wy, wz) = (4_i64, 3_i64, 0_i64);
        let g = r.grid_spawn(wx, wy, wz);
        let gid = r.grids[g as usize];
        // Turn the hull a quarter-turn about local +z via the real API.
        r.grid_orient(
            g,
            FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::from_int(1)),
            Fixed::from_f64(std::f64::consts::FRAC_PI_2),
        );
        let (origin, rotation) = {
            let t = &r.scene.grid(gid).expect("grid").transform;
            (t.origin, t.rotation)
        };

        // A box sprite model, bound to a crew-like entity that rides the grid.
        let model = r.model_box(2, 2, 2, 0x8055_5555);
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let bound = world.spawn(arch);
        let unbound = world.spawn(arch);
        let p = FixedVec3::new(Fixed::from_int(5), Fixed::from_int(2), Fixed::ZERO);
        world.set_position(bound, p);
        world.set_position(unbound, p);
        r.entity_set_model(bound.0 as i64, model);
        r.entity_set_model(unbound.0 as i64, model);
        r.entity_set_grid(bound.0 as i64, g);

        r.build_instances(&world);

        // Two sprites (no highlights). The bound one sits at the composed column;
        // the unbound one at the bare global column. x/y pin the rotation (z is
        // seated by the model drop, so only x/y are asserted).
        let composed = rotation * world_of(p) + origin;
        let global = world_of(p);
        let xy = |d: DVec3| (d.x as f32, d.y as f32);
        let (cx, cy) = xy(composed);
        let (gx, gy) = xy(global);
        assert!(
            (cx - gx).abs() > 1.0 || (cy - gy).abs() > 1.0,
            "test is only decisive if the composed and global columns differ"
        );
        let has = |x: f32, y: f32| {
            r.sprites.instances.iter().any(|i| {
                i.model != HIGHLIGHT_MODEL
                    && (i.pos[0] - x).abs() < 0.01
                    && (i.pos[1] - y).abs() < 0.01
            })
        };
        assert!(
            has(cx, cy),
            "bound entity seats at the grid-composed column"
        );
        assert!(
            has(gx, gy),
            "unbound entity seats at the bare global column"
        );
    }

    /// `grid_orient` sets a *full* 3D rotation, not a yaw: a quarter-turn about
    /// the local +x axis (a pitch) lifts an in-plane point out of the horizontal
    /// plane — something a z-only "yaw" scalar could never express. A zero-length
    /// axis defines no rotation and is ignored (the pose is left untouched).
    #[test]
    fn grid_orient_is_a_full_3d_rotation() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn(0, 0, 0);
        let gid = r.grids[g as usize];

        // Pitch 90° about local +x: +y turns onto +z (out of the horizontal plane).
        r.grid_orient(
            g,
            FixedVec3::new(Fixed::from_int(1), Fixed::ZERO, Fixed::ZERO),
            Fixed::from_f64(std::f64::consts::FRAC_PI_2),
        );
        let turned = r.scene.grid(gid).expect("grid").transform.rotation * DVec3::Y;
        assert!(
            turned.z.abs() > 0.9,
            "a pitch about +x lifts +y out of the horizontal plane, got {turned:?}"
        );

        // A zero-length axis can't define a rotation — leave the pose as it was.
        r.grid_orient(g, FixedVec3::ZERO, Fixed::from_f64(1.0));
        let after = r.scene.grid(gid).expect("grid").transform.rotation * DVec3::Y;
        assert!(
            (after - turned).length() < 1e-9,
            "a zero-length axis is ignored — pose unchanged"
        );
    }

    /// Fog rides an OFF-ORIGIN `grid_spawn` hull: the observer's world pose and
    /// any heard cell must be re-based into the grid's LOCAL voxels, or the mask
    /// stamps empty space at the grid's world offset instead of on the hull. A
    /// heard blob is deterministic (no LOS / lighting), so it pins the re-basing
    /// exactly — the live Heard cell lands on the grid-local column of the sound,
    /// not the world column (which the grid origin shifts `wx·SCALE` cells away).
    #[test]
    fn fog_rides_off_origin_grid_in_grid_local_space() {
        use roxlap_scene::fow::CellState;

        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        // A large offset so the un-rebased world column is far outside the
        // observer's vision range — the decisive assert below can't pass by a
        // coincidental cone/blob overlap.
        let (wx, wy, wz) = (20_i64, 12_i64, 2_i64);
        let g = r.grid_spawn(wx, wy, wz);
        // A floor inside the hull (grid-local sim cells). The heard path needs no
        // geometry, but this mirrors a real hull the observer stands on.
        r.voxel_fill_in(g, 0, 0, 0, 8, 8, 0, 0x8055_5f6b);
        r.vision_config(110, 6, 3);
        r.deck_clip(0, 3); // sets the deck band the mask needs

        // Observer entity at GRID-LOCAL sim (4, 4, 0) — as real crew are stored.
        // `vision_observer_in` binds it to grid `g`, so `place` composes this
        // through the hull transform to the true world seat (== WORLD sim
        // (wx+4, wy+4, wz)); `update_fow` then rebases back to grid-local.
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        world.set_position(
            e,
            FixedVec3::new(Fixed::from_int(4), Fixed::from_int(4), Fixed::ZERO),
        );
        r.vision_observer_in(e.0 as i64, g);

        // A sound at WORLD sim (wx+2, wy+6): its grid-local column is sim (2, 6).
        let (hx, hy) = (wx + 2, wy + 6);
        // Frame 1 captures the pose + queues the blob; frame 2's `update` applies
        // the blob into the mask (hear is queued after update within a frame).
        r.build_instances(&world);
        r.vision_hear(hx, hy, wz, Fixed::from_int(1));
        let _ = r.update_fow(0.016);
        r.build_instances(&world);
        let _ = r.update_fow(0.016);

        let mask = r.fow.as_ref().expect("mask built");
        // The blob is centred on the GRID-LOCAL column of the sound —
        // `world_of(local sim (2, 6))`, deck 0.
        let local = world_of(FixedVec3::new(
            Fixed::from_int((hx - wx) as i32),
            Fixed::from_int((hy - wy) as i32),
            Fixed::ZERO,
        ));
        let local_cell = IVec2::new(local.x.floor() as i32, local.y.floor() as i32);
        let (state, intensity) = mask.state(0, local_cell);
        assert!(
            matches!(state, CellState::Visible | CellState::Heard) && intensity > 0,
            "heard cell live at the grid-local column {local_cell:?}, got {state:?}/{intensity}"
        );

        // Decisive: the WORLD column (grid origin NOT subtracted) is far off the
        // hull and must stay Unseen — proves the re-basing, not a stray overlap.
        let world_pt = world_of(FixedVec3::new(
            Fixed::from_int(hx as i32),
            Fixed::from_int(hy as i32),
            Fixed::ZERO,
        ));
        let world_cell = IVec2::new(world_pt.x.floor() as i32, world_pt.y.floor() as i32);
        assert_eq!(
            mask.state(0, world_cell).0,
            CellState::Unseen,
            "un-rebased world column {world_cell:?} must stay Unseen"
        );
    }

    /// A yawing hull: the fog viewpoint must be expressed in the grid's rotated
    /// frame, not just translated. Spawn a grid at the world origin, turn it 90°
    /// about z, then hear a sound. The blob must land at the grid-local column
    /// `rotation.inverse() * world_of(sound)`, NOT at the un-rotated world column
    /// — proving the rotation term, not merely the origin subtraction, is applied.
    #[test]
    fn fog_rides_a_rotated_grid_in_grid_local_space() {
        use roxlap_scene::fow::CellState;

        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn(0, 0, 0); // at the world origin ⇒ pure rotation below
        let gid = r.grids[g as usize];
        // Turn the hull a quarter-turn about world +z. `grid_spawn` only sets a
        // translation, so there is no host API for this yet — poke the transform
        // directly, exactly as a future `grid_rotate` would.
        let grid_rot = DQuat::from_rotation_z(std::f64::consts::FRAC_PI_2);
        r.scene.grid_mut(gid).expect("grid").transform.rotation = grid_rot;

        r.voxel_fill_in(g, 0, 0, 0, 8, 8, 0, 0x8055_5f6b);
        r.vision_config(110, 6, 3);
        r.deck_clip(0, 3);

        // Observer at the world origin; a sound far out along sim +x so the
        // rotated and un-rotated columns are far apart (a small heard blob can't
        // cover both, and the cone can't reach either).
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        world.set_position(e, FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ZERO));
        r.vision_observer_in(e.0 as i64, g);

        let (hx, hy) = (10_i64, 0_i64);
        r.build_instances(&world);
        r.vision_hear(hx, hy, 0, Fixed::from_int(1));
        let _ = r.update_fow(0.016);
        r.build_instances(&world);
        let _ = r.update_fow(0.016);

        let mask = r.fow.as_ref().expect("mask built");
        let origin = r.scene.grid(gid).expect("grid").transform.origin;
        let sound = world_of(FixedVec3::new(
            Fixed::from_int(hx as i32),
            Fixed::from_int(hy as i32),
            Fixed::ZERO,
        ));
        // Where the code should place the blob: rotate the origin-relative world
        // point into the grid frame.
        let local = grid_rot.inverse() * (sound - origin);
        let local_cell = IVec2::new(local.x.floor() as i32, local.y.floor() as i32);
        let (state, intensity) = mask.state(0, local_cell);
        assert!(
            matches!(state, CellState::Visible | CellState::Heard) && intensity > 0,
            "heard cell live at the rotated grid-local column {local_cell:?}, got {state:?}/{intensity}"
        );

        // Decisive: the un-rotated column (origin subtracted but rotation NOT
        // applied) is a quarter-turn away and must stay Unseen.
        let flat_cell = IVec2::new(
            (sound.x - origin.x).floor() as i32,
            (sound.y - origin.y).floor() as i32,
        );
        assert_eq!(
            mask.state(0, flat_cell).0,
            CellState::Unseen,
            "un-rotated column {flat_cell:?} must stay Unseen"
        );
    }

    /// The vision *cone* rides the hull too, not just the position. The crew's
    /// facing is hull-relative, the mask is built grid-local, and the twin grid
    /// re-applies the grid rotation when it renders — so the grid-local cone must
    /// be independent of how the hull is turned (it is hull-fixed; the twin does
    /// the world rotation). Regression for the bug where the facing was de-rotated
    /// by `grid_rot_inv`, cancelling the twin and pinning the cone to one world
    /// direction while the hull spun under it.
    #[test]
    fn fog_cone_is_hull_fixed_under_rotation() {
        use roxlap_scene::fow::CellState;

        // Default facing yaw is 0 ⇒ the grid-local cone points local -x
        // (`(-cos 0, sin 0) = (-1, 0)`). A quarter-turn of the hull must NOT move
        // that grid-local cone: a column far down local -x stays lit and its
        // perpendicular stays dark. Both probes sit beyond the peripheral radius
        // (a full-circle near reveal) so only the directional cone can reach them.
        // Under the old de-rotating code the grid-local cone would swing to local
        // +y, flipping both assertions.
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn(0, 0, 0); // world origin ⇒ pure rotation, no offset
        let gid = r.grids[g as usize];
        r.scene.grid_mut(gid).expect("grid").transform.rotation =
            DQuat::from_rotation_z(std::f64::consts::FRAC_PI_2);

        r.voxel_fill_in(g, -80, -80, 0, 80, 80, 0, 0x8055_5f6b); // floor spanning the cone reach
        r.vision_config(110, 6, 3);
        r.deck_clip(0, 3);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        world.set_position(e, FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ZERO));
        r.vision_observer_in(e.0 as i64, g);

        // Two frames: the cone needs a prior mask to accumulate into.
        r.build_instances(&world);
        let _ = r.update_fow(0.016);
        r.build_instances(&world);
        let _ = r.update_fow(0.016);

        let mask = r.fow.as_ref().expect("mask built");
        let in_cone = IVec2::new(-60, 0);
        let across = IVec2::new(0, 60);
        assert_eq!(
            mask.state(0, in_cone).0,
            CellState::Visible,
            "the hull-fixed cone (local -x) lights {in_cone:?} regardless of hull rotation"
        );
        assert_eq!(
            mask.state(0, across).0,
            CellState::Unseen,
            "the perpendicular column {across:?} stays dark — the cone did not swing with the hull"
        );
    }

    /// The rendered hull (the twin grid) must keep turning even on a frame where
    /// the fog mask does not change. The twin — not the real grid, which is
    /// `render_excluded` — draws the hull, and `FowTwin::sync` only mirrors the
    /// real grid's transform on a NON-quiet frame (mask version or voxel mutation
    /// changed). A hull that merely rotates bumps neither, so once the crew is
    /// idle and the mask settles, `sync` early-outs and — without the host's
    /// per-frame transform copy — the twin freezes while the real grid keeps
    /// turning: the hull stalls on screen and the crew slides off it. Regression
    /// for that: rotate the real grid on a settled (quiet) frame and confirm the
    /// twin tracks it.
    #[test]
    fn fog_twin_tracks_hull_rotation_on_a_quiet_frame() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn(0, 0, 0);
        r.voxel_fill_in(g, -40, -40, 0, 40, 40, 0, 0x8055_5f6b); // floor across the cone
        r.vision_config(110, 6, 3);
        r.deck_clip(0, 3);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        // Seat the observer a hair off the cell centre (sim +1/32) so its
        // grid-local feet land mid-voxel, NOT on an integer cell boundary. Re-
        // basing maps the world feet back to `world_of(p)` up to a ~1e-14 float
        // round-trip error; on a boundary that error could tip `floor` to the
        // next cell and (spuriously) move the observer, recomputing LOS. Mid-cell
        // it can't, so rotating the hull leaves the mask provably untouched.
        let off = Fixed::from_f64(1.0 / 32.0); // 1/32 cell: exact in fixed-point
        world.set_position(e, FixedVec3::new(off, off, Fixed::ZERO));
        // Bind the observer to the hull like the real crew (`entity_set_grid`):
        // its world pose then rotates WITH the hull, and re-basing cancels that
        // rotation, so the grid-local observer — and thus the mask — is invariant
        // to how the hull is turned. An UNBOUND observer would instead slide
        // across the grid as it spun, perturbing the mask and hiding the bug.
        r.entity_set_grid(e.0 as i64, g);
        r.vision_observer_in(e.0 as i64, g);

        // Settle the mask over several static frames so `sync` reaches its
        // quiet-frame early-out (nothing to copy). The observer never moves.
        for _ in 0..4 {
            r.build_instances(&world);
            let _ = r.update_fow(0.016);
        }
        let settled_ver = r.fow.as_ref().expect("mask built").mask_version();

        // Now TURN the hull a quarter-turn — a transform-only change. Re-basing
        // makes the grid-local observer invariant to grid rotation, so the mask
        // is unchanged and `sync` early-outs on this frame (the exact case the
        // fix guards). The crew's grid-local pose is likewise unchanged.
        let gid = r.grids[g as usize];
        let turned = DQuat::from_rotation_z(std::f64::consts::FRAC_PI_2);
        r.scene.grid_mut(gid).expect("real grid").transform.rotation = turned;
        r.build_instances(&world);
        let _ = r.update_fow(0.016);

        // Guard the guard: this frame must actually be QUIET (the mask did not
        // change), or the test wouldn't exercise the early-out it regresses —
        // `sync` would mirror the transform in Phase 2 and pass even unfixed.
        assert_eq!(
            r.fow.as_ref().expect("mask built").mask_version(),
            settled_ver,
            "rotating the hull must not perturb the grid-local mask — otherwise \
             this isn't the quiet-frame path the fix targets"
        );

        // The twin (which actually renders the hull) must now carry the same
        // rotation. Before the fix it stayed at the identity it settled with,
        // because `sync` skipped its transform mirror on this quiet frame.
        let twin_id = r.fow_twin.as_ref().expect("twin armed").twin();
        let twin_rot = r.scene.grid(twin_id).expect("twin grid").transform.rotation;
        let probe = DVec3::new(1.0, 0.0, 0.0);
        assert!(
            (twin_rot * probe - turned * probe).length() < 1e-9,
            "the twin hull must track the real grid's rotation on a quiet frame \
             (twin rotated {probe:?} to {:?}, expected {:?})",
            twin_rot * probe,
            turned * probe,
        );
    }

    /// The fog-of-war path runs headlessly (no window): paint a floor, declare
    /// an observer + a deck band, capture its pose, and update the mask. Catches
    /// panics in `FowTwin::attach` / `FogOfWar::update` / `sync` and confirms a
    /// twin grid is produced for `FrameParams.fow`.
    #[test]
    fn fog_of_war_updates_without_a_renderer() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        r.voxel_fill(0, 0, 0, 8, 8, 0, 0x8055_5f6b); // floor slab (auto world grid) to see across
        r.vision_config(110, 6, 3);
        r.deck_clip(0, 3); // sets the deck band the mask needs

        // An observer entity at sim (4, 4); `build_instances` captures its pose.
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        world.set_position(
            e,
            FixedVec3::new(Fixed::from_int(4), Fixed::from_int(4), Fixed::ZERO),
        );
        r.vision_observer(e.0 as i64);
        r.build_instances(&world);

        assert!(
            r.update_fow(0.016).is_some(),
            "fog of war produced a twin grid to style"
        );
        // A second frame reuses the mask (band unchanged) — no re-attach, no panic.
        r.build_instances(&world);
        assert!(r.update_fow(0.016).is_some(), "mask persists across frames");
        // Clearing the observer detaches the twin cleanly.
        r.vision_observer(-1);
        assert!(r.update_fow(0.016).is_none(), "no observer → no fog");
    }

    /// The box-select drag quad follows the SCREEN, not world N/S/E/W. Locks
    /// the sim-space camera basis + the world-X mirror the geometry rides on.
    #[test]
    fn drag_quad_is_screen_aligned() {
        let approx = |a: (f64, f64), b: (f64, f64)| {
            assert!(
                (a.0 - b.0).abs() < 1e-9 && (a.1 - b.1).abs() < 1e-9,
                "{a:?} ≈ {b:?}"
            );
        };
        // At yaw 0 the box collapses to the old world-axis rectangle.
        let q0 = drag_quad_sim(0.0, (2.0, 3.0), (6.0, 8.0));
        approx(q0[0], (2.0, 3.0));
        approx(q0[1], (2.0, 8.0));
        approx(q0[2], (6.0, 8.0)); // opposite corner is always the release point
        approx(q0[3], (6.0, 3.0));

        // At an arbitrary yaw the quad stays a rectangle: opposite corner is
        // still exactly `b`, and adjacent edges are perpendicular (a proper
        // rotated rect, not a world-axis bbox).
        let (a, b) = ((2.0, 3.0), (6.0, 8.0));
        let q = drag_quad_sim(0.8, a, b);
        approx(q[0], a);
        approx(q[2], b);
        let e1 = (q[1].0 - q[0].0, q[1].1 - q[0].1);
        let e2 = (q[3].0 - q[0].0, q[3].1 - q[0].1);
        let dot = e1.0 * e2.0 + e1.1 * e2.1;
        assert!(
            dot.abs() < 1e-9,
            "adjacent edges are perpendicular (dot {dot})"
        );
        // …and it isn't the world-axis box: the corners moved off N/S/E/W.
        assert!(
            (q[1].0 - a.0).abs() > 1e-6,
            "the screen-right edge is rotated off the world axis"
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
