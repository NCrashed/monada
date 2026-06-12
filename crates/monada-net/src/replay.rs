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

use monada_sim::{Command, PlayerId};
use serde::{Deserialize, Serialize};

use crate::session::SimDriver;
use crate::wire::InputBundle;

/// Recorded input regrouped per executed tick into canonical per-player
/// order: `(tick, [(player, commands)])`. Produced by [`Replay::steps`] and
/// consumed by both playback and a paced replay viewer.
pub type ReplaySteps = Vec<(u64, Vec<(PlayerId, Vec<Command>)>)>;

/// A recorded match: identity metadata plus the ordered input stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Replay {
    /// The world RNG seed both peers started from.
    pub seed: u64,
    /// SHA-256 identity of the map/script that defines the rules
    /// (`monada_format::hash`, DESIGN.md §3.4). A replay only reproduces
    /// against the same map.
    pub map_hash: [u8; 32],
    /// Engine version string; replays are not guaranteed across versions.
    pub engine_version: String,
    /// Command delay the match ran with (metadata; playback does not need
    /// it since recorded bundles are already at their execution ticks).
    pub command_delay: u64,
    /// Total ticks executed. Playback steps the sim this many times, so a
    /// per-tick sim reproduces exactly even though only **command-bearing**
    /// ticks are stored in `frames` — the empty ticks between moves are
    /// re-run, not recorded (DESIGN.md §3.1).
    pub ticks: u64,
    /// Executed **non-empty** input bundles, in execution order (per tick,
    /// sorted by player). Idle ticks are not stored.
    pub frames: Vec<InputBundle>,
}

impl Replay {
    /// A fresh, empty replay carrying the match's identity metadata.
    #[must_use]
    pub fn new(
        seed: u64,
        map_hash: [u8; 32],
        engine_version: String,
        command_delay: u64,
    ) -> Replay {
        Replay {
            seed,
            map_hash,
            engine_version,
            command_delay,
            ticks: 0,
            frames: Vec::new(),
        }
    }

    /// Record one executed tick: store each player's **non-empty** command
    /// bundle (idle ticks add nothing but the tick count). Called by the
    /// session after it executes a tick.
    pub fn record(&mut self, executed: u64, commands: &[(PlayerId, Vec<Command>)]) {
        for (player, list) in commands {
            if !list.is_empty() {
                self.frames.push(InputBundle {
                    tick: executed,
                    player: *player,
                    commands: list.clone(),
                });
            }
        }
        self.ticks = executed + 1;
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
        expected_map_hash: [u8; 32],
        engine_version: &str,
    ) -> Result<u64, ReplayError> {
        self.verify(expected_map_hash, engine_version)?;
        Ok(self.playback(driver))
    }

    /// Check this replay's identity against the current build: same map
    /// hash, same engine version (DESIGN.md §3.4). The single home of the
    /// "fail loud rather than desync silently" gate — every loader (the
    /// host, `monada-chess`, [`playback_verified`](Self::playback_verified))
    /// goes through here so the two checks can never drift apart.
    ///
    /// # Errors
    /// [`ReplayError::MapMismatch`] / [`ReplayError::VersionMismatch`].
    pub fn verify(
        &self,
        expected_map_hash: [u8; 32],
        engine_version: &str,
    ) -> Result<(), ReplayError> {
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
        Ok(())
    }

    /// The recorded input regrouped into the **canonical** per-tick,
    /// per-player order live execution used: `(tick, [(player, commands)])`
    /// ascending by tick, players sorted. The single source of this
    /// grouping — both [`playback`](Self::playback) and a paced viewer
    /// consume it, so a viewer can never diverge from the verified replay.
    #[must_use]
    pub fn steps(&self) -> ReplaySteps {
        let mut by_tick: BTreeMap<u64, BTreeMap<PlayerId, Vec<Command>>> = BTreeMap::new();
        for frame in &self.frames {
            by_tick
                .entry(frame.tick)
                .or_default()
                .entry(frame.player)
                .or_default()
                .extend(frame.commands.iter().copied());
        }
        by_tick
            .into_iter()
            .map(|(tick, players)| (tick, players.into_iter().collect()))
            .collect()
    }

    /// Re-run the recorded inputs through a fresh `driver` (seeded to
    /// `self.seed` by the caller) and return its final state hash. This is
    /// the **unverified** building block — prefer
    /// [`playback_verified`](Replay::playback_verified) for loading a
    /// replay from an untrusted source.
    ///
    /// Steps the sim for **every** executed tick `0..ticks`, applying the
    /// recorded command bundles (in canonical sorted-by-[`PlayerId`] order)
    /// at the ticks that have them and re-running the idle ticks in
    /// between — identical to live execution, so the result is bit-exact
    /// with the original run.
    pub fn playback<D: SimDriver>(&self, driver: &mut D) -> u64 {
        let mut next = 0u64;
        for (tick, players) in self.steps() {
            // Re-run the idle ticks between recorded moves (not stored).
            while next < tick {
                driver.step();
                next += 1;
            }
            for (player, commands) in &players {
                for command in commands {
                    driver.apply_command(*player, command);
                }
            }
            driver.step();
            next += 1;
        }
        // Trailing idle ticks (e.g. between the last move and quit).
        while next < self.ticks {
            driver.step();
            next += 1;
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
    MapMismatch { expected: [u8; 32], found: [u8; 32] },
    /// The replay was recorded by a different engine version.
    VersionMismatch { expected: String, found: String },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplayError::MapMismatch { expected, found } => write!(
                f,
                "replay map mismatch: expected {}, replay is {}",
                ShortHash(expected),
                ShortHash(found)
            ),
            ReplayError::VersionMismatch { expected, found } => write!(
                f,
                "replay engine-version mismatch: expected {expected:?}, replay is {found:?}"
            ),
        }
    }
}

impl std::error::Error for ReplayError {}

/// The first few bytes of a map hash, hex, for error messages — enough to
/// tell two maps apart without dumping all 32 bytes.
struct ShortHash<'a>(&'a [u8; 32]);

impl fmt::Display for ShortHash<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in &self.0[..6] {
            write!(f, "{byte:02x}")?;
        }
        f.write_str("…")
    }
}
