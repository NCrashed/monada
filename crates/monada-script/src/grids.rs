//! [`GridStore`] — the deterministic frame table behind the `grid_*` verbs
//! (docs/plans/grid-entities.md §4).
//!
//! A `grid_spawn` grid is a moving frame: a hull, a platform, a shuttle. The
//! host already keeps its pose to DRAW it, but that copy is `f64` (glam
//! quaternion normalise, `sin`/`cos`), so nothing derived from it may flow back
//! into hashed state — cross-platform lockstep forbids it (DESIGN.md §3.1).
//! This store keeps the same pose a second time in **fixed-point**, computed
//! from the exact `Fixed` arguments the script passed, which is what makes
//! "where is this hull-local point in the world?" a question a map may ask and
//! then act on.
//!
//! Determinism: the store is a pure function of the script's own deterministic
//! calls, so every peer holds identical frames and its answers may steer
//! `tick()` — the contract [`VoxelStore`](crate::VoxelStore) and `nav_path`
//! already carry. It is NOT folded into the desync digest yet;
//! [`state_hash`](GridStore::state_hash) ships so that a later
//! `RhaiDriver::state_hash` fold is one line and one deliberate re-bless.
//!
//! Coordinates are SIM cells throughout — the frame the script thinks in. The
//! render's cell shape (column vs. cubic, `host_api` 15) never enters the math
//! here; it decides only whether the DRAWN hull agrees with this frame for a
//! rotation off the vertical, which is why a map that converts coordinates
//! wants a `grid_spawn_cubic` grid (see the plan's §6 and
//! [`GridStore::is_cubic`]).

// Host-API glue casts script `i64`s to the engine's id types; the values are
// small and the conversions are intentional (the `rhai_backend` stance).
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedQuat, FixedVec3};
use monada_sim::{EntityId, StateHash, StateHasher, World};
use rhai::{Array, Dynamic, Engine};

use crate::{SharedBridge, SharedWorld};

/// The script-side sentinel for "no grid" — the value `entity_set_grid` and
/// `vision_observer` already spell as "unbind / clear".
pub const NO_GRID: i64 = -1;

/// One grid's rigid frame, in sim cells and fixed-point.
///
/// The pose is `p ↦ origin + pivot + rot·(p − pivot)`: a rotation about a
/// grid-local `pivot` (so a hull turns in place rather than about the corner
/// its local origin sits on), then the grid's placement in the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct GridFrame {
    /// Where the grid sits in the world, in sim cells (`grid_spawn`'s offset,
    /// then `grid_move`).
    origin: FixedVec3,
    /// The grid-local point `rot` turns about (`grid_pivot`). `ZERO` — the
    /// grid's own local origin — until the map names one.
    pivot: FixedVec3,
    /// The orientation `grid_orient` last set. Replaced whole on every call, so
    /// a map drives it from hashed state and never accumulates drift.
    rot: FixedQuat,
    /// Whether the render paints this grid's cells as cubes
    /// (`grid_spawn_cubic`). Not used by the math below — it is recorded so the
    /// host mirror and a map's diagnostics can tell whether the DRAWN hull can
    /// honour a rotation off the vertical (plan §6).
    cubic: bool,
    /// `grid_despawn` clears this. The slot itself is never reused, so a stale
    /// handle stays inert instead of silently addressing a later grid.
    alive: bool,
}

/// The frame table: every live grid's pose plus which entity rides which.
///
/// Handles are indices into `grids`, handed to scripts as `i64` and stable for
/// the store's lifetime. An unknown, negative or despawned handle is *inert*:
/// mutators ignore it and conversions fall back to the identity (i.e. the world
/// frame), matching how every other grid verb treats a bad handle.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GridStore {
    grids: Vec<GridFrame>,
    /// Entity → the handle it rides. `BTreeMap` (never `HashMap`) so every walk
    /// is deterministic (DESIGN.md §3.1).
    riders: BTreeMap<EntityId, u32>,
    /// Grid handle → the physics body whose pose drives it (`grid_body`,
    /// docs/plans/ship-physics.md D2). The frame of a bound grid is no longer
    /// something the map computes: after each physics step the engine copies
    /// the body's pose in here, and everything riding the grid follows.
    /// `BTreeMap` for the same reason as `riders` — the sync walks it every
    /// tick, and a walk order that varied would be a desync.
    bodies: BTreeMap<u32, u64>,
}

impl GridStore {
    /// A fresh store with no grids.
    #[must_use]
    pub fn new() -> GridStore {
        GridStore::default()
    }

