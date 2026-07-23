//! Rigid bodies (docs/plans/voxel-physics.md §3 `body`).
//!
//! P1 scope: a free body — pose, velocities, explicit mass properties.
//! The voxel-derived constructor (mass/inertia summed from occupancy)
//! arrives with `shape` in P2; the explicit [`BodyDef`] path stays as
//! the tests' ground truth for P4's incremental-vs-full recompute
//! check (plan, P1 amendments).
//!
//! ## Units
//!
//! Positions are sim-space voxels (z-up, the `VoxelStore` frame),
//! linear velocity voxels/s, angular velocity radians/s in the world
//! frame, `dt` seconds. Mass is in map-defined units — only ratios
//! reach the dynamics.

use monada_fixed::{Fixed, FixedMat3, FixedQuat, FixedVec3};
use monada_sim::{StateHash, StateHasher};

use crate::ids::BodyId;

/// Spawn-time description of a rigid body. Mass properties are
/// explicit here; see the module docs for why.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BodyDef {
    /// World-space position of the centre of mass, sim-space voxels.
    pub position: FixedVec3,
    /// World-from-body rotation. Normalised at spawn, so drift-level
    /// deviation from unit length is tolerated (and corrected) rather
    /// than rejected.
    pub orientation: FixedQuat,
    /// Voxels per second.
    pub linear_velocity: FixedVec3,
    /// Radians per second, world frame.
    pub angular_velocity: FixedVec3,
    /// Must be positive (asserted at spawn).
    pub mass: Fixed,
    /// Body-space inertia tensor. Must be positive-definite — spawn
    /// asserts Sylvester's criterion on the leading principal minors.
    pub inertia_body: FixedMat3,
}

impl Default for BodyDef {
    /// A unit point-ish body at the origin: unit mass, unit inertia,
    /// identity orientation, at rest.
    fn default() -> BodyDef {
        BodyDef {
            position: FixedVec3::ZERO,
            orientation: FixedQuat::IDENTITY,
            linear_velocity: FixedVec3::ZERO,
            angular_velocity: FixedVec3::ZERO,
            mass: Fixed::ONE,
            inertia_body: FixedMat3::IDENTITY,
        }
    }
}

/// A simulated rigid body. Constructed only through
/// [`PhysicsWorld::spawn`](crate::PhysicsWorld::spawn); read-only from
/// outside the crate.
///
/// **Cache policy** (plan, P1 amendments): `inv_mass` and
/// `inv_inertia_body` are deterministic functions of `mass` /
/// `inertia_body`, computed once at spawn. They ARE serialized (so
/// snapshot → restore → step is bit-equal to step by construction) and
/// are NOT hashed (pure functions of hashed fields). Any future code
/// that mutates mass or inertia — P4 splits — must refresh the caches
/// in the same breath.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RigidBody {
    pub(crate) id: BodyId,
    pub(crate) position: FixedVec3,
    pub(crate) orientation: FixedQuat,
    pub(crate) linear_velocity: FixedVec3,
    pub(crate) angular_velocity: FixedVec3,
    pub(crate) mass: Fixed,
    pub(crate) inertia_body: FixedMat3,
    pub(crate) inv_mass: Fixed,
    pub(crate) inv_inertia_body: FixedMat3,
}

impl RigidBody {
    /// Validate `def` and build the body. See [`BodyDef`] field docs
    /// for the asserted preconditions.
    ///
    /// # Panics
    /// Panics on non-positive mass or a non-positive-definite inertia
    /// tensor.
    pub(crate) fn from_def(id: BodyId, def: &BodyDef) -> RigidBody {
        assert!(
            def.mass > Fixed::ZERO,
            "RigidBody: mass must be positive, got {:?}",
            def.mass
        );
        // Sylvester's criterion: positive leading principal minors.
        // (Physical tensors are symmetric up to per-entry rounding —
        // e.g. an R·D·Rᵀ built in fixed point — so symmetry itself is
        // deliberately not asserted.)
        let i = &def.inertia_body;
        let minor1 = i.x_axis.x;
        let minor2 = i.x_axis.x * i.y_axis.y - i.y_axis.x * i.x_axis.y;
        let minor3 = i.determinant();
        assert!(
            minor1 > Fixed::ZERO && minor2 > Fixed::ZERO && minor3 > Fixed::ZERO,
            "RigidBody: inertia_body must be positive-definite \
             (leading minors {minor1:?}, {minor2:?}, {minor3:?})"
        );
        RigidBody {
            id,
            position: def.position,
            orientation: def.orientation.normalize(),
            linear_velocity: def.linear_velocity,
            angular_velocity: def.angular_velocity,
            mass: def.mass,
            inertia_body: def.inertia_body,
            inv_mass: Fixed::ONE / def.mass,
            inv_inertia_body: def.inertia_body.inverse(),
        }
    }

    #[must_use]
    pub fn id(&self) -> BodyId {
        self.id
    }

    /// Centre-of-mass position, sim-space voxels.
    #[must_use]
    pub fn position(&self) -> FixedVec3 {
        self.position
    }

    /// World-from-body rotation (unit quaternion).
    #[must_use]
    pub fn orientation(&self) -> FixedQuat {
        self.orientation
    }

    /// Voxels per second.
    #[must_use]
    pub fn linear_velocity(&self) -> FixedVec3 {
        self.linear_velocity
    }

    /// Radians per second, world frame.
    #[must_use]
    pub fn angular_velocity(&self) -> FixedVec3 {
        self.angular_velocity
    }

    #[must_use]
    pub fn mass(&self) -> Fixed {
        self.mass
    }

    /// Body-space inertia tensor.
    #[must_use]
    pub fn inertia_body(&self) -> FixedMat3 {
        self.inertia_body
    }
}

impl StateHash for RigidBody {
    /// Canonical fold: `id`, pose, velocities, mass properties. The
    /// `inv_*` caches are excluded — pure functions of hashed fields
    /// (see the cache policy on [`RigidBody`]).
    fn hash(&self, h: &mut StateHasher) {
        self.id.hash(h);
        self.position.hash(h);
        self.orientation.hash(h);
        self.linear_velocity.hash(h);
        self.angular_velocity.hash(h);
        self.mass.hash(h);
        self.inertia_body.hash(h);
    }
}
