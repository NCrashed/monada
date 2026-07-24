//! Materials (docs/plans/voxel-physics.md §4, P2).
//!
//! The world owns a material table; ids are handed out monotonically
//! by [`register_material`](crate::PhysicsWorld::register_material)
//! and are part of hashed state (rule 3: id allocation is simulation
//! state). Terrain fields and body voxels reference the same table —
//! see the contract on [`VoxelField`](crate::VoxelField).

use monada_fixed::Fixed;
use monada_sim::{StateHash, StateHasher};

/// Index into the world's material table (registration order).
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct MaterialId(pub u16);

/// Physical properties of one material.
///
/// Contact combination rules: friction `√(μ_a·μ_b)` (so identical
/// materials keep their own μ exactly, up to `sqrt` rounding),
/// restitution `max(e_a, e_b)`, both per contact.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Material {
    /// Mass per voxel — voxels are unit cubes, so density *is* the
    /// per-voxel mass.
    pub density: Fixed,
    /// Coulomb friction coefficient.
    pub friction: Fixed,
    /// Bounciness in [0, 1]; the target feel wants ≈ 0 (plan §6 P2).
    pub restitution: Fixed,
    /// Drill resistance: the reaction force one voxel of this
    /// material exerts while being cut (P6). Intuition beats the
    /// formula here: hardness 100 stops a light vehicle dead,
    /// hardness 10 barely slows it.
    pub hardness: Fixed,
}

impl StateHash for Material {
    fn hash(&self, h: &mut StateHasher) {
        self.density.hash(h);
        self.friction.hash(h);
        self.restitution.hash(h);
        self.hardness.hash(h);
    }
}

impl StateHash for MaterialId {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(u64::from(self.0));
    }
}