    /// Register a grid at `origin` (sim cells) and return its handle. `cubic`
    /// records the render's cell shape (`grid_spawn_cubic`); the frame math is
    /// the same either way.
    pub fn spawn(&mut self, origin: FixedVec3, cubic: bool) -> i64 {
        self.grids.push(GridFrame {
            origin,
            pivot: FixedVec3::ZERO,
            rot: FixedQuat::IDENTITY,
            cubic,
            alive: true,
        });
        // A map spawning 2^63 grids has other problems; the saturating cast
        // keeps the handle a plain script `i64` without an unwrap.
        i64::try_from(self.grids.len() - 1).unwrap_or(i64::MAX)
    }

    /// How many handles have ever been issued — live and despawned alike, since
    /// slots are never reused.
    #[must_use]
    pub fn issued(&self) -> usize {
        self.grids.len()
    }

    /// Whether `grid` names a live grid.
    #[must_use]
    pub fn alive(&self, grid: i64) -> bool {
        self.frame(grid).is_some()
    }

    /// Whether `grid`'s cells are cubes (`grid_spawn_cubic`). `false` for a
    /// column-cell grid — and for an unknown handle.
    #[must_use]
    pub fn is_cubic(&self, grid: i64) -> bool {
        self.frame(grid).is_some_and(|f| f.cubic)
    }

    fn frame(&self, grid: i64) -> Option<&GridFrame> {
        usize::try_from(grid)
            .ok()
            .and_then(|i| self.grids.get(i))
            .filter(|f| f.alive)
    }

    fn frame_mut(&mut self, grid: i64) -> Option<&mut GridFrame> {
        usize::try_from(grid)
            .ok()
            .and_then(|i| self.grids.get_mut(i))
            .filter(|f| f.alive)
    }

    /// Move the grid to `origin` (sim cells) — a hull under way. Riders follow
    /// for free: their positions are grid-local, so the frame carries them.
    pub fn set_origin(&mut self, grid: i64, origin: FixedVec3) {
        if let Some(f) = self.frame_mut(grid) {
            f.origin = origin;
        }
    }

    /// Name the grid-local point [`orient`](GridStore::orient) turns about.
    /// Sticky: both call orders land the same pose, because the pose is derived
    /// from `origin`/`pivot`/`rot` on every query rather than accumulated.
    pub fn set_pivot(&mut self, grid: i64, pivot: FixedVec3) {
        if let Some(f) = self.frame_mut(grid) {
            f.pivot = pivot;
        }
    }

    /// Turn the grid to `angle` radians about `axis` (sim coordinates, need not
    /// be unit length), REPLACING its orientation. A zero-length axis defines
    /// no rotation and leaves the pose untouched — the same contract the host's
    /// `grid_orient` keeps, and deliberately not `FixedQuat`'s "zero axis ⇒
    /// identity", which would silently un-turn a hull.
    pub fn orient(&mut self, grid: i64, axis: FixedVec3, angle: Fixed) {
        if axis == FixedVec3::ZERO {
            return;
        }
        if let Some(f) = self.frame_mut(grid) {
            // Normalise on the way in. `from_axis_angle` builds the quaternion
            // from fixed-point `sin`/`cos`, so it is only NEARLY unit — and
            // `inverse` is the conjugate, whose product with the original scales
            // by `|q|²`. Left alone that turns every world→local→world trip into
            // a small dilation about the grid's origin; normalising once per
            // `grid_orient` keeps `|q|²` within rounding of 1 and the trip
            // within rounding of the identity.
            f.rot = FixedQuat::from_axis_angle(axis, angle).normalize();
        }
    }

    /// The grid's current orientation (identity for an unknown handle).
    #[must_use]
    pub fn rotation(&self, grid: i64) -> FixedQuat {
        self.frame(grid).map_or(FixedQuat::IDENTITY, |f| f.rot)
    }

    /// The grid's origin in sim cells (`ZERO` for an unknown handle).
    #[must_use]
    pub fn origin(&self, grid: i64) -> FixedVec3 {
        self.frame(grid).map_or(FixedVec3::ZERO, |f| f.origin)
    }

    /// The grid-local point it turns about (`ZERO` for an unknown handle, which
    /// is also the default: the grid's own local origin).
    #[must_use]
    pub fn pivot(&self, grid: i64) -> FixedVec3 {
        self.frame(grid).map_or(FixedVec3::ZERO, |f| f.pivot)
    }

