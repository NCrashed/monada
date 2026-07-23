//! [`PhysicsWorld`] — the physics state root (docs/plans/voxel-physics.md
//! §3 `world`).
//!
//! P1 scope: free rigid bodies — semi-implicit Euler under uniform
//! gravity, quaternion orientation integration, no collisions. The
//! deterministic shell from P0 (fixed timestep, canonical hash, serde
//! snapshot) carries over unchanged.

use monada_fixed::{Fixed, FixedQuat, FixedVec3};
use monada_sim::{StateHash, StateHasher};

use crate::body::{BodyDef, RigidBody};
use crate::ids::BodyId;

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
    /// The next id [`spawn`](PhysicsWorld::spawn) hands out. Monotonic,
    /// hashed (rule 3: id allocation is simulation state).
    next_body_id: u64,
    /// All live bodies, ascending by id. Spawn-only in P1, so a plain
    /// push keeps the order; despawn/split (P4) must preserve it.
    bodies: Vec<RigidBody>,
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
            next_body_id: 0,
            bodies: Vec::new(),
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

    /// Spawn a rigid body. Ids are handed out monotonically, in call
    /// order — which is therefore part of the determinism contract.
    ///
    /// # Panics
    /// Panics on an invalid [`BodyDef`] (non-positive mass, inertia
    /// tensor that is not positive-definite).
    pub fn spawn(&mut self, def: &BodyDef) -> BodyId {
        let id = BodyId(self.next_body_id);
        self.next_body_id += 1;
        self.bodies.push(RigidBody::from_def(id, def));
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

    /// Advance one fixed tick: semi-implicit Euler over every body.
    ///
    /// Per body (plan §6 P1, amendments):
    ///
    /// ```text
    /// v += g·dt                                   (velocity first —
    /// p += v·dt                                    that is the "semi-
    /// q  = (from_scaled_axis(ω·dt) * q).normalize  implicit" part)
    /// ω  unchanged (no gyroscopic term in v1)
    /// ```
    ///
    /// **Overflow policy (rule 5)**: plain wrapping ops, deliberately.
    /// In free fall `v` grows without bound, but the Q32.32 ceiling is
    /// ±2³¹ ≈ 2.1e9 voxels/s — at |g| = 10 that is nearly seven years
    /// of continuous falling, and position wraps on the same scale.
    /// Real scenarios hit terrain (P2) first; a velocity clamp is P2's
    /// concern alongside the solver. `checked_*` here would buy a
    /// branch per component per tick for a regime no scenario reaches.
    pub fn step(&mut self) {
        for body in &mut self.bodies {
            body.linear_velocity += self.gravity.scale(self.dt);
            body.position += body.linear_velocity.scale(self.dt);
            body.orientation = (FixedQuat::from_scaled_axis(body.angular_velocity.scale(self.dt))
                * body.orientation)
                .normalize();
        }
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
    /// Canonical fold order: `tick`, `dt`, `gravity`, `next_body_id`,
    /// then the bodies in id order (length-prefixed by the `Vec`
    /// impl). Any change here — including appending a field — re-keys
    /// every `phys@` golden and requires an explicit bless; keeping
    /// the order append-only just makes the diff-time story legible
    /// (old fields keep their positions, the bless commit points at
    /// exactly what grew).
    fn hash(&self, h: &mut StateHasher) {
        self.tick.hash(h);
        self.dt.hash(h);
        self.gravity.hash(h);
        self.next_body_id.hash(h);
        self.bodies.hash(h);
    }
}
