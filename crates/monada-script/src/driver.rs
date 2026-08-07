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
//! - `step` → the map's `tick` trigger (via `on_tick`), then — on a
//!   `terrain = "volume"` map — one [`PhysicsWorld`] step against the
//!   volume terrain (docs/plans/digger-demo.md §1b),
//! - `state_hash` → the world's canonical [`state_hash`](monada_sim::World::state_hash),
//!   or (with physics embedded) the combined digest described on
//!   [`state_hash`](RhaiDriver::state_hash) itself.
//!
//! [`PhysicsWorld`]: monada_physics::PhysicsWorld

use monada_net::SimDriver;
use monada_sim::{Command, PlayerId, StateHasher};

use crate::{
    PhysicsSim, RhaiBackend, ScriptBackend, ScriptError, SharedBridge, SharedGrids, SharedPhysics,
    SharedWorld,
};

/// A lockstep [`SimDriver`] backed by a compiled Rhai map.
pub struct RhaiDriver {
    backend: RhaiBackend,
    world: SharedWorld,
    /// The embedded physics sim of a `terrain = "volume"` map; `None`
    /// keeps the driver — and its hash — exactly the pre-physics one.
    phys: Option<SharedPhysics>,
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
        Ok(RhaiDriver {
            backend,
            world,
            phys: None,
        })
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
        Ok(RhaiDriver {
            backend,
            world,
            phys: None,
        })
    }

    /// Like [`with_bridge`](RhaiDriver::with_bridge) but for a
    /// `terrain = "volume"` map: the shared physics sim (built by
    /// [`shared_physics`](crate::shared_physics) at the manifest's fixed
    /// `sim_hz`) is registered before `init`, so the map's setup can
    /// declare materials, paint volume terrain, and spawn bodies. The
    /// driver then steps and hashes it alongside the entity world.
    ///
    /// # Errors
    /// Propagates a compile or `init`-time [`ScriptError`].
    pub fn with_physics(
        world: SharedWorld,
        source: &str,
        bridge: &SharedBridge,
        phys: &SharedPhysics,
    ) -> Result<RhaiDriver, ScriptError> {
        let mut backend = RhaiBackend::new(world.clone());
        backend.set_bridge(bridge);
        backend.set_physics(phys);
        backend.load(source)?;
        backend.on_init()?;
        Ok(RhaiDriver {
            backend,
            world,
            phys: Some(phys.clone()),
        })
    }

    /// Configure the tick duration for a fixed-rate map. Forwarded to the
    /// backend so the script's `tick(dt)` handler receives the correct value.
    pub fn set_tick_hz(&mut self, hz: u32) {
        self.backend.set_tick_hz(hz);
    }

    /// The shared world this driver mutates (e.g. for the render bridge to
    /// read positions between ticks).
    #[must_use]
    pub fn world(&self) -> &SharedWorld {
        &self.world
    }

    /// The embedded physics sim, when this drives a volume map (e.g. for
    /// the host's body render mirror to read poses between ticks).
    #[must_use]
    pub fn physics(&self) -> Option<&SharedPhysics> {
        self.phys.as_ref()
    }

    /// The grid frame table the map's `grid_*` verbs write — for the host to
    /// hand to the local layer ([`LocalBackend::set_grids`](crate::LocalBackend::set_grids))
    /// and to read grid poses between ticks.
    #[must_use]
    pub fn grids(&self) -> &SharedGrids {
        self.backend.grids()
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
        // Per-tick order (docs/plans/digger-demo.md §1b): commands were
        // already applied by the session; the script's `tick` runs first
        // (inputs, spawns, terrain edits), then physics integrates
        // against the terrain the script just finished shaping.
        self.backend.on_tick().expect("script tick trigger");
        if let Some(phys) = &self.phys {
            let mut sim = phys.lock().expect("physics mutex");
            let PhysicsSim { world, terrain, .. } = &mut *sim;
            world.step(terrain);
        }
    }

    fn state_hash(&self) -> u64 {
        // One sim, one digest (docs/plans/digger-demo.md §1b). Append
        // order is FIXED and append-only: entity-world digest, physics
        // digest, terrain digest, drill-tool digest (the D3 append),
        // folded in that order into a fresh FNV. A map without physics
        // hashes exactly as before — the plain world digest — so
        // pre-volume goldens stay valid.
        let world = self.world.lock().expect("world mutex").state_hash();
        let Some(phys) = &self.phys else {
            return world;
        };
        let sim = phys.lock().expect("physics mutex");
        let mut h = StateHasher::new();
        h.write_u64(world);
        h.write_u64(sim.world.state_hash());
        h.write_u64(sim.terrain.state_hash());
        h.write_u64(sim.tools_hash());
        h.finish()
    }
}