    /// A grid-local point in world coordinates: `origin + pivot + rot·(p −
    /// pivot)`. An unknown or despawned handle is the identity — a point with
    /// no frame is already a world point.
    ///
    /// Exact to fixed-point rounding, so a round trip through
    /// [`to_local`](GridStore::to_local) returns the point to within a few
    /// ulps, NOT bit-exactly. Convert at the moments that mean something (a
    /// crew member steps off the hull), never every tick: a per-tick round trip
    /// integrates that rounding into a drift.
    #[must_use]
    pub fn to_world(&self, grid: i64, p: FixedVec3) -> FixedVec3 {
        match self.frame(grid) {
            Some(f) => f.origin + f.pivot + f.rot * (p - f.pivot),
            None => p,
        }
    }

    /// A world point in the grid's local coordinates — the inverse of
    /// [`to_world`](GridStore::to_world), with the same rounding note.
    #[must_use]
    pub fn to_local(&self, grid: i64, p: FixedVec3) -> FixedVec3 {
        match self.frame(grid) {
            Some(f) => f.pivot + f.rot.inverse() * (p - f.origin - f.pivot),
            None => p,
        }
    }

    /// Bind `entity` to `grid` WITHOUT touching its position — the v12
    /// `entity_set_grid` meaning: the stored coordinates are simply re-read in
    /// the new frame, which is what a map wants when it authors positions in
    /// hull coordinates to begin with (the ship's crew spawn that way).
    /// [`NO_GRID`], or any unknown/despawned handle, unbinds.
    ///
    /// The pose-preserving pair is [`attach`](GridStore::attach) /
    /// [`detach`](GridStore::detach); this one moves the entity in the world by
    /// whatever the frame is, which is a feature at spawn and a bug mid-flight.
    pub fn set_grid(&mut self, entity: EntityId, grid: i64) {
        match u32::try_from(grid).ok().filter(|_| self.alive(grid)) {
            Some(g) => {
                self.riders.insert(entity, g);
            }
            None => {
                self.riders.remove(&entity);
            }
        }
    }

    /// The grid `entity` rides, or [`NO_GRID`].
    #[must_use]
    pub fn grid_of(&self, entity: EntityId) -> i64 {
        self.riders.get(&entity).map_or(NO_GRID, |&g| i64::from(g))
    }

    /// Every entity riding `grid`, ascending. Walks the whole rider map, which
    /// is the right trade while a map's riders number in the dozens; a per-grid
    /// index is the fix if an SS13-scale hull ever needs it.
    #[must_use]
    pub fn riders(&self, grid: i64) -> Vec<EntityId> {
        let Ok(g) = u32::try_from(grid) else {
            return Vec::new();
        };
        self.riders
            .iter()
            .filter(|&(_, &h)| h == g)
            .map(|(&e, _)| e)
            .collect()
    }

    /// Where `entity` is in WORLD coordinates, composing through whatever grid
    /// it currently rides. `None` if the entity does not exist.
    #[must_use]
    pub fn world_position(&self, world: &World, entity: EntityId) -> Option<FixedVec3> {
        let p = world.position(entity)?;
        Some(self.to_world(self.grid_of(entity), p))
    }

    /// Bind `entity` to `grid`, REWRITING its position into that grid's frame
    /// so it does not move in the world: the crew member who steps onto the
    /// hull stays where they stood. An entity already riding another grid hops
    /// directly (its pose is carried through the world frame), so hull-to-hull
    /// needs no detach first.
    ///
    /// Returns whether the binding happened — `false` for an unknown entity or
    /// a dead/unknown grid, so a map can tell a refused hop from a silent one.
    /// The raw, non-converting bind is `entity_set_grid`, which stays what it
    /// was: this is the pose-preserving verb (plan §2 D3).
    pub fn attach(&mut self, world: &mut World, entity: EntityId, grid: i64) -> bool {
        let Some(world_p) = self.world_position(world, entity) else {
            return false;
        };
        let Ok(g) = u32::try_from(grid) else {
            return false;
        };
        if !self.alive(grid) {
            return false;
        }
        let local = self.to_local(grid, world_p);
        world.set_position(entity, local);
        self.riders.insert(entity, g);
        true
    }

    /// Unbind `entity`, rewriting its position into world coordinates so it
    /// again does not move — stepping off the hull. Returns whether it was
    /// riding anything.
    pub fn detach(&mut self, world: &mut World, entity: EntityId) -> bool {
        let grid = self.grid_of(entity);
        if grid == NO_GRID {
            return false;
        }
        if let Some(p) = world.position(entity) {
            let world_p = self.to_world(grid, p);
            world.set_position(entity, world_p);
        }
        self.riders.remove(&entity);
        true
    }

