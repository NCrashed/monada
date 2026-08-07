//! [`LocalBackend`] — the map's **local, unsynchronized** script layer
//! (docs/plans/input-bindings.md §1).
//!
//! One instance runs per client, next to the deterministic sim backend
//! ([`RhaiBackend`](crate::RhaiBackend)), over the same map source (or a
//! dedicated `local_entry` script). It owns everything per-player: input
//! handling, selection gestures, camera and UI — and talks to the sim
//! **only** by queueing [`Command`](monada_sim::Command)s through
//! [`HostBridge::submit_command`](crate::HostBridge::submit_command),
//! which the host routes into the lockstep stream like any other input.
//!
//! The determinism wall is enforced by the registered API surface, not by
//! convention:
//!
//! - this engine registers the **read-only** world queries plus the full
//!   presentation bridge and the input queries (`action_*`, `pick_*`,
//!   `aim_yaw`, `ui_clicks`) — it physically cannot mutate hashed state
//!   or advance the shared RNG;
//! - the sim backend registers no input queries — it physically cannot
//!   observe the cursor or key state.
//!
//! Its global scope is separate from the sim backend's: helper *functions*
//! are shared through the common AST, but no variable state crosses the
//! wall.
//!
//! Entry points (all optional — a map defines what it needs):
//!
//! - `local_init()` — once after the sim's `init`; camera/UI setup.
//! - `local_frame(dt)` — once per **rendered frame**: hover highlights,
//!   tooltips, camera smoothing. This is the presentation cadence — it
//!   runs for turn-based maps too (which have no ticks between moves)
//!   and its rate is client-dependent, so anything it submits must not
//!   assume a frequency.
//! - `local_tick(dt)` — once per scheduled sim tick on this client;
//!   polls action state and assembles the per-tick input command
//!   (real-time maps). When defined, the host stops injecting its own
//!   legacy input snapshot — the map owns both ends of the encoding.
//! - `action(id, down)` — an edge event for a map-declared `[[action]]`
//!   (press = `true`, release = `false`).
//! - `pointer(button, point, entity)` — the click gesture, exactly as it
//!   was on the sim backend before the split (chess's select→move FSM
//!   runs here unchanged).

// Same intentional id/index casts as the sim backend's host-API glue.
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use monada_fixed::{Fixed, FixedVec3};
use rhai::{Dynamic, Engine, ImmutableString, Scope, AST};

use crate::grids::register_grid_read_api;
use crate::rhai_backend::{register_bridge_api, register_number_types, register_world_read_api};
use crate::{ScriptError, SharedBridge, SharedGrids, SharedWorld};

/// The map's local script layer: input → gesture/UI logic → submitted
/// commands. See the module docs for the API surface and entry points.
// The `has_*` handler-presence flags are independent booleans (same
// stance as `RhaiBackend`).
#[allow(clippy::struct_excessive_bools)]
pub struct LocalBackend {
    engine: Engine,
    ast: Option<AST>,
    scope: Scope<'static>,
    has_local_init: bool,
    has_local_frame: bool,
    has_local_tick: bool,
    has_action: bool,
    has_pointer: bool,
}

impl LocalBackend {
    /// Build a local layer over the shared `world` (read-only) and the
    /// host `bridge` (presentation + input queries + command submission).
    #[must_use]
    pub fn new(world: &SharedWorld, bridge: &SharedBridge) -> LocalBackend {
        let mut engine = Engine::new();
        // Same stance as the sim backend: map scripts are semi-trusted
        // assets; lift the conservative expression-depth limits.
        engine.set_max_expr_depths(0, 0);
        register_number_types(&mut engine);
        register_world_read_api(&mut engine, world);
        register_bridge_api(&mut engine, bridge);
        register_input_api(&mut engine, bridge);
        LocalBackend {
            engine,
            ast: None,
            scope: Scope::new(),
            has_local_init: false,
            has_local_frame: false,
            has_local_tick: false,
            has_action: false,
            has_pointer: false,
        }
    }

