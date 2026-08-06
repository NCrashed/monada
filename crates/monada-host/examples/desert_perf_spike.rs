//! Perf spike for the desert game's D-1 / D-3 gates
//! (`docs/plans/desert-game.md` §13). Answers, with numbers, the two
//! questions that can invalidate L4 (freely-oriented voxel models) and L5
//! (a volumetric map) *before* the D-0 runtime refactor is built on them:
//!
//! 1. **Render.** Does a 256×256×64 volumetric desert plus a few hundred
//!    oriented KV6 vehicles hold a frame budget? The CPU half runs
//!    headless on the same path `roxlap-render`'s CPU backend runs
//!    (`render_scene_composed_frame` for terrain, then one
//!    `draw_sprite_dense_shaded` per instance off a per-model cached
//!    `SpriteDense` — `cpu.rs`), and sweeps the map size so "big world"
//!    can be told apart from "expensive resolution". The GPU half
//!    (`--gpu`) needs a display, so it opens a window and drives the real
//!    `SceneRenderer` — including `add_sprite_instance_posed`, the exact
//!    API L4 needs monada to expose.
//! 2. **Sim.** What the hashed `VolumeStore` costs at that size: build,
//!    per-tick hash, terraform edits, and the walkable-"stand" scan the
//!    planned 3D navigation (§4c) is built on.
//!
//! Not measured (deliberately, and repeated in the report): monada's
//! per-frame sim→render mirror, SSAA, the HUD, shroud culling.
//!
//! ```text
//! cargo run --release -p monada-host --example desert_perf_spike
//! cargo run --release -p monada-host --example desert_perf_spike -- --png
//! cargo run --release -p monada-host --example desert_perf_spike -- --gpu
//! ```

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    // Terse maths names (x/y/z lattice work) and a benchmark's inline
    // constants read better here than the pedantic alternatives.
    clippy::many_single_char_names,
    clippy::similar_names,
    clippy::manual_range_contains,
    clippy::items_after_statements
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::{DVec3, IVec3};
use monada_render::OrbitCamera;
use monada_script::{MaterialId, VolumeStore};
use roxlap_core::camera_math::{self, CameraState};
use roxlap_core::dda_sprite::{draw_sprite_dense_shaded, SpriteDense};
use roxlap_core::opticast::OpticastSettings;
use roxlap_formats::kv6::Kv6;
use roxlap_formats::sprite::Sprite;
use roxlap_formats::VoxColor;
use roxlap_render::{
    BackendPreference, DynSpriteTransform, FrameParams, RenderOptions, SceneRenderer, SpriteSet,
};
use roxlap_scene::render::{render_scene_composed_frame, ComposedFrameParams, SceneRenderScratch};
use roxlap_scene::{GridId, GridTransform, Scene};

/// World units per sim cell — monada's `SCALE` (`map_render.rs`).
const SCALE: f64 = 16.0;
/// Render resolution the spike reports at.
const W: u32 = 1280;
const H: u32 = 720;
/// Measured frames per configuration (plus `WARMUP` discarded).
const FRAMES: usize = 12;
const WARMUP: usize = 3;
/// The plan's map: 64 gameplay tiles × 4 cells (§4a).
const FULL_MAP: i32 = 256;
const FULL_DEPTH: i32 = 64;

// --- deterministic integer terrain -------------------------------------

/// A cheap integer hash — the spike's stand-in for the fixed-point value
/// noise the real generator (§9) will use. Deterministic, no floats.
fn hash2(x: i32, y: i32, seed: u32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (y as u32).wrapping_mul(0xd816_3841) ^ seed;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^ (h >> 15)
}

/// Bilinear value noise on a `period`-cell lattice, in `0..=255`.
fn noise(x: i32, y: i32, period: i32, seed: u32) -> i32 {
    let (x0, y0) = (x.div_euclid(period), y.div_euclid(period));
    let (fx, fy) = (x.rem_euclid(period), y.rem_euclid(period));
    let c = |dx: i32, dy: i32| (hash2(x0 + dx, y0 + dy, seed) & 0xff) as i32;
    let (a, b, cc, d) = (c(0, 0), c(1, 0), c(0, 1), c(1, 1));
    let top = a * (period - fx) + b * fx;
    let bot = cc * (period - fx) + d * fx;
    (top * (period - fy) + bot * fy) / (period * period)
}

