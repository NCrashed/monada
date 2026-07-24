//! [`PhysicsWorld`] — the physics state root (docs/plans/voxel-physics.md
//! §3 `world`).
//!
//! P2 scope: voxel bodies against a terrain [`VoxelField`] — sphere-skin
//! narrowphase, sequential-impulse velocity solve with Coulomb
//! friction, full-K NGS position correction, persistent warm-start
//! cache. P1's free-body integration and P0's deterministic shell
//! carry over unchanged.

use monada_fixed::{Fixed, FixedQuat, FixedVec3};
use monada_sim::{StateHash, StateHasher};

use crate::body::{BodyDef, RigidBody, VoxelBodyDef};
use crate::contact::{self, ContactCacheEntry};
use crate::field::VoxelField;
use crate::ids::BodyId;
use crate::material::{Material, MaterialId};
use crate::solver;
use crate::wheels::{self, Wheel, WheelDef, WheelId, WheelInput};

/// Default speed ceiling: 2000 voxels/s = 80 voxels/tick at 25 Hz.
const DEFAULT_MAX_SPEED: Fixed = Fixed::from_int(2000);

/// Sim-side rest thresholds: a body whose speeds sit below these is
/// "at rest" for acceptance purposes (actual sleeping arrives with
/// islands in P5). Linear in voxels/s, angular in rad/s.
pub const SLEEP_LINEAR: Fixed = Fixed::from_ratio(1, 8);
/// See [`SLEEP_LINEAR`].
pub const SLEEP_ANGULAR: Fixed = Fixed::from_ratio(1, 8);

/// The physics state root. One instance per sim; stepped once per sim
/// tick, hashed into the same desync digest as the rest of the sim.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhysicsWorld {
    /// Ticks stepped since construction. Part of the hashed state so a
    /// paused world and a stepped-but-empty world cannot collide.
    tick: u64,
    /// The fixed timestep, `1 / sim_hz` (docs/plans/voxel-physics.md:
    /// no `dt` parameter anywhere downstream of construction).
    dt: Fixed,
    /// Uniform gravity, voxels/s² in sim space (z-up — so a falling
    /// world sets a negative z). Defaults to zero: the map opts in.
    gravity: FixedVec3,
    /// Speed ceiling, applied right after gravity each tick. A hashed
    /// config field (no setter until a demo needs one — plan, P2
    /// amendments).
    max_speed: Fixed,
    /// The next id [`spawn`](PhysicsWorld::spawn) hands out. Monotonic,
    /// hashed (rule 3: id allocation is simulation state).
    next_body_id: u64,
    /// All live bodies, ascending by id. Spawn-only until P4, so a
    /// plain push keeps the order; despawn/split must preserve it.
    bodies: Vec<RigidBody>,
    /// Registered materials; `MaterialId` indexes this in registration
    /// order (the cross-crate contract on [`VoxelField`]).
    materials: Vec<Material>,
    /// Warm-start impulse cache, sorted by contact key — canonical by
    /// construction (generation order is lexicographic) and hashed:
    /// accumulated impulses feed the next tick's solve (plan, P2
    /// amendments).
    impulse_cache: Vec<ContactCacheEntry>,
}

impl PhysicsWorld {
    /// A world stepping at `sim_hz` ticks per second (the map's
    /// `sim_hz`; the engine default is 25).
    ///
    /// # Panics
    /// Panics if `sim_hz` is zero.
    #[must_use]
    pub fn new(sim_hz: u32) -> PhysicsWorld {
        assert!(sim_hz > 0, "PhysicsWorld::new: sim_hz must be non-zero");
        PhysicsWorld {
            tick: 0,
            // Lossless: manifest sim_hz is far below i32::MAX.
            dt: Fixed::from_ratio(1, i32::try_from(sim_hz).expect("sim_hz fits i32")),
            gravity: FixedVec3::ZERO,
            max_speed: DEFAULT_MAX_SPEED,
            next_body_id: 0,
            bodies: Vec::new(),
            materials: Vec::new(),
            impulse_cache: Vec::new(),
        }
    }

    /// Ticks stepped since construction.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The fixed timestep in seconds.
    #[must_use]
    pub fn dt(&self) -> Fixed {
        self.dt
    }

    /// Uniform gravity, voxels/s².
    #[must_use]
    pub fn gravity(&self) -> FixedVec3 {
        self.gravity
    }

    /// Set uniform gravity (hashed state — a mid-run change re-keys
    /// the stream, as it must).
    pub fn set_gravity(&mut self, gravity: FixedVec3) {
        self.gravity = gravity;
    }

    /// Register a material; ids are handed out in call order (which is
    /// therefore part of the determinism contract, like body ids).
    ///
    /// # Panics
    /// Panics past 65534 materials (65535 is the shape sentinel).
    pub fn register_material(&mut self, material: Material) -> MaterialId {
        let id = u16::try_from(self.materials.len()).expect("material table fits u16");
        assert!(id != u16::MAX, "material id 65535 is reserved");
        self.materials.push(material);
        MaterialId(id)
    }

