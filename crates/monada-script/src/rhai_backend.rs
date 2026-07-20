//! The v0 Rhai [`ScriptBackend`]. Registers the [`monada_fixed`] sim
//! number types and the host API (DESIGN.md §3.3) against a shared
//! [`World`], then drives the map's `init` / `tick` triggers.

// Host-API glue casts script `i64`s to the engine's id/index types; the
// values are small and the conversions are intentional.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::sync::{Arc, Mutex};

use monada_fixed::{trig, Fixed, FixedVec3};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};
use rhai::{Array, Dynamic, Engine, ImmutableString, Scope, AST};

use crate::{ScriptBackend, ScriptError, SharedBridge, SharedWorld, UiEvent};

/// The buffer `ui_emit_event` pushes into and [`drain_ui_events`] empties.
/// Shared (`Arc<Mutex<_>>`) for the same reason as [`SharedWorld`]:
/// `sync`-feature Rhai needs `Send + Sync` host closures.
///
/// [`drain_ui_events`]: ScriptBackend::drain_ui_events
type UiEventBuffer = Arc<Mutex<Vec<UiEvent>>>;

/// Arity of the map's `command` trigger: `command(player, verb, target, arg)`.
const COMMAND_ARITY: usize = 4;
/// Arity of the map's `tick` trigger: `tick()`.
const TICK_ARITY: usize = 0;

/// Rhai-backed scripting runtime over a shared [`World`].
// The `has_*` handler-presence flags are independent booleans; a flat set
// is the natural shape (the lint's state-machine suggestion would obscure
// it).
#[allow(clippy::struct_excessive_bools)]
pub struct RhaiBackend {
    engine: Engine,
    ast: Option<AST>,
    scope: Scope<'static>,
    world: SharedWorld,
    /// Whether the loaded script defines a `command/4` handler. Decided
    /// once at [`load`](RhaiBackend::load) so [`on_command`](RhaiBackend::on_command)
    /// can no-op a handler-less map *without* swallowing a genuine
    /// `ErrorFunctionNotFound` raised by a typo'd host-API call inside an
    /// existing handler — that must surface as the bug it is (it could
    /// otherwise desync one peer silently).
    has_command: bool,
    /// Whether the loaded script defines a `tick/0` handler (decided at
    /// [`load`](RhaiBackend::load), like `has_command`). A command-driven
    /// map (e.g. turn-based chess) has no `tick`, so
    /// [`on_tick`](RhaiBackend::on_tick) still advances the counter but
    /// calls nothing.
    has_tick: bool,
    /// Whether the loaded script defines a `tick/1` handler that receives `dt`
    /// (the tick duration as a `Fixed`). Takes priority over `tick/0` when set.
    has_tick_with_dt: bool,
    /// The tick duration for a fixed-rate map (`ratio(1, hz)`), set via
    /// [`set_tick_hz`](RhaiBackend::set_tick_hz). `None` for command-driven maps.
    tick_dt: Option<Fixed>,
    /// UI/HUD events the script emitted via `ui_emit_event`, awaiting a
    /// [`drain_ui_events`](ScriptBackend::drain_ui_events) by the host.
    /// Render-side only — never part of [`World`](monada_sim::World) state.
    events: UiEventBuffer,
}

impl RhaiBackend {
    /// Build a backend bound to `world`, with the sim number types and
    /// host API registered.
    #[must_use]
    pub fn new(world: SharedWorld) -> RhaiBackend {
        let mut engine = Engine::new();
        // Map scripts are semi-trusted assets, not arbitrary sandboxed
        // input — lift Rhai's conservative expression-depth limits (32
        // inside functions by default) so non-trivial setup loops / rule
        // tables compile. Determinism is unaffected.
        engine.set_max_expr_depths(0, 0);
        let events: UiEventBuffer = Arc::new(Mutex::new(Vec::new()));
        register_number_types(&mut engine);
        register_host_api(&mut engine, &world, &events);
        RhaiBackend {
            engine,
            ast: None,
            scope: Scope::new(),
            world,
            has_command: false,
            has_tick: false,
            has_tick_with_dt: false,
            tick_dt: None,
            events,
        }
    }

