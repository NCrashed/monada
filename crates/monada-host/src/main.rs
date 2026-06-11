//! monada native host (DESIGN.md §4) — M1 render-bridge slice.
//!
//! Drives the M0 "100 entities walk in a circle" sim through the
//! `monada-render` bridge and roxlap's CPU render facade in a winit
//! window: the sim ticks at a fixed rate, the renderer runs at display
//! rate and interpolates between the last two sim ticks (DESIGN.md
//! §3.2). Sim state never holds a float pose — the Q32.32 -> f64
//! conversion lives entirely in `monada-render`.
//!
//! Controls: arrow keys orbit (yaw/pitch), `W`/`S` zoom, left-click to
//! pick a mover, `Esc` quits. A small egui HUD shows tick / FPS /
//! selection (DESIGN.md §3.2).

// Host-side float casts (FPS readout, scale factor) are render-side and
// deliberate; the deterministic wall is in monada-sim, not here.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::sync::Arc;
use std::time::Instant;

use monada_render::CircleScene;
use monada_sim::scenario::{CircleSim, DEFAULT_COUNT};
use monada_sim::Simulation;
use roxlap_core::opticast::OpticastSettings;
use roxlap_core::sprite::SpriteLighting;
// egui itself comes through roxlap-render's re-export so the version
// matches the one `paint_egui` rasterises with.
use roxlap_render::{egui, FrameParams, RenderOptions, SceneRenderer};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

/// Fixed simulation step (25 Hz, the WC3-parity default — DESIGN.md §3.1).
const TICK_DT: f64 = 1.0 / 25.0;
/// Packed `0x00RRGGBB` sky / clear colour.
const SKY_COLOR: u32 = 0x0099_B3D9;

/// Camera control rates (per second of held input).
const YAW_RATE: f64 = 1.4;
const PITCH_RATE: f64 = 1.0;
const ZOOM_RATE: f64 = 240.0;

fn main() {
    let event_loop = EventLoop::new().expect("winit: EventLoop::new");
    // Animate continuously: poll, don't wait for input.
    event_loop.set_control_flow(ControlFlow::Poll);
    eprintln!("monada-host: arrows orbit, W/S zoom, Esc quits");
    let mut app = App::new();
    event_loop.run_app(&mut app).expect("winit: run_app");
}

/// Which camera-control keys are currently held. A flat set of bools is
/// the natural shape for held-key state — the lint's state-machine
/// suggestion would only obscure it.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
struct Keys {
    yaw_left: bool,
    yaw_right: bool,
    pitch_up: bool,
    pitch_down: bool,
    zoom_in: bool,
    zoom_out: bool,
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<SceneRenderer>,
    scene: CircleScene,
    /// Sim state before and after the most recent fixed step; the
    /// renderer interpolates between them.
    prev: CircleSim,
    curr: CircleSim,
    /// CPU sprite shading. `default_oracle` needs no engine and is
    /// `'static`; required (as `Some`) for the CPU backend to draw the
    /// mover sprites at all.
    lighting: SpriteLighting<'static>,
    accumulator: f64,
    last_frame: Instant,
    keys: Keys,
    /// Last cursor position in physical pixels, for click picking.
    cursor: (f64, f64),
    /// Smoothed frames-per-second for the HUD.
    fps: f32,
    /// egui context + winit input bridge for the HUD overlay.
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    /// One-shot coordinate dump (set `MONADA_DEBUG=1`).
    debug_done: bool,
}

impl App {
    fn new() -> App {
        let sim = CircleSim::canonical();
        App {
            window: None,
            renderer: None,
            scene: CircleScene::new(DEFAULT_COUNT as usize),
            prev: sim.clone(),
            curr: sim,
            lighting: SpriteLighting::default_oracle(),
            accumulator: 0.0,
            last_frame: Instant::now(),
            keys: Keys::default(),
            cursor: (0.0, 0.0),
            fps: 0.0,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            debug_done: false,
        }
    }

