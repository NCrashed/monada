//! Stable identifiers (docs/plans/voxel-physics.md §3 `ids`).

use monada_sim::{StateHash, StateHasher};

/// A rigid body's identity: monotonic, never reused. Allocation order
/// is simulation state (`PhysicsWorld::next_body_id` is hashed), so ids
/// are identical across platforms and replays by construction.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct BodyId(pub u64);

impl StateHash for BodyId {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        self.0.hash(h);
    }
}
