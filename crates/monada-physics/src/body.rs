//! Rigid bodies (docs/plans/voxel-physics.md §3 `body`).
//!
//! Two spawn paths:
//!
//! - [`BodyDef`] — explicit mass properties, **NO COLLISION SKIN**: a
//!   free-flying ghost that never touches terrain. For tests (P1/P4
//!   ground truth) and future lumped functional modules. If you spawn
//!   one and it falls through the floor — that is this, working as
//!   documented.
//! - [`VoxelBodyDef`] — a voxel shape; mass, centre of mass, and
//!   inertia derive from the voxels and material densities, and the
//!   surface voxels become the collision skin.
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
use crate::material::Material;
use crate::shape::{derive_skin, mass_properties, SkinSphere, VoxelShape};
use crate::wheels::{Wheel, WheelId};

/// Spawn-time description of a ghost body (explicit mass properties,
/// no collision skin — see the module docs). The explicit path stays
/// as the tests' ground truth for P4's incremental-vs-full recompute
/// check (plan, P1 amendments).
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

/// Spawn-time description of a voxel body. Mass properties derive
/// from the shape and the world's material table.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VoxelBodyDef {
    /// The body's voxels. Shape-local coordinates are rebased around
    /// the *derived* centre of mass at spawn — so `position` places
    /// the `CoM`, which for an uneven-density body is NOT the geometric
    /// centre of the shape's AABB.
    pub shape: VoxelShape,
    /// World-space position of the (derived) centre of mass.
    pub position: FixedVec3,
    /// World-from-body rotation (normalised at spawn, like `BodyDef`).
    pub orientation: FixedQuat,
    /// Voxels per second.
    pub linear_velocity: FixedVec3,
    /// Radians per second, world frame.
    pub angular_velocity: FixedVec3,
}

/// A simulated rigid body. Constructed only through
/// [`PhysicsWorld::spawn`](crate::PhysicsWorld::spawn) /
/// [`spawn_voxels`](crate::PhysicsWorld::spawn_voxels); read-only from
/// outside the crate.
///
/// **Cache policy** (plan, P1/P2 amendments): `inv_mass`,
/// `inv_inertia_body`, and `skin` are deterministic functions of the
/// hashed fields (`mass`, `inertia_body`, `shape` + the material
/// table), computed once at spawn. They ARE serialized (so snapshot →
/// restore → step is bit-equal to step by construction) and are NOT
/// hashed. Any future code that mutates mass, inertia, or shape — P4
/// splits — must refresh all three in the same breath.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RigidBody {
    pub(crate) id: BodyId,
    pub(crate) position: FixedVec3,
    pub(crate) orientation: FixedQuat,
    pub(crate) linear_velocity: FixedVec3,
    pub(crate) angular_velocity: FixedVec3,
    pub(crate) mass: Fixed,
    pub(crate) inertia_body: FixedMat3,
    /// `None` for ghost bodies (no collision).
    pub(crate) shape: Option<VoxelShape>,
    pub(crate) inv_mass: Fixed,
    pub(crate) inv_inertia_body: FixedMat3,
    /// Derived cache: surface voxels as CoM-relative spheres.
    pub(crate) skin: Vec<SkinSphere>,
    /// Derived cache: the `CoM` in shape-local coordinates (ZERO for
    /// ghosts). The render mirror and wheel authoring both need the
    /// shape→body-frame rebase.
    pub(crate) com_local: FixedVec3,
    /// Attached wheels, ascending by [`WheelId`] (P3). Hashed.
    pub(crate) wheels: Vec<Wheel>,
    /// Next wheel id this body hands out — monotonic, never reused,
    /// hashed (rule 3: id allocation is simulation state).
    pub(crate) next_wheel_id: u32,
    /// Consecutive ticks below the sleep thresholds (P5). Hashed.
    pub(crate) sleep_timer: u32,
    /// Asleep: frozen — skipped by gravity, wheels, integration, and
    /// terrain narrowphase; still present in broadphase so awake
    /// bodies can find (and wake) it through a real contact. Hashed.
    pub(crate) asleep: bool,
    /// Derived cache: max skin-sphere reach from the `CoM`
    /// (`max |offset| + ½`; ZERO for ghosts). Broadphase and
    /// `notify_terrain_edit` bound the body by `position ± this`.
    pub(crate) bounding_radius: Fixed,
}