    /// Spawn a ghost body — explicit mass properties, **no collision
    /// skin** (it falls through terrain by design; see `BodyDef`).
    ///
    /// # Panics
    /// Panics on an invalid [`BodyDef`] (non-positive mass, inertia
    /// tensor that is not positive-definite).
    pub fn spawn(&mut self, def: &BodyDef) -> BodyId {
        let id = self.next_id();
        self.bodies.push(RigidBody::from_def(id, def));
        id
    }

    /// Spawn a voxel body: mass/CoM/inertia derived from the shape and
    /// material densities (solid-cube convention), surface voxels as
    /// the collision skin. `def.position` places the derived `CoM`.
    ///
    /// # Panics
    /// Panics on an empty shape or a material id the world has not
    /// registered.
    pub fn spawn_voxels(&mut self, def: &VoxelBodyDef) -> BodyId {
        let id = self.next_id();
        self.bodies
            .push(RigidBody::from_voxels(id, def, &self.materials));
        id
    }

    fn next_id(&mut self) -> BodyId {
        let id = BodyId(self.next_body_id);
        self.next_body_id += 1;
        id
    }

    /// The body with `id`, if it is (still) alive.
    #[must_use]
    pub fn body(&self, id: BodyId) -> Option<&RigidBody> {
        self.bodies
            .binary_search_by_key(&id, RigidBody::id)
            .ok()
            .map(|i| &self.bodies[i])
    }

    /// All live bodies, ascending by id — the canonical iteration
    /// order (also the hash fold order).
    #[must_use]
    pub fn bodies(&self) -> &[RigidBody] {
        &self.bodies
    }

    /// Apply a world-frame impulse at the centre of mass: `Δv = J/m`.
    ///
    /// # Panics
    /// Panics on an unknown body id.
    pub fn apply_impulse(&mut self, id: BodyId, impulse: FixedVec3) {
        let body = self.body_mut(id);
        body.linear_velocity += impulse.scale(body.inv_mass);
    }

    /// Apply a world-frame impulse at a world-space `point`:
    /// `Δv = J/m`, `Δω = I⁻¹_world·((point − com) × J)`.
    ///
    /// # Panics
    /// Panics on an unknown body id.
    pub fn apply_impulse_at(&mut self, id: BodyId, impulse: FixedVec3, point: FixedVec3) {
        let index = self
            .bodies
            .binary_search_by_key(&id, RigidBody::id)
            .unwrap_or_else(|_| panic!("apply_impulse_at: no body {id:?}"));
        let inv_inertia = solver::inv_inertia_world(&self.bodies[index]);
        let body = &mut self.bodies[index];
        let r = point - body.position;
        body.linear_velocity += impulse.scale(body.inv_mass);
        body.angular_velocity += inv_inertia * r.cross(impulse);
    }

    fn body_mut(&mut self, id: BodyId) -> &mut RigidBody {
        let index = self
            .bodies
            .binary_search_by_key(&id, RigidBody::id)
            .unwrap_or_else(|_| panic!("no body {id:?}"));
        &mut self.bodies[index]
    }

    /// Attach a raycast wheel to `body` (see the `wheels` module for
    /// the suspension model). Wheel ids are per-body, monotonic, never
    /// reused. Ghost bodies may carry wheels — the suspension raycasts
    /// terrain and needs no collision skin (a hover-cart; useful for
    /// isolating suspension in tests).
    ///
    /// # Panics
    /// Panics on an unknown body id.
    pub fn attach_wheel(&mut self, body: BodyId, def: &WheelDef) -> WheelId {
        let body = self.body_mut(body);
        let id = WheelId(body.next_wheel_id);
        body.next_wheel_id += 1;
        body.wheels.push(Wheel {
            id,
            def: *def,
            input: WheelInput::default(),
        });
        id
    }

    /// Detach a wheel (the P3 lost-wheel acceptance path). The id is
    /// retired, not reused.
    ///
    /// # Panics
    /// Panics on an unknown body id or a wheel that is not attached.
    pub fn detach_wheel(&mut self, body: BodyId, wheel: WheelId) {
        let body = self.body_mut(body);
        let index = body
            .wheel_index(wheel)
            .unwrap_or_else(|| panic!("detach_wheel: no wheel {wheel:?} on body"));
        body.wheels.remove(index);
    }

    /// Set a wheel's control input. Retained (hashed) state — it holds
    /// until the next call, matching the lockstep command model.
    ///
    /// # Panics
    /// Panics on an unknown body id or a wheel that is not attached.
    pub fn set_wheel_input(&mut self, body: BodyId, wheel: WheelId, input: WheelInput) {
        let body = self.body_mut(body);
        let index = body
            .wheel_index(wheel)
            .unwrap_or_else(|| panic!("set_wheel_input: no wheel {wheel:?} on body"));
        body.wheels[index].input = input;
    }

