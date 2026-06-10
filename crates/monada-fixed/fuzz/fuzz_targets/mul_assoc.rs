//! Multiplication is not bit-exactly associative (it rounds to nearest
//! each step), but `(a·b)·c` and `a·(b·c)` must stay within a small
//! rounding envelope. A gross divergence (a wrapped narrowing, a wrong
//! shift) blows far past the envelope, which is what this catches.

#![no_main]

use libfuzzer_sys::fuzz_target;
use monada_fixed::Fixed;

/// Three `Fixed` with |value| < 16 (raw bounded to 2^36), so the triple
/// product stays well inside i64 and no step can overflow.
fn triple(data: &[u8]) -> Option<(Fixed, Fixed, Fixed)> {
    if data.len() < 24 {
        return None;
    }
    let bound = 16i64 << 32; // raw magnitude ceiling
    let read = |i: usize| {
        let raw = i64::from_le_bytes(data[i..i + 8].try_into().unwrap());
        Fixed::from_bits(raw % bound) // keeps sign, |value| < 16
    };
    Some((read(0), read(8), read(16)))
}

fuzz_target!(|data: &[u8]| {
    let Some((a, b, c)) = triple(data) else {
        return;
    };

    let lhs = a.checked_mul(b).and_then(|ab| ab.checked_mul(c));
    let rhs = b.checked_mul(c).and_then(|bc| a.checked_mul(bc));
    let (Some(lhs), Some(rhs)) = (lhs, rhs) else {
        return; // bounded inputs make this unreachable, but stay safe
    };

    // Loose sanity envelope (~6e-8), not a tight error proof: the two
    // groupings differ only by intermediate rounding (≤ a few ulps for
    // |value| < 16). A shift/wrap bug would differ by ≫ 2^32.
    let diff = (lhs.to_bits() - rhs.to_bits()).abs();
    assert!(
        diff <= (1 << 8),
        "mul associativity envelope blown: {lhs:?} vs {rhs:?} (Δ={diff} raw)"
    );
});
