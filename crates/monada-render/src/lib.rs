//! monada render bridge (DESIGN.md §3.2).
//!
//! This crate lives on the render side of the sim/render wall: it reads
//! sim state (never writes it) and turns it into renderable data, doing
//! the Q32.32 -> `f64` conversion here so the sim never holds a float
//! pose (DESIGN.md §3.1).
//!
//! M1 slice ([`CircleScene`]): the M0 "100 entities walk in a circle"
//! scenario. The ground is one `roxlap_scene::Grid`; the movers are
//! **sprites** (one cube model per palette colour + an instance each),
//! drawn in a single sprite pass rather than as one grid apiece — 100
//! grids would be 100 opticast passes per frame. Positions interpolate
//! between the last two sim ticks. Picking and the egui HUD come next.
#![forbid(unsafe_code)]
// Render-side float/precision casts are intentional and audited; the
// sim/render wall keeps them off the deterministic path.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::must_use_candidate
)]

use glam::DVec3;
use monada_fixed::FixedVec3;
use roxlap_core::Camera;
use roxlap_formats::kv6::Kv6;
use roxlap_formats::sprite::{Sprite, SPRITE_FLAG_NO_SHADING};
use roxlap_render::{SpriteInstanceDesc, SpriteSet};
use roxlap_scene::{Grid, GridTransform, Scene};

mod camera;

pub use camera::OrbitCamera;

/// World voxels per sim unit. The M0 circle has radius 4..12 sim units,
/// so this spreads it across ~32..96 voxels.
const SCALE: f64 = 8.0;
/// World z of the ground plane's top surface (z grows *downward* in
/// roxlap's voxlap convention, so "up" toward the camera is smaller z).
const GROUND_Z: f64 = 100.0;
/// Mover cube edge, in voxels.
const CUBE: u32 = 10;
/// How far above the ground surface the cube centres float, in voxels
/// (clearly separated so the ground can't occlude them).
const LIFT: f64 = 24.0;
/// Half-extent of the square ground slab, in voxels. The circle spans
/// ~±96, so this keeps it filling most of the board.
const GROUND_HALF: i32 = 140;

/// Per-mover colours, voxlap-packed `0x80RRGGBB` (high byte is
/// *brightness*, not alpha — `0x00…` renders black). One sprite model
/// is built per entry; movers cycle through them so motion is legible.
const PALETTE: [u32; 6] = [
    0x80FF_6B35, // orange
    0x80F7_C548, // yellow
    0x8043_AA8B, // teal
    0x8046_82B4, // steel blue
    0x80B5_4D9C, // magenta
    0x80E8_4855, // red
];

/// Model index of the bright "selected" cube, appended after the
/// palette models.
const HIGHLIGHT_MODEL: usize = PALETTE.len();
/// Model index of the green debug cursor marker.
const CURSOR_MODEL: usize = PALETTE.len() + 1;
/// Max xy distance (world units) from the pick-plane hit to a mover for
/// it to count as picked; beyond this a click deselects.
const PICK_RADIUS: f64 = 14.0;

/// The M1 render scene: a ground grid plus a per-frame sprite set for
/// the movers.
pub struct CircleScene {
    scene: Scene,
    /// Cube models (palette + highlight + cursor) + the live instance
    /// list: instances `0..mover_count` are movers, index `mover_count`
    /// is the debug cursor marker.
    sprites: SpriteSet,
    /// Number of mover instances (the cursor marker sits just past it).
    mover_count: usize,
    center: DVec3,
    /// Currently picked mover instance, if any.
    selected: Option<usize>,
    /// Orbit camera framing the circle; driven by host input.
    pub camera: OrbitCamera,
}