/// Surface classes, in the plan's material vocabulary (§4b).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Surface {
    Sand,
    Dune,
    Rock,
    Mountain,
}

impl Surface {
    /// `0xBB_RR_GG_BB` — the high byte is BRIGHTNESS, not alpha.
    fn color(self) -> u32 {
        match self {
            Surface::Sand => 0x80c8_b48c,
            Surface::Dune => 0x80d8_c49c,
            Surface::Rock => 0x8078_6c60,
            Surface::Mountain => 0x8060_5850,
        }
    }
    fn material(self) -> MaterialId {
        MaterialId(match self {
            Surface::Sand | Surface::Dune => 0,
            Surface::Rock => 1,
            Surface::Mountain => 2,
        })
    }
}

/// A desert of a given size — dune seas over sand, rock shelves, and one
/// mountain ridge rising in 3-cell steps (so it walls vehicles out at
/// `max_step` 2 and admits infantry at 4 — the plan's §4b walk rule).
#[derive(Clone, Copy)]
struct Gen {
    map: i32,
    depth: i32,
}

impl Gen {
    /// Surface height + class for one column.
    fn column(self, x: i32, y: i32) -> (i32, Surface) {
        let mean = self.depth / 2;
        let dunes = noise(x, y, 24, 0x51ce) - 128; // −128..127
        let shelf = noise(x, y, 96, 0x0f00_d1ce);
        let base = mean - 2 + dunes / 32; // ±4 cells of dune relief
        if shelf > 168 {
            let step = 3 + noise(x, y, 32, 0xbeef) / 96; // 3..5
            return (mean + 2 + step, Surface::Rock);
        }
        let ridge = (x - self.map / 2).abs();
        if ridge < 14 && y > self.map / 6 && y < self.map - self.map / 6 {
            return (mean + 2 + ((14 - ridge) / 3) * 3, Surface::Mountain);
        }
        if dunes > 40 {
            (base, Surface::Dune)
        } else {
            (base, Surface::Sand)
        }
    }

    /// Paint the desert into a roxlap grid, one span per column — the
    /// span-based store makes solid ground cheap (a filled column is one
    /// entry, not `depth` voxels).
    fn build_grid(self, scene: &mut Scene, grid: GridId) -> usize {
        for y in 0..self.map {
            for x in 0..self.map {
                let (h, s) = self.column(x, y);
                // Grid z is down: sim height h ⇒ grid voxel depth−1−h.
                scene.grid_mut(grid).expect("grid").set_rect(
                    IVec3::new(x, y, self.depth - 1 - h),
                    IVec3::new(x, y, self.depth - 1),
                    Some(VoxColor(s.color())),
                );
            }
        }
        (self.map * self.map) as usize
    }

    /// The world point a camera should look at: map centre, mean surface.
    fn focus(self) -> DVec3 {
        DVec3::new(
            f64::from(self.map) / 2.0 * SCALE,
            f64::from(self.map) / 2.0 * SCALE,
            f64::from(self.depth - 1 - self.depth / 2) * SCALE,
        )
    }

    /// Scatter `n` vehicles on a `spacing`-cell lattice centred on the map,
    /// each seated on its column with its own heading. `spacing` must
    /// exceed the model's footprint (~3.6 cells) or the vehicles
    /// interpenetrate and the measurement stops meaning anything.
    fn scatter(self, n: usize, spacing: i32) -> Vec<Instance> {
        let mut out = Vec::with_capacity(n);
        let side = (n as f64).sqrt().ceil() as i32;
        let spread = side * spacing;
        for i in 0..n as i32 {
            let (gx, gy) = (i % side, i / side);
            let j = hash2(gx, gy, 0xfeed);
            let x = (self.map / 2 - spread / 2 + gx * spacing + (j % 2) as i32)
                .clamp(1, self.map - 2);
            let y = (self.map / 2 - spread / 2 + gy * spacing + ((j >> 8) % 2) as i32)
                .clamp(1, self.map - 2);
            let (h, _) = self.column(x, y);
            out.push(Instance {
                pos: [
                    (f64::from(x) + 0.5) as f32 * SCALE as f32,
                    (f64::from(y) + 0.5) as f32 * SCALE as f32,
                    // Just above the column's top voxel (grid z is down).
                    (self.depth - 1 - h) as f32 * SCALE as f32 - 14.0,
                ],
                yaw: (hash2(x, y, 0xbead) % 360) as f32 * std::f32::consts::TAU / 360.0,
            });
        }
        out
    }
}

