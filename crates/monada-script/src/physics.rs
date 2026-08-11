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
    BodyId, DrillTool, Material, MaterialId, VoxelBodyDef, VoxelShape, WheelDef,
    WheelId, WheelInput,
};
use rhai::Engine;

use crate::{DrillToolDef, PhysicsSim, SharedBridge, SharedGrids, SharedPhysics};


/// Lock helper: every host fn takes the lock for one call, like the
/// world lock in `register_host_api`.
fn lock(phys: &SharedPhysics) -> std::sync::MutexGuard<'_, PhysicsSim> {
    phys.lock().expect("physics mutex")
}

/// Shapes opened by `phys_shape`, awaiting the `phys_body` that spawns
/// one (docs/plans/ship-physics.md D5). The handle is the index; a
/// spawned slot is tombstoned rather than compacted, so a handle is
/// never reused and a stale one can never hit somebody else's hull.
///
/// Deliberately NOT a field of [`PhysicsSim`]: a shape is *authoring
/// scratch*, alive between the call that opens it and the call that
/// consumes it — usually two lines of the same `init`. It is therefore
/// neither snapshotted nor hashed. What it produces IS: the body's
/// mass, centre of mass, inertia tensor and collision skin all derive
/// from it and all ride the physics digest, and two peers running the
/// same script author the same shape by construction.
type ShapeTable = Arc<Mutex<Vec<Option<VoxelShape>>>>;

/// Borrow an open shape by handle, or die naming the caller. A map is a
/// fixed asset (DESIGN.md §8): a handle that does not resolve is a bug
/// in the map, and the same stance `phys_wheel` takes on an unknown
/// body — surfaced loudly rather than silently painting nothing.
fn with_shape<R>(
    shapes: &ShapeTable,
    handle: i64,
    who: &str,
    f: impl FnOnce(&mut VoxelShape) -> R,
) -> R {
    let mut table = shapes.lock().expect("shape table mutex");
    let shape = usize::try_from(handle)
        .ok()
        .and_then(|i| table.get_mut(i))
        .and_then(Option::as_mut)
        .unwrap_or_else(|| {
            panic!("{who}: shape {handle} is unknown or already spawned")
        });
    f(shape)
}

/// Carry one body's pose into the grid it drives (`grid_body`,
/// docs/plans/ship-physics.md D2): the sim-side frame table first, then
/// the render mirror, both from the same fixed-point numbers.
///
/// The frame's origin is derived, not copied: `GridStore` composes a
/// grid-local point as `origin + pivot + rot·(p − pivot)`, so putting
/// the pivot — which `grid_body` set to the body's CENTRE OF MASS —
/// exactly on the body's position means `origin = position − pivot`.
/// Get that wrong and the hull orbits its own centre of mass once per
/// revolution.
pub(crate) fn pose_bound_grid(
    grids: &SharedGrids,
    bridge: Option<&SharedBridge>,
    grid: i64,
    position: FixedVec3,
    orientation: FixedQuat,
) {
    let origin = {
        let mut store = grids.lock().expect("grids mutex");
        let origin = position - store.pivot(grid);
        store.set_origin(grid, origin);
        store.set_rotation(grid, orientation);
        origin
    };
    if let Some(bridge) = bridge {
        bridge
            .lock()
            .expect("bridge mutex")
            .grid_pose(grid, origin, orientation);
    }
}