    /// Retire `grid`: every rider is DETACHED (keeping its world pose — the sim
    /// owns entity lifetime, so a vanishing render frame must never kill crew;
    /// a map that wants them to go down with the ship despawns them itself),
    /// then the handle dies for good. Later calls naming it are inert.
    pub fn despawn(&mut self, world: &mut World, grid: i64) {
        if !self.alive(grid) {
            return;
        }
        // Detach BEFORE clearing `alive`: the conversion reads the frame, and a
        // dead frame converts as the identity — which would strand every rider
        // at its raw grid-local coordinates.
        for e in self.riders(grid) {
            self.detach(world, e);
        }
        if let Some(f) = self.frame_mut(grid) {
            f.alive = false;
        }
        // A dead grid drives nothing: drop the body binding too, or the
        // per-tick sync would keep posing a frame nobody can see.
        if let Ok(i) = u32::try_from(grid) {
            self.bodies.remove(&i);
        }
    }

    /// Drive `grid`'s frame from physics `body` (`grid_body`,
    /// docs/plans/ship-physics.md D2), or release it with a negative `body`.
    /// From here on the map does not pose this grid — the engine copies the
    /// body's pose in after each physics step, and riders, props, fog and
    /// camera follow the frame as they always have.
    ///
    /// An unknown or despawned handle is inert, like every other grid verb.
    pub fn bind_body(&mut self, grid: i64, body: i64) {
        if !self.alive(grid) {
            return;
        }
        let Ok(handle) = u32::try_from(grid) else {
            return;
        };
        match u64::try_from(body) {
            Ok(id) => self.bodies.insert(handle, id),
            Err(_) => self.bodies.remove(&handle),
        };
    }

    /// The body driving `grid`, or `-1` — the read a map uses to ask "is this
    /// hull under power" without keeping its own table.
    #[must_use]
    pub fn body_of(&self, grid: i64) -> i64 {
        u32::try_from(grid)
            .ok()
            .and_then(|h| self.bodies.get(&h))
            .and_then(|&id| i64::try_from(id).ok())
            .unwrap_or(NO_GRID)
    }

    /// Every (grid handle, body id) binding, in handle order — what the
    /// per-tick sync walks. Deterministic by construction: `BTreeMap` order.
    #[must_use]
    pub fn bound_grids(&self) -> Vec<(i64, u64)> {
        self.bodies
            .iter()
            .map(|(&h, &b)| (i64::from(h), b))
            .collect()
    }

    /// Set `grid`'s orientation from a quaternion, replacing it whole.
    ///
    /// The quaternion twin of [`orient`](GridStore::orient), for a frame driven
    /// by something that already has one — a rigid body's attitude. Going
    /// through axis/angle instead would round a pose the solver computed
    /// exactly, twice, every tick. Normalised on the way in for the reason
    /// `orient` spells out: a nearly-unit quaternion turns every
    /// world→local→world trip into a small dilation.
    pub fn set_rotation(&mut self, grid: i64, rot: FixedQuat) {
        if let Some(f) = self.frame_mut(grid) {
            f.rot = rot.normalize();
        }
    }

    /// Drop bindings whose entity is gone. Run once per tick (the backend holds
    /// both the world and the store): a despawned id left bound would leak, and
    /// entity ids are never reused so nothing can reclaim it.
    pub fn retain(&mut self, world: &World) {
        self.riders.retain(|&e, _| world.position(e).is_some());
    }

    /// Canonical digest: every frame in handle order, then the rider map.
    /// Unused by the driver today (plan §2 D2) — it exists so folding this
    /// store into the desync hash later is a one-line change, not a redesign.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = StateHasher::new();
        StateHash::hash(self, &mut h);
        h.finish()
    }
}

impl StateHash for GridStore {
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(self.grids.len() as u64);
        for f in &self.grids {
            f.origin.hash(h);
            f.pivot.hash(h);
            f.rot.hash(h);
            f.cubic.hash(h);
            f.alive.hash(h);
        }
        h.write_u64(self.riders.len() as u64);
        for (e, &g) in &self.riders {
            e.hash(h);
            h.write_u64(u64::from(g));
        }
        h.write_u64(self.bodies.len() as u64);
        for (&g, &b) in &self.bodies {
            h.write_u64(u64::from(g));
            h.write_u64(b);
        }
    }
}

/// The shared frame table — `Arc<Mutex<_>>` for the same reason as
/// [`SharedWorld`](crate::SharedWorld): `sync`-feature Rhai needs `Send + Sync`
/// host closures, and the single-threaded sim never contends the lock.
pub type SharedGrids = Arc<Mutex<GridStore>>;

/// A fresh shared frame table.
#[must_use]
pub fn shared_grids() -> SharedGrids {
    Arc::new(Mutex::new(GridStore::new()))
}

