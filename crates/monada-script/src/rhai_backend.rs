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

use monada_fixed::{trig, Fixed, FixedQuat, FixedVec3};
use monada_physics::BodyId;
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};
use rhai::{Array, Dynamic, Engine, ImmutableString, Scope, AST};

use crate::grids::{register_grid_api, shared_grids};
use crate::physics::{pose_bound_grid, register_physics_api};
use crate::{
    shared_terrain, ScriptBackend, ScriptError, SharedBridge, SharedGrids, SharedPhysics,
    SharedTerrain, SharedWorld, UiEvent,
};

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
    /// The bridge handle [`set_bridge`](RhaiBackend::set_bridge) registered,
    /// kept so [`set_physics`](RhaiBackend::set_physics) can dual-write the
    /// volume-routed terrain verbs (store + render).
    bridge: Option<SharedBridge>,
    /// The deterministic frame table behind the `grid_*` verbs
    /// (docs/plans/grid-entities.md). Always present — a grid's frame is sim
    /// truth, so it must answer identically on a headless peer and a rendering
    /// one — and handed to the host via [`grids`](RhaiBackend::grids) so the
    /// local layer can read it too.
    grids: SharedGrids,
    /// The column terrain store the paint verbs write and the collision
    /// queries read. Owned HERE, not by the bridge, so a headless peer and
    /// a drawing one answer "can I walk there?" identically
    /// (docs/plans/desert-game.md §3a).
    terrain: SharedTerrain,
    /// The embedded physics sim, once [`set_physics`](RhaiBackend::set_physics)
    /// has run — kept so [`sync_grid_bodies`](ScriptBackend::sync_grid_bodies)
    /// can read body poses after a step. `None` on a map without physics, where
    /// that sync is a no-op.
    phys: Option<SharedPhysics>,
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
        set_call_depth(&mut engine);
        let events: UiEventBuffer = Arc::new(Mutex::new(Vec::new()));
        register_number_types(&mut engine);
        register_host_api(&mut engine, &world, &events);
        // The grid verbs work against the frame table alone until a bridge
        // arrives (`set_bridge` re-registers them dual-writing), so a bridgeless
        // headless backend still spawns grids and answers `grid_world`.
        let grids = shared_grids();
        register_grid_api(&mut engine, &grids, &world, None);
        let terrain = shared_terrain();
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
            bridge: None,
            grids,
            terrain,
            phys: None,
        }
    }

    /// The frame table the `grid_*` verbs write, for the host's render mirror
    /// and for handing to the local layer
    /// ([`LocalBackend::set_grids`](crate::LocalBackend::set_grids)).
    #[must_use]
    pub fn grids(&self) -> &SharedGrids {
        &self.grids
    }

    /// The column terrain store this backend paints and queries, for the
    /// host to hand to the local layer
    /// ([`LocalBackend::set_terrain`](crate::LocalBackend::set_terrain))
    /// so both layers read one ground.
    #[must_use]
    pub fn terrain(&self) -> &SharedTerrain {
        &self.terrain
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
        // Terrain AFTER the render bridge: the paint verbs here write the
        // runtime store and then draw, so they must shadow any the bridge
        // registration left behind.
        register_terrain_api(&mut self.engine, bridge, &self.terrain.clone());
        // AFTER the bridge API, whose own `grid_spawn` / `grid_orient` /
        // `grid_pivot` / `entity_set_grid` would otherwise shadow these: the
        // frame table is the authority, the renderer its mirror.
        let (grids, world) = (self.grids.clone(), self.world.clone());
        register_grid_api(&mut self.engine, &grids, &world, Some(bridge));
        self.bridge = Some(bridge.clone());
    }

    /// Register the sim-physics host API (`phys_*`, docs/plans/digger-demo.md
    /// §1c) and re-route the terrain paint verbs through the shared volume
    /// store. Call **once, before** [`on_init`](ScriptBackend::on_init) and
    /// **after** [`set_bridge`](RhaiBackend::set_bridge) (the volume-routed
    /// `voxel_*` registrations shadow the bridge-only ones and forward to
    /// whatever bridge is set at this moment).
    pub fn set_physics(&mut self, phys: &SharedPhysics) {
        register_physics_api(
            &mut self.engine,
            phys,
            self.bridge.as_ref(),
            &self.grids.clone(),
        );
        // Kept so [`sync_grid_bodies`](ScriptBackend::sync_grid_bodies) can
        // carry body poses into the frame table after each step — the one
        // place the two halves of a map's simulation meet.
        self.phys = Some(phys.clone());
    }

    fn call<A: rhai::FuncArgs>(&mut self, name: &str, args: A) -> Result<(), ScriptError> {
        let ast = self
            .ast
            .as_ref()
            .ok_or_else(|| ScriptError::Run("no script loaded".to_string()))?;
        // `Dynamic`, not `()`: a Rhai function yields its last evaluated
        // statement's value even when the map wrote a semicolon, so an `init`
        // ending in a value-returning verb (`entity_create`, `entity_despawn`,
        // `entity_attach`) died with "Output type incorrect: i64 (expecting
        // ())" — a message about nothing the author did wrong. A trigger's
        // return value is meaningless by design; drop it.
        self.engine
            .call_fn::<rhai::Dynamic>(&mut self.scope, ast, name, args)
            .map(|_| ())
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
            .call_fn::<rhai::Dynamic>(&mut self.scope, ast, "command", args)
            .map(|_| ())
            .map_err(|e| ScriptError::Run(e.to_string()))
    }

    fn on_tick(&mut self) -> Result<(), ScriptError> {
        // The driver owns the tick counter; the script only mutates entity
        // state via the host API. A command-driven map (no `tick` handler)
        // still advances the counter — it just runs no per-tick logic.
        self.world.lock().expect("world mutex").tick += 1;
        let ran = if self.has_tick_with_dt {
            let dt = self
                .tick_dt
                .expect("tick(dt) handler requires set_tick_hz before on_tick");
            self.call("tick", (dt,))
        } else if self.has_tick {
            self.call("tick", ())
        } else {
            Ok(())
        };
        // Retire grid bindings whose entity the tick despawned. Once per tick,
        // here rather than in the store, because this is the one place that
        // holds both the world and the frame table (the host does the same for
        // its render-side bindings in `build_instances`).
        {
            let world = self.world.lock().expect("world mutex");
            self.grids.lock().expect("grids mutex").retain(&world);
        }
        ran
    }

    fn drain_ui_events(&mut self) -> Vec<UiEvent> {
        std::mem::take(&mut self.events.lock().expect("events mutex"))
    }

    fn sync_grid_bodies(&mut self) {
        let Some(phys) = self.phys.clone() else {
            return;
        };
        let bound = self.grids.lock().expect("grids mutex").bound_grids();
        if bound.is_empty() {
            return;
        }
        // Read every pose under ONE physics lock, then write: the write path
        // takes the frame-table and bridge locks, and interleaving the three
        // would nest them in an order nothing else uses.
        let poses: Vec<(i64, FixedVec3, FixedQuat)> = {
            let sim = phys.lock().expect("physics mutex");
            bound
                .iter()
                .filter_map(|&(grid, body)| {
                    sim.world
                        .body(BodyId(body))
                        .map(|b| (grid, b.position(), b.orientation()))
                })
                .collect()
        };
        for (grid, position, orientation) in poses {
            pose_bound_grid(
                &self.grids,
                self.bridge.as_ref(),
                grid,
                position,
                orientation,
            );
        }
    }
}