    /// Set the tick duration for a fixed-rate map. The value is passed to the
    /// script's `tick(dt)` handler (arity 1) each tick. Must be called before
    /// the first [`on_tick`](ScriptBackend::on_tick); calling after [`load`](ScriptBackend::load)
    /// is fine — `load` only inspects the script's arity, not the dt value.
    pub fn set_tick_hz(&mut self, hz: u32) {
        self.tick_dt = Some(Fixed::from_ratio(1, hz.max(1) as i32));
    }

    /// Register the host's render / input / command API (DESIGN.md §3.3)
    /// into the engine, forwarding to `bridge`. Call **once, before**
    /// [`on_init`](ScriptBackend::on_init); a backend with no bridge set
    /// treats those calls as undefined (a map that uses them must have a
    /// bridge). Rhai resolves calls at run time, so registering after
    /// construction is fine.
    pub fn set_bridge(&mut self, bridge: &SharedBridge) {
        register_bridge_api(&mut self.engine, bridge);
    }

    fn call<A: rhai::FuncArgs>(&mut self, name: &str, args: A) -> Result<(), ScriptError> {
        let ast = self
            .ast
            .as_ref()
            .ok_or_else(|| ScriptError::Run("no script loaded".to_string()))?;
        self.engine
            .call_fn::<()>(&mut self.scope, ast, name, args)
            .map_err(|e| ScriptError::Run(e.to_string()))
    }
}

impl ScriptBackend for RhaiBackend {
    fn load(&mut self, source: &str) -> Result<(), ScriptError> {
        let ast = self
            .engine
            .compile(source)
            .map_err(|e| ScriptError::Compile(e.to_string()))?;
        // Decide handler presence here so `on_command` never has to
        // distinguish "no handler" from "handler raised FunctionNotFound".
        let defines = |name: &str, arity: usize| {
            ast.iter_functions()
                .any(|f| f.name == name && f.params.len() == arity)
        };
        self.has_command = defines("command", COMMAND_ARITY);
        self.has_tick = defines("tick", TICK_ARITY);
        self.has_tick_with_dt = defines("tick", 1);
        self.ast = Some(ast);
        Ok(())
    }

    fn on_init(&mut self) -> Result<(), ScriptError> {
        self.call("init", ())
    }

    fn on_command(&mut self, player: PlayerId, command: &Command) -> Result<(), ScriptError> {
        // A map with no `command/4` handler simply ignores input (e.g. the
        // walk-circle scenario). This is the *only* place input is dropped;
        // once we call into a handler that exists, every error — including a
        // typo'd host-API call raising `ErrorFunctionNotFound` — propagates.
        if !self.has_command {
            return Ok(());
        }
        let ast = self
            .ast
            .as_ref()
            .ok_or_else(|| ScriptError::Run("no script loaded".to_string()))?;
        // The script interprets the command; the engine just forwards its
        // opaque fields. `arg` is a `Vec3` on the script side.
        let args = (
            i64::from(player.0),
            i64::from(command.verb),
            command.target.0 as i64,
            command.arg,
        );
        self.engine
            .call_fn::<()>(&mut self.scope, ast, "command", args)
            .map_err(|e| ScriptError::Run(e.to_string()))
    }

    fn on_tick(&mut self) -> Result<(), ScriptError> {
        // The driver owns the tick counter; the script only mutates entity
        // state via the host API. A command-driven map (no `tick` handler)
        // still advances the counter — it just runs no per-tick logic.
        self.world.lock().expect("world mutex").tick += 1;
        if self.has_tick_with_dt {
            let dt = self
                .tick_dt
                .expect("tick(dt) handler requires set_tick_hz before on_tick");
            self.call("tick", (dt,))
        } else if self.has_tick {
            self.call("tick", ())
        } else {
            Ok(())
        }
    }

