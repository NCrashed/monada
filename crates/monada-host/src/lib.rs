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
use std::time::{Duration, Instant};

use glam::DVec3;
use monada_fixed::{Fixed, FixedVec3};
use monada_format::{Map, SimHz, Terrain};
use monada_net::{LockstepSession, MatchInfo, QuicTransport, Replay, SessionConfig, SimDriver};
use monada_render::CircleScene;
use monada_script::{
    shared_physics, shared_world, LocalBackend, LocalLayer, LocalRules, MapRules, NativeBackend,
    NativeLocalBackend, PhysicsSim, RhaiBackend, RhaiDriver, ScriptBackend, SharedBridge,
    SharedPhysics, SharedWorld, COMMAND_DEMO_SCRIPT, WALK_CIRCLE_SCRIPT,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

mod audio;
pub mod autotile;
mod bindings;
pub mod cli;
mod map_render;
use audio::Audio;
use bindings::{Action, ActionRef, Bindings, Context, PhysInput};
use map_render::MapRender;
use roxlap_core::opticast::OpticastSettings;
use roxlap_core::Camera;
// egui itself comes through roxlap-render's re-export so the version
// matches the one `paint_egui` rasterises with.
use roxlap_formats::Rgb;
use roxlap_render::{
    egui, BackendPreference, FrameParams, RenderOptions, RenderResolution, SceneRenderer,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

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
const SKY_COLOR: Rgb = Rgb(0x0099_B3D9);

/// Camera control rates (per second of held input).
const YAW_RATE: f64 = 1.4;
const PITCH_RATE: f64 = 1.0;
const ZOOM_RATE: f64 = 240.0;
/// World voxels of camera-distance change per mouse-wheel notch (zoom). Unlike
/// the key orbit, wheel zoom applies even to real-time maps whose script owns
/// the camera — they set the distance once, so the player can still zoom.
const WHEEL_ZOOM_STEP: f64 = 40.0;

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
    /// Compiled rules, for a map whose logic is linked rather than
    /// scripted (decision L1 of docs/plans/desert-game.md). The archive
    /// still carries the manifest, the assets and the input bindings —
    /// only `entry` goes unread, because there is nothing to compile.
    pub native: Option<NativeMap>,
}

/// A native map's two halves, handed over by its launcher: the
/// simulation's rules and this client's gesture layer.
pub struct NativeMap {
    pub rules: Box<dyn MapRules>,
    pub local: Box<dyn LocalRules>,
}

impl MapRun {
    /// A scripted map — the shape every demo before the desert takes.
    #[must_use]
    pub fn scripted(map: Map) -> MapRun {
        MapRun { map, native: None }
    }

    /// A map whose rules are compiled in.
    #[must_use]
    pub fn native(map: Map, rules: Box<dyn MapRules>, local: Box<dyn LocalRules>) -> MapRun {
        MapRun {
            map,
            native: Some(NativeMap { rules, local }),
        }
    }
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

/// Frame-time accounting for `MONADA_PROFILE`.
///
/// **A log that only prints what crossed 20 ms cannot tell 8 ms from
/// 19.** The slow frames go out one by one because a stutter is a single
/// frame and wants its own breakdown; this is the rest of the story, a
/// line a second saying what the frames actually cost. Tuning anything
/// by the slow-frame log alone means tuning by how often it goes quiet.
struct FrameLog {
    /// When the second being summarised started.
    since: Instant,
    frames: u32,
    total: Duration,
    worst: Duration,
}

impl FrameLog {
    fn new(now: Instant) -> FrameLog {
        FrameLog {
            since: now,
            frames: 0,
            total: Duration::ZERO,
            worst: Duration::ZERO,
        }
    }

    /// Fold one frame in, and answer the summary once a second is up.
    fn record(&mut self, now: Instant, frame: Duration) -> Option<String> {
        self.frames += 1;
        self.total += frame;
        self.worst = self.worst.max(frame);
        let span = now.saturating_duration_since(self.since);
        if span < Duration::from_secs(1) {
            return None;
        }
        let ms = |d: Duration| d.as_secs_f64() * 1e3;
        let line = format!(
            "[profile] {:5.1} fps — frame {:5.1} ms avg, {:5.1} ms worst, over {} frames",
            f64::from(self.frames) / span.as_secs_f64(),
            ms(self.total) / f64::from(self.frames),
            ms(self.worst),
            self.frames,
        );
        *self = FrameLog::new(now);
        Some(line)
    }
}

/// How much smaller than the window the scene may be marched, as the
/// divisor `ui.render_scale` cycles through.
///
/// **Whole divisors, because the upscale is nearest.** A raycaster costs
/// what it is asked for in pixels, and at 1080p a map can want a third
/// of them; the frame is then blown back up to the window with no
/// filtering. A fraction that does not divide the window evenly spreads
/// a row of doubled pixels through the picture and shimmers as the
/// camera moves, which is the one thing pixel art must not do -- so the
/// choices are 1:1, 1:2 and 1:3 and nothing between.
const RENDER_DIVS: [u32; 3] = [1, 2, 3];

/// …and the least the software marcher runs at whatever is asked. Its
/// cost scales with the pixel count and it cannot hold a window's native
/// resolution on the volume maps.
const CPU_RENDER_DIV: u32 = 2;

/// Which divisor the session opens at: the player's if they said
/// (`MONADA_RENDER_SCALE=2` being "march a quarter of the pixels"),
/// else the map's own pixel grid (`Manifest::render_scale`), else the
/// window's resolution.
///
/// A number neither offers is dropped with a word rather than refusing
/// to start — a stale config or a map written against a longer ladder
/// is not a reason to leave somebody without a game.
fn starting_render_div(map_div: Option<u32>) -> u32 {
    let offered = |d: u32, what: &str| {
        if RENDER_DIVS.contains(&d) {
            return Some(d);
        }
        eprintln!("monada-host: {what} asks to render at 1/{d}, which is not one of {RENDER_DIVS:?}");
        None
    };
    let env = std::env::var("MONADA_RENDER_SCALE")
        .ok()
        .map(|v| v.trim().to_owned())
        .map(|v| {
            v.parse::<u32>()
                .ok()
                .and_then(|d| offered(d, "MONADA_RENDER_SCALE"))
        });
    match env {
        Some(Some(d)) => d,
        // Set but unusable: the player still MEANT to override the map,
        // so fall to the window rather than to the map's own grid.
        Some(None) => 1,
        None => map_div.and_then(|d| offered(d, "the map")).unwrap_or(1),
    }
}

/// The divisor after this one, wrapping — what the key cycles through.
fn next_render_div(current: u32) -> u32 {
    let at = RENDER_DIVS.iter().position(|&d| d == current);
    RENDER_DIVS[at.map_or(0, |i| (i + 1) % RENDER_DIVS.len())]
}

#[cfg(test)]
mod render_scale_tests {
    use super::{next_render_div, RENDER_DIVS};

    /// The cycle comes home, and it only ever lands on a whole divisor —
    /// the upscale is nearest, so anything else spreads a row of doubled
    /// pixels through the picture and shimmers as the camera moves.
    #[test]
    fn the_render_scale_cycles_through_whole_divisors() {
        let mut seen = vec![RENDER_DIVS[0]];
        let mut at = RENDER_DIVS[0];
        for _ in 0..RENDER_DIVS.len() {
            at = next_render_div(at);
            assert!(RENDER_DIVS.contains(&at), "{at} is not a listed divisor");
            seen.push(at);
        }
        assert_eq!(seen.first(), seen.last(), "the cycle comes home");
        assert_eq!(seen.len(), RENDER_DIVS.len() + 1, "and visits each once");
        // A value from nowhere (a stale config, a future divisor) starts
        // the cycle rather than falling off it.
        assert_eq!(next_render_div(7), RENDER_DIVS[0]);
    }
}

/// Whether a key is the Return one, wherever on the board it sits. The
/// numpad's is a different code and the same key to anyone pressing it.
fn is_enter(code: KeyCode) -> bool {
    matches!(code, KeyCode::Enter | KeyCode::NumpadEnter)
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

/// The embedded physics sim a `terrain = "volume"` map runs on, at the
/// manifest's fixed tick rate; `None` for column maps (every map before
/// the digger demo). Manifest validation already refused a volume map
/// without a fixed `sim_hz`.
fn volume_physics(manifest: &monada_format::Manifest) -> Option<SharedPhysics> {
    if manifest.terrain != Terrain::Volume {
        return None;
    }
    let SimHz::Fixed(hz) = manifest.sim_hz else {
        unreachable!("Manifest::validate: volume terrain requires a fixed sim_hz");
    };
    Some(shared_physics(hz))
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
    /// The map's simulation, whichever runtime wrote it: a Rhai script
    /// or compiled rules. The host drives the same four triggers either
    /// way (docs/plans/desert-game.md D-1).
    backend: Box<dyn ScriptBackend>,
    /// The map's local, unsynchronized layer: pointer gestures, action
    /// edges, per-tick input assembly (plan step 3) — script or compiled,
    /// behind the same seam as the backend above.
    local_layer: Box<dyn LocalLayer>,
    render: Arc<Mutex<MapRender>>,
    /// `Some(1/hz)` for a real-time map (drive its `tick` on the wall clock),
    /// `None` for a command-driven one (chess: advance only on clicks).
    tick_dt: Option<f64>,
    accumulator: f64,
    /// Player id attributed to this peer's input commands (hotseat = 0).
    local: PlayerId,
    /// The embedded physics sim of a `terrain = "volume"` map, stepped
    /// after each script `tick` — the same per-tick order as
    /// [`RhaiDriver::step`] (docs/plans/digger-demo.md §1b).
    phys: Option<SharedPhysics>,
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
        let script_input = self.local_layer.has_local_tick();
        let input_cmd = input.to_command();
        let step_fixed = Fixed::from_f64(step);
        let mut budget = MAX_CATCHUP_TICKS_PER_FRAME;
        let mut stepped = false;
        while self.accumulator >= step && budget > 0 {
            if script_input {
                // The map assembles its own per-tick input in `local_tick`
                // (submitted via the bridge, drained here) — the host's
                // legacy snapshot stays out of the stream. Note a deliberate
                // catch-up difference vs that legacy path: `ui_clicks()` is
                // take-and-clear, so a HUD click lands in exactly ONE of the
                // ticks run this frame; the old snapshot replayed the click
                // bits into every catch-up tick.
                self.local_layer
                    .on_local_tick(step_fixed)
                    .expect("map local_tick");
                let commands = self.render.lock().expect("render mutex").drain_commands();
                for command in commands {
                    self.backend
                        .on_command(self.local, &command)
                        .expect("map input command");
                }
            } else {
                self.backend
                    .on_command(self.local, &input_cmd)
                    .expect("map input command");
            }
            self.backend.on_tick().expect("map tick");
            if let Some(phys) = &self.phys {
                let mut sim = phys.lock().expect("physics mutex");
                let PhysicsSim { world, terrain, .. } = &mut *sim;
                world.step(terrain);
            }
            // The frames of body-bound grids, after the step that moved them
            // (`grid_body`, docs/plans/ship-physics.md D2) — the same order
            // `RhaiDriver::step` keeps, so a hotseat map and a networked one
            // see the identical hull pose on the identical tick.
            self.backend.sync_grid_bodies();
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

    /// Forward a pointer click to the map's local-layer `pointer` handler,
    /// then route whatever commands the gesture queued. Hotseat: commands
    /// apply immediately. The player id is a placeholder — the script
    /// enforces turn from game state, not the id; the networked
    /// player↔command mapping lands in slice 4.
    fn pointer(&mut self, button: i64, origin: DVec3, dir: DVec3) {
        let (point, entity) = {
            let r = self.render.lock().expect("render mutex");
            let w = self.world.lock().expect("world mutex");
            r.pick(&w, origin, dir)
        };
        self.local_layer
            .on_pointer(button, point, entity)
            .expect("map pointer handler");
        self.route_local_commands();
    }

    /// Forward one action edge to the local layer, then route whatever
    /// commands it submitted.
    fn action(&mut self, id: &str, down: bool) {
        self.local_layer
            .on_action(id, down)
            .expect("map action handler");
        self.route_local_commands();
    }

    /// Apply commands the local layer queued through the bridge (hotseat:
    /// immediately, as the click path always did).
    fn route_local_commands(&mut self) {
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
    /// The map's local, unsynchronized script layer (plan step 3);
    /// commands it submits are routed through the lockstep session.
    local_layer: Box<LocalBackend>,
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
        self.local_layer
            .on_pointer(button, point, entity)
            .expect("map pointer handler");
        let commands = self.render.lock().expect("render mutex").drain_commands();
        self.pending.extend(commands);
    }

    /// Forward one action edge to the local layer; submitted commands are
    /// routed through the session like clicks.
    fn action(&mut self, id: &str, down: bool) {
        self.local_layer
            .on_action(id, down)
            .expect("map action handler");
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
        let script_input = self.local_layer.has_local_tick();
        let input_cmd = input.to_command();
        let step_fixed = Fixed::from_f64(step);
        let mut budget = MAX_CATCHUP_TICKS_PER_FRAME;
        let mut consumed_input = false;
        while !self.halted && self.accumulator >= step && budget > 0 {
            let mut cmds = std::mem::take(&mut self.pending);
            // One input per scheduled tick: don't re-submit one the session
            // already buffered while stalled.
            let submit_input = !self.input_pending;
            if submit_input {
                if script_input {
                    // The map assembles this tick's input in `local_tick`;
                    // whatever it submitted rides this tick's bundle.
                    self.local_layer
                        .on_local_tick(step_fixed)
                        .expect("map local_tick");
                    cmds.extend(self.render.lock().expect("render mutex").drain_commands());
                } else {
                    cmds.push(input_cmd);
                }
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

// Four independent on/off latches (debug HUD, one-shot dump, rebind modal,
// profiler) — not a state machine in disguise.
#[allow(clippy::struct_excessive_bools)]
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
    /// Physical-input → action table (engine defaults + the map's
    /// `[[action]]` declarations + the user's `bindings.toml`), resolved
    /// per event in `window_event`.
    bindings: Bindings,
    /// Whether the key-bindings panel is open (toggled by `ui.bindings`).
    rebind_open: bool,
    /// While `Some`, the panel is waiting for a key/mouse press to bind to
    /// this target; the next input is captured instead of dispatched.
    capturing: Option<ActionRef>,
    /// The label of the slot a just-completed rebind took its key from, if
    /// any — shown once in the panel so the displacement isn't silent.
    rebind_notice: Option<String>,
    /// Which modifiers are held, for the one chord the host answers to
    /// (Alt+Enter). The binding table holds single inputs and has nowhere
    /// to write a modifier, so this is tracked beside it rather than in
    /// it — see [`Action::Fullscreen`].
    modifiers: ModifiersState,
    /// How much smaller than the window the scene is marched
    /// ([`RENDER_DIVS`]), cycled by `ui.render_scale` and seeded from
    /// `MONADA_RENDER_SCALE`.
    render_div: u32,
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
    /// egui texture handles for animated HUD images (`ui_gif`): `[gif][frame]`,
    /// each frame uploaded lazily the first time it is shown.
    ui_gif_cache: Vec<Vec<Option<egui::TextureHandle>>>,
    /// Epoch for wall-clock HUD animation timing (`ui_anim` frame selection).
    epoch: Instant,
    /// The audio mixer (feature `audio`; a no-op shim otherwise). Fed each
    /// frame by `MapRender::drain_audio`; owns the rodio output (`!Send`).
    audio: Audio,
    /// Whether the debug overlay (tick / FPS / status / lockstep line) is
    /// shown. Hidden by default so only the map's own HUD is visible; F1
    /// toggles it.
    debug_hud: bool,
    /// One-shot coordinate dump (set `MONADA_DEBUG=1`).
    debug_done: bool,
    /// Slow-frame breakdown to stderr (set `MONADA_PROFILE=1`): any frame
    /// over 20 ms logs how the time split across sim / scene-sync / render /
    /// present, to localize stutter without a profiler attached.
    profile: Option<FrameLog>,
}

impl App {
    fn new(config: RunConfig) -> App {
        // The binding table wants the map's name + `[[action]]`
        // declarations before `config` is consumed by sim construction.
        let (map_name, map_actions) = match &config {
            RunConfig::Map { run, .. } | RunConfig::Replay { run, .. } => (
                run.map.manifest.name.clone(),
                run.map.manifest.actions.clone(),
            ),
            _ => (String::new(), Vec::new()),
        };
        // The map's own pixel grid, unless the player said otherwise at
        // launch. Both are divisors of the window, and the key cycles
        // from wherever this lands.
        let map_div = match &config {
            RunConfig::Map { run, .. } | RunConfig::Replay { run, .. } => {
                run.map.manifest.render_scale
            }
            _ => None,
        };
        let bindings = Bindings::load(&map_name, &map_actions);
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
            bindings,
            rebind_open: false,
            capturing: None,
            rebind_notice: None,
            modifiers: ModifiersState::empty(),
            render_div: starting_render_div(map_div),
            input: Input::default(),
            cursor: (0.0, 0.0),
            fps: 0.0,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            ui_tex_cache: Vec::new(),
            ui_gif_cache: Vec::new(),
            epoch: Instant::now(),
            debug_hud: false,
            debug_done: false,
            profile: std::env::var_os("MONADA_PROFILE")
                .is_some_and(|v| !v.is_empty() && v != "0")
                .then(|| FrameLog::new(Instant::now())),
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
        // Hotseat: one window drives every side, so there is no single
        // local player (-1) — the map enforces turns itself.
        let MapRun { map, native } = run;
        let manifest = map.manifest.clone();
        let scripts = native.is_none().then(|| {
            (
                map.entry_script().expect("map declares an entry script").to_string(),
                map.local_script().expect("map declares a local script").to_string(),
            )
        });
        let mut map_render = MapRender::new(map.assets, None, &manifest.actions);
        if manifest.terrain == Terrain::Volume {
            map_render.set_volume_terrain();
        }
        let render = Arc::new(Mutex::new(map_render));
        let bridge: SharedBridge = render.clone();
        let phys = volume_physics(&manifest);
        let hz = match manifest.sim_hz {
            SimHz::Fixed(hz) => Some(hz),
            SimHz::OnCommand => None,
        };

        let (backend, local_layer): (Box<dyn ScriptBackend>, Box<dyn LocalLayer>) =
            if let Some(native) = native {
                Self::boot_native(&world, &bridge, phys.as_ref(), hz, native)
            } else {
                let (script, local) = scripts.expect("a scripted map has scripts");
                Self::boot_scripted(&world, &bridge, phys.as_ref(), hz, &script, &local)
            };
        // Smoothing on, now that `init` has posed whatever it poses: from the
        // first tick, a grid's pose is a target the render eases onto rather
        // than a step it takes whole (docs/plans/ship-physics.md §4).
        if let Some(hz) = hz {
            render.lock().expect("render mutex").set_tick_hz(hz);
        }
        Sim::Map(Box::new(MapSim {
            world,
            backend,
            local_layer,
            render,
            tick_dt: tick_dt(manifest.sim_hz),
            accumulator: 0.0,
            // Hotseat: one local player drives the real-time input.
            local: PlayerId(0),
            phys,
        }))
    }

    /// Boot a Rhai map: compile the entry script, run `init`, then bring
    /// up the local layer over the world `init` populated.
    ///
    /// The order is the contract, not a preference. The bridge comes
    /// before `init` (which calls `model_box` / `voxel_fill`), physics
    /// after the bridge (its volume `voxel_*` shadow the column ones) and
    /// still before `init`, and the local layer last so `local_init` sees
    /// a populated world and the frames the sim's `grid_*` just wrote.
    fn boot_scripted(
        world: &SharedWorld,
        bridge: &SharedBridge,
        phys: Option<&SharedPhysics>,
        hz: Option<u32>,
        script: &str,
        local_script: &str,
    ) -> (Box<dyn ScriptBackend>, Box<dyn LocalLayer>) {
        let mut backend = RhaiBackend::new(world.clone());
        backend.set_bridge(bridge);
        if let Some(phys) = phys {
            backend.set_physics(phys);
        }
        if let Some(hz) = hz {
            backend.set_tick_hz(hz);
        }
        backend.load(script).expect("compile map script");
        backend.on_init().expect("map init");
        backend.drain_ui_events();

        let mut local_layer = LocalBackend::new(world, bridge);
        local_layer.set_grids(backend.grids());
        // …and the same ground: collision lives in the runtime store, so
        // the local layer has to be handed the simulation's own copy.
        local_layer.set_terrain(bridge, backend.terrain());
        local_layer.load(local_script).expect("compile local script");
        local_layer.on_local_init().expect("map local_init");
        (Box::new(backend), Box::new(local_layer))
    }

    /// Boot a native map: the same order, with linked rules in place of a
    /// compiled script (decision L1). Nothing is read from `entry` — the
    /// archive is here for its assets, manifest and bindings.
    fn boot_native(
        world: &SharedWorld,
        bridge: &SharedBridge,
        phys: Option<&SharedPhysics>,
        hz: Option<u32>,
        native: NativeMap,
    ) -> (Box<dyn ScriptBackend>, Box<dyn LocalLayer>) {
        let mut backend = NativeBackend::new(world.clone(), native.rules);
        backend.set_bridge(bridge);
        if let Some(phys) = phys {
            backend.set_volume(phys);
        }
        if let Some(hz) = hz {
            backend.set_tick_hz(hz);
        }
        backend.on_init().expect("map init");

        // The local layer shares the simulation's stores rather than
        // making its own — a cursor that reads private ground reads empty
        // ground.
        let mut local_layer = NativeLocalBackend::new(
            world,
            bridge,
            backend.host().terrain_store(),
            native.local,
        );
        if let Some(phys) = phys {
            local_layer.set_volume(phys);
        }
        local_layer.on_local_init().expect("map local_init");
        (Box::new(backend), Box::new(local_layer))
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
        let local_script = run
            .map
            .local_script()
            .expect("map declares a local script")
            .to_string();
        // This peer plays the side matching its player id; the script gates
        // off-turn input on `local_player()`.
        let mut map_render = MapRender::new(
            run.map.assets,
            Some(i64::from(local.0)),
            &run.map.manifest.actions,
        );
        if run.map.manifest.terrain == Terrain::Volume {
            map_render.set_volume_terrain();
        }
        let render = Arc::new(Mutex::new(map_render));
        let bridge: SharedBridge = render.clone();
        let mut driver = match volume_physics(&run.map.manifest) {
            Some(phys) => RhaiDriver::with_physics(shared_world(SEED), &script, &bridge, &phys),
            None => RhaiDriver::with_bridge(shared_world(SEED), &script, &bridge),
        }
        .expect("compile map script");
        if let SimHz::Fixed(hz) = run.map.manifest.sim_hz {
            driver.set_tick_hz(hz);
        }
        // The local layer reads the same shared world — and the same grid
        // frames — the driver mutates.
        let mut local_layer = LocalBackend::new(driver.world(), &bridge);
        local_layer.set_grids(driver.grids());
        local_layer.set_terrain(&bridge, driver.terrain());
        local_layer
            .load(&local_script)
            .expect("compile local script");
        local_layer.on_local_init().expect("map local_init");
        // Grid-pose smoothing, after `init` — see `new_map`.
        if let SimHz::Fixed(hz) = run.map.manifest.sim_hz {
            render.lock().expect("render mutex").set_tick_hz(hz);
        }
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
            local_layer: Box::new(local_layer),
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
        // No local layer for a replay: the recorded command stream *is*
        // the input; live clicks/actions are ignored.
        let mut map_render = MapRender::new(run.map.assets, None, &run.map.manifest.actions);
        if run.map.manifest.terrain == Terrain::Volume {
            map_render.set_volume_terrain();
        }
        let render = Arc::new(Mutex::new(map_render));
        let bridge: SharedBridge = render.clone();
        let mut driver = match volume_physics(&run.map.manifest) {
            Some(phys) => {
                RhaiDriver::with_physics(shared_world(replay.seed), &script, &bridge, &phys)
            }
            None => RhaiDriver::with_bridge(shared_world(replay.seed), &script, &bridge),
        }
        .expect("compile map script");
        if let SimHz::Fixed(hz) = run.map.manifest.sim_hz {
            driver.set_tick_hz(hz);
            // Grid-pose smoothing, after `init` — see `new_map`. A replay is
            // paced from the same clock, so it smooths identically.
            render.lock().expect("render mutex").set_tick_hz(hz);
        }

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
            // Live values of the map's declared actions, so a map author
            // can watch bindings land without wiring any UI.
            let map_actions = match &self.scene {
                SceneKind::Map(render) => render.lock().expect("render mutex").action_lines(),
                SceneKind::Circle(_) => Vec::new(),
            };
            // …and what the scene itself is marched at (F3), which is the
            // number a dropped frame rate is usually about.
            let marched = self
                .renderer
                .as_ref()
                .map_or((0, 0), SceneRenderer::render_dims);
            build_hud(&ctx, tick, fps, &hud, &map_actions, marched, self.render_div);
        }
        // The map's own scripted HUD (health bar / panels / buttons), painted
        // over the status window; button clicks feed the next tick's command.
        self.paint_map_hud(&ctx);
        // …and a compiled map's own egui, over that. An authoring tool
        // wants docks and trees rather than positioned rectangles, and the
        // local layer is already outside the state hash, so handing it the
        // context reaches no further than a `status` line does.
        self.paint_map_ui(&ctx);
        // The key-bindings panel (F2), on top of everything.
        if self.rebind_open {
            self.build_rebind_panel(&ctx);
        }
        let out = ctx.end_pass();
        self.egui_state
            .as_mut()?
            .handle_platform_output(window, out.platform_output);
        let jobs = ctx.tessellate(out.shapes, out.pixels_per_point);
        Some((jobs, out.textures_delta, out.pixels_per_point))
    }

    /// Let a compiled map's local layer draw its own egui, then route
    /// whatever it submitted — a panel button is a command like any other.
    ///
    /// Scripted maps no-op: a Rhai layer cannot hold a `Context`, so its
    /// surface stays the `ui_*` verbs.
    fn paint_map_ui(&mut self, ctx: &egui::Context) {
        match &mut self.sim {
            Sim::Map(map) => {
                map.local_layer.on_local_ui(ctx).expect("map local_ui");
                map.route_local_commands();
            }
            Sim::NetMap(nm) => {
                nm.local_layer.on_local_ui(ctx).expect("map local_ui");
                let commands = nm.render.lock().expect("render mutex").drain_commands();
                nm.pending.extend(commands);
            }
            // A replay is watched, not driven, and the circle demo has no
            // map layer at all.
            _ => {}
        }
    }

    /// Paint the scripted map HUD ([`MapRender::ui_widgets`]) this frame and
    /// route button clicks into the next tick's input command. Screen-space,
    /// render-side only — never touches the sim state hash.
    #[allow(clippy::too_many_lines)] // a flat per-widget-kind match
    fn paint_map_hud(&mut self, ctx: &egui::Context) {
        let SceneKind::Map(render) = &self.scene else {
            return;
        };
        let render = render.clone();
        let size = ctx.content_rect().size();
        let (mut widgets, camera) = {
            let mut r = render.lock().expect("render mutex");
            r.set_ui_viewport(size.x as i64, size.y as i64);
            (r.ui_widgets().to_vec(), r.camera())
        };
        if widgets.is_empty() {
            return;
        }

        // Widgets the map pinned over a world point are placed here, where the
        // renderer -- and so the projection it last drew with -- is in reach.
        // An anchor behind the camera drops its widget rather than smearing it
        // along an edge.
        if widgets.iter().any(|w| w.over.is_some()) {
            let ppp = ctx.pixels_per_point();
            let renderer = self.renderer.as_ref();
            widgets.retain_mut(|w| {
                let Some(over) = w.over else { return true };
                let Some((px, py)) = renderer.and_then(|r| r.project_point(&camera, over)) else {
                    return false;
                };
                let (x, y) = w.widget.spot();
                *x += (px / ppp) as i32;
                *y += (py / ppp) as i32;
                true
            });
        }

        let mut clicked_bits = 0u64;
        for (i, w) in widgets.iter().enumerate() {
            match &w.widget {
                map_render::UiWidget::Image {
                    tex,
                    x,
                    y,
                    scale,
                    tint,
                    turn,
                } => {
                    if let Some(h) = self.ui_handle(ctx, &render, *tex) {
                        let (tint, turn) = (*tint, *turn);
                        Self::ui_area(ctx, i, *x, *y, |ui| {
                            let sz = tex_points(&h) * *scale;
                            let mut img = egui::Image::new(&h)
                                .fit_to_exact_size(sz)
                                .tint(egui::Color32::from_rgba_unmultiplied(
                                    (tint >> 16) as u8,
                                    (tint >> 8) as u8,
                                    tint as u8,
                                    (tint >> 24) as u8,
                                ));
                            if turn != 0.0 {
                                // About its own middle, which is the only
                                // origin a caller placing a picture by its
                                // corner can predict.
                                img = img.rotate(turn, egui::Vec2::splat(0.5));
                            }
                            ui.add(img);
                        });
                    }
                }
                map_render::UiWidget::ImageClip {
                    tex,
                    x,
                    y,
                    frac,
                    scale,
                } => {
                    if let Some(h) = self.ui_handle(ctx, &render, *tex) {
                        Self::ui_area(ctx, i, *x, *y, |ui| {
                            let full = tex_points(&h) * *scale;
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
                map_render::UiWidget::Anim { gif, x, y, scale } => {
                    if let Some(h) = self.ui_gif_handle(ctx, &render, *gif) {
                        Self::ui_area(ctx, i, *x, *y, |ui| {
                            let sz = tex_points(&h) * *scale;
                            ui.add(egui::Image::new(&h).fit_to_exact_size(sz));
                        });
                    }
                }
                map_render::UiWidget::Text {
                    x,
                    y,
                    text,
                    size,
                    scale,
                    tint,
                } => {
                    let col = egui::Color32::from_rgba_unmultiplied(
                        (tint >> 16) as u8,
                        (tint >> 8) as u8,
                        *tint as u8,
                        (tint >> 24) as u8,
                    );
                    Self::ui_area(ctx, i, *x, *y, |ui| {
                        // Never wrap — a HUD number like "20" must stay on one
                        // line (the default wraps it to "2" / "0" near an edge).
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(text)
                                    .size(*size * *scale)
                                    .strong()
                                    .color(col),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        );
                    });
                }
                map_render::UiWidget::TextWrap {
                    x,
                    y,
                    text,
                    size,
                    width,
                    color,
                    scale,
                } => {
                    let col = egui::Color32::from_rgb(
                        (color >> 16) as u8,
                        (color >> 8) as u8,
                        *color as u8,
                    );
                    Self::ui_area(ctx, i, *x, *y, |ui| {
                        // Word-wrap within `width`: a dialogue paragraph.
                        ui.set_max_width(*width * *scale);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(text)
                                    .size(*size * *scale)
                                    .strong()
                                    .color(col),
                            )
                            .wrap_mode(egui::TextWrapMode::Wrap),
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
                    scale,
                } => {
                    // Resolve all three state textures up front (immutable
                    // borrow of self ends before the closure borrows it again).
                    let normal = self.ui_handle(ctx, &render, *tex);
                    let hovered = self.ui_handle(ctx, &render, *hover);
                    let held = self.ui_handle(ctx, &render, *pressed);
                    let Some(normal) = normal else { continue };
                    Self::ui_area(ctx, i, *x, *y, |ui| {
                        let sz = tex_points(&normal) * *scale;
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
        // A click is a one-tick edge, latched into exactly the path the
        // map consumes: the bridge latch (taken by `local_tick` via
        // `ui_clicks()`) for a script-input map, else the legacy snapshot
        // (consumed by the next `to_command`).
        if clicked_bits != 0 {
            if self.script_input() {
                if let SceneKind::Map(render) = &self.scene {
                    render
                        .lock()
                        .expect("render mutex")
                        .add_ui_clicks(clicked_bits as i64);
                }
            } else {
                self.input.ui_bits |= clicked_bits;
            }
        }
    }

    /// A fixed-position, foreground overlay area for one HUD widget.
    fn ui_area(ctx: &egui::Context, id: usize, x: i32, y: i32, add: impl FnOnce(&mut egui::Ui)) {
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
            let image =
                egui::ColorImage::from_rgba_unmultiplied([*w as usize, *h as usize], pixels);
            let handle = ctx.load_texture(
                format!("monada_ui_{id}"),
                image,
                egui::TextureOptions::NEAREST,
            );
            self.ui_tex_cache[id] = Some(handle);
        }
        self.ui_tex_cache[id].clone()
    }

    /// The egui handle for an animated HUD image's frame at this instant: pick
    /// the wall-clock-current frame from the gif's per-frame delays, uploading
    /// it (nearest) the first time it's shown.
    fn ui_gif_handle(
        &mut self,
        ctx: &egui::Context,
        render: &Arc<Mutex<MapRender>>,
        id: usize,
    ) -> Option<egui::TextureHandle> {
        if id >= self.ui_gif_cache.len() {
            self.ui_gif_cache.resize_with(id + 1, Vec::new);
        }
        let elapsed = Instant::now()
            .saturating_duration_since(self.epoch)
            .as_millis() as u64;
        // Find the current frame + grab its pixels if not yet uploaded.
        let pixels = {
            let r = render.lock().expect("render mutex");
            let gif = r.ui_gif_data(id)?;
            let n = gif.frames.len();
            if self.ui_gif_cache[id].len() != n {
                self.ui_gif_cache[id] = vec![None; n];
            }
            let total: u64 = gif
                .frames
                .iter()
                .map(|(_, d)| u64::from(*d))
                .sum::<u64>()
                .max(1);
            let mut t = elapsed % total;
            let mut idx = n - 1;
            for (i, (_, d)) in gif.frames.iter().enumerate() {
                if t < u64::from(*d) {
                    idx = i;
                    break;
                }
                t -= u64::from(*d);
            }
            if self.ui_gif_cache[id][idx].is_some() {
                return self.ui_gif_cache[id][idx].clone();
            }
            (idx, gif.frames[idx].0.clone(), gif.width, gif.height)
        };
        let (idx, px, w, h) = pixels;
        let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px);
        let handle = ctx.load_texture(
            format!("monada_gif_{id}_{idx}"),
            image,
            egui::TextureOptions::NEAREST,
        );
        self.ui_gif_cache[id][idx] = Some(handle.clone());
        Some(handle)
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
    fn update_scene(&mut self, alpha: f64, dt: f64) {
        // Grid poses first, every frame, before anything composes against one
        // (docs/plans/ship-physics.md §4): a map writes a hull's pose once per
        // tick, and the render eases onto it over that tick so riders, props,
        // the fog and the camera all ride one smooth frame instead of a 30 Hz
        // staircase. Rebuilding instances first would seat this frame's riders
        // on last frame's hull — a one-frame shear, which is precisely what
        // composing everything from a single transform is supposed to make
        // impossible.
        if let SceneKind::Map(render) = &self.scene {
            let mut render = render.lock().expect("render mutex");
            render.advance_grid_poses(dt);
            // The rider half of the same smoothing: a hull that eases while
            // the crew walking it still steps is only half a fix, and the
            // camera follows the crew.
            render.advance_entity_tracks(dt);
        }
        // The physics body mirror (volume maps, plan §1d) syncs beside the
        // sprite rebuild on every path; `dt` drives its render-side wheel
        // spin. `sync_physics` never touches hashed state. It runs BEFORE the
        // sprite rebuild for the same reason as the pose pass: once a body can
        // pose a script grid (ship-physics S-3), riders composed first would
        // lag the hull they stand on.
        match (&self.sim, &mut self.scene) {
            (Sim::Map(map), SceneKind::Map(render)) => {
                let world = map.world.lock().expect("world mutex");
                let mut render = render.lock().expect("render mutex");
                if let Some(phys) = &map.phys {
                    render.sync_physics(&phys.lock().expect("physics mutex"), dt);
                }
                render.build_instances(&world);
            }
            (Sim::NetMap(nm), SceneKind::Map(render)) => {
                let world = nm.session.driver().world().clone();
                let guard = world.lock().expect("world mutex");
                let mut render = render.lock().expect("render mutex");
                if let Some(phys) = nm.session.driver().physics() {
                    render.sync_physics(&phys.lock().expect("physics mutex"), dt);
                }
                render.build_instances(&guard);
            }
            (Sim::Replay(r), SceneKind::Map(render)) => {
                let world = r.driver.world().clone();
                let guard = world.lock().expect("world mutex");
                let mut render = render.lock().expect("render mutex");
                if let Some(phys) = r.driver.physics() {
                    render.sync_physics(&phys.lock().expect("physics mutex"), dt);
                }
                render.build_instances(&guard);
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
        let (sounds, blips, loops, music) = match &self.scene {
            SceneKind::Map(render) => render.lock().expect("render mutex").drain_audio(),
            SceneKind::Circle(_) => return,
        };
        for (path, gain) in sounds {
            self.audio.play(&path, gain, now);
        }
        for (wave, freq, dur, gain) in blips {
            self.audio.blip(wave, freq, dur, gain);
        }
        self.audio.sync_loops(&loops, now);
        match music {
            Some(map_render::MusicCmd::Play(path)) => self.audio.play_music(&path),
            Some(map_render::MusicCmd::Stop) => self.audio.stop_music(),
            None => {}
        }
    }

    // Long by nature: the whole frame protocol (input → sim → sync → HUD →
    // render → present) lives here so the profile section timers can bracket
    // each stage in one place.
    #[allow(clippy::too_many_lines)]
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

        let t0 = Instant::now();

        // Refresh the mouse-aim direction, then advance. A real-time map paces
        // its `tick` on the wall clock with the per-tick input snapshot; a
        // command-driven map snaps to the current world.
        self.update_aim();
        self.drive_local_frame(dt);
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
        let t_sim = t0.elapsed();

        self.drive_camera(dt);
        self.update_scene(alpha, dt);
        let t_sync = t0.elapsed().saturating_sub(t_sim);

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
        let t_pre_hud = t0.elapsed();
        let hud = self.run_hud(&window);
        let t_hud = t0.elapsed().saturating_sub(t_pre_hud);

        let mut settings = OpticastSettings::for_oracle_framebuffer(size.width, size.height);
        // The ray march (and its derived GPU step budget) is bounded by
        // `max_scan_dist` VOXELS around the camera — a sphere. The oracle
        // default (1024) is fine for a scene hugging the camera, but zooming out
        // (camera distance up to 2000) pushes far geometry past it, so the scan
        // sphere cuts a circle out of the floor/ceiling around the camera's
        // nadir. Cover the whole zoom range so nothing vanishes on zoom-out.
        settings.max_scan_dist = settings.max_scan_dist.max(4096);
        // roxlap 0.30: `FrameParams` is `#[non_exhaustive]` — build from `new`
        // and override. The GPU step budget + FOV are now derived from the scan
        // distance + projection (backends can no longer disagree), and the mip
        // scan distance moved to `RenderOptions`; the circle scene keeps the
        // defaults. Sprites are flat-lit on both backends — `draw_sprites` is
        // the on/off opt-in for the movers; `side_shades` stays flat here (the
        // map paths override it from their declared sun).
        let mut frame = FrameParams::new(&settings);
        frame.sky_color = SKY_COLOR;

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        // roxlap 0.7: render() composites without presenting; the frame is
        // finished by exactly one of paint_egui (HUD) or present. The map
        // scene lives behind a Mutex, so set + render under one lock.
        let debug = self.debug_hud;
        let t_pre_render = t0.elapsed();
        match &mut self.scene {
            SceneKind::Circle(scene) => {
                renderer.set_sprites(scene.sprites());
                renderer.render(scene.scene_mut(), &camera, &frame);
            }
            SceneKind::Map(render) => {
                // The map lights itself (grid side-shades + sky) and drives
                // its animated actors; `dt` advances their animation clocks.
                // F1 (`debug`) overlays the collision footprints.
                render
                    .lock()
                    .expect("render mutex")
                    .render_into(renderer, &camera, &settings, SKY_COLOR, dt, debug);
            }
        }
        let t_render = t0.elapsed().saturating_sub(t_pre_render);
        let t_pre_present = t0.elapsed();
        match hud {
            Some((jobs, textures, ppp)) => renderer.paint_egui(&jobs, &textures, ppp),
            None => renderer.present(),
        }

        let elapsed = self.epoch.elapsed();
        if let Some(log) = self.profile.as_mut() {
            let total = t0.elapsed();
            if total.as_secs_f64() > 0.020 {
                let t_present = total.saturating_sub(t_pre_present);
                eprintln!(
                    "[profile] t={:7.2}s frame {:6.1}ms — sim {:6.1} sync {:6.1} \
                     hud {:6.1} render {:6.1} present {:6.1}",
                    elapsed.as_secs_f64(),
                    total.as_secs_f64() * 1e3,
                    t_sim.as_secs_f64() * 1e3,
                    t_sync.as_secs_f64() * 1e3,
                    t_hud.as_secs_f64() * 1e3,
                    t_render.as_secs_f64() * 1e3,
                    t_present.as_secs_f64() * 1e3,
                );
            }
            // …and what the frames cost when none of them crossed the
            // line, which is the half a slow-frame log cannot say.
            if let Some(summary) = log.record(Instant::now(), total) {
                eprintln!("{summary}");
            }
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
        // roxlap 0.30: `want_gpu: bool` became `backend: BackendPreference`
        // (`PreferGpu` = try GPU, warn + fall back to CPU on init failure;
        // `Cpu` = force software).
        let want_gpu = std::env::var_os("ROXLAP_GPU").map_or(true, |v| v != "0" && !v.is_empty());
        let opts = RenderOptions {
            backend: if want_gpu {
                BackendPreference::PreferGpu
            } else {
                BackendPreference::Cpu
            },
            ..RenderOptions::default()
        };
        // roxlap-render is now decoupled from winit: it takes any
        // raw-window-handle provider plus an explicit initial size.
        let size = window.inner_size();
        let mut renderer = SceneRenderer::new(window.clone(), (size.width, size.height), &opts);
        if let Some(info) = renderer.adapter_info() {
            eprintln!("monada-host: GPU backend — {info}");
        } else {
            // The software marcher's cost scales with the pixel count and
            // it can't hold the window's native resolution on the volume
            // maps, whatever the player asked for.
            self.render_div = self.render_div.max(CPU_RENDER_DIV);
            eprintln!("monada-host: CPU backend (rendering at half resolution)");
        }
        Self::apply_render_div(&mut renderer, self.render_div);

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
            // Which modifiers are held. Only the fullscreen chord reads
            // this; every other input is a single key or button and
            // resolves through the binding table.
            WindowEvent::ModifiersChanged(mods) => self.modifiers = mods.state(),
            // **Auto-repeat is not a press.** The OS re-sends `Pressed` while
            // a key is held, and `action(id, down)` is an EDGE -- so a map
            // that starts something on the press starts it again every
            // repeat, which is a hold that can never get anywhere. Held
            // actions read the state rather than the edge and lose nothing
            // by this.
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        physical_key: PhysicalKey::Code(code),
                        state,
                        repeat: false,
                        ..
                    },
                ..
            } => {
                let pressed = state == ElementState::Pressed;
                let escape = code == winit::keyboard::KeyCode::Escape;
                // Key capture for the rebind panel takes priority (no text
                // field can hold focus, so egui never consumes keys here).
                // Esc cancels the capture instead of binding.
                if self.capturing.is_some() {
                    if pressed {
                        if escape {
                            self.capturing = None;
                        } else {
                            self.capture_input(PhysInput::Key(code));
                        }
                    }
                } else if self.rebind_open && pressed && escape {
                    // Esc closes the open panel rather than quitting the app.
                    self.rebind_open = false;
                    self.rebind_notice = None;
                } else if pressed && self.modifiers.alt_key() && is_enter(code) {
                    // **The one chord the host knows.** Alt+Enter is what
                    // every windowed game answers to for fullscreen, and
                    // the binding table cannot hold it: an entry there is
                    // one input, with nowhere to write a modifier. So it
                    // is checked here, ahead of the table -- which also
                    // keeps a plain Enter free for whatever a map binds
                    // it to. `ui.fullscreen` is the rebindable half.
                    self.toggle_fullscreen();
                } else if !consumed {
                    self.dispatch_input(event_loop, PhysInput::Key(code), pressed);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
            }
            // Mouse-wheel zoom: nudge the camera distance directly (not via the
            // key path, which real-time maps suppress). Wheel up = zoom in.
            WindowEvent::MouseWheel { delta, .. } if !consumed => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(y),
                    // Trackpads report pixels; ~a notch per 40 px.
                    MouseScrollDelta::PixelDelta(p) => p.y / 40.0,
                };
                if scroll != 0.0 {
                    self.scene.orbit(0.0, 0.0, -scroll * WHEEL_ZOOM_STEP);
                    if let Some(window) = self.window.as_ref() {
                        window.request_redraw();
                    }
                }
            }
            // A click on the panel is consumed by egui (so a "rebind"
            // button doesn't capture its own click); an off-panel click
            // while capturing binds that mouse button.
            WindowEvent::MouseInput { state, button, .. } if !consumed => {
                if let Some(input) = PhysInput::from_mouse(button) {
                    if self.capturing.is_some() {
                        if state == ElementState::Pressed {
                            self.capture_input(input);
                        }
                    } else {
                        self.dispatch_input(event_loop, input, state == ElementState::Pressed);
                    }
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

/// A rebind-panel interaction, recorded during the egui closure and
/// applied after it (so the closure needn't hold `&mut self.bindings`).
#[derive(Clone, Copy)]
enum PanelAction {
    /// Begin capturing the next key/mouse press for this target.
    Capture(ActionRef),
    /// Reset one target to its default.
    Reset(ActionRef),
    /// Reset every binding to its default.
    ResetAll,
    /// Close the panel.
    Close,
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
/// `map_actions` = the map's declared actions as `(id, value)` lines.
fn build_hud(
    ctx: &egui::Context,
    tick: u64,
    fps: f32,
    hud: &HudState,
    map_actions: &[(String, String)],
    marched: (u32, u32),
    render_div: u32,
) {
    egui::Window::new("monada")
        .title_bar(false)
        .resizable(false)
        .anchor(egui::Align2::LEFT_TOP, egui::vec2(8.0, 8.0))
        .show(ctx, |ui| {
            ui.label(format!("tick {tick}"));
            ui.label(format!("fps  {fps:.0}"));
            ui.label(format!(
                "res  {}x{}{}",
                marched.0,
                marched.1,
                if render_div > 1 {
                    format!(" (1/{render_div})")
                } else {
                    String::new()
                },
            ));
            match hud {
                HudState::Local { selected } => {
                    match selected {
                        Some(i) => ui.label(format!("selected mover #{i}")),
                        None => ui.label("selected mover —"),
                    };
                    ui.separator();
                    ui.label("arrows orbit · W/S · wheel zoom");
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
                    ui.label("arrows orbit · W/S · wheel zoom");
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
                    ui.label("arrows orbit · W/S · wheel zoom · Esc quit");
                }
            }
            if !map_actions.is_empty() {
                ui.separator();
                for (id, value) in map_actions {
                    ui.label(format!("{id} {value}"));
                }
            }
        });
}

impl App {
    /// The binding contexts active right now, bottom → top. The host
    /// picks the gameplay context from the map's tick model; the map
    /// itself gains push/pop control in a later plan step.
    fn context_stack(&self) -> Vec<Context> {
        let mut stack = vec![Context::Global];
        stack.push(if self.realtime() {
            Context::RealTime
        } else {
            Context::TurnBased
        });
        // The map's own `[[action]]` bindings shadow the engine's
        // gameplay keys, but replay transport stays on top of both.
        if matches!(self.scene, SceneKind::Map(_)) {
            stack.push(Context::MapGameplay);
        }
        if matches!(self.sim, Sim::Replay(_)) {
            stack.push(Context::Replay);
        }
        stack
    }

    /// Resolve one physical input through the binding table and apply
    /// the action it lands on (if any).
    fn dispatch_input(&mut self, event_loop: &ActiveEventLoop, input: PhysInput, down: bool) {
        if let Some(action) = self.bindings.resolve(&self.context_stack(), input) {
            self.apply_action(event_loop, action, down);
        }
    }

    /// The finest the active backend will actually run: the software
    /// marcher cannot hold a native window whatever the player picks.
    fn render_floor(&self) -> u32 {
        match self.renderer.as_ref() {
            Some(r) if r.adapter_info().is_none() => CPU_RENDER_DIV,
            _ => 1,
        }
    }

    /// March the scene at `1/div` of the window and let the blit take it
    /// back up, nearest.
    ///
    /// **A divisor of one is not the same call as a scale of one.**
    /// `Native` is the pre-RP path — logical == swapchain, a straight
    /// blit — so a map that never asks for less pays nothing for the
    /// question, not even a resolve that copies.
    fn apply_render_div(renderer: &mut SceneRenderer, div: u32) {
        renderer.set_render_resolution(if div <= 1 {
            RenderResolution::Native
        } else {
            #[allow(clippy::cast_precision_loss)]
            RenderResolution::Scale(1.0 / div as f32)
        });
    }

    /// Go fullscreen, or come back.
    ///
    /// **Borderless rather than exclusive.** It takes the monitor it is
    /// already on (`None` = the current one), changes no video mode, and
    /// alt-tabs away without a mode switch -- which is what somebody
    /// toggling this while working on a map wants. The size change
    /// arrives as an ordinary `Resized`, so the renderer and egui follow
    /// by the path they already take.
    fn toggle_fullscreen(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let full = window.fullscreen().is_none();
        window.set_fullscreen(full.then(|| Fullscreen::Borderless(None)));
    }

    /// Execute a resolved action. Held actions (camera / move axis)
    /// track `down`; one-shot actions fire on press only.
    fn apply_action(&mut self, event_loop: &ActiveEventLoop, action: ActionRef, down: bool) {
        let action = match action {
            ActionRef::Base(action) => action,
            // A map-declared action: update its live value (polled by the
            // local layer / debug HUD), then fire the edge into the map's
            // `action(id, down)` handler.
            ActionRef::Map { index, part } => {
                if let SceneKind::Map(render) = &self.scene {
                    render
                        .lock()
                        .expect("render mutex")
                        .action_set(index, part, down);
                }
                if let Some(id) = self.bindings.map_actions().get(index).map(|a| a.id.clone()) {
                    match &mut self.sim {
                        Sim::Map(map) => map.action(&id, down),
                        Sim::NetMap(nm) => nm.action(&id, down),
                        // Replay / non-map sims take no live map input.
                        _ => {}
                    }
                }
                return;
            }
        };
        match action {
            Action::Quit if down => {
                self.save_replay();
                event_loop.exit();
            }
            // F1 toggles the debug overlay (tick / FPS / status / lockstep).
            Action::DebugHud if down => self.debug_hud = !self.debug_hud,
            // F11, and Alt+Enter beside it (`window_event`).
            Action::Fullscreen if down => self.toggle_fullscreen(),
            // F3 steps the scene's own resolution down and back round. A
            // key rather than only a launch flag: how coarse is too
            // coarse is a thing you judge by flipping between them while
            // looking at the same frame, which is the same argument the
            // sprite-facing toggle is here for.
            Action::RenderScale if down => {
                self.render_div = next_render_div(self.render_div).max(self.render_floor());
                if let Some(renderer) = self.renderer.as_mut() {
                    Self::apply_render_div(renderer, self.render_div);
                    eprintln!(
                        "monada-host: rendering at 1/{} of the window ({:?})",
                        self.render_div,
                        renderer.render_dims(),
                    );
                }
            }
            // F2 toggles the key-bindings panel; closing cancels any capture.
            Action::OpenBindings if down => {
                self.rebind_open = !self.rebind_open;
                if !self.rebind_open {
                    self.capturing = None;
                }
            }
            Action::OrbitLeft => self.keys.yaw_left = down,
            Action::OrbitRight => self.keys.yaw_right = down,
            Action::OrbitUp => self.keys.pitch_up = down,
            Action::OrbitDown => self.keys.pitch_down = down,
            // Zoom binds in the turn-based context; for a real-time map the
            // same keys land on the move axis and the script owns the camera.
            Action::ZoomIn => self.keys.zoom_in = down,
            Action::ZoomOut => self.keys.zoom_out = down,
            Action::MoveFwd => self.input.fwd = down,
            Action::MoveBack => self.input.back = down,
            Action::MoveLeft => self.input.left = down,
            Action::MoveRight => self.input.right = down,
            Action::Dodge => self.input.dodge = down,
            // Held-attack for a real-time map; the primary pointer is the
            // chess-style click gesture (press only).
            Action::Attack => self.input.attack = down,
            Action::PointerPrimary if down => self.on_click(),
            // Replay transport.
            Action::ReplayPause if down => self.replay_control(|r| r.paused = !r.paused),
            Action::ReplaySlower if down => self.replay_control(|r| r.scale_speed(0.5)),
            Action::ReplayFaster if down => self.replay_control(|r| r.scale_speed(2.0)),
            _ => {}
        }
    }

    /// Complete a rebind capture: bind the pressed `input` to the pending
    /// target, note any slot it was taken from, and persist the table.
    fn capture_input(&mut self, input: PhysInput) {
        if let Some(target) = self.capturing.take() {
            self.rebind_notice = self.bindings.rebind(target, input);
            self.bindings.save();
        }
    }

    /// Paint the key-bindings panel (F2). Immediate-mode egui can't hold a
    /// `&mut self.bindings` across the closure, so widget clicks record a
    /// [`PanelAction`] that is applied afterwards.
    fn build_rebind_panel(&mut self, ctx: &egui::Context) {
        let slots = self.bindings.slots();
        let capturing = self.capturing;
        let modified = self.bindings.is_modified();
        let notice = self.rebind_notice.clone();
        // A slot is inert if its context is not active for the running map,
        // or a higher active context wins its key — rebinding it would have
        // no visible effect (e.g. the base real-time move actions once a map
        // declares its own on the same keys). Precompute against the live
        // context stack so those rows can be shown disabled.
        let active = self.context_stack();
        let inert: Vec<bool> = slots
            .iter()
            .map(|s| {
                !active.contains(&s.context)
                    || (!s.inputs.is_empty()
                        && s.inputs
                            .iter()
                            .all(|&i| self.bindings.resolve(&active, i) != Some(s.target)))
            })
            .collect();
        let mut act: Option<PanelAction> = None;
        egui::Window::new("Key bindings")
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8.0, 8.0))
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Click a binding, then press a key. Esc closes.");
                if let Some(displaced) = &notice {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xE0, 0xA0, 0x30),
                        format!("Took the key from “{displaced}”."),
                    );
                }
                egui::ScrollArea::vertical()
                    .max_height(420.0)
                    .show(ui, |ui| {
                        let mut group: Option<Context> = None;
                        for (slot, &dim) in slots.iter().zip(&inert) {
                            if group != Some(slot.context) {
                                ui.separator();
                                ui.strong(slot.context.title());
                                group = Some(slot.context);
                            }
                            ui.horizontal(|ui| {
                                ui.set_min_width(240.0);
                                // Dim inert rows and disable their controls,
                                // so a no-effect rebind isn't offered.
                                ui.add_enabled_ui(!dim, |ui| {
                                    if dim {
                                        ui.weak(&slot.label);
                                    } else {
                                        ui.label(&slot.label);
                                    }
                                    let label = if capturing == Some(slot.target) {
                                        "press a key…".to_owned()
                                    } else if slot.inputs.is_empty() {
                                        "—".to_owned()
                                    } else {
                                        slot.inputs
                                            .iter()
                                            .map(|i| i.label())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    };
                                    let mut btn = ui.button(label);
                                    if dim {
                                        btn = btn.on_hover_text(
                                            "Shadowed by this map — no effect here.",
                                        );
                                    }
                                    if btn.clicked() {
                                        act = Some(PanelAction::Capture(slot.target));
                                    }
                                    if ui.small_button("↺").on_hover_text("reset").clicked() {
                                        act = Some(PanelAction::Reset(slot.target));
                                    }
                                });
                            });
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(modified, egui::Button::new("Reset all"))
                        .clicked()
                    {
                        act = Some(PanelAction::ResetAll);
                    }
                    if ui.button("Close").clicked() {
                        act = Some(PanelAction::Close);
                    }
                });
            });
        if let Some(act) = act {
            self.apply_panel_action(act);
        }
    }

    /// Apply one [`PanelAction`] recorded during the panel's egui pass.
    fn apply_panel_action(&mut self, act: PanelAction) {
        // Any interaction but starting a fresh capture clears the notice.
        match act {
            PanelAction::Capture(target) => {
                self.capturing = Some(target);
                self.rebind_notice = None;
            }
            PanelAction::Reset(target) => {
                self.bindings.reset(target);
                self.bindings.save();
                self.capturing = None;
                self.rebind_notice = None;
            }
            PanelAction::ResetAll => {
                self.bindings.reset_all();
                self.bindings.save();
                self.capturing = None;
                self.rebind_notice = None;
            }
            PanelAction::Close => {
                self.rebind_open = false;
                self.capturing = None;
                self.rebind_notice = None;
            }
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

    /// Run the map's `local_frame(dt)` handler (hover / tooltips / camera —
    /// the presentation cadence) and route whatever it submitted.
    fn drive_local_frame(&mut self, dt: f64) {
        let dt = Fixed::from_f64(dt);
        match &mut self.sim {
            Sim::Map(map) if map.local_layer.has_local_frame() => {
                map.local_layer
                    .on_local_frame(dt)
                    .expect("map local_frame handler");
                map.route_local_commands();
            }
            Sim::NetMap(nm) if nm.local_layer.has_local_frame() => {
                nm.local_layer
                    .on_local_frame(dt)
                    .expect("map local_frame handler");
                let commands = nm.render.lock().expect("render mutex").drain_commands();
                nm.pending.extend(commands);
            }
            // Replays and non-map sims have no local layer.
            _ => {}
        }
    }

    /// Whether the running map consumes cursor-derived state at all: its
    /// local layer polls `pick_*`/`aim_yaw` (has `local_frame`/`local_tick`)
    /// or the legacy real-time snapshot needs `aim_yaw` refreshed. A plain
    /// turn-based map (chess) hits neither — skip the per-frame world lock
    /// + entity pick entirely.
    fn wants_cursor(&self) -> bool {
        match &self.sim {
            Sim::Map(map) => {
                map.tick_dt.is_some()
                    || map.local_layer.has_local_frame()
                    || map.local_layer.has_local_tick()
            }
            Sim::NetMap(nm) => {
                nm.tick_dt.is_some()
                    || nm.local_layer.has_local_frame()
                    || nm.local_layer.has_local_tick()
            }
            _ => false,
        }
    }

    /// Whether the running map assembles its own per-tick input in
    /// `local_tick` (⇒ HUD clicks route via the bridge latch, not the
    /// legacy snapshot).
    fn script_input(&self) -> bool {
        match &self.sim {
            Sim::Map(map) => map.local_layer.has_local_tick(),
            Sim::NetMap(nm) => nm.local_layer.has_local_tick(),
            _ => false,
        }
    }

    /// Refresh the cursor-derived state on the render bridge (`pick_ground`
    /// / `aim_yaw` for the local layer), and mirror the aim into the legacy
    /// input snapshot for maps that predate `local_tick`.
    fn update_aim(&mut self) {
        if !self.wants_cursor() {
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
            let world = self.sim.world();
            let mut r = render.lock().expect("render mutex");
            let w = world.lock().expect("world mutex");
            r.set_cursor_ray(&w, ray.origin, ray.dir);
            self.input.aim_yaw = r.aim_f64();
        }
    }

    /// Apply a transport control to the replay sim, if one is running.
    fn replay_control(&mut self, f: impl FnOnce(&mut ReplaySim)) {
        if let Sim::Replay(r) = &mut self.sim {
            f(r);
        }
    }
}