/// Pin how deep a map's own functions may call, EXPLICITLY — because
/// Rhai's default depends on the build profile: 64 levels in release, **8
/// in debug**.
///
/// That asymmetry is not a tuning knob, it is a divergence. A map whose
/// rules nest ten calls deep (the ship's `tick → step_crew → try_move →
/// reachable → blocked → occupied → prop_covers → …` is exactly that)
/// runs on a release peer and raises "Stack overflow" on a debug one —
/// mid-tick, on one side of a lockstep session only, with an error naming
/// no function at all. A limit that changes what a script *means*
/// between two builds of the same engine cannot be left implicit.
///
/// 64 is the release default, kept as a real guard: runaway recursion
/// should still stop, it just must stop identically everywhere.
pub(crate) fn set_call_depth(engine: &mut Engine) {
    engine.set_max_call_levels(64);
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
    engine.register_fn("!=", |a: Fixed, b: Fixed| a != b);
    engine.register_fn("<", |a: Fixed, b: Fixed| a < b);
    engine.register_fn(">", |a: Fixed, b: Fixed| a > b);
    engine.register_fn("<=", |a: Fixed, b: Fixed| a <= b);
    engine.register_fn(">=", |a: Fixed, b: Fixed| a >= b);

    // Fixed-point trig + the turn constant (the circle scenario's only
    // transcendentals). atan2 joined for angle work a map can't fake
    // with sin/cos alone — wrapping an angle difference for a smooth
    // chase-cam lerp (digger D4).
    engine.register_fn("sin", trig::sin);
    engine.register_fn("cos", trig::cos);
    engine.register_fn("atan2", trig::atan2);
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

/// Register the world-painting verbs and the collision queries over the
/// **runtime's** terrain store (docs/plans/desert-game.md §3a).
///
/// This is the seam that used to run through the bridge, and the reason
/// it moved: what a map may walk on is simulation state, so a headless
/// peer has to answer it identically to a drawing one. It used to be
/// answered by whichever `HostBridge` happened to be installed — the
/// host's `MapRender` when rendering, a store-keeping `TerrainBridge`
/// when not — which
/// is two implementations of one deterministic fact, on the wrong side of
/// the wall.
///
/// Now each paint writes the store first and then hands the same call to
/// the bridge to draw. Colour is presentation; solidity is not.
///
/// Ordering: register **after** [`register_bridge_api`], and before
/// [`register_physics_api`](crate::physics::register_physics_api), whose
/// volume-map overloads deliberately shadow the paint verbs here (a
/// volume map's terrain lives in the hashed `VolumeStore` instead).
pub(crate) fn register_terrain_api(
    engine: &mut Engine,
    bridge: &SharedBridge,
    terrain: &SharedTerrain,
) {
    let (t, b) = (terrain.clone(), bridge.clone());
    engine.register_fn(
        "voxel_fill",
        move |x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64| {
            t.lock()
                .expect("terrain mutex")
                .fill(x0, y0, z0, x1, y1, z1);
            b.lock()
                .expect("bridge mutex")
                .voxel_fill(x0, y0, z0, x1, y1, z1, color);
        },
    );

    let (t, b) = (terrain.clone(), bridge.clone());
    engine.register_fn("voxel_set", move |x: i64, y: i64, z: i64, color: i64| {
        t.lock().expect("terrain mutex").set(x, y, z);
        b.lock().expect("bridge mutex").voxel_set(x, y, z, color);
    });

    let (t, b) = (terrain.clone(), bridge.clone());
    engine.register_fn("voxel_clear", move |x: i64, y: i64, z: i64| {
        t.lock().expect("terrain mutex").clear_above(x, y, z);
        b.lock().expect("bridge mutex").voxel_clear(x, y, z);
    });

    // A tile paint is a voxel paint with a texture: the store must see it
    // too, or a tiled floor would be walkable on screen and thin air to
    // the simulation.
    let (t, b) = (terrain.clone(), bridge.clone());
    engine.register_fn(
        "tile_fill",
        move |x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, tile: i64| {
            t.lock()
                .expect("terrain mutex")
                .fill(x0, y0, z0, x1, y1, z1);
            b.lock()
                .expect("bridge mutex")
                .tile_fill(x0, y0, z0, x1, y1, z1, tile);
        },
    );

    // Ambient occlusion, baked once the terrain is down. Under the light
    // rig this byte IS the ambient fill, so without it terrain meeting
    // terrain has nothing to mark it.
    let b = bridge.clone();
    engine.register_fn("bake_ao", move |strength: i64, radius: i64| {
        b.lock().expect("bridge mutex").bake_ao(strength, radius);
    });

    let b = bridge.clone();
    engine.register_fn(
        "bake_ao_in",
        move |x0: i64, y0: i64, x1: i64, y1: i64, strength: i64, radius: i64| {
            b.lock()
                .expect("bridge mutex")
                .bake_ao_in((x0, y0), (x1, y1), strength, radius);
        },
    );

    // One cell whose surface is not flat. The feet still get a cell; only
    // the eye gets the relief.
    let b = bridge.clone();
    engine.register_fn("cell_voxels", move || -> i64 {
        b.lock().expect("bridge mutex").cell_voxels()
    });

    let b = bridge.clone();
    let t = terrain.clone();
    engine.register_fn(
        "tile_relief",
        move |x: i64, y: i64, floor: i64, walkable: i64, tops: Array, tile: i64| {
            let tops: Vec<i64> = tops.into_iter().map(|v| v.as_int().unwrap_or(0)).collect();
            t.lock()
                .expect("terrain mutex")
                .fill(x, y, floor, x, y, walkable);
            b.lock()
                .expect("bridge mutex")
                .tile_relief(x, y, floor, walkable, &tops, tile);
        },
    );

    let b = bridge.clone();
    let t = terrain.clone();
    engine.register_fn(
        "tile_relief_mixed",
        move |x: i64, y: i64, floor: i64, walkable: i64, tops: Array, tiles: Array| {
            let ints = |a: Array| -> Vec<i64> {
                a.into_iter().map(|v| v.as_int().unwrap_or(0)).collect()
            };
            let (tops, tiles) = (ints(tops), ints(tiles));
            t.lock()
                .expect("terrain mutex")
                .fill(x, y, floor, x, y, walkable);
            b.lock()
                .expect("bridge mutex")
                .tile_relief_mixed(x, y, floor, walkable, &tops, &tiles);
        },
    );

    register_terrain_queries(engine, terrain);
}

/// The read half of the terrain API: what the store can be asked, as opposed
/// to what it can be told. Split out of [`register_terrain_api`] for length
/// alone -- the two halves are registered together and always have been.
fn register_terrain_queries(engine: &mut Engine, terrain: &SharedTerrain) {
    // Collision queries (sim coords). Deterministic: the store is a pure
    // function of the map's own paint calls, so every peer answers
    // identically — safe to feed hashed `tick()` decisions.
    let t = terrain.clone();
    engine.register_fn("voxel_solid", move |x: i64, y: i64, z: i64| -> bool {
        t.lock().expect("terrain mutex").solid(x, y, z)
    });

    let t = terrain.clone();
    engine.register_fn("ground_height", move |x: i64, y: i64| -> i64 {
        t.lock().expect("terrain mutex").ground_height(x, y)
    });

    // Deterministic navigation (docs/plans/rts-demo.md §1a): a budgeted
    // integer A* over the same store the collision queries read, plus an
    // explicit blocker overlay. Same determinism contract as above.
    let t = terrain.clone();
    engine.register_fn("nav_block", move |x: i64, y: i64, on: bool| {
        t.lock().expect("terrain mutex").nav_block(x, y, on);
    });

    let t = terrain.clone();
    engine.register_fn(
        "nav_path",
        move |x0: i64, y0: i64, x1: i64, y1: i64, max_step: i64| -> Array {
            t.lock()
                .expect("terrain mutex")
                .nav_path(x0, y0, x1, y1, max_step)
                .into_iter()
                .map(Dynamic::from)
                .collect()
        },
    );
    let t = terrain.clone();
    engine.register_fn(
        "nav_path_drop",
        move |x0: i64, y0: i64, x1: i64, y1: i64, max_step: i64, max_drop: i64| -> Array {
            t.lock()
                .expect("terrain mutex")
                .nav_path_drop(x0, y0, x1, y1, max_step, max_drop)
                .into_iter()
                .map(Dynamic::from)
                .collect()
        },
    );
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
        "model_box_sides",
        #[allow(clippy::too_many_arguments)]
        move |w: i64,
              h: i64,
              d: i64,
              x: i64,
              neg_x: i64,
              y: i64,
              neg_y: i64,
              z: i64,
              neg_z: i64|
              -> i64 {
            b.lock()
                .expect("bridge mutex")
                .model_box_sides(w, h, d, x, neg_x, y, neg_y, z, neg_z)
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

    let b = bridge.clone();
    engine.register_fn("entity_set_grid", move |e: i64, grid: i64| {
        b.lock().expect("bridge mutex").entity_set_grid(e, grid);
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
    // Rigged `.rkc` character: `model_character(path, height_cells)`, then the
    // same per-entity `entity_set_anim` (by clip name) / `entity_set_facing`.
    let b = bridge.clone();
    engine.register_fn(
        "model_character",
        move |path: ImmutableString, height_cells: Fixed| -> i64 {
            b.lock()
                .expect("bridge mutex")
                .model_character(path.as_str(), height_cells)
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
    engine.register_fn("entity_set_side", move |e: i64, dir: i64, roll: i64| {
        b.lock()
            .expect("bridge mutex")
            .entity_set_side(e, dir, roll);
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
    engine.register_fn("grid_spawn", move |wx: i64, wy: i64, wz: i64| -> i64 {
        b.lock().expect("bridge mutex").grid_spawn(wx, wy, wz)
    });

    // The cubic-cell twin (host_api 15): same handle space, a cell is a cube.
    let b = bridge.clone();
    engine.register_fn(
        "grid_spawn_cubic",
        move |wx: i64, wy: i64, wz: i64| -> i64 {
            b.lock().expect("bridge mutex").grid_spawn_cubic(wx, wy, wz)
        },
    );

    let b = bridge.clone();
    engine.register_fn(
        "voxel_fill_in",
        move |grid: i64, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64| {
            b.lock()
                .expect("bridge mutex")
                .voxel_fill_in(grid, x0, y0, z0, x1, y1, z1, color);
        },
    );

    let b = bridge.clone();
    engine.register_fn(
        "grid_orient",
        move |grid: i64, axis: FixedVec3, angle: Fixed| {
            b.lock()
                .expect("bridge mutex")
                .grid_orient(grid, axis, angle);
        },
    );

    let b = bridge.clone();
    engine.register_fn("grid_pivot", move |grid: i64, point: FixedVec3| {
        b.lock().expect("bridge mutex").grid_pivot(grid, point);
    });

    let b = bridge.clone();
    engine.register_fn("highlight", move |e: i64| {
        b.lock().expect("bridge mutex").highlight(e);
    });

    let b = bridge.clone();
    engine.register_fn("highlight_add", move |e: i64| {
        b.lock().expect("bridge mutex").highlight_add(e);
    });

    let b = bridge.clone();
    engine.register_fn("highlighted_all", move || -> Array {
        b.lock()
            .expect("bridge mutex")
            .highlighted_all()
            .into_iter()
            .map(Dynamic::from)
            .collect()
    });

    let b = bridge.clone();
    engine.register_fn("drag_begin", move || {
        b.lock().expect("bridge mutex").drag_begin();
    });

    let b = bridge.clone();
    engine.register_fn("drag_end", move || -> Array {
        b.lock()
            .expect("bridge mutex")
            .drag_end()
            .into_iter()
            .map(Dynamic::from)
            .collect()
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
    engine.register_fn(
        "camera_focus_entity",
        move |entity: i64, point: FixedVec3| {
            b.lock()
                .expect("bridge mutex")
                .camera_focus_entity(entity, point);
        },
    );

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
    engine.register_fn("vision_observer", move |entity: i64| {
        b.lock().expect("bridge mutex").vision_observer(entity);
    });

    // 2-arg overload (host_api 7): fog rides the named grid_spawn grid.
    let b = bridge.clone();
    engine.register_fn("vision_observer", move |entity: i64, grid: i64| {
        b.lock()
            .expect("bridge mutex")
            .vision_observer_in(entity, grid);
    });

    // A party rather than a protagonist: `vision_observer` sets the list,
    // these two grow and empty it.
    let b = bridge.clone();
    engine.register_fn("vision_observer_add", move |entity: i64| {
        b.lock().expect("bridge mutex").vision_observer_add(entity);
    });

    let b = bridge.clone();
    engine.register_fn("vision_observer_clear", move || {
        b.lock().expect("bridge mutex").vision_observer_clear();
    });

    // Unexplored ground opaque rather than transparent — what an outdoor
    // map wants, where transparent shows the sky through the ground.
    let b = bridge.clone();
    engine.register_fn("vision_shroud", move |opaque: bool| {
        b.lock().expect("bridge mutex").vision_shroud(opaque);
    });

    let b = bridge.clone();
    engine.register_fn(
        "vision_config",
        move |cone_deg: i64, range: i64, peripheral: i64| {
            b.lock()
                .expect("bridge mutex")
                .vision_config(cone_deg, range, peripheral);
        },
    );

    let b = bridge.clone();
    engine.register_fn(
        "vision_hear",
        move |x: i64, y: i64, z: i64, loudness: Fixed| {
            b.lock()
                .expect("bridge mutex")
                .vision_hear(x, y, z, loudness);
        },
    );

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

    // Real cast shadows from the sun. A column map takes per-face shading
    // until it asks for these, so every existing map keeps its look.
    let b = bridge.clone();
    engine.register_fn("set_shadows", move |strength: Fixed| {
        b.lock().expect("bridge mutex").set_shadows(strength);
    });

    // View-plane billboards. Off by default, so a map drawn against the
    // eye-facing look keeps it.
    let b = bridge.clone();
    engine.register_fn("set_sprite_facing", move |view_plane: bool| {
        b.lock()
            .expect("bridge mutex")
            .set_sprite_facing(view_plane);
    });

    // Horizontal field of view. Narrow it and pull the camera back the
    // same factor for a near-orthographic look at unchanged framing.
    let b = bridge.clone();
    engine.register_fn("camera_fov", move |degrees: Fixed| {
        b.lock().expect("bridge mutex").camera_fov(degrees);
    });

    let b = bridge.clone();
    engine.register_fn("set_sky", move |path: ImmutableString| {
        b.lock().expect("bridge mutex").set_sky(path.as_str());
    });

    // The flat background behind the sky. Black is what a fogged outdoor
    // map wants — see the verb's docs.
    let b = bridge.clone();
    engine.register_fn("set_sky_color", move |color: i64| {
        b.lock().expect("bridge mutex").set_sky_color(color);
    });

    // Per-cell PNG tiles: `tile(path)` loads; `tile_fill` paints and is
    // registered with the terrain verbs, because what it paints is solid.
    let b = bridge.clone();
    engine.register_fn("tile", move |path: ImmutableString| -> i64 {
        b.lock().expect("bridge mutex").tile(path.as_str())
    });

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
        "ui_mark",
        move |tex: i64, x: i64, y: i64, tint: i64, turn: Fixed| {
            b.lock().expect("bridge mutex").ui_mark(tex, x, y, tint, turn);
        },
    );
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
        "ui_text_tint",
        move |x: i64, y: i64, text: ImmutableString, size: i64, tint: i64| {
            b.lock()
                .expect("bridge mutex")
                .ui_text_tint(x, y, text.as_str(), size, tint);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare engine with just the number types — the same registration
    /// `RhaiBackend::new` applies.
    fn engine() -> Engine {
        let mut e = Engine::new();
        register_number_types(&mut e);
        e
    }

    /// Every ordering / equality operator a map may write on `Fixed` resolves.
    /// Regression: `>=`/`<=` were unregistered, so a script using them (the RTS
    /// `box_select` bounds test) panicked at run time with
    /// `Function not found: >= (Fixed, Fixed)`.
    #[test]
    fn fixed_comparison_operators_resolve() {
        let e = engine();
        for (expr, want) in [
            ("fixed(3) >= fixed(3)", true),
            ("fixed(3) >= fixed(2)", true),
            ("fixed(1) >= fixed(2)", false),
            ("fixed(2) <= fixed(2)", true),
            ("fixed(3) <= fixed(2)", false),
            ("fixed(1) <= fixed(2)", true),
            ("fixed(3) != fixed(2)", true),
            ("fixed(2) != fixed(2)", false),
            ("fixed(2) == fixed(2)", true),
            ("fixed(2) < fixed(3)", true),
            ("fixed(3) > fixed(2)", true),
        ] {
            assert_eq!(
                e.eval::<bool>(expr)
                    .unwrap_or_else(|err| panic!("{expr}: {err}")),
                want,
                "{expr}"
            );
        }
    }
}