    fn drain_ui_events(&mut self) -> Vec<UiEvent> {
        std::mem::take(&mut self.events.lock().expect("events mutex"))
    }
}

/// Register `Fixed` / `Vec3` and the only arithmetic scripts get (all
/// fixed-point — `no_float` Rhai forbids IEEE math entirely).
pub(crate) fn register_number_types(engine: &mut Engine) {
    engine.register_type_with_name::<Fixed>("Fixed");
    engine.register_type_with_name::<FixedVec3>("Vec3");

    // Constructors.
    engine.register_fn("fixed", |i: i64| Fixed::from_int(i as i32));
    engine.register_fn("ratio", |n: i64, d: i64| {
        Fixed::from_ratio(n as i32, d as i32)
    });
    engine.register_fn("vec3", FixedVec3::new);

    // Bridge `Fixed` -> script `i64` for integer gameplay (chess board
    // coords, archetype/field tags). Floors toward -inf; values stored
    // via `fixed(i)` round-trip exactly. Generic — the engine ships no
    // genre — but it is what lets a board game do its math in native
    // integers instead of fighting fixed-point for an L-move.
    engine.register_fn("to_int", |a: Fixed| -> i64 { i64::from(a.floor_to_int()) });
    // Fixed-returning rounding; pipe through to_int() to get an integer.
    engine.register_fn("floor", |a: Fixed| a.floor());
    engine.register_fn("round", |a: Fixed| a.round());
    engine.register_fn("ceil", |a: Fixed| a.ceil());

    // Read `Vec3` components in scripts (e.g. a command's `arg.x`). The
    // setter side stays in `vec3(...)` reconstruction — vectors are
    // value types.
    engine.register_get("x", |v: &mut FixedVec3| v.x);
    engine.register_get("y", |v: &mut FixedVec3| v.y);
    engine.register_get("z", |v: &mut FixedVec3| v.z);

    // Fixed arithmetic operators.
    engine.register_fn("+", |a: Fixed, b: Fixed| a + b);
    engine.register_fn("-", |a: Fixed, b: Fixed| a - b);
    engine.register_fn("*", |a: Fixed, b: Fixed| a * b);
    engine.register_fn("/", |a: Fixed, b: Fixed| a / b);
    engine.register_fn("-", |a: Fixed| -a);
    engine.register_fn("==", |a: Fixed, b: Fixed| a == b);
    engine.register_fn("<", |a: Fixed, b: Fixed| a < b);
    engine.register_fn(">", |a: Fixed, b: Fixed| a > b);

    // Fixed-point trig + the turn constant (the circle scenario's only
    // transcendentals).
    engine.register_fn("sin", trig::sin);
    engine.register_fn("cos", trig::cos);
    engine.register_fn("tau", || trig::TAU);
    engine.register_fn("pi", || trig::PI);
    engine.register_fn("pi_2", || trig::FRAC_PI_2);
    engine.register_fn("to_debug", |a: Fixed| format!("{a:?}"));
}

