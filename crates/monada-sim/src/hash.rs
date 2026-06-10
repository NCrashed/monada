//! Canonical state hashing for desync detection (DESIGN.md §3.1).
//!
//! Every Nth tick each client hashes its sim state and broadcasts the
//! digest; a mismatch halts the match and dumps for diff. The hash
//! must therefore be a *canonical* walk — fixed field order, no
//! `HashMap` iteration, no float bit patterns — so two bit-identical
//! states always hash equal and two divergent states (almost) never do.
//!
//! Algorithm: **FNV-1a 64-bit**. Cheap, allocation-free, order-
//! sensitive (which is what we want — field order is part of the
//! canonical form). xxhash is the documented upgrade if throughput
//! ever matters; the [`StateHash`] trait keeps callers agnostic.

/// 64-bit FNV-1a accumulator.
#[derive(Clone, Debug)]
pub struct StateHasher {
    hash: u64,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

impl StateHasher {
    #[must_use]
    pub fn new() -> StateHasher {
        StateHasher { hash: FNV_OFFSET }
    }

    #[inline]
    pub fn write_u8(&mut self, b: u8) {
        self.hash ^= u64::from(b);
        self.hash = self.hash.wrapping_mul(FNV_PRIME);
    }

    #[inline]
    pub fn write_u64(&mut self, v: u64) {
        for b in v.to_le_bytes() {
            self.write_u8(b);
        }
    }

    #[inline]
    pub fn write_i64(&mut self, v: i64) {
        self.write_u64(v as u64);
    }

    /// Fold a raw byte slice in, in order. The low-level primitive for
    /// hashing opaque blobs — map archives, asset bytes (`monada-format`).
    /// Callers that need collision resistance between adjacent fields
    /// should length-prefix themselves (the [`StateHash`] impl for
    /// slices already does); this writes the bytes verbatim.
    #[inline]
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u8(b);
        }
    }

    /// Finalize and return the digest.
    #[inline]
    #[must_use]
    pub fn finish(&self) -> u64 {
        self.hash
    }
}

impl Default for StateHasher {
    fn default() -> StateHasher {
        StateHasher::new()
    }
}

/// A sim type that can be folded into a [`StateHasher`] in canonical
/// order. Implementations must be deterministic and field-order-stable.
pub trait StateHash {
    fn hash(&self, h: &mut StateHasher);
}

impl StateHash for u64 {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(*self);
    }
}

impl StateHash for i64 {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        h.write_i64(*self);
    }
}

impl StateHash for u32 {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(u64::from(*self));
    }
}

impl StateHash for bool {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        h.write_u8(u8::from(*self));
    }
}

impl StateHash for monada_fixed::Fixed {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        h.write_i64(self.to_bits());
    }
}

impl StateHash for monada_fixed::FixedVec3 {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        self.x.hash(h);
        self.y.hash(h);
        self.z.hash(h);
    }
}

impl StateHash for monada_fixed::FixedVec2 {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        self.x.hash(h);
        self.y.hash(h);
    }
}

impl<T: StateHash> StateHash for [T] {
    /// Length-prefixed so `[a]` and `[a, a]` cannot collide.
    fn hash(&self, h: &mut StateHasher) {
        h.write_u64(self.len() as u64);
        for item in self {
            item.hash(h);
        }
    }
}

impl<T: StateHash> StateHash for Vec<T> {
    #[inline]
    fn hash(&self, h: &mut StateHasher) {
        self.as_slice().hash(h);
    }
}
