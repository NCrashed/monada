//! Voxel-native rigid-body physics (DESIGN.md §3.6,
//! docs/plans/voxel-physics.md).
//!
//! Rigid bodies own a sim-side voxel occupancy with a fixed-point
//! centre-of-mass, inertia tensor, and linear/angular velocity;
//! collision is a surface-voxel sphere skin against the terrain's
//! `VoxelField` (and skin-vs-skin between bodies); destruction removes
//! voxels sim-side and splits separated clusters into new bodies. All
//! arithmetic is fixed-point (`monada-fixed`); the crate never touches
//! render state — bodies are mirrored to roxlap grids by the engine.
//!
//! Built milestone by milestone (P0–P6, see the plan). Currently at
//! **P1**: free rigid bodies — semi-implicit Euler under uniform
//! gravity, quaternion orientation integration, no collisions — on top
//! of P0's deterministic shell (fixed timestep, canonical
//! [`state_hash`](PhysicsWorld::state_hash), serde snapshots, the
//! `phys@` oracle golden gating it in CI).
//!
//! ## Units
//!
//! Sim space throughout: positions in voxels (z-up), linear velocity
//! voxels/s, angular velocity radians/s (world frame), gravity
//! voxels/s², `dt` seconds. Mass units are map-defined — only ratios
//! reach the dynamics.

#![forbid(unsafe_code)]
// The same determinism guardrails as monada-sim (DESIGN.md §3.1):
// float *arithmetic* and hash-ordered containers are hard errors in
// simulation code, not just workspace warnings.
#![deny(clippy::float_arithmetic, clippy::disallowed_types)]

mod body;
mod ids;
mod world;

pub use body::{BodyDef, RigidBody};
pub use ids::BodyId;
pub use world::PhysicsWorld;
