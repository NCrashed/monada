//! Physics inside the scripted sim (docs/plans/digger-demo.md §1b/§1c).
//!
//! A `terrain = "volume"` map embeds a [`PhysicsWorld`] and a
//! [`VolumeStore`] beside the entity [`World`](monada_sim::World): one
//! sim, one combined digest, one desync stream. This module is the glue
//! — the shared handle the driver steps and hashes, plus the `phys_*`
//! host functions (the sim layer's deterministic wall: they mirror the
//! physics crate 1:1 and mutate only hashed state).
//!
//! It also re-registers the terrain paint verbs (`voxel_fill` /
//! `voxel_set` / `voxel_clear`): on a volume map they write the volume
//! store (hashed, physics-visible, with the P6
//! [`notify_terrain_edit`](PhysicsWorld::notify_terrain_edit) discipline)
//! *and* still forward to the host bridge for the render world-grid —
//! same script vocabulary, deeper semantics.

// Host-API glue casts script `i64`s to the engine's id types; the values
// are small and the conversions are intentional (same stance as
// `rhai_backend`).
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedQuat, FixedVec3};
use monada_physics::{
    BodyId, Material, MaterialId, PhysicsWorld, VoxelBodyDef, VoxelShape, WheelDef, WheelId,
    WheelInput,
};
use rhai::Engine;

use crate::{SharedBridge, VolumeStore};

/// The physics side of a volume map's sim state: the rigid-body world
/// and the terrain it collides with. Snapshots serialize it whole; the
/// driver folds both digests into the combined `state_hash`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhysicsSim {
    pub world: PhysicsWorld,
    pub terrain: VolumeStore,
}

/// The shared physics handle — `Arc<Mutex<…>>` for the same reason as
/// [`SharedWorld`](crate::SharedWorld): `sync`-feature Rhai needs
/// `Send + Sync` host closures, and the single-threaded sim never
/// contends the lock.
pub type SharedPhysics = Arc<Mutex<PhysicsSim>>;

/// A fresh shared physics sim stepped at `sim_hz` (the manifest's fixed
/// tick rate — physics `dt` must equal the sim tick). Gravity starts at
/// the physics crate's opt-in ZERO; the map script owns feel constants
/// and sets it via `phys_gravity` (plan §1f).
#[must_use]
pub fn shared_physics(sim_hz: u32) -> SharedPhysics {
    Arc::new(Mutex::new(PhysicsSim {
        world: PhysicsWorld::new(sim_hz),
        terrain: VolumeStore::new(),
    }))
}

/// Lock helper: every host fn takes the lock for one call, like the
/// world lock in `register_host_api`.
fn lock(phys: &SharedPhysics) -> std::sync::MutexGuard<'_, PhysicsSim> {
    phys.lock().expect("physics mutex")
}

