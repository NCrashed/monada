//! Headless probe: reproduce the ship's two-deck hull + deck cutaway on the
//! CPU renderer (no window) and dump PNGs, so we can SEE what `deck_clip`
//! (`Grid::z_clip`) actually does from the SS13 top-down camera — the "circle
//! that cuts the ceiling" the live GPU run showed. Renders the lower-deck view
//! (z_clip set), the upper-deck view, and a no-clip reference.
//!
//! ```text
//! cargo run -p monada-host --example ship_clip_probe
//! ```

#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use glam::{DVec3, IVec3};
use image::{ImageBuffer, Rgb};
use monada_render::OrbitCamera;
use roxlap_core::opticast::OpticastSettings;
use roxlap_formats::VoxColor;
use roxlap_scene::render::{render_scene, CpuFog};
use roxlap_scene::{GridId, GridTransform, Scene};

const S: i32 = 16; // SCALE
const G: i32 = 100; // GROUND_Z

/// Replicate `MapRender::voxel_fill`'s sim→grid mapping (X mirrored, z-up sim →
/// world `G - z`).
#[allow(clippy::too_many_arguments)]
fn fill(
    scene: &mut Scene,
    grid: GridId,
    x0: i32,
    y0: i32,
    z0: i32,
    x1: i32,
    y1: i32,
    z1: i32,
    col: u32,
) {
    let lo = IVec3::new(-(x1 + 1) * S, y0 * S, G - z1);
    let hi = IVec3::new(-x0 * S - 1, (y1 + 1) * S - 1, G - z0);
    scene
        .grid_mut(grid)
        .unwrap()
        .set_rect(lo, hi, Some(VoxColor(col)));
}

/// The ship's `world_of` for the camera focus (X mirrored, z unscaled).
fn world_of(x: f64, y: f64, z: f64) -> DVec3 {
    DVec3::new(
        -(x + 0.5) * f64::from(S),
        (y + 0.5) * f64::from(S),
        f64::from(G) - z,
    )
}

fn build_hull(scene: &mut Scene, grid: GridId) {
    let plate = 0x8055_5f6b;
    let wall = 0x8079_8592;
    let stair = 0x804d_9a8f;
    // deck 0 (lower): floor z=0, walls z 1..24
    fill(scene, grid, 0, 0, 0, 19, 19, 0, plate);
    rim(scene, grid, 1, 24, wall);
    fill(scene, grid, 1, 10, 1, 8, 10, 24, wall);
    fill(scene, grid, 11, 10, 1, 15, 10, 24, wall);
    // deck 1 (upper): floor z=28, walls z 29..52
    fill(scene, grid, 0, 0, 28, 19, 19, 28, plate);
    rim(scene, grid, 29, 52, wall);
    fill(scene, grid, 10, 1, 29, 10, 8, 52, wall);
    fill(scene, grid, 10, 11, 29, 10, 18, 52, wall);
    // stair markers
    fill(scene, grid, 16, 1, 0, 18, 18, 0, stair);
    fill(scene, grid, 16, 1, 28, 18, 18, 28, stair);
}

fn rim(scene: &mut Scene, grid: GridId, lo: i32, hi: i32, col: u32) {
    fill(scene, grid, 0, 0, lo, 19, 0, hi, col);
    fill(scene, grid, 0, 19, lo, 19, 19, hi, col);
    fill(scene, grid, 0, 0, lo, 0, 19, hi, col);
    fill(scene, grid, 19, 0, lo, 19, 19, hi, col);
}

fn render_png(name: &str, z_clip: Option<i32>, focus: DVec3) {
    let (w, h) = (640u32, 480u32);
    let mut scene = Scene::new();
    let grid = scene.add_grid(GridTransform::identity());
    build_hull(&mut scene, grid);
    scene.grid_mut(grid).unwrap().z_clip = z_clip;

    let cam = OrbitCamera {
        center: focus,
        yaw: 0.8,
        pitch: 1.2,
        dist: 60.0,
    }
    .to_roxlap();

    let mut settings = OpticastSettings::for_oracle_framebuffer(w, h);
    settings.max_scan_dist = 4096;

    let mut fb = vec![0x0099_b3d9u32; (w * h) as usize];
    let mut zb = vec![f32::INFINITY; (w * h) as usize];
    render_scene(
        &mut fb,
        &mut zb,
        w as usize,
        w,
        h,
        CpuFog::default(),
        &mut scene,
        &cam,
        &settings,
        None,
    );

    let img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |x, y| {
        let px = fb[(y * w + x) as usize];
        Rgb([
            ((px >> 16) & 0xff) as u8,
            ((px >> 8) & 0xff) as u8,
            (px & 0xff) as u8,
        ])
    });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("{name}.png"));
    img.save(&path).unwrap();
    eprintln!("wrote {}", path.display());
}

fn main() {
    // Crew centred at sim (10,10). Lower deck: clip = G - deck_top(0)=27 → 73.
    let lower_focus = world_of(10.0, 10.0, 0.0);
    render_png("probe_lower_clip73", Some(73), lower_focus);
    render_png("probe_lower_noclip", None, lower_focus);
    // Upper deck: focus at sim z=28, clip = G - deck_top(1)=55 → 45.
    let upper_focus = world_of(10.0, 10.0, 28.0);
    render_png("probe_upper_clip45", Some(45), upper_focus);
}
