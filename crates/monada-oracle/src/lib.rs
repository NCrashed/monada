//! monada determinism harness (DESIGN.md §3.1, §7).
//!
//! Runs the canonical [`CircleSim`] scenario and records its
//! [`state_hash`](monada_sim::Simulation::state_hash) at a fixed set of
//! tick checkpoints. CI runs this on every supported platform and diffs
//! the result against the committed goldens in `monada-hashes.txt`; any
//! mismatch is a determinism regression and halts the build. This is a
//! direct lift of `roxlap-oracle`'s hash-and-diff style.
//!
//! Checkpointing at several tick counts (not just the final one) is the
//! same idea as the periodic desync hash on the wire (DESIGN.md §3.1):
//! it localizes *when* a divergence first appears, which is far more
//! useful than a single end-state mismatch.

use std::fmt::Write as _;

use monada_sim::scenario::CircleSim;
use monada_sim::{advance, Simulation};

/// Tick counts at which the canonical scenario is hashed. Ascending;
/// `0` captures the seeded initial state before any step.
pub const TICK_CHECKPOINTS: &[u64] = &[0, 1, 30, 150, 600];

/// One `(tick, hash)` checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub tick: u64,
    pub hash: u64,
}

impl Checkpoint {
    /// The line key used in `monada-hashes.txt` (e.g. `circle@600`).
    #[must_use]
    pub fn key(&self) -> String {
        format!("circle@{}", self.tick)
    }
}

/// Run the canonical scenario and collect a hash at every checkpoint in
/// [`TICK_CHECKPOINTS`].
#[must_use]
pub fn canonical_checkpoints() -> Vec<Checkpoint> {
    let mut sim = CircleSim::canonical();
    let mut prev = 0;
    let mut out = Vec::with_capacity(TICK_CHECKPOINTS.len());
    for &tick in TICK_CHECKPOINTS {
        debug_assert!(tick >= prev, "TICK_CHECKPOINTS must be ascending");
        advance(&mut sim, tick - prev);
        prev = tick;
        out.push(Checkpoint {
            tick,
            hash: sim.state_hash(),
        });
    }
    out
}

/// Render checkpoints as the on-disk goldens file (`key = hash` lines
/// with a header). The inverse of [`parse_goldens`].
#[must_use]
pub fn render_goldens(checkpoints: &[Checkpoint]) -> String {
    let mut s = String::new();
    s.push_str("# monada determinism goldens — @generated, do not hand-edit.\n");
    s.push_str("# Canonical CircleSim: seed \"MONADA_0\", 100 movers.\n");
    s.push_str("# Regenerate with `cargo run -p monada-oracle -- --bless`.\n");
    for c in checkpoints {
        let _ = writeln!(s, "{} = {}", c.key(), c.hash);
    }
    s
}

/// Parse a goldens file into `(key, hash)` pairs, ignoring blank and
/// `#`-comment lines.
///
/// # Errors
/// Returns the offending line on a malformed entry.
pub fn parse_goldens(text: &str) -> Result<Vec<(String, u64)>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed line (no '='): {line:?}"))?;
        let hash = value
            .trim()
            .parse::<u64>()
            .map_err(|e| format!("bad hash in {line:?}: {e}"))?;
        out.push((key.trim().to_string(), hash));
    }
    Ok(out)
}

/// A single checkpoint's verdict against the goldens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Match,
    Mismatch { golden: u64, got: u64 },
    MissingGolden,
}

/// Diff freshly-computed checkpoints against parsed goldens, in
/// checkpoint order.
#[must_use]
pub fn diff(checkpoints: &[Checkpoint], goldens: &[(String, u64)]) -> Vec<(Checkpoint, Verdict)> {
    checkpoints
        .iter()
        .map(|c| {
            let verdict = match goldens.iter().find(|(k, _)| *k == c.key()) {
                None => Verdict::MissingGolden,
                Some((_, g)) if *g == c.hash => Verdict::Match,
                Some((_, g)) => Verdict::Mismatch {
                    golden: *g,
                    got: c.hash,
                },
            };
            (*c, verdict)
        })
        .collect()
}
