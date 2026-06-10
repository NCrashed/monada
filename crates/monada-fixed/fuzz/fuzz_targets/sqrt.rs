//! `sqrt` invariants over non-negative inputs: non-negativity,
//! monotonicity, and the lower floor bound `r² ≤ x`.
//!
//! (The matching upper bound `(r+ε)² > x` is intentionally *not*
//! asserted: `mul` rounds to nearest, so a true value a hair above `x`
//! can round back down to `x` — that would be a false positive, not a
//! sqrt bug.)

#![no_main]

use libfuzzer_sys::fuzz_target;
use monada_fixed::Fixed;

/// Two non-negative `Fixed` from the input, smallest first. Magnitudes
/// are kept below 2^40 raw so the `r * r` check can't overflow the i64
/// product back into range.
fn ordered_pair(data: &[u8]) -> Option<(Fixed, Fixed)> {
    if data.len() < 16 {
        return None;
    }
    let mask = (1i64 << 40) - 1;
    let x = Fixed::from_bits(i64::from_le_bytes(data[0..8].try_into().unwrap()) & mask);
    let y = Fixed::from_bits(i64::from_le_bytes(data[8..16].try_into().unwrap()) & mask);
    Some((x.min(y), x.max(y)))
}

fuzz_target!(|data: &[u8]| {
    let Some((lo, hi)) = ordered_pair(data) else {
        return;
    };

    let r_lo = lo.sqrt();
    let r_hi = hi.sqrt();

    // Non-negative, and monotonic non-decreasing.
    assert!(r_lo >= Fixed::ZERO);
    assert!(r_lo <= r_hi, "sqrt not monotonic: sqrt({lo:?})={r_lo:?} > sqrt({hi:?})={r_hi:?}");

    // Floor bound: r² ≤ x. checked_mul so a genuine overflow surfaces as
    // a failed invariant rather than a silent wrap.
    let sq = r_lo.checked_mul(r_lo).expect("r*r fits for masked input");
    assert!(sq <= lo, "sqrt({lo:?})²={sq:?} exceeds input");
});
