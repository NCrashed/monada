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

// Host-side float casts (FPS readout, scale/camera math) are render-side
// and deliberate; the deterministic wall is in monada-sim, not here. The
// sign-loss / wrap casts convert small sim values (entity / model ids,
// voxel coords, colours) for the renderer and ids — never onto the
// deterministic path.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
// Prose acronyms in docs (`QUIC`, `HUD`) read worse backticked (matches
// the sim/net crates' stance).
#![allow(clippy::doc_markdown)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use glam::DVec3;
use monada_fixed::{Fixed, FixedVec3};
use monada_format::{Map, SimHz};
use monada_net::{LockstepSession, MatchInfo, QuicTransport, Replay, SessionConfig, SimDriver};
use monada_render::CircleScene;
use monada_script::{
    shared_world, RhaiBackend, RhaiDriver, ScriptBackend, SharedBridge, SharedWorld,
    COMMAND_DEMO_SCRIPT, WALK_CIRCLE_SCRIPT,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

pub mod autotile;
pub mod cli;
mod audio;
mod map_render;
use audio::Audio;
use map_render::MapRender;
use roxlap_core::opticast::OpticastSettings;
use roxlap_core::Camera;
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
/// Reserved verb for the host's per-tick real-time input snapshot, injected
/// into a fixed-rate map's command stream (the generic WASD/dodge/attack
/// primitive). `target` carries the button bitmask, `arg = vec3(move_x,
/// move_y, aim)`. A map decodes it in its `command(player, verb, ...)`
/// handler; the engine attaches no meaning to it.
const VERB_INPUT: u32 = 0;
/// Real-time input button bits packed into a [`VERB_INPUT`] command's target.
const BTN_ATTACK: u64 = 1 << 0;
const BTN_DODGE: u64 = 1 << 1;
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
pub enum NetRole {
    /// Server / player 0: bind and wait for a peer.
    Listen(SocketAddr),
    /// Client / player 1: connect to a peer.
    Connect(SocketAddr),
}

/// A scripted map to run locally (the hotseat path): the loaded archive,
/// whose entry script the backend runs and whose `assets/` the render
/// bridge resolves. The host is genre-agnostic — the map paints its own
/// board and defines its own pieces/interaction.
pub struct MapRun {
    pub map: Map,
}

/// What the host runs this session. Built by the CLI (`main.rs`) or by a
/// launcher like `monada-chess`, then handed to [`run`].
pub enum RunConfig {
    /// The M1 walk-in-a-circle sim, single instance.
    Local,
    /// A two-process lockstep match of the `command_demo` map (M3).
    Net(NetRole),
    /// A scripted map loaded from an archive. `net` = `None` is a local
    /// hotseat (one window, both sides); `Some(role)` is a two-process
    /// lockstep match over QUIC, each peer playing its own side.
    Map { run: MapRun, net: Option<NetRole> },
    /// Watch a recorded `.replay` against its map: the input stream is
    /// re-applied on a timer and rendered (DESIGN.md §3.1). The caller has
    /// already verified the replay's map hash + engine version.
    Replay { run: MapRun, replay: Replay },
}

/// Run the host event loop for `config` (blocks until the window closes).
///
/// # Panics
/// Panics if the winit event loop / window cannot be created, or if a
/// fixed map asset (the script) fails to compile — environment / build
/// faults the host cannot proceed past, matching its `expect`-on-asset
/// stance elsewhere.
pub fn run(config: RunConfig) {
    let event_loop = EventLoop::new().expect("winit: EventLoop::new");
    // Animate continuously: poll, don't wait for input.
    event_loop.set_control_flow(ControlFlow::Poll);
    match &config {
        RunConfig::Net(_) => {
            eprintln!("monada-host: networked — arrows orbit, W/S zoom, click spawns, Esc quits");
        }
        RunConfig::Map { run, net } => {
            let how = if net.is_some() { "LAN" } else { "local" };
            eprintln!(
                "monada-host: {} ({how}) — arrows orbit, W/S zoom, click to interact, Esc quits",
                run.map.manifest.name
            );
        }
        RunConfig::Replay { run, .. } => {
            eprintln!(
                "monada-host: replaying {} — arrows orbit, W/S zoom, [ ] speed, Space pause, Esc quits",
                run.map.manifest.name
            );
        }
        RunConfig::Local => {
            eprintln!("monada-host: local — arrows orbit, W/S zoom, click picks, Esc quits");
        }
    }
    let mut app = App::new(config);
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

/// Real-time gameplay input for a fixed-rate map: a held-key snapshot the
/// host samples once per frame and injects as one [`VERB_INPUT`] command per
/// sim tick. Generic (move axis + dodge + attack) — the map decides what the
/// axes and buttons mean. `attack` tracks the left mouse button held.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default, Clone, Copy)]
struct Input {
    fwd: bool,
    back: bool,
    left: bool,
    right: bool,
    dodge: bool,
    attack: bool,
    /// Aim direction (sim radians): the world-space angle from the local
    /// player (the camera focus) to the cursor's ground point. Computed
    /// host-side per frame; the map uses it for e.g. attack direction.
    aim_yaw: f64,
    /// Button bits contributed by HUD `ui_button` clicks this frame (the map
    /// chooses each button's bit). OR-ed into the command's button mask, then
    /// cleared — a click is a one-tick edge, not a held key.
    ui_bits: u64,
}

impl Input {
    /// Pack this snapshot into the per-tick command the map decodes: integer
    /// move axis in `arg.xy` (no float crosses the wall), buttons bitmask in
    /// `target`, mouse-aim yaw (radians) in `arg.z`.
    fn to_command(self) -> Command {
        let mx = i32::from(self.right) - i32::from(self.left);
        let my = i32::from(self.fwd) - i32::from(self.back);
        let buttons = (u64::from(self.attack) * BTN_ATTACK)
            | (u64::from(self.dodge) * BTN_DODGE)
            | self.ui_bits;
        Command::on(
            VERB_INPUT,
            EntityId(buttons),
            FixedVec3::new(
                Fixed::from_int(mx),
                Fixed::from_int(my),
                Fixed::from_f64(self.aim_yaw),
            ),
        )
    }
}

/// The map's tick model as a render-loop pace: `Some(1/hz)` for a fixed-rate
/// (real-time) map, `None` for a command-driven (turn-based) one.
fn tick_dt(hz: SimHz) -> Option<f64> {
    match hz {
        SimHz::OnCommand => None,
        SimHz::Fixed(h) => Some(1.0 / f64::from(h.max(1))),
    }
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

/// A local scripted-map match (hotseat). The host knows no genre: the map
/// paints its board, defines its pieces, and runs its interaction in the
/// script. The render + bridge state lives in [`MapRender`] (shared with
/// the Rhai engine as a [`HostBridge`](monada_script::HostBridge)).
struct MapSim {
    world: SharedWorld,
    backend: Box<RhaiBackend>,
    render: Arc<Mutex<MapRender>>,
    /// `Some(1/hz)` for a real-time map (drive its `tick` on the wall clock),
    /// `None` for a command-driven one (chess: advance only on clicks).
    tick_dt: Option<f64>,
    accumulator: f64,
    /// Player id attributed to this peer's input commands (hotseat = 0).
    local: PlayerId,
}

impl MapSim {
    /// Drive a real-time (fixed-rate) map on the wall clock: each accumulated
    /// tick, feed the local input snapshot as one command, run the map's
    /// `tick`, then apply any commands the script queued. A command-driven
    /// map (`tick_dt == None`) does nothing here — it advances only on
    /// clicks (`pointer`).
    fn advance(&mut self, dt: f64, input: Input) -> bool {
        let Some(step) = self.tick_dt else {
            return false;
        };
        self.accumulator = (self.accumulator + dt).min(0.25);
        let input_cmd = input.to_command();
        let mut budget = MAX_CATCHUP_TICKS_PER_FRAME;
        let mut stepped = false;
        while self.accumulator >= step && budget > 0 {
            self.backend
                .on_command(self.local, &input_cmd)
                .expect("map input command");
            self.backend.on_tick().expect("map tick");
            let commands = self.render.lock().expect("render mutex").drain_commands();
            for command in commands {
                self.backend
                    .on_command(self.local, &command)
                    .expect("map command handler");
            }
            self.backend.drain_ui_events();
            self.accumulator -= step;
            budget -= 1;
            stepped = true;
        }
        stepped
    }

    /// Forward a pointer click to the map's `pointer` handler, then route
    /// whatever commands the gesture queued. Hotseat: commands apply
    /// immediately. The player id is a placeholder — the script enforces
    /// turn from game state, not the id; the networked player↔command
    /// mapping lands in slice 4.
    fn pointer(&mut self, button: i64, origin: DVec3, dir: DVec3) {
        let (point, entity) = {
            let r = self.render.lock().expect("render mutex");
            let w = self.world.lock().expect("world mutex");
            r.pick(&w, origin, dir)
        };
        self.backend
            .on_pointer(button, point, entity)
            .expect("map pointer handler");
        let commands = self.render.lock().expect("render mutex").drain_commands();
        for command in commands {
            self.backend
                .on_command(PlayerId(0), &command)
                .expect("map command handler");
        }
        // Status updates flow through the bridge; nothing to mirror here.
        self.backend.drain_ui_events();
    }
}

/// A networked scripted-map match: two processes over QUIC lockstep, each
/// peer playing its own side. Like [`MapSim`], but a move command — instead
/// of applying locally — is routed through the [`LockstepSession`] so both
/// peers re-derive identical state from the shared input stream.
struct NetMapSim {
    session: LockstepSession<QuicTransport, RhaiDriver>,
    render: Arc<Mutex<MapRender>>,
    local: PlayerId,
    /// Local commands queued by clicks; submitted on the next ready tick.
    pending: Vec<Command>,
    /// `Some(1/hz)` for a real-time map (submit a per-tick input command),
    /// `None` for a command-driven one (chess: clicks only).
    tick_dt: Option<f64>,
    accumulator: f64,
    /// True when a real-time input command is buffered in the session's
    /// outbox during a stall, so we don't submit a duplicate next frame
    /// (the session collapses everything buffered into one tick's bundle).
    input_pending: bool,
    halted: bool,
    replay_path: String,
    saved: bool,
}

impl NetMapSim {
    /// Run the map's pointer gesture on the live networked world. The
    /// command it queues is routed through the session (`pending` → `step`),
    /// not applied locally — both peers apply it in lockstep. The script's
    /// `local_player()` gating means only the side-to-move client submits.
    fn pointer(&mut self, button: i64, origin: DVec3, dir: DVec3) {
        let world = self.session.driver().world().clone();
        let (point, entity) = {
            let r = self.render.lock().expect("render mutex");
            let w = world.lock().expect("world mutex");
            r.pick(&w, origin, dir)
        };
        self.session
            .driver_mut()
            .on_pointer(button, point, entity)
            .expect("map pointer handler");
        let commands = self.render.lock().expect("render mutex").drain_commands();
        self.pending.extend(commands);
    }

    /// Advance the lockstep sim. A real-time map paces tick execution on the
    /// wall clock and submits one input command per scheduled tick; a
    /// command-driven map (chess) executes every ready tick, routing only
    /// queued clicks. Queued commands are buffered by `step`, never dropped.
    fn advance(&mut self, dt: f64, input: Input) -> bool {
        let Some(step) = self.tick_dt else {
            // Command-driven (chess): execute every ready tick, routing only
            // queued clicks.
            let mut budget = MAX_CATCHUP_TICKS_PER_FRAME;
            while !self.halted && budget > 0 {
                let cmds = std::mem::take(&mut self.pending);
                match self.session.step(cmds) {
                    Ok(true) => budget -= 1,
                    Ok(false) => break,
                    Err(desync) => {
                        eprintln!("monada-host: {desync} — halting");
                        self.halted = true;
                    }
                }
            }
            return false;
        };

        // Real-time: pace tick execution on the wall clock, submitting one
        // input command per scheduled tick.
        self.accumulator = (self.accumulator + dt).min(0.25);
        let input_cmd = input.to_command();
        let mut budget = MAX_CATCHUP_TICKS_PER_FRAME;
        let mut consumed_input = false;
        while !self.halted && self.accumulator >= step && budget > 0 {
            let mut cmds = std::mem::take(&mut self.pending);
            // One input per scheduled tick: don't re-submit one the session
            // already buffered while stalled.
            let submit_input = !self.input_pending;
            if submit_input {
                cmds.push(input_cmd);
            }
            match self.session.step(cmds) {
                Ok(true) => {
                    if submit_input {
                        consumed_input = true;
                    }
                    self.input_pending = false;
                    self.accumulator -= step;
                    budget -= 1;
                }
                Ok(false) => {
                    // Stalled: anything passed is buffered in the session's
                    // outbox; remember the input is there.
                    if submit_input {
                        self.input_pending = true;
                    }
                    break;
                }
                Err(desync) => {
                    eprintln!("monada-host: {desync} — halting");
                    self.halted = true;
                }
            }
        }
        consumed_input
    }
}

/// Default pace for a **command-driven** map's replay: seconds per move
/// (idle ticks between moves are re-run instantly). Fixed-Hz maps pace at
/// `1/hz` per tick instead.
const REPLAY_MOVE_DT: f64 = 0.7;

/// Recorded commands by execution tick (sparse — only ticks that had
/// input; idle ticks are re-run, not stored).
type ReplayByTick = BTreeMap<u64, Vec<(PlayerId, Vec<Command>)>>;

/// Watching a recorded `.replay`: every executed tick `0..total` is re-run
/// on a fresh driver (applying recorded commands at their ticks), then
/// rendered. Paced by the map's `sim_hz` — `1/hz` per tick for a fixed-rate
/// map, or one move per [`REPLAY_MOVE_DT`] for a command-driven one (idle
/// ticks free). No interaction; no network.
struct ReplaySim {
    driver: RhaiDriver,
    render: Arc<Mutex<MapRender>>,
    by_tick: ReplayByTick,
    /// Total executed ticks to re-run.
    total: u64,
    /// Next tick to execute.
    cursor: u64,
    /// Seconds per paced unit (per move if `command_driven`, else per tick).
    step_dt: f64,
    /// Command-driven map: idle ticks cost no time; only moves are paced.
    command_driven: bool,
    /// Playback speed multiplier (adjusted with the `[` / `]` keys).
    speed: f64,
    paused: bool,
    /// Real seconds accumulated toward the next paced step.
    elapsed: f64,
}

impl ReplaySim {
    /// Re-run recorded ticks as real time passes. Idle ticks of a command-
    /// driven map advance for free; paced ticks/moves wait `step_dt/speed`.
    fn advance(&mut self, dt: f64) {
        if self.paused || self.cursor >= self.total {
            return;
        }
        self.elapsed += dt * self.speed;
        while self.cursor < self.total {
            let has_cmd = self.by_tick.contains_key(&self.cursor);
            let cost = if self.command_driven && !has_cmd {
                0.0
            } else {
                self.step_dt
            };
            if self.elapsed < cost {
                break;
            }
            self.elapsed -= cost;
            if let Some(cmds) = self.by_tick.get(&self.cursor) {
                for (player, list) in cmds {
                    for command in list {
                        self.driver.apply_command(*player, command);
                    }
                }
            }
            self.driver.step();
            self.cursor += 1;
        }
    }

    /// Multiply the playback speed, clamped to a sane range.
    fn scale_speed(&mut self, factor: f64) {
        self.speed = (self.speed * factor).clamp(0.125, 16.0);
    }
}

/// The simulation behind the render bridge: local single-instance, a
/// networked lockstep session, a local / networked scripted map, or a
/// replay being watched.
enum Sim {
    Local {
        world: SharedWorld,
        // Boxed: a `RhaiBackend` (which owns a whole Rhai `Engine`) and a
        // `LockstepSession` are both large; box each so the two variants
        // stay a similar, small size.
        backend: Box<RhaiBackend>,
    },
    Net(Box<Net>),
    Map(Box<MapSim>),
    NetMap(Box<NetMapSim>),
    Replay(Box<ReplaySim>),
}

impl Sim {
    /// The sim tick counter (post-init = 0).
    fn tick(&self) -> u64 {
        match self {
            Sim::Local { world, .. } => world.lock().expect("world mutex").tick,
            Sim::Net(net) => net.session.tick(),
            Sim::Map(map) => map.world.lock().expect("world mutex").tick,
            Sim::NetMap(nm) => nm.session.tick(),
            Sim::Replay(r) => r.driver.world().lock().expect("world mutex").tick,
        }
    }

    /// A handle on the world being rendered.
    fn world(&self) -> SharedWorld {
        match self {
            Sim::Local { world, .. } => world.clone(),
            Sim::Net(net) => net.session.driver().world().clone(),
            Sim::Map(map) => map.world.clone(),
            Sim::NetMap(nm) => nm.session.driver().world().clone(),
            Sim::Replay(r) => r.driver.world().clone(),
        }
    }

    /// Snapshot the rendered archetype's positions (circle/net movers).
    /// The map paths render generically from `MapRender`, so they need no
    /// position snapshot here.
    fn positions(&self) -> Vec<FixedVec3> {
        let arch = match self {
            Sim::Local { .. } => MOVER,
            Sim::Net(_) => UNIT,
            Sim::Map(_) | Sim::NetMap(_) | Sim::Replay(_) => return Vec::new(),
        };
        let world = self.world();
        let guard = world.lock().expect("world mutex");
        guard.positions(arch).to_vec()
    }
}

/// The render scene, one per sim flavour. `Circle` is the M1/M3 mover
/// scene (local + net); `Map` is the generic [`MapRender`] (shared with
/// the script engine). Per-frame `set_sprites` + `render` is done inline
/// in [`App::redraw`] because `Map`'s state lives behind a `Mutex` and
/// can't hand out borrows through an accessor.
// One `App` holds exactly one scene, so the Circle/Map size gap is a
// non-issue — boxing the circle scene would only add an indirection.
#[allow(clippy::large_enum_variant)]
enum SceneKind {
    Circle(CircleScene),
    Map(Arc<Mutex<MapRender>>),
}

impl SceneKind {
    fn camera(&self) -> Camera {
        match self {
            SceneKind::Circle(s) => s.camera(),
            SceneKind::Map(r) => r.lock().expect("render mutex").camera(),
        }
    }

    fn orbit(&mut self, dyaw: f64, dpitch: f64, ddist: f64) {
        match self {
            SceneKind::Circle(s) => s.camera.orbit(dyaw, dpitch, ddist),
            SceneKind::Map(r) => r.lock().expect("render mutex").orbit(dyaw, dpitch, ddist),
        }
    }

    /// Track the picking ray (circle scene only; the map has no hover
    /// marker).
    fn hover(&mut self, origin: DVec3, dir: DVec3) {
        if let SceneKind::Circle(s) = self {
            s.hover(origin, dir);
        }
    }
}

struct App {
    // Field order matters for GPU teardown: the renderer owns the wgpu
    // surface/device, which must drop *before* the window it was created from.
    // Rust drops fields top-to-bottom, so `renderer` is declared before
    // `window` — this keeps the order correct even on the panic-unwind path,
    // where `exiting` (the graceful `wait_idle` teardown) never runs. Dropping
    // the surface after the window can leave the driver/compositor showing
    // stale buffers (roxlap's "leftover triangles / flicker" symptom).
    renderer: Option<SceneRenderer>,
    window: Option<Arc<Window>>,
    scene: SceneKind,
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
    accumulator: f64,
    last_frame: Instant,
    keys: Keys,
    /// Real-time gameplay input (WASD / dodge / attack), sampled per frame
    /// and injected per tick into a fixed-rate map.
    input: Input,
    /// Last cursor position in physical pixels, for click picking.
    cursor: (f64, f64),
    /// Smoothed frames-per-second for the HUD.
    fps: f32,
    /// egui context + winit input bridge for the HUD overlay.
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    /// egui texture handles for the map's HUD images, indexed by the id
    /// `ui_texture` returned to the script (uploaded lazily, once each).
    ui_tex_cache: Vec<Option<egui::TextureHandle>>,
    /// The audio mixer (feature `audio`; a no-op shim otherwise). Fed each
    /// frame by `MapRender::drain_audio`; owns the rodio output (`!Send`).
    audio: Audio,
    /// Whether the debug overlay (tick / FPS / status / lockstep line) is
    /// shown. Hidden by default so only the map's own HUD is visible; F1
    /// toggles it.
    debug_hud: bool,
    /// One-shot coordinate dump (set `MONADA_DEBUG=1`).
    debug_done: bool,
}

impl App {
    fn new(config: RunConfig) -> App {
        let sim = match config {
            RunConfig::Local => Self::new_local(),
            RunConfig::Net(role) => Self::new_net(&role),
            RunConfig::Map { run, net: None } => Self::new_map(run),
            RunConfig::Map {
                run,
                net: Some(role),
            } => Self::new_net_map(run, &role),
            RunConfig::Replay { run, replay } => Self::new_replay(run, &replay),
        };
        let curr_pos = sim.positions();
        let scene = match &sim {
            // The map scenes share the render bridge the script writes to.
            Sim::Map(map) => SceneKind::Map(map.render.clone()),
            Sim::NetMap(nm) => SceneKind::Map(nm.render.clone()),
            Sim::Replay(r) => SceneKind::Map(r.render.clone()),
            _ => SceneKind::Circle(CircleScene::new(curr_pos.len())),
        };
        // Hand the map's sound assets to the mixer once (opens the audio device
        // now; a headless box with no output just mutes). Non-map scenes carry
        // no sounds.
        let sounds = match &scene {
            SceneKind::Map(render) => render.lock().expect("render mutex").sound_assets(),
            SceneKind::Circle(_) => Vec::new(),
        };
        App {
            audio: Audio::new(sounds),
            window: None,
            renderer: None,
            scene,
            sim,
            prev_pos: curr_pos.clone(),
            live_count: curr_pos.len(),
            curr_pos,
            accumulator: 0.0,
            last_frame: Instant::now(),
            keys: Keys::default(),
            input: Input::default(),
            cursor: (0.0, 0.0),
            fps: 0.0,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            ui_tex_cache: Vec::new(),
            debug_hud: false,
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
            map_hash: monada_format::hash(COMMAND_DEMO_SCRIPT.as_bytes()),
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

    /// Build a local scripted-map match (the M4 hotseat). The host is
    /// genre-agnostic: it wires the map's `assets` into a [`MapRender`],
    /// hands that to the backend as the [`HostBridge`](monada_script::HostBridge),
    /// then runs `init` — the script paints its board, defines its models,
    /// spawns its entities, and sets the HUD status.
    fn new_map(run: MapRun) -> Sim {
        let world = shared_world(SEED);
        let mut backend = RhaiBackend::new(world.clone());
        let script = run
            .map
            .entry_script()
            .expect("map declares an entry script")
            .to_string();
        // Hotseat: one window drives every side, so there is no single
        // local player (-1) — the script enforces turns by piece colour.
        let render = Arc::new(Mutex::new(MapRender::new(run.map.assets, None)));
        // Bridge must be set before `init` calls model_box / voxel_fill / …
        let bridge: SharedBridge = render.clone();
        backend.set_bridge(&bridge);
        backend.load(&script).expect("compile map script");
        backend.on_init().expect("map init");
        backend.drain_ui_events();
        Sim::Map(Box::new(MapSim {
            world,
            backend: Box::new(backend),
            render,
            tick_dt: tick_dt(run.map.manifest.sim_hz),
            accumulator: 0.0,
            // Hotseat: one local player drives the real-time input.
            local: PlayerId(0),
        }))
    }

    /// Build a networked scripted-map match: connect over QUIC, then run
    /// the same map as the hotseat but route moves through a lockstep
    /// session. Each peer is a fixed player id (`listen` = 0, `connect` =
    /// 1); the map's `local_player()` gating ties that to the side it may
    /// move. The map identity is the archive's SHA-256.
    fn new_net_map(run: MapRun, role: &NetRole) -> Sim {
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

        let script = run
            .map
            .entry_script()
            .expect("map declares an entry script")
            .to_string();
        // This peer plays the side matching its player id; the script gates
        // off-turn input on `local_player()`.
        let render = Arc::new(Mutex::new(MapRender::new(
            run.map.assets,
            Some(i64::from(local.0)),
        )));
        let bridge: SharedBridge = render.clone();
        let driver = RhaiDriver::with_bridge(shared_world(SEED), &script, &bridge)
            .expect("compile map script");
        let info = MatchInfo {
            seed: SEED,
            map_hash: run.map.hash,
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
        Sim::NetMap(Box::new(NetMapSim {
            session,
            render,
            local,
            pending: Vec::new(),
            tick_dt: tick_dt(run.map.manifest.sim_hz),
            accumulator: 0.0,
            input_pending: false,
            halted: false,
            replay_path: format!("monada-{tag}.replay"),
            saved: false,
        }))
    }

    /// Build a replay viewer: a fresh driver seeded from the replay, the
    /// recorded input stream grouped by tick for paced re-application, and a
    /// render bridge to draw it. The caller has already verified the
    /// replay's map hash + engine version against `run`.
    fn new_replay(run: MapRun, replay: &Replay) -> Sim {
        let script = run
            .map
            .entry_script()
            .expect("map declares an entry script")
            .to_string();
        // Pace from the map's declared tick model: a fixed-Hz map replays at
        // its real rate (1/hz per tick); a command-driven map replays one
        // move at a time (idle ticks re-run instantly).
        let (command_driven, step_dt) = match run.map.manifest.sim_hz {
            SimHz::OnCommand => (true, REPLAY_MOVE_DT),
            SimHz::Fixed(hz) => (false, 1.0 / f64::from(hz.max(1))),
        };
        let render = Arc::new(Mutex::new(MapRender::new(run.map.assets, None)));
        let bridge: SharedBridge = render.clone();
        let driver = RhaiDriver::with_bridge(shared_world(replay.seed), &script, &bridge)
            .expect("compile map script");

        // Consume the replay's own canonical grouping — the *same* source
        // `Replay::playback` uses, so the paced viewer can't diverge from
        // the verified playback.
        let by_tick: ReplayByTick = replay.steps().into_iter().collect();
        eprintln!(
            "monada-host: replaying {} ticks ({} with input)",
            replay.ticks,
            replay.frames.len()
        );

        Sim::Replay(Box::new(ReplaySim {
            driver,
            render,
            by_tick,
            total: replay.ticks,
            cursor: 0,
            step_dt,
            command_driven,
            speed: 1.0,
            paused: false,
            elapsed: 0.0,
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
        let hud = match &self.sim {
            Sim::Local { .. } => HudState::Local {
                selected: match &self.scene {
                    SceneKind::Circle(s) => s.selected(),
                    SceneKind::Map(_) => None,
                },
            },
            Sim::Net(net) => HudState::Net(NetHud {
                player: net.local.0,
                units: self.curr_pos.len(),
                halted: net.halted,
                connected: net.session.connected(),
            }),
            // The map owns its HUD text — the host just shows whatever the
            // script set via `status(...)`, knowing nothing of its meaning.
            Sim::Map(map) => HudState::Map {
                status: map
                    .render
                    .lock()
                    .expect("render mutex")
                    .status_text()
                    .to_string(),
                net: None,
            },
            Sim::NetMap(nm) => HudState::Map {
                status: nm
                    .render
                    .lock()
                    .expect("render mutex")
                    .status_text()
                    .to_string(),
                net: Some(MapNet {
                    player: nm.local.0,
                    halted: nm.halted,
                    connected: nm.session.connected(),
                }),
            },
            Sim::Replay(r) => {
                let status = r
                    .render
                    .lock()
                    .expect("render mutex")
                    .status_text()
                    .to_string();
                let pace = if r.paused {
                    "paused".to_string()
                } else {
                    format!("{:.2}x", r.speed)
                };
                HudState::Map {
                    status: format!("{status} · replay {}/{} · {pace}", r.cursor, r.total),
                    net: None,
                }
            }
        };
        // Clone the (Arc-backed) context so `self` is free for the mutable
        // borrows the map-HUD pass needs (texture cache + input bits).
        let ctx = self.egui_ctx.clone();
        let raw = self.egui_state.as_mut()?.take_egui_input(window);
        // egui 0.34 deprecated `Context::run` (its `run_ui` hands a `&mut Ui`,
        // but `build_hud` paints free-floating `egui::Window`s, which want the
        // `&Context`) — drive a pass explicitly instead.
        ctx.begin_pass(raw);
        // Debug overlay (tick / FPS / status / lockstep) only when toggled on
        // with F1 — otherwise just the map's own HUD shows.
        if self.debug_hud {
            build_hud(&ctx, tick, fps, &hud);
        }
        // The map's own scripted HUD (health bar / panels / buttons), painted
        // over the status window; button clicks feed the next tick's command.
        self.paint_map_hud(&ctx);
        let out = ctx.end_pass();
        self.egui_state
            .as_mut()?
            .handle_platform_output(window, out.platform_output);
        let jobs = ctx.tessellate(out.shapes, out.pixels_per_point);
        Some((jobs, out.textures_delta, out.pixels_per_point))
    }

    /// Paint the scripted map HUD ([`MapRender::ui_widgets`]) this frame and
    /// route button clicks into the next tick's input command. Screen-space,
    /// render-side only — never touches the sim state hash.
    fn paint_map_hud(&mut self, ctx: &egui::Context) {
        let SceneKind::Map(render) = &self.scene else {
            return;
        };
        let render = render.clone();
        let size = ctx.content_rect().size();
        let (widgets, scale) = {
            let mut r = render.lock().expect("render mutex");
            r.set_ui_viewport(size.x as i64, size.y as i64);
            (r.ui_widgets().to_vec(), r.ui_scale_factor())
        };
        if widgets.is_empty() {
            return;
        }

        let mut clicked_bits = 0u64;
        for (i, w) in widgets.iter().enumerate() {
            match w {
                map_render::UiWidget::Image { tex, x, y } => {
                    if let Some(h) = self.ui_handle(ctx, &render, *tex) {
                        Self::ui_area(ctx, i, *x, *y, |ui| {
                            let sz = tex_points(&h) * scale;
                            ui.add(egui::Image::new(&h).fit_to_exact_size(sz));
                        });
                    }
                }
                map_render::UiWidget::ImageClip { tex, x, y, frac } => {
                    if let Some(h) = self.ui_handle(ctx, &render, *tex) {
                        Self::ui_area(ctx, i, *x, *y, |ui| {
                            let full = tex_points(&h) * scale;
                            let sz = egui::vec2(full.x * frac, full.y);
                            let uv = egui::Rect::from_min_max(
                                egui::pos2(0.0, 0.0),
                                egui::pos2(*frac, 1.0),
                            );
                            // Paint into the exact clipped rect: `fit_to_exact_size`
                            // keeps aspect and would letterbox (shrinking the bar
                            // in Y as the width drops), so allocate + `paint_at`.
                            let (rect, _) = ui.allocate_exact_size(sz, egui::Sense::hover());
                            egui::Image::new(&h).uv(uv).paint_at(ui, rect);
                        });
                    }
                }
                map_render::UiWidget::Text { x, y, text, size } => {
                    Self::ui_area(ctx, i, *x, *y, |ui| {
                        // Never wrap — a HUD number like "20" must stay on one
                        // line (the default wraps it to "2" / "0" near an edge).
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(text)
                                    .size(*size * scale)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    });
                }
                map_render::UiWidget::Button {
                    tex,
                    hover,
                    pressed,
                    x,
                    y,
                    bit,
                } => {
                    // Resolve all three state textures up front (immutable
                    // borrow of self ends before the closure borrows it again).
                    let normal = self.ui_handle(ctx, &render, *tex);
                    let hovered = self.ui_handle(ctx, &render, *hover);
                    let held = self.ui_handle(ctx, &render, *pressed);
                    let Some(normal) = normal else { continue };
                    Self::ui_area(ctx, i, *x, *y, |ui| {
                        let sz = tex_points(&normal) * scale;
                        let (rect, resp) = ui.allocate_exact_size(sz, egui::Sense::click());
                        let shown = if resp.is_pointer_button_down_on() {
                            held.as_ref().unwrap_or(&normal)
                        } else if resp.hovered() {
                            hovered.as_ref().unwrap_or(&normal)
                        } else {
                            &normal
                        };
                        egui::Image::new(shown).paint_at(ui, rect);
                        if resp.clicked() {
                            clicked_bits |= *bit;
                        }
                    });
                }
            }
        }
        // A click is a one-tick edge: OR it in; the next `to_command` consumes
        // it and `clear_ui_bits` wipes it so it fires exactly once.
        self.input.ui_bits |= clicked_bits;
    }

    /// A fixed-position, foreground overlay area for one HUD widget.
    fn ui_area(
        ctx: &egui::Context,
        id: usize,
        x: i32,
        y: i32,
        add: impl FnOnce(&mut egui::Ui),
    ) {
        egui::Area::new(egui::Id::new(("monada_ui", id)))
            .order(egui::Order::Foreground)
            .fixed_pos(egui::pos2(x as f32, y as f32))
            .show(ctx, |ui| add(ui));
    }

    /// The egui texture handle for a map HUD texture id, uploading it (nearest
    /// filtered, for crisp pixel art) the first time it is referenced.
    fn ui_handle(
        &mut self,
        ctx: &egui::Context,
        render: &Arc<Mutex<MapRender>>,
        id: usize,
    ) -> Option<egui::TextureHandle> {
        if id >= self.ui_tex_cache.len() {
            self.ui_tex_cache.resize(id + 1, None);
        }
        if self.ui_tex_cache[id].is_none() {
            let r = render.lock().expect("render mutex");
            let (pixels, w, h) = r.ui_texture_data(id)?;
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [*w as usize, *h as usize],
                pixels,
            );
            let handle = ctx.load_texture(
                format!("monada_ui_{id}"),
                image,
                egui::TextureOptions::NEAREST,
            );
            self.ui_tex_cache[id] = Some(handle);
        }
        self.ui_tex_cache[id].clone()
    }

    /// Handle a left-click: pick a mover (local), queue a spawn command at
    /// the picked point (networked), or forward a generic pointer event to
    /// the map's script (map). The host interprets none of it for a map —
    /// the script's `pointer` handler runs the gesture and may
    /// `submit_command`, which the host then routes.
    fn on_click(&mut self) {
        let cam = self.scene.camera();
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let Some(ray) = renderer.view_ray(&cam, self.cursor.0, self.cursor.1) else {
            return;
        };
        match (&mut self.sim, &mut self.scene) {
            (Sim::Local { .. }, SceneKind::Circle(scene)) => {
                match scene.pick_ground(ray.origin, ray.dir) {
                    Some(i) => eprintln!("picked mover #{i}"),
                    None => eprintln!("picked: (none)"),
                }
            }
            (Sim::Net(net), SceneKind::Circle(scene)) => {
                if let Some((x, y)) = scene.ground_sim_xy(ray.origin, ray.dir) {
                    let arg = FixedVec3::new(Fixed::from_f64(x), Fixed::from_f64(y), Fixed::ZERO);
                    net.pending.push(Command::at(SPAWN_VERB, arg));
                    eprintln!("spawn @ ({x:.2}, {y:.2})");
                }
            }
            (Sim::Map(map), SceneKind::Map(_)) => {
                map.pointer(/* left button */ 0, ray.origin, ray.dir);
            }
            (Sim::NetMap(nm), SceneKind::Map(_)) => {
                nm.pointer(/* left button */ 0, ray.origin, ray.dir);
            }
            // sim / scene flavours are constructed together, so the
            // mixed pairs never occur.
            _ => {}
        }
    }

    /// Refresh the render scene from the current sim: the circle/net scene
    /// interpolates mover positions; the map scenes (local / networked /
    /// replay) rebuild sprites from the live world + the script's model
    /// bindings.
    fn update_scene(&mut self, alpha: f64) {
        match (&self.sim, &mut self.scene) {
            (Sim::Map(map), SceneKind::Map(render)) => {
                let world = map.world.lock().expect("world mutex");
                render.lock().expect("render mutex").build_instances(&world);
            }
            (Sim::NetMap(nm), SceneKind::Map(render)) => {
                let world = nm.session.driver().world().clone();
                let guard = world.lock().expect("world mutex");
                render.lock().expect("render mutex").build_instances(&guard);
            }
            (Sim::Replay(r), SceneKind::Map(render)) => {
                let world = r.driver.world().clone();
                let guard = world.lock().expect("world mutex");
                render.lock().expect("render mutex").build_instances(&guard);
            }
            (_, SceneKind::Circle(scene)) => {
                scene.update(&self.prev_pos, &self.curr_pos, alpha);
            }
            _ => {}
        }
    }

    /// Advance the camera from currently-held keys. A real-time map owns its
    /// camera (the script aims it each tick), so host orbit is suppressed.
    fn drive_camera(&mut self, dt: f64) {
        if self.realtime() {
            return;
        }
        let dyaw = (f64::from(self.keys.yaw_right) - f64::from(self.keys.yaw_left)) * YAW_RATE * dt;
        let dpitch =
            (f64::from(self.keys.pitch_down) - f64::from(self.keys.pitch_up)) * PITCH_RATE * dt;
        let ddist = (f64::from(self.keys.zoom_out) - f64::from(self.keys.zoom_in)) * ZOOM_RATE * dt;
        if dyaw != 0.0 || dpitch != 0.0 || ddist != 0.0 {
            self.scene.orbit(dyaw, dpitch, ddist);
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
        // the camera) so every live unit has a sprite instance. Net mode
        // always runs the circle scene.
        if self.live_count != self.curr_pos.len() {
            if let SceneKind::Circle(scene) = &mut self.scene {
                let cam = scene.camera;
                let mut rebuilt = CircleScene::new(self.curr_pos.len());
                rebuilt.camera = cam;
                *scene = rebuilt;
            }
            self.live_count = self.curr_pos.len();
        }
        self.prev_pos.clone_from(&self.curr_pos);
    }

    /// Write a networked match's replay to disk (once), on exit — both the
    /// `command_demo` net mode and a networked map.
    fn save_replay(&mut self) {
        let (replay, path, saved) = match &mut self.sim {
            Sim::Net(net) => (net.session.replay(), &net.replay_path, &mut net.saved),
            Sim::NetMap(nm) => (nm.session.replay(), &nm.replay_path, &mut nm.saved),
            _ => return,
        };
        if *saved {
            return;
        }
        *saved = true;
        let ticks = replay.frames.len();
        match replay.encode() {
            Ok(bytes) => match std::fs::write(path, bytes) {
                Ok(()) => eprintln!("monada-host: wrote {path} ({ticks} input frames)"),
                Err(e) => eprintln!("monada-host: failed to write replay: {e}"),
            },
            Err(e) => eprintln!("monada-host: replay encode failed: {e}"),
        }
    }

    /// Tear the renderer + window down in the order a clean GPU shutdown needs:
    /// drain in-flight GPU work (`wait_idle`), then drop the renderer (releasing
    /// the wgpu device/queue/surface), then the egui state, then the window.
    /// Dropping the surface/device before the window — queue idle, no acquired
    /// frame — is what stops an exit leaving the driver/compositor showing stale
    /// buffers (roxlap's "leftover triangles / flicker" symptom). Idempotent.
    fn teardown(&mut self) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.wait_idle();
        }
        self.renderer = None;
        self.egui_state = None;
        self.window = None;
    }

    /// Drain the map's queued audio for this frame and feed the mixer: the
    /// de-duplicated one-shot SFX, then any music change. Drains under the
    /// render lock, releasing it before touching `self.audio` (disjoint state).
    fn play_pending_audio(&mut self, now: Instant) {
        let (sounds, loops, music) = match &self.scene {
            SceneKind::Map(render) => render.lock().expect("render mutex").drain_audio(),
            SceneKind::Circle(_) => return,
        };
        for (path, gain) in sounds {
            self.audio.play(&path, gain, now);
        }
        self.audio.sync_loops(&loops, now);
        match music {
            Some(map_render::MusicCmd::Play(path)) => self.audio.play_music(&path),
            Some(map_render::MusicCmd::Stop) => self.audio.stop_music(),
            None => {}
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

        // Refresh the mouse-aim direction, then advance. A real-time map paces
        // its `tick` on the wall clock with the per-tick input snapshot; a
        // command-driven map snaps to the current world.
        self.update_aim();
        let input = self.input;
        // A HUD button click latches into `input.ui_bits`; clear it only once
        // a tick has actually consumed the input snapshot, so a click on a
        // frame that ran zero ticks still fires on the next stepped frame.
        let mut consumed_input = false;
        let alpha = match &mut self.sim {
            Sim::Local { .. } => self.advance_local(dt),
            Sim::Net(_) => {
                self.advance_net();
                1.0
            }
            Sim::Map(map) => {
                consumed_input = map.advance(dt, input);
                1.0
            }
            Sim::NetMap(nm) => {
                consumed_input = nm.advance(dt, input);
                1.0
            }
            Sim::Replay(r) => {
                r.advance(dt);
                1.0
            }
        };
        if consumed_input {
            self.input.ui_bits = 0;
        }

        // Play whatever sound the map queued in this frame's tick(s).
        self.play_pending_audio(now);

        self.drive_camera(dt);
        self.update_scene(alpha);

        if !self.debug_done && std::env::var_os("MONADA_DEBUG").is_some() {
            self.debug_done = true;
            let cam = self.scene.camera();
            eprintln!("[debug] camera pos={:?} forward={:?}", cam.pos, cam.forward);
            if let SceneKind::Circle(scene) = &self.scene {
                let (center, sample) = scene.debug_positions();
                eprintln!("[debug] board center={center:?}");
                for (i, p) in sample.iter().enumerate() {
                    eprintln!("[debug] cube[{i}] world={p:?}");
                }
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
            // Sprites are flat-lit on both backends in roxlap 0.19; just the
            // on/off opt-in for the circle scene's movers.
            draw_sprites: true,
            // Per-face grid shading. Flat for the circle scene; the map
            // paths override this from their declared sun.
            side_shades: [0; 6],
            // Dynamic lighting (GPU-only sun + point lights) — unused here.
            lights: None,
        };

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        // roxlap 0.7: render() composites without presenting; the frame is
        // finished by exactly one of paint_egui (HUD) or present. The map
        // scene lives behind a Mutex, so set + render under one lock.
        let debug = self.debug_hud;
        match &mut self.scene {
            SceneKind::Circle(scene) => {
                renderer.set_sprites(scene.sprites());
                renderer.render(scene.scene_mut(), &camera, &frame);
            }
            SceneKind::Map(render) => {
                // The map lights itself (grid side-shades + sky) and drives
                // its animated actors; `dt` advances their animation clocks.
                // F1 (`debug`) overlays the collision footprints.
                render.lock().expect("render mutex").render_into(
                    renderer, &camera, &settings, SKY_COLOR, dt, debug,
                );
            }
        }
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

        // GPU (wgpu) backend by default; roxlap-render falls back to CPU
        // automatically if init fails. Set `ROXLAP_GPU=0` to force the CPU
        // backend (e.g. a headless box or a flaky driver).
        let want_gpu = std::env::var_os("ROXLAP_GPU").map_or(true, |v| v != "0" && !v.is_empty());
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
                state,
                button: MouseButton::Left,
                ..
            } if !consumed => {
                let down = state == ElementState::Pressed;
                // Held-attack for a real-time map; a chess-style pick for a
                // command-driven one (on press only).
                self.input.attack = down;
                if down && !self.realtime() {
                    self.on_click();
                }
            }
            WindowEvent::RedrawRequested => self.redraw(),
            _ => {}
        }
    }

    /// Graceful shutdown: winit calls this once the loop is told to exit
    /// (`event_loop.exit()` from a close / Esc). Drain and drop the GPU
    /// cleanly here so an exit never yanks the swapchain mid-frame.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.teardown();
    }

    /// The platform asked us to release the window/surface (Android-style; rare
    /// on desktop). Drop the GPU resources cleanly too — `resumed` rebuilds
    /// them. Same clean-teardown path as `exiting`.
    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        self.teardown();
    }
}

/// HUD fields specific to a networked match.
struct NetHud {
    player: u32,
    units: usize,
    halted: bool,
    connected: bool,
}

/// Lockstep status for a networked map (the connection line).
struct MapNet {
    player: u32,
    halted: bool,
    connected: bool,
}

/// Per-mode HUD state passed to [`build_hud`].
enum HudState {
    Local {
        selected: Option<usize>,
    },
    Net(NetHud),
    /// A scripted map: the status line the map set via `status(...)` (the
    /// host attaches no meaning to it), plus the lockstep line when
    /// networked.
    Map {
        status: String,
        net: Option<MapNet>,
    },
}

/// A HUD texture's size in egui points (its pixel dimensions at 1:1). The map
/// lays the HUD out at the asset's native pixel size; egui scales to physical
/// pixels by the display's `pixels_per_point`.
fn tex_points(handle: &egui::TextureHandle) -> egui::Vec2 {
    let [w, h] = handle.size();
    egui::vec2(w as f32, h as f32)
}

/// Build the HUD widget tree (DESIGN.md §3.2's egui HUD).
fn build_hud(ctx: &egui::Context, tick: u64, fps: f32, hud: &HudState) {
    egui::Window::new("monada")
        .title_bar(false)
        .resizable(false)
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(8.0, 8.0))
        .show(ctx, |ui| {
            ui.label(format!("tick {tick}"));
            ui.label(format!("fps  {fps:.0}"));
            match hud {
                HudState::Local { selected } => {
                    match selected {
                        Some(i) => ui.label(format!("selected mover #{i}")),
                        None => ui.label("selected mover —"),
                    };
                    ui.separator();
                    ui.label("arrows orbit · W/S zoom");
                    ui.label("click a cube to pick · Esc quit");
                }
                HudState::Net(net) => {
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
                // The host shows the map's status verbatim — it has no idea
                // what game the string describes.
                HudState::Map { status, net } => {
                    ui.label(status);
                    if let Some(net) = net {
                        if net.halted {
                            ui.colored_label(egui::Color32::RED, "DESYNC — halted");
                        } else if net.connected {
                            ui.label(format!("player {} · lockstep in sync", net.player));
                        } else {
                            ui.colored_label(egui::Color32::RED, "peer lost — no reconnect");
                        }
                    }
                    ui.separator();
                    ui.label("arrows orbit · W/S zoom · Esc quit");
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
            // F1 toggles the debug overlay (tick / FPS / status / lockstep).
            KeyCode::F1 if down => self.debug_hud = !self.debug_hud,
            KeyCode::ArrowLeft => self.keys.yaw_left = down,
            KeyCode::ArrowRight => self.keys.yaw_right = down,
            KeyCode::ArrowUp => self.keys.pitch_up = down,
            KeyCode::ArrowDown => self.keys.pitch_down = down,
            // W/S zoom the camera for the orbit scenes (circle / chess); for a
            // real-time map they drive movement and the script owns the camera.
            KeyCode::KeyW if self.realtime() => self.input.fwd = down,
            KeyCode::KeyS if self.realtime() => self.input.back = down,
            KeyCode::KeyW => self.keys.zoom_in = down,
            KeyCode::KeyS => self.keys.zoom_out = down,
            KeyCode::KeyA => self.input.left = down,
            KeyCode::KeyD => self.input.right = down,
            // Space is the real-time dodge; for a replay it toggles pause
            // (the two modes never coexist).
            KeyCode::Space => {
                self.input.dodge = down;
                if down {
                    self.replay_control(|r| r.paused = !r.paused);
                }
            }
            // Replay transport: `[` slower, `]` faster.
            KeyCode::BracketLeft if down => self.replay_control(|r| r.scale_speed(0.5)),
            KeyCode::BracketRight if down => self.replay_control(|r| r.scale_speed(2.0)),
            _ => {}
        }
    }

    /// Whether the running sim is a real-time (fixed-rate) map — the script
    /// drives movement from the input snapshot and owns the camera.
    fn realtime(&self) -> bool {
        match &self.sim {
            Sim::Map(map) => map.tick_dt.is_some(),
            Sim::NetMap(nm) => nm.tick_dt.is_some(),
            _ => false,
        }
    }

    /// Recompute `input.aim_yaw`: the sim-space angle from the local player
    /// (the camera focus) to the cursor's ground point. Only meaningful for a
    /// real-time map (which owns its camera and runs the input loop).
    fn update_aim(&mut self) {
        if !self.realtime() {
            return;
        }
        let cam = self.scene.camera();
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let Some(ray) = renderer.view_ray(&cam, self.cursor.0, self.cursor.1) else {
            return;
        };
        if let SceneKind::Map(render) = &self.scene {
            let r = render.lock().expect("render mutex");
            if let Some((mx, my)) = r.ground_sim(ray.origin, ray.dir) {
                let (px, py) = r.camera_center_sim();
                let (dx, dy) = (mx - px, my - py);
                if dx.mul_add(dx, dy * dy) > 1e-6 {
                    self.input.aim_yaw = dy.atan2(dx);
                }
            }
        }
    }

    /// Apply a transport control to the replay sim, if one is running.
    fn replay_control(&mut self, f: impl FnOnce(&mut ReplaySim)) {
        if let Sim::Replay(r) = &mut self.sim {
            f(r);
        }
    }
}