    /// Give the local layer read access to the sim's grid frames
    /// ([`RhaiBackend::grids`](crate::RhaiBackend::grids)): `grid_world` /
    /// `grid_local` / `entity_grid` / `grid_riders`, so a per-client script can
    /// turn a cursor hit into a hull cell. READ-only by construction — the
    /// mutators live in the sim layer, because moving a hull or re-seating an
    /// entity is shared, hashed-adjacent state, not a per-client decision.
    /// Registering after construction is fine (Rhai resolves at call time), the
    /// same as [`RhaiBackend::set_bridge`](crate::RhaiBackend::set_bridge).
    pub fn set_grids(&mut self, grids: &SharedGrids) {
        register_grid_read_api(&mut self.engine, grids);
    }

    /// Compile `source` (the map's entry script, or its dedicated
    /// `local_entry`). Handler presence is decided here, like the sim
    /// backend's `load`, so a missing handler is a clean no-op while a
    /// raise from an existing one always surfaces.
    ///
    /// # Errors
    /// [`ScriptError::Compile`] on a parse failure.
    pub fn load(&mut self, source: &str) -> Result<(), ScriptError> {
        let ast = self
            .engine
            .compile(source)
            .map_err(|e| ScriptError::Compile(e.to_string()))?;
        let defines = |name: &str, arity: usize| {
            ast.iter_functions()
                .any(|f| f.name == name && f.params.len() == arity)
        };
        self.has_local_init = defines("local_init", 0);
        self.has_local_frame = defines("local_frame", 1);
        self.has_local_tick = defines("local_tick", 1);
        self.has_action = defines("action", 2);
        self.has_pointer = defines("pointer", 3);
        self.ast = Some(ast);
        Ok(())
    }

    /// Whether the map assembles its own per-tick input command in
    /// `local_tick(dt)`. When `true`, the host must not inject its legacy
    /// input snapshot — the map owns the encoding end to end.
    #[must_use]
    pub fn has_local_tick(&self) -> bool {
        self.has_local_tick
    }

    /// Whether the map defines a `local_frame(dt)` handler (so the host
    /// can skip the per-frame call + drain entirely when it doesn't).
    #[must_use]
    pub fn has_local_frame(&self) -> bool {
        self.has_local_frame
    }

    /// Run `local_init()` (once, after the sim's `init`).
    ///
    /// # Errors
    /// [`ScriptError::Run`] if the handler raises.
    pub fn on_local_init(&mut self) -> Result<(), ScriptError> {
        if !self.has_local_init {
            return Ok(());
        }
        self.call("local_init", ())
    }

    /// Run `local_frame(dt)` for one rendered frame (hover / tooltips /
    /// camera; presentation cadence, client-dependent rate).
    ///
    /// # Errors
    /// [`ScriptError::Run`] if the handler raises.
    pub fn on_local_frame(&mut self, dt: Fixed) -> Result<(), ScriptError> {
        if !self.has_local_frame {
            return Ok(());
        }
        self.call("local_frame", (dt,))
    }

    /// Run `local_tick(dt)` for one scheduled sim tick on this client.
    ///
    /// # Errors
    /// [`ScriptError::Run`] if the handler raises.
    pub fn on_local_tick(&mut self, dt: Fixed) -> Result<(), ScriptError> {
        if !self.has_local_tick {
            return Ok(());
        }
        self.call("local_tick", (dt,))
    }

    /// Run `action(id, down)` for one edge of a map-declared action.
    ///
    /// # Errors
    /// [`ScriptError::Run`] if the handler raises.
    pub fn on_action(&mut self, id: &str, down: bool) -> Result<(), ScriptError> {
        if !self.has_action {
            return Ok(());
        }
        self.call("action", (ImmutableString::from(id), down))
    }

