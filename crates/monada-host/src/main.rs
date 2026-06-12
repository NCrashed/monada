//! monada native host (DESIGN.md §4) — M1 render bridge + M3 lockstep.
//!
//! Two modes share one winit window, render path, and camera:
//!
//! - **Local** (no args): the M0 "100 entities walk in a circle" sim
//!   (`WALK_CIRCLE_SCRIPT`), single instance, fixed 25 Hz tick with
//!   render-rate interpolation between the last two ticks (DESIGN.md
//!   §3.2). Left-click picks a mover.
//! - **Networked** (`--listen <addr>` / `--connect <addr>`): two hosts
//!   run the command-driven `command_demo` map in lockstep over QUIC
//!   (DESIGN.md §3.1, M3). Only inputs cross the wire; each client
//!   re-derives identical state. Left-click issues a *spawn* command at
//!   the picked point; the HUD shows the desync state; the input stream
//!   is written to a `.replay` on exit.
//!
//! Sim state never holds a float pose — the Q32.32 -> f64 conversion
//! lives entirely in `monada-render`.
//!
//! Controls: arrow keys orbit (yaw/pitch), `W`/`S` zoom, `Esc` quits.

// Host-side float casts (FPS readout, scale factor) are render-side and
// deliberate; the deterministic wall is in monada-sim, not here.
#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
// Prose acronyms in docs (`QUIC`, `HUD`) read worse backticked (matches
// the sim/net crates' stance).
#![allow(clippy::doc_markdown)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use monada_fixed::{Fixed, FixedVec3};
use monada_net::{LockstepSession, MatchInfo, QuicTransport, SessionConfig};
use monada_render::CircleScene;
use monada_script::{
    shared_world, RhaiBackend, RhaiDriver, ScriptBackend, SharedWorld, COMMAND_DEMO_SCRIPT,
    WALK_CIRCLE_SCRIPT,
};
use monada_sim::{ArchetypeId, Command, PlayerId};
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
/// Seed for the scripted scenario's deterministic RNG (`MONADA_0`).
const SEED: u64 = 0x4D4F_4E41_4441_5F30;
/// The walk-circle script declares the mover archetype first.
const MOVER: ArchetypeId = ArchetypeId(0);
/// The command-demo script declares the unit archetype first.
const UNIT: ArchetypeId = ArchetypeId(0);
/// `command_demo` verb: spawn a unit at the command's point.
const SPAWN_VERB: u32 = 1;
/// Packed `0x00RRGGBB` sky / clear colour.
const SKY_COLOR: u32 = 0x0099_B3D9;

/// Camera control rates (per second of held input).
const YAW_RATE: f64 = 1.4;
const PITCH_RATE: f64 = 1.0;
const ZOOM_RATE: f64 = 240.0;

/// Max networked ticks executed per rendered frame. After a stall clears,
/// a backlog of ready ticks would otherwise drain all at once and hitch
/// the render thread; this caps the catch-up so the frame stays
/// responsive and the rest drains over the next frames (still in lockstep
/// — ticks are deferred, never skipped).
const MAX_CATCHUP_TICKS_PER_FRAME: u32 = 8;

/// How the host connects for a networked match.
enum NetRole {
    /// Server / player 0: bind and wait for a peer.
    Listen(SocketAddr),
    /// Client / player 1: connect to a peer.
    Connect(SocketAddr),
}

/// Parse `--listen <addr>` / `--connect <addr>` from argv. Returns `None`
/// for local single-instance play. Exits with a usage message on a
/// malformed flag.
fn parse_net_role() -> Option<NetRole> {
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let role = match flag.as_str() {
            "--listen" => args.next().map(|a| ("--listen", a, true)),
            "--connect" => args.next().map(|a| ("--connect", a, false)),
            _ => continue,
        };
        let Some((name, addr, listen)) = role else {
            eprintln!("monada-host: {flag} needs an <addr> (e.g. 127.0.0.1:5000)");
            std::process::exit(2);
        };
        match addr.parse::<SocketAddr>() {
            Ok(a) if listen => return Some(NetRole::Listen(a)),
            Ok(a) => return Some(NetRole::Connect(a)),
            Err(e) => {
                eprintln!("monada-host: {name} {addr:?}: {e}");
                std::process::exit(2);
            }
        }
    }
    None
}

