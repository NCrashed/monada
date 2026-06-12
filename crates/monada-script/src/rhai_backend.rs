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

use monada_fixed::{trig, Fixed, FixedVec3};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};
use rhai::{Array, Dynamic, Engine, ImmutableString, Scope, AST};

use crate::{ScriptBackend, ScriptError, SharedWorld};

/// Arity of the map's `command` trigger: `command(player, verb, target, arg)`.
const COMMAND_ARITY: usize = 4;

/// Rhai-backed scripting runtime over a shared [`World`].
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
}

impl RhaiBackend {
    /// Build a backend bound to `world`, with the sim number types and
    /// host API registered.
    #[must_use]
    pub fn new(world: SharedWorld) -> RhaiBackend {
        let mut engine = Engine::new();
        register_number_types(&mut engine);
        register_host_api(&mut engine, &world);
        RhaiBackend {
            engine,
            ast: None,
            scope: Scope::new(),
            world,
            has_command: false,
        }
    }

    fn call(&mut self, name: &str) -> Result<(), ScriptError> {
        let ast = self
            .ast
            .as_ref()
            .ok_or_else(|| ScriptError::Run("no script loaded".to_string()))?;
        self.engine
            .call_fn::<()>(&mut self.scope, ast, name, ())
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
        self.has_command = ast
            .iter_functions()
            .any(|f| f.name == "command" && f.params.len() == COMMAND_ARITY);
        self.ast = Some(ast);
        Ok(())
    }

    fn on_init(&mut self) -> Result<(), ScriptError> {
        self.call("init")
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
        // The driver owns the tick counter; the script only mutates
        // entity state via the host API.
        self.world.lock().expect("world mutex").tick += 1;
        self.call("tick")
    }
}

/// Register `Fixed` / `Vec3` and the only arithmetic scripts get (all
/// fixed-point — `no_float` Rhai forbids IEEE math entirely).
fn register_number_types(engine: &mut Engine) {
    engine.register_type_with_name::<Fixed>("Fixed");
    engine.register_type_with_name::<FixedVec3>("Vec3");

    // Constructors.
    engine.register_fn("fixed", |i: i64| Fixed::from_int(i as i32));
    engine.register_fn("ratio", |n: i64, d: i64| {
        Fixed::from_ratio(n as i32, d as i32)
    });
    engine.register_fn("vec3", FixedVec3::new);

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
    engine.register_fn("to_debug", |a: Fixed| format!("{a:?}"));
}

/// Register the host API (DESIGN.md §3.3). Each function locks the shared
/// world for the call; the sim is single-threaded so the lock is
/// uncontended.
fn register_host_api(engine: &mut Engine, world: &SharedWorld) {
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
    engine.register_fn("entity_position", move |e: i64| -> FixedVec3 {
        w.lock()
            .expect("world mutex")
            .position(EntityId(e as u64))
            .unwrap_or(FixedVec3::ZERO)
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

    let w = world.clone();
    engine.register_fn("rng01", move || -> Fixed {
        w.lock().expect("world mutex").rng.next_fixed_01()
    });

    let w = world.clone();
    engine.register_fn("rng_below", move |n: i64| -> i64 {
        w.lock().expect("world mutex").rng.gen_below(n as u64) as i64
    });
}