fn lock(grids: &SharedGrids) -> std::sync::MutexGuard<'_, GridStore> {
    grids.lock().expect("grids mutex")
}

/// A sim point from the integer cell offset the spawn verbs take.
fn cells(x: i64, y: i64, z: i64) -> FixedVec3 {
    let f = |v: i64| Fixed::from_int(i32::try_from(v).unwrap_or(0));
    FixedVec3::new(f(x), f(y), f(z))
}

/// Mirror a spawn into the render bridge and check the two handle spaces stayed
/// in step. Both sides allocate by push order, so they agree as long as EVERY
/// spawn goes through here — a grid spawned behind the store's back (from the
/// local layer, say, which holds the bridge API directly) would shift every
/// later handle and paint hulls into the wrong grid. Cheap to check, miserable
/// to debug otherwise.
fn mirror_spawn(
    bridge: Option<&SharedBridge>,
    handle: i64,
    wx: i64,
    wy: i64,
    wz: i64,
    cubic: bool,
) {
    let Some(bridge) = bridge else { return };
    let mut b = bridge.lock().expect("bridge mutex");
    let mirrored = if cubic {
        b.grid_spawn_cubic(wx, wy, wz)
    } else {
        b.grid_spawn(wx, wy, wz)
    };
    // A headless bridge answers `-1` (no render grid at all) — expected, not a
    // skew. Only a real, differing handle means the spaces diverged.
    if mirrored >= 0 && mirrored != handle {
        eprintln!(
            "monada-script: grid handle skew — the frame table issued {handle}, \
             the renderer {mirrored}; a grid was spawned outside the sim layer"
        );
    }
}