impl CircleScene {
    /// Build the ground grid and `count` mover sprite instances (placed
    /// at the centre until the first [`update`](Self::update)).
    pub fn new(count: usize) -> CircleScene {
        let mut scene = Scene::new();
        let center = DVec3::new(0.0, 0.0, GROUND_Z);
        build_ground(&mut scene, center);

        // One flat-shaded cube model per palette colour, plus a bright
        // "selected" model (index HIGHLIGHT_MODEL) for picking.
        let cube_sprite = |col: u32, edge: u32| {
            let mut sprite = Sprite::axis_aligned(Kv6::solid_cube(edge, col), [0.0, 0.0, 0.0]);
            // Show the raw bright palette colour rather than relying on a
            // lighting bake for this visibility-first slice.
            sprite.flags = SPRITE_FLAG_NO_SHADING;
            sprite
        };
        let mut models: Vec<Sprite> = PALETTE.iter().map(|&col| cube_sprite(col, CUBE)).collect();
        models.push(cube_sprite(0x80FF_FFFF, CUBE + 2)); // HIGHLIGHT_MODEL
        models.push(cube_sprite(0x8000_FF00, CUBE - 4)); // CURSOR_MODEL (green)

        let plane_z = center.z - LIFT;
        let mut instances: Vec<SpriteInstanceDesc> = (0..count)
            .map(|i| SpriteInstanceDesc {
                model: i % PALETTE.len(),
                pos: [center.x as f32, center.y as f32, plane_z as f32],
            })
            .collect();
        // Debug cursor marker (index == count), follows the picking ray.
        instances.push(SpriteInstanceDesc {
            model: CURSOR_MODEL,
            pos: [center.x as f32, center.y as f32, plane_z as f32],
        });
        let sprites = SpriteSet {
            models,
            instances,
            carve_model: None,
        };

        CircleScene {
            scene,
            sprites,
            mover_count: count,
            center,
            selected: None,
            camera: OrbitCamera::framing(center),
        }
    }

    /// Update mover sprite positions to the interpolation of `prev` and
    /// `curr` sim positions at blend factor `alpha` in `[0, 1]`.
    ///
    /// `prev`/`curr` are sim-space [`FixedVec3`] read from the sim; the
    /// Q32.32 -> `f64` conversion and the lerp happen here, on the
    /// render side of the wall.
    pub fn update(&mut self, prev: &[FixedVec3], curr: &[FixedVec3], alpha: f64) {
        let n = self.mover_count.min(prev.len()).min(curr.len());
        for i in 0..n {
            let w = self.mover_world(prev[i], curr[i], alpha);
            let inst = &mut self.sprites.instances[i];
            inst.pos = [w.x as f32, w.y as f32, w.z as f32];
            inst.model = if self.selected == Some(i) {
                HIGHLIGHT_MODEL
            } else {
                i % PALETTE.len()
            };
        }
    }

    /// The horizontal plane the movers sit on (their cube centres). The
    /// movers float [`LIFT`] above the ground, so picking must intersect
    /// *this* plane, not the ground — intersecting the ground plane and
    /// matching xy mis-picks by the movers' height parallax.
    fn pick_plane(&self) -> f64 {
        self.center.z - LIFT
    }

    /// Move the debug cursor marker to where a world-space ray crosses
    /// the mover plane; returns that world point. Call every frame so the
    /// marker tracks the mouse (not only on click).
    pub fn hover(&mut self, origin: DVec3, dir: DVec3) -> Option<DVec3> {
        let hit = ground_hit(origin, dir, self.pick_plane());
        if let Some(h) = hit {
            self.sprites.instances[self.mover_count].pos = [h.x as f32, h.y as f32, h.z as f32];
        }
        hit
    }

    /// Pick the mover nearest to where a world-space ray crosses the
    /// mover plane (tile-style selection — no depth readback; DESIGN.md
    /// §3.2). Updates and returns the selection; a click that lands
    /// farther than [`PICK_RADIUS`] from any mover clears it.
    pub fn pick_ground(&mut self, origin: DVec3, dir: DVec3) -> Option<usize> {
        self.selected =
            ground_hit(origin, dir, self.pick_plane()).and_then(|hit| self.nearest_mover(hit));
        self.selected
    }