    /// Run the egui HUD for this frame and tessellate it. Returns the
    /// paint jobs + texture delta to hand to `paint_egui`, or `None`
    /// before the egui state exists (pre-`resumed`).
    fn run_hud(
        &mut self,
        window: &Window,
    ) -> Option<(Vec<egui::ClippedPrimitive>, egui::TexturesDelta, f32)> {
        let tick = self.curr.tick();
        let fps = self.fps;
        let selected = self.scene.selected();
        let ctx = &self.egui_ctx;
        let state = self.egui_state.as_mut()?;

        let raw = state.take_egui_input(window);
        let out = ctx.run(raw, |ui_ctx| build_hud(ui_ctx, tick, fps, selected));
        state.handle_platform_output(window, out.platform_output);
        let jobs = ctx.tessellate(out.shapes, out.pixels_per_point);
        Some((jobs, out.textures_delta, out.pixels_per_point))
    }

    /// Pick the mover under the cursor: unproject through the renderer's
    /// last-frame projection, intersect the ground plane, select the
    /// nearest mover (DESIGN.md §3.2).
    fn pick_under_cursor(&mut self) {
        let cam = self.scene.camera();
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let Some(ray) = renderer.view_ray(&cam, self.cursor.0, self.cursor.1) else {
            return;
        };
        match self.scene.pick_ground(ray.origin, ray.dir) {
            Some(i) => eprintln!("picked mover #{i}"),
            None => eprintln!("picked: (none)"),
        }
    }

