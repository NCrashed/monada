//! [`RhaiDriver`] — the adapter that lets the Rhai backend drive a
//! lockstep session (DESIGN.md §3.1, M3).
//!
//! `monada-net`'s [`SimDriver`](monada_net::SimDriver) is the seam the
//! lockstep session steps; this is its only Rhai implementation, so the
//! session crate never has to link the script language. The driver owns
//! a [`RhaiBackend`] plus a handle on the same [`World`](monada_sim::World)
//! it mutates, and maps the three session operations onto the script
//! triggers:
//!
//! - `apply_command` → the map's `command` trigger (via `on_command`),
//! - `step` → the map's `tick` trigger (via `on_tick`),
//! - `state_hash` → the world's canonical [`state_hash`](monada_sim::World::state_hash).

use monada_fixed::FixedVec3;
use monada_net::SimDriver;
use monada_sim::{Command, PlayerId};

use crate::{RhaiBackend, ScriptBackend, ScriptError, SharedBridge, SharedWorld};

/// A lockstep [`SimDriver`] backed by a compiled Rhai map.
pub struct RhaiDriver {
    backend: RhaiBackend,
    world: SharedWorld,
}

impl RhaiDriver {
    /// Build a driver: bind a fresh backend to `world`, compile `source`,
    /// and run its `init` trigger so the world is populated before tick 0.
    ///
    /// # Errors
    /// Propagates a compile or `init`-time [`ScriptError`].
    pub fn new(world: SharedWorld, source: &str) -> Result<RhaiDriver, ScriptError> {
        let mut backend = RhaiBackend::new(world.clone());
        backend.load(source)?;
        backend.on_init()?;
        Ok(RhaiDriver { backend, world })
    }

    /// Like [`new`](RhaiDriver::new) but with a host [`HostBridge`](crate::HostBridge)
    /// set **before** `init` — required for maps whose `init` calls the
    /// render/input host-API (defining models, painting the board). Headless
    /// callers (oracle, net tests) pass a `NullBridge`.
    ///
    /// # Errors
    /// Propagates a compile or `init`-time [`ScriptError`].
    pub fn with_bridge(
        world: SharedWorld,
        source: &str,
        bridge: &SharedBridge,
    ) -> Result<RhaiDriver, ScriptError> {
        let mut backend = RhaiBackend::new(world.clone());
        backend.set_bridge(bridge);
        backend.load(source)?;
        backend.on_init()?;
        Ok(RhaiDriver { backend, world })
    }

    /// The shared world this driver mutates (e.g. for the render bridge to
    /// read positions between ticks).
    #[must_use]
    pub fn world(&self) -> &SharedWorld {
        &self.world
    }

    /// Forward a pointer event to the map's `pointer` trigger (the host's
    /// click FSM for a networked map). Commands the gesture queues are
    /// drained from the bridge by the host, not applied here.
    ///
    /// # Errors
    /// Propagates a [`ScriptError`] the handler raises.
    pub fn on_pointer(
        &mut self,
        button: i64,
        point: FixedVec3,
        entity: i64,
    ) -> Result<(), ScriptError> {
        self.backend.on_pointer(button, point, entity)
    }
}

impl SimDriver for RhaiDriver {
    fn apply_command(&mut self, player: PlayerId, command: &Command) {
        // Scripts are fixed map assets: a raise here is a bug, surfaced
        // the same way the host treats `on_tick` (DESIGN.md §8).
        self.backend
            .on_command(player, command)
            .expect("script command trigger");
    }

    fn step(&mut self) {
        self.backend.on_tick().expect("script tick trigger");
    }

    fn state_hash(&self) -> u64 {
        self.world.lock().expect("world mutex").state_hash()
    }
}
