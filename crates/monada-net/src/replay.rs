//! Replays (DESIGN.md §3.1, M3).
//!
//! A replay is the whole match compressed to its inputs:
//! `(seed, map hash, engine version, ordered input stream)`. Re-running
//! the same seed through the same ordered commands on any platform
//! reproduces bit-identical state — that is the determinism guarantee,
//! turned into a file. Spectating is the same file played live.
//!
//! Only *executed* bundles are recorded (each tagged with the tick it
//! ran on, in execution order), so [`Replay::playback`] can re-apply
//! them through any [`SimDriver`] with no transport and no lockstep
//! scheduling involved.

use std::collections::BTreeMap;

use std::fmt;

use monada_sim::PlayerId;
use serde::{Deserialize, Serialize};

use crate::session::SimDriver;
use crate::wire::InputBundle;

/// A recorded match: identity metadata plus the ordered input stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replay {
    /// The world RNG seed both peers started from.
    pub seed: u64,
    /// Hash of the map/script that defines the rules (see [`map_hash`]).
    /// A replay only reproduces against the same map.
    pub map_hash: u64,
    /// Engine version string; replays are not guaranteed across versions.
    pub engine_version: String,
    /// Command delay the match ran with (metadata; playback does not need
    /// it since recorded bundles are already at their execution ticks).
    pub command_delay: u64,
    /// Executed input bundles, in execution order (per tick, sorted by
    /// player).
    pub frames: Vec<InputBundle>,
}

impl Replay {
    /// A fresh, empty replay carrying the match's identity metadata.
    #[must_use]
    pub fn new(seed: u64, map_hash: u64, engine_version: String, command_delay: u64) -> Replay {
        Replay {
            seed,
            map_hash,
            engine_version,
            command_delay,
            frames: Vec::new(),
        }
    }

    /// Append one executed bundle (called by the session as each tick
    /// runs).
    pub fn push(&mut self, frame: InputBundle) {
        self.frames.push(frame);
    }

    /// Verify the replay's identity against the current build, then play
    /// it back. The whole point of storing `map_hash` + `engine_version`
    /// is to **fail loudly on a wrong map/version rather than desync
    /// silently** (DESIGN.md §3.4): a replay only reproduces against the
    /// map it was recorded on. The caller must also have seeded `driver`'s
    /// world to [`self.seed`](Replay::seed).
    ///
    /// # Errors
    /// Returns [`ReplayError::MapMismatch`] / [`ReplayError::VersionMismatch`]
    /// before touching the driver if the identity does not match.
    pub fn playback_verified<D: SimDriver>(
        &self,
        driver: &mut D,
        expected_map_hash: u64,
        engine_version: &str,
    ) -> Result<u64, ReplayError> {
        if self.map_hash != expected_map_hash {
            return Err(ReplayError::MapMismatch {
                expected: expected_map_hash,
                found: self.map_hash,
            });
        }
        if self.engine_version != engine_version {
            return Err(ReplayError::VersionMismatch {
                expected: engine_version.to_string(),
                found: self.engine_version.clone(),
            });
        }
        Ok(self.playback(driver))
    }

    /// Re-run the recorded inputs through a fresh `driver` (seeded to
    /// `self.seed` by the caller) and return its final state hash. This is
    /// the **unverified** building block — prefer
    /// [`playback_verified`](Replay::playback_verified) for loading a
    /// replay from an untrusted source.
    ///
    /// Frames are grouped by tick and applied in the canonical
    /// sorted-by-[`PlayerId`] order — identical to live execution — so
    /// the result is bit-exact with the original run.
    pub fn playback<D: SimDriver>(&self, driver: &mut D) -> u64 {
        let mut by_tick: BTreeMap<u64, BTreeMap<PlayerId, &InputBundle>> = BTreeMap::new();
        for frame in &self.frames {
            by_tick
                .entry(frame.tick)
                .or_default()
                .insert(frame.player, frame);
        }
        for (_tick, players) in by_tick {
            for (&player, frame) in &players {
                for command in &frame.commands {
                    driver.apply_command(player, command);
                }
            }
            driver.step();
        }
        driver.state_hash()
    }

    /// Encode this replay to bytes for an on-disk `.replay` file.
    ///
    /// # Errors
    /// Propagates a [`postcard`] serialisation failure.
    pub fn encode(&self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// Decode a replay produced by [`encode`](Self::encode).
    ///
    /// # Errors
    /// Returns a [`postcard`] error on malformed input.
    pub fn decode(bytes: &[u8]) -> Result<Replay, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}

/// Why a replay can't be trusted against the current build — surfaced
/// by [`Replay::playback_verified`] instead of letting playback diverge
/// silently (DESIGN.md §3.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// The replay was recorded against a different map/script.
    MapMismatch { expected: u64, found: u64 },
    /// The replay was recorded by a different engine version.
    VersionMismatch { expected: String, found: String },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplayError::MapMismatch { expected, found } => write!(
                f,
                "replay map mismatch: expected {expected:#018x}, replay is {found:#018x}"
            ),
            ReplayError::VersionMismatch { expected, found } => write!(
                f,
                "replay engine-version mismatch: expected {expected:?}, replay is {found:?}"
            ),
        }
    }
}

impl std::error::Error for ReplayError {}

/// Hash a map/script source into a [`Replay::map_hash`] (FNV-1a 64).
/// Until the tar.zst map archive lands (M4) the "map" is the script
/// text, so hashing the source pins the ruleset a replay belongs to.
///
/// FNV-1a is an **interim** stand-in: it is fine for catching an honest
/// wrong-map mistake, but it is **not collision-resistant**, so it must
/// not be the trust boundary for *untrusted* replays (anti-cheat,
/// DESIGN.md §10.2). When the map archive lands this moves to the
/// SHA-256-of-archive that DESIGN.md §3.4 specifies.
#[must_use]
pub fn map_hash(source: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in source.as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(PRIME);
    }
    h
}