    /// Index of the mover whose xy is closest to `hit`, within
    /// [`PICK_RADIUS`].
    fn nearest_mover(&self, hit: DVec3) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for (i, inst) in self.sprites.instances[..self.mover_count]
            .iter()
            .enumerate()
        {
            let dx = f64::from(inst.pos[0]) - hit.x;
            let dy = f64::from(inst.pos[1]) - hit.y;
            let d2 = dx * dx + dy * dy;
            if best.map_or(true, |(_, b)| d2 < b) {
                best = Some((i, d2));
            }
        }
        best.filter(|&(_, d2)| d2 <= PICK_RADIUS * PICK_RADIUS)
            .map(|(i, _)| i)
    }

    /// The currently selected mover, if any.
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// Sim-space `(x, y)` of where a world-space ray crosses the mover
    /// plane — the inverse of [`mover_world`](Self::mover_world)'s xy map.
    /// The host quantises this to fixed-point for a spawn command
    /// (DESIGN.md §3.1, M3): click-to-place in the command-driven demo.
    pub fn ground_sim_xy(&self, origin: DVec3, dir: DVec3) -> Option<(f64, f64)> {
        ground_hit(origin, dir, self.pick_plane()).map(|hit| {
            (
                (hit.x - self.center.x) / SCALE,
                (hit.y - self.center.y) / SCALE,
            )
        })
    }

    /// Interpolated world-space centre of one mover cube: lerp the sim
    /// position in `f64`, scale into world voxels about the centre, and
    /// seat the cube on the ground (cube centre is half an edge above
    /// the surface; smaller z is up).
    fn mover_world(&self, prev: FixedVec3, curr: FixedVec3, alpha: f64) -> DVec3 {
        let x = lerp(prev.x.to_f64(), curr.x.to_f64(), alpha);
        let y = lerp(prev.y.to_f64(), curr.y.to_f64(), alpha);
        let z = lerp(prev.z.to_f64(), curr.z.to_f64(), alpha);
        DVec3::new(
            self.center.x + x * SCALE,
            self.center.y + y * SCALE,
            // Smaller z is "up" (z grows downward): lift above the board.
            self.center.z - LIFT - z * SCALE,
        )
    }

    /// World-space centre of the board and the first few mover cube
    /// positions, for one-shot host debug output when framing looks off.
    pub fn debug_positions(&self) -> (DVec3, Vec<[f32; 3]>) {
        let sample = self
            .sprites
            .instances
            .iter()
            .take(5)
            .map(|inst| inst.pos)
            .collect();
        (self.center, sample)
    }

    /// The mover sprite set, to hand to `SceneRenderer::set_sprites`.
    pub fn sprites(&self) -> &SpriteSet {
        &self.sprites
    }

    /// The renderer needs `&mut Scene` (CPU rebuilds each frame; GPU
    /// tracks dirty chunks).
    pub fn scene_mut(&mut self) -> &mut Scene {
        &mut self.scene
    }

    /// The current camera, as roxlap's `right/down/forward` basis.
    pub fn camera(&self) -> Camera {
        self.camera.to_roxlap()
    }
}

// --- M4 chess board scene --------------------------------------------

/// Board square edge, in world voxels (8 squares -> 128 voxels across).
const SQUARE: f64 = 16.0;
/// World z of the board's top surface (z grows downward in voxlap).
const BOARD_Z: f64 = 100.0;
/// Piece body footprint (x/y), in voxels.
const PIECE_W: u32 = 10;
/// Piece body height by `kind` (pawn, knight, bishop, rook, queen, king),
/// in voxels — the "voxel art bootstrap" of DESIGN.md §6: a cube-stack
/// stand-in until authored KV6 art (slice 5). Taller = stronger piece, so
/// the silhouette alone reads the board.
const PIECE_H: [u32; 6] = [10, 14, 16, 14, 22, 26];
/// Body colour by side (voxlap-packed `0x80RRGGBB`, high byte brightness):
/// ivory white, slate black. No shading so they read without a light bake.
const SIDE_COLOR: [u32; 2] = [0x80F0_EAD8, 0x8028_2C34];
/// Model index of the selected-square highlight slab (after the 12
/// piece models).
const SELECT_MODEL: usize = 12;
/// Model index of the chess hover cursor marker.
const CHESS_CURSOR_MODEL: usize = 13;

/// One piece to draw, as the host reads it from the sim `World`: a board
/// square plus its kind/colour tags. The sim/render bridge for chess —
/// the analogue of [`CircleScene`]'s `&[FixedVec3]`, but chess needs the
/// per-entity tags to pick a model, so the host hands over this richer
/// view instead of bare positions.
#[derive(Clone, Copy, Debug)]
pub struct PieceView {
    pub x: i8,
    pub y: i8,
    /// 0 pawn, 1 knight, 2 bishop, 3 rook, 4 queen, 5 king.
    pub kind: u8,
    /// 0 white, 1 black.
    pub color: u8,
}

/// The M4 chess render scene: an 8×8 checkered board grid plus a sprite
/// per live piece, rebuilt from the sim each frame. Mirrors the method
/// surface the host drives on [`CircleScene`] (`camera`/`sprites`/
/// `scene_mut`/`hover`) so the host can render either through one path.
pub struct ChessScene {
    scene: Scene,
    /// 12 piece models (`kind*2 + color`) + select slab + hover cursor;
    /// instances are rebuilt every [`update`](Self::update) (cheap at ≤32
    /// pieces, and captures just drop an instance).
    sprites: SpriteSet,
    selected: Option<(i8, i8)>,
    /// Where the picking ray last crossed the board plane (cursor marker).
    cursor_world: DVec3,
    /// Orbit camera framing the board; driven by host input.
    pub camera: OrbitCamera,
}