/// A vehicle: turret, hull and tracks in one KV6. Only its surface shell
/// is decoded (`SpriteDense` keeps the visible hull), which is what a real
/// tank model will be.
fn tank_kv6() -> Kv6 {
    Kv6::from_fn(24, 16, 10, |x, y, z| {
        let (x, y, z) = (x as i32, y as i32, z as i32);
        // KV6 z is down: z = 0 is the model's top.
        if (6..16).contains(&x) && (4..12).contains(&y) && z < 4 {
            Some(VoxColor(0x8090_9a86)) // turret
        } else if (4..8).contains(&z) {
            Some(VoxColor(0x80a8_b48c)) // hull
        } else if z >= 8 && (y < 4 || y >= 12) {
            Some(VoxColor(0x8050_4c48)) // tracks
        } else {
            None
        }
    })
}

/// One drawable instance: a world pose.
struct Instance {
    pos: [f32; 3],
    yaw: f32,
}

impl Instance {
    /// Basis columns (model +x/+y/+z → world), yawed about world z and
    /// uniformly scaled so a 24-voxel hull spans ~3.6 cells.
    fn basis(&self, scale: f32, oriented: bool) -> ([f32; 3], [f32; 3], [f32; 3]) {
        let (sy, cy) = if oriented {
            (self.yaw.sin(), self.yaw.cos())
        } else {
            (0.0, 1.0)
        };
        (
            [cy * scale, sy * scale, 0.0],
            [-sy * scale, cy * scale, 0.0],
            [0.0, 0.0, scale],
        )
    }
}

const SPRITE_SCALE: f32 = 2.4;

/// Draw every instance the way `cpu.rs` does: cached dense decode, one
/// `draw_sprite_dense_shaded` per instance, z-tested against the terrain
/// pass's buffer. Returns pixels written — the guard against timing an
/// off-screen no-op.
fn draw_instances(
    fb: &mut [u32],
    zb: &mut [f32],
    (w, h): (u32, u32),
    cam: &CameraState,
    settings: &OpticastSettings,
    dense: &SpriteDense,
    insts: &[Instance],
    oriented: bool,
) -> u64 {
    let mut written = 0u64;
    for it in insts {
        let (right, up, forward) = it.basis(SPRITE_SCALE, oriented);
        written += u64::from(draw_sprite_dense_shaded(
            fb, zb, w as usize, w, h, cam, settings, dense, it.pos, right, up, forward, 0, None,
        ));
    }
    written
}

fn median_ms(mut v: Vec<Duration>) -> f64 {
    v.sort_unstable();
    v[v.len() / 2].as_secs_f64() * 1000.0
}

fn settings_for(w: u32, h: u32) -> OpticastSettings {
    let mut s = OpticastSettings::for_oracle_framebuffer(w, h);
    // What monada-host does: cover the whole zoom range so far geometry
    // is not scan-clipped (`lib.rs`).
    s.max_scan_dist = s.max_scan_dist.max(4096);
    s
}

// --- CPU measurements ---------------------------------------------------

struct CpuFrame {
    total: Vec<Duration>,
    terrain: Vec<Duration>,
    sprites: Vec<Duration>,
    px: u64,
}

fn cpu_frames(
    scene: &mut Scene,
    scratch: &mut SceneRenderScratch,
    fb: &mut [u32],
    zb: &mut [f32],
    (w, h): (u32, u32),
    settings: &OpticastSettings,
    camera: &roxlap_core::Camera,
    dense: &SpriteDense,
    insts: &[Instance],
    oriented: bool,
) -> CpuFrame {
    let cam_state = camera_math::derive(camera, w, h, settings.hx, settings.hy, settings.hz);
    let mut out = CpuFrame {
        total: Vec::new(),
        terrain: Vec::new(),
        sprites: Vec::new(),
        px: 0,
    };
    for f in 0..(FRAMES + WARMUP) {
        let t_all = Instant::now();
        fb.fill(0x0099_b3d9);
        zb.fill(f32::INFINITY);
        let t_terrain = Instant::now();
        let params = ComposedFrameParams::new(camera, settings);
        let _ = render_scene_composed_frame(fb, zb, w as usize, w, h, scene, &params, scratch);
        let d_terrain = t_terrain.elapsed();
        let t_sprites = Instant::now();
        out.px = draw_instances(fb, zb, (w, h), &cam_state, settings, dense, insts, oriented);
        let d_sprites = t_sprites.elapsed();
        if f >= WARMUP {
            out.terrain.push(d_terrain);
            out.sprites.push(d_sprites);
            out.total.push(t_all.elapsed());
        }
    }
    out
}

