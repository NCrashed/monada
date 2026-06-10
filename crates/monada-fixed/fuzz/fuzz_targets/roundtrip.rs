//! Algebraic invariants that must hold *exactly* for every bit pattern:
//! bit round-trip, additive identity/inverse, and commutativity of the
//! two operations whose rounding is order-independent.

#![no_main]

use libfuzzer_sys::fuzz_target;
use monada_fixed::Fixed;

/// Split `data` into the first two `i64`s, or `None` if too short.
fn pair(data: &[u8]) -> Option<(i64, i64)> {
    if data.len() < 16 {
        return None;
    }
    let a = i64::from_le_bytes(data[0..8].try_into().unwrap());
    let b = i64::from_le_bytes(data[8..16].try_into().unwrap());
    Some((a, b))
}

fuzz_target!(|data: &[u8]| {
    let Some((ra, rb)) = pair(data) else { return };
    let a = Fixed::from_bits(ra);
    let b = Fixed::from_bits(rb);

    // Bit round-trip is the identity.
    assert_eq!(Fixed::from_bits(a.to_bits()), a);

    // Additive identity / inverse, defined on every input (wrapping).
    assert_eq!(a + Fixed::ZERO, a);
    assert_eq!(a - a, Fixed::ZERO);
    assert_eq!(-(-a), a);

    // Commutativity: add wraps identically either way; mul rounds
    // symmetrically so order can't matter.
    assert_eq!(a + b, b + a);
    assert_eq!(a * b, b * a);

    // Multiplicative identity / annihilator.
    assert_eq!(a * Fixed::ONE, a);
    assert_eq!(a * Fixed::ZERO, Fixed::ZERO);
});