impl Default for ChessScene {
    fn default() -> ChessScene {
        ChessScene::new()
    }
}

impl ChessScene {
    /// Build the board grid and the fixed piece-model set (no instances
    /// until the first [`update`](Self::update)).
    pub fn new() -> ChessScene {
        let mut scene = Scene::new();
        let center = DVec3::new(0.0, 0.0, BOARD_Z);
        build_board(&mut scene);

        let sprites = SpriteSet {
            models: build_piece_models(),
            instances: Vec::new(),
            carve_model: None,
        };
        ChessScene {
            scene,
            sprites,
            selected: None,
            cursor_world: center,
            // Closer + a touch flatter than the circle's framing: the
            // board is ~128 voxels, smaller than the circle cloud.
            camera: OrbitCamera {
                center,
                yaw: 0.0,
                pitch: 1.0,
                dist: 180.0,
            },
        }
    }

    /// Rebuild the sprite instances from the current pieces, highlighting
    /// `selected` (a board square) if set. `pieces` is sim-derived; the
    /// host reads it from the `World` (positions + kind/colour fields).
    pub fn update(&mut self, pieces: &[PieceView], selected: Option<(i8, i8)>) {
        self.selected = selected;
        let insts = &mut self.sprites.instances;
        insts.clear();
        for pv in pieces {
            let k = pv.kind as usize % PIECE_H.len();
            let (wx, wy) = square_center_world(pv.x, pv.y);
            // Seat the box on the surface: its centre is half a height up
            // (smaller z is up), so the bottom face rests on BOARD_Z.
            let z = BOARD_Z - f64::from(PIECE_H[k]) * 0.5;
            insts.push(SpriteInstanceDesc {
                model: k * 2 + (pv.color as usize & 1),
                pos: [wx as f32, wy as f32, z as f32],
            });
        }
        if let Some((sx, sy)) = selected {
            let (wx, wy) = square_center_world(sx, sy);
            insts.push(SpriteInstanceDesc {
                model: SELECT_MODEL,
                pos: [wx as f32, wy as f32, (BOARD_Z - 2.0) as f32],
            });
        }
        let c = self.cursor_world;
        insts.push(SpriteInstanceDesc {
            model: CHESS_CURSOR_MODEL,
            pos: [c.x as f32, c.y as f32, (BOARD_Z - 3.0) as f32],
        });
    }

    /// Move the hover cursor to where a world ray crosses the board plane;
    /// returns that point. Call every frame so the marker tracks the mouse.
    pub fn hover(&mut self, origin: DVec3, dir: DVec3) -> Option<DVec3> {
        let hit = ground_hit(origin, dir, BOARD_Z);
        if let Some(h) = hit {
            self.cursor_world = h;
        }
        hit
    }

    /// The board square `(0..8, 0..8)` a world ray hits, or `None` if it
    /// misses the board. The host turns this into a move command's source
    /// (first click) or destination (second click).
    pub fn board_square(&self, origin: DVec3, dir: DVec3) -> Option<(i8, i8)> {
        let hit = ground_hit(origin, dir, BOARD_Z)?;
        let sx = (hit.x / SQUARE).floor() as i32 + 4;
        let sy = (hit.y / SQUARE).floor() as i32 + 4;
        if (0..8).contains(&sx) && (0..8).contains(&sy) {
            Some((sx as i8, sy as i8))
        } else {
            None
        }
    }

    /// The currently highlighted square, if any.
    pub fn selected(&self) -> Option<(i8, i8)> {
        self.selected
    }

    /// The piece sprite set, to hand to `SceneRenderer::set_sprites`.
    pub fn sprites(&self) -> &SpriteSet {
        &self.sprites
    }

    /// The renderer needs `&mut Scene` each frame.
    pub fn scene_mut(&mut self) -> &mut Scene {
        &mut self.scene
    }

    /// The current camera, as roxlap's `right/down/forward` basis.
    pub fn camera(&self) -> Camera {
        self.camera.to_roxlap()
    }
}

/// World-space `(x, y)` of the centre of board square `(sx, sy)`, with the
/// board centred on the world origin (its grid is at identity, like the
/// circle's ground — see [`build_ground`]).
fn square_center_world(sx: i8, sy: i8) -> (f64, f64) {
    (
        (f64::from(sx) - 3.5) * SQUARE,
        (f64::from(sy) - 3.5) * SQUARE,
    )
}