/// Vehicles are ~3.6 cells long, so a 5-cell lattice is bumper-to-bumper
/// without interpenetration — the densest honest battle line.
const SPACING: i32 = 5;

fn cpu_bench(dump_png: bool) {
    let dense = {
        let kv6 = tank_kv6();
        let t = Instant::now();
        let d = SpriteDense::from_kv6(&kv6);
        println!(
            "model decode        : {:>8.3} ms   (once per model; the backend caches it)",
            t.elapsed().as_secs_f64() * 1000.0
        );
        d
    };
    // One buffer pair, sized for the largest resolution measured.
    let (max_w, max_h) = (1920u32, 1080u32);
    let mut fb = vec![0x0099_b3d9u32; (max_w * max_h) as usize];
    let mut zb = vec![f32::INFINITY; (max_w * max_h) as usize];
    let mut scratch = SceneRenderScratch::default();

    // --- M1: does terrain cost follow map size, or resolution? ------------
    println!("\nM1 terrain only @ {W}×{H}, identical tactical camera (dist 700)");
    println!("map              build ms   frame ms");
    let settings = settings_for(W, H);
    for map in [48, 96, 160, FULL_MAP] {
        let gen = Gen {
            map,
            depth: if map == FULL_MAP { FULL_DEPTH } else { 32 },
        };
        let mut scene = Scene::new();
        let grid = scene.add_grid(GridTransform::at_scale(DVec3::ZERO, SCALE));
        let t = Instant::now();
        gen.build_grid(&mut scene, grid);
        let build = t.elapsed().as_secs_f64() * 1000.0;
        let camera = OrbitCamera {
            center: gen.focus(),
            yaw: 0.9,
            pitch: 1.05,
            dist: 700.0,
        }
        .to_roxlap();
        let n = (W * H) as usize;
        let r = cpu_frames(
            &mut scene,
            &mut scratch,
            &mut fb[..n],
            &mut zb[..n],
            (W, H),
            &settings,
            &camera,
            &dense,
            &[],
            true,
        );
        println!(
            "{map:>3}×{map:<3}×{:<3}     {build:>8.1}   {:>8.1}",
            gen.depth,
            median_ms(r.total)
        );
    }

    // The full map, once, for everything below.
    let gen = Gen {
        map: FULL_MAP,
        depth: FULL_DEPTH,
    };
    let mut scene = Scene::new();
    let grid = scene.add_grid(GridTransform::at_scale(DVec3::ZERO, SCALE));
    gen.build_grid(&mut scene, grid);

    // --- M1b: the resolution knob (a retro renderer's cheapest lever) -----
    println!("\nM1b terrain only, {FULL_MAP}×{FULL_MAP}×{FULL_DEPTH}, tactical camera");
    println!("resolution       frame ms");
    for (w, h) in [(1920u32, 1080u32), (W, H), (960, 540), (640, 360)] {
        let s = settings_for(w, h);
        let camera = OrbitCamera {
            center: gen.focus(),
            yaw: 0.9,
            pitch: 1.05,
            dist: 700.0,
        }
        .to_roxlap();
        let n = (w * h) as usize;
        let r = cpu_frames(
            &mut scene,
            &mut scratch,
            &mut fb[..n],
            &mut zb[..n],
            (w, h),
            &s,
            &camera,
            &dense,
            &[],
            true,
        );
        println!("{w:>5}×{h:<5}    {:>8.1}", median_ms(r.total));
    }

    // --- M2: the marginal cost of oriented vehicles -----------------------
    println!("\nM2 {FULL_MAP}×{FULL_MAP}×{FULL_DEPTH} + oriented vehicles @ {W}×{H}");
    println!("case                        frame ms   terrain   sprites   px drawn   µs/sprite");
    let cases: [(&str, f64, usize, bool); 8] = [
        ("tactical    0", 700.0, 0, true),
        ("tactical   60 in view", 700.0, 60, true),
        ("tactical  120 in view", 700.0, 120, true),
        ("tactical  400 alive", 700.0, 400, true),
        ("tactical  400 axis-aligned", 700.0, 400, false),
        ("strategic   0", 2400.0, 0, true),
        ("strategic 400 alive", 2400.0, 400, true),
        ("strategic 800 alive", 2400.0, 800, true),
    ];
    let mut png_fb = None;
    for (label, dist, units, oriented) in cases {
        let camera = OrbitCamera {
            center: gen.focus(),
            yaw: 0.9,
            pitch: 1.05,
            dist,
        }
        .to_roxlap();
        let insts = gen.scatter(units, SPACING);
        let n = (W * H) as usize;
        let r = cpu_frames(
            &mut scene,
            &mut scratch,
            &mut fb[..n],
            &mut zb[..n],
            (W, H),
            &settings,
            &camera,
            &dense,
            &insts,
            oriented,
        );
        let sprites_ms = median_ms(r.sprites);
        let per = if units == 0 {
            0.0
        } else {
            sprites_ms * 1000.0 / units as f64
        };
        println!(
            "{label:<26} {:>9.1}  {:>8.1}  {:>8.1}  {:>9}   {per:>9.1}",
            median_ms(r.total),
            median_ms(r.terrain),
            sprites_ms,
            r.px
        );
        if units == 120 && oriented {
            png_fb = Some(fb[..n].to_vec());
        }
    }

    if let (true, Some(buf)) = (dump_png, png_fb.as_ref()) {
        let img: image::ImageBuffer<image::Rgb<u8>, Vec<u8>> =
            image::ImageBuffer::from_fn(W, H, |x, y| {
                let p = buf[(y * W + x) as usize];
                image::Rgb([
                    ((p >> 16) & 0xff) as u8,
                    ((p >> 8) & 0xff) as u8,
                    (p & 0xff) as u8,
                ])
            });
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("desert_spike.png");
        img.save(&path).expect("save png");
        println!("\nwrote {}", path.display());
    }
}

