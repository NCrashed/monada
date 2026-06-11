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

/// The M1 render scene: a ground grid plus a per-frame sprite set for
/// the movers.
pub struct CircleScene {
    scene: Scene,
    /// Cube models (one per palette colour) + the live instance list.
    sprites: SpriteSet,
    center: DVec3,
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

        // One flat-shaded cube model per palette colour.
        let models: Vec<Sprite> = PALETTE
            .iter()
            .map(|&col| {
                let mut sprite = Sprite::axis_aligned(Kv6::solid_cube(CUBE, col), [0.0, 0.0, 0.0]);
                // Show the raw bright palette colour rather than relying
                // on a lighting bake for this visibility-first slice.
                sprite.flags = SPRITE_FLAG_NO_SHADING;
                sprite
            })
            .collect();
        let instances = (0..count)
            .map(|i| SpriteInstanceDesc {
                model: i % PALETTE.len(),
                pos: [center.x as f32, center.y as f32, center.z as f32],
            })
            .collect();
        let sprites = SpriteSet {
            models,
            instances,
            carve_model: None,
        };

        CircleScene {
            scene,
            sprites,
            center,
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
        let n = self.sprites.instances.len().min(prev.len()).min(curr.len());
        for i in 0..n {
            let w = self.mover_world(prev[i], curr[i], alpha);
            self.sprites.instances[i].pos = [w.x as f32, w.y as f32, w.z as f32];
        }
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