/// Register the host API (DESIGN.md §3.3). Each function locks the shared
/// world for the call; the sim is single-threaded so the lock is
/// uncontended. `events` backs `ui_emit_event` (render-side, never hashed).
fn register_host_api(engine: &mut Engine, world: &SharedWorld, events: &UiEventBuffer) {
    let w = world.clone();
    engine.register_fn("archetype", move |names: Array| -> i64 {
        let fields: Vec<String> = names
            .into_iter()
            .map(|d| d.into_string().unwrap_or_default())
            .collect();
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        i64::from(w.lock().expect("world mutex").register_archetype(&refs).0)
    });

    let w = world.clone();
    engine.register_fn("entity_create", move |arch: i64| -> i64 {
        w.lock()
            .expect("world mutex")
            .spawn(ArchetypeId(arch as u32))
            .0 as i64
    });

    let w = world.clone();
    engine.register_fn("entity_set_position", move |e: i64, p: FixedVec3| {
        w.lock()
            .expect("world mutex")
            .set_position(EntityId(e as u64), p);
    });

    let w = world.clone();
    engine.register_fn(
        "entity_set_field",
        move |e: i64, name: ImmutableString, v: Fixed| {
            w.lock()
                .expect("world mutex")
                .set_field(EntityId(e as u64), name.as_str(), v);
        },
    );

    let w = world.clone();
    engine.register_fn("rng01", move || -> Fixed {
        w.lock().expect("world mutex").rng.next_fixed_01()
    });

    let w = world.clone();
    engine.register_fn("rng_below", move |n: i64| -> i64 {
        w.lock().expect("world mutex").rng.gen_below(n as u64) as i64
    });

    // Despawn an entity; returns whether it was present. Needed for
    // capture (chess), death (RTS) — anything that removes an entity.
    let w = world.clone();
    engine.register_fn("entity_despawn", move |e: i64| -> bool {
        w.lock().expect("world mutex").despawn(EntityId(e as u64))
    });

    // The read-only world queries are shared with the local layer.
    register_world_read_api(engine, world);

    // Push a UI/HUD event (DESIGN.md §3.3). Render-side only: it lands in
    // the drain buffer, never in `World` state or the desync hash. All-
    // integer payload; the script defines what the codes mean.
    let ev = events.clone();
    engine.register_fn("ui_emit_event", move |code: i64, a: i64, b: i64, c: i64| {
        ev.lock().expect("events mutex").push(UiEvent {
            code: code as u32,
            a,
            b,
            c,
        });
    });
}

/// Register the **read-only** world queries — the subset of the sim host
/// API that is safe on both sides of the sync wall: the sim backend gets
/// them alongside the mutators, the local layer ([`crate::LocalBackend`])
/// gets *only* these (it may observe the world to drive UI/selection, but
/// can never mutate hashed state or advance the shared RNG).
pub(crate) fn register_world_read_api(engine: &mut Engine, world: &SharedWorld) {
    let w = world.clone();
    engine.register_fn("entity_position", move |e: i64| -> FixedVec3 {
        w.lock()
            .expect("world mutex")
            .position(EntityId(e as u64))
            .unwrap_or(FixedVec3::ZERO)
    });

    let w = world.clone();
    engine.register_fn(
        "entity_field",
        move |e: i64, name: ImmutableString| -> Fixed {
            w.lock()
                .expect("world mutex")
                .field(EntityId(e as u64), name.as_str())
                .unwrap_or(Fixed::ZERO)
        },
    );

    let w = world.clone();
    engine.register_fn("entities", move || -> Array {
        w.lock()
            .expect("world mutex")
            .all_entities()
            .into_iter()
            .map(|e| Dynamic::from(e.0 as i64))
            .collect()
    });

    // Ascending ids of one archetype (a coarse `entity_query`, §3.3):
    // lets a script scan just its pieces (board occupancy) or reach a
    // singleton, without walking `entities()` across every archetype.
    let w = world.clone();
    engine.register_fn("entities_of", move |arch: i64| -> Array {
        w.lock()
            .expect("world mutex")
            .entities(ArchetypeId(arch as u32))
            .iter()
            .map(|e| Dynamic::from(e.0 as i64))
            .collect()
    });
}

