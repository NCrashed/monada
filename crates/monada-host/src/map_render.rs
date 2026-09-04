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
use glam::{DMat3, DQuat, DVec3, IVec2, IVec3, Vec2};
use monada_fixed::{Fixed, FixedQuat, FixedVec3};
use monada_format::ActionDecl;
use monada_render::OrbitCamera;
use monada_script::{Direction, HostBridge, Roll, VoxelStore};
use monada_sim::{Command, EntityId, World};
use roxlap_core::kfa_draw::{compose_attachment, solve_kfa_limbs};
use roxlap_core::opticast::OpticastSettings;
use roxlap_core::sky::Sky;
use roxlap_core::Camera;
use roxlap_formats::character::{self, Character, ClipData, MeshRef};
use roxlap_formats::kv6::{self, Kv6};
use roxlap_formats::sprite::{Sprite, SPRITE_FLAG_NO_SHADING};
use roxlap_formats::voxel_clip::{DecodedClip, LoopMode};
use roxlap_formats::{OverlayColor, Rgb, VoxColor};
use roxlap_render::gif_import::{voxel_clip_from_gif, GifImportOpts};
use roxlap_render::{
    ActorFacing, ActorState, BillboardActorDef, BillboardActorId, BillboardMode, BillboardUp,
    CharacterId, CollisionMode, DynSpriteTransform, FrameParams, Line3, ParticleEmitterDef,
    ParticleSystem, SceneRenderer, SpawnMode, SpriteInstanceDesc, SpriteSet, VelocityDef,
    ViewCutout, VoxelClipId,
};
use roxlap_scene::fow::{DeckBand, FogOfWar, FowObserver, FowTwin, VisionConfig};
use roxlap_scene::{addr, ColumnSpan, GridId, GridTransform, RayHit, Scene};

/// World voxels per sim unit (x/y). The board's 8 squares span 8·SCALE.
const SCALE: f64 = 16.0;
/// How dark a cast shadow goes when a map asks for shadows without saying
/// how hard, and what a volume map has always used.
const DEFAULT_SHADOW_STRENGTH: f32 = 0.42;
/// World z of the board surface (z grows downward in voxlap).
const GROUND_Z: f64 = 100.0;
/// The no-op actor tint (`0x00RR_GGBB` colour multiply): white leaves the art
/// unchanged. `entity_set_tint` overrides it (e.g. red for a damage flash).
const WHITE_TINT: u32 = 0x00FF_FFFF;
/// …and the no-op HUD tint, which carries an alpha the actor one does
/// not: a mark fades by dropping it, so "unchanged" has to say opaque.
const WHITE_MARK: u32 = 0xFFFF_FFFF;
/// Reserved model 0: the selection-highlight marker the host draws on the
/// locally selected entity. Map-defined models start at 1.
const HIGHLIGHT_MODEL: usize = 0;
/// How far above and below the datum a regional AO bake reaches, in sim
/// z. A column map's terrain lives well inside this; the box has to clear
/// it in both directions, because relighting half a hill shades it
/// against air that is not there.
const BAKE_DEPTH: i64 = 256;

/// Reserved material palette id for placement ghosts, taken from the far
/// end so that map-facing material ids, whenever there are any, can grow
/// from 1 without meeting it. Id 0 is roxlap's locked opaque entry.
const GHOST_MATERIAL: u8 = 255;

/// How thick the selection ring's band is, in world voxels of a sprite a
/// little over a cell across.
///
/// A hairline that says where a unit stands without drawing attention to
/// itself -- the marker is the answer to "which one is selected", and at
/// two and a half voxels it read as a painted circle the unit was standing
/// in. Thin enough to be a line, thick enough to survive a shallow camera.
///
/// A feel parameter -- tune it against the running game.
const RING_THICKNESS: f64 = 1.25;

/// Model 0: a flat amber selection ring circling the selected entity's
/// footprint on the ground under the sprite — the classic RTS read (a
/// multi-selected squad shows one ring per unit). Same placement contract
/// as the old filled tile (`from_fn` and `solid_box` share the pivot
/// path), so chess's square highlight simply became a circle on its square.
fn selection_ring() -> Sprite {
    let d = SCALE as u32 + 4; // a hair wider than the cell
    let c = f64::from(d - 1) * 0.5;
    let (r_out, r_in) = (c, c - RING_THICKNESS);
    let kv6 = Kv6::from_fn(d, d, 2, |x, y, _z| {
        let (dx, dy) = (f64::from(x) - c, f64::from(y) - c);
        let d2 = dx.mul_add(dx, dy * dy);
        (d2 <= r_out * r_out && d2 >= r_in * r_in).then_some(VoxColor(0x80FF_E000))
    });
    let mut s = Sprite::axis_aligned(kv6, [0.0, 0.0, 0.0]);
    s.flags = SPRITE_FLAG_NO_SHADING;
    s
}

/// The placement previews a map asked for, and the instances drawing them.
///
/// Immediate mode, like the gizmos: `ghost_model` fills `targets`,
/// `sync_ghosts` turns them into instances and empties the list, so what a
/// frame asked for is what a frame gets.
#[derive(Default)]
struct Ghosts {
    /// This frame's requests: `(sprite model index, world seat, world
    /// yaw, alpha)`.
    targets: Vec<(usize, DVec3, f64, u8)>,
    /// Live instance ids, torn down and re-issued each frame.
    ids: Vec<roxlap_render::SpriteInstanceId>,
    /// This frame's actor request: `(actor model index, world seat, local
    /// yaw, alpha)`. One, because a cursor previews one thing.
    actor: Option<(usize, DVec3, f64, u8)>,
    /// The live actor preview, and which model it is wearing.
    ///
    /// Kept between frames, unlike [`ids`](Self::ids): an actor has an
    /// animation clock and a directional clip to hold on to. See
    /// [`MapRender::sync_ghost_actor`].
    live: Option<(usize, BillboardActorId)>,
    /// Whether the translucent material has been registered yet. Defined
    /// on first use rather than at startup: a map that never previews
    /// anything should not put a translucent entry in the palette,
    /// because a non-opaque palette is what switches the renderer onto
    /// its two-sweep sprite path.
    material: bool,
}
/// How wide of a body's standing line a click still counts, in world
/// units — three quarters of a cell either side.
const PICK_RADIUS: f64 = 12.0;

/// How tall a body is assumed to be when nothing says otherwise, in world
/// units.
///
/// A model-bound entity whose model has no measured height -- a plain box
/// -- still has to be clickable above its feet. Half a cell is the old
/// behaviour's reach, so nothing that used to be pickable stops being so.
const PICK_FALLBACK_TALL: f64 = SCALE * 0.5;
/// Strongest per-face grid darkening a sun can apply, to the face pointing
/// fully away from the sun (voxlap side-shade units out of the 0x80
/// brightness reference). Kept gentle so the board reads bright — only
/// shadowed faces darken (see `set_light`), not perpendicular ones.
const MAX_SIDE_SHADE: f32 = 18.0;
/// The twelve edges of a `gizmo_box`, as index pairs into the eight
/// corners [`MapRender::cell_box_corners`] returns: the near face, the far
/// face, and the four struts between them.
const BOX_EDGES: [(usize, usize); 12] = [
    (0, 1),
    (1, 2),
    (2, 3),
    (3, 0),
    (4, 5),
    (5, 6),
    (6, 7),
    (7, 4),
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7),
];
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

/// Decode an `entity_set_side` `dir` argument — a `Direction` discriminant
/// crossing the host-API wall as a plain `i64` (`monada-runtime` cannot
/// name the `monada-script` type). Out of range is `None`, the same
/// convention as an out-of-range grid handle: ignored, not a panic.
fn direction_from_i64(dir: i64) -> Option<Direction> {
    match dir {
        0 => Some(Direction::X),
        1 => Some(Direction::NegX),
        2 => Some(Direction::Y),
        3 => Some(Direction::NegY),
        4 => Some(Direction::Z),
        5 => Some(Direction::NegZ),
        _ => None,
    }
}

/// Decode an `entity_set_side` `roll` argument — a `Roll` discriminant.
fn roll_from_i64(roll: i64) -> Option<Roll> {
    match roll {
        0 => Some(Roll::Deg0),
        1 => Some(Roll::Deg90),
        2 => Some(Roll::Deg180),
        3 => Some(Roll::Deg270),
        _ => None,
    }
}

/// `dir`'s unit vector in SIM space.
fn direction_axis(dir: Direction) -> DVec3 {
    match dir {
        Direction::X => DVec3::X,
        Direction::NegX => DVec3::NEG_X,
        Direction::Y => DVec3::Y,
        Direction::NegY => DVec3::NEG_Y,
        Direction::Z => DVec3::Z,
        Direction::NegZ => DVec3::NEG_Z,
    }
}

/// The WORLD-space rotation for `(dir, roll)`: `dir` picks the box's
/// forward face, `roll` turns a quarter-turn around it. Ports `world_of`'s
/// sign flip (`(x,y,z) -> (-x,y,-z)`, see its doc comment) from positions
/// to directions — only the sign pattern matters here, not `world_of`'s
/// scale/offset — then builds the rotation from the resulting world nose
/// and up, the same construction a pure yaw reduces to: locked in by
/// `side_quat_matches_facing_on_the_horizontal` below, which checks all 4
/// horizontal faces against `entity_set_facing`'s own basis.
fn side_quat(dir: Direction, roll: Roll) -> DQuat {
    let mirror = |v: DVec3| DVec3::new(-v.x, v.y, -v.z);

    let nose_sim = direction_axis(dir);
    // The roll reference is sim +Z ("up") rotated around the nose axis —
    // except when the nose IS the Z axis, which has no perpendicular Z to
    // reference, so it rolls around sim +X instead. Either choice is a
    // free pick: nothing yet gives a vertical face a "home" roll to match.
    let up_ref = if matches!(dir, Direction::Z | Direction::NegZ) {
        DVec3::X
    } else {
        DVec3::Z
    };
    let roll_angle = match roll {
        Roll::Deg0 => 0.0,
        Roll::Deg90 => std::f64::consts::FRAC_PI_2,
        Roll::Deg180 => std::f64::consts::PI,
        Roll::Deg270 => 3.0 * std::f64::consts::FRAC_PI_2,
    };
    let up_sim = DQuat::from_axis_angle(nose_sim, roll_angle) * up_ref;

    // Local +X is forward (matches `entity_set_facing`'s `rot * DVec3::X`);
    // local -Z is up (roxlap's world up is -z, see `actor_pose`'s `BillboardUp`),
    // so local +Z carries the up reference's mirrored OPPOSITE.
    let forward = mirror(nose_sim);
    let local_z = -mirror(up_sim);
    let local_y = local_z.cross(forward);
    DQuat::from_mat3(&DMat3::from_cols(forward, local_y, local_z))
}

/// A pending background-music change the map requested (`play_music` /
/// `stop_music`), drained by the host each frame.
pub enum MusicCmd {
    /// Stop the current track.
    Stop,
    /// Start (or keep, if already this track) the looping track at this path.
    Play(String),
}

/// One HUD widget the map described this frame, and where it goes.
///
/// A widget is placed in screen points unless the map pinned it with
/// `ui_pin`, in which case `over` is a point in the world, the widget's
/// `x`/`y` are an offset from where that point lands, and the host drops the
/// widget when it projects behind the camera. The map cannot do this
/// projection itself: it never sees the camera the renderer ends up with.
#[derive(Clone)]
pub struct Placed {
    pub over: Option<[f32; 3]>,
    pub widget: UiWidget,
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
        /// `0xAARRGGBB`, multiplied over the texture. Opaque white for an
        /// ordinary HUD image; a marker paints one arrow per meaning with
        /// it, and fades on the alpha.
        tint: u32,
        /// Radians clockwise about the image's own centre. Drawn as a
        /// mesh, so a turned picture may spill outside the box its size
        /// claims -- which is what the caller lays out for.
        turn: f32,
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
        /// `0xAARRGGBB`. Opaque white for an ordinary label; a word that
        /// means something by its colour carries it here, and fades on
        /// the alpha.
        tint: u32,
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

impl UiWidget {
    /// Where the widget sits, mutably. Every kind carries the same pair, so a
    /// pinned widget can be slid onto its world point without the painter
    /// having to know which kind it is.
    pub fn spot(&mut self) -> (&mut i32, &mut i32) {
        match self {
            Self::Image { x, y, .. }
            | Self::ImageClip { x, y, .. }
            | Self::Text { x, y, .. }
            | Self::TextWrap { x, y, .. }
            | Self::Anim { x, y, .. }
            | Self::Button { x, y, .. } => (x, y),
        }
    }
}

/// A model the script bound via `entity_set_model`: a static sprite (box /
/// kv6), an animated billboard [`ActorModel`], or a rigged [`CharacterModel`]
/// (`.rkc`). Unifies the public model-id space the script sees.
#[derive(Clone, Copy)]
enum ModelRef {
    /// Index into [`SpriteSet::models`].
    Sprite(usize),
    /// Index into [`MapRender::actors`].
    Actor(usize),
    /// Index into [`MapRender::characters`].
    Character(usize),
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
    /// How tall it is drawn, in world units -- the height its first
    /// state was sized to. What a cursor has to be able to hit: a body is
    /// a standing card, not the cell under it.
    tall: f32,
}

/// Which actor models still have to hand their clips to the renderer.
///
/// **Asked per model, not once for all of them.** One latch was right
/// while every actor model was defined at `init`, and stopped being right
/// the moment a LOCAL layer could define one: a placement preview asks for
/// a kind's art when a designer first picks it, which is hundreds of
/// frames after the latch was set. Its clips were then never registered
/// and it drew nothing, silently and for good.
///
/// Indices rather than a filtered iterator, so the caller can hold a
/// `&mut` on one model at a time while it registers.
fn awaiting_clips(actors: &[ActorModel]) -> Vec<usize> {
    actors
        .iter()
        .enumerate()
        .filter(|(_, a)| a.registered.is_none())
        .map(|(i, _)| i)
        .collect()
}

/// Closest approach between a ray and a segment: the squared distance,
/// and how far along the ray it happens.
///
/// What picking a standing body needs. A ray-versus-cylinder would do as
/// well, but this stays right when the camera looks straight down the
/// body's own axis -- where a cylinder's quadratic goes degenerate and the
/// pick blinks out at exactly the angle a top-down game spends its time
/// at.
fn ray_to_segment(origin: DVec3, dir: DVec3, from: DVec3, to: DVec3) -> (f64, f64) {
    let span = to - from;
    let offset = origin - from;
    let dir_dir = dir.dot(dir);
    let dir_span = dir.dot(span);
    let span_span = span.dot(span);
    let dir_offset = dir.dot(offset);
    let span_offset = span.dot(offset);
    let det = dir_dir * span_span - dir_span * dir_span;
    // How far along the segment the closest point sits. Parallel, or a
    // segment of no length, both mean its near end.
    let along_span = if det.abs() < 1e-9 || span_span < 1e-9 {
        0.0
    } else {
        (dir_dir * span_offset - dir_span * dir_offset) / det
    };
    // The segment is a segment, not a line: past either end is that end,
    // which is what makes a head and a pair of feet the ends of a body
    // rather than the middle of an infinite pole.
    let along_span = along_span.clamp(0.0, 1.0);
    let along_ray = (along_span * dir_span - dir_offset) / dir_dir.max(1e-9);
    let gap = offset + dir * along_ray - span * along_span;
    (gap.dot(gap), along_ray)
}

/// Build a fresh actor recipe from registered directional clip ids.
fn actor_def(
    registered: &[(&'static str, Vec<VoxelClipId>)],
    mode: BillboardMode,
) -> BillboardActorDef {
    BillboardActorDef {
        states: registered
            .iter()
            .map(|(name, ids)| ActorState {
                name: (*name).to_string(),
                dirs: ids.clone(),
            })
            .collect(),
        // Yaw-only, whichever variant: the card only yaws to face the camera,
        // staying upright on its floor. A grounded character's feet stay
        // planted on its pivot.
        // Spherical tilts the whole card to face the camera *including pitch*,
        // which at this steep view leans the body up- and-back off its ground
        // anchor — the sprite reads as floating above its collision box even
        // though the feet-pivot is correctly on the ground. Feet-planted wins
        // for a top-down ARPG.
        //
        // Which floor that is comes from `up`, left at its default here because
        // it is only an initial value: `update_actors` re-poses every actor each
        // frame from the floor it actually stands on (`actor_pose`) — world for
        // an unbound one, its grid's own up for a rider. Cylindrical yaws about
        // whichever axis that is (roxlap 0.32 / BB.6), so a card on a tilted
        // deck stands on the deck rather than leaning across it.
        //
        // Eye-facing or view-plane is the map's call (`set_sprite_facing`),
        // because it is a question about the ART: a sprite drawn to be seen
        // flat wants the view plane, a card standing in for a volume wants
        // the eye. Defaults to the eye, which is what every map before
        // `host_api` 29 was drawn against.
        mode,
        ..BillboardActorDef::default()
    }
}

impl MapRender {
    /// `settings` with the map's field of view applied.
    fn projected(&self, settings: &OpticastSettings) -> OpticastSettings {
        let mut s = *settings;
        if let Some(deg) = self.look.fov_deg {
            s.hz = s.hx / (deg.to_radians() / 2.0).tan();
        }
        s
    }

    /// How billboard actors turn this frame.
    fn sprite_mode(&self) -> BillboardMode {
        if self.look.sprite_view_plane {
            BillboardMode::CylindricalViewPlane
        } else {
            BillboardMode::Cylindrical
        }
    }
}

/// The look a map asked for: the sun, how billboards turn, how deep cast
/// shadows go.
///
/// One struct rather than three fields on [`MapRender`] because they are
/// one question — what this scene should look like — and every one of them
/// defaults to "as it was before the verb existed", which is what keeps
/// older maps rendering unchanged.
#[derive(Clone, Copy, Debug, Default)]
struct Look {
    /// The sun as declared by `set_light`: unit travel direction in sim
    /// space, plus intensity. Feeds the dynamic [`LightRig`] (sun +
    /// baked-AO ambient + stylized shadows) so voxel edges read.
    sun: Option<(DVec3, f32)>,
    /// Whether billboard actors align to the view plane rather than aim at
    /// the eye (`set_sprite_facing`).
    ///
    /// Aiming each card at the camera POSITION turns it as it drifts off
    /// the middle of the screen — under a 90° field of view, a card at the
    /// edge stands ~45° away from one in the centre. Right for a card
    /// standing in for a volume, wrong for a sprite drawn to be seen flat,
    /// so it is the map's call. Off by default: every map before
    /// `host_api` 29 was drawn against the eye-facing look.
    sprite_view_plane: bool,
    /// Background colour a ray that hits nothing lands on
    /// (`set_sky_color`), or `None` for the host's daylight blue.
    sky_color: Option<Rgb>,
    /// Horizontal field of view in degrees (`camera_fov`), or `None` for
    /// the host's 90°.
    ///
    /// Narrowing this and pulling the camera back the same factor is how a
    /// perspective raycaster imitates an orthographic one: the further and
    /// tighter the cone, the less an object's size depends on its depth,
    /// until the scene reads flat. It stays an imitation — a true
    /// orthographic projection would mean parallel primary rays, which is
    /// a change to both backends' ray setup, not a projection constant.
    fov_deg: Option<f32>,
    /// Cast-shadow strength asked for with `set_shadows`, or `None` for the
    /// legacy per-face `side_shades` look.
    ///
    /// Volume maps take the rig unconditionally, because isotropic voxel
    /// edges need it to read at all. Column maps ask, and the default
    /// keeps them byte-stable: chess, the RPG and the RTS were tuned
    /// against `side_shades` and a silent switch would restyle all three.
    shadows: Option<f32>,
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
    /// The facing mode last pushed to the renderer — only re-set on change,
    /// so a map that never calls `set_sprite_facing` costs nothing per frame.
    applied_mode: BillboardMode,
    /// The tint last pushed to the renderer — only re-set on change.
    applied_tint: u32,
}

/// A rigged `.rkc` character model (`model_character`): the parsed
/// container, its clips by name, and the placement the map asked for.
/// Registration with the renderer is per ENTITY (roxlap's `add_character`
/// spawns one instance per bone attachment), so nothing here is
/// renderer-side — unlike [`ActorModel`], whose clips upload once.
struct CharacterModel {
    /// Meshes + skeleton + clips, as parsed from the archive asset.
    ch: Character,
    /// Clip name → index into `ch.clips`, for `entity_set_anim`.
    clips: BTreeMap<String, usize>,
    /// World voxels per model voxel, so the character renders
    /// `height_cells` tall whatever scale the artist rigged it at. `1.0`
    /// when the map asked for the native size (`height_cells <= 0`).
    scale: f32,
    /// World distance from the character's root anchor DOWN to its lowest
    /// posed voxel (already scaled), so the feet sit on the entity's cell
    /// instead of the rig origin.
    lift: f32,
    /// Extra world-space vertical offset (`model_drop`), added when seating:
    /// positive lowers, negative lifts (world +z is down) — the knob for a
    /// hovering character that should float above its cell.
    drop: f32,
    /// How tall it is drawn, in world units. See `ActorModel::tall`.
    tall: f32,
    /// Clip names `entity_set_anim` asked for that this character doesn't
    /// have, so the warning is printed once per name, not once per frame.
    warned: BTreeSet<String>,
}

impl CharacterModel {
    /// The clip a per-entity instance should play, or `None` for a
    /// character with no clips at all (roxlap poses it at rest).
    fn clip_of(&self, inst: &CharInst) -> Option<usize> {
        (!self.ch.clips.is_empty()).then_some(inst.clip)
    }

    /// The world transform seating this character at `pos` facing `yaw`:
    /// roxlap takes the ROOT limb's basis, whose vector lengths carry the
    /// scale. The model's local +z stays world +z (down), so the rig's
    /// z-down convention is preserved and yaw is a plain spin about it.
    fn transform(&self, pos: [f32; 3], yaw: f64) -> DynSpriteTransform {
        #[allow(clippy::cast_possible_truncation)]
        let (sy, cy) = (yaw.sin() as f32, yaw.cos() as f32);
        let k = self.scale;
        DynSpriteTransform {
            pos,
            right: [k * cy, k * sy, 0.0],
            up: [-k * sy, k * cy, 0.0],
            forward: [0.0, 0.0, k],
        }
    }
}

/// Per-entity live character state: the renderer handle (created lazily on
/// the first frame it renders), the model it draws, and the clip / facing
/// the script asked for.
struct CharInst {
    /// Index into [`MapRender::characters`].
    model: usize,
    /// The live renderer character, created on the first frame it renders.
    id: Option<CharacterId>,
    /// Desired clip index (script-set via `entity_set_anim`).
    clip: usize,
    /// The clip the live `id` was BUILT with. roxlap bakes the clip into the
    /// skeleton at `add_character` and has no setter, so a change here
    /// re-registers the character (cheap for the small rigs this is for).
    applied_clip: Option<usize>,
    /// Desired facing yaw in sim radians (script-set).
    facing: f64,
}

/// The voxel bounding box of a kv6 mesh, in voxel units RELATIVE TO ITS
/// PIVOT (the point roxlap places at the bone), or `None` if the mesh holds
/// no voxels. Measured from the actual voxels, not the authored `xsiz`
/// extent, so padding around the art doesn't inflate the character.
fn mesh_voxel_box(kv6: &Kv6) -> Option<([f32; 3], [f32; 3])> {
    let (mut lo, mut hi) = ([u32::MAX; 3], [0u32; 3]);
    let mut any = false;
    let mut idx = 0usize;
    for x in 0..kv6.xsiz {
        for y in 0..kv6.ysiz {
            let n = *kv6
                .ylen
                .get(x as usize)
                .and_then(|row| row.get(y as usize))
                .unwrap_or(&0) as usize;
            for _ in 0..n {
                let Some(v) = kv6.voxels.get(idx) else { break };
                idx += 1;
                any = true;
                for (a, c) in [x, y, u32::from(v.z)].into_iter().enumerate() {
                    lo[a] = lo[a].min(c);
                    hi[a] = hi[a].max(c);
                }
            }
        }
    }
    let piv = [kv6.xpiv, kv6.ypiv, kv6.zpiv];
    #[allow(clippy::cast_precision_loss)]
    any.then(|| {
        (
            [0, 1, 2].map(|a| lo[a] as f32 - piv[a]),
            // The max voxel occupies the cell up to `hi + 1`.
            [0, 1, 2].map(|a| (hi[a] + 1) as f32 - piv[a]),
        )
    })
}

/// How far a character reaches above and below its root anchor while `clip`
/// plays: `(top, bottom)` in model voxel units with roxlap's z-down sign, so
/// `bottom - top` is its height and `bottom` is its feet. Sampled across the
/// clip (the envelope, not one frame), so a flapping wing or a crouch can't
/// make the size or the grounding pop mid-animation.
///
/// Only STATIC mesh attachments count: an animated clip attachment (VCL.5's
/// flame on a hand) is decoration and must not decide how tall a character
/// is. `None` when the rig draws no static geometry at all.
fn clip_z_envelope(ch: &Character, clip: Option<usize>) -> Option<(f64, f64)> {
    /// Samples per clip. The rigs this serves hold a handful of keyframes;
    /// 16 steps catch their extremes without a measurable load cost.
    const STEPS: i32 = 16;

    // Mesh boxes are pose-independent — measure each one once, then only
    // transform its 8 corners per sample.
    let boxes: Vec<Option<([f32; 3], [f32; 3])>> = ch.meshes.iter().map(mesh_voxel_box).collect();
    let mut kfa = ch.to_kfa_sprite(clip);
    // Identity root basis at the origin: the envelope is relative to the
    // anchor `set_character_world_transform` will place.
    kfa.p = [0.0; 3];
    kfa.s = [1.0, 0.0, 0.0];
    kfa.h = [0.0, 1.0, 0.0];
    kfa.f = [0.0, 0.0, 1.0];
    let dur = clip
        .and_then(|c| match &ch.clips[c].data {
            ClipData::Skeletal { seq, .. } => seq.iter().map(|s| s.tim).max(),
            ClipData::Unknown { .. } => None,
        })
        .unwrap_or(0);
    let mut env: Option<(f64, f64)> = None;
    for step in 0..=STEPS {
        if step > 0 {
            if dur <= 0 {
                break; // a rest pose / an empty clip has nothing to sweep
            }
            kfa.animsprite(dur / STEPS);
        }
        solve_kfa_limbs(&mut kfa);
        for (bi, bone) in ch.bones.iter().enumerate() {
            let Some(limb) = kfa.limbs.get(bi) else {
                continue;
            };
            for att in &bone.attachments {
                let MeshRef::Static(mi) = att.target else {
                    continue;
                };
                let Some(&Some((lo, hi))) = boxes.get(mi) else {
                    continue;
                };
                let (basis_x, basis_y, basis_z, origin) =
                    compose_attachment(limb.s, limb.h, limb.f, limb.p, &att.local_offset);
                for corner in 0..8u8 {
                    // Only the world-z row of the basis matters for a height.
                    let dx = if corner & 1 == 0 { lo[0] } else { hi[0] };
                    let dy = if corner & 2 == 0 { lo[1] } else { hi[1] };
                    let dz = if corner & 4 == 0 { lo[2] } else { hi[2] };
                    let world_z =
                        f64::from(origin[2] + dx * basis_x[2] + dy * basis_y[2] + dz * basis_z[2]);
                    env = Some(match env {
                        Some((top, bottom)) => (top.min(world_z), bottom.max(world_z)),
                        None => (world_z, world_z),
                    });
                }
            }
        }
    }
    env
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

/// A box sprite model with each of its 6 local faces painted a distinct
/// colour — a debug/demo aid that makes a rotation visible without relying
/// on directional shading alone to tell one side from another. `x`/`neg_x`/
/// `y`/`neg_y`/`z`/`neg_z` are the box's own local axes: the same local
/// X/Y/Z `side_quat` rotates (local +X is the `entity_set_side` "forward"
/// [`direction_axis`] maps onto, before mirroring into world space). A
/// voxel shared by more than one face — an edge or corner — shows
/// whichever of z, then y, then x it matches; a fixed but arbitrary
/// priority, since one voxel can only carry one colour.
#[allow(clippy::too_many_arguments, clippy::many_single_char_names)]
fn sprite_box_sides(
    w: u32,
    h: u32,
    d: u32,
    x: u32,
    neg_x: u32,
    y: u32,
    neg_y: u32,
    z: u32,
    neg_z: u32,
) -> Sprite {
    let kv6 = Kv6::from_fn(w, h, d, |vx, vy, vz| {
        Some(VoxColor(if vz == 0 {
            neg_z
        } else if vz == d - 1 {
            z
        } else if vy == 0 {
            neg_y
        } else if vy == h - 1 {
            y
        } else if vx == 0 {
            neg_x
        } else if vx == w - 1 {
            x
        } else {
            // Interior: `from_fn` only emits surface voxels, so this
            // colour is never actually seen.
            0x8080_8080
        }))
    });
    Sprite::axis_aligned(kv6, [0.0, 0.0, 0.0])
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
/// The particle system's own seed. Fixed rather than the world's: what a
/// burst scatters is decoration, no peer compares it, and a session that
/// replayed the same explosion differently would be no less correct.
const SPARK_SEED: u64 = 0x5061_7274_6963_6c65;

/// How big a particle is drawn, in voxels a side. Two is a speck at this
/// game's zoom and a cube at a chess board's; scale on the emitter is what
/// tunes it per burst.
const SPARK_VOXELS: u32 = 3;

/// …and how much bigger than its authored size it is drawn. A cell is
/// [`SCALE`] world units across, so three voxels at twice size is about
/// two fifths of a cell -- a droplet you can see from the camera this
/// game is played at rather than a pixel of grit. UNTUNED.
const SPARK_SCALE: f32 = 1.6;

/// The material particles wear: translucent, so a burst veils what is
/// behind it rather than replacing it.
///
/// One below the ghosts' slot. Declared on first use for the same reason
/// theirs is -- a non-opaque palette switches the renderer onto its
/// two-sweep sprite path, and a map that never throws anything should not
/// pay for it.
const SPARK_MATERIAL: u8 = 254;

/// How much of a particle shows, `0..=255`. Water rather than smoke: it
/// wants to read as spray, and spray you cannot see past is a wall.
/// UNTUNED.
const SPARK_ALPHA: u8 = 150;

/// The most pieces one burst may ask for.
///
/// A guard rather than a budget: the particle system has its own ceiling
/// and drops what it cannot hold, but a map that asked for a million
/// would spend the frame counting them first.
const SPARK_CAP: u32 = 256;

/// A particle: a small white cube. White because the emitter's tint is
/// what colours it, so one model serves water, sparks and dust.
fn spark_kv6() -> Kv6 {
    Kv6::solid_cube(SPARK_VOXELS, VoxColor(0x80_FF_FF_FF))
}

/// One burst a map asked for, waiting for a frame to happen.
struct Spark {
    /// Where it goes off, in world space.
    at: DVec3,
    /// How many pieces fly out.
    count: u32,
    /// How fast they leave, in world units per second.
    speed: f32,
    /// How long they last, in seconds.
    life: f32,
    /// `0xRRGGBB`, tinted over a white model.
    tint: u32,
}

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

/// Map an inclusive sim-cell region to its voxel box inside a CUBIC
/// `grid_spawn_cubic` grid (`host_api` 15): a cell is a `SCALE³` cube of world
/// voxels, so sim z scales exactly like x/y. Same world-X mirror and z-down
/// flip as [`sim_box_to_world`] — it differs only in z, where the column
/// convention gives a cell a single voxel row.
///
/// The z convention is the column one, generalised: an entity at sim z `N`
/// stands on TOP of cell `N` (`world_of`'s `GROUND_Z - N·SCALE` here), so cell
/// `N` hangs below that plane, occupying voxels `[GROUND_Z - N·SCALE,
/// GROUND_Z - N·SCALE + SCALE - 1]`. With `SCALE = 1` this is exactly
/// `sim_box_to_world`, which is the invariant to keep the two readable
/// together. (NB the *volume world grid* seats entities one cell higher — see
/// [`cell_box_to_volume_grid`] — an older wart no walking map has hit.)
fn cell_box_to_cubic(x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64) -> (IVec3, IVec3) {
    let s = SCALE as i64;
    let gz = GROUND_Z as i64;
    let (xa, xb) = (x0.min(x1), x0.max(x1));
    let (ya, yb) = (y0.min(y1), y0.max(y1));
    let (za, zb) = (z0.min(z1), z0.max(z1));
    let lo = IVec3::new(
        (-(xb + 1) * s) as i32,
        (ya * s) as i32,
        (gz - zb * s) as i32,
    );
    let hi = IVec3::new(
        (-xa * s - 1) as i32,
        ((yb + 1) * s - 1) as i32,
        (gz - za * s + s - 1) as i32,
    );
    (lo, hi)
}

/// Map an inclusive sim-cell region to its grid-voxel box on the ISOTROPIC
/// volume world grid (one grid voxel per cell, `voxel_world_size = SCALE`,
/// origin `(0, 0, GROUND_Z)`): cell `(x, y, z)` is grid voxel
/// `(-x-1, y, -z-1)` — the same world-X mirror and z-down flip as
/// `sim_box_to_world`, but in cells, not world voxels.
#[allow(clippy::cast_possible_truncation)]
fn cell_box_to_volume_grid(x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64) -> (IVec3, IVec3) {
    let (xa, xb) = (x0.min(x1), x0.max(x1));
    let (ya, yb) = (y0.min(y1), y0.max(y1));
    let (za, zb) = (z0.min(z1), z0.max(z1));
    let lo = IVec3::new((-xb - 1) as i32, ya as i32, (-zb - 1) as i32);
    let hi = IVec3::new((-xa - 1) as i32, yb as i32, (-za - 1) as i32);
    (lo, hi)
}

/// [`world_of`] the other way: a world point back to sim cells (x/y).
///
/// **The half cell is the whole point.** `world_of` puts sim `c` at the
/// CENTRE of cell `c`'s voxels, so an inverse that drops the `+0.5` lands
/// half a cell off in both axes — and then the nearest-cell rounding that
/// follows it lands a whole cell off, because half plus half is one.
///
/// It stayed hidden for as long as a pick only had to name a cell to walk
/// to on flat ground. It is glaring the moment anything is drawn under the
/// cursor: the outline and the placement ghost both stood diagonally off
/// the pointer, which is what finally showed it.
fn sim_of(w: DVec3) -> (f64, f64) {
    (-w.x / SCALE - 0.5, w.y / SCALE - 0.5)
}

/// The continuous sim→world map of a volume map (see the `volume` field):
/// `(0, 0, GROUND_Z) + SCALE · R_y(π) · p`. Agrees with
/// `cell_box_to_volume_grid` on cell corners and with `world_of` on x/y —
/// only z differs (scaled by `SCALE` instead of unscaled).
fn volume_world_of(p: DVec3) -> DVec3 {
    DVec3::new(-SCALE * p.x, SCALE * p.y, GROUND_Z - SCALE * p.z)
}

/// [`volume_world_of`] the other way: world back to sim cells. What the
/// cursor needs, since a pick starts as a world-space ray.
fn volume_sim_of(w: DVec3) -> DVec3 {
    DVec3::new(-w.x / SCALE, w.y / SCALE, (GROUND_Z - w.z) / SCALE)
}

/// [`world_of`] with a map's z convention: a volume map scales z by `SCALE`
/// (isotropic cells — see [`MapRender::volume`]), a column map keeps the
/// unscaled-z convention. Free function over the flag rather than a method, so
/// a caller mutating a disjoint `MapRender` field can still use it.
fn entity_world_of_in(volume: bool, p: FixedVec3) -> DVec3 {
    let mut w = world_of(p);
    if volume {
        w.z = GROUND_Z - p.z.to_f64() * SCALE;
    }
    w
}

/// [`MapRender::place`] over explicitly borrowed fields: seat sim position `p`
/// in world space, composed through the grid entity `e` rides (if any). Split
/// out of the method so `build_instances` can compose a seat while pushing into
/// the disjoint `sprites` field — a `&self` method would borrow all of it.
fn place_in(
    entity_grid: &BTreeMap<EntityId, GridId>,
    anchors: &BTreeMap<GridId, GridAnchor>,
    scene: &Scene,
    volume: bool,
    e: EntityId,
    p: FixedVec3,
) -> DVec3 {
    let Some(&g) = entity_grid.get(&e) else {
        // Unbound: the global frame, under the MAP's z convention.
        return entity_world_of_in(volume, p);
    };
    // Bound: the z convention comes from the GRID, not the map — the entity's
    // position is a point in that grid's frame, so it must match the voxels
    // `voxel_fill_in` painted there: scaled z inside a cubic grid (which is
    // what makes its rotation exact), unscaled inside a column-cell one, even
    // on a volume map whose *world* grid scales.
    let local = entity_world_of_in(anchors.get(&g).is_some_and(|a| a.cubic), p);
    match scene.grid(g) {
        Some(grid) => grid.transform.rotation * local + grid.transform.origin,
        None => local,
    }
}

/// What a `grid_spawn` grid turns about. `GridTransform` rotates about the
/// grid's own local origin, which for a hull painted from sim cell `(0,0,0)`
/// up is a CORNER — and `GROUND_Z` above the deck at that, so a hull orienting
/// about it swings through an arc wider than the hull. A pivot fixes a chosen
/// grid-local point instead: rotating about `pivot` is rotating about the local
/// origin and then translating so `pivot` lands back where it started, i.e.
/// `origin = spawn_origin + (I − R)·pivot` — see [`MapRender::apply_grid_pose`].
/// Both fields are world-space, and both are needed because `transform.origin`
/// becomes derived state once a pivot is in play.
#[derive(Clone, Copy)]
struct GridAnchor {
    /// The origin `grid_spawn` placed the grid at — its pose at zero rotation,
    /// and what `transform.origin` equals whenever the rotation is identity.
    spawn_origin: DVec3,
    /// The point held still, in the grid's LOCAL frame (`world_of` of the sim
    /// cell the map named). `ZERO` — the grid's local origin — until the map
    /// calls `grid_pivot`, so an unset pivot is exactly the old behaviour.
    pivot: DVec3,
    /// Render-rate smoothing between the last two tick-exact poses
    /// ([`PoseTrack`]). Inert on a map with no fixed tick rate.
    pose: PoseTrack,
    /// Whether this grid's CELLS ARE CUBES (`grid_spawn_cubic`, `host_api` 15):
    /// `SCALE³` world voxels per cell, so sim z scales like x/y. A plain
    /// `grid_spawn` grid keeps the column convention (`SCALE×SCALE×1`, z
    /// unscaled) and this stays `false`. Everything that reads a grid's frame —
    /// `voxel_fill_in`, `grid_pivot`, a bound entity's seat, the deck cutaway,
    /// the fog band — asks the anchor rather than the map, because the cell
    /// shape belongs to the grid.
    cubic: bool,
}

/// A script grid's pose, twice: the tick-exact one the sim asked for, and the
/// one that was on screen when it arrived (docs/plans/ship-physics.md §4).
///
/// A map writes a grid's pose once per tick; a display draws 60+ frames a
/// second. Drawn as written, a hull that turns visibly steps — and so, rigidly
/// attached to it, does every rider. So a pose write becomes a TARGET, and
/// [`advance_grid_poses`](MapRender::advance_grid_poses) eases the scene
/// transform onto it over exactly one tick.
///
/// Why the whole thing works from one write: a rider's seat ([`place_in`]), a
/// prop's basis, an actor's facing, the fog twin, the deck cutaway and the
/// camera's orbit frame ALL compose against `scene.grid(id).transform` at draw
/// time. Nobody keeps a private copy of a hull's pose, so nothing can shear
/// against the deck it stands on.
///
/// Render-side only. The sim's own frame table (`monada_script::GridStore`)
/// stays tick-exact, so `grid_world` / `grid_local` and every hashed decision
/// are bit-identical to what they were before smoothing existed.
#[derive(Clone, Copy)]
struct PoseTrack {
    /// The pose that was DRAWN when `curr` arrived — deliberately not the
    /// previous target. A frame that runs several catch-up ticks would
    /// otherwise rewind to a pose the player already watched go by.
    prev: (DVec3, DQuat),
    /// The tick-exact pose the map last asked for.
    curr: (DVec3, DQuat),
    /// Seconds since `curr` arrived. `>= tick_dt` means fully arrived, and the
    /// scene transform already equals `curr`.
    age: f64,
}

/// Beyond this much translation between two poses, smoothing would smear a
/// deliberate jump (a dock snap, a jump drive) across a tick instead of
/// showing it. Two cells, in world voxels.
const POSE_SNAP_DIST: f64 = 2.0 * SCALE;

/// The rotation counterpart: `|dot(q0, q1)| = cos(θ/2)`, so this bound is a
/// quarter-turn. Nothing physical turns 90° in one tick; a pose that does was
/// re-authored, not integrated.
const POSE_SNAP_DOT: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// Is the step from `a` to `b` a re-authored pose rather than a tick of
/// motion — i.e. must it snap rather than ease?
fn is_pose_jump(a: (DVec3, DQuat), b: (DVec3, DQuat)) -> bool {
    a.0.distance_squared(b.0) > POSE_SNAP_DIST * POSE_SNAP_DIST
        || a.1.dot(b.1).abs() < POSE_SNAP_DOT
}

/// One entity's draw-position track — what turns a 30 Hz sim into a picture
/// that moves every frame (docs/plans/ship-physics.md §4.3, the S-6 slice).
///
/// [`PoseTrack`] is the grid twin, and this is the same argument one step
/// further in: smoothing the hull fixes a rider STANDING on it, and this
/// fixes one that walks. Both were the same bug from the start — a position
/// written once a tick and drawn as written — and the hull half was simply
/// the half the ship demo could see.
///
/// Held in the entity's OWN frame (sim coordinates, before `place` composes
/// the grid), so a crew member walking a turning deck gets both smoothings,
/// composed in the right order, without either knowing about the other.
///
/// Render-side only. `World::position` stays tick-exact, so every hashed
/// decision is bit-identical to what it was before smoothing existed.
#[derive(Clone, Copy)]
struct PosTrack {
    /// Where the entity was DRAWN when `curr` arrived — deliberately not the
    /// previous sim position, for [`PoseTrack`]'s reason: a frame that runs
    /// several catch-up ticks would otherwise rewind past what was on screen.
    prev: FixedVec3,
    /// The tick-exact position.
    curr: FixedVec3,
    /// Seconds since `curr` arrived. `>= tick_dt` means fully arrived.
    age: f64,
}

/// The field-explicit twin of [`MapRender::drawn_at`], for a caller already
/// pushing into another of its fields — the same split `place_in` exists for.
fn drawn_in(
    tracks: &BTreeMap<EntityId, PosTrack>,
    tick_dt: Option<f64>,
    e: EntityId,
    p: FixedVec3,
) -> FixedVec3 {
    match (tick_dt, tracks.get(&e)) {
        (Some(step), Some(track)) => p + track.drawn(step) - track.curr,
        _ => p,
    }
}

impl PosTrack {
    /// Where this entity is on screen right now, `step` seconds to a tick.
    fn drawn(&self, step: f64) -> FixedVec3 {
        if self.age >= step {
            return self.curr; // arrived — and the common case, so say so first
        }
        let a = Fixed::from_f64((self.age / step).clamp(0.0, 1.0));
        self.prev + (self.curr - self.prev) * a
    }
}

/// How many world voxels one sim cell spans along z inside a grid with this
/// cell shape: `SCALE` for a cubic grid, `1` for the column convention. The one
/// place the two conventions differ, so the z formulas can be written once.
fn cell_z_voxels(cubic: bool) -> i64 {
    if cubic {
        SCALE as i64
    } else {
        1
    }
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
    /// The fine-voxel decoration grid (`body_deco_box`), lazily created
    /// and posed identically to `grid` at `voxel_world_size = 1`.
    deco_grid: Option<GridId>,
    /// Deco boxes blitted so far — a longer list re-blits the tail.
    deco_blitted: usize,
    /// The spinning drill-cone grid (`drill_indicator`), plus its
    /// render-side spin angle.
    drill: Option<DrillMirror>,
}

/// The drill indicator's render state: a cone grid pitched with the
/// commanded bore and spun (render-side accumulator, like wheel spin)
/// while the drill actively cuts.
struct DrillMirror {
    grid: GridId,
    spin: f64,
    /// Cone blit extent (length, base radius), fine voxels — the box to
    /// clear on removal.
    len: i32,
    base_r: i32,
}

/// Blit the drill cone into its (world-voxel) grid: axis along local +x
/// from the base at the origin, tapering to a bright tip. A darker
/// half-stripe alternating along the length makes the render-side spin
/// readable.
#[allow(clippy::cast_possible_truncation)]
fn blit_drill_cone(scene: &mut Scene, id: GridId, len: i32, base_r: i32) {
    let Some(grid) = scene.grid_mut(id) else {
        return;
    };
    for x in 0..len {
        let r = f64::from(base_r) * f64::from(len - x) / f64::from(len) + 1.0;
        let ri = r.ceil() as i32;
        let tip = x > len - 6;
        for y in -ri..=ri {
            for z in -ri..=ri {
                let (cy, cz) = (f64::from(y) + 0.5, f64::from(z) + 0.5);
                if cy.mul_add(cy, cz * cz) > r * r {
                    continue;
                }
                let stripe = (z >= 0) ^ ((x / 5) % 2 == 0);
                let color = if tip {
                    0x80e8_e8f0
                } else if stripe {
                    0x8050_5058
                } else {
                    0x8098_98a4
                };
                let c = IVec3::new(x, y, z);
                grid.set_rect(c, c, Some(VoxColor(color)));
            }
        }
    }
}

/// Drill-cone spin rate while cutting, radians per second.
const DRILL_SPIN_RATE: f64 = 9.0;

/// Decoration (`body_deco_box`): fine-voxel trim sharing the body pose.
/// Same origin AND rotation as the cell grid — fine voxel `f = 16·l`
/// lands on cell point `l` because the deco grid's voxel_world_size is 1.
fn sync_body_deco(
    scene: &mut Scene,
    decos: Option<&Vec<(IVec3, IVec3, u32)>>,
    mirror: &mut BodyMirror,
    origin: DVec3,
    rot: DQuat,
) {
    let deco_len = decos.map_or(0, Vec::len);
    if deco_len > 0 && mirror.deco_grid.is_none() {
        mirror.deco_grid = Some(new_prop_grid(scene, 1.0));
    }
    if let (Some(gid), Some(boxes)) = (mirror.deco_grid, decos) {
        if let Some(grid) = scene.grid_mut(gid) {
            if mirror.deco_blitted != deco_len {
                for &(lo, hi, color) in boxes {
                    grid.set_rect(lo, hi, Some(VoxColor(color)));
                }
                mirror.deco_blitted = deco_len;
            }
            grid.transform.origin = origin;
            grid.transform.rotation = rot;
        }
    }
}

/// The drill indicator (`drill_indicator`): a cone mirroring the
/// registered TOOL — based at the tool box's rear-centre, pitched
/// exactly like the bore, spun render-side while cutting. The spin is
/// the "drilling works" telltale.
#[allow(clippy::cast_possible_truncation)]
fn sync_drill_cone(
    scene: &mut Scene,
    vis: Option<&(f64, bool)>,
    tool: Option<&monada_script::DrillToolDef>,
    mirror: &mut BodyMirror,
    q: DQuat,
    position: DVec3,
    dt: f64,
) {
    let (Some(&(pitch, spinning)), Some(tool)) = (vis, tool) else {
        return;
    };
    if mirror.drill.is_none() {
        // Chunky on purpose: the cone blits at DOUBLE voxel size (vws 2,
        // half the grid dims) — same silhouette, half the detail, reads
        // better next to the cell-sized hull.
        let len = (tool.half_extents.x.to_f64() * SCALE) as i32;
        let base_r = (tool
            .half_extents
            .y
            .to_f64()
            .min(tool.half_extents.z.to_f64())
            * SCALE
            * 0.35) as i32;
        let grid = new_prop_grid(scene, 2.0);
        blit_drill_cone(scene, grid, len, base_r);
        mirror.drill = Some(DrillMirror {
            grid,
            spin: 0.0,
            len,
            base_r,
        });
    }
    let dm = mirror.drill.as_mut().expect("just ensured");
    if spinning {
        dm.spin = (dm.spin + DRILL_SPIN_RATE * dt) % std::f64::consts::TAU;
    }
    let base_body = dvec3(tool.anchor) - DVec3::new(tool.half_extents.x.to_f64(), 0.0, 0.0);
    let base_sim = position + q * base_body;
    let crot =
        mirror_half_turn() * q * DQuat::from_rotation_y(-pitch) * DQuat::from_rotation_x(dm.spin);
    if let Some(grid) = scene.grid_mut(dm.grid) {
        grid.transform.origin = volume_world_of(base_sim);
        grid.transform.rotation = crot;
    }
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
const WHEEL_HALF_WIDTH: i32 = 5;

/// Empty every grid one body mirror owns — hull, wheels, deco trim, drill
/// cone. roxlap skips an empty grid, so this is how a mirror stops being
/// drawn; shared by the "the body is gone" path and the "somebody else draws
/// this body now" one ([`MapRender::retire_mirror`]).
fn clear_mirror(scene: &mut Scene, mirror: &BodyMirror, decos: Option<&Vec<(IVec3, IVec3, u32)>>) {
    if let Some(grid) = scene.grid_mut(mirror.grid) {
        grid.set_rect(IVec3::ZERO, mirror.dims - IVec3::ONE, None);
    }
    for wm in &mirror.wheels {
        let (r, hw) = wm.extent;
        if let Some(grid) = scene.grid_mut(wm.grid) {
            grid.set_rect(IVec3::new(-r, -hw, -r), IVec3::new(r, hw, r), None);
        }
    }
    if let (Some(gid), Some(boxes)) = (mirror.deco_grid, decos) {
        if let Some(grid) = scene.grid_mut(gid) {
            for &(lo, hi, _) in boxes {
                grid.set_rect(lo, hi, None);
            }
        }
    }
    if let Some(dm) = &mirror.drill {
        if let Some(grid) = scene.grid_mut(dm.grid) {
            let r = dm.base_r + 2;
            grid.set_rect(IVec3::new(0, -r, -r), IVec3::new(dm.len, r, r), None);
        }
    }
}

/// Render-only wheel inflation: the physics radius (half a cell) reads
/// tiny against a six-cell hull, so the mirror draws wheels this much
/// larger and seats them so the enlarged rim still touches the contact
/// point (the extra radius lifts the centre, not buries the rim).
const WHEEL_RENDER_SCALE: f64 = 1.5;

/// One debris puff: a dust sprite rising from a carved cell for
/// [`PUFF_TTL`] seconds. World position is the cell centre at spawn; the
/// rise is derived from `age` at draw time.
struct Puff {
    /// The sim cell it rose from. A CELL, not a world point: dust is
    /// drawn as voxels in an effects grid now, and a grid addresses
    /// cells.
    cell: (i64, i64, i64),
    color: u32,
    age: f64,
}

/// Puff lifetime, seconds.
const PUFF_TTL: f64 = 0.45;
/// The most dust that may be in the air at once.
///
/// The feature was written for a drill, which carves a handful of cells
/// a tick. A terraforming game carves *thousands* — one radius-12 crater
/// is 3356 — and a puff is a 7³ sprite instance, so an unbudgeted blast
/// puts three thousand of them in the sprite set for half a second.
/// Decoration gets a ceiling; a drill never reaches it.
const MAX_PUFFS: usize = 96;

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
/// sim-z `z_hi` maps to grid-z `GROUND_Z - z_hi·cell_z`, kept; the layer above
/// it (sim-z `z_hi+1`) is `cell_z` lower and cut. `cell_z` is the grid's own
/// cell height in voxels ([`cell_z_voxels`]): `1` on a column-cell grid, `SCALE`
/// on a cubic one, where a cell's `SCALE` voxel rows hang BELOW its top plane
/// (see [`cell_box_to_cubic`]) so the threshold still lands on the band top.
/// Unit-tested against a REAL grid.
fn deck_clip_world_z(z_hi: i64, cell_z: i64) -> i32 {
    (GROUND_Z as i64 - z_hi * cell_z) as i32
}

/// The pose roxlap needs for a billboard actor whose facing is `local_yaw` in
/// the frame of a grid rotated by `rot` — the card's FLOOR, in other words
/// (BB.6, roxlap 0.32).
///
/// Both halves of an actor's orientation are questions about the floor it stands
/// on, not about the world:
///
/// - which directional sprite to show is the angle between the viewer and the
///   character's nose, measured in the plane it walks on. roxlap's `Yaw`
///   spelling measures it in the WORLD's horizontal plane, so a consumer with a
///   turning floor has to flatten a rotated nose — and rotate-then-flatten does
///   not commute with flatten-then-rotate under a tilted rotation, so the sprite
///   drifts (0.27 rad at the ship's tumble, a third of a sector: an actor
///   standing still visibly turning on the spot). `Dir` takes the world-space
///   nose and does the flattening in the actor's own frame.
/// - which way is up inside the card: pinned to world up, a card on a tilted
///   deck leans, and so does one seen by a camera riding that deck. `Axis`
///   stands it on the deck instead — and upright on screen for free while the
///   camera rides the same body.
///
/// An unrotated grid (and an unbound entity) takes the verbatim `Yaw` + `World`
/// path, which roxlap keeps bit-identical to its pre-BB.6 sector maths.
fn actor_pose(local_yaw: f64, rot: DQuat) -> (ActorFacing, BillboardUp) {
    if rot == DQuat::IDENTITY {
        return (ActorFacing::Yaw(local_yaw), BillboardUp::World);
    }
    let axis = |v: DVec3| [v.x as f32, v.y as f32, v.z as f32];
    // A grid's local frame IS the world frame at rest, so the nose is the yaw's
    // own direction and the deck's up is roxlap's world up (`-z`, z-down); the
    // grid's rotation carries both into the world.
    let nose = rot * DVec3::new(local_yaw.cos(), local_yaw.sin(), 0.0);
    let up = rot * DVec3::new(0.0, 0.0, -1.0);
    (ActorFacing::Dir(axis(nose)), BillboardUp::Axis(axis(up)))
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
    plane_hit(origin, dir, 0)
}

/// Where a ray crosses the horizontal plane at sim height `z`, or `None`
/// if it never does.
///
/// The datum is not the only plane worth asking about: a map that digs
/// below it has ground the datum plane sits *above*, and a cursor that
/// stopped at the datum would answer with a point in mid-air over the
/// hollow it was pointing into.
fn plane_hit(origin: DVec3, dir: DVec3, z: i64) -> Option<DVec3> {
    if dir.z.abs() < 1e-9 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let t = (GROUND_Z - z as f64 - origin.z) / dir.z;
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
    /// One sim tick in seconds, for a map that declared a fixed rate — the
    /// window a grid pose is eased over ([`PoseTrack`]). `None` on a
    /// command-driven map, which turns smoothing off entirely: a turn-based
    /// map's grid is posed by a click, not by a clock, and there is no next
    /// pose to be on the way to. Set by the host from the manifest AFTER the
    /// map's `init`, so poses authored during setup land immediately.
    tick_dt: Option<f64>,
    /// Per-`BodyId` render mirrors of the embedded physics sim (volume maps
    /// only), fed by [`sync_physics`](Self::sync_physics). Map scripts never
    /// hand-mirror a body (plan §1d locked decision).
    body_mirrors: BTreeMap<u64, BodyMirror>,
    /// Body id → the script grid bound to it (`grid_body`, ship-physics D4).
    /// A body in here is NOT auto-mirrored: the map's own painted grid is its
    /// picture, and the mirror would draw a second, material-coloured copy of
    /// the same hull inside it.
    grid_bodies: BTreeMap<u64, GridId>,
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
    /// The effects grid debris is painted into, and the cells painted
    /// last frame so they can be rubbed out.
    fx_grid: Option<i64>,
    fx_painted: Vec<(i64, i64, i64)>,
    /// Map-declared physics-material colours (`phys_material_color`,
    /// D4): the body mirror consults this before the engine's fallback
    /// palette. Render-side only.
    phys_colors: BTreeMap<u16, u32>,
    /// How the map asked the scene to look.
    look: Look,
    /// Render-only decoration boxes per body (`body_deco_box`): FINE
    /// voxels (16 per cell), shape-local. Blitted into a `vws = 1` grid
    /// posed identically to the body's cell grid — skirts, cockpits,
    /// trim that should ride the physics pose without touching the
    /// hashed shape.
    body_decos: BTreeMap<u64, Vec<(IVec3, IVec3, u32)>>,
    /// Per-body drill indicator state (`drill_indicator`): commanded
    /// pitch (radians) + whether the drill is actively cutting. Drives
    /// the spinning cone grid in [`sync_physics`](Self::sync_physics).
    drill_vis: BTreeMap<u64, (f64, bool)>,
    /// Grids spawned by the script via `grid_spawn` (e.g. the ship demo's hull).
    /// The script's i64 handle is the index into this Vec; never reordered or
    /// compacted so handles stay stable. Painted by `voxel_fill_in`. Kept
    /// separate from [`world_grid`](Self::world_grid) so terrain/fog never attach
    /// to a decorative grid.
    grids: Vec<Option<GridId>>,
    /// Per-`grids` rotation anchor, keyed by `GridId`. Once `grid_orient` can
    /// turn a grid about a pivot, `transform.origin` is DERIVED (spawn origin
    /// composed with the pivot swing), so the spawn origin can no longer be
    /// read back out of it — it lives here instead. See [`GridAnchor`].
    grid_anchors: BTreeMap<GridId, GridAnchor>,
    /// The grid a map NAMED on `vision_observer`'s 2-arg overload (`host_api` 7):
    /// fog + `deck_clip` ride it instead of the world grid, for a map whose
    /// observer entity is not itself bound to a grid. A fallback only — an
    /// observer WITH an [`entity_grid`](Self::entity_grid) binding rides that
    /// instead (see [`MapRender::vision_grid`]).
    named_vision_grid: Option<GridId>,
    /// Sprite model registry (index 0 = highlight marker) + per-frame
    /// instances. Holds the box/kv6 models; actor models live in `actors`.
    sprites: SpriteSet,
    /// Public model-id registry: each `entity_set_model` id resolves here to
    /// a static sprite or an animated actor (unifies the two id spaces).
    model_refs: Vec<ModelRef>,
    /// Animated billboard actor models (decoded GIF clips per state).
    actors: Vec<ActorModel>,
    /// Rigged `.rkc` character models (`model_character`), parsed from the
    /// archive at `init` and instanced per entity by `update_characters`.
    characters: Vec<CharacterModel>,
    /// Entity → public model id, set by `entity_set_model`. Render-side, not
    /// hashed. Despawned entities are skipped (positions read live).
    models: BTreeMap<EntityId, usize>,
    /// Entity → how far above its seat it is drawn, in world units.
    /// Render-side and per body: `model_drop` says how a KIND sits on the
    /// ground, this says where ONE of them floats.
    entity_lift: BTreeMap<EntityId, f64>,
    /// Per-entity draw-position smoothing ([`PosTrack`]). Only entities the
    /// map draws or sees from are tracked, and a track is dropped as its
    /// entity despawns — so this is the size of what is on screen, not of
    /// the world.
    entity_tracks: BTreeMap<EntityId, PosTrack>,
    /// Entity → the `grids` grid it rides, set by `entity_set_grid`. An entity
    /// here has its sim `position` read as grid-local and composed through the
    /// grid's transform (origin + rotation) when seated — so crew stay put on a
    /// moving/rotating hull. Unbound entities render in the global frame
    /// (`world_of` directly). Render-side, not hashed.
    entity_grid: BTreeMap<EntityId, GridId>,
    /// Per-entity live actor state (only for entities bound to an actor
    /// model). Created on bind, driven by `entity_set_anim` / `_facing`.
    /// A script-set facing per entity, in SIM radians. Kept for every
    /// binding, not just actors: a plain KV6 model turns its geometry
    /// (decision L4), and which kind an entity is bound to is not settled
    /// until `build_instances` walks it.
    entity_yaw: BTreeMap<EntityId, f64>,
    /// Per-entity full orientation set by `entity_set_side`, in WORLD
    /// space (already through `world_of`'s sim→world map — see
    /// `side_quat`). Geometry-turning only, like `entity_yaw`, and
    /// preferred over it in `seat_sprite` when both are set: a side is a
    /// stronger claim (6 faces × 4 rolls) than a bare yaw, so it wins
    /// rather than composing.
    entity_orient: BTreeMap<EntityId, DQuat>,
    entity_actors: BTreeMap<EntityId, ActorInst>,
    /// Per-entity live character state (entities bound to a `.rkc` model).
    /// The character twin of [`entity_actors`](Self::entity_actors), driven
    /// by the same `entity_set_anim` / `_facing` verbs.
    entity_chars: BTreeMap<EntityId, CharInst>,
    /// Actor render targets computed by `build_instances` (which has the
    /// world) for `render_into` (which has the renderer) to apply:
    /// `(entity, model index, world pos, world yaw)`.
    actor_targets: Vec<(EntityId, usize, [f32; 3], f64, DQuat)>,
    /// Character render targets, same shape and lifetime as
    /// [`actor_targets`](Self::actor_targets) — the world pos is already
    /// seated (feet on the cell, `model_drop` applied).
    char_targets: Vec<(EntityId, usize, [f32; 3], f64)>,
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
    /// One-shot particle bursts — a spell going off, something breaking.
    ///
    /// Render-side and never hashed: a map asks for a burst the way it asks
    /// for a sound, and a headless peer draws none. Its own seed rather
    /// than the world's, because what it scatters is not something two
    /// peers have to agree about.
    sparks: ParticleSystem,
    /// The particle's own model, registered the first time one is asked
    /// for. White, because the tint does the colouring.
    spark_model: Option<roxlap_render::SpriteModelId>,
    /// Bursts asked for since the last frame. A map calls `burst` from its
    /// tick, which is not inside a frame, so they queue until there is a
    /// renderer to register a model with.
    spark_queue: Vec<Spark>,
    ring_model: Option<roxlap_render::SpriteModelId>,
    /// Live ring instance ids, torn down and re-issued each frame.
    ring_ids: Vec<roxlap_render::SpriteInstanceId>,
    /// Static-sprite props that ride a TURNING grid, computed by
    /// `build_instances` for [`sync_props`](Self::sync_props) to place:
    /// `(sprite model index, world seat, full basis rotation, pivot-drop
    /// rotation, pivot drop)`. They cannot use the static instance list — it
    /// has no orientation — so they live on the dynamic layer, posed by the
    /// grid's own basis. The drop rotation is kept separate from the full
    /// basis rotation: see `seat_sprite`'s split of `grid_rot` from `rot`.
    prop_targets: Vec<(usize, DVec3, DQuat, DQuat, f64)>,
    /// Sprite models registered with the renderer's dynamic layer, by their
    /// index in [`SpriteSet::models`]. Registered lazily, once per model.
    prop_models: BTreeMap<usize, roxlap_render::SpriteModelId>,
    /// Live prop instance ids, torn down and re-issued each frame like
    /// [`ring_ids`](Self::ring_ids) — a handful of crates per hull, so the
    /// churn is cheaper than tracking per-entity lifetimes.
    prop_ids: Vec<roxlap_render::SpriteInstanceId>,
    /// Placement previews: what the local layer asked for this frame and
    /// what is drawing it.
    ghosts: Ghosts,
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
    /// How many times each of those has been repainted (`ui_paint`).
    ///
    /// **What tells the painter its copy is stale.** A loaded texture is
    /// uploaded to the GPU once and cached by id; one the map paints changes
    /// under that cache, and without a stamp to compare the minimap would be
    /// the first frame of the game for the rest of it.
    ui_stamps: Vec<u64>,
    /// The HUD widget list the map rebuilt this tick (immediate mode: cleared
    /// by `ui_clear`, appended by `ui_image`/`ui_text`/`ui_button`). The host
    /// paints it via egui each frame and reports button clicks back.
    ui_widgets: Vec<Placed>,
    /// The world point `ui_pin` set for the next widget, if any. One-shot: the
    /// push that follows takes it, so pinning never leaks into later widgets.
    ui_pin: Option<[f32; 3]>,
    /// The viewport size (screen points) the host last rendered at, so the map
    /// can lay the HUD out relative to the window (`ui_width`/`ui_height`).
    ui_viewport: (i64, i64),
    /// Where the cursor is, in those same points. `None` off the window.
    ui_cursor: Option<(i64, i64)>,
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
    /// This frame's view ray under the cursor, kept RAW (`origin, dir`)
    /// beside the points derived from it: the grid picks
    /// (`pick_grid` / `pick_cell` / `pick_face`) march the scene
    /// themselves, and a ray is the only form that question has an answer
    /// in. `None` before the first refresh, or with no window.
    cursor_ray: Option<(DVec3, DVec3)>,
    /// Overlay outlines the local layer queued (`gizmo_*`), drawn over
    /// the composited frame and RETAINED until the map calls
    /// `gizmo_clear` — the same contract the HUD canvas has, and for the
    /// same reason: a map draws them on its own clock (a tick, a frame),
    /// which is slower than the renderer's.
    gizmos: Vec<Line3>,
    /// Current `gizmo_style`: line width in pixels, and whether segments
    /// ignore the depth buffer. State, like `ui_scale`.
    gizmo_style: (f32, bool),
    /// Sim-space aim yaw toward the cursor (`aim_yaw`), holding its last
    /// value while the ray misses — the twin-stick aim convention.
    cursor_aim: f64,
    /// The entity under the cursor (`pick_entity`), refreshed per frame
    /// by the host alongside the ray; `-1` = none.
    cursor_entity: i64,
    /// The grid the camera rides (`camera_grid`): its orbit frame is turned by
    /// that grid.s rotation, so a ship.s deck holds still on screen while the
    /// sky turns. `None` = the world frame (every map before this one).
    camera_grid: Option<GridId>,
    /// What `camera_focus_entity` is following: the entity and the sim point
    /// the map named, KEPT UNCOMPOSED. The focus has to be re-composed through
    /// the grid every frame rather than at the tick that set it — with a
    /// smoothed hull a stored world point is a tick stale, and the whole ship
    /// slides under a lagging camera centre (docs/plans/ship-physics.md §4.4).
    /// `None` = the focus is a world point (`camera_focus`).
    camera_follow: Option<(EntityId, FixedVec3)>,
    /// Third-person wall cutout (`camera_cutout`): keyhole `(radius, feather)`
    /// in sim cells, projected to pixels each frame around the camera focus.
    /// `None` = no cutout. Render-side, never hashed.
    cutout: Option<(f64, f64)>,
    /// The current deck band (`deck_clip`'s sim `z_lo..=z_hi`): the cutaway the
    /// vision grid is clipped to AND the band the fog of war builds its
    /// `DeckBand` from. Kept in SIM cells, not as a resolved `z_clip` threshold,
    /// because the grid it lands on is derived (`vision_grid`) and may use
    /// either cell shape — resolving early froze the column convention's z into
    /// a value a cubic hull would cut at the wrong height. `None` = no deck
    /// declared (show the whole grid). Render-side, never hashed.
    deck_band: Option<(i64, i64)>,
    /// Fog of war (`vision_observer`): the local observer entity, or `None`.
    /// Per-client, never hashed.
    /// Who the fog sees through. Empty means no fog at all.
    ///
    /// A list because a strategy map has a party, not a protagonist: what
    /// is visible is the union of what each of them sees. The FIRST one is
    /// privileged in one way only — it derives
    /// [`vision_grid`](Self::vision_grid), since a mask belongs to one
    /// grid and observers split across hulls have no single answer.
    vision_entities: Vec<EntityId>,
    /// Fog-of-war tuning (`vision_config`): `(cone_deg, range_cells, peripheral_cells)`.
    vision_cfg: (i64, i64, i64),
    /// Whether never-seen ground renders opaque black (`vision_shroud`)
    /// rather than as air. Off by default — see the verb's docs.
    shroud_unseen: bool,
    /// The live fog mask + its dimmed "known twin" grid, lazily built for the
    /// observer once a deck band is known; rebuilt when the band changes (deck
    /// change). `fow_band` is the sim band the current mask was built for.
    fow: Option<FogOfWar>,
    fow_twin: Option<FowTwin>,
    fow_band: Option<(i64, i64)>,
    /// The observer's world feet-position + facing yaw, captured in
    /// `build_instances` (which has the `World`) for `render_into` to build the
    /// `FowObserver` from.
    observer_poses: Vec<(EntityId, DVec3, f64)>,
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
        let sprites = SpriteSet {
            models: vec![selection_ring()],
            instances: Vec::new(),
            carve_model: None,
        };
        MapRender {
            scene,
            world_grid: None,
            volume: false,
            tick_dt: None,
            body_mirrors: BTreeMap::new(),
            grid_bodies: BTreeMap::new(),
            puffs: Vec::new(),
            fx_grid: None,
            fx_painted: Vec::new(),
            phys_colors: BTreeMap::new(),
            look: Look::default(),
            body_decos: BTreeMap::new(),
            drill_vis: BTreeMap::new(),
            grids: Vec::new(),
            grid_anchors: BTreeMap::new(),
            named_vision_grid: None,
            sprites,
            model_refs: Vec::new(),
            actors: Vec::new(),
            characters: Vec::new(),
            models: BTreeMap::new(),
            entity_lift: BTreeMap::new(),
            entity_tracks: BTreeMap::new(),
            entity_grid: BTreeMap::new(),
            entity_yaw: BTreeMap::new(),
            entity_orient: BTreeMap::new(),
            entity_actors: BTreeMap::new(),
            entity_chars: BTreeMap::new(),
            actor_targets: Vec::new(),
            char_targets: Vec::new(),
            highlighted: BTreeSet::new(),
            camera_grid: None,
            camera_follow: None,
            sparks: ParticleSystem::new(SPARK_SEED),
            spark_model: None,
            spark_queue: Vec::new(),
            ring_model: None,
            ring_ids: Vec::new(),
            prop_targets: Vec::new(),
            prop_models: BTreeMap::new(),
            prop_ids: Vec::new(),
            ghosts: Ghosts::default(),
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
            ui_stamps: Vec::new(),
            ui_widgets: Vec::new(),
            ui_pin: None,
            ui_viewport: (0, 0),
            ui_cursor: None,
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
            cursor_ray: None,
            gizmos: Vec::new(),
            gizmo_style: (2.0, false),
            cursor_aim: 0.0,
            cursor_entity: -1,
            cutout: None,
            deck_band: None,
            vision_entities: Vec::new(),
            vision_cfg: (100, 8, 3),
            shroud_unseen: false,
            fow: None,
            fow_twin: None,
            fow_band: None,
            observer_poses: Vec::new(),
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
                // convention's floor surface sits. Full detail only — at
                // voxel_world_size 16 every voxel is already 16× coarser
                // than a world unit, so the mip ladder buys little and
                // costs correctness: mip 1 aggregates a carved tunnel
                // back into solid, and the player digs "invisible walls"
                // sketched by the coarser level.
                let id = self.scene.add_grid(GridTransform::at_scale(
                    DVec3::new(0.0, 0.0, GROUND_Z),
                    SCALE,
                ));
                if let Some(grid) = self.scene.grid_mut(id) {
                    grid.mip_levels_override = Some(1);
                }
                id
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

    /// Declare the map's fixed tick rate, which turns grid-pose smoothing on
    /// ([`PoseTrack`], docs/plans/ship-physics.md §4): from here on a
    /// `grid_move` / `grid_orient` / `grid_pivot` is a target the render eases
    /// onto over one tick instead of a pose that lands whole.
    ///
    /// The host calls this AFTER the map's `init`, deliberately: setup poses a
    /// hull once, from nowhere, and easing that first pose in from the grid's
    /// spawn frame would open the match with a 33 ms wobble. A command-driven
    /// map never calls it at all.
    pub fn set_tick_hz(&mut self, hz: u32) {
        self.tick_dt = Some(1.0 / f64::from(hz.max(1)));
    }

    /// The grid the fog / deck cutaway attach to, DERIVED in this order:
    ///
    /// 1. the grid the observer entity explicitly rides (`entity_set_grid`) —
    ///    the same binding [`place`](Self::place) seats its sprite through, so
    ///    the cone and the crew member can never disagree about which hull they
    ///    are on. Binding is opt-in: naming a fog grid never binds an entity.
    /// 2. else the grid the map named on `vision_observer`'s 2-arg overload
    ///    (`host_api` 7) — fog on a hull nobody rides.
    /// 3. else the world grid.
    ///
    /// `None` only when a map has none of them — i.e. never painted terrain nor
    /// named a vision grid.
    fn vision_grid(&self) -> Option<GridId> {
        self.vision_entities
            .first()
            .and_then(|e| self.entity_grid.get(e).copied())
            .or(self.named_vision_grid)
            .or(self.world_grid)
    }

    /// Set the fog observer entity and (optionally) the grid its mask falls back
    /// to. Rebuilds the mask (drops it, so it re-arms next frame) whenever the
    /// observer or the *derived* [`vision_grid`](Self::vision_grid) changes — a
    /// new observer or a switch to another hull both invalidate the old mask.
    fn set_observer(&mut self, entity: i64, grid: Option<GridId>) {
        let e = (entity >= 0).then_some(EntityId(entity as u64));
        self.set_observers(e.into_iter().collect(), grid);
    }

    /// Replace the whole observer list. `grid` is the mask's fallback grid
    /// (`vision_observer_in`), left alone by the add/clear verbs.
    fn set_observers(&mut self, entities: Vec<EntityId>, grid: Option<GridId>) {
        let before = self.vision_grid();
        let changed = entities != self.vision_entities;
        self.vision_entities = entities;
        self.named_vision_grid = grid;
        if changed || self.vision_grid() != before {
            self.retarget_vision(before);
        }
    }

    /// React to the vision grid moving off `before`: drop the mask (it is
    /// grid-local, so it means nothing on another grid) and un-clip the grid we
    /// are leaving — `apply_deck_clip` only ever writes the CURRENT vision grid,
    /// so a hull left behind would keep the last crew's cutaway forever.
    fn retarget_vision(&mut self, before: Option<GridId>) {
        if before != self.vision_grid() {
            if let Some(grid) = before.and_then(|id| self.scene.grid_mut(id)) {
                grid.z_clip = None;
            }
        }
        self.drop_fow();
    }

    /// Push the current deck cutaway onto the vision grid's `z_clip` (the render
    /// and the unit test share this one apply path, so the test exercises exactly
    /// what `render_into` does). `None` clears the clip (whole grid shown). Only
    /// the vision grid is clipped, because the band is grid-local and applying it
    /// to a grid at another origin would cut that grid at the wrong world height.
    fn apply_deck_clip(&mut self) {
        if let Some(id) = self.vision_grid() {
            let z_clip = self.deck_band.map(|(_, z_hi)| self.deck_clip_z(id, z_hi));
            if let Some(grid) = self.scene.grid_mut(id) {
                grid.z_clip = z_clip;
            }
        }
    }

    /// The `z_clip` threshold for band top `z_hi` in grid `id`'s own voxel
    /// coordinates. Three conventions meet here, all keeping cells `≤ z_hi`:
    /// the volume world grid is grid-LOCAL in cells (cell z sits at voxel
    /// `-z-1`, `GROUND_Z` living in the transform origin), a cubic script grid
    /// scales z by `SCALE`, and a column grid gives a cell one voxel row.
    fn deck_clip_z(&self, id: GridId, z_hi: i64) -> i32 {
        #[allow(clippy::cast_possible_truncation)]
        if self.volume && Some(id) == self.world_grid {
            (-z_hi - 1) as i32
        } else {
            deck_clip_world_z(z_hi, cell_z_voxels(self.grid_is_cubic(id)))
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
        if self.vision_entities.is_empty() {
            return None; // no observers ⇒ no fog (guards a stale mask)
        }
        self.deck_band?; // a deck was declared (deck_clip ran) ⇒ vision is ready
        let main_grid = self.vision_grid()?; // no grid yet ⇒ no fog
                                             // The grid's cell shape sets the z scale everything below works in (the
                                             // deck band, the eye height): a cubic hull's cell is SCALE voxels tall.
        let cubic = self.grid_is_cubic(main_grid);
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
            // tallest deck + its walls with margin — in the grid's OWN z units,
            // so a cubic hull (a cell is SCALE voxels tall) gets the same eight
            // cells of headroom the column tuning's 64 voxels bought.
            let hull_span: i32 = if cubic { 8 * SCALE as i32 } else { 64 };
            // The floor is the LOWEST GROUND, not the datum. A map that
            // digs a hollow has columns below sim z 0, and a band that
            // stopped at the datum left them off every deck — where the
            // classifier reads "unexplored basement" and paints them
            // opaque black. A hollow you can walk into and cannot see.
            //
            // Zero where nothing is dug, so a hull whose lowest deck sits
            // on the datum keeps exactly the band it had.
            #[allow(clippy::cast_possible_truncation)]
            let dug = self.terrain.lowest().min(0) as i32;
            let z_bottom = GROUND_Z as i32 - dug;
            let z_top = GROUND_Z as i32 - hull_span; // generous ceiling
            let mut cfg = VisionConfig::for_decks(vec![DeckBand { z_top, z_bottom }]);
            cfg.cone_half_angle = (cone_deg as f32).to_radians() * 0.5;
            // A full circle has no rim to soften, and the default taper
            // would leave a hairline blind wedge directly behind every
            // observer -- the taper fades intensity to zero AT the cone
            // edge, and for a 360 cone that edge is the observer's own
            // back. `>= 360` is how a map says "sees all round".
            if cone_deg >= 360 {
                cfg.cone_taper = 0.0;
            }
            // Ranges are sim cells; grid columns are SCALE finer.
            cfg.range = range as f32 * SCALE as f32;
            cfg.peripheral_range = peripheral as f32 * SCALE as f32;
            // **The mask is kept at the grain the map thinks in.** A
            // column map's voxels are a texture -- what a body stands on
            // is a cell, `SCALE` columns across -- so a per-column mask
            // asks the shadowcast for SCALE² times the cells it needs
            // and re-runs the lot whenever an observer crosses a
            // sixteenth of a cell. A cubic hull is the other case: there
            // a cell IS a voxel, a crewman leans round a doorframe, and
            // the span stays one.
            cfg.cell_span = if cubic { 1 } else { SCALE as u32 };
            cfg.unseen_occludes = self.shroud_unseen;
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
        // **A full circle is not asked which way it faces, and the eye is
        // asked in quarter-cells.** Both are about how OFTEN roxlap
        // recomputes, not about what it answers: the shadowcast is
        // skipped while the observer's key holds, and that key is (cell,
        // facing, eye). The cell is the mask's own business now
        // (`VisionConfig::cell_span`); these two are the host's.
        //
        // The eye, because a body thrown into the air is still standing
        // where it stood -- the toss is the sim moving it, not the player
        // learning anything -- and following it re-cast twice a frame on
        // the way up and again on the way down. A quarter of a cell is
        // fine enough that a ramp (12 voxels a level) still steps it.
        let eye_step = if cubic { 1.0 } else { SCALE / 4.0 };
        let observers: Vec<FowObserver> = self
            .observer_poses
            .iter()
            .map(|&(_, feet, yaw)| {
                let feet = grid_rot_inv * (feet - grid_origin);
                // A full circle sees the same set whichever way it is
                // turned, so it must not be keyed on a turn: `for_decks`
                // + a 360 cone drops the taper, and every direction is
                // then the same answer. Anything narrower keeps its own.
                let facing_local = if self.vision_cfg.0 >= 360 {
                    DVec3::new(1.0, 0.0, 0.0)
                } else {
                    DVec3::new(-(yaw.cos()), yaw.sin(), 0.0)
                };
                FowObserver {
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
                    //
                    // A CUBIC hull quantises a step to a whole cell (SCALE voxels), and
                    // the eye's opacity band is `±EYE_HALF` (2) around `eye_z`: at `-16`
                    // the band's top (`eye − 2` = feet − 18) sits BELOW the next riser's
                    // top (feet − 16), so the riser blocks and the run fogs from below
                    // again. `-(SCALE + 2·EYE_HALF)` = `-20` lifts the whole band clear
                    // of it — still under the ~22-tall crew's head, i.e. the same ~83%.
                    eye_z: ((feet.z / eye_step).floor() * eye_step) as i32
                        - if cubic { SCALE as i32 + 4 } else { 16 },
                }
            })
            .collect();
        // Take the mask + twin out to keep `self.scene` borrows disjoint.
        let mut fow = self.fow.take()?;
        let mut twin = self.fow_twin.take()?;
        if let Some(grid) = self.scene.grid(main_grid) {
            fow.update_many(grid, &observers, dt as f32);
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
        // The twin renders the hull (the real grid is `render_excluded`), and
        // `sync` mirrors the real grid's render config onto it on EVERY call —
        // including quiet frames, which matters because two render-side verbs
        // move that config without touching either of `sync`'s gates:
        // `grid_orient` (not a voxel edit, and it turns no cell visible/dark)
        // and `deck_clip` (a `z_clip` write). roxlap ≤ 0.31.0 gated the mirror
        // behind its quiet-frame early-out, so the hull stalled mid-turn while
        // the crew — placed from the live real transform in `place` — slid off
        // it, and a deck cutaway raised on a settled frame never opened; this
        // host mirrored the set itself to compensate. 0.31.1 fixed it at the
        // source, so nothing is needed here.
        self.fow = Some(fow);
        out
    }

    /// Keep the GPU's scene LOD measured against how far the eye already
    /// stands.
    ///
    /// **"Far" is not an absolute number of voxels.** A chunk marches at
    /// `floor(log2(t / d))`, where `t` is how far along the ray it sits
    /// and `d` is roxlap's scan distance — so a hull camera five metres
    /// off its subject and a strategy camera a hundred metres off it
    /// cannot share a `d`. roxlap's default (64 world units) is the
    /// hull's; feed the same number to an isometric map and the ENTIRE
    /// scene sits past it, so every chunk marches at the coarsest mip in
    /// the ladder. That reads as a blurred, half-resolution world — and
    /// it is fast for exactly the wrong reason.
    ///
    /// The camera's own orbit distance is the honest scale: mip-0 reaches
    /// twice it (the ladder's first step is at `2d`), which covers the
    /// ground a top-down view is looking at, and coarsens only what is
    /// further from the eye than the subject is again.
    ///
    /// `ROXLAP_GPU_MIP_SCAN_DIST` is a user's escape hatch on a shipped
    /// binary, and a host that wrote this every frame would quietly take
    /// it away — so an env override leaves the knob alone.
    fn drive_mip_scan(&self, renderer: &mut SceneRenderer) {
        if let Some(dist) = self.mip_scan_dist() {
            renderer.set_gpu_mip_scan_dist(dist);
        }
    }

    /// …and the number itself, so the rule can be checked without a GPU.
    /// `None` where the env has taken the knob.
    #[must_use]
    fn mip_scan_dist(&self) -> Option<f32> {
        if std::env::var_os("ROXLAP_GPU_MIP_SCAN_DIST").is_some() {
            return None;
        }
        #[allow(clippy::cast_possible_truncation)]
        Some(self.camera.dist.max(1.0) as f32)
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
        self.cursor_ray = Some((origin, dir));
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
        entity_world_of_in(self.volume, p)
    }

    /// Seat sim position `p` in world space, composed through the grid the
    /// entity rides (if any). [`Self::entity_world_of`] maps sim → the grid's
    /// LOCAL world frame (the same frame `voxel_fill_in`'s voxels live in); a
    /// bound grid's transform (rotation then origin) then carries it into the
    /// world — so a crew member tracks its hull as it moves or turns. Unbound
    /// entities have an identity transform, i.e. `entity_world_of(p)`
    /// unchanged (chess/RPG/RTS).
    fn place(&self, e: EntityId, p: FixedVec3) -> DVec3 {
        place_in(
            &self.entity_grid,
            &self.grid_anchors,
            &self.scene,
            self.volume,
            e,
            p,
        )
    }

    /// Allocate a script-facing grid and its anchor; the shared body of
    /// `grid_spawn` (column cells) and `grid_spawn_cubic` (cube cells).
    ///
    /// `(wx, wy, wz)` is a SIM cell offset, so the grid composes with the
    /// mirrored/scaled voxels `voxel_fill_in` paints inside it: a spawn at sim
    /// cell `wx` shifts world X by `-wx·SCALE` (world X is mirrored — see
    /// `world_of`), Y by `+wy·SCALE`, and z by `-wz` times the grid's own cell
    /// height (`SCALE` for a cubic grid, `1` for a column one — the same factor
    /// its voxels use, so the offset moves it by whole cells either way). At
    /// `(0, 0, 0)` this is the identity, the common world-origin case.
    fn spawn_grid(&mut self, wx: i64, wy: i64, wz: i64, cubic: bool) -> i64 {
        let pos = DVec3::new(
            -(wx as f64) * SCALE,
            wy as f64 * SCALE,
            -(wz as f64) * cell_z_voxels(cubic) as f64,
        );
        let id = self.scene.add_grid(GridTransform::at(pos));
        let idx = self.grids.len() as i64;
        self.grids.push(Some(id));
        // A fresh grid turns about its own local origin until the map names a
        // pivot — the pose `GridTransform::at` just set.
        self.grid_anchors.insert(
            id,
            GridAnchor {
                spawn_origin: pos,
                pivot: DVec3::ZERO,
                cubic,
                // Born already arrived at its spawn pose: `age` is only ever
                // compared against `tick_dt`, and `f64::MAX` is past every one.
                pose: PoseTrack {
                    prev: (pos, DQuat::IDENTITY),
                    curr: (pos, DQuat::IDENTITY),
                    age: f64::MAX,
                },
            },
        );
        idx
    }

    /// Whether the script grid `id` was spawned with cube cells
    /// (`grid_spawn_cubic`). Unknown grids — the world grid, a physics mirror —
    /// answer `false`: they are not script grids and never carry an anchor.
    fn grid_is_cubic(&self, id: GridId) -> bool {
        self.grid_anchors.get(&id).is_some_and(|a| a.cubic)
    }

    /// Resolve a script grid handle to its scene grid. `None` for a negative,
    /// out-of-range or DESPAWNED handle — the slot of a retired grid is kept as
    /// a tombstone rather than compacted, so every later handle keeps its value
    /// and a stale one can never address a grid spawned after it.
    fn grid_id(&self, handle: i64) -> Option<GridId> {
        usize::try_from(handle)
            .ok()
            .and_then(|i| self.grids.get(i))
            .copied()
            .flatten()
    }

    /// Write a `grid_spawn` grid's pose: `rotation`, plus the origin that keeps
    /// its [`GridAnchor::pivot`] where it was. `origin + R·pivot` is where the
    /// pivot lands, and we want it at `spawn_origin + pivot` (where zero
    /// rotation puts it), so `origin = spawn_origin + (I − R)·pivot`. With the
    /// default `ZERO` pivot this collapses to `origin = spawn_origin` — the
    /// grid turns about its local origin exactly as it did before pivots
    /// existed. The single writer for both `grid_orient` and `grid_pivot`, so
    /// the two can be called in either order and land the same pose.
    fn apply_grid_pose(&mut self, id: GridId, rotation: DQuat) {
        let Some(anchor) = self.grid_anchors.get(&id).copied() else {
            return;
        };
        let origin = anchor.spawn_origin + anchor.pivot - rotation * anchor.pivot;
        self.set_grid_pose(id, origin, rotation);
    }

    /// The single writer of a script grid's pose, and the seam smoothing hangs
    /// off (docs/plans/ship-physics.md §4).
    ///
    /// On a map with a fixed tick rate the pose is a TARGET: the track keeps
    /// what is on screen now, and [`advance_grid_poses`](Self::advance_grid_poses)
    /// eases onto the target over one tick. Without a declared rate — every
    /// turn-based map, and every test that poses a grid and reads it back — the
    /// pose lands immediately, exactly as it did before smoothing existed.
    ///
    /// A pose that JUMPS ([`is_pose_jump`]) always lands immediately: easing a
    /// dock snap or a re-authored frame across a tick would smear it, not
    /// smooth it.
    fn set_grid_pose(&mut self, id: GridId, origin: DVec3, rotation: DQuat) {
        if !self.grid_anchors.contains_key(&id) {
            return;
        }
        let target = (origin, rotation);
        let drawn = self
            .scene
            .grid(id)
            .map(|g| (g.transform.origin, g.transform.rotation));
        let step = self.tick_dt;
        let snap = match (step, drawn) {
            (Some(_), Some(drawn)) => is_pose_jump(drawn, target),
            // No tick rate (or no scene grid to read a drawn pose from):
            // nothing to interpolate between.
            _ => true,
        };
        if let Some(anchor) = self.grid_anchors.get_mut(&id) {
            anchor.pose.prev = drawn.unwrap_or(target);
            anchor.pose.curr = target;
            // A snapped pose is "fully arrived" by definition; `f64::MAX`
            // survives the `+= dt` below without ever coming back under a step.
            anchor.pose.age = if snap { f64::MAX } else { 0.0 };
        }
        if snap {
            if let Some(g) = self.scene.grid_mut(id) {
                g.transform.origin = origin;
                g.transform.rotation = rotation;
            }
        }
    }

    /// Ease every script grid's drawn pose toward the one the sim last asked
    /// for, `dt` seconds' worth (docs/plans/ship-physics.md §4). Called once
    /// per frame, BEFORE anything reads a grid transform — riders, props,
    /// actors, the fog twin, the deck cutaway and the camera all compose
    /// against it, and a reader that ran first would be one frame behind the
    /// hull it stands on.
    ///
    /// A no-op on a map with no declared tick rate, so a turn-based map's
    /// grids sit exactly where `grid_orient` put them.
    pub fn advance_grid_poses(&mut self, dt: f64) {
        let Some(step) = self.tick_dt else {
            return;
        };
        // Disjoint fields, one loop: the anchors carry the track, the scene
        // carries what is drawn.
        let MapRender {
            grid_anchors,
            scene,
            ..
        } = self;
        for (&id, anchor) in grid_anchors.iter_mut() {
            if anchor.pose.age >= step {
                continue; // arrived — the transform already equals `curr`
            }
            anchor.pose.age += dt;
            let a = (anchor.pose.age / step).clamp(0.0, 1.0);
            let (prev_origin, prev_rot) = anchor.pose.prev;
            let (curr_origin, curr_rot) = anchor.pose.curr;
            if let Some(g) = scene.grid_mut(id) {
                g.transform.origin = prev_origin.lerp(curr_origin, a);
                // `slerp` takes the short way round the double cover (glam
                // flips the sign on a negative dot), so a hull never spins the
                // long way between two poses a tick apart.
                g.transform.rotation = prev_rot.slerp(curr_rot, a);
            }
        }
    }

    /// Age every entity's draw-position track by `dt` seconds — the twin of
    /// [`advance_grid_poses`](Self::advance_grid_poses), called beside it and
    /// for the same reason.
    ///
    /// Ageing is split from where a new target is DETECTED (`build_instances`,
    /// below) because entity positions have no single writer to hang a track
    /// off the way `set_grid_pose` does: they live in the `World`, so the
    /// track has to compare against it — and compare against an age this
    /// frame has already moved.
    ///
    /// A no-op on a map with no declared tick rate. With no tick to
    /// interpolate across, positions land whole exactly as they always did,
    /// which is what keeps every turn-based map and every host test that
    /// places an entity and reads it back on the next line unmoved.
    pub fn advance_entity_tracks(&mut self, dt: f64) {
        if self.tick_dt.is_none() {
            return;
        }
        for track in self.entity_tracks.values_mut() {
            track.age += dt;
        }
    }

    /// Point every drawn entity's track at what the world says now, and
    /// forget the ones that are gone.
    ///
    /// At the head of `build_instances`, which is the only place holding a
    /// `World` to compare against.
    fn track_positions(&mut self, world: &World) {
        let Some(step) = self.tick_dt else {
            return;
        };
        // What is drawn, seen from, or ringed — an entity nobody looks at
        // needs no track, and tracking the whole world would grow this
        // without bound. A selection marker is here because it is drawn at
        // its entity's position and must agree with the body it marks, even
        // where that body has no model of its own.
        let shown: BTreeSet<EntityId> = self
            .models
            .keys()
            .copied()
            .chain(self.vision_entities.iter().copied())
            .chain(self.highlighted.iter().copied())
            .collect();
        self.entity_tracks.retain(|e, _| shown.contains(e));
        for e in shown {
            let Some(p) = world.position(e) else {
                self.entity_tracks.remove(&e); // despawned
                continue;
            };
            let Some(track) = self.entity_tracks.get(&e).copied() else {
                // Fresh: arrived by definition. A spawn must not ease in from
                // wherever the last holder of this id happened to stand.
                self.entity_tracks.insert(
                    e,
                    PosTrack {
                        prev: p,
                        curr: p,
                        age: f64::MAX,
                    },
                );
                continue;
            };
            if track.curr == p {
                continue; // no tick landed on this entity — keep easing
            }
            let now = track.drawn(step);
            // The jump rule, measured where it means something: `place`
            // composes both points through the entity's own frame, so the
            // threshold is one distance in world voxels whatever z convention
            // the entity's grid keeps. A teleport, a spawn onto a used id, or
            // an attach that rebases the local position all land here and
            // snap, rather than drawing a streak across the tick.
            let snap = self.place(e, now).distance_squared(self.place(e, p))
                > POSE_SNAP_DIST * POSE_SNAP_DIST;
            self.entity_tracks.insert(
                e,
                PosTrack {
                    prev: now,
                    curr: p,
                    age: if snap { f64::MAX } else { 0.0 },
                },
            );
        }
    }

    /// `p` in `e`'s own frame, moved by however far `e` is DRAWN from where
    /// this tick left it.
    ///
    /// Where `p` is the entity's own position — which is every caller in
    /// `build_instances` — this IS the drawn position. Written as a shift so
    /// that a point named in the entity's frame rather than at it (the turret
    /// mount `camera_focus_entity` leaves the door open for) rides along
    /// instead of collapsing onto the body.
    ///
    /// `p` unchanged where nothing is tracked: an untracked entity, or a map
    /// with no tick rate to interpolate over.
    fn drawn_at(&self, e: EntityId, p: FixedVec3) -> FixedVec3 {
        drawn_in(&self.entity_tracks, self.tick_dt, e, p)
    }

    /// The sim-space facing yaw the script last set on an entity, whichever
    /// animated model kind it is bound to (`0` for an entity with neither).
    /// The fog observer reads it through here so a crew member rigged as a
    /// `.rkc` character aims the same cone a billboard actor would.
    fn entity_facing(&self, e: EntityId) -> f64 {
        self.entity_actors
            .get(&e)
            .map(|a| a.facing)
            .or_else(|| self.entity_chars.get(&e).map(|c| c.facing))
            .unwrap_or(0.0)
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

    /// Seat one static-sprite entity, already placed at world `w`.
    ///
    /// roxlap anchors the kv6's stored pivot at the sprite position, so seat by
    /// the pivot, not an assumed centre: the model's bottom face sits
    /// `(zsiz - zpiv)` below it (z grows down). For a centre-pivot box that is
    /// the old `w.z - zsiz/2`; an off-centre piece no longer sinks.
    ///
    /// A prop riding a grid goes on the DYNAMIC layer, always. Two reasons, and
    /// the second is the one that bites:
    ///
    /// 1. It has to TURN with its grid. roxlap's static instance
    ///    (`SpriteInstanceDesc`) carries a position and nothing else, so a crate
    ///    left there keeps its world-axis alignment while the hull rolls under
    ///    it.
    /// 2. In a dynamic-layer map the static set is uploaded EXACTLY ONCE
    ///    (re-uploading resets the actors), so a static instance is FROZEN at
    ///    wherever it stood on the first rendered frame. Routing only *turning*
    ///    grids to the dynamic layer left every prop a ghost: the hull has not
    ///    turned yet on frame 0 (`grid_orient` runs in `tick`), so each crate
    ///    was baked into the static set and then ALSO drawn posed — one copy
    ///    riding the ship, one hanging in space where it started.
    ///
    /// Hence the test is "does it ride a grid", not "is that grid turning": a
    /// grid at rest poses with the identity basis, which is exactly what the
    /// static path would have drawn.
    fn seat_sprite(&mut self, e: EntityId, si: usize, w: DVec3) {
        let drop = self.sprites.models.get(si).map_or(SCALE * 0.5, |m| {
            f64::from(m.kv6.zsiz) - f64::from(m.kv6.zpiv)
        });
        let grid_rot = self
            .entity_grid
            .get(&e)
            .and_then(|&g| self.scene.grid(g))
            .map(|g| g.transform.rotation);
        // A script-set facing turns the model's geometry (L4). It composes
        // with the grid's rotation rather than replacing it: a tank parked
        // on a listing hull points where it was told to, in the hull's
        // frame, exactly as a crew member does.
        //
        // ANY recorded facing routes here, including zero. Skipping the
        // identity looks like a free optimisation and is a freeze: a
        // recorded facing already made this a dynamic-layer map, so the
        // static set is uploaded once, and an instance left on the static
        // path stops moving. The desert's vehicle drives due +x with
        // heading 0 until it reaches the map edge, so it sat still on
        // screen while the simulation drove it across the dunes.
        // `entity_set_side` wins over `entity_set_facing` when a script sets
        // both: a side is a stronger claim (6 faces * 4 rolls) than a bare
        // yaw, and composing the two would need a precedence rule with no
        // obvious right answer (a scripted side is presumably deliberate,
        // not a stale yaw the map forgot to clear).
        let facing = self.entity_orient.get(&e).copied().or_else(|| {
            self.entity_yaw.get(&e).copied().map(|y| {
                // World yaw, not sim yaw: `world_of` mirrors X, so a sim
                // heading reads as `PI - yaw` on screen (`facing_to_world_yaw`).
                DQuat::from_rotation_z(facing_to_world_yaw(y))
            })
        });
        let rot = match (grid_rot, facing) {
            (Some(g), Some(f)) => Some(g * f),
            (Some(g), None) => Some(g),
            (None, f) => f,
        };
        // **On a dynamic-layer map, EVERY model-bound entity rides the
        // dynamic layer -- rotated or not.** The static set is uploaded
        // exactly once there, so the static path is only correct for a map
        // that re-uploads every frame (chess). Anything seated on it after
        // that upload is not merely frozen, as the note above describes:
        // an entity SPAWNED later was never in the upload at all and never
        // appears. A spell that raises a model mid-fight is invisible, and
        // so is a prop the editor puts down.
        //
        // The cost is that a still prop is torn down and re-issued each
        // frame with the moving ones. That is the trade `sync_props`
        // already makes, and a map's props are dozens rather than
        // thousands.
        if self.dynamic_layer() {
            let rot = rot.unwrap_or(DQuat::IDENTITY);
            // The pivot-drop offset (`sync_props`) turns with `drop_rot`, not
            // the full `rot`: it exists so a hull rolled onto its side pushes
            // a resting crate sideways instead of down (the grid tipping the
            // FLOOR the model's local -Z sits on). `facing`/`entity_orient` is
            // a different thing — the model's OWN pose, e.g. a crate spun in
            // a crew member's hand — and folding it into the drop rotation
            // makes the model orbit the entity's position instead of turning
            // in place, since the offset vector swings every time the model's
            // own orientation changes rather than staying fixed. So only the
            // grid contributes to the drop; `rot` (grid * facing) still drives
            // the drawn basis.
            let drop_rot = grid_rot.unwrap_or(DQuat::IDENTITY);
            self.prop_targets.push((si, w, rot, drop_rot, drop));
        } else {
            self.sprites.instances.push(SpriteInstanceDesc {
                model: si,
                pos: [w.x as f32, w.y as f32, (w.z - drop) as f32],
            });
        }
    }

    /// Rebuild the sprite instances from the live world: one sprite per
    /// entity that has a model binding, seated on the board, plus the
    /// highlight marker on the selected entity.
    pub fn build_instances(&mut self, world: &World) {
        self.sprites.instances.clear();
        self.actor_targets.clear();
        self.prop_targets.clear();
        self.char_targets.clear();
        // Capture the fog-of-war observer's world pose (feet + facing yaw) while
        // we have the World; `render_into` builds the `FowObserver` from it.
        // Each observer that still exists, carrying its own id: a despawned
        // one is dropped rather than freezing the fog at its last pose, so
        // the list is shorter than `vision_entities` and cannot be indexed
        // in parallel with it.
        // Smoothed first, so everything below composes from one answer about
        // where each entity is this frame.
        self.track_positions(world);
        self.observer_poses = self
            .vision_entities
            .clone()
            .into_iter()
            .filter_map(|e| {
                let p = self.drawn_at(e, world.position(e)?);
                let yaw = self.entity_facing(e);
                Some((e, self.place(e, p), yaw))
            })
            .collect();
        // Snapshot the bindings so the loop can mutate the disjoint sprite /
        // actor-target fields freely (the map is small — per-entity).
        let bindings: Vec<(EntityId, usize)> = self.models.iter().map(|(&e, &m)| (e, m)).collect();
        for (e, model_id) in bindings {
            let Some(p) = world.position(e) else {
                continue; // despawned (e.g. captured / killed)
            };
            let p = self.drawn_at(e, p);
            // The observer entity is usually model-bound too, so reuse the
            // seat we already composed for `observer_pose` above instead of
            // running `place` (a quaternion rotate) a second time this frame.
            let w = match self.observer_poses.iter().find(|&&(o, ..)| o == e) {
                Some(&(_, pos, _)) => pos,
                None => self.place(e, p),
            };
            // …and where this ONE body is drawn relative to where it
            // stands. Here rather than in `place`, which is also what the
            // camera follows and what a fog observer sees from: a hovering
            // body should float, not carry the view up with it.
            let w = w - DVec3::Z * self.lift(e);
            match self.model_refs.get(model_id) {
                Some(&ModelRef::Sprite(si)) => self.seat_sprite(e, si, w),
                Some(&ModelRef::Actor(ai)) => {
                    // A directional billboard actor: seat its bottom-centre
                    // pivot on the surface (plus the model's `model_drop`
                    // offset, world +z = down); facing comes from the script.
                    //
                    // The facing is kept in the entity's OWN frame here, paired
                    // with the grid's rotation; `update_actors` turns the two
                    // into the floor roxlap measures against (see `actor_pose`).
                    // Which sprite to show is a question about the angle between
                    // the viewer and the character's nose, and that angle only
                    // means something in the frame the nose is defined in.
                    let local_yaw = self
                        .entity_actors
                        .get(&e)
                        .map_or(0.0, |a| facing_to_world_yaw(a.facing));
                    let rot = self
                        .entity_grid
                        .get(&e)
                        .and_then(|&g| self.scene.grid(g))
                        .map_or(DQuat::IDENTITY, |g| g.transform.rotation);
                    let drop = self.actors.get(ai).map_or(0.0, |a| a.drop);
                    self.actor_targets.push((
                        e,
                        ai,
                        [w.x as f32, w.y as f32, w.z as f32 + drop],
                        local_yaw,
                        rot,
                    ));
                }
                Some(&ModelRef::Character(ci)) => {
                    // A rigged `.rkc` character: the transform anchors its
                    // ROOT, so pull the anchor up by the measured `lift` to
                    // land its lowest posed voxel on the cell (`model_drop`
                    // then nudges, world +z = down). Real geometry turns, so
                    // the yaw is the model's own spin, not a sprite pick.
                    let yaw = self.grid_facing_yaw(
                        e,
                        self.entity_chars
                            .get(&e)
                            .map_or(0.0, |c| facing_to_world_yaw(c.facing)),
                    );
                    let (lift, drop) = self
                        .characters
                        .get(ci)
                        .map_or((0.0, 0.0), |c| (c.lift, c.drop));
                    self.char_targets.push((
                        e,
                        ci,
                        [w.x as f32, w.y as f32, w.z as f32 - lift + drop],
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
        // Grid bindings are per-entity render state, so retire them with their
        // entity: a despawned id left bound would leak (a long session churning
        // crew grows the map forever) and could seat a LATER entity on a hull it
        // never asked to ride if the world reuses the id. Losing the OBSERVER's
        // binding moves the fog off that hull, so route it through the same
        // re-target the explicit verbs use (drop the grid-local mask, un-clip the
        // hull left behind) instead of letting the derivation shift silently.
        let fog_grid = self.vision_grid();
        self.entity_grid.retain(|e, _| world.position(*e).is_some());
        if self.vision_grid() != fog_grid {
            self.retarget_vision(fog_grid);
        }
        // Compose each marker's seat through `place_in` (the field-explicit twin
        // of `place`) rather than snapshotting the set into a fresh `Vec` every
        // frame: the `&self` method would clash with pushing into `self.sprites`,
        // while passing the disjoint `entity_grid` / `scene` fields the borrow
        // checker can split from `sprites` costs no allocation — and keeps the
        // volume map's z convention the inlined `world_of` used to drop.
        for &h in &self.highlighted {
            if let Some(p) = world.position(h) {
                // Through the SMOOTHED position, like the body it rings. A
                // marker left on the tick-exact point leads its own body by
                // up to a tick, and a ring sliding around underneath the
                // thing it marks is the judder back in a smaller place.
                let p = drawn_in(&self.entity_tracks, self.tick_dt, h, p);
                let w = place_in(
                    &self.entity_grid,
                    &self.grid_anchors,
                    &self.scene,
                    self.volume,
                    h,
                    p,
                );
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
    // One linear per-body sweep (blit → pose → wheels → trim → cone);
    // the deco/cone stages are already extracted, and slicing the blit
    // or wheel stages would thread half the mirror state through
    // parameters for no clarity gain.
    #[allow(clippy::too_many_lines)]
    pub fn sync_physics(&mut self, sim: &monada_script::PhysicsSim, dt: f64) {
        self.retire_dead_mirrors(sim);
        for body in sim.world.bodies() {
            // A body bound to a script grid draws as that grid (`grid_body`,
            // ship-physics D4) — the hull the map painted, with its plating,
            // its doors and its stair brass, rather than a palette-coloured
            // block of the same silhouette. Its pose arrives through
            // `grid_pose` on the tick, not from here.
            if self.grid_bodies.contains_key(&body.id().0) {
                continue;
            }
            let Some(shape) = body.shape() else { continue };
            let (origin, rot) =
                body_grid_pose(body.position(), body.orientation(), body.com_in_shape());
            let mirror = self.body_mirrors.entry(body.id().0).or_insert_with(|| {
                let grid = new_prop_grid(&mut self.scene, SCALE);
                BodyMirror {
                    grid,
                    blitted: usize::MAX,
                    dims: IVec3::ONE,
                    wheels: Vec::new(),
                    deco_grid: None,
                    deco_blitted: 0,
                    drill: None,
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
                    grid.set_rect(IVec3::ZERO, IVec3::new(dx - 1, dy - 1, dz - 1), None);
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
                    let r_world = radius * SCALE * WHEEL_RENDER_SCALE;
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
                // `wheel_travel`), not the rest length — then lift the
                // centre by the render-only radius surplus so the
                // inflated rim touches where the physics contact is.
                let anchor_sim = dvec3(body.position()) + q * dvec3(wheel.def().anchor);
                let down_sim = q * DVec3::NEG_Z;
                let rest = wheel.def().rest_length.to_f64();
                let travel = wheel_travel(&sim.terrain, anchor_sim, down_sim, rest, radius);
                let lift = radius * (WHEEL_RENDER_SCALE - 1.0);
                let center_sim = anchor_sim + down_sim * (travel - lift);
                let wrot = mirror_half_turn()
                    * q
                    * DQuat::from_rotation_z(steer)
                    * DQuat::from_rotation_y(wm.spin);
                if let Some(grid) = self.scene.grid_mut(wm.grid) {
                    grid.transform.origin = volume_world_of(center_sim);
                    grid.transform.rotation = wrot;
                }
            }

            sync_body_deco(
                &mut self.scene,
                self.body_decos.get(&body.id().0),
                mirror,
                origin,
                rot,
            );
            sync_drill_cone(
                &mut self.scene,
                self.drill_vis.get(&body.id().0),
                sim.tools.get(&body.id().0),
                mirror,
                q,
                dvec3(body.position()),
                dt,
            );
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
            clear_mirror(&mut self.scene, mirror, self.body_decos.get(id));
            false
        });
    }

    /// Retire ONE body's mirror because something else now draws it: a
    /// `grid_body` binding hands that job to the map's own painted grid
    /// (ship-physics D4). Without this the hull would be drawn twice — the
    /// plating the map painted, and a material-coloured block of the same
    /// shape inside it, agreeing exactly, so the bug reads as "the hull looks
    /// wrong" rather than "there are two of them".
    fn retire_mirror(&mut self, body: u64) {
        if let Some(mirror) = self.body_mirrors.remove(&body) {
            clear_mirror(&mut self.scene, &mirror, self.body_decos.get(&body));
        }
    }

    /// Pick under a world ray: the sim-space point on the board plane, and
    /// the nearest model-bound entity within [`PICK_RADIUS`] (`-1` none).
    pub fn pick(&self, world: &World, origin: DVec3, dir: DVec3) -> (FixedVec3, i64) {
        // Where the cursor meets the ground, and in what frame.
        //
        // A volume map cannot use the `z = 0` plane: its ground sits at
        // `GROUND_Z - SCALE·z`, hundreds of world units below it, so the
        // plane hit lands far from anything the player is looking at and
        // every entity pick misses. The column path is untouched — it is
        // what every existing demo was measured against.
        let found = if self.volume {
            self.volume_ground_sim(origin, dir).map(|(sx, sy)| {
                let point = FixedVec3::new(Fixed::from_f64(sx), Fixed::from_f64(sy), Fixed::ZERO);
                // Back to world through the ENTITY transform, not
                // `volume_world_of`: the two differ by half a cell,
                // because `world_of` seats a sprite at its cell's centre
                // while the voxel map addresses corners. Comparing the
                // cursor against entity seats in the wrong one of those
                // costs half a cell of aim in x and y — which, at a pick
                // radius of three quarters of a cell, is most of it.
                (point, entity_world_of_in(true, point))
            })
        } else {
            ground_hit(origin, dir).map(|hit| {
                let (x, y) = sim_of(hit);
                (
                    FixedVec3::new(Fixed::from_f64(x), Fixed::from_f64(y), Fixed::ZERO),
                    hit,
                )
            })
        };
        // A ray that met no ground still answers about bodies: one
        // standing on a ledge against the sky is a thing a player points
        // at, and the ground point it does not have is reported as none
        // the way it always was.
        let point = found.map_or(FixedVec3::ZERO, |(p, _)| p);
        // **A body is a standing card, not the cell under it.** Measuring
        // the cursor's GROUND point against an entity's seat meant only
        // the feet could be clicked: a figure drawn a cell and a third
        // tall stands entirely outside the radius, so aiming at the chest
        // -- which is what a player aims at -- missed.
        //
        // So the ray is tested against the line the body stands on, from
        // its feet to the top of its drawn height, and the nearest hit
        // ALONG THE RAY wins. Depth order rather than ground distance is
        // also the honest answer where two bodies overlap on screen: the
        // one in front is the one being pointed at.
        let mut best: Option<(EntityId, f64)> = None;
        for &e in self.models.keys() {
            let Some(p) = world.position(e) else { continue };
            // Compose through the grid the entity rides (rotation + origin), the
            // same seat `build_instances` renders it at — hit-testing against the
            // bare `world_of(p)` mis-picks on a moved/rotated hull.
            let seat = self.place(e, p);
            // The column runs from the floor of the cell it stands in to
            // the top of its art. The cell, not the seat, because a model
            // is anchored at its own pivot and `model_drop` moves that on
            // purpose -- so the seat is where a body STANDS, not where its
            // art begins. Clicking the ground at its feet went on picking
            // it before this, and still does.
            //
            // World +z is down, so up is -z.
            let floor = seat + DVec3::Z * (SCALE * 0.5);
            let head = seat - DVec3::Z * self.drawn_height(e);
            let (d2, along) = ray_to_segment(origin, dir, floor, head);
            if d2 > PICK_RADIUS * PICK_RADIUS || along <= 0.0 {
                continue;
            }
            if best.map_or(true, |(_, nearest)| along < nearest) {
                best = Some((e, along));
            }
        }
        let entity = best.map_or(-1, |(e, _)| e.0 as i64);
        (point, entity)
    }

    /// How far above where it stands `e` is drawn, in world units.
    fn lift(&self, e: EntityId) -> f64 {
        self.entity_lift.get(&e).copied().unwrap_or(0.0)
    }

    /// How tall `e` is drawn, in world units, or a fallback where its
    /// model never measured itself.
    fn drawn_height(&self, e: EntityId) -> f64 {
        let tall = self
            .models
            .get(&e)
            .and_then(|&m| self.model_refs.get(m))
            .and_then(|&r| match r {
                // A `.kv6` is authored one voxel to a world unit, so its
                // own box is its height.
                ModelRef::Sprite(si) => self.sprites.models.get(si).map(|m| f64::from(m.kv6.zsiz)),
                ModelRef::Actor(ai) => self.actors.get(ai).map(|a| f64::from(a.tall)),
                ModelRef::Character(ci) => self.characters.get(ci).map(|c| f64::from(c.tall)),
            });
        tall.filter(|&t| t > 0.0).unwrap_or(PICK_FALLBACK_TALL)
    }

    /// Commands the map queued this trigger, for the host to route.
    pub fn drain_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.pending)
    }

    pub fn camera(&self) -> Camera {
        // Re-compose a followed focus HERE, per frame, rather than trusting the
        // world point the tick composed: with a smoothed hull that point is up
        // to a tick stale, and a stale centre makes the whole ship slide across
        // the screen — a worse artifact than the judder smoothing removes
        // (docs/plans/ship-physics.md §4.4). Unfollowed maps keep the stored
        // centre exactly as before.
        let mut orbit = self.camera;
        if let Some((e, p)) = self.camera_follow {
            // Through the SMOOTHED position, for §4.4's reason applied to the
            // rider rather than the hull: a camera pinned to the tick while
            // the body it follows is drawn between ticks makes that body
            // shake against a world sliding smoothly behind it.
            orbit.center = self.place(e, self.drawn_at(e, p));
        }
        let mut cam = orbit.to_roxlap();
        // Ride a grid (`camera_grid`): turn the WHOLE orbit frame — the eye
        // offset and the view basis — by that grid's rotation, so the deck stays
        // put on screen and the sky turns around it instead. Without this a
        // tumbling hull spins under a world-fixed camera, and since the crew's
        // position is grid-LOCAL, the direction "W" moves them in changes every
        // tick: the player is steering in the ship's frame while looking at the
        // world's. Composing here fixes both at once, and needs nothing from the
        // map's movement math — its view-relative input is already hull-local.
        if let Some(rot) = self
            .camera_grid
            .and_then(|g| self.scene.grid(g))
            .map(|g| g.transform.rotation)
        {
            let turn = |v: [f64; 3]| (rot * DVec3::from_array(v)).to_array();
            cam.forward = turn(cam.forward);
            cam.right = turn(cam.right);
            cam.down = turn(cam.down);
            cam.pos = (orbit.center - DVec3::from_array(cam.forward) * orbit.dist).to_array();
        }
        // Volume maps WITH TERRAIN: third-person camera collision (the keyhole
        // cutout is gone, so nothing else keeps the eye out of rock — a flipped
        // vehicle by the mountain face was a full-screen sandstone wall).
        //
        // The world-grid condition is the whole point of the pass, not a
        // shortcut: this exists to keep the eye out of TERRAIN, and a map that
        // declares volume for the physics alone (the ship — every voxel it has
        // is its own hull) has none. Left ungated, the ray would find the hull
        // the player is deliberately looking into and yank the camera in every
        // time it grazed a rim wall.
        // Walk the focus→eye ray through the CLIPPED scene (the same cut
        // the player sees; a 1-frame-old clip is fine) and pull the eye
        // just short of the first hit. Render-side only — the orbit
        // distance the wheel owns is untouched.
        if self.volume && self.world_grid.is_some() {
            let center = orbit.center;
            let eye = DVec3::from_array(cam.pos);
            let back = eye - center;
            let dist = back.length();
            if dist > 1e-6 {
                if let Some(hit) = self.scene.raycast_clipped(center, back / dist, dist) {
                    // The vehicle's own mirror grids are not obstacles —
                    // a grazing hit on the hull would judder the camera —
                    // and a hit within one cell of the focus means the
                    // FOCUS itself is buried (a degenerate ray born in
                    // rock), which pulling could only make worse.
                    let pulled = (hit.t - 12.0).max(24.0);
                    if hit.t > 16.0 && pulled < dist && !self.is_vehicle_grid(hit.grid) {
                        cam.pos = (center + back / dist * pulled).to_array();
                    }
                }
            }
            self.unbury_eye(&mut cam, center);
        }
        cam
    }

    /// Second camera-collision pass: dig the EYE itself out of terrain.
    /// The focus→eye march above stops at its FIRST hit, and when that
    /// hit is one of the vehicle's own mirror grids (the ray passes
    /// backward through the hull) the wall behind the vehicle is never
    /// seen — so a digger breaking out of a mountain face leaves the eye
    /// inside the face it just exited, and with open sky above the
    /// cockpit there is no roof-detect deck clip to cut that rock away.
    /// Sample the volume grid's voxels from the eye toward the focus and
    /// step past the whole solid run (a raycast can't do this: born in
    /// rock, it hits at t≈0). Deck-clipped voxels (`z < z_clip`) render
    /// as air, so sitting inside them is fine and skipping them keeps
    /// the underground camera steady. Render-side only, like the pull.
    fn unbury_eye(&self, cam: &mut Camera, center: DVec3) {
        let Some(grid) = self.world_grid.and_then(|id| self.scene.grid(id)) else {
            return;
        };
        let eye = DVec3::from_array(cam.pos);
        let toward = center - eye;
        let len = toward.length();
        if len <= 1e-6 {
            return;
        }
        let origin = grid.transform.origin;
        let vws = grid.transform.voxel_world_size;
        let z_clip = grid.z_clip;
        let solid = |p: DVec3| {
            let l = (p - origin) / vws;
            let v = IVec3::new(l.x.floor() as i32, l.y.floor() as i32, l.z.floor() as i32);
            !z_clip.is_some_and(|zc| v.z < zc) && grid.voxel_solid(v)
        };
        if !solid(eye) {
            return;
        }
        // Half-voxel steps toward the focus until the eye clears rock,
        // never closer than the 24-unit floor the pull pass also keeps;
        // +12 margin so the near plane doesn't kiss the exit face.
        let dirn = toward / len;
        let max_t = (len - 24.0).max(0.0);
        let mut t = 0.0;
        while t < max_t && solid(eye + dirn * t) {
            t += vws * 0.5;
        }
        cam.pos = (eye + dirn * (t + 12.0).min(max_t)).to_array();
    }

    /// Whether `id` is one of the physics-mirror grids (hull, deco,
    /// wheels, drill cone) — the camera-collision ray ignores those.
    fn is_vehicle_grid(&self, id: GridId) -> bool {
        self.body_mirrors.values().any(|m| {
            m.grid == id
                || m.deco_grid == Some(id)
                || m.wheels.iter().any(|w| w.grid == id)
                || m.drill.as_ref().is_some_and(|d| d.grid == id)
        })
    }
    pub fn orbit(&mut self, dyaw: f64, dpitch: f64, ddist: f64) {
        self.camera.orbit(dyaw, dpitch, ddist);
    }

    /// The sim-space ground point under a world ray (cursor → aim), in the
    /// same un-mirrored convention as [`pick`](Self::pick). `None` if the ray
    /// misses the ground plane.
    #[must_use]
    pub fn ground_sim(&self, origin: DVec3, dir: DVec3) -> Option<(f64, f64)> {
        if self.volume {
            return self.volume_ground_sim(origin, dir);
        }
        // Heightfield-aware pick: march the ray against the sim collision
        // store, so a click on a plateau / ramp lands on the cell actually
        // under the cursor instead of its z=0 shadow (docs/plans/rts-demo.md
        // §1b). Maps whose terrain is all at z 0 (chess, RPG floor) hit at
        // exactly the old plane intersection; the plane stays the fallback
        // for rays that never meet painted terrain. Render-side only.
        let plane = ground_hit(origin, dir)?;
        // March as far as the LOWEST ground the map has, not as far as the
        // datum. A hollow dug below the datum is ground the datum plane
        // sits above, so a march that stopped there never met the surface
        // and fell back to the plane -- and the cursor answered with a
        // point in mid-air over the hollow it was pointing into. Every
        // brush then worked a few cells short of where it was aimed, which
        // is the shape of "it paints hills and not hollows".
        // One past the lowest, not level with it: the march stops when the
        // ray gets BELOW the surface, so a cap exactly at the deepest
        // ground is reached before that ever happens and the fallback
        // fires on the very cell the ray was aimed at.
        let floor = self.terrain.lowest().min(0) - 1;
        let far = plane_hit(origin, dir, floor).unwrap_or(plane);
        let t_plane = (far - origin).length();
        let below = |t: f64| {
            let p = origin + dir * t;
            let (sx, sy) = sim_of(p);
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
            // Off the terrain altogether: the datum is still the honest
            // answer for a ray that met no ground at all.
            return Some(sim_of(plane));
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
        Some(sim_of(origin + dir * hi))
    }

    /// Where the cursor ray first meets solid ground on a **volume** map.
    ///
    /// A volume map cannot use the column-store march above, and the way
    /// it fails is quiet: `terrain` is the column heightmap, which is
    /// empty by design on a volume map, so every probe reads a ground
    /// height of zero, the march falls straight through to the `z = 0`
    /// plane, and the answer comes back in the column convention's
    /// coordinates rather than the isotropic cell grid's. The cursor
    /// lands somewhere plausible-looking and completely wrong — which is
    /// what a build placement that goes nowhere looks like from the
    /// outside.
    ///
    /// So: march the *render* grid, which holds exactly the geometry the
    /// player is pointing at, in the volume map's own transform.
    fn volume_ground_sim(&self, origin: DVec3, dir: DVec3) -> Option<(f64, f64)> {
        let id = self.world_grid?;
        let grid = self.scene.grid(id)?;
        let solid = |p: DVec3| {
            let sim = volume_sim_of(p);
            #[allow(clippy::cast_possible_truncation)]
            let cell = (
                sim.x.floor() as i64,
                sim.y.floor() as i64,
                sim.z.floor() as i64,
            );
            // The WORLD grid's mapping, not a cubic grid's: it is created
            // with `at_scale`, so its cells address differently.
            let (lo, _) = cell_box_to_volume_grid(cell.0, cell.1, cell.2, cell.0, cell.1, cell.2);
            grid.voxel_color(lo).is_some()
        };

        // Bound the march where the ray leaves the world downwards: below
        // sim z 0 there is nothing to hit, and an unbounded march off the
        // edge of a map is a hang rather than a miss.
        let floor_z = GROUND_Z; // world z of sim z = 0
        if dir.z.abs() < 1e-9 {
            return None;
        }
        let t_floor = (floor_z - origin.z) / dir.z;
        if t_floor <= 0.0 {
            return None; // pointing up, away from the ground
        }

        // Quarter-cell steps, then bisect the crossing — the same shape
        // as the column march, against the geometry that actually exists.
        let step = SCALE / 4.0;
        let mut t = 0.0;
        while t < t_floor && !solid(origin + dir * t) {
            t += step;
        }
        let hit = if t >= t_floor {
            // Nothing solid on the way down: answer with the bedrock
            // plane, so a click on empty sky still names a cell rather
            // than nothing at all.
            t_floor
        } else {
            let (mut lo, mut hi) = ((t - step).max(0.0), t);
            for _ in 0..12 {
                let mid = (lo + hi) * 0.5;
                if solid(origin + dir * mid) {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            hi
        };
        let sim = volume_sim_of(origin + dir * hit);
        Some((sim.x, sim.y))
    }

    // --- the grid cursor (`host_api` 24) -----------------------------------
    //
    // The two picks above answer "where does the cursor meet the ground",
    // which presumes a ground: a plane on a column map, the world grid on
    // a volume one. A map whose entire world is a moving hull has
    // neither — `volume_ground_sim` bails at its first `?` — and even
    // with a plane, a plane cannot name a cell of a two-deck ship at
    // attitude. So this asks the scene instead: which voxel of which grid
    // does the ray actually hit, and which cell of that grid's own
    // convention is it.

    /// How far the cursor ray is marched, in world voxels. The camera's
    /// own orbit tops out at 2000 and a hull is ~300 across, so this
    /// clears any framing of any map without being unbounded — an
    /// unbounded march off the edge of a world is a hang, not a miss.
    const CURSOR_RANGE: f64 = 4096.0;

    /// The cursor ray's first solid voxel across the scene's *queryable*
    /// grids (so a fog twin, which is presentation-only, is not in the
    /// way), or `None` while it meets nothing.
    ///
    /// **Clipped** on purpose: `raycast_clipped` reads each grid's
    /// `z_clip`, which `apply_deck_clip` has already set to the local
    /// crew's deck band — so the cursor lands on the deck the player is
    /// looking into rather than on the roof that was cut away to let them
    /// look. Gameplay traces would want the unclipped `raycast`; a cursor
    /// wants what is on screen.
    fn cursor_hit(&self) -> Option<RayHit> {
        let (origin, dir) = self.cursor_ray?;
        self.scene
            .raycast_clipped(origin, dir, MapRender::CURSOR_RANGE)
    }

    /// The scene grid a `pick_*`/`gizmo_*` handle names: a script grid by
    /// its handle, or the world/terrain grid for `-1`. `None` when the
    /// handle is stale or the map has painted no world grid yet.
    fn addressed_grid(&self, handle: i64) -> Option<GridId> {
        if handle < 0 {
            self.world_grid
        } else {
            self.grid_id(handle)
        }
    }

    /// The script handle of a scene grid, or `-1` (the world grid, and
    /// any grid the engine spawned for itself, are not script grids).
    fn grid_handle(&self, id: GridId) -> i64 {
        self.grids
            .iter()
            .position(|g| *g == Some(id))
            .map_or(-1, |i| i as i64)
    }

    /// The sim cell a grid-local voxel belongs to: the inverse of the
    /// three box transforms this file paints with, chosen by the same
    /// three-way rule `deck_clip_z` uses — the isotropic volume world
    /// grid addresses cells directly, a cubic script grid scales z like
    /// x/y, and a column grid gives a cell one voxel row.
    #[allow(clippy::cast_possible_truncation)]
    fn cell_of_voxel(&self, id: GridId, v: IVec3) -> (i64, i64, i64) {
        let (vx, vy, vz) = (i64::from(v.x), i64::from(v.y), i64::from(v.z));
        if self.volume && Some(id) == self.world_grid {
            // `cell_box_to_volume_grid` inverted.
            return (-vx - 1, vy, -vz - 1);
        }
        let s = SCALE as i64;
        let gz = GROUND_Z as i64;
        // `sim_box_to_world` / `cell_box_to_cubic` inverted: world X is
        // mirrored (cell x owns voxels `[-(x+1)·S, -x·S)`), world z grows
        // down from `GROUND_Z`, and only a cubic grid scales z.
        (
            (-vx - 1).div_euclid(s),
            vy.div_euclid(s),
            if self.grid_is_cubic(id) {
                (gz + s - 1 - vz).div_euclid(s)
            } else {
                gz - vz
            },
        )
    }

    /// A sim cell as the script's fixed-point vector.
    #[allow(clippy::cast_possible_truncation)]
    fn cell_vec(cell: (i64, i64, i64)) -> FixedVec3 {
        let int = |v: i64| Fixed::from_int(i32::try_from(v).unwrap_or(0));
        FixedVec3::new(int(cell.0), int(cell.1), int(cell.2))
    }

    /// The eight world-space corners of an inclusive sim-cell box in
    /// `handle`'s frame, in the order `draw_gizmo_box` walks its edges.
    ///
    /// One place where all three cell conventions and the grid's pose
    /// meet, so a gizmo lands exactly on the voxels `voxel_fill_in`
    /// painted with the same numbers — including on a hull that is
    /// somewhere else entirely by the time it is drawn.
    fn cell_box_corners(&self, handle: i64, a: (i64, i64, i64), b: (i64, i64, i64)) -> [DVec3; 8] {
        let id = self.addressed_grid(handle);
        let (lo, hi, vws) = match id {
            Some(id) if self.volume && Some(id) == self.world_grid => {
                let (lo, hi) = cell_box_to_volume_grid(a.0, a.1, a.2, b.0, b.1, b.2);
                (lo, hi, SCALE)
            }
            Some(id) if self.grid_is_cubic(id) => {
                let (lo, hi) = cell_box_to_cubic(a.0, a.1, a.2, b.0, b.1, b.2);
                (lo, hi, 1.0)
            }
            _ => {
                let (lo, hi) = sim_box_to_world(
                    a.0.min(b.0),
                    a.1.min(b.1),
                    a.2.min(b.2),
                    a.0.max(b.0),
                    a.1.max(b.1),
                    a.2.max(b.2),
                );
                (lo, hi, 1.0)
            }
        };
        // A voxel box is inclusive, so the far corner is one voxel past
        // `hi` — the outside of the last cell rather than its near edge.
        let mn = DVec3::new(f64::from(lo.x), f64::from(lo.y), f64::from(lo.z)) * vws;
        let mx = DVec3::new(
            f64::from(hi.x) + 1.0,
            f64::from(hi.y) + 1.0,
            f64::from(hi.z) + 1.0,
        ) * vws;
        // The grid's pose, if it has one: an unbound world frame is the
        // identity, which is what a map with no grids expects.
        let pose = id.and_then(|id| self.scene.grid(id)).map(|g| g.transform);
        let place = |p: DVec3| match pose {
            Some(t) => t.rotation * p + t.origin,
            None => p,
        };
        [
            place(DVec3::new(mn.x, mn.y, mn.z)),
            place(DVec3::new(mx.x, mn.y, mn.z)),
            place(DVec3::new(mx.x, mx.y, mn.z)),
            place(DVec3::new(mn.x, mx.y, mn.z)),
            place(DVec3::new(mn.x, mn.y, mx.z)),
            place(DVec3::new(mx.x, mn.y, mx.z)),
            place(DVec3::new(mx.x, mx.y, mx.z)),
            place(DVec3::new(mn.x, mx.y, mx.z)),
        ]
    }

    /// A sim-space point of `handle`'s frame, in world space — the
    /// `gizmo_line` endpoint transform. Uses the same seat convention as
    /// an entity's (`entity_world_of_in`, cell centres), because a map
    /// draws a line between things it can also stand on.
    fn gizmo_point(&self, handle: i64, p: FixedVec3) -> DVec3 {
        let id = self.addressed_grid(handle);
        let cubic = id.is_some_and(|id| self.grid_is_cubic(id));
        // A script grid's own z convention; the world frame keeps the
        // map's (`self.volume`), exactly as `place_in` splits them.
        let local = entity_world_of_in(if handle < 0 { self.volume } else { cubic }, p);
        match id.and_then(|id| self.scene.grid(id)) {
            Some(g) => g.transform.rotation * local + g.transform.origin,
            None => local,
        }
    }

    /// Draw this frame's queued overlay outlines. Retained state, so a
    /// map that draws on the tick clock keeps its ghost on screen through
    /// the frames between ticks; `gizmo_clear` is the map's to call.
    fn draw_gizmos(&self, renderer: &mut SceneRenderer, camera: &Camera) {
        if !self.gizmos.is_empty() {
            renderer.draw_lines(camera, &self.gizmos);
        }
    }

    /// The camera's focus point in the same sim convention as
    /// [`ground_sim`](Self::ground_sim). Maps follow the local player with
    /// `camera_focus`, so this is effectively the local player's position —
    /// the host derives the mouse-aim direction from it without the genre.
    #[must_use]
    pub fn camera_center_sim(&self) -> (f64, f64) {
        sim_of(self.camera.center)
    }
    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// The HUD widget list the map rebuilt this tick, for the host's egui pass.
    #[must_use]
    pub fn ui_widgets(&self) -> &[Placed] {
        &self.ui_widgets
    }

    /// Append a widget, consuming any pending `ui_pin`.
    fn stack(&mut self, widget: UiWidget) {
        let over = self.ui_pin.take();
        self.ui_widgets.push(Placed { over, widget });
    }

    /// A HUD texture's RGBA8 pixels + dimensions, by the id `ui_texture`
    /// returned to the map. `None` for an out-of-range id.
    #[must_use]
    pub fn ui_texture_data(&self, id: usize) -> Option<&(Vec<u8>, u32, u32)> {
        self.ui_textures.get(id)
    }

    /// …and how many times the map has repainted it, so a painter that
    /// caches the upload can tell its copy is stale.
    #[must_use]
    pub fn ui_texture_stamp(&self, id: usize) -> u64 {
        self.ui_stamps.get(id).copied().unwrap_or(0)
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

    /// …and where the cursor is in them, so a map can tell a click on its
    /// chart from a click on the ground behind it.
    pub fn set_ui_cursor(&mut self, at: Option<(i64, i64)>) {
        self.ui_cursor = at;
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
    /// Let out whatever bursts were asked for, then carry every particle
    /// already in the air one frame.
    ///
    /// Emitters are added and retired in the same breath: a burst is one
    /// puff of particles and then nothing, and an emitter left behind
    /// would be a slot held for the rest of the map. The particles it
    /// already spawned outlive it — the system keeps the state a live
    /// particle needs until the last of them dies.
    fn update_sparks(&mut self, renderer: &mut SceneRenderer, dt: f64) {
        if !self.spark_queue.is_empty() {
            // Registered on first use rather than in `new`: most maps
            // never throw anything, and a model costs an upload.
            let model = *self.spark_model.get_or_insert_with(|| {
                renderer.define_material(
                    SPARK_MATERIAL,
                    roxlap_formats::material::Material {
                        alpha: SPARK_ALPHA,
                        mode: roxlap_formats::material::BlendMode::AlphaBlend,
                        emissive: 0,
                    },
                );
                renderer.add_sprite_model(&spark_kv6())
            });
            for spark in std::mem::take(&mut self.spark_queue) {
                let id = self.sparks.add_emitter(ParticleEmitterDef {
                    // Exactly where it was asked for. Where a burst
                    // *looks* right is the caller's -- a thing drawn above
                    // its own feet knows how far, and this does not.
                    pos: [spark.at.x as f32, spark.at.y as f32, spark.at.z as f32],
                    spawn: SpawnMode::Burst(spark.count),
                    lifetime: spark.life..spark.life * 1.6,
                    velocity: VelocityDef {
                        spread: spark.speed,
                        ..VelocityDef::default()
                    },
                    // **Through the floor rather than killed on it.** A
                    // burst goes off at ground level by definition, and a
                    // splash that the ground eats is a splash nobody ever
                    // sees -- which is exactly how this shipped the first
                    // time. They fade before they have sunk far enough to
                    // notice.
                    collision: CollisionMode::None,
                    scale: SPARK_SCALE,
                    scale_end: Some(SPARK_SCALE * 0.3),
                    fade_out_frac: 0.4,
                    tint: Rgb(spark.tint),
                    material: SPARK_MATERIAL,
                    ..ParticleEmitterDef::new(model)
                });
                self.sparks.remove_emitter(id);
            }
        }
        // **Particles, not emitters.** A burst's emitter is retired the
        // instant it is added -- it has already spawned everything it ever
        // will -- and `emitter_count` does not count a retired one. Asking
        // it whether there is work left me never stepping the system at
        // all, so no particle was ever handed to the renderer and none of
        // this drew anything.
        if self.sparks.particle_count() > 0 {
            self.sparks.tick_with_scene(renderer, dt, &self.scene);
        }
    }

    fn update_actors(&mut self, renderer: &mut SceneRenderer, camera: &Camera, dt: f64) {
        if self.actors.is_empty() {
            return;
        }
        // Register each actor model's directional clips once (needs the
        // renderer, which doesn't exist until the first frame).
        for i in awaiting_clips(&self.actors) {
            let am = &mut self.actors[i];
            let mut reg = Vec::with_capacity(am.states.len());
            for (name, clips) in &am.states {
                let ids: Vec<VoxelClipId> =
                    clips.iter().map(|c| renderer.add_voxel_clip(c)).collect();
                reg.push((*name, ids));
            }
            am.registered = Some(reg);
        }

        // Read once: the loop below borrows `entity_actors` mutably.
        let mode = self.sprite_mode();

        // Create / update each present actor entity from this frame's targets.
        let present: BTreeSet<EntityId> = self.actor_targets.iter().map(|t| t.0).collect();
        for &(e, ai, pos, local_yaw, rot) in &self.actor_targets {
            let (facing, up) = actor_pose(local_yaw, rot);
            let Some(inst) = self.entity_actors.get_mut(&e) else {
                continue;
            };
            match inst.id {
                Some(id) => {
                    renderer.set_actor_pose(id, pos, facing, up);
                    if inst.anim != inst.applied_anim {
                        renderer.set_actor_state(id, inst.anim);
                        inst.applied_anim = inst.anim;
                    }
                    if inst.tint != inst.applied_tint {
                        renderer.set_actor_tint(id, Rgb(inst.tint));
                        inst.applied_tint = inst.tint;
                    }
                    if mode != inst.applied_mode {
                        renderer.set_actor_mode(id, mode);
                        inst.applied_mode = mode;
                    }
                }
                None => {
                    if let Some(reg) = self.actors.get(ai).and_then(|a| a.registered.as_ref()) {
                        // roxlap 0.30: `add_billboard_actor` returns `None` for a
                        // malformed def (no states / empty dirs); skip if so.
                        if let Some(id) =
                            renderer.add_billboard_actor(actor_def(reg, mode), pos, local_yaw)
                        {
                            // `add_billboard_actor` still takes a world yaw, so
                            // give a fresh actor its real floor at once — else
                            // it draws one frame world-aligned.
                            renderer.set_actor_pose(id, pos, facing, up);
                            renderer.set_actor_state(id, inst.anim);
                            if inst.tint != WHITE_TINT {
                                renderer.set_actor_tint(id, Rgb(inst.tint));
                            }
                            inst.id = Some(id);
                            inst.applied_anim = inst.anim;
                            inst.applied_tint = inst.tint;
                            inst.applied_mode = mode;
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

    /// Drive the rigged `.rkc` characters for this frame: create / re-clip /
    /// move / retire one [`CharacterId`] per character-bound entity from the
    /// targets `build_instances` computed, then advance each skeleton by
    /// `dt`. Render-side only.
    ///
    /// Unlike the actor path there is nothing to register up front: roxlap's
    /// `add_character` uploads a character's meshes when the INSTANCE is
    /// created, so registration is per entity and its teardown frees them.
    fn update_characters(&mut self, renderer: &mut SceneRenderer, dt: f64) {
        if self.characters.is_empty() {
            return;
        }
        let present: BTreeSet<EntityId> = self.char_targets.iter().map(|t| t.0).collect();
        for &(e, ci, pos, yaw) in &self.char_targets {
            let (Some(inst), Some(model)) =
                (self.entity_chars.get_mut(&e), self.characters.get(ci))
            else {
                continue;
            };
            // A clip switch re-registers: the clip is baked into the skeleton
            // at `add_character` and roxlap has no setter for it. Dropping the
            // old instance first also frees its uploaded meshes, so a state
            // machine cycling states doesn't leak models.
            if inst.applied_clip.is_some() && inst.applied_clip != Some(inst.clip) {
                if let Some(id) = inst.id.take() {
                    renderer.remove_character(id);
                }
            }
            let id = if let Some(id) = inst.id {
                id
            } else {
                let id = renderer.add_character(&model.ch, model.clip_of(inst));
                inst.id = Some(id);
                inst.applied_clip = Some(inst.clip);
                id
            };
            // Seat first, then tick: `advance_character` re-solves the whole
            // skeleton from the root we just set, so the frame ends on one
            // consistent pose (a tick-then-seat order would re-solve twice).
            renderer.set_character_world_transform(id, model.transform(pos, yaw));
            renderer.advance_character(id, dt);
        }

        // Retire characters whose entity is gone this frame (despawned).
        let gone: Vec<EntityId> = self
            .entity_chars
            .iter()
            .filter(|(e, c)| c.id.is_some() && !present.contains(e))
            .map(|(&e, _)| e)
            .collect();
        for e in gone {
            if let Some(inst) = self.entity_chars.remove(&e) {
                if let Some(id) = inst.id {
                    renderer.remove_character(id);
                }
            }
        }
    }

    /// Whether this map drives roxlap's DYNAMIC layer (billboard actors or
    /// rigged characters). That layer is reset by `set_sprites`, so a map on
    /// it uploads its static sprite set exactly once and mirrors the
    /// selection markers through the dynamic path instead.
    fn dynamic_layer(&self) -> bool {
        // A turned plain model joins actors and characters here: it has to
        // be a POSED instance rather than a positional one, and the posed
        // layer is what `set_sprites` resets — so the static set must be
        // uploaded once instead of rebuilt per frame.
        !self.actors.is_empty()
            || !self.characters.is_empty()
            || !self.entity_yaw.is_empty()
            || !self.entity_orient.is_empty()
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
        // The map's own field of view, if it asked for one. `hz` is the
        // focal length in pixels, so `hx / tan(fov/2)` is the definition
        // read backwards. Applied here rather than where the host builds
        // the settings, because it is the MAP's look and both backends
        // derive their projection from this one struct.
        let settings = &self.projected(settings);
        // GPU backend has its own sky path — upload the panorama once.
        if !self.sky_uploaded {
            if let Some((rgba, w, h)) = &self.sky_panorama {
                renderer.set_sky_panorama(rgba, *w, *h);
            }
            self.sky_uploaded = true;
        }
        // Debris puffs ride the static instance list, which is only safe
        // while that list is genuinely rebuilt every frame. On a
        // dynamic-layer map it is uploaded ONCE — see below — so a puff
        // appended here would be baked into that single upload and hang
        // in the air for the rest of the match, and every puff after it
        // would be invisible. A crater's worth of them frozen over the
        // hole is a permanent, expensive dust cloud.
        //
        // So: age them either way, and only draw them where drawing them
        // works. Dust on a dynamic-layer map wants the dynamic path,
        // which is the FX bridge of D-6 rather than a patch here.
        self.sync_puffs(dt);
        // Upload the static sprite set *before* driving the actors:
        // `set_sprites` resets the dynamic layer (clips + actors +
        // characters), so a map with animated models uploads its static set
        // exactly once (before any of them is registered), while a static map
        // (chess) rebuilds the set each frame (it has nothing to clobber).
        if self.dynamic_layer() {
            if !self.sprites_uploaded {
                // Selection markers must NOT be in that one upload. On this
                // path they ride the dynamic layer (`sync_rings` mirrors
                // them), so a marker already present on the first rendered
                // frame is baked into the static set — frozen at that
                // position for the rest of the session — and then drawn a
                // SECOND time, live, by the mirror. A map that selects
                // something in `local_init` sees the ghost immediately; one
                // that only selects on a click never does, which is why the
                // RTS demo never hit it. `sync_rings` still reads the
                // markers from the list below, so put them back after.
                let markers = self.take_markers();
                renderer.set_sprites(&self.sprites);
                self.sprites.instances.extend(markers);
                self.sprites_uploaded = true;
            }
        } else {
            renderer.set_sprites(&self.sprites);
        }
        self.update_actors(renderer, camera, dt);
        self.update_characters(renderer, dt);
        self.update_sparks(renderer, dt);
        self.sync_props(renderer);
        self.sync_rings(renderer);
        self.sync_ghosts(renderer);
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
        frame.sky_color = self.look.sky_color.unwrap_or(sky_color);
        frame.sky = self.sky.as_ref(); // CPU backend sky panorama
                                       // Sprites are flat-lit on both backends; this is just the on/off opt-in.
        frame.draw_sprites = true;
        // Light through the dynamic rig — the map's sun as a real
        // directional light over the baked ambient/AO channel, with
        // stylized shadows, so voxel edges READ. The sim→world direction
        // composes the world-X mirror and the z-down flip (`R_y(π)`):
        // `(dx, dy, dz) → (−dx, dy, −dz)`.
        //
        // Volume maps take it unconditionally (isotropic edges need it).
        // A column map opts in with `set_shadows`, and without that keeps
        // the legacy per-face side_shades, byte-stable.
        let rig = self
            .look
            .shadows
            .or_else(|| self.volume.then_some(DEFAULT_SHADOW_STRENGTH));
        if let (Some(strength), Some((dir, intensity))) = (rig, self.look.sun) {
            #[allow(clippy::cast_possible_truncation)]
            {
                frame.lights = Some(roxlap_render::LightRig {
                    sun: Some(roxlap_render::DirectionalLight {
                        direction: [-dir.x as f32, dir.y as f32, -dir.z as f32],
                        color: [1.0; 3],
                        intensity,
                        casts_shadow: true,
                    }),
                    ambient: [0.62; 3],
                    shadow_strength: strength,
                    shadow_bias_voxels: 1.5,
                    shadow_max_dist: 2400.0,
                    ..roxlap_render::LightRig::default()
                });
            }
        } else {
            frame.side_shades = self.side_shades;
        }
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
        self.drive_mip_scan(renderer);
        renderer.render(&mut self.scene, camera, &frame);

        self.draw_drag_rect(renderer, camera);
        self.draw_gizmos(renderer, camera);
        if debug {
            self.draw_debug_footprints(renderer, camera);
        }
    }

    /// Mirror this frame's selection-marker instances onto the renderer's
    /// dynamic sprite layer (see the `ring_model` field for why the static
    /// path can't carry them on actor maps). Tear-down + re-add per frame:
    /// selection counts are tiny and `remove_sprite_instance` is an O(1)
    /// swap, so reconciliation would be complexity for nothing.
    /// Age the debris puffs and drop the dead ones, without drawing any.
    ///
    /// The half of [`sync_puffs`](Self::sync_puffs) that has to happen on
    /// every map: a puff nobody draws must still expire, or a map that
    /// carves and never draws dust accumulates it forever.
    fn age_puffs(&mut self, dt: f64) {
        if self.puffs.is_empty() {
            return;
        }
        for p in &mut self.puffs {
            p.age += dt;
        }
        self.puffs.retain(|p| p.age < PUFF_TTL);
    }

    /// Age, cull and draw the debris puffs: a voxel each, in an effects
    /// grid of the renderer's own, cleared and repainted every frame.
    ///
    /// **Voxels rather than sprites, and that is the fix.** Dust used to
    /// be sprite instances appended to the STATIC set — safe only while
    /// that set is rebuilt each frame, which stops being true the moment
    /// a map poses anything. On a dynamic-layer map the set is uploaded
    /// once, so the dust alive at that instant froze onto the screen for
    /// the rest of the match and every later puff was invisible.
    ///
    /// A grid has no such contract: the scene is walked every frame, so
    /// geometry painted into one appears and disappears when it is told
    /// to. It is also the native idiom here — this is a voxel game, and
    /// a cell of dust in the colour of the cell that was carved reads
    /// exactly right.
    fn sync_puffs(&mut self, dt: f64) {
        self.age_puffs(dt);
        if self.puffs.is_empty() && self.fx_painted.is_empty() {
            return;
        }
        let grid = if let Some(g) = self.fx_grid {
            g
        } else {
            let g = self.grid_spawn_cubic(0, 0, 0);
            self.fx_grid = Some(g);
            g
        };
        for (x, y, z) in std::mem::take(&mut self.fx_painted) {
            self.voxel_clear_in(grid, x, y, z);
        }
        let live: Vec<((i64, i64, i64), i64)> = self
            .puffs
            .iter()
            .map(|p| {
                // Half a life in, the dust lifts a cell. Cell granularity
                // is all a grid has, so it is a hop rather than a drift —
                // which at four hundredths of a second reads as a puff.
                let lift = i64::from(p.age * 2.0 > PUFF_TTL);
                (
                    (p.cell.0, p.cell.1, p.cell.2 + lift),
                    i64::from(p.color) & 0xffff_ffff,
                )
            })
            .collect();
        for (cell, color) in live {
            self.voxel_set_in(grid, cell.0, cell.1, cell.2, color);
            self.fx_painted.push(cell);
        }
    }

    /// Place the props that ride a turning grid, on the renderer's DYNAMIC
    /// sprite layer — the only one that takes an orientation.
    ///
    /// The pose is the grid's rotation applied to the model's own axes, and the
    /// pivot drop turns WITH it: the drop is a model-space offset ("the bottom
    /// face is this far below the pivot"), so on a hull rolled onto its side it
    /// has to push the crate sideways, not down. Leaving it in world z is what
    /// makes a rotated prop sink through its own deck.
    ///
    /// Instances are torn down and re-issued each frame, like
    /// [`sync_rings`](Self::sync_rings): a map carries a handful of props, and
    /// the alternative is tracking a per-entity instance across model rebinds,
    /// grid hops and despawns for no visible gain.
    fn sync_props(&mut self, renderer: &mut SceneRenderer) {
        for id in self.prop_ids.drain(..) {
            renderer.remove_sprite_instance(id);
        }
        for (si, seat, rot, drop_rot, drop) in std::mem::take(&mut self.prop_targets) {
            let model = if let Some(&m) = self.prop_models.get(&si) {
                m
            } else {
                let Some(sprite) = self.sprites.models.get(si) else {
                    continue;
                };
                let m = renderer.add_sprite_model(&sprite.kv6);
                self.prop_models.insert(si, m);
                m
            };
            let axis = |v: DVec3| [v.x as f32, v.y as f32, v.z as f32];
            let pos = seat + drop_rot * DVec3::new(0.0, 0.0, -drop);
            let xf = DynSpriteTransform {
                pos: axis(pos),
                right: axis(rot * DVec3::X),
                up: axis(rot * DVec3::Y),
                forward: axis(rot * DVec3::Z),
            };
            if let Some(id) = renderer.add_sprite_instance_posed(model, xf) {
                self.prop_ids.push(id);
            }
        }
    }

    /// Place this frame's placement ghosts, and take away last frame's.
    ///
    /// Same teardown-and-reissue as [`sync_props`](Self::sync_props), for
    /// the same reason: a preview is one or two instances that move every
    /// frame anyway, so tracking their lifetimes would buy nothing.
    ///
    /// A ghost is a **preview and not an entity**: it casts no shadow, is
    /// not in the world the cursor picks against, and nothing in the
    /// simulation can see it. That is the whole reason the verb exists —
    /// entities are simulation state, and the layer that wants to show
    /// what it is about to place is the one layer that may not make one.
    fn sync_ghosts(&mut self, renderer: &mut SceneRenderer) {
        for id in self.ghosts.ids.drain(..) {
            renderer.remove_sprite_instance(id);
        }
        let targets = std::mem::take(&mut self.ghosts.targets);
        // First, and outside the `targets.is_empty()` gate below: an actor
        // preview outlives the frame that asked for it, so a frame asking
        // for nothing is exactly what has to retire it.
        let wanted = self.ghosts.actor.take();
        self.sync_ghost_actor(renderer, wanted);
        if targets.is_empty() {
            return;
        }
        self.ghost_material(renderer);
        for (si, pos, yaw, alpha) in targets {
            let model = if let Some(&m) = self.prop_models.get(&si) {
                m
            } else {
                let Some(sprite) = self.sprites.models.get(si) else {
                    continue;
                };
                let m = renderer.add_sprite_model(&sprite.kv6);
                self.prop_models.insert(si, m);
                m
            };
            let rot = DQuat::from_rotation_z(yaw);
            let axis = |v: DVec3| [v.x as f32, v.y as f32, v.z as f32];
            let xf = DynSpriteTransform {
                pos: axis(pos),
                right: axis(rot * DVec3::X),
                up: axis(rot * DVec3::Y),
                forward: axis(rot * DVec3::Z),
            };
            let Some(id) = renderer.add_sprite_instance_posed(model, xf) else {
                continue;
            };
            renderer.set_sprite_instance_material(id, GHOST_MATERIAL);
            renderer.set_sprite_instance_alpha(id, alpha);
            // A preview that darkened the ground under it would read as a
            // prop already standing there, which is the one thing it is
            // not.
            renderer.set_sprite_instance_shadow_flags(
                id,
                roxlap_render::ShadowFlags {
                    casts: false,
                    receives: false,
                },
            );
            self.ghosts.ids.push(id);
        }
    }

    /// Register the translucent material the previews wear, once.
    ///
    /// Deferred rather than declared at startup: a non-opaque palette is
    /// what switches the renderer onto its two-sweep sprite path, and a map
    /// that never previews anything should not pay for it.
    fn ghost_material(&mut self, renderer: &mut SceneRenderer) {
        if self.ghosts.material {
            return;
        }
        renderer.define_material(
            GHOST_MATERIAL,
            roxlap_formats::material::Material {
                alpha: 255,
                mode: roxlap_formats::material::BlendMode::AlphaBlend,
                emissive: 0,
            },
        );
        self.ghosts.material = true;
    }

    /// Keep the actor preview in step with what this frame asked for.
    ///
    /// **Kept alive between frames, unlike the sprite ghosts.** An actor
    /// carries an animation clock and a directional clip picked from the
    /// camera's bearing; tearing one down and rebuilding it every frame
    /// would restart both, so an idling preview would stand frozen on its
    /// first frame and orbiting the camera would flicker through clip
    /// swaps. So it is created when a kind is first previewed, moved while
    /// the cursor moves, and retired when a frame asks for nothing.
    ///
    /// One at a time, because a cursor previews one thing.
    fn sync_ghost_actor(
        &mut self,
        renderer: &mut SceneRenderer,
        wanted: Option<(usize, DVec3, f64, u8)>,
    ) {
        let Some((ai, pos, yaw, alpha)) = wanted else {
            if let Some((_, id)) = self.ghosts.live.take() {
                renderer.remove_billboard_actor(id);
            }
            return;
        };
        // A different kind is a different set of directional clips, so it
        // is a different actor rather than a state change on this one.
        if let Some((was, id)) = self.ghosts.live {
            if was != ai {
                renderer.remove_billboard_actor(id);
                self.ghosts.live = None;
            }
        }
        let seat = [pos.x as f32, pos.y as f32, pos.z as f32];
        if self.ghosts.live.is_none() {
            self.ghost_material(renderer);
            let mode = self.sprite_mode();
            // Absent only on the first frame, before `update_actors` has had
            // a renderer to register the clips with.
            let Some(reg) = self.actors.get(ai).and_then(|a| a.registered.as_ref()) else {
                return;
            };
            // Whichever state the art listed first. Which one a preview
            // should play is the map's question, not the host's, and
            // nothing has asked it.
            let Some(id) = renderer.add_billboard_actor(actor_def(reg, mode), seat, yaw) else {
                return; // a malformed def: no states, or a state with no dirs
            };
            renderer.set_actor_material(id, GHOST_MATERIAL);
            // A preview that darkened the ground under it would read as a
            // character already standing there, which is the one thing it
            // is not.
            renderer.set_actor_shadow_flags(
                id,
                roxlap_render::ShadowFlags {
                    casts: false,
                    receives: false,
                },
            );
            self.ghosts.live = Some((ai, id));
        }
        let Some((_, id)) = self.ghosts.live else {
            return;
        };
        // Every frame, so the preview follows the cursor and a map may fade
        // it without respawning anything.
        let (facing, up) = actor_pose(yaw, DQuat::IDENTITY);
        renderer.set_actor_pose(id, seat, facing, up);
        renderer.set_actor_alpha(id, alpha);
    }

    /// Lift the selection markers out of the static instance list.
    fn take_markers(&mut self) -> Vec<SpriteInstanceDesc> {
        let (markers, rest) = std::mem::take(&mut self.sprites.instances)
            .into_iter()
            .partition(|i| i.model == HIGHLIGHT_MODEL);
        self.sprites.instances = rest;
        markers
    }

    fn sync_rings(&mut self, renderer: &mut SceneRenderer) {
        if !self.dynamic_layer() {
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
        for &(_, ai, pos, ..) in &self.actor_targets {
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
    fn burst(&mut self, at: FixedVec3, count: i64, speed: Fixed, life: Fixed, color: i64) {
        // A burst of nothing is a call a map made for no reason, and one
        // that lasts no time is a frame nobody sees. Both are silently
        // dropped rather than queued: this is decoration, and refusing it
        // loudly would be worse than not drawing it.
        let count = u32::try_from(count).unwrap_or(0).min(SPARK_CAP);
        if count == 0 || life <= Fixed::ZERO {
            return;
        }
        self.spark_queue.push(Spark {
            at: world_of(at),
            count,
            speed: (speed.to_f64() * SCALE) as f32,
            life: life.to_f64() as f32,
            tint: (color as u32) & 0x00FF_FFFF,
        });
    }

    fn model_box(&mut self, w: i64, h: i64, d: i64, color: i64) -> i64 {
        self.sprites
            .models
            .push(sprite_box(w as u32, h as u32, d as u32, color as u32, true));
        self.push_sprite_model(self.sprites.models.len() - 1)
    }

    #[allow(clippy::too_many_arguments, clippy::many_single_char_names)]
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
    ) -> i64 {
        self.sprites.models.push(sprite_box_sides(
            w as u32,
            h as u32,
            d as u32,
            x as u32,
            neg_x as u32,
            y as u32,
            neg_y as u32,
            z as u32,
            neg_z as u32,
        ));
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
            //
            // `cast` joins them: a spell is a gesture with an end, and a
            // looping one leaves a caster winding up forever after the
            // spell has left.
            //
            // …and `decay`, which is a corpse going to bone: what it holds
            // is what the body IS from then on, and a looping one would
            // have a skeleton rot back into a carcass every two seconds.
            let mut opts = GifImportOpts::default();
            if matches!(
                state.as_str(),
                "death" | "decay" | "attack" | "dodge" | "hurt" | "cast"
            ) {
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
            // state name. Bounded by the number of model DEFINITIONS, which
            // is content-sized -- not by frames. Models are no longer all
            // defined at `init`: a local layer defines one when a placement
            // preview first needs a kind's art.
            let name: &'static str = Box::leak(state.clone().into_boxed_str());
            actor_states.push((name, clips));
        }
        if actor_states.is_empty() {
            return -1;
        }

        // Measure the *opaque* bounding box over every frame and side of the
        // FIRST state — the idle, by convention — so transparent padding
        // around the art doesn't shrink the character or lift it off the
        // ground. Size and ground every state against that one box, so the
        // character stays one size and grounded as it animates.
        //
        // The first state rather than all of them, for the reason
        // `model_character` already measures its first clip: **a death
        // sprawl reaches past a standing figure at both ends**, and folded
        // into the box it shrinks the idle pose and grounds it below its own
        // feet — a character that stands smaller and floats, the moment its
        // art gains an animation that lies down. Measured on real art: a
        // standing hero spanned `z 23..=64` and its death `z 11..=75`, so
        // merging left the idle 65% of its size, hanging twelve voxels up.
        //
        // A bigger pose still draws bigger, which is what one scale for
        // every state means.
        let mut bb: Option<(u32, u32, u32, u32)> = None; // (min_x, max_x, min_z, max_z)
        for c in actor_states.first().into_iter().flat_map(|(_, cs)| cs) {
            bb = merge_box(bb, opaque_box(c));
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
            #[allow(clippy::cast_possible_truncation)]
            tall: target_h as f32,
        });
        self.push_model_ref(ModelRef::Actor(self.actors.len() - 1))
    }

    fn model_character(&mut self, asset_path: &str, height_cells: Fixed) -> i64 {
        let Some(bytes) = self.assets.get(asset_path) else {
            eprintln!("monada-host: model_character: missing asset {asset_path:?}");
            return -1;
        };
        let ch = match character::parse(bytes) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("monada-host: model_character: {asset_path:?} is not a usable .rkc: {e}");
                return -1;
            }
        };
        // Size and ground from the FIRST clip's envelope (the idle, by
        // convention): a death sprawl or a lunge would otherwise shrink the
        // character every other frame of its life. A clip-less rig measures
        // its rest pose.
        let first_clip = (!ch.clips.is_empty()).then_some(0);
        let Some((top, bottom)) = clip_z_envelope(&ch, first_clip) else {
            eprintln!("monada-host: model_character: {asset_path:?} draws no static meshes");
            return -1;
        };
        let target_h = height_cells.to_f64() * SCALE;
        let native_h = bottom - top;
        // `height_cells <= 0` keeps the artist's scale (one model voxel per
        // world voxel) — the right default for a rig authored against the
        // map's own voxel grid.
        #[allow(clippy::cast_possible_truncation)]
        let scale = if target_h > 0.0 && native_h > 1e-6 {
            (target_h / native_h) as f32
        } else {
            1.0
        };
        let mut clips: BTreeMap<String, usize> = BTreeMap::new();
        for (i, clip) in ch.clips.iter().enumerate() {
            if !clip.name.is_empty() {
                // First wins: a duplicate clip name resolves to the earlier
                // clip rather than silently shadowing it.
                clips.entry(clip.name.clone()).or_insert(i);
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        self.characters.push(CharacterModel {
            ch,
            clips,
            scale,
            lift: (bottom * f64::from(scale)) as f32,
            drop: 0.0,
            tall: (native_h * f64::from(scale)) as f32,
            warned: BTreeSet::new(),
        });
        self.push_model_ref(ModelRef::Character(self.characters.len() - 1))
    }

    fn model_drop(&mut self, model: i64, cells: Fixed) {
        // Resolve the public model id to its actor / character slot, then
        // store the offset in world units (down = +z). No-op for a static
        // sprite model or a bad id.
        #[allow(clippy::cast_possible_truncation)]
        let offset = (cells.to_f64() * SCALE) as f32;
        match usize::try_from(model)
            .ok()
            .and_then(|m| self.model_refs.get(m))
        {
            Some(&ModelRef::Actor(ai)) => {
                if let Some(actor) = self.actors.get_mut(ai) {
                    actor.drop = offset;
                }
            }
            Some(&ModelRef::Character(ci)) => {
                if let Some(c) = self.characters.get_mut(ci) {
                    c.drop = offset;
                }
            }
            Some(&ModelRef::Sprite(_)) | None => {}
        }
    }

    fn entity_set_model(&mut self, entity: i64, model: i64) {
        let e = EntityId(entity as u64);
        let id = model as usize;
        self.models.insert(e, id);
        // Binding an animated model sets up the per-entity state (initial
        // animation = the model's first state / clip).
        match self.model_refs.get(id) {
            Some(&ModelRef::Actor(ai)) => {
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
                        // Set for real when the actor is first created, from
                        // whatever `set_sprite_facing` says then.
                        applied_mode: BillboardMode::None,
                    },
                );
            }
            Some(&ModelRef::Character(ci)) => {
                self.entity_chars.insert(
                    e,
                    CharInst {
                        model: ci,
                        id: None,
                        clip: 0,
                        applied_clip: None,
                        facing: 0.0,
                    },
                );
            }
            Some(&ModelRef::Sprite(_)) | None => {}
        }
    }

    fn entity_set_grid(&mut self, entity: i64, grid: i64) {
        let e = EntityId(entity as u64);
        let before = self.vision_grid();
        if grid < 0 {
            // Unbind (`-1`, the same "no target" convention `vision_observer`
            // uses): the entity returns to the global frame — a crew member who
            // steps off the hull, a prop released from a platform.
            self.entity_grid.remove(&e);
        } else if let Some(id) = self.grid_id(grid) {
            // Resolve the script's grid handle (index into `grids`) to a GridId;
            // an out-of-range handle is ignored, matching `voxel_fill_in`.
            self.entity_grid.insert(e, id);
        } else {
            return;
        }
        // Binding the OBSERVER moves the fog: `vision_grid` derives from this map,
        // so re-arm the (grid-local) mask on the new hull.
        if self.vision_grid() != before {
            self.retarget_vision(before);
        }
    }

    fn grid_orient(&mut self, grid: i64, axis: FixedVec3, angle: Fixed) {
        // Resolve the handle as `voxel_fill_in`/`entity_set_grid` do.
        let Some(id) = self.grid_id(grid) else {
            return;
        };
        // Build a full 3D rotation from axis + angle. The axis arrives in SIM
        // coordinates — the frame the script thinks in, where +z is up — but the
        // grid transform rotates world space, so map the axis the way `world_of`
        // maps a direction: its linear part is `diag(-SCALE, SCALE, -1)`, i.e.
        // sim +x → world −x and sim +z (up) → world −z (down). Up to scale that
        // is `diag(-1, 1, -1)`, itself a 180° turn about y and therefore a PROPER
        // rotation (det +1) — so conjugating by it carries a rotation to a
        // rotation, mapping the axis to `(−x, y, −z)` and leaving the angle
        // alone. Without this a "quarter turn about sim +z" renders as a turn the
        // other way. NB the x/y and z scales differ on a column map (SCALE vs 1),
        // so only the yaw part is scale-exact there; a TILTED axis is honest as a
        // world rotation about the mapped axis, which is what the fog twin, the
        // crew seats and `grid_facing_yaw` all compose against. Volume maps scale
        // z by SCALE too and are exact for any axis.
        //
        // glam requires a unit axis, so normalise here and drop a zero-length one
        // (leaves the pose unchanged). The whole pose is replaced each call, so
        // the script drives orientation from hashed sim state and never
        // accumulates float drift render-side. The turn is about the grid's
        // `grid_pivot` point — its local origin unless the map named one.
        let a = DVec3::new(-axis.x.to_f64(), axis.y.to_f64(), -axis.z.to_f64());
        let Some(unit) = a.try_normalize() else {
            return;
        };
        self.apply_grid_pose(id, DQuat::from_axis_angle(unit, angle.to_f64()));
    }

    fn entity_set_anim(&mut self, entity: i64, state: &str) {
        let e = EntityId(entity as u64);
        if let Some(&ActorInst { model, .. }) = self.entity_actors.get(&e) {
            // Reuse the model's interned `'static` name so the renderer state
            // and the change-detection compare cheaply.
            let interned = self
                .actors
                .get(model)
                .and_then(|a| a.states.iter().find(|(n, _)| *n == state))
                .map(|(n, _)| *n);
            if let (Some(name), Some(inst)) = (interned, self.entity_actors.get_mut(&e)) {
                inst.anim = name;
            }
            return;
        }
        // A character's states are its `.rkc` clip names.
        let Some(&CharInst { model, .. }) = self.entity_chars.get(&e) else {
            return;
        };
        match self.characters.get(model).and_then(|c| c.clips.get(state)) {
            Some(&clip) => {
                if let Some(inst) = self.entity_chars.get_mut(&e) {
                    inst.clip = clip;
                }
            }
            // Unknown name: keep playing whatever is on, and say so ONCE —
            // a per-frame state machine would otherwise flood the log.
            None => {
                if let Some(c) = self.characters.get_mut(model) {
                    if c.warned.insert(state.to_string()) {
                        let have: Vec<&str> = c.clips.keys().map(String::as_str).collect();
                        eprintln!(
                            "monada-host: entity_set_anim: character has no clip {state:?} \
                             (clips: {have:?})"
                        );
                    }
                }
            }
        }
    }

    fn entity_set_facing(&mut self, entity: i64, yaw: Fixed) {
        let e = EntityId(entity as u64);
        if let Some(inst) = self.entity_actors.get_mut(&e) {
            inst.facing = yaw.to_f64();
        }
        if let Some(inst) = self.entity_chars.get_mut(&e) {
            inst.facing = yaw.to_f64();
        }
        // A plain KV6 model turns its GEOMETRY (decision L4 of
        // docs/plans/desert-game.md): a tank hull is a voxel solid, not a
        // card, so there is no pre-drawn side to pick — the model itself
        // yaws in the world. Recorded for every entity, because whether
        // the binding is a sprite, an actor or a character is not settled
        // until `build_instances` walks it.
        self.entity_yaw.insert(e, yaw.to_f64());
    }

    fn entity_set_side(&mut self, entity: i64, dir: i64, roll: i64) {
        let (Some(dir), Some(roll)) = (direction_from_i64(dir), roll_from_i64(roll)) else {
            return;
        };
        self.entity_orient
            .insert(EntityId(entity as u64), side_quat(dir, roll));
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
        self.spawn_grid(wx, wy, wz, false)
    }

    fn grid_spawn_cubic(&mut self, wx: i64, wy: i64, wz: i64) -> i64 {
        self.spawn_grid(wx, wy, wz, true)
    }

    fn grid_move(&mut self, grid: i64, origin: FixedVec3) {
        let Some(id) = self.grid_id(grid) else {
            return;
        };
        let Some(anchor) = self.grid_anchors.get_mut(&id) else {
            return;
        };
        // The origin TRANSLATES the grid's frame — it is not a point inside it —
        // so it takes `world_of`'s linear part only (no cell-centre half-step),
        // scaled in z by the grid's own cell height exactly as `spawn_grid`
        // scales its integer offset. `grid_move(grid_spawn's cells)` therefore
        // lands the grid back where it spawned.
        let cell_z = cell_z_voxels(anchor.cubic) as f64;
        anchor.spawn_origin = DVec3::new(
            -origin.x.to_f64() * SCALE,
            origin.y.to_f64() * SCALE,
            -origin.z.to_f64() * cell_z,
        );
        // Re-derive the pose through the single writer, so a move composes with
        // whatever rotation and pivot the grid already carries.
        let rotation = self
            .scene
            .grid(id)
            .map_or(DQuat::IDENTITY, |g| g.transform.rotation);
        self.apply_grid_pose(id, rotation);
    }

    fn grid_body(&mut self, grid: i64, body: i64) {
        // Drop any previous claim on this grid, then record the new one. The
        // table is keyed by BODY because its one reader — `sync_physics` —
        // walks bodies and asks "is this one already drawn as somebody's
        // hull?"; a body with no entry keeps the automatic mirror it has had
        // since the digger.
        let id = self.grid_id(grid);
        self.grid_bodies.retain(|_, &mut g| Some(g) != id);
        if let (Some(id), Ok(body)) = (id, u64::try_from(body)) {
            self.grid_bodies.insert(body, id);
            // A body that WAS auto-mirrored keeps a grid full of voxels
            // hanging in the scene; retire it now rather than leaving a
            // material-coloured ghost of the hull inside the hull.
            self.retire_mirror(body);
        }
    }

    fn grid_pose(&mut self, grid: i64, origin: FixedVec3, rot: FixedQuat) {
        let Some(id) = self.grid_id(grid) else {
            return;
        };
        let Some(anchor) = self.grid_anchors.get_mut(&id) else {
            return;
        };
        // The origin translates the frame, exactly as `grid_move`'s does —
        // `world_of`'s linear part, z by the grid's own cell height.
        let cell_z = cell_z_voxels(anchor.cubic) as f64;
        anchor.spawn_origin = DVec3::new(
            -origin.x.to_f64() * SCALE,
            origin.y.to_f64() * SCALE,
            -origin.z.to_f64() * cell_z,
        );
        // The rotation is CONJUGATED into world space, not composed with the
        // mirror: this grid's voxels were painted through `world_of` already
        // (`voxel_fill_in`), so its local frame is world-oriented, and a sim
        // rotation becomes `M q M⁻¹` with `M = R_y(π)` — precisely what
        // `grid_orient` does to an axis (map by `diag(-1, 1, -1)`, keep the
        // angle). Composing instead — which is right for a BODY MIRROR grid,
        // whose voxels are still in shape coordinates (`body_grid_pose`) —
        // would leave the hull correct only at identity.
        let m = mirror_half_turn();
        self.apply_grid_pose(id, m * dquat(rot) * m.inverse());
    }

    fn grid_despawn(&mut self, grid: i64) {
        let Some(id) = self.grid_id(grid) else {
            return; // unknown or already retired — inert, by design
        };
        // Order matters: tear the fog down while the grid still EXISTS, then
        // remove it. `retarget_vision` un-clips the grid being left behind and
        // detaches the twin, both of which reach into the scene for it.
        let before = self.vision_grid();
        self.entity_grid.retain(|_, &mut g| g != id);
        if self.named_vision_grid == Some(id) {
            self.named_vision_grid = None;
        }
        if self.vision_grid() != before {
            self.retarget_vision(before);
        }
        self.scene.remove_grid(id);
        self.grid_anchors.remove(&id);
        // Tombstone the handle rather than compacting the table: later handles
        // keep their values, and this one stays inert forever.
        if let Ok(i) = usize::try_from(grid) {
            if let Some(slot) = self.grids.get_mut(i) {
                *slot = None;
            }
        }
    }

    fn camera_grid(&mut self, grid: i64) {
        // `-1` (or any dead handle) puts the camera back in the world frame.
        self.camera_grid = self.grid_id(grid);
    }

    fn voxel_set_in(&mut self, grid: i64, x: i64, y: i64, z: i64, color: i64) {
        self.voxel_fill_in(grid, x, y, z, x, y, z, color);
    }

    fn voxel_clear_in(&mut self, grid: i64, x: i64, y: i64, z: i64) {
        let Some(id) = self.grid_id(grid) else {
            return;
        };
        // One cell back to air — the `voxel_fill_in` inverse, through the same
        // cell-shape branch so a cubic grid clears the whole cube, not the
        // column convention's single voxel row out of the middle of it.
        let (lo, hi) = if self.grid_is_cubic(id) {
            cell_box_to_cubic(x, y, z, x, y, z)
        } else {
            sim_box_to_world(x, y, z, x, y, z)
        };
        if let Some(g) = self.scene.grid_mut(id) {
            g.set_rect(lo, hi, None);
        }
    }

    fn grid_pivot(&mut self, grid: i64, point: FixedVec3) {
        let Some(id) = self.grid_id(grid) else {
            return;
        };
        // `point` is a grid-local SIM cell, the frame `voxel_fill_in` paints in,
        // so map it the same way those voxels were placed: `world_of` on a
        // column-cell grid (z unscaled), the cubic frame's scaled z on a
        // `grid_spawn_cubic` one. (Not `entity_world_of`: the pivot is a point
        // on the HULL, so it follows the GRID's convention, never the map's.)
        let Some(anchor) = self.grid_anchors.get_mut(&id) else {
            return;
        };
        anchor.pivot = entity_world_of_in(anchor.cubic, point);
        // Re-derive the origin so a pivot named after the grid is already
        // turned lands the same as one named before it.
        let rotation = self
            .scene
            .grid(id)
            .map_or(DQuat::IDENTITY, |g| g.transform.rotation);
        self.apply_grid_pose(id, rotation);
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
        // coordinate transform as voxel_fill (world X mirrored), with the cell's
        // z height taken from the grid: a cube on a `grid_spawn_cubic` grid, the
        // column convention's single voxel row otherwise.
        let Some(id) = self.grid_id(grid) else {
            return;
        };
        let (lo, hi) = if self.grid_is_cubic(id) {
            cell_box_to_cubic(x0, y0, z0, x1, y1, z1)
        } else {
            sim_box_to_world(x0, y0, z0, x1, y1, z1)
        };
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
                let room = self.puffs.len() < MAX_PUFFS;
                if let Some(grid) = self.scene.grid_mut(id) {
                    if let (true, Some(color)) = (room, grid.voxel_color(lo)) {
                        self.puffs.push(Puff {
                            cell: (x, y, z),
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

    fn voxel_slide(&mut self, from: (i64, i64, i64), to: (i64, i64, i64)) {
        // Volume maps only: loose material is a volume-store idea, and a
        // column heightmap has nowhere to put a grain that moved sideways.
        if !self.volume {
            return;
        }
        let Some(id) = self.world_grid else {
            return;
        };
        let (flo, fhi) = cell_box_to_volume_grid(from.0, from.1, from.2, from.0, from.1, from.2);
        let (tlo, thi) = cell_box_to_volume_grid(to.0, to.1, to.2, to.0, to.1, to.2);
        let Some(grid) = self.scene.grid_mut(id) else {
            return;
        };
        // The colour travels with the cell — the automaton moved sand, not
        // "a cell", and reading the source is the only way the render side
        // can know which. No debris puff: a slump is not a carve.
        let Some(color) = grid.voxel_color(flo) else {
            return;
        };
        grid.set_rect(flo, fhi, None);
        grid.set_rect(tlo, thi, Some(color));
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
                        // One span per sub-column rather than one call per
                        // voxel: the colour is constant down the column and
                        // `set_rect` decomposes per chunk.
                        grid.set_rect(
                            IVec3::new(wx, wy, (g - z0.max(z1)) as i32),
                            IVec3::new(wx, wy, (g - z0.min(z1)) as i32),
                            Some(VoxColor(color)),
                        );
                    }
                }
            }
        }
    }

    fn cell_voxels(&self) -> i64 {
        SCALE as i64
    }

    fn tile_relief(
        &mut self,
        x: i64,
        y: i64,
        floor: i64,
        walkable: i64,
        tops: &[i64],
        tile: i64,
    ) {
        self.tile_relief_mixed(x, y, floor, walkable, tops, &[tile]);
    }

    fn tile_relief_mixed(
        &mut self,
        x: i64,
        y: i64,
        floor: i64,
        walkable: i64,
        tops: &[i64],
        tiles: &[i64],
    ) {
        // **The cell's column is this call's to own.** Painting a relief
        // over whatever was there leaves every voxel the new surface does
        // not reach exactly as it was -- the old shape, in the old colour,
        // standing through the new one. Re-sculpting a hill and then
        // repainting it showed the leftovers, and they read as the paint
        // not having landed.
        //
        // The span cleared is the tallest of what the store says was here,
        // what the map says is here now, and what this relief actually
        // reaches: sub-column tops lean toward taller neighbours, so they
        // can stand above the cell's own walkable height.
        let ceiling = tops
            .iter()
            .copied()
            .fold(walkable.max(self.terrain.ground_height(x, y)), i64::max);
        if ceiling >= floor {
            let world = self.world_grid();
            let (lo, hi) = sim_box_to_world(x, y, floor, x, y, ceiling);
            if let Some(grid) = self.scene.grid_mut(world) {
                grid.set_rect(lo, hi, None);
            }
        }
        // The feet get one height per cell, as they always have: the
        // heightfield store is what `ground_height` and `nav_path` read,
        // and a pathfinder over sub-columns is a different engine.
        self.terrain.clear_above(x, y, floor);
        self.terrain.fill(x, y, floor, x, y, walkable);
        // Every tile this cell names, resolved once. A border cell asks
        // for two or three of them and a plain one for a single tile, so
        // the lookup is per cell rather than per sub-column.
        let palette: Vec<Option<Vec<u32>>> = tiles
            .iter()
            .map(|&t| usize::try_from(t).ok().and_then(|i| self.tiles.get(i).cloned()))
            .collect();
        if palette.iter().all(Option::is_none) {
            return;
        }
        let span = SCALE as i64;
        let base = GROUND_Z as i64;
        let world = self.world_grid();
        let Some(grid) = self.scene.grid_mut(world) else {
            return;
        };
        // One batch for the cell, not a call per sub-column. A sub-column
        // is a single colour by construction, so each is one span — but
        // every `set_rect` opens its own edit context, and opening one
        // allocates and zeroes a row cache as wide as the chunk. Two
        // hundred and fifty-six of those per cell was the whole of a map's
        // load: twenty-five seconds on a 128-cell map, against half of one
        // through `set_columns`.
        let mut batch: Vec<ColumnSpan> = Vec::with_capacity((span * span) as usize);
        for ly in 0..span {
            for lx in 0..span {
                let slot = (ly * span + lx) as usize;
                let Some(&top) = tops.get(slot) else {
                    continue; // a short slice leaves the rest unpainted
                };
                // A one-entry slice is a plain cell; a short one falls
                // back to its first rather than leaving holes.
                let Some(Some(cells)) = palette.get(slot).or_else(|| palette.first()) else {
                    continue;
                };
                // World X is mirrored (see `world_of`); tile column `lx`
                // maps across the cell'span mirrored X span, row `ly` along Y.
                batch.push(ColumnSpan {
                    x: (-x * span - 1 - lx) as i32,
                    y: (y * span + ly) as i32,
                    z0: (base - top) as i32,
                    z1: (base - floor) as i32,
                    color: VoxColor(cells[slot]),
                });
            }
        }
        grid.set_columns(&batch);
    }

    fn bake_ao(&mut self, strength: i64, radius: i64) {
        let world = self.world_grid();
        let Some(grid) = self.scene.grid_mut(world) else {
            return;
        };
        #[allow(clippy::cast_possible_truncation)]
        let params = roxlap_core::AoParams {
            strength: strength.clamp(0, 100) as f32 / 100.0,
            radius: radius.clamp(1, 8) as i32,
            ..roxlap_core::AoParams::default()
        };
        grid.bake(roxlap_scene::BakeMode::AmbientOcclusion(params));
    }

    fn bake_ao_in(&mut self, lo: (i64, i64), hi: (i64, i64), strength: i64, radius: i64) {
        let world = self.world_grid();
        let Some(grid) = self.scene.grid_mut(world) else {
            return;
        };
        #[allow(clippy::cast_possible_truncation)]
        let params = roxlap_core::AoParams {
            strength: strength.clamp(0, 100) as f32 / 100.0,
            radius: radius.clamp(1, 8) as i32,
            ..roxlap_core::AoParams::default()
        };
        // The cells' whole world span in z, not a slice of it: a bake
        // reads the shape around a voxel, so relighting only part of a
        // hill would shade it against air that is not there. `BAKE_DEPTH`
        // is a column map's whole range with room to spare.
        let (blo, bhi) = sim_box_to_world(lo.0, lo.1, -BAKE_DEPTH, hi.0, hi.1, BAKE_DEPTH);
        grid.bake_bbox(blo, bhi, roxlap_scene::BakeMode::AmbientOcclusion(params));
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

    fn ui_texture_size(&mut self, tex: i64) -> (i64, i64) {
        usize::try_from(tex)
            .ok()
            .and_then(|id| self.ui_textures.get(id))
            .map_or((0, 0), |&(_, w, h)| (i64::from(w), i64::from(h)))
    }

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
        self.ui_stamps.push(0);
        (self.ui_textures.len() - 1) as i64
    }

    fn ui_canvas(&mut self, w: i64, h: i64) -> i64 {
        let (Ok(w), Ok(h)) = (u32::try_from(w), u32::try_from(h)) else {
            return -1;
        };
        if w == 0 || h == 0 {
            return -1;
        }
        // Transparent until the map paints it: a canvas nobody has filled
        // in yet should show nothing rather than a black square.
        let pixels = vec![0u8; (w as usize) * (h as usize) * 4];
        self.ui_textures.push((pixels, w, h));
        self.ui_stamps.push(0);
        (self.ui_textures.len() - 1) as i64
    }

    fn ui_read(&mut self, tex: i64, pixels: &mut [u32]) -> i64 {
        let Ok(tex) = usize::try_from(tex) else {
            return 0;
        };
        let Some((from, w, h)) = self.ui_textures.get(tex) else {
            return 0;
        };
        let room = (*w as usize) * (*h as usize);
        let mut wrote = 0;
        for (i, out) in pixels.iter_mut().take(room).enumerate() {
            // RGBA8 in, `0xAARRGGBB` out -- the packing the rest of the
            // HUD surface speaks, so a map reads back what it would have
            // written.
            *out = (u32::from(from[i * 4 + 3]) << 24)
                | (u32::from(from[i * 4]) << 16)
                | (u32::from(from[i * 4 + 1]) << 8)
                | u32::from(from[i * 4 + 2]);
            wrote += 1;
        }
        wrote
    }

    fn ui_paint(&mut self, tex: i64, pixels: &[u32]) {
        let Ok(tex) = usize::try_from(tex) else {
            return;
        };
        let Some((into, w, h)) = self.ui_textures.get_mut(tex) else {
            return;
        };
        // As many as both sides agree on: a caller that has miscounted
        // draws a wrong picture rather than losing the frame.
        let room = (*w as usize) * (*h as usize);
        for (i, &argb) in pixels.iter().take(room).enumerate() {
            // `0xAARRGGBB` in, RGBA8 out -- the packing every other colour
            // on the HUD surface uses, unpacked once here.
            into[i * 4] = (argb >> 16) as u8;
            into[i * 4 + 1] = (argb >> 8) as u8;
            into[i * 4 + 2] = argb as u8;
            into[i * 4 + 3] = (argb >> 24) as u8;
        }
        if let Some(stamp) = self.ui_stamps.get_mut(tex) {
            *stamp = stamp.wrapping_add(1);
        }
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
            self.stack(UiWidget::Anim {
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
    fn ui_cursor(&self) -> Option<(i64, i64)> {
        self.ui_cursor
    }
    fn ui_height(&self) -> i64 {
        self.ui_viewport.1
    }

    fn ui_scale(&mut self, factor: Fixed) {
        self.ui_scale = (factor.to_f64() as f32).max(0.1);
    }

    fn ui_clear(&mut self) {
        self.ui_widgets.clear();
        self.ui_pin = None;
    }

    fn ui_pin(&mut self, at: FixedVec3) {
        // **Stored in world space, not sim space.** The map speaks cells and
        // the projection speaks voxels -- and world X is mirrored besides --
        // so a sim position handed to the renderer lands somewhere else
        // entirely. Converted here, where the map's z convention is known,
        // rather than at paint time where it is not.
        let w = entity_world_of_in(self.volume, at);
        self.ui_pin = Some([w.x as f32, w.y as f32, w.z as f32]);
    }

    fn ui_image(&mut self, tex: i64, x: i64, y: i64) {
        if let Ok(tex) = usize::try_from(tex) {
            self.stack(UiWidget::Image {
                tex,
                x: x as i32,
                y: y as i32,
                scale: self.ui_scale,
                tint: WHITE_MARK,
                turn: 0.0,
            });
        }
    }

    fn ui_mark(&mut self, tex: i64, x: i64, y: i64, tint: i64, turn: Fixed) {
        if let Ok(tex) = usize::try_from(tex) {
            self.stack(UiWidget::Image {
                tex,
                x: x as i32,
                y: y as i32,
                scale: self.ui_scale,
                // Anything that does not fit a colour word is drawn as it
                // was authored, which is what a caller that passed
                // rubbish most likely wanted to see.
                tint: u32::try_from(tint).unwrap_or(WHITE_MARK),
                turn: turn.to_f32(),
            });
        }
    }

    fn ui_image_clip(&mut self, tex: i64, x: i64, y: i64, frac: Fixed) {
        if let Ok(tex) = usize::try_from(tex) {
            let frac = frac.to_f64().clamp(0.0, 1.0) as f32;
            self.stack(UiWidget::ImageClip {
                tex,
                x: x as i32,
                y: y as i32,
                frac,
                scale: self.ui_scale,
            });
        }
    }

    fn ui_text(&mut self, x: i64, y: i64, text: &str, size: i64) {
        self.stack(UiWidget::Text {
            x: x as i32,
            y: y as i32,
            text: text.to_string(),
            size: size.max(1) as f32,
            scale: self.ui_scale,
            tint: WHITE_MARK,
        });
    }

    fn ui_text_tint(&mut self, x: i64, y: i64, text: &str, size: i64, tint: i64) {
        self.stack(UiWidget::Text {
            x: x as i32,
            y: y as i32,
            text: text.to_string(),
            size: size.max(1) as f32,
            scale: self.ui_scale,
            // Anything that does not fit a colour word is drawn plain,
            // which is what a caller that passed rubbish wanted to see.
            tint: u32::try_from(tint).unwrap_or(WHITE_MARK),
        });
    }

    fn ui_text_wrap(&mut self, x: i64, y: i64, text: &str, size: i64, width: i64, color: i64) {
        self.stack(UiWidget::TextWrap {
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
        self.stack(UiWidget::Button {
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
        // A world point stops any follow: the two are the same knob, and the
        // last caller wins.
        self.camera_follow = None;
        self.camera.center = self.entity_world_of(point);
    }

    fn camera_focus_entity(&mut self, entity: i64, point: FixedVec3) {
        // Compose through the grid the entity rides (the same seat
        // `build_instances` renders it at), so following a crew member on a
        // rotating hull tracks it instead of aiming at the un-transformed cell.
        //
        // Composed twice, on purpose: the stored centre keeps `camera_pan`,
        // `camera_center_sim` and the cursor path tick-exact, while
        // [`camera`](Self::camera) re-composes the same pair each frame against
        // the pose actually on screen (§4.4 of the ship-physics plan).
        let e = EntityId(entity as u64);
        self.camera_follow = Some((e, point));
        self.camera.center = self.place(e, point);
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
        // Just remember the SIM band: it drives both the cutaway and the fog's
        // `DeckBand`, and the grid each lands on is derived later
        // (`vision_grid`), so the `z_clip` threshold is resolved at apply time
        // by `deck_clip_z` — in whichever cell shape that grid uses. roxlap's
        // `Grid::z_clip` cuts one side (voxels with grid-z BELOW the threshold —
        // smaller grid-z = higher up, z-down — read as air), so the threshold
        // sits at the band top and everything above it goes; a band whose top is
        // the world's tallest voxel cuts nothing.
        self.deck_band = Some((z_lo, z_hi));
    }

    fn vision_observer(&mut self, entity: i64) {
        // Legacy 1-arg form: fog rides the world grid.
        self.set_observer(entity, None);
    }

    fn vision_observer_in(&mut self, entity: i64, grid: i64) {
        // Fog rides the named `grid_spawn` grid (the crew's hull); an out-of-range
        // handle leaves it on the world grid rather than blindly picking one.
        let g = self.grid_id(grid);
        self.set_observer(entity, g);
    }

    fn vision_observer_add(&mut self, entity: i64) {
        if entity < 0 {
            return;
        }
        let e = EntityId(entity as u64);
        if self.vision_entities.contains(&e) {
            return; // idempotent: adding twice is one observer
        }
        let mut all = std::mem::take(&mut self.vision_entities);
        all.push(e);
        self.set_observers(all, self.named_vision_grid);
    }

    fn vision_observer_clear(&mut self) {
        self.set_observers(Vec::new(), self.named_vision_grid);
    }

    fn vision_shroud(&mut self, opaque: bool) {
        if opaque != self.shroud_unseen {
            self.shroud_unseen = opaque;
            self.drop_fow(); // it rides the config, which is built once
        }
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

    fn body_deco_box(
        &mut self,
        body: i64,
        x0: i64,
        y0: i64,
        z0: i64,
        x1: i64,
        y1: i64,
        z1: i64,
        color: i64,
    ) {
        let lo = IVec3::new(x0.min(x1) as i32, y0.min(y1) as i32, z0.min(z1) as i32);
        let hi = IVec3::new(x0.max(x1) as i32, y0.max(y1) as i32, z0.max(z1) as i32);
        self.body_decos
            .entry(body as u64)
            .or_default()
            .push((lo, hi, color as u32));
    }

    fn drill_indicator(&mut self, body: i64, pitch: Fixed, spinning: bool) {
        self.drill_vis
            .insert(body as u64, (pitch.to_f64(), spinning));
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

    fn pick_grid(&self) -> i64 {
        self.cursor_hit()
            .map_or(-1, |hit| self.grid_handle(hit.grid))
    }

    fn pick_cell(&self, grid: i64) -> Option<FixedVec3> {
        let hit = self.cursor_hit()?;
        let want = self.addressed_grid(grid)?;
        // A map asks about the hull it cares about: a hit on some other
        // grid is a miss, not a cell in the wrong frame.
        (hit.grid == want).then(|| MapRender::cell_vec(self.cell_of_voxel(want, hit.voxel)))
    }

    fn pick_face(&self, grid: i64) -> Option<FixedVec3> {
        let (_, dir) = self.cursor_ray?;
        let hit = self.cursor_hit()?;
        let want = self.addressed_grid(grid)?;
        if hit.grid != want {
            return None;
        }
        let g = self.scene.grid(want)?;
        let len = dir.length();
        if len < 1e-9 {
            return None;
        }
        let dirn = dir / len;
        // Step back half a voxel along the ray: out of the solid cell it
        // hit, into the air the surface faces. The same nudge
        // `Scene::resolve_voxel` uses to get *in*, run the other way.
        let out = hit.world - dirn * (0.5 * g.transform.voxel_world_size);
        let glp = addr::world_to_grid_local(out, &g.transform);
        let d = addr::voxel_global(glp.chunk, glp.voxel) - hit.voxel;
        // One axis, even if a corner graze moved two: a face is one face.
        // A degenerate step (the ray nearly parallel to the surface)
        // falls back to the axis the ray came down, which is the face it
        // must have crossed.
        let local_dir = g.transform.rotation.inverse() * dirn;
        let (dx, dy, dz) = (d.x, d.y, d.z);
        let axis = if dx == 0 && dy == 0 && dz == 0 {
            let (ax, ay, az) = (local_dir.x.abs(), local_dir.y.abs(), local_dir.z.abs());
            if ax >= ay && ax >= az {
                (-local_dir.x.signum() as i32, 0, 0)
            } else if ay >= az {
                (0, -local_dir.y.signum() as i32, 0)
            } else {
                (0, 0, -local_dir.z.signum() as i32)
            }
        } else if dx.abs() >= dy.abs() && dx.abs() >= dz.abs() {
            (dx.signum(), 0, 0)
        } else if dy.abs() >= dz.abs() {
            (0, dy.signum(), 0)
        } else {
            (0, 0, dz.signum())
        };
        // Voxel axes → SIM axes: world X is mirrored and world z grows
        // down, so a normal along voxel +x points at sim −x, and the top
        // face of a floor (voxel −z) is sim +z, which is up.
        Some(FixedVec3::new(
            Fixed::from_int(-axis.0),
            Fixed::from_int(axis.1),
            Fixed::from_int(-axis.2),
        ))
    }

    fn gizmo_clear(&mut self) {
        self.gizmos.clear();
        self.gizmo_style = (2.0, false);
    }

    fn gizmo_style(&mut self, width_px: i64, on_top: bool) {
        self.gizmo_style = (width_px.clamp(1, 32) as f32, on_top);
    }

    fn gizmo_box(
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
        let c = self.cell_box_corners(grid, (x0, y0, z0), (x1, y1, z1));
        let (width_px, on_top) = self.gizmo_style;
        for (a, b) in BOX_EDGES {
            self.gizmos.push(Line3 {
                a: c[a].to_array(),
                b: c[b].to_array(),
                color: OverlayColor(color as u32),
                width_px,
                depth_test: !on_top,
            });
        }
    }

    fn gizmo_line(&mut self, grid: i64, a: FixedVec3, b: FixedVec3, color: i64) {
        let (width_px, on_top) = self.gizmo_style;
        self.gizmos.push(Line3 {
            a: self.gizmo_point(grid, a).to_array(),
            b: self.gizmo_point(grid, b).to_array(),
            color: OverlayColor(color as u32),
            width_px,
            depth_test: !on_top,
        });
    }

    fn ghost_clear(&mut self) {
        self.ghosts.targets.clear();
        self.ghosts.actor = None;
    }

    fn entity_set_lift(&mut self, entity: i64, cells: Fixed) {
        let Ok(id) = u64::try_from(entity) else {
            return;
        };
        let lift = cells.to_f64() * SCALE;
        // Zero is the common case and the default, so it is stored as an
        // absence: a map that lifts one body should not grow a map entry
        // per entity that never leaves the ground.
        if lift == 0.0 {
            self.entity_lift.remove(&EntityId(id));
        } else {
            self.entity_lift.insert(EntityId(id), lift);
        }
    }

    fn entity_drawn(&mut self, entity: i64) -> Option<FixedVec3> {
        let id = EntityId(u64::try_from(entity).ok()?);
        let step = self.tick_dt?;
        Some(self.entity_tracks.get(&id)?.drawn(step))
    }

    fn point_visible(&mut self, at: FixedVec3) -> bool {
        // No fog, no mask yet, or a point off the fog grid: open ground.
        // The same three answers the renderer's own sprite cull gives, and
        // deliberately so -- an overlay that disagreed with the body it is
        // about would hang in the dark or vanish over a lit one.
        let (Some(fow), Some(eye)) = (self.fow.as_ref(), self.vision_grid()) else {
            return true;
        };
        let Some(grid) = self.scene.grid(eye) else {
            return true;
        };
        let Some(footprint) = grid.footprint_cells() else {
            return true;
        };
        !fow.hides_sprite(
            &grid.transform,
            footprint,
            entity_world_of_in(self.volume, at),
        )
    }

    fn ghost_model(&mut self, model: i64, pos: FixedVec3, yaw: Fixed, alpha: i64) {
        let Some(&kind) = usize::try_from(model)
            .ok()
            .and_then(|i| self.model_refs.get(i))
        else {
            return;
        };
        let alpha = u8::try_from(alpha.clamp(0, 255)).unwrap_or(255);
        // Seated exactly as the real thing is, or the preview lies about
        // the one thing it is for. The two kinds are seated differently
        // because they are anchored differently, and both follow what
        // `build_instances` does for a live entity.
        let w = entity_world_of_in(self.volume, pos);
        match kind {
            ModelRef::Sprite(si) => {
                // `drop` puts the model's feet on the point rather than its
                // pivot, and the world yaw mirrors the sim one because
                // `world_of` mirrors x.
                let drop = self.sprites.models.get(si).map_or(SCALE * 0.5, |m| {
                    f64::from(m.kv6.zsiz) - f64::from(m.kv6.zpiv)
                });
                let seat = DVec3::new(w.x, w.y, w.z - drop);
                self.ghosts
                    .targets
                    .push((si, seat, facing_to_world_yaw(yaw.to_f64()), alpha));
            }
            ModelRef::Actor(ai) => {
                // A card's pivot is already its bottom centre, so only the
                // art's own `model_drop` correction applies (world +z is
                // down). The yaw stays in the entity's frame: which of the
                // eight cards to show is an angle between the viewer and
                // the character's nose, and `update_billboard_actors`
                // measures it there.
                let drop = f64::from(self.actors.get(ai).map_or(0.0, |a| a.drop));
                let seat = DVec3::new(w.x, w.y, w.z + drop);
                self.ghosts.actor = Some((ai, seat, facing_to_world_yaw(yaw.to_f64()), alpha));
            }
            // A rigged character is real geometry driven by a clip:
            // previewing one means posing an animation, and nothing has
            // asked to.
            ModelRef::Character(_) => {}
        }
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
        #[allow(clippy::cast_possible_truncation)]
        {
            // Volume maps light through the dynamic rig (render_into);
            // stored in SIM space, transformed there.
            self.look.sun = Some((travel, intensity.to_f64() as f32));
        }
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

    fn set_sprite_facing(&mut self, view_plane: bool) {
        self.look.sprite_view_plane = view_plane;
    }

    fn camera_fov(&mut self, degrees: Fixed) {
        // Outside this the projection stops being useful rather than
        // merely odd: at 1° the camera has to sit kilometres back, and at
        // 170° the edges of the frame are unreadable smear.
        #[allow(clippy::cast_possible_truncation)]
        let d = degrees.to_f64() as f32;
        self.look.fov_deg = (1.0..=170.0).contains(&d).then_some(d);
    }

    fn set_shadows(&mut self, strength: Fixed) {
        // `0` (and anything below) means "back to per-face shading", so the
        // rig is absent rather than present-but-black.
        #[allow(clippy::cast_possible_truncation)]
        let s = strength.to_f64().clamp(0.0, 1.0) as f32;
        self.look.shadows = (s > 0.0).then_some(s);
    }

    fn set_sky_color(&mut self, color: i64) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            self.look.sky_color = Some(Rgb(color as u32 & 0x00FF_FFFF));
        }
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

    // Collision and navigation are no longer the bridge's to answer: the
    // runtime owns the terrain store the map paints, so a headless peer and
    // a drawing one agree on what is walkable by construction rather than
    // by two bridges implementing the same store
    // (docs/plans/desert-game.md §3a). `self.terrain` survives as this
    // renderer's private height index — what a selection ring or a drag
    // rectangle needs to hug the ground — and is fed by the same paints.
}

#[cfg(test)]
mod tests {
    //! The renderer-free half of the actor bridge (the GPU/clip half needs a
    //! window): GIF decode, per-entity binding, and the actor-target the
    //! frame computes for `update_actors` to apply.
    use super::*;
    use monada_sim::World;

    /// `ui_pin` binds the next widget and only the next one.
    ///
    /// A pin that stayed set would silently drag every later widget in the
    /// frame onto the same world point -- and the HUD is rebuilt from
    /// scratch each frame, so the damage would be invisible in the source
    /// and total on screen. The one-shot take is the whole contract.
    #[test]
    fn a_pin_binds_one_widget_and_lets_the_next_one_go() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let tex = 0;
        r.ui_pin(FixedVec3::new(
            Fixed::from_int(3),
            Fixed::from_int(4),
            Fixed::from_int(5),
        ));
        r.ui_image(tex, 2, 2);
        r.ui_image(tex, 9, 9);

        let placed = r.ui_widgets();
        assert_eq!(placed.len(), 2);
        let over = placed[0].over.expect("the pinned widget carries a point");
        // **In the space the renderer draws in, not the one the map speaks.**
        // Asserted as the mirror and the scale rather than as three numbers:
        // a pin that passed sim cells straight through would land the widget
        // a screen away, and that is the shape of the mistake -- x flips
        // sign, y grows by a cell's worth of voxels, and z counts DOWN from
        // the deck.
        assert!(over[0] < 0.0, "world x is mirrored: {over:?}");
        assert!(over[1] > 4.0, "world y is scaled: {over:?}");
        assert!(over[2] < GROUND_Z as f32, "world z counts down: {over:?}");
        assert_eq!(placed[1].over, None, "and the pin does not outlive it");

        // A pin left dangling by a map that never drew must not survive into
        // the next frame's first widget either.
        r.ui_pin(FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ZERO));
        r.ui_clear();
        r.ui_image(tex, 1, 1);
        assert_eq!(r.ui_widgets()[0].over, None);
    }

    /// Dust is decoration, and decoration gets a ceiling.
    ///
    /// The feature was written for a drill carving a handful of cells a
    /// tick. A terraforming game carves thousands at once — one crater is
    /// three thousand — and each puff is a 7³ sprite instance in the set
    /// that gets uploaded. Unbudgeted, a single blast is a visible stall.
    #[test]
    fn a_blast_cannot_fill_the_air_with_dust() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        r.set_volume_terrain();
        r.voxel_fill(0, 0, 0, 40, 40, 4, 0x8070_5838);
        for y in 0..40 {
            for x in 0..40 {
                r.voxel_clear(x, y, 4);
            }
        }
        assert_eq!(
            r.puffs.len(),
            MAX_PUFFS,
            "sixteen hundred carves put {} puffs in the air",
            r.puffs.len()
        );
        // And they still expire — a cap that leaked would be worse than
        // no cap, because the air would never clear again.
        r.age_puffs(PUFF_TTL + 0.01);
        assert!(r.puffs.is_empty());
    }

    /// Dust must draw on every map, and must never touch the sprite set.
    ///
    /// It used to be sprite instances appended to the STATIC list, which
    /// is rebuilt each frame — but only on a map with no posed sprites.
    /// On a dynamic-layer map that list is uploaded exactly once, so the
    /// dust alive at that instant froze onto the screen for the rest of
    /// the match while every later puff was invisible. A crater's worth
    /// of it hanging over the hole is what playing the desert looked
    /// like. Voxels in an effects grid have no such contract: the scene
    /// is walked every frame.
    #[test]
    fn dust_is_drawn_as_geometry_on_any_map() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        r.set_volume_terrain();
        r.voxel_fill(0, 0, 0, 8, 8, 2, 0x8070_5838);

        // Turn something: the static set is now upload-once, which is the
        // case the old path got wrong.
        r.entity_set_facing(1, Fixed::from_f64(0.5));
        assert!(r.dynamic_layer());

        r.voxel_clear(1, 1, 2);
        let sprites = r.sprites.instances.len();
        r.sync_puffs(0.0);
        assert_eq!(
            r.sprites.instances.len(),
            sprites,
            "dust reached a sprite set that will be frozen"
        );
        assert_eq!(r.fx_painted.len(), 1, "dust was not drawn at all");
        let grid = r.fx_grid.and_then(|g| r.grid_id(g)).expect("effects grid");
        let cell = r.fx_painted[0];
        let (lo, _) = cell_box_to_cubic(cell.0, cell.1, cell.2, cell.0, cell.1, cell.2);
        assert!(
            r.scene.grid(grid).and_then(|g| g.voxel_color(lo)).is_some(),
            "the effects grid has no voxel where the dust is"
        );

        // And it clears itself: a frame after the puff dies, the cell it
        // occupied is empty again.
        r.sync_puffs(PUFF_TTL + 0.01);
        assert!(r.puffs.is_empty(), "the dust never expired");
        assert!(r.fx_painted.is_empty(), "the dust was never rubbed out");
        assert!(
            r.scene.grid(grid).and_then(|g| g.voxel_color(lo)).is_none(),
            "a dead puff left its voxel behind"
        );
    }

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
        assert_eq!(
            r.puffs[0].color, 0x8070_5838,
            "puff wears the voxel's colour"
        );
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

    /// The cursor reads back the cell the map painted — the whole point
    /// of `pick_cell` (`host_api` 24, docs/plans/ship-building.md).
    ///
    /// A ray down a known cell of a cubic grid must answer that cell's
    /// own coordinates, in the numbers `voxel_fill_in` was called with,
    /// and name the top face as up. A map addressing some *other* grid
    /// gets a miss rather than a cell in the wrong frame.
    #[test]
    fn the_cursor_names_the_cell_the_map_painted() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let world = World::new(0);
        let g = r.grid_spawn_cubic(0, 0, 0);
        let other = r.grid_spawn_cubic(0, 0, 0);
        let (cx, cy) = (2_i64, 3_i64);
        r.voxel_fill_in(g, cx, cy, 0, cx, cy, 0, 0x8055_5f6b);

        // Straight down the mirrored cell centre, from above the deck.
        let col = DVec3::new(-(cx as f64 + 0.5) * SCALE, (cy as f64 + 0.5) * SCALE, 0.0);
        r.set_cursor_ray(&world, col, DVec3::new(0.0, 0.0, 1.0));

        assert_eq!(r.pick_grid(), g, "the cursor names the grid it hit");
        assert_eq!(
            r.pick_cell(g),
            Some(FixedVec3::new(
                Fixed::from_int(cx as i32),
                Fixed::from_int(cy as i32),
                Fixed::ZERO
            )),
            "the cell read back is the cell painted"
        );
        assert_eq!(
            r.pick_face(g),
            Some(FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE)),
            "a floor's exposed face points up, in SIM axes"
        );
        assert_eq!(r.pick_cell(other), None, "another grid is a miss");
        assert_eq!(r.pick_cell(-1), None, "so is a world grid nobody painted");

        // Off the deck entirely: no hit, no cell.
        r.set_cursor_ray(&world, DVec3::new(500.0, 500.0, 0.0), DVec3::Z);
        assert_eq!(r.pick_grid(), -1);
        assert_eq!(r.pick_cell(g), None);
    }

    /// The cursor rides the hull. A cell of a grid at some arbitrary
    /// attitude — the ship under thrust — must still read back as itself:
    /// the ray is aimed at where the gizmo path says that cell IS, and
    /// the pick path must agree. The two are inverses, and a placement
    /// ghost is only honest while they are.
    #[test]
    fn the_cursor_and_the_gizmo_agree_on_a_turned_hull() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let world = World::new(0);
        let g = r.grid_spawn_cubic(0, 0, 0);
        let cell = (4_i64, 1_i64, 2_i64);
        r.voxel_fill_in(
            g,
            cell.0,
            cell.1,
            cell.2,
            cell.0,
            cell.1,
            cell.2,
            0x80b8_9850,
        );
        // A tilted tumble, pivoted about the middle of the block — the
        // pose `grid_body` writes every tick on the ship.
        r.grid_pivot(
            g,
            FixedVec3::new(Fixed::from_int(4), Fixed::from_int(1), Fixed::from_int(2)),
        );
        r.grid_orient(
            g,
            FixedVec3::new(
                Fixed::from_ratio(3, 10),
                Fixed::from_ratio(-2, 10),
                Fixed::ONE,
            ),
            Fixed::from_ratio(7, 10),
        );

        // Where that cell now is, per the gizmo path.
        let corners = r.cell_box_corners(g, cell, cell);
        let centre = corners.iter().fold(DVec3::ZERO, |a, c| a + *c) / 8.0;
        assert!(
            (corners[0] - corners[6]).length() > SCALE,
            "a cell of a cubic grid is a cube {SCALE} across, not a point"
        );

        // Aim at it from far away, down an axis nothing is aligned with.
        let dir = DVec3::new(1.0, 2.0, -3.0).normalize();
        r.set_cursor_ray(&world, centre - dir * 400.0, dir);

        assert_eq!(r.pick_grid(), g);
        assert_eq!(
            r.pick_cell(g),
            Some(MapRender::cell_vec(cell)),
            "a cell at attitude reads back as itself"
        );
        let face = r.pick_face(g).expect("a hit has a face");
        let axes = [face.x, face.y, face.z].map(|c| i64::from(c.floor_to_int()).abs());
        assert_eq!(
            axes.iter().sum::<i64>(),
            1,
            "a face is one unit axis: {face:?}"
        );
    }

    /// The deck cutaway moves the cursor with it: pointing at a hull
    /// whose upper deck is cut away lands on the deck the player can
    /// see. Free, because the pick marches the CLIPPED scene — and it is
    /// the whole reason it does.
    #[test]
    fn the_deck_cutaway_moves_the_cursor_down_a_deck() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let world = World::new(0);
        let g = r.grid_spawn_cubic(0, 0, 0);
        let (cx, cy) = (2_i64, 2_i64);
        r.voxel_fill_in(g, cx, cy, 0, cx, cy, 0, 0x8055_5f6b); // lower deck plate
        r.voxel_fill_in(g, cx, cy, 3, cx, cy, 3, 0x8055_5f6b); // upper deck plate
                                                               // The fog observer is what names the grid a deck band clips.
        r.vision_observer_in(-1, g);

        let col = DVec3::new(-(cx as f64 + 0.5) * SCALE, (cy as f64 + 0.5) * SCALE, 0.0);
        let aim = |r: &mut MapRender| {
            r.apply_deck_clip();
            r.set_cursor_ray(&world, col, DVec3::new(0.0, 0.0, 1.0));
            r.pick_cell(g).map(|c| i64::from(c.z.floor_to_int()))
        };

        assert_eq!(aim(&mut r), Some(3), "uncut, the cursor lands on the roof");
        r.deck_clip(0, 2); // stand on the lower deck
        assert_eq!(
            aim(&mut r),
            Some(0),
            "cut away, it lands on the deck being looked into"
        );
    }

    /// A gizmo box is drawn in its grid's frame, in the grid's own cells,
    /// and it is the map that clears it — the `ui_clear` contract, so a
    /// ghost drawn on the tick clock survives the frames between ticks.
    #[test]
    fn a_gizmo_box_rides_its_grid_and_is_the_map_s_to_clear() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        r.gizmo_style(3, true);
        r.gizmo_box(g, 1, 1, 0, 1, 1, 0, 0x9040_ff70);
        assert_eq!(r.gizmos.len(), 12, "a box is twelve edges");
        assert!(
            r.gizmos
                .iter()
                .all(|l| !l.depth_test && (l.width_px - 3.0).abs() < 1e-6),
            "the style applies to the segments drawn after it"
        );

        // Turning the grid carries the outline with it.
        let before = r.gizmos[0].a;
        r.gizmo_clear();
        r.grid_orient(
            g,
            FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE),
            Fixed::from_ratio(5, 10),
        );
        r.gizmo_box(g, 1, 1, 0, 1, 1, 0, 0x9040_ff70);
        let swing: f64 = (0..3).map(|i| (r.gizmos[0].a[i] - before[i]).abs()).sum();
        assert!(swing > 1.0, "the ghost stayed behind on a turn");

        // Nothing clears it but the map — `draw_gizmos` takes `&self`, so
        // a frame *cannot* eat what the map queued on its slower clock.
        r.gizmo_clear();
        assert!(r.gizmos.is_empty(), "gizmo_clear starts a fresh set");
        assert_eq!(r.gizmo_style, (2.0, false), "and resets the style");
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
        assert_eq!(
            hit.grid,
            r.grid_id(g).expect("live grid"),
            "hit the spawned grid"
        );
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
        let gid = r.grid_id(g).expect("live grid");
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
        let gid = r.grid_id(g).expect("live grid");

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

    /// `grid_pivot` names the grid-local point `grid_orient` turns about, so a
    /// hull turns IN PLACE: the pivot cell holds still under any rotation while
    /// the rest of the hull sweeps around it. Without it a grid turns about its
    /// local origin, which for a hull painted up from cell `(0,0,0)` is a corner
    /// (and `GROUND_Z` above the deck), so the whole ship swings through an arc
    /// wider than itself. Proven through `place`, the seat everything renders
    /// from — and the two call orders must land the same pose.
    #[test]
    fn grid_pivot_holds_its_cell_still_under_rotation() {
        let quarter = Fixed::from_f64(std::f64::consts::FRAC_PI_2);
        let sim_z = FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::from_int(1));
        // The hull's middle cell, and a corner cell that must move.
        let mid = FixedVec3::new(Fixed::from_f64(9.5), Fixed::from_f64(9.5), Fixed::ZERO);
        let corner = FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ZERO);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let at_mid = world.spawn(arch);
        let at_corner = world.spawn(arch);

        // Pivot first, then orient.
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn(4, 3, 0);
        r.entity_set_grid(at_mid.0 as i64, g);
        r.entity_set_grid(at_corner.0 as i64, g);
        let seated_mid = r.place(at_mid, mid);
        let seated_corner = r.place(at_corner, corner);
        r.grid_pivot(g, mid);
        r.grid_orient(g, sim_z, quarter);

        assert!(
            (r.place(at_mid, mid) - seated_mid).length() < 1e-9,
            "the pivot cell must not move when the hull turns about it"
        );
        let swung = r.place(at_corner, corner);
        assert!(
            (swung - seated_corner).length() > 1.0,
            "the rest of the hull still sweeps around the pivot"
        );
        // Turning in place: the pivot is the centre of the swing, so a corner
        // keeps its distance from it.
        let radius = |p: DVec3| (p - seated_mid).length();
        assert!(
            (radius(swung) - radius(seated_corner)).abs() < 1e-9,
            "a turn about the pivot preserves every cell's distance to it"
        );

        // Orient first, then pivot — the same pose.
        let mut r2 = MapRender::new(BTreeMap::new(), None, &[]);
        let g2 = r2.grid_spawn(4, 3, 0);
        r2.entity_set_grid(at_corner.0 as i64, g2);
        r2.grid_orient(g2, sim_z, quarter);
        r2.grid_pivot(g2, mid);
        assert!(
            (r2.place(at_corner, corner) - swung).length() < 1e-9,
            "grid_pivot and grid_orient must commute"
        );
    }

    /// A grid nobody pivots keeps turning about its own local origin — the
    /// pre-`grid_pivot` behaviour, so the verb is purely opt-in and a map
    /// written before it is unaffected.
    #[test]
    fn an_unpivoted_grid_still_turns_about_its_local_origin() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn(4, 3, 0);
        let gid = r.grid_id(g).expect("live grid");
        let spawned = r.scene.grid(gid).expect("grid").transform.origin;
        r.grid_orient(
            g,
            FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::from_int(1)),
            Fixed::from_f64(std::f64::consts::FRAC_PI_2),
        );
        assert_eq!(
            r.scene.grid(gid).expect("grid").transform.origin,
            spawned,
            "with no pivot the origin is untouched — the grid turns about it"
        );
    }

    /// `grid_orient`'s axis is in SIM coordinates, so the host must map it
    /// through the same sim→world transform the grid's voxels are painted with
    /// (`world_of`: sim +x → world −x, sim +z up → world −z down). Ask for a
    /// quarter-turn about sim +z and require that it carries the sim +x
    /// DIRECTION onto the sim +y direction — right-handed in the frame the
    /// script thinks in. Feeding the axis through un-mapped (the pre-fix bug)
    /// turns the hull the other way, landing on sim −y.
    #[test]
    fn grid_orient_axis_is_in_sim_coordinates() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn(0, 0, 0);
        let gid = r.grid_id(g).expect("live grid");
        r.grid_orient(
            g,
            FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::from_int(1)),
            Fixed::from_f64(std::f64::consts::FRAC_PI_2),
        );
        let rot = r.scene.grid(gid).expect("grid").transform.rotation;
        // Sim directions in world, per `world_of`'s linear part (x/y scale by
        // SCALE, so use unit directions — only the heading is asserted).
        let sim_x = DVec3::new(-1.0, 0.0, 0.0);
        let sim_y = DVec3::new(0.0, 1.0, 0.0);
        let turned = rot * sim_x;
        assert!(
            (turned - sim_y).length() < 1e-9,
            "a quarter-turn about sim +z carries sim +x onto sim +y, got {turned:?} \
             (mirrored ⇒ the axis was not mapped sim→world)"
        );
    }

    /// A `grid_spawn_cubic` grid paints a cell as a CUBE: `SCALE³` world voxels,
    /// z scaled exactly like x/y, instead of the column convention's
    /// `SCALE×SCALE×1` slab. The cube hangs BELOW its cell's top plane
    /// (`GROUND_Z - z·SCALE`), which is where an entity at that sim z stands —
    /// the column convention's rule, generalised.
    #[test]
    fn cubic_grid_paints_cells_as_cubes() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        assert!(g >= 0, "cubic grid handle allocated");
        r.voxel_fill_in(g, 0, 0, 0, 0, 0, 0, 0x8055_5555);

        let hull = r
            .scene
            .grid(r.grid_id(g).expect("live grid"))
            .expect("grid");
        let gz = GROUND_Z as i32;
        let s = SCALE as i32;
        let solid = |x: i32, y: i32, z: i32| hull.voxel_color(IVec3::new(x, y, z)).is_some();

        // Cell (0,0,0) fills world voxels x ∈ [-SCALE, -1], y ∈ [0, SCALE-1],
        // z ∈ [GROUND_Z, GROUND_Z + SCALE - 1] — a cube, both corners in.
        assert!(solid(-1, 0, gz), "cube's near-top corner");
        assert!(solid(-s, s - 1, gz + s - 1), "cube's far-bottom corner");
        // ...and nothing outside it, on any axis.
        assert!(!solid(0, 0, gz), "x is mirrored: cell 0 stops at -1");
        assert!(!solid(-s - 1, 0, gz), "one voxel past the cube in x");
        assert!(!solid(-1, s, gz), "one voxel past the cube in y");
        assert!(
            !solid(-1, 0, gz - 1),
            "the cell's TOP plane is GROUND_Z: nothing above it"
        );
        assert!(
            !solid(-1, 0, gz + s),
            "the cube is exactly SCALE voxels deep"
        );
    }

    /// An entity bound to a cubic grid seats on the TOP of the very cell it
    /// names — the column convention ("floor painted at z, crew at z"), now with
    /// z scaled like x/y. Proven against the real painted cube, so the entity
    /// map and the voxel map can't drift apart (the S-C verticality bug, which
    /// floated the crew ~60 units off its own deck, was exactly this drift).
    #[test]
    fn cubic_grid_seats_a_bound_entity_on_the_cell_it_names() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        let (cx, cy, cz) = (3_i64, 4_i64, 2_i64);
        r.voxel_fill_in(g, cx, cy, cz, cx, cy, cz, 0x8055_5555);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        let p = FixedVec3::new(
            Fixed::from_int(cx as i32),
            Fixed::from_int(cy as i32),
            Fixed::from_int(cz as i32),
        );
        world.set_position(e, p);
        r.entity_set_grid(e.0 as i64, g);

        let seat = r.place(e, p);
        let want = DVec3::new(
            -(cx as f64 + 0.5) * SCALE,
            (cy as f64 + 0.5) * SCALE,
            GROUND_Z - cz as f64 * SCALE,
        );
        assert!(
            (seat - want).length() < 1e-9,
            "cubic seat at the cell's top-centre, want {want:?}, got {seat:?}"
        );

        // And that plane really is the top of the painted cube.
        let hull = r
            .scene
            .grid(r.grid_id(g).expect("live grid"))
            .expect("grid");
        let column = IVec3::new(-(cx as i32) * SCALE as i32 - 1, cy as i32 * SCALE as i32, 0);
        let top = seat.z as i32;
        assert!(
            hull.voxel_color(IVec3::new(column.x, column.y, top))
                .is_some(),
            "the crew's feet plane is the cube's first solid voxel row"
        );
        assert!(
            hull.voxel_color(IVec3::new(column.x, column.y, top - 1))
                .is_none(),
            "and nothing is painted above it"
        );
    }

    /// The property the cubic cell exists for: on a cubic grid the rendered pose
    /// IS the sim-space pose, for ANY axis. sim→world there is
    /// `W(p) = M·(p + (½, ½, 0)) + (0, 0, GROUND_Z)` with `M = diag(-S, S, -S)`
    /// — a uniform scale times a half-turn about +y — so `W ∘ R_sim = R_world ∘
    /// W` holds exactly, and a map may convert coordinates between the hull and
    /// the world. Turn a hull about the ship demo's own TILTED axis and require
    /// the seat to equal the sim-space prediction to floating-point noise.
    ///
    /// The same check on a column-cell grid is off by whole cells (z unscaled
    /// there ⇒ `M` is not a similarity ⇒ conjugation is not a rotation), which
    /// is asserted too: it is what makes this test decisive rather than a
    /// restatement of the host's own arithmetic.
    #[test]
    fn a_cubic_grid_turns_exactly_about_a_tilted_axis() {
        let (ox, oy, oz) = (4_i64, 3_i64, 2_i64);
        let pivot = DVec3::new(9.5, 9.5, 2.0);
        let axis = DVec3::new(0.3, 0.0, 1.0); // the ship's tumble axis
        let angle = 0.7_f64;
        let local = DVec3::new(5.0, 2.0, 3.0);

        let fixed3 = |v: DVec3| {
            FixedVec3::new(
                Fixed::from_f64(v.x),
                Fixed::from_f64(v.y),
                Fixed::from_f64(v.z),
            )
        };
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let crew = world.spawn(arch);

        // The sim-space pose the script asked for: turn about the pivot,
        // right-handed in sim coordinates, then shift by the spawn offset.
        let turned = DQuat::from_axis_angle(axis.normalize(), angle) * (local - pivot)
            + pivot
            + DVec3::new(ox as f64, oy as f64, oz as f64);

        // Cubic: map that sim point through the cubic frame and expect the seat.
        let mut cube = MapRender::new(BTreeMap::new(), None, &[]);
        let hull = cube.grid_spawn_cubic(ox, oy, oz);
        cube.entity_set_grid(crew.0 as i64, hull);
        cube.grid_pivot(hull, fixed3(pivot));
        cube.grid_orient(hull, fixed3(axis), Fixed::from_f64(angle));
        let want = DVec3::new(
            -(turned.x + 0.5) * SCALE,
            (turned.y + 0.5) * SCALE,
            GROUND_Z - turned.z * SCALE,
        );
        let seat = cube.place(crew, fixed3(local));
        assert!(
            (seat - want).length() < 1e-6,
            "a cubic hull renders the sim-space turn exactly: want {want:?}, got {seat:?}"
        );

        // Column: the same script calls, the column frame — and the sim-space
        // prediction (z unscaled) no longer lands where the hull draws.
        let mut column = MapRender::new(BTreeMap::new(), None, &[]);
        let slab = column.grid_spawn(ox, oy, oz);
        column.entity_set_grid(crew.0 as i64, slab);
        column.grid_pivot(slab, fixed3(pivot));
        column.grid_orient(slab, fixed3(axis), Fixed::from_f64(angle));
        let want_col = DVec3::new(
            -(turned.x + 0.5) * SCALE,
            (turned.y + 0.5) * SCALE,
            GROUND_Z - turned.z,
        );
        assert!(
            (column.place(crew, fixed3(local)) - want_col).length() > 1.0,
            "a column hull cannot honour a tilted sim rotation — if this ever \
             passes, the anisotropy is gone and the cubic grid is redundant"
        );
    }

    /// The deck cutaway on a cubic hull, end-to-end against the real grid (the
    /// cubic twin of `deck_clip_cuts_the_deck_above_via_a_real_grid`): the
    /// threshold must follow the grid's own cell height, or a band top of `z_hi`
    /// cuts SCALE times too low and takes the crew's own deck with it.
    #[test]
    fn deck_clip_cuts_a_cubic_deck_above_via_a_real_grid() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        let (cx, cy) = (2_i64, 2_i64);
        r.voxel_fill_in(g, cx, cy, 0, cx, cy, 0, 0x80AA_AAAA); // lower deck plate
        r.voxel_fill_in(g, cx, cy, 3, cx, cy, 3, 0x8055_5555); // upper deck plate
                                                               // The fog/cutaway ride this grid (no observer ⇒ the named-grid fallback).
        r.vision_observer_in(-1, g);

        let col = DVec3::new(-(cx as f64 + 0.5) * SCALE, (cy as f64 + 0.5) * SCALE, 0.0);
        let down = DVec3::new(0.0, 0.0, 1.0);
        let hit_z = |r: &MapRender| {
            r.scene
                .raycast_clipped(col, down, 4096.0)
                .map(|h| h.voxel.z)
        };
        let gz = GROUND_Z as i32;
        let s = SCALE as i32;

        r.apply_deck_clip(); // no band declared yet
        assert_eq!(
            hit_z(&r).expect("hits the upper plate"),
            gz - 3 * s,
            "upper deck's cube tops out at GROUND_Z - 3·SCALE"
        );

        // The lower deck's band (cells 0..=2) cuts the upper plate away.
        r.deck_clip(0, 2);
        r.apply_deck_clip();
        assert_eq!(
            hit_z(&r).expect("hits the lower plate after the cut"),
            gz,
            "the cut exposes the lower deck, whose cube tops out at GROUND_Z"
        );

        // The upper deck's own band keeps everything (nothing is above it).
        r.deck_clip(3, 5);
        r.apply_deck_clip();
        assert_eq!(
            hit_z(&r).expect("hits the upper plate again"),
            gz - 3 * s,
            "a band that contains the tallest cell cuts nothing"
        );
    }

    /// The contract the whole grid-entities slice rests on: the frame a MAP
    /// computes with (`monada-script`'s fixed-point [`GridStore`], which answers
    /// `grid_world`) and the frame the host DRAWS are the same frame. Feed both
    /// the identical calls a script would make, then require that seating a
    /// point through the store and mapping it sim→world lands where `place`
    /// renders it.
    ///
    /// They cannot be bit-equal — the store turns a fixed-point quaternion, the
    /// renderer an `f64` one — so this also pins HOW closely they agree:
    /// measured at ~1e-6 world voxels over a 20-cell hull (a cell is `SCALE`
    /// voxels, so ~1e-7 of a cell), asserted at 1e-5 to leave room for a
    /// platform's `f64` `sin`/`cos` differing in its last bits. A map may
    /// therefore convert a hull-local point and act on the answer without the
    /// cell it lands in drifting away from the pixel the player sees.
    #[test]
    fn the_sim_frame_and_the_drawn_frame_agree() {
        let (ox, oy, oz) = (4_i64, 3_i64, 2_i64);
        let pivot = FixedVec3::new(
            Fixed::from_f64(9.5),
            Fixed::from_f64(9.5),
            Fixed::from_int(2),
        );
        let axis = FixedVec3::new(Fixed::from_f64(0.3), Fixed::ZERO, Fixed::from_int(1));
        let angle = Fixed::from_f64(0.7);

        // The script's calls, once into the sim's frame table…
        let mut store = monada_script::GridStore::new();
        let handle = store.spawn(
            FixedVec3::new(
                Fixed::from_int(ox as i32),
                Fixed::from_int(oy as i32),
                Fixed::from_int(oz as i32),
            ),
            true,
        );
        store.set_pivot(handle, pivot);
        store.orient(handle, axis, angle);

        // …and once into the renderer.
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(ox, oy, oz);
        r.grid_pivot(g, pivot);
        r.grid_orient(g, axis, angle);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let crew = world.spawn(arch);
        r.entity_set_grid(crew.0 as i64, g);

        for local in [
            FixedVec3::new(Fixed::from_int(5), Fixed::from_int(2), Fixed::from_int(3)),
            FixedVec3::ZERO,
            FixedVec3::new(Fixed::from_int(19), Fixed::from_int(19), Fixed::from_int(5)),
        ] {
            let drawn = r.place(crew, local);
            let computed = entity_world_of_in(true, store.to_world(handle, local));
            assert!(
                (drawn - computed).length() < 1e-5,
                "the map's frame and the renderer's disagree at {local:?}: \
                 drawn {drawn:?}, computed {computed:?}"
            );
        }
    }

    /// The same agreement, for a frame a BODY drives (`grid_pose`,
    /// docs/plans/ship-physics.md S-3). This is the one that decides whether a
    /// ship is a rigid body or merely looks like one: feed the identical pose
    /// to `GridStore` (what the map reads, in fixed point) and to the renderer
    /// (what the player sees, in `f64`), and require a hull-local point to land
    /// in the same world place through both.
    ///
    /// The trap it exists to catch is the rotation's mapping. A body's attitude
    /// is a SIM rotation, and a script grid's voxels were painted through
    /// `world_of` already — so the drawn rotation is the CONJUGATE `M q M⁻¹`,
    /// not the mirror composed with it (`body_grid_pose`'s `M ∘ q`, which is
    /// right for a body-mirror grid whose voxels are still shape-local). The
    /// two agree at identity and nowhere else, so only a tilted pose tells them
    /// apart.
    #[test]
    fn a_body_driven_frame_agrees_with_what_is_drawn() {
        let pivot = FixedVec3::new(Fixed::from_int(10), Fixed::from_int(10), Fixed::from_int(3));
        // A tumble about a tilted axis — the ship's own — so a mapping that is
        // only right about z cannot pass.
        let axis = FixedVec3::new(Fixed::from_f64(0.3), Fixed::from_f64(-0.2), Fixed::ONE);
        let rot = monada_fixed::FixedQuat::from_axis_angle(axis, Fixed::from_f64(0.9)).normalize();
        let origin = FixedVec3::new(
            Fixed::from_f64(-4.25),
            Fixed::from_f64(11.5),
            Fixed::from_f64(2.75),
        );

        let mut store = monada_script::GridStore::new();
        let handle = store.spawn(FixedVec3::ZERO, true);
        store.set_pivot(handle, pivot);
        store.set_origin(handle, origin);
        store.set_rotation(handle, rot);

        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        r.grid_pivot(g, pivot);
        r.grid_pose(g, origin, rot);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let crew = world.spawn(arch);
        r.entity_set_grid(crew.0 as i64, g);

        for local in [
            pivot,
            FixedVec3::ZERO,
            FixedVec3::new(Fixed::from_int(19), Fixed::from_int(19), Fixed::from_int(5)),
        ] {
            let drawn = r.place(crew, local);
            let computed = entity_world_of_in(true, store.to_world(handle, local));
            assert!(
                (drawn - computed).length() < 1e-5,
                "a body-driven frame disagrees with the drawing at {local:?}: \
                 drawn {drawn:?}, computed {computed:?}"
            );
        }
    }

    /// A body bound to a script grid is not drawn twice (D4). The map's painted
    /// hull is its picture; the automatic mirror would put a second,
    /// material-coloured copy of the same silhouette inside it — which, because
    /// the two agree exactly, reads as "the hull looks wrong" rather than as a
    /// duplicate.
    #[test]
    fn a_bound_body_is_not_auto_mirrored() {
        use monada_script::ScriptBackend as _;

        let phys = monada_script::shared_physics(30);
        let mut backend = monada_script::RhaiBackend::new(monada_script::shared_world(1));
        let r = std::sync::Arc::new(std::sync::Mutex::new(MapRender::new(
            BTreeMap::new(),
            None,
            &[],
        )));
        let bridge: monada_script::SharedBridge = r.clone();
        backend.set_bridge(&bridge);
        backend.set_physics(&phys);
        backend
            .load(
                r"
                fn init() {
                    phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));
                    let g = grid_spawn_cubic(0, 0, 0);
                    let s = phys_shape(4, 4, 4);
                    phys_shape_fill(s, 0, 0, 0, 3, 3, 3, 0);
                    let b = phys_body(s, vec3(fixed(0), fixed(0), fixed(0)));
                    grid_body(g, b);

                    // A second body, deliberately unbound: the mirror is not
                    // switched off globally, only for hulls somebody draws.
                    let s2 = phys_shape(2, 2, 2);
                    phys_shape_fill(s2, 0, 0, 0, 1, 1, 1, 0);
                    phys_body(s2, vec3(fixed(30), fixed(0), fixed(0)));
                }
            ",
            )
            .expect("compile");
        monada_script::ScriptBackend::on_init(&mut backend).expect("init");

        let mut render = r.lock().expect("render mutex");
        render.set_volume_terrain();
        render.sync_physics(&phys.lock().expect("physics mutex"), 1.0 / 30.0);
        assert!(
            !render.body_mirrors.contains_key(&0),
            "the bound hull draws as its own grid"
        );
        assert!(
            render.body_mirrors.contains_key(&1),
            "an unbound body still gets the automatic mirror"
        );
    }

    /// Both halves of a billboard actor's orientation are questions about the
    /// FLOOR it stands on: which directional sprite to show (the angle between
    /// the viewer and its nose, measured in the plane it walks on) and which way
    /// is up inside the card. roxlap 0.32's `ActorFacing::Dir` +
    /// `BillboardUp::Axis` take that floor as a world-space nose and up axis, so
    /// the host's job is to apply the grid's rotation to both — and to keep the
    /// old world-floor spelling when there is no grid, which roxlap pins as
    /// bit-identical to its pre-BB.6 maths.
    #[test]
    fn a_billboard_stands_on_its_grids_floor() {
        let local_yaw = 0.6;

        // No grid: the verbatim pre-BB.6 path.
        let (facing, up) = actor_pose(local_yaw, DQuat::IDENTITY);
        assert_eq!(facing, ActorFacing::Yaw(local_yaw));
        assert_eq!(up, BillboardUp::World);

        // On a hull rolled about a tilted axis, both the nose and the deck's up
        // are that rotation applied to the actor's own frame.
        let rot = DQuat::from_axis_angle(DVec3::new(0.3, 0.0, 1.0).normalize(), 0.7);
        let (facing, up) = actor_pose(local_yaw, rot);
        let arr = |a: [f32; 3]| DVec3::new(f64::from(a[0]), f64::from(a[1]), f64::from(a[2]));
        let ActorFacing::Dir(dir) = facing else {
            panic!("a turning floor needs a direction, not a world yaw");
        };
        let BillboardUp::Axis(deck) = up else {
            panic!("a turning floor needs its own up axis, got {up:?}");
        };
        let want_nose = rot * DVec3::new(local_yaw.cos(), local_yaw.sin(), 0.0);
        let want_up = rot * DVec3::new(0.0, 0.0, -1.0);
        assert!(
            (arr(dir) - want_nose).length() < 1e-6,
            "the nose rides the hull"
        );
        assert!(
            (arr(deck) - want_up).length() < 1e-6,
            "and so does the deck's up"
        );
        assert!(
            // Both axes cross the wall to roxlap as `f32`, so this is exact to
            // that rounding, not to `f64`'s.
            arr(dir).dot(arr(deck)).abs() < 1e-6,
            "the nose lies IN the deck plane — it is a facing, not a lean"
        );

        // The tilt is what makes this worth passing at all: flattening the nose
        // into the WORLD plane (every spelling before 0.32) points somewhere
        // else, and that error is what turned a standing crew member on the spot.
        let flattened = DVec3::new(want_nose.x, want_nose.y, 0.0).normalize();
        assert!(
            flattened.angle_between(want_nose) > 0.05,
            "a tilted deck's nose is not its world-flattened shadow"
        );
    }

    /// `camera_grid` turns the whole orbit frame with its grid, so the deck
    /// holds still on screen while the world sweeps past. What makes it a
    /// correctness fix rather than a look: an entity bound to a grid has a
    /// grid-LOCAL position, so a map reading input relative to a world-fixed
    /// camera steers in the ship's frame while the player watches the world's —
    /// "forward" lands somewhere new every tick the hull turns.
    ///
    /// Requires the eye to stay on its orbit: the offset from the focus must
    /// turn with the basis, or riding a rotating grid would swing the camera
    /// through the hull.
    #[test]
    fn the_camera_can_ride_a_grids_rotation() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        r.camera_angle(Fixed::from_f64(0.8), Fixed::from_f64(1.2));
        r.camera_dist(Fixed::from_int(60));
        r.camera_focus(FixedVec3::new(
            Fixed::from_int(6),
            Fixed::from_int(4),
            Fixed::ZERO,
        ));
        let world_frame = r.camera();

        // Riding an UNTURNED grid changes nothing — the identity composes away.
        r.camera_grid(g);
        let at_rest = r.camera();
        for i in 0..3 {
            assert!(
                (at_rest.forward[i] - world_frame.forward[i]).abs() < 1e-12
                    && (at_rest.pos[i] - world_frame.pos[i]).abs() < 1e-9,
                "an unturned grid leaves the camera where it was"
            );
        }

        // Turn the hull: the basis must turn with it, exactly.
        r.grid_orient(
            g,
            FixedVec3::new(Fixed::from_f64(0.3), Fixed::ZERO, Fixed::from_int(1)),
            Fixed::from_f64(0.7),
        );
        let rot = r
            .scene
            .grid(r.grid_id(g).expect("live grid"))
            .expect("grid")
            .transform
            .rotation;
        let riding = r.camera();
        let want = rot * DVec3::from_array(world_frame.forward);
        assert!(
            (DVec3::from_array(riding.forward) - want).length() < 1e-12,
            "the view basis rides the hull's rotation"
        );
        // …and the eye still sits `dist` back along that turned forward, on the
        // focus point — not swung off the orbit.
        let center = r.camera.center;
        let eye = DVec3::from_array(riding.pos);
        assert!(
            ((center - eye).length() - r.camera.dist).abs() < 1e-9,
            "the eye keeps its orbit distance from the focus"
        );
        assert!(
            ((center - eye).normalize() - DVec3::from_array(riding.forward)).length() < 1e-9,
            "and still looks straight at it"
        );

        // `-1` returns the camera to the world frame.
        r.camera_grid(-1);
        assert!(
            (DVec3::from_array(r.camera().forward) - DVec3::from_array(world_frame.forward))
                .length()
                < 1e-12,
            "-1 puts the camera back in the world frame"
        );
    }

    /// The cursor must land on the ground a **volume** map actually has.
    ///
    /// The column march reads `self.terrain`, the heightmap store, which
    /// is empty by design on a volume map — so every probe saw a ground
    /// height of zero, the ray fell through to the `z = 0` plane, and the
    /// answer came back in the column convention's coordinates instead of
    /// the isotropic cell grid's. Nothing crashed and nothing logged: the
    /// cursor simply pointed somewhere else, which from the outside looks
    /// like a build placement that does nothing at all.
    #[test]
    fn a_volume_map_picks_the_ground_it_has() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        r.set_volume_terrain();
        // A plateau twenty cells up, around sim (40, 40).
        r.voxel_fill(30, 30, 0, 50, 50, 20, 0x80c8_b48c);

        // An OBLIQUE ray, because that is what a camera casts and because
        // a vertical one cannot tell the two paths apart: straight down,
        // the plane hit and the surface hit share an x and a y, and the
        // test would pass against the broken code.
        let eye = volume_world_of(DVec3::new(20.0, 20.0, 60.0));
        let target = volume_world_of(DVec3::new(40.5, 40.5, 20.5));
        let dir = (target - eye).normalize();
        let (x, y) = r.ground_sim(eye, dir).expect("the ray meets the plateau");
        // Within a cell of the aim point. The broken path answers ~47.9,
        // where the ray would have crossed the world z = 0 plane eight
        // cells further on and four cells below the plateau it hit.
        assert!(
            (x - 40.5).abs() < 1.0 && (y - 40.5).abs() < 1.0,
            "the cursor landed at ({x:.2}, {y:.2}), not on the cell under it"
        );

        // And the same ray picks an entity standing there, which the flat
        // `z = 0` hit could not: the plane is hundreds of world units
        // below the plateau, so every candidate was out of range.
        let hull = r.model_box(24, 16, 10, 0x80a8_b48c);
        let mut world = World::new(0);
        let arch = world.register_archetype(&[]);
        let unit = world.spawn(arch);
        world.set_position(
            unit,
            FixedVec3::new(
                Fixed::from_f64(40.5),
                Fixed::from_f64(40.5),
                Fixed::from_int(21),
            ),
        );
        r.entity_set_model(unit.0 as i64, hull);
        let (_, picked) = r.pick(&world, eye, dir);
        assert_eq!(
            picked, unit.0 as i64,
            "the unit under the cursor was missed"
        );
    }

    /// A plain KV6 model with a script-set facing must turn its GEOMETRY
    /// (decision L4 of docs/plans/desert-game.md).
    ///
    /// A billboard actor answers a facing by picking one of eight
    /// pre-drawn sides; a voxel hull has no sides to pick, so the model
    /// itself has to yaw — which means leaving the positional instance
    /// path for the posed one. Before this, `entity_set_facing` on a
    /// `model_kv6` / `model_box` binding was silently dropped: the tank
    /// drove sideways and nothing in the engine complained.
    ///
    /// Asserts the routing and the basis, which is what a headless test
    /// can reach; the pixels still want an eye on them.
    #[test]
    fn a_faced_model_turns_its_geometry() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let hull = r.model_box(24, 16, 10, 0x80a8_b48c);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["heading"]);
        let tank = world.spawn(arch);
        world.set_position(
            tank,
            FixedVec3::new(Fixed::from_int(10), Fixed::from_int(10), Fixed::ZERO),
        );
        r.entity_set_model(tank.0 as i64, hull);

        // Unturned: nothing dynamic in the map, so the cheap static path.
        r.build_instances(&world);
        assert_eq!(r.prop_targets.len(), 0);
        assert_eq!(r.sprites.instances.len(), 1, "placed, not posed");

        // A quarter turn to sim +y. The map is now a dynamic-layer map,
        // because a posed instance is the only way to express it.
        r.entity_set_facing(tank.0 as i64, Fixed::from_f64(std::f64::consts::FRAC_PI_2));
        r.build_instances(&world);
        assert_eq!(r.prop_targets.len(), 1, "a faced model is posed");
        assert!(
            r.sprites.instances.is_empty(),
            "…and leaves the static path, or it would be drawn twice"
        );

        // The basis must be the WORLD yaw: `world_of` mirrors X, so a sim
        // heading reads as `PI - yaw` on screen. Getting this backwards
        // is a tank that turns the wrong way — visible, but only to an
        // eye that knows which way it asked for.
        let (_, _, rot, _, _) = r.prop_targets[0];
        let nose = rot * DVec3::X;
        let want =
            DQuat::from_rotation_z(facing_to_world_yaw(std::f64::consts::FRAC_PI_2)) * DVec3::X;
        assert!(
            (nose - want).length() < 1e-9,
            "the hull points at {nose:?}, expected {want:?}"
        );
    }

    /// A facing of ZERO must be posed like any other — the regression the
    /// first live run of the desert caught.
    ///
    /// Recording a facing is what makes a map dynamic-layer, and a
    /// dynamic-layer map uploads its static sprite set exactly once. So
    /// treating the identity rotation as "nothing to do" and leaving the
    /// instance on the static path does not skip work, it *freezes the
    /// model*: the desert's vehicle drives due +x on heading 0 until it
    /// reaches the map edge, and it sat perfectly still on screen while
    /// the simulation drove it across the dunes. Headless tests all
    /// passed, because the world was moving exactly as asked.
    #[test]
    fn a_facing_of_zero_is_still_posed() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let hull = r.model_box(24, 16, 10, 0x80a8_b48c);
        let mut world = World::new(0);
        let arch = world.register_archetype(&["heading"]);
        let tank = world.spawn(arch);
        world.set_position(
            tank,
            FixedVec3::new(Fixed::from_int(3), Fixed::ZERO, Fixed::ZERO),
        );
        r.entity_set_model(tank.0 as i64, hull);
        r.entity_set_facing(tank.0 as i64, Fixed::ZERO);

        r.build_instances(&world);
        assert_eq!(
            r.prop_targets.len(),
            1,
            "a zero facing is a facing: the model must be posed, not left \
             on the static path this map no longer rebuilds"
        );
        assert!(r.sprites.instances.is_empty());

        // …and it must follow the entity, which is the property the freeze
        // actually broke.
        world.set_position(
            tank,
            FixedVec3::new(Fixed::from_int(9), Fixed::ZERO, Fixed::ZERO),
        );
        r.build_instances(&world);
        let (_, seat, _, _, _) = r.prop_targets[0];
        assert!(
            (seat.x - world_of(FixedVec3::new(Fixed::from_int(9), Fixed::ZERO, Fixed::ZERO)).x)
                .abs()
                < 1e-9,
            "the posed instance did not follow the entity"
        );
    }

    /// `entity_set_side`'s basis must agree with `entity_set_facing`'s on
    /// every horizontal face: `side_quat(dir, Deg0)`'s nose has to be the
    /// same world vector `entity_set_facing`'s `PI - yaw` basis already
    /// produces for the matching sim yaw, or a map switching between the
    /// two verbs would see its model snap by however far the two
    /// conventions disagree — exactly the handedness bug this test is
    /// here to catch.
    #[test]
    fn side_quat_matches_facing_on_the_horizontal() {
        let cases = [
            (Direction::X, 0.0),
            (Direction::Y, std::f64::consts::FRAC_PI_2),
            (Direction::NegX, std::f64::consts::PI),
            (Direction::NegY, -std::f64::consts::FRAC_PI_2),
        ];
        for (dir, yaw) in cases {
            let from_side = side_quat(dir, Roll::Deg0) * DVec3::X;
            let from_facing = DQuat::from_rotation_z(facing_to_world_yaw(yaw)) * DVec3::X;
            assert!(
                (from_side - from_facing).length() < 1e-9,
                "{dir:?}: side gave {from_side:?}, facing gave {from_facing:?}"
            );
        }
    }

    /// A `entity_set_side` model turns its GEOMETRY exactly like a faced one
    /// (decision L4) — same routing to the posed/dynamic path, same reason: a
    /// voxel solid has no pre-drawn side to pick.
    #[test]
    fn a_sided_model_turns_its_geometry() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let hull = r.model_box(24, 16, 10, 0x80a8_b48c);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["heading"]);
        let tank = world.spawn(arch);
        world.set_position(
            tank,
            FixedVec3::new(Fixed::from_int(10), Fixed::from_int(10), Fixed::ZERO),
        );
        r.entity_set_model(tank.0 as i64, hull);

        // Unturned: nothing dynamic in the map, so the cheap static path.
        r.build_instances(&world);
        assert_eq!(r.prop_targets.len(), 0);
        assert_eq!(r.sprites.instances.len(), 1, "placed, not posed");

        // Face sim +y, unrolled.
        r.entity_set_side(tank.0 as i64, Direction::Y as i64, Roll::Deg0 as i64);
        r.build_instances(&world);
        assert_eq!(r.prop_targets.len(), 1, "a sided model is posed");
        assert!(
            r.sprites.instances.is_empty(),
            "…and leaves the static path, or it would be drawn twice"
        );

        let (_, _, rot, _, _) = r.prop_targets[0];
        let nose = rot * DVec3::X;
        let want =
            DQuat::from_rotation_z(facing_to_world_yaw(std::f64::consts::FRAC_PI_2)) * DVec3::X;
        assert!(
            (nose - want).length() < 1e-9,
            "the hull points at {nose:?}, expected {want:?}"
        );
    }

    /// A model spun in place with `entity_set_side` must turn about its OWN
    /// centre, not swing around it: with no grid underneath it, the pivot-drop
    /// offset (`sync_props`'s `seat + drop_rot * (0,0,-drop)`) must stay
    /// IDENTITY-rotated regardless of which way the model itself faces, since
    /// there is no floor here for the model's local -Z to stay planted
    /// against. Folding the model's own orientation into `drop_rot` (as it
    /// once did) made the offset vector swing with every `entity_set_side`
    /// call — the same rotation that turns the geometry also dragged the
    /// whole model sideways, so what should have been a spin in place read as
    /// an orbit around the entity's position.
    #[test]
    fn entity_set_side_spins_the_model_without_moving_it() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let hull = r.model_box(24, 16, 10, 0x80a8_b48c);
        let mut world = World::new(0);
        let arch = world.register_archetype(&["heading"]);
        let tank = world.spawn(arch);
        world.set_position(
            tank,
            FixedVec3::new(Fixed::from_int(10), Fixed::from_int(10), Fixed::ZERO),
        );
        r.entity_set_model(tank.0 as i64, hull);

        let mut seats = Vec::new();
        let mut bases = Vec::new();
        for (dir, roll) in [
            (Direction::X, Roll::Deg0),
            (Direction::Y, Roll::Deg90),
            (Direction::NegZ, Roll::Deg270),
        ] {
            r.entity_set_side(tank.0 as i64, dir as i64, roll as i64);
            r.build_instances(&world);
            let (_, seat, rot, drop_rot, drop) = r.prop_targets[0];
            assert_eq!(
                drop_rot,
                DQuat::IDENTITY,
                "{dir:?}/{roll:?}: no grid to tip, so the drop rotation must stay put"
            );
            seats.push(seat + drop_rot * DVec3::new(0.0, 0.0, -drop));
            bases.push(rot * DVec3::X);
        }
        for w in seats.windows(2) {
            assert!(
                (w[0] - w[1]).length() < 1e-9,
                "the drawn pivot moved between orientations: {:?} -> {:?}",
                w[0],
                w[1]
            );
        }
        assert!(
            (bases[0] - bases[1]).length() > 1e-3 && (bases[1] - bases[2]).length() > 1e-3,
            "the model's basis must actually have turned between the three sides"
        );
    }

    /// A side of `(X, Deg0)` — the "do nothing" orientation — must still be
    /// posed: the `entity_set_side` analogue of `a_facing_of_zero_is_still_posed`.
    /// Recording a side is what makes a map dynamic-layer, so leaving the
    /// identity case on the static path would freeze the model exactly the
    /// way a zero facing once did.
    #[test]
    fn a_side_of_x_deg0_is_still_posed() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let hull = r.model_box(24, 16, 10, 0x80a8_b48c);
        let mut world = World::new(0);
        let arch = world.register_archetype(&["heading"]);
        let tank = world.spawn(arch);
        world.set_position(
            tank,
            FixedVec3::new(Fixed::from_int(3), Fixed::ZERO, Fixed::ZERO),
        );
        r.entity_set_model(tank.0 as i64, hull);
        r.entity_set_side(tank.0 as i64, Direction::X as i64, Roll::Deg0 as i64);

        r.build_instances(&world);
        assert_eq!(
            r.prop_targets.len(),
            1,
            "an identity side is still a side: the model must be posed"
        );
        assert!(r.sprites.instances.is_empty());
    }

    /// `entity_set_side` wins over an earlier `entity_set_facing` on the
    /// same entity — the precedence `seat_sprite` picks when a script sets
    /// both (accidentally or not).
    #[test]
    fn entity_set_side_wins_over_entity_set_facing() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let hull = r.model_box(24, 16, 10, 0x80a8_b48c);
        let mut world = World::new(0);
        let arch = world.register_archetype(&["heading"]);
        let tank = world.spawn(arch);
        world.set_position(
            tank,
            FixedVec3::new(Fixed::from_int(3), Fixed::ZERO, Fixed::ZERO),
        );
        r.entity_set_model(tank.0 as i64, hull);
        r.entity_set_facing(tank.0 as i64, Fixed::ZERO);
        r.entity_set_side(tank.0 as i64, Direction::Y as i64, Roll::Deg0 as i64);

        r.build_instances(&world);
        let (_, _, rot, _, _) = r.prop_targets[0];
        let nose = rot * DVec3::X;
        let want =
            DQuat::from_rotation_z(facing_to_world_yaw(std::f64::consts::FRAC_PI_2)) * DVec3::X;
        assert!(
            (nose - want).length() < 1e-9,
            "facing (yaw 0) should have been overridden by side (+y): got {nose:?}, want {want:?}"
        );
    }

    /// An out-of-range `dir`/`roll` discriminant is ignored, the same
    /// convention as an out-of-range grid handle elsewhere in this bridge —
    /// not a panic, and not a pose.
    #[test]
    fn entity_set_side_ignores_an_out_of_range_discriminant() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let hull = r.model_box(24, 16, 10, 0x80a8_b48c);
        let mut world = World::new(0);
        let arch = world.register_archetype(&["heading"]);
        let tank = world.spawn(arch);
        world.set_position(tank, FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ZERO));
        r.entity_set_model(tank.0 as i64, hull);
        r.entity_set_side(tank.0 as i64, 99, 0);

        r.build_instances(&world);
        assert_eq!(
            r.prop_targets.len(),
            0,
            "a bad discriminant must not pose the model"
        );
        assert_eq!(r.sprites.instances.len(), 1);
    }

    /// A prop riding a TURNING grid must be POSED, not merely placed: roxlap's
    /// static sprite instance carries a position and nothing else, so a crate
    /// left on that path keeps its world-axis alignment while the hull rolls
    /// under it — which is exactly what the first live run of the cargo slice
    /// showed. It has to be routed to the dynamic layer with the grid's basis.
    ///
    /// Asserts the routing and the pose, which is what a headless test can
    /// reach; the pixels still want an eye on them.
    #[test]
    fn a_prop_on_a_turning_grid_is_posed_not_axis_aligned() {
        let mut r = MapRender::new(hero_assets(), None, &[]);
        // An actor model is what makes this a dynamic-layer map (the static
        // sprite set is uploaded once instead of every frame), the precondition
        // for any instance to be posed at all.
        assert!(
            r.model_actor("char/hero", &["idle".to_string()], Fixed::from_int(2)) >= 0,
            "actor model registered"
        );
        let crate_model = r.model_box(4, 4, 4, 0x80a8_7a3c);
        let g = r.grid_spawn_cubic(0, 0, 0);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["held"]);
        let stowed = world.spawn(arch);
        let loose = world.spawn(arch);
        let p = FixedVec3::new(Fixed::from_int(6), Fixed::from_int(2), Fixed::ZERO);
        world.set_position(stowed, p);
        world.set_position(loose, p);
        r.entity_set_model(stowed.0 as i64, crate_model);
        r.entity_set_model(loose.0 as i64, crate_model);
        r.entity_set_grid(stowed.0 as i64, g);

        // Even at REST the stowed crate must be posed, not placed: in a
        // dynamic-layer map the static set is uploaded once, so a static
        // instance is frozen at its frame-0 spot forever. Routing on "is the
        // grid turning" left exactly that ghost — the hull is unturned on frame
        // 0, so the crate was baked in and then also drawn posed.
        //
        // The world-frame crate rides the dynamic layer for the same reason,
        // not a weaker one: the single upload cannot hold an entity that
        // spawns later or moves later, and the renderer has no way to know
        // which entity that will be.
        r.build_instances(&world);
        assert_eq!(
            r.prop_targets.len(),
            2,
            "every model-bound prop is posed on a dynamic-layer map"
        );
        assert_eq!(
            r.sprites.instances.len(),
            0,
            "and nothing is baked into the one static upload"
        );

        // Roll the hull onto a tilted axis — now the stowed crate must be posed.
        r.grid_orient(
            g,
            FixedVec3::new(Fixed::from_f64(0.3), Fixed::ZERO, Fixed::from_int(1)),
            Fixed::from_f64(0.7),
        );
        r.build_instances(&world);
        assert_eq!(r.prop_targets.len(), 2, "both crates are posed");

        // Which target is the stowed one is not a question about ordering.
        // Picked by where it rides -- the rolled hull has carried it well
        // away from the world-frame crate they were both spawned on -- so
        // the basis assertions below still have something to prove.
        let stowed_seat = r.place(stowed, p);
        let (si, seat, rot, drop_rot, drop) = r
            .prop_targets
            .iter()
            .copied()
            .find(|&(_, seat, ..)| (seat - stowed_seat).length() < 1e-9)
            .expect("the crate on the hull is posed where it rides");
        assert_eq!(si, crate_model as usize, "posed with its own sprite model");
        let grid_rot = r
            .scene
            .grid(r.grid_id(g).expect("live grid"))
            .expect("grid")
            .transform
            .rotation;
        assert!(
            (rot * DVec3::X - grid_rot * DVec3::X).length() < 1e-12,
            "the prop's basis IS the hull's rotation"
        );
        assert!(
            (drop_rot * DVec3::X - grid_rot * DVec3::X).length() < 1e-12,
            "with no entity_set_side, the drop rotation is the hull's too"
        );
        assert!(
            (seat - r.place(stowed, p)).length() < 1e-9,
            "and it is seated where the entity rides"
        );
        // The pivot drop is a MODEL-space offset, so a rolled hull must push the
        // crate along the hull's own down, not the world's.
        let posed = seat + drop_rot * DVec3::new(0.0, 0.0, -drop);
        let naive = seat - DVec3::new(0.0, 0.0, drop);
        assert!(
            drop > 0.0 && (posed - naive).length() > 1e-6,
            "the seat correction turns with the hull (drop {drop})"
        );
    }

    /// `grid_move` re-places a grid after it was spawned — a hull under way —
    /// and its riders go with it: a bound entity's seat shifts by exactly the
    /// move, in the grid's own cell shape (a cubic grid scales z like x/y).
    /// Composes with a rotation already set, since both go through the one pose
    /// writer.
    #[test]
    fn grid_move_carries_its_riders() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        r.grid_orient(
            g,
            FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::from_int(1)),
            Fixed::from_f64(std::f64::consts::FRAC_PI_2),
        );

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let crew = world.spawn(arch);
        let p = FixedVec3::new(Fixed::from_int(5), Fixed::from_int(2), Fixed::from_int(1));
        world.set_position(crew, p);
        r.entity_set_grid(crew.0 as i64, g);
        let before = r.place(crew, p);

        r.grid_move(
            g,
            FixedVec3::new(Fixed::from_int(3), Fixed::from_int(-2), Fixed::from_int(1)),
        );
        let after = r.place(crew, p);
        // sim (3, -2, 1) through `world_of`'s linear part on a cubic grid:
        // x mirrors and scales, y scales, z scales AND flips (z-down).
        let want = before + DVec3::new(-3.0 * SCALE, -2.0 * SCALE, -SCALE);
        assert!(
            (after - want).length() < 1e-9,
            "the rider rides the move: want {want:?}, got {after:?}"
        );

        // And moving back to the spawn offset restores the original seat.
        r.grid_move(g, FixedVec3::ZERO);
        assert!(
            (r.place(crew, p) - before).length() < 1e-9,
            "grid_move(spawn offset) is the pose grid_spawn gave it"
        );
    }

    /// `grid_despawn` retires the whole frame: the scene grid goes, riders lose
    /// their binding, the fog that rode it is dropped, and the handle is inert
    /// forever — while the NEXT spawn gets a fresh handle that works. The
    /// tombstone matters: a compacted table would hand grid 1's identity to a
    /// stale reference to grid 0.
    #[test]
    fn grid_despawn_retires_the_grid_its_riders_and_its_fog() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        let gid = r.grid_id(g).expect("live grid");
        r.voxel_fill_in(g, -4, -4, 0, 4, 4, 0, 0x8055_5f6b);
        r.vision_config(110, 6, 3);
        r.deck_clip(0, 2);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let crew = world.spawn(arch);
        let off = Fixed::from_f64(1.0 / 32.0);
        world.set_position(crew, FixedVec3::new(off, off, Fixed::ZERO));
        r.entity_set_grid(crew.0 as i64, g);
        r.vision_observer(crew.0 as i64);
        r.build_instances(&world);
        r.apply_deck_clip();
        let _ = r.update_fow(0.016);
        assert!(r.fow.is_some(), "the fog armed on the hull");
        assert_eq!(r.vision_grid(), Some(gid), "and rides it");

        r.grid_despawn(g);

        assert!(r.grid_id(g).is_none(), "the handle is retired");
        assert!(r.scene.grid(gid).is_none(), "the scene grid is gone");
        assert!(!r.grid_anchors.contains_key(&gid), "and so is its anchor");
        assert!(r.entity_grid.is_empty(), "the rider's binding went with it");
        assert!(r.fow.is_none(), "the fog that rode it was dropped");
        assert_ne!(r.vision_grid(), Some(gid), "and no longer names it");

        // A retired handle is inert, not a hit on the next grid.
        let next = r.grid_spawn_cubic(0, 0, 0);
        assert_ne!(next, g, "handles are never reused");
        r.voxel_fill_in(g, 0, 0, 0, 0, 0, 0, 0x80FF_0000);
        let fresh = r.grid_id(next).expect("the new grid is live");
        assert!(
            r.scene
                .grid(fresh)
                .expect("grid")
                .voxel_color(IVec3::new(-1, 0, GROUND_Z as i32))
                .is_none(),
            "a paint through the dead handle must not land in the new grid"
        );
    }

    /// `voxel_clear_in` is `voxel_fill_in`'s inverse — the door primitive. On a
    /// cubic grid it must erase the whole CUBE: clearing the column
    /// convention's single voxel row would leave 15 rows of a "removed" wall
    /// standing, and the map would have no way to see it.
    #[test]
    fn voxel_clear_in_erases_a_whole_cubic_cell() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        r.voxel_fill_in(g, 0, 0, 0, 1, 0, 0, 0x8055_5f6b); // two cells side by side
        let gid = r.grid_id(g).expect("live grid");
        let gz = GROUND_Z as i32;
        let s = SCALE as i32;
        let solid = |r: &MapRender, x: i32, z: i32| {
            r.scene
                .grid(gid)
                .expect("grid")
                .voxel_color(IVec3::new(x, 0, z))
                .is_some()
        };
        assert!(
            solid(&r, -1, gz) && solid(&r, -1, gz + s - 1),
            "cell 0 solid"
        );

        r.voxel_clear_in(g, 0, 0, 0);
        assert!(!solid(&r, -1, gz), "the cube's top row is air");
        assert!(!solid(&r, -1, gz + s - 1), "and so is its bottom row");
        assert!(
            solid(&r, -s - 1, gz) && solid(&r, -s - 1, gz + s - 1),
            "the neighbouring cell is untouched"
        );

        // Clearing through a dead handle is a no-op, not a panic.
        r.grid_despawn(g);
        r.voxel_clear_in(g, 1, 0, 0);
    }

    /// The grid binding is per-entity render state with a full lifecycle: `-1`
    /// unbinds (a crew member steps off the hull and seats in the global frame
    /// again), and a despawned entity's binding is retired rather than leaking —
    /// or a long session churning crew grows the map forever, and a reused id
    /// would inherit a hull it never asked to ride.
    #[test]
    fn entity_set_grid_unbinds_and_retires_with_its_entity() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn(6, 4, 0); // off-origin ⇒ bound ≠ global seat
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        let p = FixedVec3::new(Fixed::from_int(2), Fixed::from_int(1), Fixed::ZERO);
        world.set_position(e, p);

        r.entity_set_grid(e.0 as i64, g);
        let bound = r.place(e, p);
        assert!(
            (bound - world_of(p)).length() > 1.0,
            "the bound seat must differ from the global one for this to be decisive"
        );

        // `-1` unbinds: back to the global frame.
        r.entity_set_grid(e.0 as i64, -1);
        assert!(
            (r.place(e, p) - world_of(p)).length() < 1e-9,
            "an unbound entity seats in the global frame"
        );

        // Re-bind, then despawn: the next frame's `build_instances` retires it.
        r.entity_set_grid(e.0 as i64, g);
        world.despawn(e);
        r.build_instances(&world);
        assert!(
            r.entity_grid.is_empty(),
            "a despawned entity's grid binding is retired, not leaked"
        );
    }

    /// The selection marker sits under the entity on a VOLUME map too. Volume
    /// maps scale sim z by `SCALE` (isotropic cells) while column maps leave it
    /// unscaled, and every seat goes through `entity_world_of` for exactly that
    /// reason — a marker composed from the bare `world_of` floats `(SCALE−1)·z`
    /// world units above the digger it marks.
    #[test]
    fn highlight_marker_honors_the_volume_z_scale() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        r.set_volume_terrain();
        let model = r.model_box(2, 2, 2, 0x8055_5555);
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        let p = FixedVec3::new(Fixed::from_int(1), Fixed::from_int(1), Fixed::from_int(3));
        world.set_position(e, p);
        r.entity_set_model(e.0 as i64, model);
        r.highlight(e.0 as i64);

        r.build_instances(&world);
        let marker = r
            .sprites
            .instances
            .iter()
            .find(|i| i.model == HIGHLIGHT_MODEL)
            .expect("the selected entity has a marker");
        let want = (GROUND_Z - 3.0 * SCALE - 1.0) as f32;
        assert!(
            (marker.pos[2] - want).abs() < 0.01,
            "marker seats on the entity's volume-scaled height (want {want}, got {})",
            marker.pos[2]
        );
    }

    /// Binding is opt-in and one-directional: naming a grid on `vision_observer`
    /// says where the FOG rides, never that the observer is seated on it (a v7
    /// map that named a fog grid must keep placing its entities in the global
    /// frame — the auto-binding this replaces silently moved them). Binding the
    /// observer with `entity_set_grid` is what moves the fog, so the cone and
    /// the crew member can never disagree about which hull they are on.
    #[test]
    fn naming_a_fog_grid_never_binds_the_observer() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let named = r.grid_spawn(6, 4, 0);
        let named_id = r.grid_id(named).expect("live grid");
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let observer = world.spawn(arch);
        let seat = FixedVec3::new(Fixed::from_int(2), Fixed::from_int(1), Fixed::ZERO);
        world.set_position(observer, seat);

        // v7 semantics: the fog rides the named grid, the entity does not.
        r.vision_observer_in(observer.0 as i64, named);
        assert_eq!(
            r.vision_grid(),
            Some(named_id),
            "the named grid carries the fog"
        );
        assert!(
            (r.place(observer, seat) - world_of(seat)).length() < 1e-9,
            "naming a fog grid must not seat the entity on it"
        );

        // The explicit binding is what seats the crew — and the fog derives from
        // it, so a second hull can't disagree with the one the crew rides.
        let hull = r.grid_spawn(1, 9, 0);
        let hull_id = r.grid_id(hull).expect("live grid");
        r.entity_set_grid(observer.0 as i64, hull);
        assert_eq!(
            r.vision_grid(),
            Some(hull_id),
            "the grid the observer rides carries the fog, whatever was named"
        );
        let origin = r.scene.grid(hull_id).expect("hull").transform.origin;
        assert!(
            (r.place(observer, seat) - (world_of(seat) + origin)).length() < 1e-9,
            "the entity now seats through the grid it was bound to"
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
        // `entity_set_grid` binds it to grid `g` (the explicit opt-in; naming the
        // grid on `vision_observer` would NOT bind it), so `place` composes this
        // through the hull transform to the true world seat (== WORLD sim
        // (wx+4, wy+4, wz)); `update_fow` then rebases back to grid-local.
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        world.set_position(
            e,
            FixedVec3::new(Fixed::from_int(4), Fixed::from_int(4), Fixed::ZERO),
        );
        r.entity_set_grid(e.0 as i64, g);
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
        let gid = r.grid_id(g).expect("live grid");
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
        let gid = r.grid_id(g).expect("live grid");
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
    /// `render_excluded` — draws the hull, and a hull that merely rotates bumps
    /// neither of `FowTwin::sync`'s gates (mask version, voxel mutation), so
    /// once the crew is idle and the mask settles every frame is a quiet one.
    /// roxlap ≤ 0.31.0 gated its render-config mirror on those gates, and the
    /// twin froze while the real grid kept turning: the hull stalled on screen
    /// and the crew — seated from the live real transform — slid off it. This
    /// pins the contract monada depends on (roxlap 0.31.1 mirrors on every
    /// `sync`): rotate the real grid on a settled frame, the twin tracks it.
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
        let gid = r.grid_id(g).expect("live grid");
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

    /// The deck cutaway must reach the grid that actually DRAWS. `apply_deck_clip`
    /// writes `z_clip` on the real grid, but the real grid is `render_excluded`
    /// once fog arms — the twin renders. A crew member climbing a deck changes
    /// `z_clip` without touching a voxel or a visible cell, so the frame is a
    /// quiet one, and under roxlap ≤ 0.31.0 — which gated the render-config
    /// mirror on that — the cutaway never opened. This pins the contract
    /// monada depends on: flip the deck band on a quiet frame and confirm the
    /// twin carries the new clip. Guards the WHOLE mirrored set, not just the
    /// transform the sibling rotation test covers.
    #[test]
    fn deck_clip_reaches_the_twin_on_a_quiet_frame() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn(0, 0, 0);
        r.voxel_fill_in(g, -40, -40, 0, 40, 40, 0, 0x8055_5f6b);
        r.vision_config(110, 6, 3);
        r.deck_clip(0, 3);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        // Mid-cell, as in `fog_twin_tracks_hull_rotation_on_a_quiet_frame`: the
        // re-basing round-trip can't tip `floor` to a neighbouring cell, so the
        // mask provably settles.
        let off = Fixed::from_f64(1.0 / 32.0);
        world.set_position(e, FixedVec3::new(off, off, Fixed::ZERO));
        r.entity_set_grid(e.0 as i64, g);
        r.vision_observer(e.0 as i64);

        // Settle the mask so `sync` reaches its quiet-frame early-out.
        for _ in 0..4 {
            r.build_instances(&world);
            r.apply_deck_clip();
            let _ = r.update_fow(0.016);
        }
        let settled_ver = r.fow.as_ref().expect("mask built").mask_version();

        // The crew climbs: a new band ⇒ a new `z_clip`, nothing else.
        r.deck_clip(4, 7);
        let want = deck_clip_world_z(7, 1); // a column-cell grid: one voxel per cell
        r.build_instances(&world);
        r.apply_deck_clip(); // the order `render_into` uses (clip, then fog)
        let _ = r.update_fow(0.016);

        assert_eq!(
            r.fow.as_ref().expect("mask built").mask_version(),
            settled_ver,
            "a deck flip must not perturb the mask — otherwise this isn't the \
             quiet-frame path the fix targets"
        );
        let twin_id = r.fow_twin.as_ref().expect("twin armed").twin();
        assert_eq!(
            r.scene.grid(twin_id).expect("twin grid").z_clip,
            Some(want),
            "the rendered twin carries the new deck cutaway"
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

    /// **The scene's LOD is measured against the camera, not against a
    /// constant.** A chunk marches at `floor(log2(t / d))`; roxlap's
    /// default `d` is a hull camera's 64 world units, and an isometric
    /// map watching from sixteen hundred sits wholly past it — every
    /// chunk at the ladder's coarsest mip, a blurred world, fast for the
    /// wrong reason. Pinned as the rule rather than the number: mip 0
    /// has to reach past the ground a top-down view is looking at.
    #[test]
    fn scene_lod_keeps_the_ground_a_top_down_camera_watches_at_mip_zero() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        // necromy's opening frame: a 64-cell field (1024 world voxels
        // across, so ~1450 corner to corner) seen from 1600 away.
        r.camera_dist(Fixed::from_int(1600));
        let d = r.mip_scan_dist().expect("no env override in tests");
        assert_eq!(d as i32, 1600, "the camera's own distance is the scale");

        // The ladder's first step is at 2d, and the far corner of that
        // field sits at the camera distance plus its own diagonal.
        let mip = |t: f32| (t.max(d) / d).log2().floor() as i32;
        assert_eq!(mip(1600.0 + 1450.0), 0, "the far corner is still mip 0");
        // …and something twice as far from the eye as the subject does
        // coarsen, or this would be LOD in name only.
        assert!(mip(1600.0 * 2.5) >= 1);

        // Zoomed in, the same rule keeps the near ground sharp while the
        // rest of the map — well outside a 10° cone — coarsens.
        r.camera_dist(Fixed::from_int(60));
        assert_eq!(r.mip_scan_dist().map(|d| d as i32), Some(60));
    }

    /// **The mask is kept at the grain the map thinks in.** A column
    /// map's cell is `SCALE` columns of texture and the fog costs
    /// `span²` per pass; a cubic hull's cell IS a voxel, and a crewman
    /// leaning round a doorframe wants every one of them.
    #[test]
    fn a_column_map_fogs_by_the_cell_and_a_hull_by_the_voxel() {
        let stand = |cubic: bool| {
            let mut r = MapRender::new(BTreeMap::new(), None, &[]);
            let grid = if cubic {
                let g = r.grid_spawn_cubic(0, 0, 0);
                r.voxel_fill_in(g, 0, 0, 0, 8, 8, 0, 0x8055_5f6b);
                Some(g)
            } else {
                r.voxel_fill(0, 0, 0, 8, 8, 0, 0x8055_5f6b); // auto world grid
                None
            };
            r.vision_config(360, 6, 6);
            r.deck_clip(0, 3);

            let mut world = World::new(0);
            let arch = world.register_archetype(&["deck"]);
            let e = world.spawn(arch);
            world.set_position(
                e,
                FixedVec3::new(Fixed::from_int(4), Fixed::from_int(4), Fixed::ZERO),
            );
            if let Some(g) = grid {
                r.entity_set_grid(e.0 as i64, g);
            }
            r.vision_observer(e.0 as i64);
            r.build_instances(&world);
            let _ = r.update_fow(0.016);
            r.fow.as_ref().expect("the fog armed").cell_span()
        };

        assert_eq!(stand(false), SCALE as u32, "a column map fogs by the cell");
        assert_eq!(stand(true), 1, "a hull fogs by the voxel");
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

    /// `set_sprite_facing` picks the mode actors are posed with, and the
    /// default is what every map before `host_api` 29 was drawn against.
    #[test]
    fn sprite_facing_picks_the_billboard_mode() {
        let mut r = MapRender::new(hero_assets(), Some(0), &[]);
        assert_eq!(
            r.sprite_mode(),
            BillboardMode::Cylindrical,
            "eye by default"
        );

        r.set_sprite_facing(true);
        assert_eq!(r.sprite_mode(), BillboardMode::CylindricalViewPlane);

        r.set_sprite_facing(false);
        assert_eq!(r.sprite_mode(), BillboardMode::Cylindrical, "and back");
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

    /// **One animation must not resize and lift the rest.** A death sprawl
    /// reaches past a standing figure at both ends; folded into the scale
    /// box it shrinks the idle pose and grounds it below its own feet, so
    /// a character starts standing smaller and floating the moment its art
    /// gains an animation that lies down.
    ///
    /// The first state is the reference, as `model_character`'s first clip
    /// is. A bigger pose still draws bigger — that is what one scale for
    /// every state means.
    #[test]
    fn the_first_state_sizes_and_grounds_the_rest() {
        let mut a = BTreeMap::new();
        for side in ACTOR_SIDES {
            // Opaque only in `z 2..=4` of an 8-tall frame: three voxels
            // tall, feet two off the frame's own bottom.
            a.insert(format!("char/hero/idle/{side}.gif"), padded_gif());
            // …and a pose that reaches both lower and higher, as a sprawl
            // does: fully opaque, `z 0..=5`.
            a.insert(format!("char/hero/death/{side}.gif"), tiny_gif());
        }
        let mut r = MapRender::new(a, Some(0), &[]);
        r.model_actor(
            "char/hero",
            &["idle".to_string(), "death".to_string()],
            Fixed::from_int(2),
        );

        let target = 2.0 * SCALE;
        for (state, clips) in &r.actors[0].states {
            for clip in clips {
                assert!(
                    (f64::from(clip.voxel_world_size) - target / 3.0).abs() < 1e-4,
                    "{state}: scaled by the sprawl, not by the idle's three voxels",
                );
                assert!(
                    (clip.pivot[2] - 2.0).abs() < 1e-4,
                    "{state}: grounded below the idle's own feet",
                );
            }
        }
    }

    /// **A gesture with an end must not loop.** A death pose stays a
    /// corpse and a swing does not restart mid-play; a cast is the same
    /// shape of thing, and left looping it leaves a caster winding up
    /// forever after the spell has gone. So is `decay`: what it holds is
    /// the skeleton the body has become, and a looping one would rot it
    /// back into a carcass every two seconds.
    #[test]
    fn a_gesture_holds_its_last_frame_and_a_cycle_repeats() {
        let mut a = BTreeMap::new();
        for state in ["idle", "run", "cast", "death", "decay"] {
            for side in ACTOR_SIDES {
                a.insert(format!("char/hero/{state}/{side}.gif"), tiny_gif());
            }
        }
        let mut r = MapRender::new(a, Some(0), &[]);
        r.model_actor(
            "char/hero",
            &[
                "idle".to_string(),
                "run".to_string(),
                "cast".to_string(),
                "death".to_string(),
                "decay".to_string(),
            ],
            Fixed::from_int(2),
        );

        let modes: Vec<(&str, bool)> = r.actors[0]
            .states
            .iter()
            .map(|(name, clips)| (*name, clips[0].loop_mode == LoopMode::Once))
            .collect();
        assert_eq!(
            modes,
            vec![
                ("idle", false),
                ("run", false),
                ("cast", true),
                ("death", true),
                ("decay", true),
            ],
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

    /// A one-bone `.rkc`: a 2x2x4 box (so its z envelope is exactly 4 model
    /// voxels, pivot-centred) rigged to a root bone, with two named clips.
    fn tiny_rkc() -> Vec<u8> {
        use roxlap_formats::character::{Attachment, Bone, Clip};
        use roxlap_formats::kfa::{Hinge, Point3, Seq};
        use roxlap_formats::xform::BoneXform;

        let zero = Point3 {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let axis = Point3 {
            x: 0.0,
            y: 0.0,
            z: 1.0,
        };
        let clip = |name: &str| Clip {
            name: name.to_string(),
            data: ClipData::Skeletal {
                frmval: vec![vec![BoneXform::IDENTITY]],
                seq: vec![Seq { tim: 0, frm: 0 }],
            },
        };
        character::serialize(&Character {
            name: "tiny".to_string(),
            root: [0.0; 3],
            meshes: vec![Kv6::solid_box(2, 2, 4, VoxColor(0x8080_8080))],
            bones: vec![Bone {
                name: "body".to_string(),
                attachments: vec![Attachment::static_mesh(0)],
                hinge: Hinge {
                    parent: -1,
                    p: [zero, zero],
                    v: [axis, axis],
                    vmin: 0,
                    vmax: 0,
                    htype: 0,
                    filler: [0; 7],
                },
            }],
            clips: vec![clip("hover"), clip("attack")],
            voxel_clips: Vec::new(),
            extra_chunks: Vec::new(),
        })
    }

    /// The whole `.rkc` path with no renderer: parse, auto-scale to the asked
    /// height, bind, name a clip, and produce the seated character target the
    /// frame hands to `update_characters`.
    #[test]
    fn model_character_scales_binds_and_targets() {
        let assets = BTreeMap::from([("mobs/moth.rkc".to_string(), tiny_rkc())]);
        let mut r = MapRender::new(assets, Some(0), &[]);
        let model = r.model_character("mobs/moth.rkc", Fixed::from_int(2));
        assert!(model >= 0, "character model registered");
        assert_eq!(r.characters.len(), 1);
        // 2 cells x SCALE(16) over the 4-voxel envelope → 8 world voxels per
        // model voxel, and the feet sit `2 * 8` below the (centred) root.
        assert!(
            (r.characters[0].scale - 8.0).abs() < 1e-3,
            "sized by the first clip's envelope, got {}",
            r.characters[0].scale
        );
        assert!(
            (r.characters[0].lift - 16.0).abs() < 1e-3,
            "feet offset from the root anchor, got {}",
            r.characters[0].lift
        );

        let mut world = World::new(0);
        let arch = world.register_archetype(&["hp"]);
        let e = world.spawn(arch);
        world.set_position(
            e,
            FixedVec3::new(Fixed::from_int(2), Fixed::from_int(3), Fixed::ZERO),
        );
        r.entity_set_model(e.0 as i64, model);
        assert_eq!(
            r.entity_chars.get(&e).map(|c| c.clip),
            Some(0),
            "binding starts on the first clip"
        );
        r.entity_set_anim(e.0 as i64, "attack");
        r.entity_set_facing(e.0 as i64, Fixed::ZERO);
        assert_eq!(
            r.entity_chars.get(&e).map(|c| c.clip),
            Some(1),
            "a clip is selected by its `.rkc` name"
        );
        // An unknown state leaves the current clip playing (warned once).
        r.entity_set_anim(e.0 as i64, "moonwalk");
        assert_eq!(r.entity_chars.get(&e).map(|c| c.clip), Some(1));

        // The frame produces one character target, not a sprite instance, and
        // seats it feet-on-the-floor: the root anchor rides `lift` above it.
        r.build_instances(&world);
        assert_eq!(r.char_targets.len(), 1, "one character target");
        assert_eq!(r.char_targets[0].0, e);
        assert!(
            (f64::from(r.char_targets[0].2[2]) - (GROUND_Z - 16.0)).abs() < 1e-3,
            "seated by the measured lift, got {}",
            r.char_targets[0].2[2]
        );
        assert!(
            r.sprites.instances.is_empty(),
            "a character is not a static sprite instance"
        );

        // `model_drop` nudges it down the same way it does an actor.
        r.model_drop(model, Fixed::from_int(1));
        r.build_instances(&world);
        assert!(
            (f64::from(r.char_targets[0].2[2]) - (GROUND_Z - 16.0 + SCALE)).abs() < 1e-3,
            "model_drop lowers the character by a cell"
        );
    }

    /// `height_cells <= 0` means "the artist's scale": one model voxel per
    /// world voxel, whatever the rig was authored at.
    #[test]
    fn model_character_zero_height_keeps_native_scale() {
        let assets = BTreeMap::from([("mobs/moth.rkc".to_string(), tiny_rkc())]);
        let mut r = MapRender::new(assets, None, &[]);
        assert!(r.model_character("mobs/moth.rkc", Fixed::ZERO) >= 0);
        assert!((r.characters[0].scale - 1.0).abs() < 1e-6, "native scale");
        assert!(
            (r.characters[0].lift - 2.0).abs() < 1e-6,
            "feet 2 voxels down"
        );
    }

    /// A world transform carries the scale in its basis lengths and yaws the
    /// geometry about the vertical axis (the model's own +z stays world +z).
    #[test]
    fn character_transform_carries_scale_and_yaw() {
        let assets = BTreeMap::from([("mobs/moth.rkc".to_string(), tiny_rkc())]);
        let mut r = MapRender::new(assets, None, &[]);
        r.model_character("mobs/moth.rkc", Fixed::from_int(2));
        let xf = r.characters[0].transform([1.0, 2.0, 3.0], std::f64::consts::FRAC_PI_2);
        let close = |got: [f32; 3], want: [f32; 3]| (0..3).all(|a| (got[a] - want[a]).abs() < 1e-3);
        assert!(close(xf.pos, [1.0, 2.0, 3.0]), "seated where asked");
        assert!(
            close(xf.right, [0.0, 8.0, 0.0]),
            "a quarter turn puts local +x on world +y, scaled: {:?}",
            xf.right
        );
        assert!(
            close(xf.forward, [0.0, 0.0, 8.0]),
            "z-down stays z-down, scaled: {:?}",
            xf.forward
        );
    }

    // --- grid-pose smoothing (docs/plans/ship-physics.md §4) --------------

    /// One tick at 30 Hz, in seconds — what `set_tick_hz(30)` declares.
    const TICK: f64 = 1.0 / 30.0;

    /// The drawn pose of a script grid.
    fn drawn(r: &MapRender, g: i64) -> (DVec3, DQuat) {
        let id = r.grid_id(g).expect("live grid");
        let t = &r.scene.grid(id).expect("grid").transform;
        (t.origin, t.rotation)
    }

    /// Turn `g` about +z by `angle` — the one-line pose write the ship's
    /// `step_ship` makes every tick.
    fn spin(r: &mut MapRender, g: i64, angle: f64) {
        r.grid_orient(
            g,
            FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::from_int(1)),
            Fixed::from_f64(angle),
        );
    }

    /// A map that declared a tick rate gets its grid poses EASED: the write
    /// itself changes nothing on screen, and the drawn pose arrives over
    /// exactly one tick's worth of frames. Without this a 30 Hz hull is 30
    /// distinct poses a second on a 60+ Hz display — the judder every rider
    /// inherits rigidly.
    #[test]
    fn a_grid_pose_arrives_over_one_tick() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        r.set_tick_hz(30);

        spin(&mut r, g, 1.0);
        let turn = |r: &MapRender| (drawn(r, g).1 * DVec3::Y).angle_between(DVec3::Y);
        assert!(
            turn(&r) < 1e-9,
            "the write alone must not move the drawn pose: {}",
            turn(&r)
        );

        // Half a tick in, half turned — the frame the old code could not draw.
        r.advance_grid_poses(TICK / 2.0);
        let half = turn(&r);
        assert!(
            (half - 0.5).abs() < 0.02,
            "half a tick is half the turn, got {half}"
        );

        // The rest of the tick lands it, and it stays landed.
        r.advance_grid_poses(TICK / 2.0);
        assert!((turn(&r) - 1.0).abs() < 1e-9, "arrived after one tick");
        r.advance_grid_poses(TICK);
        assert!((turn(&r) - 1.0).abs() < 1e-9, "and does not drift past it");
    }

    /// The invariant the whole design rests on: mid-interpolation a rider is
    /// seated through the pose that is ON SCREEN, not the tick-exact one the
    /// sim asked for. Compose it against the target instead and the crew
    /// shears across a deck that has not turned that far yet — the one artifact
    /// smoothing must not introduce.
    #[test]
    fn a_rider_is_seated_through_the_pose_that_is_drawn() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        r.set_tick_hz(30);
        let model = r.model_box(2, 2, 2, 0x8055_5555);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let rider = world.spawn(arch);
        let p = FixedVec3::new(Fixed::from_int(9), Fixed::from_int(3), Fixed::ZERO);
        world.set_position(rider, p);
        r.entity_set_model(rider.0 as i64, model);
        r.entity_set_grid(rider.0 as i64, g);

        spin(&mut r, g, 1.0);
        r.advance_grid_poses(TICK / 2.0);
        r.build_instances(&world);

        let local = entity_world_of_in(true, p); // cubic grid, cubic z
        let (origin, rotation) = drawn(&r, g);
        let seat = rotation * local + origin;
        let (target_o, target_r) = {
            let id = r.grid_id(g).expect("live grid");
            r.grid_anchors[&id].pose.curr
        };
        let target_seat = target_r * local + target_o;
        assert!(
            seat.distance(target_seat) > 1.0,
            "the test is only decisive while the two frames differ"
        );

        let at = |w: DVec3| {
            r.sprites.instances.iter().any(|i| {
                i.model != HIGHLIGHT_MODEL
                    && (f64::from(i.pos[0]) - w.x).abs() < 0.01
                    && (f64::from(i.pos[1]) - w.y).abs() < 0.01
            })
        };
        assert!(at(seat), "the rider sits on the deck as drawn");
        assert!(
            !at(target_seat),
            "and NOT where the deck will be at the end of the tick"
        );
    }

    /// A camera following a rider re-composes its focus every frame. Left as
    /// the tick composed it, the focus lags the smoothed hull and the whole
    /// ship slides across the screen — worse than the judder it replaced.
    #[test]
    fn a_followed_focus_tracks_the_drawn_hull() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        r.set_tick_hz(30);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let rider = world.spawn(arch);
        let p = FixedVec3::new(Fixed::from_int(9), Fixed::from_int(3), Fixed::ZERO);
        world.set_position(rider, p);
        r.entity_set_grid(rider.0 as i64, g);

        // The tick: turn the hull, then aim at the rider (the order
        // `follow_camera` uses — `step_ship` runs first).
        spin(&mut r, g, 1.0);
        r.camera_focus_entity(rider.0 as i64, p);
        let stale = r.camera.center;

        r.advance_grid_poses(TICK / 2.0);
        // `to_roxlap` puts the eye `dist` behind the focus along `forward`, so
        // the focus is recoverable from the camera the renderer is handed.
        let cam = r.camera();
        let fwd = DVec3::from_array(cam.forward);
        let focus = DVec3::from_array(cam.pos) + fwd * r.camera.dist;
        let (origin, rotation) = drawn(&r, g);
        let seat = rotation * entity_world_of_in(true, p) + origin;
        assert!(
            focus.distance(seat) < 1e-6,
            "the focus is composed through the drawn hull: {focus:?} vs {seat:?}"
        );
        assert!(
            focus.distance(stale) > 1.0,
            "and that is not where the tick left it"
        );
    }

    /// Seat one body, one model, no grid: the fixture the rider-smoothing
    /// tests share. Returns the render, the world and the entity.
    fn walker(hz: Option<u32>, at: FixedVec3) -> (MapRender, World, EntityId) {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        if let Some(hz) = hz {
            r.set_tick_hz(hz);
        }
        let model = r.model_box(2, 2, 2, 0x8055_5555);
        let mut world = World::new(0);
        let arch = world.register_archetype(&["walk"]);
        let body = world.spawn(arch);
        world.set_position(body, at);
        r.entity_set_model(body.0 as i64, model);
        (r, world, body)
    }

    /// Where the one body in a `walker` map is drawn, along x.
    fn walker_x(r: &MapRender) -> f64 {
        let i = r
            .sprites
            .instances
            .iter()
            .find(|i| i.model != HIGHLIGHT_MODEL)
            .expect("the body is drawn");
        f64::from(i.pos[0])
    }

    /// The rider half of the same smoothing (§4.3, the S-6 slice): a body
    /// that WALKS is drawn between the ticks it steps on.
    ///
    /// Smoothing the hull alone was only ever half the fix, and the missing
    /// half stayed invisible while the camera stepped along with the body.
    /// A camera that eases every frame turns it into the followed body
    /// shaking against a world sliding smoothly behind it.
    #[test]
    fn a_walking_body_is_drawn_between_the_ticks_it_steps_on() {
        let from = FixedVec3::new(Fixed::from_int(4), Fixed::ZERO, Fixed::ZERO);
        let to = FixedVec3::new(Fixed::from_int(5), Fixed::ZERO, Fixed::ZERO);
        let (mut r, mut world, body) = walker(Some(30), from);

        // The frame it appears on: drawn where it stands. A spawn must not
        // ease in from wherever the last holder of that id happened to be.
        r.build_instances(&world);
        let (a, b) = (r.entity_world_of(from).x, r.entity_world_of(to).x);
        assert!(
            (walker_x(&r) - a).abs() < 1e-9,
            "a fresh body is drawn where it stands"
        );

        // One tick of walking. The frame it lands on still draws the old
        // spot: that is where the body was when the new position arrived.
        world.set_position(body, to);
        r.build_instances(&world);
        assert!(
            (walker_x(&r) - a).abs() < 1e-9,
            "the tick lands, and the picture starts from what was on screen"
        );

        // Half a tick later it is half way, and provably neither end.
        r.advance_entity_tracks(TICK / 2.0);
        r.build_instances(&world);
        let mid = walker_x(&r);
        assert!(
            mid > a.min(b) && mid < a.max(b),
            "drawn between the two ticks: {mid} not inside ({a}, {b})"
        );
        assert!(
            (mid - (a + b) / 2.0).abs() < 0.5,
            "and about half way across: {mid}"
        );

        // …and it arrives, rather than trailing for ever.
        r.advance_entity_tracks(TICK);
        r.build_instances(&world);
        assert!((walker_x(&r) - b).abs() < 1e-9, "arrived by the tick after");
    }

    /// A body that was PUT somewhere — a teleport, a spawn onto a reused id,
    /// an attach that rebases its local position — lands whole. Easing it
    /// would draw a streak through places it was never in.
    #[test]
    fn a_body_that_is_put_somewhere_lands_whole() {
        let from = FixedVec3::new(Fixed::from_int(4), Fixed::ZERO, Fixed::ZERO);
        let (mut r, mut world, body) = walker(Some(30), from);
        r.build_instances(&world);

        // Well past POSE_SNAP_DIST (two cells).
        let far = FixedVec3::new(Fixed::from_int(40), Fixed::ZERO, Fixed::ZERO);
        world.set_position(body, far);
        r.build_instances(&world);
        assert!(
            (walker_x(&r) - r.entity_world_of(far).x).abs() < 1e-9,
            "a long move lands immediately, got {}",
            walker_x(&r)
        );
    }

    /// A map that never declared a tick rate places bodies whole, exactly as
    /// it did before smoothing existed. This is what keeps every turn-based
    /// map — and every host test that moves an entity and reads it back on
    /// the next line — byte for byte unmoved.
    #[test]
    fn a_map_with_no_tick_rate_places_bodies_whole() {
        let from = FixedVec3::new(Fixed::from_int(4), Fixed::ZERO, Fixed::ZERO);
        let to = FixedVec3::new(Fixed::from_int(5), Fixed::ZERO, Fixed::ZERO);
        let (mut r, mut world, body) = walker(None, from);
        r.build_instances(&world);

        world.set_position(body, to);
        r.build_instances(&world);
        assert!(
            (walker_x(&r) - r.entity_world_of(to).x).abs() < 1e-9,
            "with no tick to interpolate across, the move lands at once"
        );
    }

    /// A selection marker is drawn through the same smoothed position as the
    /// body it rings.
    ///
    /// Left on the tick-exact point it LEADS that body by up to a tick, and
    /// a ring sliding around underneath the thing it marks is the judder
    /// back in a smaller place — which is exactly how it was found.
    #[test]
    fn a_selection_marker_rides_the_body_it_rings() {
        let from = FixedVec3::new(Fixed::from_int(4), Fixed::ZERO, Fixed::ZERO);
        let to = FixedVec3::new(Fixed::from_int(5), Fixed::ZERO, Fixed::ZERO);
        let (mut r, mut world, body) = walker(Some(30), from);
        r.highlight(body.0 as i64);
        r.build_instances(&world);

        world.set_position(body, to);
        r.build_instances(&world);
        r.advance_entity_tracks(TICK / 2.0);
        r.build_instances(&world);

        let ring = f64::from(
            r.sprites
                .instances
                .iter()
                .find(|i| i.model == HIGHLIGHT_MODEL)
                .expect("the body is ringed")
                .pos[0],
        );
        assert!(
            (ring - walker_x(&r)).abs() < 1e-9,
            "the ring sits on the body it marks: {ring} vs {}",
            walker_x(&r)
        );
        assert!(
            (ring - r.entity_world_of(to).x).abs() > 1e-6,
            "and not on the point the tick left, which is what led it"
        );
    }

    /// A camera following a body composes its focus through where that body
    /// is DRAWN. Left on the tick-exact point while the body eases, the body
    /// shakes against a smoothly sliding world — §4.4's trap, one frame in
    /// from the hull.
    #[test]
    fn a_followed_focus_tracks_the_drawn_body() {
        let from = FixedVec3::new(Fixed::from_int(4), Fixed::ZERO, Fixed::ZERO);
        let to = FixedVec3::new(Fixed::from_int(5), Fixed::ZERO, Fixed::ZERO);
        let (mut r, mut world, body) = walker(Some(30), from);
        r.build_instances(&world);

        // The tick: the body steps, then the map aims at it.
        world.set_position(body, to);
        r.build_instances(&world);
        r.camera_focus_entity(body.0 as i64, to);
        let stale = r.camera.center;

        r.advance_entity_tracks(TICK / 2.0);
        r.build_instances(&world);

        // `to_roxlap` puts the eye `dist` behind the focus along `forward`,
        // so the focus is recoverable from the camera the renderer is handed.
        let cam = r.camera();
        let fwd = DVec3::from_array(cam.forward);
        let focus = DVec3::from_array(cam.pos) + fwd * r.camera.dist;
        let (a, b) = (r.entity_world_of(from).x, r.entity_world_of(to).x);
        assert!(
            focus.x > a.min(b) && focus.x < a.max(b),
            "the focus rides the drawn body: {} not inside ({a}, {b})",
            focus.x
        );
        assert!(
            (focus.x - walker_x(&r)).abs() < 1e-6,
            "and it is exactly where the body is drawn"
        );
        assert!(
            (focus.x - stale.x).abs() > 1e-6,
            "which is not where the tick left it"
        );
    }

    /// A pose that JUMPS — a dock snap, a re-authored frame — lands whole.
    /// Easing it would smear a deliberate discontinuity across a tick.
    #[test]
    fn a_re_authored_pose_snaps() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);
        r.set_tick_hz(30);

        // Well past POSE_SNAP_DIST (two cells).
        r.grid_move(
            g,
            FixedVec3::new(Fixed::from_int(40), Fixed::ZERO, Fixed::ZERO),
        );
        assert!(
            (drawn(&r, g).0.x + 40.0 * SCALE).abs() < 1e-9,
            "a long move lands immediately, got {:?}",
            drawn(&r, g).0
        );

        // …and so does a rotation nobody could have integrated in one tick.
        spin(&mut r, g, std::f64::consts::PI);
        let turned = drawn(&r, g).1 * DVec3::Y;
        assert!(
            (turned + DVec3::Y).length() < 1e-9,
            "a half-turn lands immediately, got {turned:?}"
        );
    }

    /// A map with no declared tick rate — every turn-based map, and every
    /// host test that poses a grid and reads it back — is untouched: there is
    /// no next pose to be on the way to, so poses land as they always did.
    #[test]
    fn a_map_without_a_tick_rate_poses_whole() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let g = r.grid_spawn_cubic(0, 0, 0);

        spin(&mut r, g, 1.0);
        let turn = (drawn(&r, g).1 * DVec3::Y).angle_between(DVec3::Y);
        assert!((turn - 1.0).abs() < 1e-9, "landed whole, got {turn}");
        r.advance_grid_poses(TICK);
        let after = (drawn(&r, g).1 * DVec3::Y).angle_between(DVec3::Y);
        assert!((after - 1.0).abs() < 1e-9, "and the frame pass is inert");
    }

    #[test]
    fn model_character_missing_or_broken_asset_is_minus_one() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        assert_eq!(
            r.model_character("mobs/moth.rkc", Fixed::from_int(2)),
            -1,
            "a missing asset aborts the character model"
        );
        let assets = BTreeMap::from([("mobs/moth.rkc".to_string(), b"not an rkc".to_vec())]);
        let mut r = MapRender::new(assets, None, &[]);
        assert_eq!(
            r.model_character("mobs/moth.rkc", Fixed::from_int(2)),
            -1,
            "a malformed .rkc aborts the character model"
        );
    }
    /// **A relief owns its cell's column.**
    ///
    /// Painting one over whatever was there left every voxel the new
    /// surface did not reach exactly as it was: the old shape, in the old
    /// colour, standing through the new one. Sculpt a hill, lower it, then
    /// repaint, and the leftovers of the taller version are still there --
    /// which reads as the paint not having landed, and reads that way only
    /// where the ground has been shaped, because flat ground has no old
    /// shape to leave behind.
    #[test]
    fn a_relief_leaves_nothing_of_the_shape_before_it() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(1, 1, image::Rgba([200, 100, 50, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode a tile");
        let mut assets = BTreeMap::new();
        assets.insert("t.png".to_string(), png);
        let mut r = MapRender::new(assets, None, &[]);
        let tile = r.tile("t.png");
        assert!(tile >= 0, "the tile loaded");
        let tall = 20;
        let flat = [0i64; 256];
        let raised = [tall; 256];

        // A cell painted tall, then repainted low: nothing of the tall
        // version may still be standing.
        r.tile_relief(4, 4, -1, tall, &raised, tile);
        let column = |r: &MapRender, z: i64| {
            let world = r.world_grid.expect("a grid was painted");
            let (lo, _) = sim_box_to_world(4, 4, z, 4, 4, z);
            r.scene
                .grid(world)
                .and_then(|g| g.voxel_color(lo))
                .is_some()
        };
        assert!(column(&r, tall), "the tall version was painted");

        r.tile_relief(4, 4, -1, 0, &flat, tile);
        assert!(column(&r, 0), "the low version is painted");
        for z in 1..=tall {
            assert!(
                !column(&r, z),
                "a voxel of the taller version is still standing at z {z}",
            );
        }
    }

    /// **A cursor has to reach ground dug below the datum.**
    ///
    /// The march stopped at the datum plane, so a hollow — ground the
    /// datum sits *above* — was never met, and the pick fell back to the
    /// plane: a point in mid-air over the hollow being pointed into.
    /// Every brush then worked several cells short of where it was aimed,
    /// which reads as "it paints hills and not hollows".
    #[test]
    fn the_cursor_reaches_into_a_hollow() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        // Flat ground with a wide pit dug well below the datum.
        for y in 0..16 {
            for x in 0..16 {
                r.voxel_fill(x, y, -1, x, y, 0, 0x8040_4040);
            }
        }
        let floor: i64 = -40;
        for y in 2..11 {
            for x in 2..11 {
                r.voxel_clear(x, y, -1);
                r.voxel_fill(x, y, floor - 1, x, y, floor, 0x8040_4040);
            }
        }

        // A slanted ray, the way a camera looks: it crosses the datum
        // inside the pit's mouth and goes on to the floor. The two answers
        // are cells apart, which is the whole point -- a vertical ray
        // cannot tell them apart and so cannot test this.
        let target = world_of(FixedVec3::new(
            Fixed::from_int(6),
            Fixed::from_int(6),
            #[allow(clippy::cast_possible_truncation)]
            Fixed::from_int(floor as i32),
        ));
        let dir = DVec3::new(1.0, 0.0, 1.0).normalize();
        let origin = target - dir * 400.0;
        let (x, y) = r.ground_sim(origin, dir).expect("the ray meets ground");
        let cell = |v: f64| (v + 0.5).floor() as i64;
        assert_eq!(
            (cell(x), cell(y)),
            (6, 6),
            "the pick landed at {x},{y}, not in the pit it was aimed at",
        );
    }

    /// **The pick and the placement must agree about where a cell is.**
    ///
    /// `world_of` seats a sim coordinate at the centre of its cell; the
    /// inverse the cursor came back through dropped that half cell, so
    /// every pick on a column map landed half a cell off in x and y — and
    /// the nearest-cell rounding after it then landed a WHOLE cell off,
    /// because half plus half is one. Invisible while a pick only had to
    /// name a cell to walk to, and glaring the moment a ghost was drawn
    /// under the pointer.
    #[test]
    fn a_picked_point_is_the_point_that_was_picked() {
        for (x, y) in [(0.0, 0.0), (5.0, 2.0), (-3.0, 11.0), (0.5, 0.5)] {
            let p = FixedVec3::new(Fixed::from_f64(x), Fixed::from_f64(y), Fixed::ZERO);
            let (bx, by) = sim_of(world_of(p));
            assert!(
                (bx - x).abs() < 1e-9 && (by - y).abs() < 1e-9,
                "{x},{y} came back as {bx},{by}",
            );
        }
    }

    /// …and the rounding built on it names the cell the point is in, not
    /// its neighbour. This is the half that actually moved the terrain
    /// under the cursor.
    #[test]
    fn a_point_inside_a_cell_names_that_cell() {
        let cell_of = |p: FixedVec3| {
            let (sx, sy) = sim_of(world_of(p));
            ((sx + 0.5).floor() as i64, (sy + 0.5).floor() as i64)
        };
        let at = |x: f64, y: f64| {
            cell_of(FixedVec3::new(
                Fixed::from_f64(x),
                Fixed::from_f64(y),
                Fixed::ZERO,
            ))
        };
        assert_eq!(at(7.0, 3.0), (7, 3), "dead centre");
        assert_eq!(at(7.4, 3.4), (7, 3), "still inside it");
        assert_eq!(at(7.6, 3.6), (8, 4), "and over the line is the next one");
    }

    /// **A preview that stood somewhere else would be worse than none.**
    /// The whole promise of a ghost is that the prop lands where the ghost
    /// stood, so it is seated through the same pivot drop an entity's
    /// sprite is -- and this pins the two together rather than trusting
    /// two copies of the arithmetic to stay equal.
    #[test]
    fn a_ghost_stands_exactly_where_the_prop_will() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let model = r.model_kv6("missing.kv6", 0);
        let p = FixedVec3::new(Fixed::from_int(5), Fixed::from_int(2), Fixed::from_int(3));

        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        world.set_position(e, p);
        r.entity_set_model(e.0 as i64, model);
        r.build_instances(&world);
        let placed = r
            .sprites
            .instances
            .iter()
            .find(|i| i.model != HIGHLIGHT_MODEL)
            .expect("the entity got a sprite")
            .pos;

        r.ghost_model(model, p, Fixed::ZERO, 128);
        let (_, seat, _, alpha) = r.ghosts.targets[0];
        for (a, b) in [seat.x as f32, seat.y as f32, seat.z as f32]
            .into_iter()
            .zip(placed)
        {
            assert!(
                (a - b).abs() < 1e-4,
                "the ghost and the prop disagree about where they stand: {a} against {b}",
            );
        }
        assert_eq!(alpha, 128);
    }

    /// A sim yaw reads mirrored on screen (`world_of` mirrors x), so a
    /// ghost that took the sim yaw straight would face the wrong way --
    /// visible on anything that is not symmetric, which is most props.
    #[test]
    fn a_ghost_turns_the_way_the_prop_turns() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let model = r.model_kv6("missing.kv6", 0);
        let yaw = Fixed::from_f64(0.75);
        r.ghost_model(model, FixedVec3::ZERO, yaw, 255);
        let (_, _, world_yaw, _) = r.ghosts.targets[0];
        assert!((world_yaw - facing_to_world_yaw(0.75)).abs() < 1e-9);
    }

    /// Immediate mode: what a frame asked for is what a frame gets, and a
    /// **A lift moves the drawing and nothing else.** It is what a hover
    /// or a bob is made of, so it may be driven per client per frame --
    /// which is only safe because the body goes on standing, walking and
    /// being reached exactly where it did.
    #[test]
    fn a_lift_floats_a_body_without_moving_it() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let model = r.model_kv6("missing.kv6", 0);
        let mut world = World::new(0);
        let arch = world.register_archetype(&["hp"]);
        let e = world.spawn(arch);
        let at = FixedVec3::new(Fixed::from_int(4), Fixed::from_int(4), Fixed::ZERO);
        world.set_position(e, at);
        r.entity_set_model(e.0 as i64, model);

        let seat = |r: &mut MapRender| {
            r.build_instances(&world);
            r.sprites
                .instances
                .iter()
                .find(|i| i.model != HIGHLIGHT_MODEL)
                .expect("the body got a sprite")
                .pos[2]
        };
        let grounded = seat(&mut r);

        // Half a cell up. World +z is down, so drawn higher is a SMALLER z.
        r.entity_set_lift(e.0 as i64, Fixed::from_ratio(1, 2));
        let floating = seat(&mut r);
        assert!(
            (f64::from(grounded - floating) - SCALE * 0.5).abs() < 1e-3,
            "drawn at {floating} rather than half a cell above {grounded}",
        );
        // …and it still stands where it stood.
        assert_eq!(world.position(e), Some(at));

        // Back to nothing, and the entry goes with it rather than piling
        // up one per body that never leaves the ground.
        r.entity_set_lift(e.0 as i64, Fixed::ZERO);
        assert!(r.entity_lift.is_empty());
        assert!((seat(&mut r) - grounded).abs() < 1e-3);
    }

    /// **A body is clicked where it is drawn, not where it stands.**
    /// Picking measured the cursor's GROUND point against an entity's
    /// seat, so only the feet answered: a figure drawn more than a cell
    /// tall stands entirely outside the radius, and aiming at the chest --
    /// which is what a player aims at -- missed.
    #[test]
    fn a_body_is_picked_by_its_art_and_not_only_by_its_feet() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        // Two cells tall, so the top of it is well clear of its own cell.
        let tall = r.model_box(8, 8, (SCALE * 2.0) as i64, 0x80a8_b48c);
        let mut world = World::new(0);
        let arch = world.register_archetype(&[]);
        let unit = world.spawn(arch);
        let at = FixedVec3::new(Fixed::from_int(10), Fixed::from_int(10), Fixed::ZERO);
        world.set_position(unit, at);
        r.entity_set_model(unit.0 as i64, tall);

        // A shallow eye, as this camera is, looking at the body's HEAD --
        // a cell and a half up, which is chest-to-head on a figure this
        // size and nowhere near the cell it stands on.
        let feet = entity_world_of_in(false, at);
        let head = feet - DVec3::Z * (SCALE * 1.5);
        let eye = head + DVec3::new(-260.0, -260.0, -150.0);
        let dir = (head - eye).normalize();

        assert_eq!(
            r.pick(&world, eye, dir).1,
            unit.0 as i64,
            "a click on the body missed it",
        );

        // …and a ray that goes nowhere near it still misses, or the fix
        // would be "pick everything".
        let wide = (head + DVec3::new(0.0, 400.0, 0.0) - eye).normalize();
        assert_eq!(r.pick(&world, eye, wide).1, -1);
    }

    /// The nearest body ALONG THE RAY wins. Two of them overlapping on
    /// screen is the case ground distance answered wrongly: it would hand
    /// back whichever stood nearer the cursor's ground point, which is the
    /// one BEHIND when you are looking down a line of them.
    #[test]
    fn the_body_in_front_is_the_one_picked() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let box_model = r.model_box(8, 8, SCALE as i64, 0x80a8_b48c);
        let mut world = World::new(0);
        let arch = world.register_archetype(&[]);

        let near = world.spawn(arch);
        let far = world.spawn(arch);
        let near_at = FixedVec3::new(Fixed::from_int(10), Fixed::from_int(10), Fixed::ZERO);
        let far_at = FixedVec3::new(Fixed::from_int(12), Fixed::from_int(12), Fixed::ZERO);
        world.set_position(near, near_at);
        world.set_position(far, far_at);
        r.entity_set_model(near.0 as i64, box_model);
        r.entity_set_model(far.0 as i64, box_model);

        // Down the line joining them, from beyond the near one.
        let (a, b) = (
            entity_world_of_in(false, near_at),
            entity_world_of_in(false, far_at),
        );
        let dir = (b - a).normalize();
        let eye = a - dir * 300.0 - DVec3::Z * 200.0;
        let dir = ((a - DVec3::Z * (SCALE * 0.5)) - eye).normalize();
        assert_eq!(r.pick(&world, eye, dir).1, near.0 as i64);
    }

    /// **A map with no fog sees everything, and must say so.** The
    /// question is asked by anything a map draws about a body -- a health
    /// bar, a name -- and answering "hidden" where there is no mask would
    /// blank every one of them on every map that never declared an
    /// observer, which is most of them.
    #[test]
    fn nothing_is_hidden_where_there_is_no_fog() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        assert!(r.fow.is_none(), "no observer was declared");
        for at in [
            FixedVec3::ZERO,
            FixedVec3::new(Fixed::from_int(40), Fixed::from_int(40), Fixed::ZERO),
            FixedVec3::new(-Fixed::from_int(9), Fixed::from_int(900), Fixed::ZERO),
        ] {
            assert!(r.point_visible(at));
        }
    }

    /// map that stops asking stops drawing. The actor preview outlives its
    /// frame on the renderer's side, but the REQUEST does not -- a frame
    /// that asks for nothing is what retires it.
    #[test]
    fn ghosts_last_exactly_as_long_as_they_are_asked_for() {
        let mut r = MapRender::new(hero_assets(), Some(0), &[]);
        let model = r.model_kv6("missing.kv6", 0);
        let actor = r.model_actor("char/hero", &["idle".to_string()], Fixed::from_int(2));
        r.ghost_model(model, FixedVec3::ZERO, Fixed::ZERO, 200);
        r.ghost_model(model, FixedVec3::ZERO, Fixed::ZERO, 200);
        r.ghost_model(actor, FixedVec3::ZERO, Fixed::ZERO, 200);
        assert_eq!(r.ghosts.targets.len(), 2);
        assert!(r.ghosts.actor.is_some());
        r.ghost_clear();
        assert!(r.ghosts.targets.is_empty());
        assert!(r.ghosts.actor.is_none(), "the actor request went too");
    }

    /// An actor previews as itself, on its own path: it is eight cards
    /// picked from the camera's bearing rather than one posed model, so it
    /// cannot ride the sprite instances. A rigged character has no preview
    /// at all -- posing a clip is a question nothing has asked -- and a
    /// magenta box in its place would be a worse answer than nothing.
    #[test]
    fn an_actor_previews_as_an_actor_and_nothing_else_previews_at_all() {
        let mut r = MapRender::new(hero_assets(), Some(0), &[]);
        let actor = r.model_actor("char/hero", &["idle".to_string()], Fixed::from_int(2));
        assert!(actor >= 0, "the actor model registered");

        r.ghost_model(actor, FixedVec3::ZERO, Fixed::ZERO, 255);
        assert_eq!(r.ghosts.actor.map(|a| a.0), Some(0), "the actor model");
        assert!(
            r.ghosts.targets.is_empty(),
            "an actor is not a sprite instance",
        );

        r.ghost_clear();
        r.ghost_model(9999, FixedVec3::ZERO, Fixed::ZERO, 255);
        assert!(r.ghosts.actor.is_none() && r.ghosts.targets.is_empty());
    }

    /// **An actor model may be defined at any time, not only at `init`.**
    /// Registration used to sit behind one latch set on the first rendered
    /// frame, so a model defined later -- which is exactly what a placement
    /// preview does, when a designer first picks a kind -- never got its
    /// clips and drew nothing, silently and for good.
    #[test]
    fn an_actor_defined_after_the_first_frame_still_gets_its_clips() {
        let mut r = MapRender::new(hero_assets(), Some(0), &[]);
        r.model_actor("char/hero", &["idle".to_string()], Fixed::from_int(2));
        assert_eq!(awaiting_clips(&r.actors), vec![0], "nothing registered yet");

        // What the first rendered frame does, without a renderer to do it.
        r.actors[0].registered = Some(Vec::new());
        assert!(awaiting_clips(&r.actors).is_empty());

        // …and hundreds of frames later, a preview asks for a kind's art.
        r.model_actor("char/hero", &["run".to_string()], Fixed::from_int(2));
        assert_eq!(
            awaiting_clips(&r.actors),
            vec![1],
            "the late model is still waiting, and the early one is not asked twice",
        );
    }

    /// A cursor previews one thing, so a second ask in a frame replaces the
    /// first rather than leaving two figures standing.
    #[test]
    fn only_one_actor_is_previewed_at_a_time() {
        let mut r = MapRender::new(hero_assets(), Some(0), &[]);
        let actor = r.model_actor("char/hero", &["idle".to_string()], Fixed::from_int(2));
        r.ghost_model(actor, FixedVec3::ZERO, Fixed::ZERO, 100);
        r.ghost_model(
            actor,
            FixedVec3::new(Fixed::from_int(4), Fixed::ZERO, Fixed::ZERO),
            Fixed::ZERO,
            200,
        );
        let (_, _, _, alpha) = r.ghosts.actor.expect("one preview");
        assert_eq!(alpha, 200, "the last ask wins");
    }

    /// **The same promise the sprite ghost keeps.** A card is anchored at
    /// its bottom centre and a prop through its pivot, so the two are
    /// seated by different arithmetic -- which is exactly why the actor
    /// ghost has to be pinned against a placed actor rather than trusted to
    /// have copied it right.
    #[test]
    fn an_actor_ghost_stands_and_turns_as_the_actor_will() {
        let mut r = MapRender::new(hero_assets(), Some(0), &[]);
        let model = r.model_actor("char/hero", &["idle".to_string()], Fixed::from_int(2));
        let p = FixedVec3::new(Fixed::from_int(5), Fixed::from_int(2), Fixed::from_int(3));
        let yaw = Fixed::from_f64(0.75);

        let mut world = World::new(0);
        let arch = world.register_archetype(&["hp"]);
        let e = world.spawn(arch);
        world.set_position(e, p);
        r.entity_set_model(e.0 as i64, model);
        r.entity_set_facing(e.0 as i64, yaw);
        r.build_instances(&world);
        let (.., placed, placed_yaw, _) = r.actor_targets[0];

        r.ghost_model(model, p, yaw, 128);
        let (ai, seat, ghost_yaw, alpha) = r.ghosts.actor.expect("a preview");
        assert_eq!(ai, 0);
        for (a, b) in [seat.x as f32, seat.y as f32, seat.z as f32]
            .into_iter()
            .zip(placed)
        {
            assert!(
                (a - b).abs() < 1e-4,
                "the ghost and the actor disagree about where they stand: {a} against {b}",
            );
        }
        assert!(
            (ghost_yaw - placed_yaw).abs() < 1e-9,
            "and about which way they look",
        );
        assert_eq!(alpha, 128);
    }

    /// Alpha crosses the wall as a plain number, so it has to be clamped
    /// rather than trusted: a wrapped 256 is a ghost that vanishes.
    #[test]
    fn a_ghosts_alpha_is_clamped_and_not_wrapped() {
        let mut r = MapRender::new(BTreeMap::new(), None, &[]);
        let model = r.model_kv6("missing.kv6", 0);
        r.ghost_model(model, FixedVec3::ZERO, Fixed::ZERO, 4096);
        r.ghost_model(model, FixedVec3::ZERO, Fixed::ZERO, -7);
        assert_eq!(r.ghosts.targets[0].3, 255);
        assert_eq!(r.ghosts.targets[1].3, 0);
    }
}