    /// Run `pointer(button, point, entity)` for a click gesture.
    ///
    /// # Errors
    /// [`ScriptError::Run`] if the handler raises.
    pub fn on_pointer(
        &mut self,
        button: i64,
        point: FixedVec3,
        entity: i64,
    ) -> Result<(), ScriptError> {
        if !self.has_pointer {
            return Ok(());
        }
        self.call("pointer", (button, point, entity))
    }

    fn call<A: rhai::FuncArgs>(&mut self, name: &str, args: A) -> Result<(), ScriptError> {
        let ast = self
            .ast
            .as_ref()
            .ok_or_else(|| ScriptError::Run("no local script loaded".to_string()))?;
        // `Dynamic`, not `()`: a Rhai function yields its last evaluated
        // statement's value even when the map wrote a semicolon, so a handler
        // ending in a value-returning verb (`entity_attach`, `submit_command`'s
        // neighbours, `ui_clicks`) would otherwise die with "Output type
        // incorrect: bool (expecting ())" — a message about nothing the author
        // did wrong. A trigger's return value is meaningless by design; drop it.
        self.engine
            .call_fn::<rhai::Dynamic>(&mut self.scope, ast, name, args)
            .map(|_| ())
            .map_err(|e| ScriptError::Run(e.to_string()))
    }
}

