//! Saved games — a canonical byte image of the mutable simulation
//! (docs/plans/desert-game.md §3d).
//!
//! monada could always *replay* a match (seed + inputs) but never resume
//! one. Replays are the wrong shape for a campaign: a player who quits at
//! minute forty wants to come back to minute forty, not re-simulate it.
//!
//! A snapshot is the state that a tick changes; everything a map *builds*
//! — models, terrain paint, HUD — is rebuilt by running the map's `init`,
//! which a host does anyway when it loads the map. So the restore path is
//! the same one a save-game menu walks: **load the map, run `init`, then
//! restore**, at which point the snapshot's world replaces whatever `init`
//! spawned, RNG position included.
//!
//! The format is postcard (the workspace's wire format already) behind an
//! explicit [`SNAPSHOT_VERSION`], so a blob from another build is refused
//! rather than misread.
//!
//! **What is not in here yet.** The column [`VoxelStore`](crate::VoxelStore)
//! is owned by the render bridge rather than by this crate (the
//! bridge-owned-determinism-state smell `docs/plans/rts-demo.md` flagged),
//! so a *column* map's in-play terrain edits — a felled tree, a razed
//! footprint — are not captured. Volume maps, which is what the desert
//! game is (decision L5), keep their terrain in the hashed
//! [`VolumeStore`](crate::VolumeStore) and are unaffected once the driver
//! folds it in. Moving that store into the runtime is the next step and a
//! format bump.

use monada_sim::World;
use serde::{Deserialize, Serialize};

/// The snapshot format's version. Bumped whenever the blob's shape or
/// meaning changes; a mismatch is refused loudly.
pub const SNAPSHOT_VERSION: u16 = 1;

/// The wire shape of a save.
#[derive(Serialize, Deserialize)]
pub(crate) struct Blob {
    pub(crate) version: u16,
    pub(crate) world: World,
    /// [`MapRules::snapshot`](crate::MapRules::snapshot) — the map's own
    /// hashed state, opaque to the engine.
    pub(crate) rules: Vec<u8>,
}

/// Encode a snapshot.
pub(crate) fn encode(world: &World, rules: Vec<u8>) -> Result<Vec<u8>, String> {
    let blob = Blob {
        version: SNAPSHOT_VERSION,
        world: world.clone(),
        rules,
    };
    postcard::to_stdvec(&blob).map_err(|e| format!("encoding a snapshot failed: {e}"))
}

/// Decode a snapshot, refusing a blob this build cannot read.
pub(crate) fn decode(bytes: &[u8]) -> Result<Blob, String> {
    let blob: Blob =
        postcard::from_bytes(bytes).map_err(|e| format!("this is not a monada snapshot: {e}"))?;
    if blob.version != SNAPSHOT_VERSION {
        return Err(format!(
            "snapshot is version {}; this build reads version {SNAPSHOT_VERSION}",
            blob.version
        ));
    }
    Ok(blob)
}
