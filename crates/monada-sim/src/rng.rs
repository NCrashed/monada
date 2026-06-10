//! The single seeded PRNG advanced only inside the simulation
//! (DESIGN.md §3.1). No `thread_rng`, no OS entropy — the seed is part
//! of the replay file, so every client re-derives the same stream.
//!
//! Generator: **xoshiro256\*\***, seeded by **SplitMix64**. Both are
//! integer-only and identical on every target. Sub-streams are split
//! off as a pure function of `(seed, stream_id)` (see [`DeterministicRng::fork`])
//! so a script that draws from its own stream is insensitive to how
//! far the main stream has advanced.

use monada_fixed::Fixed;

use crate::hash::{StateHash, StateHasher};

/// SplitMix64 — used both to expand a `u64` seed into xoshiro's 256-bit
/// state and to mix fork keys.
#[inline]
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[inline]
fn rotl(x: u64, k: u32) -> u64 {
    x.rotate_left(k)
}

/// A deterministic xoshiro256\*\* generator.
///
/// Carries its 256-bit working state plus the original `seed` so a
/// [`DeterministicRng::fork`] is reproducible regardless of stream
/// position. `serde`-serializable so it round-trips inside a replay
/// snapshot.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeterministicRng {
    seed: u64,
    s: [u64; 4],
}

impl DeterministicRng {
    /// Seed the generator from a single `u64`. The seed is expanded
    /// through SplitMix64; an all-zero state (which xoshiro cannot
    /// escape) is impossible because SplitMix64 never emits four zeros
    /// in a row from a single counter.
    #[must_use]
    pub fn seed_from_u64(seed: u64) -> DeterministicRng {
        let mut sm = seed;
        let s = [
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
        ];
        DeterministicRng { seed, s }
    }

    /// Derive an independent sub-stream keyed by `stream_id`.
    ///
    /// Pure in `(self.seed, stream_id)` — it does **not** consume the
    /// parent stream — so the order in which forks are taken across a
    /// tick does not affect any individual sub-stream (DESIGN.md §3.1).
    ///
    /// Forks are *the* mechanism that makes script execution order not
    /// matter, so the combined key is mixed deliberately hard: the two
    /// inputs are folded with distinct odd multipliers (so adjacent
    /// `seed`s and adjacent `stream_id`s both diverge across the whole
    /// 64-bit word, not just one half), then run through several
    /// SplitMix64 rounds before `seed_from_u64` expands it into state.
    /// Cheap insurance against correlated sub-streams for neighbouring
    /// `(seed, stream_id)` pairs.
    #[must_use]
    pub fn fork(&self, stream_id: u64) -> DeterministicRng {
        let mut key = self
            .seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(stream_id.wrapping_mul(0xD1B5_4A32_D192_ED03))
            ^ 0xA076_1D64_78BD_642F;
        // Several rounds: each advances `key` and returns a mixed word;
        // the last return value is the well-decorrelated fork seed.
        splitmix64(&mut key);
        splitmix64(&mut key);
        let seed = splitmix64(&mut key);
        DeterministicRng::seed_from_u64(seed)
    }

    /// Next raw 64-bit output, advancing the state.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let result = rotl(self.s[1].wrapping_mul(5), 7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = rotl(self.s[3], 45);
        result
    }

    /// Next 32-bit output (the high, best-mixed bits of [`Self::next_u64`]).
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Integer in `[0, bound)` via Lemire's multiply-shift.
    ///
    /// **Slightly biased**: this is the fast variant *without* the
    /// rejection step, so bounds that don't divide `2^64` favour the low
    /// end by ~`bound / 2^64`. Utterly negligible at sim scale (a few
    /// thousand units, dice rolls), and it is deterministic — which is
    /// all lockstep needs. Do **not** reuse it anywhere correctness
    /// depends on uniformity (shuffles for fairness, anything
    /// crypto-adjacent); add the rejection loop there.
    ///
    /// # Panics
    /// Panics if `bound == 0`.
    #[inline]
    pub fn gen_below(&mut self, bound: u64) -> u64 {
        assert!(bound != 0, "gen_below: bound must be non-zero");
        ((u128::from(self.next_u64()) * u128::from(bound)) >> 64) as u64
    }

    /// A [`Fixed`] uniformly in `[0, 1)`.
    ///
    /// The top 32 bits of a draw become the Q32.32 fractional part, so
    /// the value lands in `[0, 1)` exactly with no float in sight.
    ///
    /// **Precision:** one 32-bit draw fills all 32 fractional bits, so
    /// the result already has the finest resolution Q32.32 can hold
    /// (`2^-32`) — no entropy is wasted and none is missing. Note this
    /// is coarser than an `f64` in `[0, 1)` (52-bit mantissa): callers
    /// who reason in floating-point intuition should not expect more
    /// than ~9 significant decimal digits here.
    #[inline]
    pub fn next_fixed_01(&mut self) -> Fixed {
        Fixed::from_bits(i64::from(self.next_u32()))
    }

    /// Coin flip.
    #[inline]
    pub fn next_bool(&mut self) -> bool {
        self.next_u64() >> 63 == 1
    }
}

impl StateHash for DeterministicRng {
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(self.seed);
        for word in self.s {
            h.write_u64(word);
        }
    }
}