// --- GPU measurement (needs a display) ----------------------------------

/// Drives the real `SceneRenderer` over a window: terrain + 400 posed
/// instances through `add_sprite_instance_posed` — the API L4 needs.
/// Two configurations, because monada currently pins the GPU to mip-0
/// everywhere (`gpu_mip_scan_dist: 8192.0` in `lib.rs`, justified there by
/// "monada's scenes are small"), and this map is the one that isn't.
/// Sustained frames per configuration. The GPU path must be measured over
/// a long run, not a handful of frames: `render` only *records* work, so a
/// short burst times command submission (which reads as 0.0 ms) rather than
/// the frame. Over enough frames the swapchain saturates and wall-clock
/// converges on the real cost.
const GPU_FRAMES: usize = 240;
const GPU_WARMUP: usize = 60;

struct GpuApp;

impl GpuApp {
    /// One configuration end to end on an existing window: build the
    /// renderer, the desert, 400 posed instances, and measure both zooms
    /// two ways — *pipelined* (`render` + `present`, frames overlapping as
    /// they do in a real loop) and *drained* (`+ wait_idle`, a hard
    /// per-frame upper bound). The truth is bracketed by the pair.
    fn run_config(window: &Arc<winit::window::Window>, mip_scan: f32) {
        let opts = RenderOptions {
            backend: BackendPreference::RequireGpu,
            gpu_mip_scan_dist: mip_scan,
            ..RenderOptions::default()
        };
        let mut renderer = match SceneRenderer::try_new(window.clone(), (W, H), &opts) {
            Ok(r) => r,
            Err(e) => {
                println!("GPU init failed: {e}");
                return;
            }
        };
        let gen = Gen {
            map: FULL_MAP,
            depth: FULL_DEPTH,
        };
        let mut scene = Scene::new();
        let grid = scene.add_grid(GridTransform::at_scale(DVec3::ZERO, SCALE));
        gen.build_grid(&mut scene, grid);

        let settings = settings_for(W, H);
        println!("   units    tactical (p/d)   strategic (p/d)");
        for units in [0usize, 400, 800] {
            // `set_sprites` replaces the instance world, so re-seating the
            // model here is how the count is varied without rebuilding the
            // renderer or the terrain.
            let models = renderer.set_sprites(&SpriteSet {
                models: vec![Sprite::axis_aligned(tank_kv6(), [0.0, 0.0, 0.0])],
                instances: Vec::new(),
                carve_model: None,
            });
            let mut spawned = 0;
            for it in &gen.scatter(units, SPACING) {
                let (right, up, forward) = it.basis(SPRITE_SCALE, true);
                if renderer
                    .add_sprite_instance_posed(
                        models[0],
                        DynSpriteTransform {
                            pos: it.pos,
                            right,
                            up,
                            forward,
                        },
                    )
                    .is_some()
                {
                    spawned += 1;
                }
            }

            let mut cells = Vec::new();
            for dist in [700.0, 2400.0] {
                let camera = OrbitCamera {
                    center: gen.focus(),
                    yaw: 0.9,
                    pitch: 1.05,
                    dist,
                }
                .to_roxlap();
                let mut frame = FrameParams::new(&settings);
                frame.sky_color = roxlap_formats::Rgb(0x0099_b3d9);

                for _ in 0..GPU_WARMUP {
                    renderer.render(&mut scene, &camera, &frame);
                    renderer.present();
                }
                let t = Instant::now();
                for _ in 0..GPU_FRAMES {
                    renderer.render(&mut scene, &camera, &frame);
                    renderer.present();
                }
                renderer.wait_idle();
                let pipelined = t.elapsed().as_secs_f64() * 1000.0 / GPU_FRAMES as f64;

                let mut drained = Vec::new();
                for _ in 0..24 {
                    let t = Instant::now();
                    renderer.render(&mut scene, &camera, &frame);
                    renderer.present();
                    renderer.wait_idle();
                    drained.push(t.elapsed());
                }
                cells.push(format!("{pipelined:>6.2}/{:<6.2}", median_ms(drained)));
            }
            println!("   {spawned:>5}    {}   {}", cells[0], cells[1]);
        }
        renderer.wait_idle();
    }
}