    /// Advance one fixed tick against the terrain `field` (a read-only
    /// per-tick input — physics never owns terrain).
    ///
    /// Pipeline, in canonical order (plan §3, P2 amendments):
    ///
    /// ```text
    /// 1. v += g·dt; v clamped to max_speed
    /// 2. narrowphase: bodies by id → skin spheres by index → cells
    /// 3. warm start from the impulse cache
    /// 4. velocity iterations: normal impulses + Coulomb friction
    /// 5. integrate positions/orientations (P1 integrator, ω constant)
    /// 6. full-K NGS position iterations
    /// 7. rebuild the impulse cache from surviving contacts
    /// ```
    ///
    /// **Overflow policy (rule 5)**: plain wrapping ops past the
    /// clamp. The `max_speed` ceiling (80 voxels/tick at defaults) is
    /// simultaneously the tunnelling bound — a step never moves a body
    /// further than that, so terrain thinner than `max_speed·dt` can
    /// be skipped over. Today's column terrain is solid to the bottom;
    /// for future walls/overhangs the map must pick `max_speed` (or
    /// wall thickness) accordingly. Fast projectiles stay engine-side
    /// raycasts (non-goal).
    pub fn step(&mut self, field: &dyn VoxelField) {
        // 1. Integrate velocities.
        let g_dt = self.gravity.scale(self.dt);
        for body in &mut self.bodies {
            body.linear_velocity = (body.linear_velocity + g_dt).clamp_length_max(self.max_speed);
        }

        // 1½. Wheel pass — before the contact solver, so a chassis
        // bottoming out on a step is still corrected by NGS.
        wheels::wheel_pass(&mut self.bodies, field, &self.materials, g_dt, self.dt);

        // 2. Narrowphase at pre-step poses.
        let mut contacts = contact::generate(&self.bodies, field, &self.materials);

        // Per-body world-frame inverse inertia for this tick's solve.
        let inv_inertias: Vec<_> = self.bodies.iter().map(solver::inv_inertia_world).collect();

        // 3. Load accumulated impulses from the previous tick's cache.
        for c in &mut contacts {
            if let Ok(i) = self.impulse_cache.binary_search_by_key(&c.key, |e| e.key) {
                let e = &self.impulse_cache[i];
                c.accumulated_normal = e.normal_impulse;
                c.accumulated_tangent = e.tangent_impulse;
            }
        }
        solver::prepare(&mut self.bodies, &inv_inertias, &mut contacts);

        // 4. Velocity solve.
        solver::solve_velocities(&mut self.bodies, &inv_inertias, &mut contacts);

        // 5. Integrate poses (P1 integrator; no gyroscopic term).
        for body in &mut self.bodies {
            body.position += body.linear_velocity.scale(self.dt);
            body.orientation = (FixedQuat::from_scaled_axis(body.angular_velocity.scale(self.dt))
                * body.orientation)
                .normalize();
        }

        // 6. Position correction.
        solver::solve_positions(&mut self.bodies, &contacts);

        // 7. Persist accumulated impulses (contacts are in key order —
        // the generation loops iterate x→y→z outermost-first to match
        // `ContactKey`'s derived `Ord` — so the rebuilt cache is
        // sorted by construction).
        self.impulse_cache.clear();
        self.impulse_cache.extend(
            contacts
                .iter()
                .filter(|c| {
                    c.accumulated_normal != Fixed::ZERO
                        || c.accumulated_tangent.0 != Fixed::ZERO
                        || c.accumulated_tangent.1 != Fixed::ZERO
                })
                .map(|c| ContactCacheEntry {
                    key: c.key,
                    normal_impulse: c.accumulated_normal,
                    tangent_impulse: c.accumulated_tangent,
                }),
        );
        // Tripwire for the loop-order ↔ Ord pairing above: the
        // binary_search warm-start lookup silently degrades if this
        // ever breaks (strictly ascending — duplicates are impossible
        // by key construction).
        debug_assert!(
            self.impulse_cache.windows(2).all(|w| w[0].key < w[1].key),
            "impulse cache no longer sorted — generation order diverged from ContactKey::Ord"
        );

        self.tick += 1;
    }

    /// The canonical FNV-1a digest of the full physics state, for the
    /// per-tick desync exchange and the oracle goldens.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = StateHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

impl StateHash for PhysicsWorld {
    /// Canonical fold order: `tick`, `dt`, `gravity`, `max_speed`,
    /// `next_body_id`, bodies in id order, materials in registration
    /// order, the impulse cache in key order (each Vec length-prefixed
    /// by the slice impl). Any change here — including appending a
    /// field — re-keys every `phys@` golden and requires an explicit
    /// bless; keeping the order append-only just makes the diff-time
    /// story legible (old fields keep their positions, the bless
    /// commit points at exactly what grew).
    fn hash(&self, h: &mut StateHasher) {
        self.tick.hash(h);
        self.dt.hash(h);
        self.gravity.hash(h);
        self.max_speed.hash(h);
        self.next_body_id.hash(h);
        self.bodies.hash(h);
        self.materials.hash(h);
        self.impulse_cache.hash(h);
    }
}