fn main() {
    let role = parse_net_role();
    let event_loop = EventLoop::new().expect("winit: EventLoop::new");
    // Animate continuously: poll, don't wait for input.
    event_loop.set_control_flow(ControlFlow::Poll);
    match role {
        Some(_) => {
            eprintln!("monada-host: networked — arrows orbit, W/S zoom, click spawns, Esc quits");
        }
        None => eprintln!("monada-host: local — arrows orbit, W/S zoom, click picks, Esc quits"),
    }
    let mut app = App::new(role);
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

/// A live networked lockstep match.
struct Net {
    session: LockstepSession<QuicTransport, RhaiDriver>,
    local: PlayerId,
    /// Local commands queued by clicks, submitted on the next ready tick.
    pending: Vec<Command>,
    /// Set once a desync is detected; the sim freezes and the HUD warns.
    halted: bool,
    replay_path: String,
    /// Guards against writing the replay twice (Esc then CloseRequested).
    saved: bool,
}

/// The simulation behind the render bridge: local single-instance or a
/// networked lockstep session.
enum Sim {
    Local {
        world: SharedWorld,
        // Boxed: a `RhaiBackend` (which owns a whole Rhai `Engine`) and a
        // `LockstepSession` are both large; box each so the two variants
        // stay a similar, small size.
        backend: Box<RhaiBackend>,
    },
    Net(Box<Net>),
}

impl Sim {
    /// The sim tick counter (post-init = 0).
    fn tick(&self) -> u64 {
        match self {
            Sim::Local { world, .. } => world.lock().expect("world mutex").tick,
            Sim::Net(net) => net.session.tick(),
        }
    }

    /// A handle on the world being rendered.
    fn world(&self) -> SharedWorld {
        match self {
            Sim::Local { world, .. } => world.clone(),
            Sim::Net(net) => net.session.driver().world().clone(),
        }
    }

    /// The archetype whose positions the scene renders.
    fn render_arch(&self) -> ArchetypeId {
        match self {
            Sim::Local { .. } => MOVER,
            Sim::Net(_) => UNIT,
        }
    }

    /// Snapshot the rendered archetype's positions.
    fn positions(&self) -> Vec<FixedVec3> {
        let arch = self.render_arch();
        let world = self.world();
        let guard = world.lock().expect("world mutex");
        guard.positions(arch).to_vec()
    }
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<SceneRenderer>,
    scene: CircleScene,
    /// The simulation (local walk-circle or a networked lockstep match).
    sim: Sim,
    /// Sprite positions before and after the most recent fixed step; the
    /// renderer interpolates between them (local mode).
    prev_pos: Vec<FixedVec3>,
    curr_pos: Vec<FixedVec3>,
    /// Number of mover sprites the scene was built for; in networked mode
    /// the unit count grows as players spawn, so the scene is rebuilt when
    /// it changes.
    live_count: usize,
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
    fn new(role: Option<NetRole>) -> App {
        let sim = match role {
            None => Self::new_local(),
            Some(role) => Self::new_net(&role),
        };
        let curr_pos = sim.positions();
        App {
            window: None,
            renderer: None,
            scene: CircleScene::new(curr_pos.len()),
            sim,
            prev_pos: curr_pos.clone(),
            live_count: curr_pos.len(),
            curr_pos,
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

    /// Build the local walk-circle sim (the M1 scenario).
    fn new_local() -> Sim {
        let world = shared_world(SEED);
        let mut backend = RhaiBackend::new(world.clone());
        backend
            .load(WALK_CIRCLE_SCRIPT)
            .expect("compile walk_circle.rhai");
        backend.on_init().expect("script init");
        Sim::Local {
            world,
            backend: Box::new(backend),
        }
    }

    /// Establish the QUIC link (blocks until the peer connects) and start
    /// a lockstep session over the command-demo map.
    fn new_net(role: &NetRole) -> Sim {
        let (transport, local, tag) = match *role {
            NetRole::Listen(addr) => {
                eprintln!("monada-host: listening on {addr} — waiting for a peer…");
                let t = QuicTransport::listen(addr).expect("quic listen");
                (t, PlayerId(0), "host")
            }
            NetRole::Connect(addr) => {
                eprintln!("monada-host: connecting to {addr}…");
                let t = QuicTransport::connect(addr).expect("quic connect");
                (t, PlayerId(1), "client")
            }
        };
        eprintln!("monada-host: peer connected — player {}", local.0);

        let driver = RhaiDriver::new(shared_world(SEED), COMMAND_DEMO_SCRIPT)
            .expect("compile command_demo.rhai");
        let info = MatchInfo {
            seed: SEED,
            map_hash: monada_net::map_hash(COMMAND_DEMO_SCRIPT),
            engine_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let session = LockstepSession::new(
            driver,
            transport,
            local,
            &[PlayerId(0), PlayerId(1)],
            SessionConfig::default(),
            info,
        );
        Sim::Net(Box::new(Net {
            session,
            local,
            pending: Vec::new(),
            halted: false,
            replay_path: format!("monada-{tag}.replay"),
            saved: false,
        }))
    }

    /// Run the egui HUD for this frame and tessellate it. Returns the
    /// paint jobs + texture delta to hand to `paint_egui`, or `None`
    /// before the egui state exists (pre-`resumed`).
    fn run_hud(
        &mut self,
        window: &Window,
    ) -> Option<(Vec<egui::ClippedPrimitive>, egui::TexturesDelta, f32)> {
        let tick = self.sim.tick();
        let fps = self.fps;
        let selected = self.scene.selected();
        let net = match &self.sim {
            Sim::Local { .. } => None,
            Sim::Net(net) => Some(NetHud {
                player: net.local.0,
                units: self.curr_pos.len(),
                halted: net.halted,
                connected: net.session.connected(),
            }),
        };
        let ctx = &self.egui_ctx;
        let state = self.egui_state.as_mut()?;

        let raw = state.take_egui_input(window);
        let out = ctx.run(raw, |ui_ctx| {
            build_hud(ui_ctx, tick, fps, selected, net.as_ref());
        });
        state.handle_platform_output(window, out.platform_output);
        let jobs = ctx.tessellate(out.shapes, out.pixels_per_point);
        Some((jobs, out.textures_delta, out.pixels_per_point))
    }

    /// Handle a left-click: pick a mover (local) or queue a spawn command
    /// at the picked point (networked).
    fn on_click(&mut self) {
        let cam = self.scene.camera();
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let Some(ray) = renderer.view_ray(&cam, self.cursor.0, self.cursor.1) else {
            return;
        };
        match &mut self.sim {
            Sim::Local { .. } => match self.scene.pick_ground(ray.origin, ray.dir) {
                Some(i) => eprintln!("picked mover #{i}"),
                None => eprintln!("picked: (none)"),
            },
            Sim::Net(net) => {
                if let Some((x, y)) = self.scene.ground_sim_xy(ray.origin, ray.dir) {
                    let arg = FixedVec3::new(Fixed::from_f64(x), Fixed::from_f64(y), Fixed::ZERO);
                    net.pending.push(Command::at(SPAWN_VERB, arg));
                    eprintln!("spawn @ ({x:.2}, {y:.2})");
                }
            }
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

    /// Step the local sim on the fixed-timestep accumulator and return the
    /// render interpolation factor.
    fn advance_local(&mut self, dt: f64) -> f64 {
        self.accumulator += dt;
        while self.accumulator >= TICK_DT {
            self.prev_pos.clone_from(&self.curr_pos);
            if let Sim::Local { backend, .. } = &mut self.sim {
                backend.on_tick().expect("script tick");
            }
            self.curr_pos = self.sim.positions();
            self.accumulator -= TICK_DT;
        }
        (self.accumulator / TICK_DT).clamp(0.0, 1.0)
    }

    /// Advance the networked sim: execute every tick whose inputs have
    /// arrived. Queued local commands are handed to `step`, which buffers
    /// them and emits them on the next executed tick — so a stalled frame
    /// never loses a click. Networked ticks are network-paced, not
    /// accumulator-paced, so the render snaps to the current state (no
    /// interpolation).
    fn advance_net(&mut self) {
        if let Sim::Net(net) = &mut self.sim {
            // Bounded catch-up: drain at most a budget of ready ticks this
            // frame; any remainder waits for the next frame.
            let mut budget = MAX_CATCHUP_TICKS_PER_FRAME;
            while !net.halted && budget > 0 {
                // `pending` is non-empty only on the first iteration after a
                // click; `step` buffers it, so a stall holds rather than
                // drops it.
                let cmds = std::mem::take(&mut net.pending);
                match net.session.step(cmds) {
                    Ok(true) => budget -= 1, // advanced; keep draining within budget
                    Ok(false) => break,      // stalled; buffered commands retained
                    Err(desync) => {
                        eprintln!("monada-host: {desync} — halting");
                        net.halted = true;
                    }
                }
            }
        }
        self.curr_pos = self.sim.positions();
        // The unit count grows as players spawn; rebuild the scene (keeping
        // the camera) so every live unit has a sprite instance.
        if self.live_count != self.curr_pos.len() {
            let cam = self.scene.camera;
            self.scene = CircleScene::new(self.curr_pos.len());
            self.scene.camera = cam;
            self.live_count = self.curr_pos.len();
        }
        self.prev_pos.clone_from(&self.curr_pos);
    }

    /// Write the networked match's replay to disk (once), on exit.
    fn save_replay(&mut self) {
        let Sim::Net(net) = &mut self.sim else {
            return;
        };
        if net.saved {
            return;
        }
        net.saved = true;
        match net.session.replay().encode() {
            Ok(bytes) => {
                let ticks = net.session.replay().frames.len();
                match std::fs::write(&net.replay_path, bytes) {
                    Ok(()) => eprintln!(
                        "monada-host: wrote {} ({ticks} input frames)",
                        net.replay_path
                    ),
                    Err(e) => eprintln!("monada-host: failed to write replay: {e}"),
                }
            }
            Err(e) => eprintln!("monada-host: replay encode failed: {e}"),
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

        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f64().min(0.25);
        self.last_frame = now;
        if dt > 0.0 {
            // Exponential smoothing so the HUD reading is steady.
            self.fps = self.fps.mul_add(0.9, (1.0 / dt) as f32 * 0.1);
        }

        // Advance the sim and compute the render blend factor.
        let alpha = match &self.sim {
            Sim::Local { .. } => self.advance_local(dt),
            Sim::Net(_) => {
                self.advance_net();
                1.0
            }
        };

        self.drive_camera(dt);
        self.scene.update(&self.prev_pos, &self.curr_pos, alpha);

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
            .with_title("monada-host")
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
            WindowEvent::CloseRequested => {
                self.save_replay();
                event_loop.exit();
            }
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
            } if !consumed => self.on_click(),
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }
}

/// HUD fields specific to a networked match.
struct NetHud {
    player: u32,
    units: usize,
    halted: bool,
    connected: bool,
}

/// Build the HUD widget tree (DESIGN.md §3.2's egui HUD).
fn build_hud(
    ctx: &egui::Context,
    tick: u64,
    fps: f32,
    selected: Option<usize>,
    net: Option<&NetHud>,
) {
    egui::Window::new("monada")
        .title_bar(false)
        .resizable(false)
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(8.0, 8.0))
        .show(ctx, |ui| {
            ui.label(format!("tick {tick}"));
            ui.label(format!("fps  {fps:.0}"));
            match net {
                None => {
                    match selected {
                        Some(i) => ui.label(format!("selected mover #{i}")),
                        None => ui.label("selected mover —"),
                    };
                    ui.separator();
                    ui.label("arrows orbit · W/S zoom");
                    ui.label("click a cube to pick · Esc quit");
                }
                Some(net) => {
                    ui.label(format!("player {} · {} units", net.player, net.units));
                    if net.halted {
                        ui.colored_label(egui::Color32::RED, "DESYNC — halted");
                    } else if net.connected {
                        ui.label("lockstep · in sync");
                    } else {
                        ui.colored_label(egui::Color32::RED, "peer lost — no reconnect");
                    }
                    ui.separator();
                    ui.label("arrows orbit · W/S zoom");
                    ui.label("click to spawn · Esc quit");
                }
            }
        });
}

impl App {
    fn on_key(&mut self, event_loop: &ActiveEventLoop, code: KeyCode, state: ElementState) {
        let down = state == ElementState::Pressed;
        match code {
            KeyCode::Escape => {
                self.save_replay();
                event_loop.exit();
            }
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