/// Register the grid verbs (docs/plans/grid-entities.md §3) against the shared
/// frame table, dual-writing to `bridge` so the render mirror follows.
///
/// Call it **after** [`register_bridge_api`](crate::rhai_backend::register_bridge_api):
/// Rhai resolves at call time and later registrations win, so these shadow the
/// bridge-only `grid_spawn` / `grid_orient` / `grid_pivot` / `entity_set_grid`
/// and make the store — not the renderer — the place a grid's frame lives. The
/// same shadowing discipline `register_physics_api` uses for the paint verbs.
///
/// With `bridge` as `None` (a bridgeless headless backend) the verbs still work
/// against the store alone, so `grid_world` answers the same on a peer with no
/// window as on one with a renderer. That is the whole reason the frame lives
/// here rather than in the host.
#[allow(clippy::too_many_lines)] // a flat list of host-fn registrations
pub(crate) fn register_grid_api(
    engine: &mut Engine,
    grids: &SharedGrids,
    world: &SharedWorld,
    bridge: Option<&SharedBridge>,
) {
    // --- lifecycle -------------------------------------------------------
    let (g, b) = (grids.clone(), bridge.cloned());
    engine.register_fn("grid_spawn", move |wx: i64, wy: i64, wz: i64| -> i64 {
        let handle = lock(&g).spawn(cells(wx, wy, wz), false);
        mirror_spawn(b.as_ref(), handle, wx, wy, wz, false);
        handle
    });

    let (g, b) = (grids.clone(), bridge.cloned());
    engine.register_fn(
        "grid_spawn_cubic",
        move |wx: i64, wy: i64, wz: i64| -> i64 {
            let handle = lock(&g).spawn(cells(wx, wy, wz), true);
            mirror_spawn(b.as_ref(), handle, wx, wy, wz, true);
            handle
        },
    );

    let (g, w, b) = (grids.clone(), world.clone(), bridge.cloned());
    engine.register_fn("grid_despawn", move |grid: i64| {
        // Take the rider list BEFORE the store retires them, so the render
        // bindings can be dropped too: a bridge whose `grid_despawn` is still
        // the default no-op would otherwise keep drawing crew on a ghost hull.
        let riders: Vec<i64> = {
            let mut store = lock(&g);
            let riders = store.riders(grid).iter().map(|e| e.0 as i64).collect();
            let mut world = w.lock().expect("world mutex");
            store.despawn(&mut world, grid);
            riders
        };
        if let Some(bridge) = &b {
            let mut bridge = bridge.lock().expect("bridge mutex");
            for e in riders {
                bridge.entity_set_grid(e, NO_GRID);
            }
            bridge.grid_despawn(grid);
        }
    });

    // --- pose ------------------------------------------------------------
    let (g, b) = (grids.clone(), bridge.cloned());
    engine.register_fn("grid_move", move |grid: i64, origin: FixedVec3| {
        lock(&g).set_origin(grid, origin);
        if let Some(bridge) = &b {
            bridge.lock().expect("bridge mutex").grid_move(grid, origin);
        }
    });

    let (g, b) = (grids.clone(), bridge.cloned());
    engine.register_fn(
        "grid_orient",
        move |grid: i64, axis: FixedVec3, angle: Fixed| {
            lock(&g).orient(grid, axis, angle);
            if let Some(bridge) = &b {
                bridge
                    .lock()
                    .expect("bridge mutex")
                    .grid_orient(grid, axis, angle);
            }
        },
    );

    let (g, b) = (grids.clone(), bridge.cloned());
    engine.register_fn("grid_pivot", move |grid: i64, point: FixedVec3| {
        lock(&g).set_pivot(grid, point);
        if let Some(bridge) = &b {
            bridge.lock().expect("bridge mutex").grid_pivot(grid, point);
        }
    });

    // --- membership ------------------------------------------------------
    let (g, b) = (grids.clone(), bridge.cloned());
    engine.register_fn("entity_set_grid", move |e: i64, grid: i64| {
        lock(&g).set_grid(EntityId(e as u64), grid);
        if let Some(bridge) = &b {
            bridge
                .lock()
                .expect("bridge mutex")
                .entity_set_grid(e, grid);
        }
    });

    let (g, w, b) = (grids.clone(), world.clone(), bridge.cloned());
    engine.register_fn("entity_attach", move |e: i64, grid: i64| -> bool {
        let ok = {
            // Lock order is grids → world everywhere in this module.
            let mut store = lock(&g);
            let mut world = w.lock().expect("world mutex");
            store.attach(&mut world, EntityId(e as u64), grid)
        };
        if ok {
            if let Some(bridge) = &b {
                bridge
                    .lock()
                    .expect("bridge mutex")
                    .entity_set_grid(e, grid);
            }
        }
        ok
    });

    let (g, w, b) = (grids.clone(), world.clone(), bridge.cloned());
    engine.register_fn("entity_detach", move |e: i64| -> bool {
        let ok = {
            let mut store = lock(&g);
            let mut world = w.lock().expect("world mutex");
            store.detach(&mut world, EntityId(e as u64))
        };
        if ok {
            if let Some(bridge) = &b {
                bridge
                    .lock()
                    .expect("bridge mutex")
                    .entity_set_grid(e, NO_GRID);
            }
        }
        ok
    });

    // --- grid voxels (render-only, so bridge-only) ------------------------
    if let Some(bridge) = bridge {
        let b = bridge.clone();
        engine.register_fn(
            "voxel_set_in",
            move |grid: i64, x: i64, y: i64, z: i64, color: i64| {
                b.lock()
                    .expect("bridge mutex")
                    .voxel_set_in(grid, x, y, z, color);
            },
        );

        let b = bridge.clone();
        engine.register_fn(
            "voxel_clear_in",
            move |grid: i64, x: i64, y: i64, z: i64| {
                b.lock()
                    .expect("bridge mutex")
                    .voxel_clear_in(grid, x, y, z);
            },
        );

        // The camera's frame is a grid property too — it belongs beside the
        // rest of the grid verbs even though it draws nothing itself.
        let b = bridge.clone();
        engine.register_fn("camera_grid", move |grid: i64| {
            b.lock().expect("bridge mutex").camera_grid(grid);
        });
    }

    register_grid_read_api(engine, grids);
}