/// Register the local-layer input queries (the functions the sim backend
/// must never see). All results are sim types, ready for `Command`
/// payloads — the "no float leaks into the stream" invariant holds by
/// construction.
fn register_input_api(engine: &mut Engine, bridge: &SharedBridge) {
    let b = bridge.clone();
    engine.register_fn("action_down", move |id: ImmutableString| -> bool {
        b.lock().expect("bridge mutex").action_down(id.as_str())
    });

    let b = bridge.clone();
    engine.register_fn("action_axis", move |id: ImmutableString| -> i64 {
        b.lock().expect("bridge mutex").action_axis(id.as_str())
    });

    // `axis2` comes back as a `Vec3` (x, y, z = 0) so it can flow into a
    // command's `arg` without repacking.
    let b = bridge.clone();
    engine.register_fn("action_axis2", move |id: ImmutableString| -> FixedVec3 {
        let (x, y) = b.lock().expect("bridge mutex").action_axis2(id.as_str());
        FixedVec3::new(
            Fixed::from_int(x as i32),
            Fixed::from_int(y as i32),
            Fixed::ZERO,
        )
    });

    // `()` when the cursor ray misses — scripts branch with `hit != ()`.
    let b = bridge.clone();
    engine.register_fn("pick_ground", move || -> Dynamic {
        match b.lock().expect("bridge mutex").pick_ground() {
            Some(point) => Dynamic::from(point),
            None => Dynamic::UNIT,
        }
    });

    let b = bridge.clone();
    engine.register_fn("pick_entity", move || -> i64 {
        b.lock().expect("bridge mutex").pick_entity()
    });

    let b = bridge.clone();
    engine.register_fn("aim_yaw", move || -> Fixed {
        b.lock().expect("bridge mutex").aim_yaw()
    });

    let b = bridge.clone();
    engine.register_fn("ui_clicks", move || -> i64 {
        b.lock().expect("bridge mutex").ui_clicks()
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use monada_fixed::{Fixed, FixedVec3};

    use super::*;
    use crate::{shared_world, HostBridge, RhaiBackend, ScriptBackend};

    /// A headless bridge with a *functional* selection latch, command
    /// queue and canned input state — enough to exercise the local-layer
    /// loop (gesture FSM + per-tick input assembly) without a host.
    #[derive(Default)]
    struct TestBridge {
        highlighted: i64,
        pending: Vec<(i64, i64, FixedVec3)>,
        attack_down: bool,
        move_xy: (i64, i64),
        aim: Fixed,
        clicks: i64,
        under_cursor: i64,
    }

    impl TestBridge {
        fn new() -> TestBridge {
            TestBridge {
                highlighted: -1,
                under_cursor: -1,
                ..TestBridge::default()
            }
        }
    }

    impl HostBridge for TestBridge {
        fn model_box(&mut self, _w: i64, _h: i64, _d: i64, _color: i64) -> i64 {
            0
        }
        fn model_kv6(&mut self, _asset_path: &str, _turns: i64) -> i64 {
            0
        }
        fn entity_set_model(&mut self, _entity: i64, _model: i64) {}
        #[allow(clippy::too_many_arguments)]
        fn voxel_fill(
            &mut self,
            _x0: i64,
            _y0: i64,
            _z0: i64,
            _x1: i64,
            _y1: i64,
            _z1: i64,
            _c: i64,
        ) {
        }
        fn voxel_set(&mut self, _x: i64, _y: i64, _z: i64, _color: i64) {}
        fn highlight(&mut self, entity: i64) {
            self.highlighted = entity;
        }
        fn highlight_clear(&mut self) {
            self.highlighted = -1;
        }
        fn highlighted(&self) -> i64 {
            self.highlighted
        }
        fn status(&mut self, _text: &str) {}
        fn camera_focus(&mut self, _point: FixedVec3) {}
        fn camera_angle(&mut self, _yaw: Fixed, _pitch: Fixed) {}
        fn submit_command(&mut self, verb: i64, target: i64, arg: FixedVec3) {
            self.pending.push((verb, target, arg));
        }
        fn local_player(&self) -> Option<i64> {
            None
        }
        fn set_light(&mut self, _dir: FixedVec3, _intensity: Fixed) {}
        fn set_sky(&mut self, _asset_path: &str) {}
        fn action_down(&self, id: &str) -> bool {
            id == "attack" && self.attack_down
        }
        fn action_axis2(&self, id: &str) -> (i64, i64) {
            if id == "move" {
                self.move_xy
            } else {
                (0, 0)
            }
        }
        fn aim_yaw(&self) -> Fixed {
            self.aim
        }
        fn ui_clicks(&mut self) -> i64 {
            std::mem::take(&mut self.clicks)
        }
        fn pick_entity(&self) -> i64 {
            self.under_cursor
        }
    }

    /// A chess-shaped two-click gesture: select on the first click,
    /// submit the move on the second. Shares source with a sim `init`
    /// that spawns the piece — the same one-file layout real maps use.
    const GESTURE_MAP: &str = r"
        fn init() {
            let a = archetype([]);
            let p = entity_create(a);
            entity_set_position(p, vec3(fixed(1), fixed(1), fixed(0)));
        }
        fn pointer(button, point, entity) {
            if button != 0 { return; }
            let sel = highlighted();
            if sel == -1 {
                if entity != -1 { highlight(entity); }
            } else {
                highlight_clear();
                submit_command(1, sel, point);
            }
        }
    ";

    #[test]
    fn pointer_gesture_selects_then_submits() {
        let world = shared_world(1);
        let concrete = Arc::new(Mutex::new(TestBridge::new()));
        let bridge: crate::SharedBridge = concrete.clone();
        let mut sim = RhaiBackend::new(world.clone());
        sim.set_bridge(&bridge);
        sim.load(GESTURE_MAP).unwrap();
        sim.on_init().unwrap();

        let mut local = LocalBackend::new(&world, &bridge);
        local.load(GESTURE_MAP).unwrap();
        assert!(!local.has_local_tick());

        let at = FixedVec3::new(Fixed::from_int(1), Fixed::from_int(1), Fixed::ZERO);
        // First click: selection latches on the bridge, nothing submitted.
        local.on_pointer(0, at, 0).unwrap();
        {
            let b = concrete.lock().unwrap();
            assert_eq!(b.highlighted(), 0);
        }
        // Second click: the move command is queued, selection cleared.
        let dest = FixedVec3::new(Fixed::from_int(3), Fixed::from_int(4), Fixed::ZERO);
        local.on_pointer(0, dest, -1).unwrap();
        let b = concrete.lock().unwrap();
        assert_eq!(b.highlighted(), -1);
        assert_eq!(b.pending.as_slice(), &[(1, 0, dest)]);
    }

    const INPUT_MAP: &str = r#"
        fn local_tick(dt) {
            let btn = ui_clicks();
            if action_down("attack") { btn |= 1; }
            let mv = action_axis2("move");
            submit_command(0, btn, vec3(mv.x, mv.y, aim_yaw()));
        }
    "#;

    #[test]
    fn local_tick_assembles_the_input_command() {
        let world = shared_world(1);
        let concrete = Arc::new(Mutex::new(TestBridge::new()));
        let bridge: crate::SharedBridge = concrete.clone();
        {
            let mut b = concrete.lock().unwrap();
            b.attack_down = true;
            b.move_xy = (1, -1);
            b.aim = Fixed::from_ratio(3, 2);
            b.clicks = 16;
        }
        let mut local = LocalBackend::new(&world, &bridge);
        local.load(INPUT_MAP).unwrap();
        assert!(local.has_local_tick());
        local.on_local_tick(Fixed::from_ratio(1, 30)).unwrap();

        let b = concrete.lock().unwrap();
        let (verb, buttons, arg) = b.pending[0];
        assert_eq!(verb, 0);
        assert_eq!(buttons, 16 | 1, "HUD click bit OR-ed with held attack");
        assert_eq!(arg.x, Fixed::from_int(1));
        assert_eq!(arg.y, Fixed::from_int(-1));
        assert_eq!(arg.z, Fixed::from_ratio(3, 2));
        // The click latch is take-and-clear.
        drop(b);
        concrete.lock().unwrap().pending.clear();
        local.on_local_tick(Fixed::from_ratio(1, 30)).unwrap();
        let b = concrete.lock().unwrap();
        assert_eq!(b.pending[0].1, 1, "clicks consumed, attack still held");
    }

    /// The hover loop: `local_frame` polls `pick_entity` each rendered
    /// frame and drives the local highlight — the WC3 invisible-unit-
    /// sphere use case as three lines of map script.
    #[test]
    fn local_frame_drives_hover_highlight() {
        let world = shared_world(1);
        let concrete = Arc::new(Mutex::new(TestBridge::new()));
        let bridge: crate::SharedBridge = concrete.clone();
        let mut local = LocalBackend::new(&world, &bridge);
        local
            .load(
                "fn local_frame(dt) {
                    let e = pick_entity();
                    if e != -1 { highlight(e); } else { highlight_clear(); }
                 }",
            )
            .unwrap();
        assert!(local.has_local_frame());

        concrete.lock().unwrap().under_cursor = 7;
        local.on_local_frame(Fixed::from_ratio(1, 60)).unwrap();
        assert_eq!(concrete.lock().unwrap().highlighted(), 7);

        concrete.lock().unwrap().under_cursor = -1;
        local.on_local_frame(Fixed::from_ratio(1, 60)).unwrap();
        assert_eq!(concrete.lock().unwrap().highlighted(), -1);
    }

    /// The wall itself: the local engine must not expose world mutators
    /// or the shared RNG — calling one is a runtime error, not a silent
    /// desync.
    #[test]
    fn local_layer_cannot_mutate_the_world() {
        let world = shared_world(1);
        let concrete = Arc::new(Mutex::new(TestBridge::new()));
        let bridge: crate::SharedBridge = concrete.clone();
        let mut local = LocalBackend::new(&world, &bridge);
        local
            .load("fn pointer(b, p, e) { entity_create(0); }")
            .unwrap();
        let err = local.on_pointer(0, FixedVec3::ZERO, -1).unwrap_err();
        assert!(
            format!("{err:?}").contains("entity_create"),
            "mutator must be unregistered: {err:?}"
        );
        local.load("fn pointer(b, p, e) { rng01(); }").unwrap();
        assert!(local.on_pointer(0, FixedVec3::ZERO, -1).is_err());
    }
}
