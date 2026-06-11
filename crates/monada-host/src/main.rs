//! monada native host (DESIGN.md §4) — M1 render-bridge slice.
//!
//! Drives the M0 "100 entities walk in a circle" sim through the
//! `monada-render` bridge and roxlap's CPU render facade in a winit
//! window: the sim ticks at a fixed rate, the renderer runs at display
//! rate and interpolates between the last two sim ticks (DESIGN.md
//! §3.2). Sim state never holds a float pose — the Q32.32 -> f64
//! conversion lives entirely in `monada-render`.
//!
//! Controls: arrow keys orbit (yaw/pitch), `W`/`S` zoom, `Esc` quits.
//!
//! Picking and the egui HUD are the next M1 slices.

use std::sync::Arc;
use std::time::Instant;

use monada_render::CircleScene;
use monada_sim::scenario::{CircleSim, DEFAULT_COUNT};
use monada_sim::Simulation;
use roxlap_core::opticast::OpticastSettings;
use roxlap_core::sprite::SpriteLighting;
use roxlap_render::{FrameParams, RenderOptions, SceneRenderer};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
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
            debug_done: false,
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
        renderer.render(self.scene.scene_mut(), &camera, &frame);

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

        window.request_redraw();
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.last_frame = Instant::now();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
            } => self.on_key(event_loop, code, state),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
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