/// A script-authored cell box, ordered — a map may name either corner
/// first, exactly as the `voxel_fill*` verbs allow.
fn shape_box(
    x0: i64,
    y0: i64,
    z0: i64,
    x1: i64,
    y1: i64,
    z1: i64,
) -> ((i32, i32, i32), (i32, i32, i32)) {
    (
        (x0.min(x1) as i32, y0.min(y1) as i32, z0.min(z1) as i32),
        (x0.max(x1) as i32, y0.max(y1) as i32, z0.max(z1) as i32),
    )
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
    grids: &SharedGrids,
) {
    // Authoring scratch, owned by these closures rather than by the sim
    // (see [`ShapeTable`]). One table per registration, so a re-registered
    // engine starts with no open shapes.
    let shapes: ShapeTable = Arc::new(Mutex::new(Vec::new()));

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

    // --- freeform shapes (ship-physics S-2) ---------------------------
    // `phys_box` spawns the one shape a map could describe in a call: a
    // solid block. A hull is a SHELL, and the difference is not
    // cosmetic — a shell's inertia tensor is not a block's, which is
    // exactly what an engine mounted off the centreline feels. So a map
    // opens a shape, writes into it with the same cell boxes it paints
    // the hull's voxels with, and spawns a body from the result: mass,
    // centre of mass, inertia and collision skin all derive from the
    // geometry the player can see.

    let tbl = shapes.clone();
    engine.register_fn("phys_shape", move |sx: i64, sy: i64, sz: i64| -> i64 {
        let mut table = tbl.lock().expect("shape table mutex");
        table.push(Some(VoxelShape::new(sx as i32, sy as i32, sz as i32)));
        table.len() as i64 - 1
    });

    let tbl = shapes.clone();
    engine.register_fn(
        "phys_shape_fill",
        move |shape: i64, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, mat: i64| {
            let (lo, hi) = shape_box(x0, y0, z0, x1, y1, z1);
            with_shape(&tbl, shape, "phys_shape_fill", |sh| {
                sh.fill_box(lo, hi, MaterialId(mat as u16));
            });
        },
    );

    let tbl = shapes.clone();
    engine.register_fn(
        "phys_shape_clear",
        move |shape: i64, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64| {
            let (lo, hi) = shape_box(x0, y0, z0, x1, y1, z1);
            with_shape(&tbl, shape, "phys_shape_clear", |sh| {
                sh.clear_box(lo, hi);
            });
        },
    );

    // Spawning CONSUMES the shape: a body owns its voxels from here on
    // (destruction carves them), so leaving the authoring copy addressable
    // would be two truths about one hull. `point` places the DERIVED centre
    // of mass, the same convention `phys_box` documents.
    let p = phys.clone();
    let tbl = shapes.clone();
    engine.register_fn("phys_body", move |shape: i64, point: FixedVec3| -> i64 {
        let taken = {
            let mut table = tbl.lock().expect("shape table mutex");
            usize::try_from(shape)
                .ok()
                .and_then(|i| table.get_mut(i))
                .and_then(Option::take)
        };
        let shape = taken.unwrap_or_else(|| {
            panic!("phys_body: shape {shape} is unknown or already spawned")
        });
        lock(&p)
            .world
            .spawn_voxels(&VoxelBodyDef {
                shape,
                position: point,
                orientation: FixedQuat::IDENTITY,
                linear_velocity: FixedVec3::ZERO,
                angular_velocity: FixedVec3::ZERO,
            })
            .0 as i64
    });

    // --- a grid driven by a body (ship-physics S-3) --------------------
    // The verb that marries the two halves: from here the map does not
    // pose this grid at all. What rides the grid — crew, crates, actor
    // facings, the fog cone, the deck cutaway, the camera — follows the
    // frame exactly as it always has, so a ship becomes a rigid body
    // without changing one line of what it means to walk around inside
    // one. Registered here rather than beside the other `grid_*` verbs
    // because it is the only one that needs to read a body.
    let p = phys.clone();
    let frames = grids.clone();
    let b = bridge.cloned();
    engine.register_fn("grid_body", move |grid: i64, body: i64| {
        // The grid turns about the body's centre of mass (D3), so a map
        // can never let its hand-authored pivot drift from where the
        // dynamics actually turn — and a hull that loses a wing turns
        // about its new CoM for free.
        let pose = u64::try_from(body).ok().and_then(|id| {
            let sim = lock(&p);
            sim.world
                .body(BodyId(id))
                .map(|body| (body.com_in_shape(), body.position(), body.orientation()))
        });
        {
            let mut store = frames.lock().expect("grids mutex");
            store.bind_body(grid, body);
            if let Some((com, _, _)) = pose {
                store.set_pivot(grid, com);
            }
        }
        if let Some(b) = &b {
            let mut bridge = b.lock().expect("bridge mutex");
            bridge.grid_body(grid, body);
            if let Some((com, _, _)) = pose {
                bridge.grid_pivot(grid, com);
            }
        }
        // Pose it NOW as well as every tick from here: binding during
        // `init` otherwise leaves the hull sitting at its spawn origin
        // until the first step, which is one visible frame of the ship
        // in the wrong place.
        if let Some((_, position, orientation)) = pose {
            pose_bound_grid(&frames, b.as_ref(), grid, position, orientation);
        }
    });

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

    // The body's DERIVED mass — the sum of its cells' densities, so a
    // shell weighs a shell. A map sizes thrust and reads a HUD off it;
    // ZERO for an unknown id, like `phys_pos`.
    let p = phys.clone();
    engine.register_fn("phys_mass", move |body: i64| -> Fixed {
        lock(&p)
            .world
            .body(BodyId(body as u64))
            .map_or(Fixed::ZERO, monada_physics::RigidBody::mass)
    });

    let p = phys.clone();
    engine.register_fn("phys_vel", move |body: i64| -> FixedVec3 {
        lock(&p)
            .world
            .body(BodyId(body as u64))
            .map_or(FixedVec3::ZERO, monada_physics::RigidBody::linear_velocity)
    });

    // The body's heading about +z (radians, sim frame) — the D2 chase-cam
    // read (plan §1c amendment, like phys_gravity): rotate the body's nose
    // axis into the world and take its ground-plane bearing. ZERO for an
    // unknown id, like phys_pos.
    let p = phys.clone();
    engine.register_fn("phys_yaw", move |body: i64| -> Fixed {
        lock(&p)
            .world
            .body(BodyId(body as u64))
            .map_or(Fixed::ZERO, |b| {
                let nose = b.orientation() * FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO);
                monada_fixed::trig::atan2(nose.y, nose.x)
            })
    });

    // A deterministic solidity read of the VOLUME store (the sim-side
    // "is there a roof over me" predicate — the column voxel_solid reads
    // an empty world on volume maps by design). Hashed-state read, safe
    // to steer hashed tick decisions AND render verbs alike.
    let p = phys.clone();
    engine.register_fn("phys_solid", move |x: i64, y: i64, z: i64| -> bool {
        lock(&p).terrain.get(x, y, z).is_some()
    });

    // The body's attitude pitch (radians; positive = nose above the
    // horizon). The D3 drill companion to phys_yaw: a map that wants a
    // gravity-stable bore subtracts this from its commanded drill pitch,
    // so a chassis nosing up against the face doesn't ratchet its own
    // tunnel upward.
    let p = phys.clone();
    engine.register_fn("phys_pitch", move |body: i64| -> Fixed {
        lock(&p)
            .world
            .body(BodyId(body as u64))
            .map_or(Fixed::ZERO, |b| {
                let nose = b.orientation() * FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO);
                let flat = FixedVec3::new(nose.x, nose.y, Fixed::ZERO).length();
                monada_fixed::trig::atan2(nose.z, flat)
            })
    });

    // --- the drill (plan §1c, D3) -------------------------------------

    // Tool geometry, set once at vehicle spawn. SHAPE coordinates, like
    // wheel anchors — the engine rebases via the derived CoM.
    let p = phys.clone();
    engine.register_fn(
        "phys_drill_tool",
        move |body: i64, ax: Fixed, ay: Fixed, az: Fixed, hx: Fixed, hy: Fixed, hz: Fixed| {
            let mut sim = lock(&p);
            let com = sim
                .world
                .body(BodyId(body as u64))
                .expect("phys_drill_tool: unknown body")
                .com_in_shape();
            sim.tools.insert(
                body as u64,
                DrillToolDef {
                    anchor: FixedVec3::new(ax, ay, az) - com,
                    half_extents: FixedVec3::new(hx, hy, hz),
                },
            );
        },
    );

    // The one-call drill loop (locked decision, plan §4 of the physics
    // plan satisfied engine-side): query → cut policy → carve store →
    // notify → reaction, one deterministic sweep. The POLICY is
    // front-to-back within a hardness budget: overlapped cells sorted by
    // their projection onto the drill axis (ties broken by cell coords —
    // fully deterministic), cut while the summed hardness fits `budget`.
    // `pitch` tilts the nose about the body's Y axle, positive = UP,
    // clamping is the map's business. Returns the number of voxels cut
    // (HUD/score feedback). Carves mirror to the render bridge cell by
    // cell — the same path scripted `voxel_clear`s take.
    let p = phys.clone();
    let b = bridge.cloned();
    engine.register_fn(
        "phys_drill",
        move |body: i64, pitch: Fixed, budget: Fixed| -> i64 {
            let cut = {
                let mut sim = lock(&p);
                let id = BodyId(body as u64);
                let def = *sim
                    .tools
                    .get(&(body as u64))
                    .expect("phys_drill: no tool registered (call phys_drill_tool at spawn)");
                // Positive pitch = nose UP: a rotation about +y carries
                // +x toward −z (right-hand rule), so negate. The box
                // pivots about its REAR-BOTTOM edge, not its centre: a
                // centre pivot lifts the near-bottom corner above the
                // wheel plane when pitched down, so the cut ramp starts
                // ahead of the wheels and the vehicle bridges it forever
                // (the D3 descent stall). Pivoted at the rear-bottom
                // edge — the wheels' contact plane — a pitched-down box
                // sweeps the FAR end down and the ramp begins exactly
                // under the wheels. Identity pitch is unchanged.
                let orientation = FixedQuat::from_axis_angle(
                    FixedVec3::new(Fixed::ZERO, Fixed::ONE, Fixed::ZERO),
                    -pitch,
                );
                let pivot = def.anchor
                    - FixedVec3::new(def.half_extents.x, Fixed::ZERO, def.half_extents.z);
                let tool = DrillTool {
                    anchor: pivot + orientation * (def.anchor - pivot),
                    half_extents: def.half_extents,
                    orientation,
                };
                let PhysicsSim { world, terrain, .. } = &mut *sim;
                let body_ref = world.body(id).expect("phys_drill: unknown body");
                let axis = body_ref.orientation()
                    * (tool.orientation * FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO));
                let center = body_ref.position() + body_ref.orientation() * tool.anchor;
                let mut samples: Vec<(Fixed, (i64, i64, i64), MaterialId)> = world
                    .drill_query(id, &tool, terrain)
                    .into_iter()
                    .map(|s| {
                        let c = FixedVec3::new(
                            Fixed::from_int(s.cell.0 as i32) + Fixed::HALF,
                            Fixed::from_int(s.cell.1 as i32) + Fixed::HALF,
                            Fixed::from_int(s.cell.2 as i32) + Fixed::HALF,
                        );
                        ((c - center).dot(axis), s.cell, s.material)
                    })
                    .collect();
                samples.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

                let mut spent = Fixed::ZERO;
                let mut cells: Vec<(i64, i64, i64)> = Vec::new();
                let mut mats: Vec<MaterialId> = Vec::new();
                for (_, cell, mat) in samples {
                    let hardness = world.material(mat).hardness;
                    if spent + hardness > budget {
                        break;
                    }
                    spent += hardness;
                    cells.push(cell);
                    mats.push(mat);
                }
                if !cells.is_empty() {
                    let mut lo = cells[0];
                    let mut hi = cells[0];
                    for &(x, y, z) in &cells {
                        terrain.clear(x, y, z);
                        lo = (lo.0.min(x), lo.1.min(y), lo.2.min(z));
                        hi = (hi.0.max(x), hi.1.max(y), hi.2.max(z));
                    }
                    world.notify_terrain_edit(lo, hi);
                    let _ = world.drill_reaction(id, &tool, &mats);
                }
                cells
            };
            // Mirror the carve to the render world-grid (and its debris
            // puffs) outside the physics lock.
            if let Some(b) = &b {
                let mut bridge = b.lock().expect("bridge mutex");
                for &(x, y, z) in &cut {
                    bridge.voxel_clear(x, y, z);
                }
            }
            cut.len() as i64
        },
    );

    // The body mirror's material→colour binding (D4): a straight bridge
    // forward, registered here rather than in the bridge API because
    // only volume maps have mirrored bodies to colour.
    let b = bridge.cloned();
    engine.register_fn("phys_material_color", move |mat: i64, color: i64| {
        if let Some(b) = &b {
            b.lock()
                .expect("bridge mutex")
                .phys_material_color(mat, color);
        }
    });

    // Body-mirror trim + the drill telltale (feel polish): straight
    // bridge forwards, volume-map-only for the same reason.
    let b = bridge.cloned();
    engine.register_fn(
        "body_deco_box",
        move |body: i64, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, color: i64| {
            if let Some(b) = &b {
                b.lock()
                    .expect("bridge mutex")
                    .body_deco_box(body, x0, y0, z0, x1, y1, z1, color);
            }
        },
    );

    let b = bridge.cloned();
    engine.register_fn(
        "drill_indicator",
        move |body: i64, pitch: Fixed, spinning: bool| {
            if let Some(b) = &b {
                b.lock()
                    .expect("bridge mutex")
                    .drill_indicator(body, pitch, spinning);
            }
        },
    );

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