impl RigidBody {
    /// Shared tail of both spawn paths: validate mass properties,
    /// normalise the orientation, build the caches.
    // A private constructor mirroring the struct's own field list; a
    // params struct would just restate `RigidBody` a third time.
    #[allow(clippy::too_many_arguments)]
    fn build(
        id: BodyId,
        position: FixedVec3,
        orientation: FixedQuat,
        linear_velocity: FixedVec3,
        angular_velocity: FixedVec3,
        mass: Fixed,
        inertia_body: FixedMat3,
        shape: Option<VoxelShape>,
        skin: Vec<SkinSphere>,
        com_local: FixedVec3,
    ) -> RigidBody {
        assert!(
            mass > Fixed::ZERO,
            "RigidBody: mass must be positive, got {mass:?}"
        );
        // Sylvester's criterion via the wide-arithmetic minors — the
        // third minor of a body barely 6 voxels across already wraps
        // Q32.32. (Physical tensors are symmetric up to per-entry
        // rounding — e.g. an R·D·Rᵀ built in fixed point — so symmetry
        // itself is deliberately not asserted.)
        assert!(
            inertia_body.leading_minors_positive(),
            "RigidBody: inertia_body must be positive-definite, got {inertia_body:?}"
        );
        RigidBody {
            id,
            position,
            orientation: orientation.normalize(),
            linear_velocity,
            angular_velocity,
            mass,
            inertia_body,
            shape,
            inv_mass: Fixed::ONE / mass,
            inv_inertia_body: inertia_body.inverse(),
            bounding_radius: skin
                .iter()
                .map(|s| s.offset.length())
                .max()
                .map_or(Fixed::ZERO, |m| m + crate::shape::SKIN_RADIUS),
            skin,
            com_local,
            wheels: Vec::new(),
            next_wheel_id: 0,
            sleep_timer: 0,
            asleep: false,
        }
    }

    /// Ghost body from explicit mass properties (no collision skin).
    ///
    /// # Panics
    /// Panics on non-positive mass or a non-positive-definite inertia
    /// tensor.
    pub(crate) fn from_def(id: BodyId, def: &BodyDef) -> RigidBody {
        RigidBody::build(
            id,
            def.position,
            def.orientation,
            def.linear_velocity,
            def.angular_velocity,
            def.mass,
            def.inertia_body,
            None,
            Vec::new(),
            FixedVec3::ZERO,
        )
    }

    /// Voxel body: mass/CoM/inertia summed from the shape (solid-cube
    /// convention, see `shape`), skin from the surface voxels.
    ///
    /// # Panics
    /// Panics on an empty shape or a material id the world has not
    /// registered.
    pub(crate) fn from_voxels(id: BodyId, def: &VoxelBodyDef, materials: &[Material]) -> RigidBody {
        let props = mass_properties(&def.shape, materials);
        let skin = derive_skin(&def.shape, props.com);
        RigidBody::build(
            id,
            def.position,
            def.orientation,
            def.linear_velocity,
            def.angular_velocity,
            props.mass,
            props.inertia,
            Some(def.shape.clone()),
            skin,
            props.com,
        )
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

    /// The body's voxels (`None` for ghost bodies). Read access for
    /// the engine's render mirror; shape-local frame — remember the
    /// `CoM` rebase (see [`VoxelBodyDef::shape`]).
    #[must_use]
    pub fn shape(&self) -> Option<&VoxelShape> {
        self.shape.as_ref()
    }

    /// Number of collision-skin spheres (surface voxels); 0 for
    /// ghosts.
    #[must_use]
    pub fn skin_len(&self) -> usize {
        self.skin.len()
    }

    /// Attached wheels, ascending by [`WheelId`].
    #[must_use]
    pub fn wheels(&self) -> &[Wheel] {
        &self.wheels
    }

    /// The centre of mass in shape-local coordinates (`ZERO` for
    /// ghosts): body frame = shape frame − this. The seam for the
    /// render mirror's rebase and for authoring wheel anchors from
    /// shape geometry.
    #[must_use]
    pub fn com_in_shape(&self) -> FixedVec3 {
        self.com_local
    }

    /// Asleep (P5): frozen until a real contact, an external mutation,
    /// a gravity change, or a nearby terrain edit wakes it.
    #[must_use]
    pub fn asleep(&self) -> bool {
        self.asleep
    }

    /// Index of `wheel` in the sorted wheel Vec, if attached.
    pub(crate) fn wheel_index(&self, wheel: WheelId) -> Option<usize> {
        self.wheels.binary_search_by_key(&wheel, |w| w.id).ok()
    }
}

impl StateHash for RigidBody {
    /// Canonical fold: `id`, pose, velocities, mass properties, shape,
    /// then wheels + `next_wheel_id` (P3 append). The `inv_*` and
    /// `skin` caches are excluded — pure functions of hashed fields
    /// (see the cache policy on [`RigidBody`]).
    fn hash(&self, h: &mut StateHasher) {
        self.id.hash(h);
        self.position.hash(h);
        self.orientation.hash(h);
        self.linear_velocity.hash(h);
        self.angular_velocity.hash(h);
        self.mass.hash(h);
        self.inertia_body.hash(h);
        match &self.shape {
            None => h.write_u8(0),
            Some(shape) => {
                h.write_u8(1);
                shape.hash(h);
            }
        }
        self.wheels.hash(h);
        h.write_u64(u64::from(self.next_wheel_id));
        h.write_u64(u64::from(self.sleep_timer));
        self.asleep.hash(h);
    }
}