impl winit::application::ApplicationHandler for GpuApp {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(
                    winit::window::Window::default_attributes()
                        .with_title("desert perf spike")
                        .with_inner_size(winit::dpi::PhysicalSize::new(W, H)),
                )
                .expect("create_window"),
        );
        // Both configurations share the window: winit allows exactly one
        // `EventLoop` per process, so they cannot each own one.
        for (mip_scan, label) in [
            (8192.0f32, "mip-0 (monada today)"),
            (64.0f32, "roxlap LOD default"),
        ] {
            println!("-- gpu_mip_scan_dist: {label}");
            Self::run_config(&window, mip_scan);
        }
        event_loop.exit();
    }

    fn window_event(
        &mut self,
        event_loop: &winit::event_loop::ActiveEventLoop,
        _id: winit::window::WindowId,
        event: winit::event::WindowEvent,
    ) {
        if matches!(event, winit::event::WindowEvent::CloseRequested) {
            event_loop.exit();
        }
    }
}

fn gpu_bench() {
    println!(
        "\nM1b/M2b GPU backend, {FULL_MAP}×{FULL_MAP}×{FULL_DEPTH} + 400 posed instances @ {W}×{H}"
    );
    let event_loop = match winit::event_loop::EventLoop::new() {
        Ok(e) => e,
        Err(e) => {
            println!("no display ({e}) — run --gpu on a machine with one");
            return;
        }
    };
    let _ = event_loop.run_app(&mut GpuApp);
}

// --- M3: the sim-side volume store --------------------------------------