/// Register the host's render / input / command API (DESIGN.md §3.3),
/// each call forwarding to the shared [`HostBridge`](crate::HostBridge).
/// Kept separate from the sim host API because the *implementation* lives
/// in the host (roxlap render) while this crate knows only the primitive
/// signatures — the sim / script wall.
#[allow(clippy::too_many_lines)] // a flat list of host-fn registrations
pub(crate) fn register_bridge_api(engine: &mut Engine, bridge: &SharedBridge) {
    let b = bridge.clone();
    engine.register_fn(
        "model_box",
        move |w: i64, h: i64, d: i64, color: i64| -> i64 {
            b.lock().expect("bridge mutex").model_box(w, h, d, color)
        },
    );

    let b = bridge.clone();
    engine.register_fn(
        "model_kv6",
        move |path: ImmutableString, turns: i64| -> i64 {
            b.lock()
                .expect("bridge mutex")
                .model_kv6(path.as_str(), turns)
        },
    );

    let b = bridge.clone();
    engine.register_fn("entity_set_model", move |e: i64, model: i64| {
        b.lock().expect("bridge mutex").entity_set_model(e, model);
    });

    // Animated 8-direction billboard actor: `model_actor(dir, [states])`,
    // then per-entity `entity_set_anim` / `entity_set_facing` (render-side).
    let b = bridge.clone();
    engine.register_fn(
        "model_actor",
        move |path: ImmutableString, states: Array, height_cells: Fixed| -> i64 {
            let states: Vec<String> = states
                .into_iter()
                .map(|d| d.into_string().unwrap_or_default())
                .collect();
            b.lock()
                .expect("bridge mutex")
                .model_actor(path.as_str(), &states, height_cells)
        },
    );
    let b = bridge.clone();
    engine.register_fn("model_drop", move |model: i64, cells: Fixed| {
        b.lock().expect("bridge mutex").model_drop(model, cells);
    });

    let b = bridge.clone();
    engine.register_fn("entity_set_anim", move |e: i64, state: ImmutableString| {
        b.lock()
            .expect("bridge mutex")
            .entity_set_anim(e, state.as_str());
    });

    let b = bridge.clone();
    engine.register_fn("entity_set_facing", move |e: i64, yaw: Fixed| {
        b.lock().expect("bridge mutex").entity_set_facing(e, yaw);
    });
    let b = bridge.clone();
    engine.register_fn("entity_set_tint", move |e: i64, tint: i64| {
        b.lock().expect("bridge mutex").entity_set_tint(e, tint);
    });

    // Audio (render-side).
    let b = bridge.clone();
    engine.register_fn("play_sound", move |path: ImmutableString| {
        b.lock().expect("bridge mutex").play_sound(path.as_str());
    });
    let b = bridge.clone();
    engine.register_fn(
        "play_sound_gain",
        move |path: ImmutableString, gain: Fixed| {
            b.lock()
                .expect("bridge mutex")
                .play_sound_gain(path.as_str(), gain);
        },
    );
    let b = bridge.clone();
    engine.register_fn(
        "play_blip",
        move |wave: i64, freq: i64, dur_ms: i64, gain: Fixed| {
            b.lock()
                .expect("bridge mutex")
                .play_blip(wave, freq, dur_ms, gain);
        },
    );
    let b = bridge.clone();
    engine.register_fn("play_loop", move |path: ImmutableString| {
        b.lock().expect("bridge mutex").play_loop(path.as_str());
    });
    let b = bridge.clone();
    engine.register_fn("play_music", move |path: ImmutableString| {
        b.lock().expect("bridge mutex").play_music(path.as_str());
    });
    let b = bridge.clone();
    engine.register_fn("stop_music", move || {
        b.lock().expect("bridge mutex").stop_music();
    });

    let b = bridge.clone();
    engine.register_fn(
        "voxel_fill",
        move |x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64| {
            b.lock()
                .expect("bridge mutex")
                .voxel_fill(x0, y0, z0, x1, y1, z1, color);
        },
    );

    let b = bridge.clone();
    engine.register_fn("voxel_set", move |x: i64, y: i64, z: i64, color: i64| {
        b.lock().expect("bridge mutex").voxel_set(x, y, z, color);
    });

    let b = bridge.clone();
    engine.register_fn("highlight", move |e: i64| {
        b.lock().expect("bridge mutex").highlight(e);
    });

    let b = bridge.clone();
    engine.register_fn("highlight_clear", move || {
        b.lock().expect("bridge mutex").highlight_clear();
    });

    let b = bridge.clone();
    engine.register_fn("highlighted", move || -> i64 {
        b.lock().expect("bridge mutex").highlighted()
    });

    let b = bridge.clone();
    engine.register_fn("status", move |text: ImmutableString| {
        b.lock().expect("bridge mutex").status(text.as_str());
    });

    let b = bridge.clone();
    engine.register_fn("camera_focus", move |point: FixedVec3| {
        b.lock().expect("bridge mutex").camera_focus(point);
    });

    let b = bridge.clone();
    engine.register_fn("camera_angle", move |yaw: Fixed, pitch: Fixed| {
        b.lock().expect("bridge mutex").camera_angle(yaw, pitch);
    });

    let b = bridge.clone();
    engine.register_fn("camera_dist", move |dist: Fixed| {
        b.lock().expect("bridge mutex").camera_dist(dist);
    });

    let b = bridge.clone();
    engine.register_fn("camera_pan", move |dx: Fixed, dy: Fixed| {
        b.lock().expect("bridge mutex").camera_pan(dx, dy);
    });

    let b = bridge.clone();
    engine.register_fn("camera_cutout", move |radius: Fixed, feather: Fixed| {
        b.lock()
            .expect("bridge mutex")
            .camera_cutout(radius, feather);
    });

    let b = bridge.clone();
    engine.register_fn("deck_clip", move |z_lo: i64, z_hi: i64| {
        b.lock().expect("bridge mutex").deck_clip(z_lo, z_hi);
    });

    let b = bridge.clone();
    engine.register_fn(
        "submit_command",
        move |verb: i64, target: i64, arg: FixedVec3| {
            b.lock()
                .expect("bridge mutex")
                .submit_command(verb, target, arg);
        },
    );

    // The only place the script-side sentinel lives: `None` (no single
    // local player — hotseat) maps to a negative id a `no_float` Rhai
    // script can branch on. The host bridge stays in clean `Option`-land.
    let b = bridge.clone();
    engine.register_fn("local_player", move || -> i64 {
        b.lock()
            .expect("bridge mutex")
            .local_player()
            .unwrap_or(NO_LOCAL_PLAYER)
    });

    let b = bridge.clone();
    engine.register_fn("set_light", move |dir: FixedVec3, intensity: Fixed| {
        b.lock().expect("bridge mutex").set_light(dir, intensity);
    });

    let b = bridge.clone();
    engine.register_fn("set_sky", move |path: ImmutableString| {
        b.lock().expect("bridge mutex").set_sky(path.as_str());
    });

    // Terrain collision queries (sim coords). Deterministic: the store is a
    // pure function of the script's own paint calls, so every peer answers
    // identically — safe to feed hashed `tick()` decisions.
    let b = bridge.clone();
    engine.register_fn("voxel_solid", move |x: i64, y: i64, z: i64| -> bool {
        b.lock().expect("bridge mutex").voxel_solid(x, y, z)
    });

    let b = bridge.clone();
    engine.register_fn("ground_height", move |x: i64, y: i64| -> i64 {
        b.lock().expect("bridge mutex").ground_height(x, y)
    });

    // Deterministic navigation (docs/plans/rts-demo.md §1a): a budgeted
    // integer A* over the same store the collision queries read, plus an
    // explicit blocker overlay. Same determinism contract as above.
    let b = bridge.clone();
    engine.register_fn("nav_block", move |x: i64, y: i64, on: bool| {
        b.lock().expect("bridge mutex").nav_block(x, y, on);
    });

    let b = bridge.clone();
    engine.register_fn(
        "nav_path",
        move |x0: i64, y0: i64, x1: i64, y1: i64, max_step: i64| -> Array {
            b.lock()
                .expect("bridge mutex")
                .nav_path(x0, y0, x1, y1, max_step)
                .into_iter()
                .map(Dynamic::from)
                .collect()
        },
    );

    // Per-cell PNG tiles: `tile(path)` loads, `tile_fill` paints a cell region.
    let b = bridge.clone();
    engine.register_fn("tile", move |path: ImmutableString| -> i64 {
        b.lock().expect("bridge mutex").tile(path.as_str())
    });

    let b = bridge.clone();
    engine.register_fn(
        "tile_fill",
        move |x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, tile: i64| {
            b.lock()
                .expect("bridge mutex")
                .tile_fill(x0, y0, z0, x1, y1, z1, tile);
        },
    );

    // Autotiled terrain: register transition sheets, paint per-cell types,
    // then blit the blended flat floor.
    let b = bridge.clone();
    engine.register_fn(
        "transition",
        move |low: i64, high: i64, path: ImmutableString| {
            b.lock()
                .expect("bridge mutex")
                .transition(low, high, path.as_str());
        },
    );

    let b = bridge.clone();
    engine.register_fn(
        "terrain_fill",
        move |x0: i64, y0: i64, x1: i64, y1: i64, type_id: i64| {
            b.lock()
                .expect("bridge mutex")
                .terrain_fill(x0, y0, x1, y1, type_id);
        },
    );

    let b = bridge.clone();
    engine.register_fn("terrain_blit", move |base_type: i64| {
        b.lock().expect("bridge mutex").terrain_blit(base_type);
    });

    // HUD / UI overlay.
    let b = bridge.clone();
    engine.register_fn("ui_texture", move |path: ImmutableString| -> i64 {
        b.lock().expect("bridge mutex").ui_texture(path.as_str())
    });
    let b = bridge.clone();
    engine.register_fn("ui_gif", move |path: ImmutableString| -> i64 {
        b.lock().expect("bridge mutex").ui_gif(path.as_str())
    });
    let b = bridge.clone();
    engine.register_fn("ui_anim", move |gif: i64, x: i64, y: i64| {
        b.lock().expect("bridge mutex").ui_anim(gif, x, y);
    });
    let b = bridge.clone();
    engine.register_fn("ui_width", move || -> i64 {
        b.lock().expect("bridge mutex").ui_width()
    });
    let b = bridge.clone();
    engine.register_fn("ui_height", move || -> i64 {
        b.lock().expect("bridge mutex").ui_height()
    });
    let b = bridge.clone();
    engine.register_fn("ui_scale", move |factor: Fixed| {
        b.lock().expect("bridge mutex").ui_scale(factor);
    });
    let b = bridge.clone();
    engine.register_fn("ui_clear", move || {
        b.lock().expect("bridge mutex").ui_clear();
    });
    let b = bridge.clone();
    engine.register_fn("ui_image", move |tex: i64, x: i64, y: i64| {
        b.lock().expect("bridge mutex").ui_image(tex, x, y);
    });
    let b = bridge.clone();
    engine.register_fn(
        "ui_image_clip",
        move |tex: i64, x: i64, y: i64, frac: Fixed| {
            b.lock()
                .expect("bridge mutex")
                .ui_image_clip(tex, x, y, frac);
        },
    );
    let b = bridge.clone();
    engine.register_fn(
        "ui_text",
        move |x: i64, y: i64, text: ImmutableString, size: i64| {
            b.lock()
                .expect("bridge mutex")
                .ui_text(x, y, text.as_str(), size);
        },
    );
    let b = bridge.clone();
    engine.register_fn(
        "ui_text_wrap",
        move |x: i64, y: i64, text: ImmutableString, size: i64, width: i64, color: i64| {
            b.lock()
                .expect("bridge mutex")
                .ui_text_wrap(x, y, text.as_str(), size, width, color);
        },
    );
    let b = bridge.clone();
    engine.register_fn(
        "ui_button",
        move |tex: i64, hover: i64, pressed: i64, x: i64, y: i64, bit: i64| {
            b.lock()
                .expect("bridge mutex")
                .ui_button(tex, hover, pressed, x, y, bit);
        },
    );
}

/// The `local_player()` script sentinel for "no single local player".
const NO_LOCAL_PLAYER: i64 = -1;