/// Register the `phys_*` sim verbs (plan §1c) and re-register the
/// terrain paint verbs to route through the volume store. `bridge` is
/// the render side of the dual-write; pass what the backend holds —
/// `None` (a bridgeless headless backend) paints the store only.
///
/// Ordering: must run **after** `register_bridge_api`, because the
/// `voxel_*` registrations here shadow the bridge-only ones.
///
/// ## The material-0 contract (part of `host_api` 8)
///
/// Terrain paints without a material argument write `MaterialId(0)`,
/// and the [`VoxelField`](monada_physics::VoxelField) contract lets the
/// solver assert on any id the world never registered. So a volume
/// map's **first `phys_material` call is its ground material**, and it
/// must happen before the first tick that can bring a body into
/// terrain contact. Painting before registering is fine (the store
/// holds bare ids); *stepping onto* unregistered terrain is not — and
/// the panic is data-dependent (fires at first contact), so a map that
/// gets this wrong may survive its first N ticks.
#[allow(clippy::too_many_lines)] // a flat list of host-fn registrations
pub(crate) fn register_physics_api(
    engine: &mut Engine,
    phys: &SharedPhysics,
    bridge: Option<&SharedBridge>,
) {
    // --- the drive-train verbs (plan §1c, 1:1 onto the crate) ---------

    let p = phys.clone();
    engine.register_fn("phys_gravity", move |gx: Fixed, gy: Fixed, gz: Fixed| {
        lock(&p).world.set_gravity(FixedVec3::new(gx, gy, gz));
    });

    let p = phys.clone();
    engine.register_fn(
        "phys_material",
        move |density: Fixed, friction: Fixed, restitution: Fixed, hardness: Fixed| -> i64 {
            i64::from(
                lock(&p)
                    .world
                    .register_material(Material {
                        density,
                        friction,
                        restitution,
                        hardness,
                    })
                    .0,
            )
        },
    );

    // Box bodies only in v1 (freeform voxel authoring is v2, plan §6):
    // a solid `sx × sy × sz` block of `mat`, its derived CoM placed at
    // `(x, y, z)`.
    let p = phys.clone();
    engine.register_fn(
        "phys_box",
        move |sx: i64, sy: i64, sz: i64, mat: i64, x: Fixed, y: Fixed, z: Fixed| -> i64 {
            let mut shape = VoxelShape::new(sx as i32, sy as i32, sz as i32);
            shape.fill_box(
                (0, 0, 0),
                (sx as i32 - 1, sy as i32 - 1, sz as i32 - 1),
                MaterialId(mat as u16),
            );
            lock(&p)
                .world
                .spawn_voxels(&VoxelBodyDef {
                    shape,
                    position: FixedVec3::new(x, y, z),
                    orientation: FixedQuat::IDENTITY,
                    linear_velocity: FixedVec3::ZERO,
                    angular_velocity: FixedVec3::ZERO,
                })
                .0 as i64
        },
    );

    // Wheel anchors are authored in SHAPE coordinates (the box the map
    // just built); the engine rebases into the body frame via the
    // derived CoM — the `com_in_shape` seam (physics plan P3).
    let p = phys.clone();
    engine.register_fn(
        "phys_wheel",
        move |body: i64,
              ax: Fixed,
              ay: Fixed,
              az: Fixed,
              rest: Fixed,
              radius: Fixed,
              k: Fixed,
              c: Fixed,
              mu: Fixed|
              -> i64 {
            let mut sim = lock(&p);
            let id = BodyId(body as u64);
            let com = sim
                .world
                .body(id)
                .expect("phys_wheel: unknown body")
                .com_in_shape();
            i64::from(
                sim.world
                    .attach_wheel(
                        id,
                        &WheelDef {
                            anchor: FixedVec3::new(ax, ay, az) - com,
                            rest_length: rest,
                            radius,
                            stiffness: k,
                            damping: c,
                            friction: mu,
                        },
                    )
                    .0,
            )
        },
    );

    let p = phys.clone();
    engine.register_fn(
        "phys_wheel_input",
        move |body: i64, wheel: i64, steer: Fixed, drive: Fixed, brake: Fixed| {
            lock(&p).world.set_wheel_input(
                BodyId(body as u64),
                WheelId(wheel as u32),
                WheelInput {
                    steer,
                    drive,
                    brake,
                },
            );
        },
    );

    let p = phys.clone();
    engine.register_fn(
        "phys_impulse",
        move |body: i64, jx: Fixed, jy: Fixed, jz: Fixed| {
            lock(&p)
                .world
                .apply_impulse(BodyId(body as u64), FixedVec3::new(jx, jy, jz));
        },
    );

    // Pose reads for game logic (sensors, HUD): ZERO for an unknown id,
    // matching `entity_position`'s missing-entity convention.
    let p = phys.clone();
    engine.register_fn("phys_pos", move |body: i64| -> FixedVec3 {
        lock(&p)
            .world
            .body(BodyId(body as u64))
            .map_or(FixedVec3::ZERO, monada_physics::RigidBody::position)
    });

    let p = phys.clone();
    engine.register_fn("phys_vel", move |body: i64| -> FixedVec3 {
        lock(&p)
            .world
            .body(BodyId(body as u64))
            .map_or(FixedVec3::ZERO, monada_physics::RigidBody::linear_velocity)
    });

    // --- terrain paints, volume-routed (plan §1a) ---------------------
    // Same names and arities the bridge registered, plus a trailing
    // material-id overload; these registrations shadow the bridge-only
    // ones. Every edit wakes/invalidates physics over the touched box
    // (the P6 discipline), then forwards to the bridge for the render
    // world-grid. No-material paints write id 0 — the material-0
    // contract above.

    let paint_box = {
        let p = phys.clone();
        move |x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, mat: u16| {
            let mut sim = lock(&p);
            sim.terrain.fill(x0, y0, z0, x1, y1, z1, MaterialId(mat));
            sim.world.notify_terrain_edit(
                (x0.min(x1), y0.min(y1), z0.min(z1)),
                (x0.max(x1), y0.max(y1), z0.max(z1)),
            );
        }
    };

    let paint = paint_box.clone();
    let b = bridge.cloned();
    engine.register_fn(
        "voxel_fill",
        move |x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64| {
            paint(x0, y0, z0, x1, y1, z1, 0);
            if let Some(b) = &b {
                b.lock()
                    .expect("bridge mutex")
                    .voxel_fill(x0, y0, z0, x1, y1, z1, color);
            }
        },
    );

    let paint = paint_box;
    let b = bridge.cloned();
    engine.register_fn(
        "voxel_fill",
        move |x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64, mat: i64| {
            paint(x0, y0, z0, x1, y1, z1, mat as u16);
            if let Some(b) = &b {
                b.lock()
                    .expect("bridge mutex")
                    .voxel_fill(x0, y0, z0, x1, y1, z1, color);
            }
        },
    );

    let paint_one = {
        let p = phys.clone();
        move |x: i64, y: i64, z: i64, mat: u16| {
            let mut sim = lock(&p);
            sim.terrain.set(x, y, z, MaterialId(mat));
            sim.world.notify_terrain_edit((x, y, z), (x, y, z));
        }
    };

    let paint = paint_one.clone();
    let b = bridge.cloned();
    engine.register_fn("voxel_set", move |x: i64, y: i64, z: i64, color: i64| {
        paint(x, y, z, 0);
        if let Some(b) = &b {
            b.lock().expect("bridge mutex").voxel_set(x, y, z, color);
        }
    });

    let paint = paint_one;
    let b = bridge.cloned();
    engine.register_fn(
        "voxel_set",
        move |x: i64, y: i64, z: i64, color: i64, mat: i64| {
            paint(x, y, z, mat as u16);
            if let Some(b) = &b {
                b.lock().expect("bridge mutex").voxel_set(x, y, z, color);
            }
        },
    );

    // On a volume store a clear is a true hole-punch of ONE cell — the
    // tunnel primitive the column store could only fake by truncating a
    // column. The bridge still receives its column-semantics call; the
    // render mirror for volume terrain is D2/D3's business.
    let p = phys.clone();
    let b = bridge.cloned();
    engine.register_fn("voxel_clear", move |x: i64, y: i64, z: i64| {
        {
            let mut sim = lock(&p);
            sim.terrain.clear(x, y, z);
            sim.world.notify_terrain_edit((x, y, z), (x, y, z));
        }
        if let Some(b) = &b {
            b.lock().expect("bridge mutex").voxel_clear(x, y, z);
        }
    });
}