    /// Advance the camera from currently-held keys.
    fn drive_camera(&mut self, dt: f64) {
        let dyaw = (f64::from(self.keys.yaw_right) - f64::from(self.keys.yaw_left)) * YAW_RATE * dt;
        let dpitch =
            (f64::from(self.keys.pitch_down) - f64::from(self.keys.pitch_up)) * PITCH_RATE * dt;
        let ddist = (f64::from(self.keys.zoom_out) - f64::from(self.keys.zoom_in)) * ZOOM_RATE * dt;
        if dyaw != 0.0 || dpitch != 0.0 || ddist != 0.0 {
            self.scene.camera.orbit(dyaw, dpitch, ddist);
        }
    }

    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }

        // Fixed-step the sim, accumulating real time; render interpolates
        // the leftover fraction.
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f64().min(0.25);
        self.last_frame = now;
        if dt > 0.0 {
            // Exponential smoothing so the HUD reading is steady.
            self.fps = self.fps.mul_add(0.9, (1.0 / dt) as f32 * 0.1);
        }
        self.accumulator += dt;
        while self.accumulator >= TICK_DT {
            self.prev = self.curr.clone();
            self.curr.step();
            self.accumulator -= TICK_DT;
        }
        let alpha = (self.accumulator / TICK_DT).clamp(0.0, 1.0);

        self.drive_camera(dt);
        self.scene.update(
            self.prev.movers().columns().pos(),
            self.curr.movers().columns().pos(),
            alpha,
        );

        if !self.debug_done && std::env::var_os("MONADA_DEBUG").is_some() {
            self.debug_done = true;
            let cam = self.scene.camera();
            let (center, sample) = self.scene.debug_positions();
            eprintln!("[debug] camera pos={:?} forward={:?}", cam.pos, cam.forward);
            eprintln!("[debug] board center={center:?}");
            for (i, p) in sample.iter().enumerate() {
                eprintln!("[debug] cube[{i}] world={p:?}");
            }
        }

        let camera = self.scene.camera();

        // Track the picking ray under the cursor every frame (debug
        // marker), using the previous frame's projection.
        if let Some(renderer) = self.renderer.as_ref() {
            if let Some(ray) = renderer.view_ray(&camera, self.cursor.0, self.cursor.1) {
                self.scene.hover(ray.origin, ray.dir);
            }
        }

        // Build the HUD before borrowing the renderer / `self.lighting`.
        let hud = self.run_hud(&window);

        let settings = OpticastSettings::for_oracle_framebuffer(size.width, size.height);
        let frame = FrameParams {
            settings: &settings,
            sky_color: SKY_COLOR,
            sky: None,
            fog_color: 0,
            fog_max_scan_dist: 0,
            treat_z_max_as_air: true,
            gpu_mip_scan_dist: 64.0,
            // Enough chunk steps for the GPU marcher to reach the ground
            // grid; 0 renders nothing.
            gpu_max_outer_steps: 64,
            gpu_fov_y_rad: 1.2,
            // Required (Some) for the CPU backend to draw the sprites.
            sprite_lighting: Some(&self.lighting),
        };

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_sprites(self.scene.sprites());
        // roxlap 0.7: render() composites without presenting; the frame
        // is finished by exactly one of paint_egui (HUD) or present.
        renderer.render(self.scene.scene_mut(), &camera, &frame);
        match hud {
            Some((jobs, textures, ppp)) => renderer.paint_egui(&jobs, &textures, ppp),
            None => renderer.present(),
        }

        window.request_redraw();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("monada-host — M0 circle (M1 render slice)")
            .with_inner_size(LogicalSize::new(960.0, 720.0));
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .expect("winit: create_window"),
        );

        // `ROXLAP_GPU=1` tries the wgpu backend; roxlap-render falls back
        // to CPU automatically if init fails.
        let want_gpu = std::env::var_os("ROXLAP_GPU").is_some_and(|v| v != "0" && !v.is_empty());
        let opts = RenderOptions {
            want_gpu,
            ..RenderOptions::default()
        };
        // roxlap-render is now decoupled from winit: it takes any
        // raw-window-handle provider plus an explicit initial size.
        let size = window.inner_size();
        let renderer = SceneRenderer::new(window.clone(), (size.width, size.height), &opts);
        match renderer.adapter_info() {
            Some(info) => eprintln!("monada-host: GPU backend — {info}"),
            None => eprintln!("monada-host: CPU backend"),
        }

        // egui input bridge bound to this window (clipboard / display
        // handle, initial scale factor).
        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            None,
        ));

        window.request_redraw();
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.last_frame = Instant::now();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Let egui see the event first; `consumed` means a widget took it
        // (e.g. a click landed on the HUD), so we skip camera/picking.
        let consumed = match (self.window.clone(), self.egui_state.as_mut()) {
            (Some(window), Some(state)) => state.on_window_event(&window, &event).consumed,
            _ => false,
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.resize(size.width, size.height);
                }
            }
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state,
                        ..
                    },
                ..
            } if !consumed => self.on_key(event_loop, code, state),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if !consumed => self.pick_under_cursor(),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
}

/// Build the HUD widget tree (DESIGN.md §3.2's egui HUD, M1 form).
fn build_hud(ctx: &egui::Context, tick: u64, fps: f32, selected: Option<usize>) {
    egui::Window::new("monada")
        .title_bar(false)
        .resizable(false)
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(8.0, 8.0))
        .show(ctx, |ui| {
            ui.label(format!("tick {tick}"));
            ui.label(format!("fps  {fps:.0}"));
            match selected {
                Some(i) => ui.label(format!("selected mover #{i}")),
                None => ui.label("selected mover —"),
            };
            ui.separator();
            ui.label("arrows orbit · W/S zoom");
            ui.label("click a cube to pick · Esc quit");
        });
}

impl App {
    fn on_key(&mut self, event_loop: &ActiveEventLoop, code: KeyCode, state: ElementState) {
        let down = state == ElementState::Pressed;
        match code {
            KeyCode::Escape => event_loop.exit(),
            KeyCode::ArrowLeft => self.keys.yaw_left = down,
            KeyCode::ArrowRight => self.keys.yaw_right = down,
            KeyCode::ArrowUp => self.keys.pitch_up = down,
            KeyCode::ArrowDown => self.keys.pitch_down = down,
            KeyCode::KeyW => self.keys.zoom_in = down,
            KeyCode::KeyS => self.keys.zoom_out = down,
            _ => {}
        }
    }
}
