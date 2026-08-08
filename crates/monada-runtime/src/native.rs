//! [`NativeBackend`] — the second [`ScriptBackend`], running a map whose
//! rules are compiled Rust (docs/plans/desert-game.md §3b, decision L1).
//!
//! The desert game's rules are too large and too hot for a tree-walking
//! interpreter, and typed Rust state buys something Rhai structurally
//! cannot: a map may hold its own hashed state (a spice field, build
//! queues, AI memory) instead of contorting everything into entity
//! fields. The price is that determinism stops being enforced by the
//! runtime and becomes a discipline the rules crate must keep — see
//! [`MapRules`].
//!
//! The same [`MapRules`] source compiles to wasm later (§3f); only the
//! linkage changes.

use monada_fixed::{Fixed, FixedVec3};
use monada_sim::{Command, EntityId, PlayerId, StateHash, StateHasher};

use crate::host::{Host, LocalHost, RuntimeHost};
use crate::{ScriptBackend, ScriptError, SharedBridge, SharedWorld, UiEvent};

/// A map's rules, as compiled code rather than a script.
///
/// **The determinism contract.** Everything reachable from a `MapRules`
/// value is part of the simulation: [`snapshot`](MapRules::snapshot) must
/// return canonical bytes for it, and the driver folds those into the
/// desync digest beside the world. Concretely, an implementation must
/// hold no floats, no `HashMap`/`HashSet` iteration, no clock, no
/// `rand`, no I/O and no threads, and must draw randomness only from
/// [`Host::rng01`] / [`Host::rng_below`]. A rules crate is expected to
/// carry `#![deny(clippy::float_arithmetic, clippy::disallowed_types)]`
/// and to be exercised by the oracle on the CI platform matrix; Rhai
/// made these impossible to express, native Rust only makes them
/// forbidden (§3c).
pub trait MapRules: Send {
    /// The map's `init` trigger: declare archetypes, spawn the starting
    /// world, paint terrain.
    fn init(&mut self, host: &dyn Host);

    /// One player [`Command`], applied before the tick that released it,
    /// in canonical player order. The default ignores input — the
    /// counterpart of a script with no `command` handler.
    fn command(&mut self, host: &dyn Host, player: PlayerId, command: &Command) {
        let (_, _, _) = (host, player, command);
    }

    /// One simulation tick. `dt` is the tick duration
    /// ([`NativeBackend::set_tick_hz`]), or zero for a command-driven map.
    /// The default does nothing, for maps that advance only on commands.
    fn tick(&mut self, host: &dyn Host, dt: Fixed) {
        let (_, _) = (host, dt);
    }

    /// Canonical bytes of the rules' own hashed state. The default is
    /// "no state of my own" — correct for a map that keeps everything in
    /// the [`World`](monada_sim::World), the way a Rhai script must.
    fn snapshot(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Restore state previously produced by [`snapshot`](MapRules::snapshot).
    fn restore(&mut self, bytes: &[u8]) {
        let _ = bytes;
    }
}

/// Fold a rules value into a state digest — the same walk the driver will
/// apply beside `World::state_hash` once snapshots land (§3d).
impl StateHash for dyn MapRules {
    fn hash(&self, hasher: &mut StateHasher) {
        let bytes = self.snapshot();
        hasher.write_u64(bytes.len() as u64);
        hasher.write_bytes(&bytes);
    }
}

/// A [`ScriptBackend`] over compiled [`MapRules`].
///
/// Mirrors [`RhaiBackend`](../../monada_script/struct.RhaiBackend.html)
/// trigger for trigger, so the lockstep session, the oracle and the host
/// loop drive either without knowing which they hold.
pub struct NativeBackend {
    rules: Box<dyn MapRules>,
    host: RuntimeHost,
    world: SharedWorld,
    /// The tick duration handed to [`MapRules::tick`]; zero until
    /// [`set_tick_hz`](NativeBackend::set_tick_hz) says otherwise, which
    /// is the command-driven case.
    tick_dt: Fixed,
}

impl NativeBackend {
    /// Bind `rules` to `world`.
    #[must_use]
    pub fn new(world: SharedWorld, rules: Box<dyn MapRules>) -> NativeBackend {
        NativeBackend {
            rules,
            host: RuntimeHost::new(world.clone()),
            world,
            tick_dt: Fixed::ZERO,
        }
    }

    /// Attach the render / input bridge. Call before
    /// [`on_init`](ScriptBackend::on_init), matching `RhaiBackend`.
    pub fn set_bridge(&mut self, bridge: &SharedBridge) {
        self.host.set_bridge(bridge);
    }

    /// Set the tick duration for a fixed-rate map, as
    /// `RhaiBackend::set_tick_hz` does for `tick(dt)`.
    pub fn set_tick_hz(&mut self, hz: u32) {
        self.tick_dt = Fixed::from_ratio(1, i32::try_from(hz.max(1)).unwrap_or(i32::MAX));
    }

