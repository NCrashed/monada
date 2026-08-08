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

use monada_fixed::Fixed;
use monada_sim::{Command, PlayerId, StateHash, StateHasher};

use crate::host::{Host, RuntimeHost};
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