/// Register the **read-only** frame queries — the subset both sides of the sync
/// wall may hold. The sim backend gets them beside the mutators (via
/// [`register_grid_api`]); the local layer gets *only* these, so an unsynced
/// per-client script can turn a cursor hit into a hull cell but can never move
/// a hull or re-seat an entity. The same split
/// [`register_world_read_api`](crate::rhai_backend::register_world_read_api)
/// draws for entity state.
pub(crate) fn register_grid_read_api(engine: &mut Engine, grids: &SharedGrids) {
    let g = grids.clone();
    engine.register_fn("grid_world", move |grid: i64, p: FixedVec3| -> FixedVec3 {
        lock(&g).to_world(grid, p)
    });

    let g = grids.clone();
    engine.register_fn("grid_local", move |grid: i64, p: FixedVec3| -> FixedVec3 {
        lock(&g).to_local(grid, p)
    });

    let g = grids.clone();
    engine.register_fn("entity_grid", move |e: i64| -> i64 {
        lock(&g).grid_of(EntityId(e as u64))
    });

    let g = grids.clone();
    engine.register_fn("grid_riders", move |grid: i64| -> Array {
        lock(&g)
            .riders(grid)
            .into_iter()
            .map(|e| Dynamic::from(e.0 as i64))
            .collect()
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sim points compare to fixed-point rounding, not bit-exactly: a frame
    /// conversion runs a quaternion product in Q32.32, so a few ulps of the
    /// 2⁻³² grid survive. A cell is 1.0, so 1e-6 is a millionth of a cell —
    /// tight enough to catch a real error, loose enough to be honest about the
    /// arithmetic.
    const EPS: f64 = 1e-6;

    fn v(x: f64, y: f64, z: f64) -> FixedVec3 {
        FixedVec3::new(Fixed::from_f64(x), Fixed::from_f64(y), Fixed::from_f64(z))
    }

    fn near(a: FixedVec3, b: FixedVec3) -> bool {
        (a - b).length().to_f64() < EPS
    }

    /// A hull the ship demo would spawn: off the world origin, pivoted about
    /// its middle, tumbling about a tilted axis.
    fn hull(store: &mut GridStore) -> i64 {
        let g = store.spawn(v(4.0, 3.0, 0.0), true);
        store.set_pivot(g, v(9.5, 9.5, 2.0));
        store.orient(g, v(0.3, 0.0, 1.0), Fixed::from_f64(0.7));
        g
    }

    fn world_with_entity(p: FixedVec3) -> (World, EntityId) {
        let mut world = World::new(0);
        let arch = world.register_archetype(&["deck"]);
        let e = world.spawn(arch);
        world.set_position(e, p);
        (world, e)
    }

    /// The frame and its inverse are inverses, for a pose with all three parts
    /// engaged (origin, pivot, a tilted rotation).
    #[test]
    fn to_world_and_to_local_round_trip() {
        let mut store = GridStore::new();
        let g = hull(&mut store);
        for p in [v(5.0, 2.0, 3.0), v(0.0, 0.0, 0.0), v(-7.25, 19.5, 5.0)] {
            let round = store.to_local(g, store.to_world(g, p));
            assert!(near(round, p), "round trip: want {p:?}, got {round:?}");
        }
        // The pivot is the one point the rotation holds still.
        let pivot = v(9.5, 9.5, 2.0);
        assert!(
            near(store.to_world(g, pivot), pivot + store.origin(g)),
            "the pivot only translates by the grid's origin"
        );
    }

    /// A point with no frame is already a world point: an unknown handle
    /// converts as the identity rather than inventing a pose.
    #[test]
    fn an_unknown_handle_converts_as_the_identity() {
        let store = GridStore::new();
        let p = v(5.0, 2.0, 3.0);
        for bad in [NO_GRID, 0, 7, i64::MAX] {
            assert!(near(store.to_world(bad, p), p));
            assert!(near(store.to_local(bad, p), p));
        }
    }

    /// Attaching rewrites the position into the hull's frame WITHOUT moving the
    /// entity in the world — the whole point of the verb. Detaching undoes it.
    #[test]
    fn attach_and_detach_preserve_the_world_pose() {
        let mut store = GridStore::new();
        let g = hull(&mut store);
        let stood = v(5.0, 2.0, 3.0);
        let (mut world, e) = world_with_entity(stood);

        assert!(store.attach(&mut world, e, g), "attached");
        assert_eq!(store.grid_of(e), g);
        let local = world.position(e).expect("still alive");
        assert!(
            !near(local, stood),
            "a tumbling hull's local frame differs from the world one — \
             otherwise this test proves nothing"
        );
        assert!(
            near(store.world_position(&world, e).expect("seated"), stood),
            "the crew member did not move in the world"
        );

        assert!(store.detach(&mut world, e), "detached");
        assert_eq!(store.grid_of(e), NO_GRID);
        assert!(
            near(world.position(e).expect("still alive"), stood),
            "and is back in world coordinates where it stood"
        );
        assert!(!store.detach(&mut world, e), "detaching twice is a no-op");
    }

    /// Hull to hull in one call: an entity already riding a grid hops through
    /// the world frame, so a shuttle's crew can board a station without the map
    /// converting anything by hand.
    #[test]
    fn attaching_while_bound_hops_between_grids() {
        let mut store = GridStore::new();
        let a = hull(&mut store);
        let b = store.spawn(v(-20.0, 6.5, 1.0), true);
        // A different pose, so "the pose survived the hop" is a real claim.
        store.set_pivot(b, v(2.0, 2.0, 0.0));
        store.orient(b, v(0.0, 1.0, 0.0), Fixed::from_f64(0.4));
        let stood = v(5.0, 2.0, 3.0);
        let (mut world, e) = world_with_entity(stood);

        assert!(store.attach(&mut world, e, a));
        let seated = store.world_position(&world, e).expect("seated");
        assert!(store.attach(&mut world, e, b), "hopped");
        assert_eq!(store.grid_of(e), b);
        assert!(
            near(store.world_position(&world, e).expect("seated"), seated),
            "a hop keeps the world pose"
        );
    }

    /// Despawning a grid detaches its riders ALIVE and at their world poses,
    /// and kills the handle for good.
    #[test]
    fn despawn_detaches_riders_and_retires_the_handle() {
        let mut store = GridStore::new();
        let g = hull(&mut store);
        let stood = v(5.0, 2.0, 3.0);
        let (mut world, e) = world_with_entity(stood);
        store.attach(&mut world, e, g);

        store.despawn(&mut world, g);
        assert!(!store.alive(g), "the handle is retired");
        assert_eq!(store.grid_of(e), NO_GRID, "the rider was detached");
        assert!(
            world.position(e).is_some(),
            "and is still alive — a render frame never kills sim entities"
        );
        assert!(
            near(world.position(e).expect("alive"), stood),
            "detached at its world pose, not stranded at raw grid-local coords"
        );

        // A dead handle is inert, not a hit on someone else's hull.
        assert!(!store.attach(&mut world, e, g), "cannot ride a dead grid");
        store.set_origin(g, v(99.0, 99.0, 99.0));
        assert!(
            near(store.to_world(g, stood), stood),
            "no frame, no rotation"
        );
        assert!(store.riders(g).is_empty());
    }

    /// Handles are monotonic: a despawn does not free its slot for the next
    /// spawn (the `EntityId` argument — a stale handle must stay inert).
    #[test]
    fn handles_are_never_reused() {
        let mut store = GridStore::new();
        let mut world = World::new(0);
        let first = store.spawn(v(0.0, 0.0, 0.0), false);
        store.despawn(&mut world, first);
        let second = store.spawn(v(1.0, 0.0, 0.0), false);
        assert_ne!(first, second, "the retired handle is not reissued");
        assert!(!store.alive(first));
        assert!(store.alive(second));
        assert_eq!(store.issued(), 2);
    }

    /// A zero-length axis leaves the pose alone — it does NOT reset the hull to
    /// identity, which is what `FixedQuat::from_axis_angle` alone would do.
    #[test]
    fn a_zero_axis_leaves_the_pose_alone() {
        let mut store = GridStore::new();
        let g = hull(&mut store);
        let turned = store.rotation(g);
        store.orient(g, FixedVec3::ZERO, Fixed::from_f64(1.0));
        assert_eq!(store.rotation(g), turned);
    }

    /// Bindings are retired with their entity: a despawned rider must not leak
    /// (a long session churning crew would grow the map forever).
    #[test]
    fn retain_drops_bindings_of_despawned_entities() {
        let mut store = GridStore::new();
        let g = hull(&mut store);
        let (mut world, e) = world_with_entity(v(5.0, 2.0, 3.0));
        store.attach(&mut world, e, g);

        world.despawn(e);
        store.retain(&world);
        assert_eq!(store.grid_of(e), NO_GRID);
        assert!(store.riders(g).is_empty());
    }

    /// The digest keys every part of the frame and the rider map, and two
    /// stores fed the same calls agree — the property a desync check would rest
    /// on when this is folded into the driver hash.
    #[test]
    fn state_hash_keys_the_frame_and_its_riders() {
        let build = || {
            let mut store = GridStore::new();
            let g = hull(&mut store);
            let (mut world, e) = world_with_entity(v(5.0, 2.0, 3.0));
            store.attach(&mut world, e, g);
            (store, world, e, g)
        };
        let (store, ..) = build();
        let (twin, ..) = build();
        let base = store.state_hash();
        assert_eq!(base, twin.state_hash(), "the same calls hash the same");

        for mutate in [
            &(|s: &mut GridStore, _: &mut World, _: EntityId, g: i64| {
                s.set_origin(g, v(1.0, 0.0, 0.0));
            }) as &dyn Fn(&mut GridStore, &mut World, EntityId, i64),
            &|s, _, _, g| s.set_pivot(g, v(0.0, 1.0, 0.0)),
            &|s, _, _, g| s.orient(g, v(0.0, 0.0, 1.0), Fixed::from_f64(0.2)),
            &|s, w, e, _| {
                s.detach(w, e);
            },
            &|s, w, _, g| s.despawn(w, g),
            &|s, _, _, _| {
                s.spawn(v(0.0, 0.0, 0.0), false);
            },
        ] {
            let (mut store, mut world, e, g) = build();
            mutate(&mut store, &mut world, e, g);
            assert_ne!(store.state_hash(), base, "every frame change re-keys");
        }
    }
}
