//! The **Rhai** script backend and its host-API registration (DESIGN.md
//! §3.3, §5.2).
//!
//! This crate is the strict wall between the Rhai language and sim types:
//! it is the only place that links `rhai` *and* `monada-sim`. Everything
//! language-neutral — the [`ScriptBackend`] contract, the [`HostBridge`]
//! render/input seam, the deterministic world services
//! ([`VoxelStore`], [`VolumeStore`]) — lives in `monada-runtime`, so the
//! native and wasm backends share one definition of the host API rather
//! than reimplementing it (docs/plans/desert-game.md §3a, decision L7).
//! Those items are re-exported here unchanged, so a map host may keep
//! depending on `monada-script` alone.
//!
//! Determinism: Rhai is built with `no_float`, so scripts cannot do IEEE
//! arithmetic at all — sim math goes through `monada-fixed`. All gameplay
//! state lives in the [`World`](monada_sim::World) (decision A2), so a
//! Rhai script keeps no hashed state of its own; it reads and writes
//! entity position and fields through the host API.
#![forbid(unsafe_code)]

mod driver;
mod grids;
mod local_backend;
mod physics;
mod rhai_backend;

pub use driver::RhaiDriver;
pub use grids::{shared_grids, GridStore, SharedGrids, NO_GRID};
pub use local_backend::LocalBackend;
pub use rhai_backend::RhaiBackend;

// The language-neutral runtime, re-exported so `monada-script` remains a
// single-dependency entry point for hosts and tests (and so this crate's
// own modules keep referring to `crate::HostBridge` and friends).
pub use monada_runtime::{
    check_host_api, shared_physics, shared_terrain, shared_world, DrillToolDef, HostBridge,
    MaterialId, NullBridge, PhysicsSim, ScriptBackend, ScriptError, SharedBridge, SharedPhysics,
    SharedTerrain, SharedWorld, UiEvent, VolumeStore, VoxelStore, HOST_API_OLDEST,
    HOST_API_VERSION,
};

/// The M2 walk-in-a-circle scenario, as a script (DESIGN.md §7) — the
/// engine knows nothing about circles. Embedded until the map archive
/// format lands (M4).
pub const WALK_CIRCLE_SCRIPT: &str = include_str!("../scripts/walk_circle.rhai");

/// The M3 command-driven demo scenario (DESIGN.md §3.1, §7). Players
/// issue `Command`s over the lockstep wire: `verb == 1` spawns a unit
/// at the command's point, `verb == 2` sets a unit's velocity; `tick`
/// integrates position by velocity. The engine knows nothing about the
/// verbs — it is the script that interprets them. Exercises the whole
/// command path end to end (`on_command` -> host API -> `World`).
pub const COMMAND_DEMO_SCRIPT: &str = include_str!("../scripts/command_demo.rhai");

/// Build a seeded world, load `source`, run its `init` trigger then
/// `ticks` `tick` triggers, and return the shared world. The reusable
/// scenario runner for tests and the determinism oracle.
///
/// # Errors
/// Propagates any compile/run [`ScriptError`].
pub fn run_script(seed: u64, source: &str, ticks: u64) -> Result<SharedWorld, ScriptError> {
    let world = shared_world(seed);
    let mut backend = RhaiBackend::new(world.clone());
    backend.load(source)?;
    backend.on_init()?;
    for _ in 0..ticks {
        backend.on_tick()?;
    }
    Ok(world)
}