    /// The rules value, for tests and for the host's own inspection.
    #[must_use]
    pub fn rules(&self) -> &dyn MapRules {
        self.rules.as_ref()
    }
}

impl ScriptBackend for NativeBackend {
    /// A native map's rules are linked, not compiled at load: the
    /// manifest's `entry` is advisory and the source is ignored. Kept in
    /// the trait so a host can drive either backend through one path.
    fn load(&mut self, _source: &str) -> Result<(), ScriptError> {
        Ok(())
    }

    fn on_init(&mut self) -> Result<(), ScriptError> {
        self.rules.init(&self.host);
        Ok(())
    }

    fn on_command(&mut self, player: PlayerId, command: &Command) -> Result<(), ScriptError> {
        self.rules.command(&self.host, player, command);
        Ok(())
    }

    fn on_tick(&mut self) -> Result<(), ScriptError> {
        // The driver owns the tick counter and bumps it before the rules
        // run — the same order `RhaiBackend::on_tick` uses, so a map sees
        // the same `tick` value under either backend.
        self.world.lock().expect("world mutex").tick += 1;
        self.rules.tick(&self.host, self.tick_dt);
        Ok(())
    }

    fn drain_ui_events(&mut self) -> Vec<UiEvent> {
        // Rules push HUD events through the bridge, not through a
        // script-side buffer; nothing to drain here yet.
        Vec::new()
    }
}

/// A map's **local** layer, as compiled code — the native counterpart of
/// `LocalBackend`'s script handlers (docs/plans/input-bindings.md §1).
///
/// One instance runs per client beside [`MapRules`], over a
/// [`LocalHost`], and reaches the simulation only by submitting commands.
/// Every entry point is optional: a map implements the gestures it has.
///
/// Unlike [`MapRules`], this trait carries **no determinism contract**.
/// Its state is per-client by definition (a drag anchor, a hover, a
/// camera), never hashed, never sent; the type system already prevents it
/// from touching the world (see [`LocalHost`]).
pub trait LocalRules: Send {
    /// Once, after the simulation's `init` — camera and UI setup.
    fn local_init(&mut self, host: &dyn LocalHost) {
        let _ = host;
    }

    /// Once per **rendered frame**: hover, tooltips, camera smoothing.
    /// Its rate is client-dependent, so nothing it submits may assume a
    /// frequency.
    fn local_frame(&mut self, host: &dyn LocalHost, dt: Fixed) {
        let (_, _) = (host, dt);
    }

    /// Once per scheduled sim tick on this client: poll actions and
    /// assemble the per-tick input command (real-time maps).
    fn local_tick(&mut self, host: &dyn LocalHost, dt: Fixed) {
        let (_, _) = (host, dt);
    }

    /// An edge event for a map-declared action (press = `true`).
    fn action(&mut self, host: &dyn LocalHost, id: &str, down: bool) {
        let (_, _, _) = (host, id, down);
    }

    /// A click gesture: which button, where on the ground, and what was
    /// under the cursor.
    fn pointer(
        &mut self,
        host: &dyn LocalHost,
        button: i64,
        point: FixedVec3,
        entity: Option<EntityId>,
    ) {
        let (_, _, _, _) = (host, button, point, entity);
    }

    /// Whether this map assembles its own per-tick input command in
    /// [`local_tick`](LocalRules::local_tick). When `true` the host must
    /// not inject its legacy input snapshot — the map owns the encoding
    /// end to end (the `has_local_tick` question asked of a script).
    fn owns_tick_input(&self) -> bool {
        false
    }
}

/// The local layer's backend: compiled [`LocalRules`] over a
/// [`LocalHost`]. Mirrors `LocalBackend`'s entry points method for
/// method, so a host drives either without knowing which it holds.
pub struct NativeLocalBackend {
    rules: Box<dyn LocalRules>,
    host: RuntimeHost,
}

impl NativeLocalBackend {
    /// Build the local layer over the shared world (read-only through
    /// [`LocalHost`]) and this client's bridge.
    #[must_use]
    pub fn new(world: &SharedWorld, bridge: &SharedBridge, rules: Box<dyn LocalRules>) -> Self {
        let mut host = RuntimeHost::new(world.clone());
        host.set_bridge(bridge);
        NativeLocalBackend { rules, host }
    }

    /// Whether the map owns its per-tick input encoding.
    #[must_use]
    pub fn has_local_tick(&self) -> bool {
        self.rules.owns_tick_input()
    }

    /// Run the map's `local_init`.
    pub fn on_local_init(&mut self) {
        self.rules.local_init(&self.host);
    }

    /// Run the map's `local_frame` for one rendered frame.
    pub fn on_local_frame(&mut self, dt: Fixed) {
        self.rules.local_frame(&self.host, dt);
    }

    /// Run the map's `local_tick` for one scheduled sim tick.
    pub fn on_local_tick(&mut self, dt: Fixed) {
        self.rules.local_tick(&self.host, dt);
    }

    /// Deliver one action edge.
    pub fn on_action(&mut self, id: &str, down: bool) {
        self.rules.action(&self.host, id, down);
    }

    /// Deliver one click gesture.
    pub fn on_pointer(&mut self, button: i64, point: FixedVec3, entity: Option<EntityId>) {
        self.rules.pointer(&self.host, button, point, entity);
    }
}