fn sim_bench() {
    let gen = Gen {
        map: FULL_MAP,
        depth: FULL_DEPTH,
    };
    println!("\nM3 sim side — the hashed VolumeStore ({FULL_MAP}×{FULL_MAP}×{FULL_DEPTH})");

    let mut store = VolumeStore::new();
    let t = Instant::now();
    let mut cells = 0u64;
    for y in 0..gen.map {
        for x in 0..gen.map {
            let (h, s) = gen.column(x, y);
            store.fill(
                i64::from(x),
                i64::from(y),
                0,
                i64::from(x),
                i64::from(y),
                i64::from(h),
                s.material(),
            );
            cells += (h + 1) as u64;
        }
    }
    println!(
        "store build         : {:>8.1} ms   ({cells} solid cells ≈ {} MB of dense 16³ chunks)",
        t.elapsed().as_secs_f64() * 1000.0,
        (cells / 4096 + 1) * 8 / 1024
    );

    let t = Instant::now();
    let h0 = store.state_hash();
    println!(
        "state_hash (quiet)  : {:>8.3} ms   (every desync-hash tick; digest {h0:#018x})",
        t.elapsed().as_secs_f64() * 1000.0
    );

    // One tick's terraform at a generous work rate, measured in the two
    // shapes the store offers: a Dweller trench carved cell-by-cell
    // (`clear`, the only carve verb — `voxel_clear` hole-punches ONE cell)
    // and a Surfling berm raised with the box `fill`.
    let rows = (gen.map * 3 / 8)..(gen.map * 5 / 8);
    let t = Instant::now();
    let mut carved = 0u64;
    for y in rows.clone() {
        for x in 100..104 {
            let (h, _) = gen.column(x, y);
            for z in (h - 5)..=h {
                store.clear(i64::from(x), i64::from(y), i64::from(z));
                carved += 1;
            }
        }
    }
    let carve_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = Instant::now();
    let mut raised = 0u64;
    for y in rows {
        for x in 140..144 {
            let (h, _) = gen.column(x, y);
            store.fill(
                i64::from(x),
                i64::from(y),
                i64::from(h + 1),
                i64::from(x),
                i64::from(y),
                i64::from(h + 6),
                MaterialId(3),
            );
            raised += 6;
        }
    }
    let fill_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "carve  {carved:>5} cells  : {carve_ms:>8.2} ms   ({:>5.2} µs/cell — `clear`, one voxel per call)",
        carve_ms * 1000.0 / carved as f64
    );
    println!(
        "raise  {raised:>5} cells  : {fill_ms:>8.2} ms   ({:>5.2} µs/cell — `fill`, one call per column)",
        fill_ms * 1000.0 / raised as f64
    );

    // The control that isolates the cost: the SAME cell count in a single
    // box `fill`, so the per-chunk rehash runs a handful of times instead
    // of once per call.
    let t = Instant::now();
    store.fill(60, 60, 40, 63, 123, 45, MaterialId(3));
    let bulk = 4 * 64 * 6;
    let bulk_ms = t.elapsed().as_secs_f64() * 1000.0;
    println!(
        "bulk   {bulk:>5} cells  : {bulk_ms:>8.2} ms   ({:>5.2} µs/cell — ONE `fill` of the same volume)",
        bulk_ms * 1000.0 / f64::from(bulk)
    );

    let t = Instant::now();
    let h1 = store.state_hash();
    println!(
        "state_hash (dirty)  : {:>8.3} ms   (digest {h1:#018x})",
        t.elapsed().as_secs_f64() * 1000.0
    );

    // Stand extraction — the 3D-nav (§4c) primitive: per column, every
    // solid cell with HEADROOM clear cells above it.
    const HEADROOM: i32 = 3;
    let scan = |x0: i32, x1: i32, y0: i32, y1: i32| {
        let mut stands = 0u64;
        for y in y0..y1 {
            for x in x0..x1 {
                let mut clear = 0;
                for z in (0..gen.depth).rev() {
                    if store
                        .get(i64::from(x), i64::from(y), i64::from(z))
                        .is_some()
                    {
                        if clear >= HEADROOM {
                            stands += 1;
                        }
                        clear = 0;
                    } else {
                        clear += 1;
                    }
                }
            }
        }
        stands
    };
    let t = Instant::now();
    let all = scan(0, gen.map, 0, gen.map);
    println!(
        "stand scan (whole)  : {:>8.1} ms   ({all} stands, {} columns, per-cell get())",
        t.elapsed().as_secs_f64() * 1000.0,
        gen.map * gen.map
    );
    let t = Instant::now();
    let patch = scan(100, 108, 96, 104);
    println!(
        "stand scan (64 cols): {:>8.3} ms   ({patch} stands — the per-edit invalidation cost)",
        t.elapsed().as_secs_f64() * 1000.0
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let gpu = args.iter().any(|a| a == "--gpu");
    let dump_png = args.iter().any(|a| a == "--png");

    println!("desert perf spike — {W}×{H}, {SCALE} world units/cell");
    println!("budget reference: a 30 Hz tick is 33.3 ms; a 60 fps frame is 16.7 ms");

    if gpu {
        gpu_bench();
    } else {
        cpu_bench(dump_png);
        sim_bench();
        println!(
            "\nCPU raycaster only. Run with --gpu on a machine with a display for the\n\
             backend monada actually ships (PreferGpu). Not included anywhere above:\n\
             monada's sim→render mirror, SSAA, HUD, shroud culling."
        );
    }
}