/// Lay down the 8×8 checkered board, two voxels thick, top face at
/// `BOARD_Z`. Identity grid for the same reason as [`build_ground`]: the
/// GPU sprite pass projects through the first grid's frame, so the one
/// grid must sit at world identity for sprites to land on the right
/// squares.
fn build_board(scene: &mut Scene) {
    let id = scene.add_grid(GridTransform::identity());
    if let Some(grid) = scene.grid_mut(id) {
        const LIGHT: u32 = 0x80B0_8858; // warm wood
        const DARK: u32 = 0x8060_4028;
        let edge = SQUARE as i32;
        let cz = BOARD_Z as i32;
        for sy in 0..8 {
            for sx in 0..8 {
                let color = if (sx + sy) % 2 == 0 { DARK } else { LIGHT };
                let x0 = (sx - 4) * edge;
                let y0 = (sy - 4) * edge;
                let lo = glam::IVec3::new(x0, y0, cz);
                let hi = glam::IVec3::new(x0 + edge - 1, y0 + edge - 1, cz + 1);
                grid.set_rect(lo, hi, Some(color));
            }
        }
    }
}

/// The 14 chess sprite models: a body box per (kind, side), then the
/// select slab and hover cursor. Indexed `kind*2 + color`, then
/// [`SELECT_MODEL`] / [`CURSOR_MODEL`].
fn build_piece_models() -> Vec<Sprite> {
    let body = |xy: u32, h: u32, col: u32| {
        let mut s = Sprite::axis_aligned(Kv6::solid_box(xy, xy, h, col), [0.0, 0.0, 0.0]);
        // Visibility-first slice: show the raw bright colour, no light bake.
        s.flags = SPRITE_FLAG_NO_SHADING;
        s
    };
    let mut models: Vec<Sprite> = Vec::with_capacity(14);
    for &h in &PIECE_H {
        for &col in &SIDE_COLOR {
            models.push(body(PIECE_W, h, col));
        }
    }
    models.push(body(SQUARE as u32 - 2, 3, 0x80FF_E000)); // SELECT_MODEL (amber)
    models.push(body(6, 6, 0x8000_FF00)); // CHESS_CURSOR_MODEL (green)
    models
}

/// Lay down a flat square ground slab around `center`, two voxels thick,
/// top face at `center.z`. Two-tone checkerboard so motion over it reads
/// clearly.
///
/// The grid is at **identity** transform with voxels at world-absolute
/// coordinates — *not* a translated grid origin. This is deliberate: the
/// GPU backend's instanced sprite pass projects sprites through
/// `cameras[0]` (the world camera transformed into the first grid's
/// local frame, roxlap-gpu lib.rs), so a translated ground grid would
/// offset every world-space mover sprite by that grid's origin. Keeping
/// the only grid at identity makes `cameras[0]` the true world camera.
fn build_ground(scene: &mut Scene, center: DVec3) {
    let id = scene.add_grid(GridTransform::identity());
    if let Some(grid) = scene.grid_mut(id) {
        checker_ground(grid, center);
    }
}

fn checker_ground(grid: &mut Grid, center: DVec3) {
    const TILE: i32 = 16;
    // 0x80 brightness so the unbaked ground is visible (not black).
    const LIGHT: u32 = 0x8050_5860;
    const DARK: u32 = 0x8038_3E44;
    let (cx, cy, cz) = (center.x as i32, center.y as i32, center.z as i32);
    let tiles = (GROUND_HALF * 2) / TILE;
    for ty in 0..tiles {
        for tx in 0..tiles {
            let color = if (tx + ty) % 2 == 0 { LIGHT } else { DARK };
            let x0 = cx - GROUND_HALF + tx * TILE;
            let y0 = cy - GROUND_HALF + ty * TILE;
            let lo = glam::IVec3::new(x0, y0, cz);
            let hi = glam::IVec3::new(x0 + TILE - 1, y0 + TILE - 1, cz + 1);
            grid.set_rect(lo, hi, Some(color));
        }
    }
}

/// Linear interpolation in `f64` (render side only).
fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Intersect a world ray with the horizontal plane `z = plane_z`.
/// `None` if the ray is parallel to the plane or points away from it.
fn ground_hit(origin: DVec3, dir: DVec3, plane_z: f64) -> Option<DVec3> {
    if dir.z.abs() < 1e-9 {
        return None;
    }
    let t = (plane_z - origin.z) / dir.z;
    if t <= 0.0 {
        return None;
    }
    Some(origin + dir * t)
}
