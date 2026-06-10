//! Arithmetic-invariant tests for the Q32.32 core. These guard the
//! determinism contract: defined overflow, fixed rounding, and
//! integer-only trig (DESIGN.md §3.1).

use monada_fixed::trig::{cos, sin, FRAC_PI_2, PI, TAU};
use monada_fixed::{Fixed, FixedVec2, FixedVec3};

/// Assert two `Fixed` are within `eps` raw steps of each other.
fn close(a: Fixed, b: Fixed, eps_bits: i64) {
    let d = (a.to_bits() - b.to_bits()).abs();
    assert!(d <= eps_bits, "‖{a:?} - {b:?}‖ = {d} bits > {eps_bits}");
}

/// A tiny deterministic LCG so sampling tests need no `rand` dep and
/// stay reproducible.
fn lcg(state: &mut u64) -> i32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    (*state >> 33) as i32
}

#[test]
fn additive_and_multiplicative_identities() {
    let mut s = 0x1234_5678;
    for _ in 0..10_000 {
        let a = Fixed::from_bits(i64::from(lcg(&mut s)) << 8 | i64::from(lcg(&mut s)));
        assert_eq!(a + Fixed::ZERO, a);
        assert_eq!(a - Fixed::ZERO, a);
        assert_eq!(a * Fixed::ONE, a);
        assert_eq!(a * Fixed::ZERO, Fixed::ZERO);
        assert_eq!(a - a, Fixed::ZERO);
        assert_eq!(-(-a), a);
    }
}

#[test]
fn add_mul_are_commutative() {
    let mut s = 99;
    for _ in 0..10_000 {
        let a = Fixed::from_bits(i64::from(lcg(&mut s)) << 4);
        let b = Fixed::from_bits(i64::from(lcg(&mut s)) << 4);
        assert_eq!(a + b, b + a);
        assert_eq!(a * b, b * a);
    }
}

#[test]
fn overflow_wraps_not_panics() {
    // Defined behaviour in both debug and release: two's-complement wrap.
    assert_eq!(Fixed::MAX + Fixed::EPSILON, Fixed::MIN);
    assert_eq!(Fixed::MIN - Fixed::EPSILON, Fixed::MAX);
    assert_eq!(Fixed::MAX.checked_add(Fixed::EPSILON), None);
    assert_eq!(
        Fixed::from_int(2).checked_add(Fixed::from_int(3)),
        Some(Fixed::from_int(5))
    );
}

#[test]
fn int_round_trip_and_ordering() {
    for i in -1000..=1000 {
        let f = Fixed::from_int(i);
        assert_eq!(f.floor_to_int(), i);
        assert!((f.to_f64() - f64::from(i)).abs() < 1e-9);
        assert!(f == i);
        assert!(f.partial_cmp(&(i + 1)) == Some(core::cmp::Ordering::Less));
    }
}

#[test]
fn mul_div_are_inverse() {
    let mut s = 7;
    for _ in 0..10_000 {
        let a = Fixed::from_ratio(lcg(&mut s) % 1000, 7);
        let b = Fixed::from_ratio((lcg(&mut s) % 1000) | 1, 13); // never 0
                                                                 // (a / b) * b ≈ a, within a few rounding steps.
        close(a / b * b, a, 1 << 12);
    }
}

#[test]
fn rounding_helpers() {
    let cases = [
        (Fixed::from_ratio(7, 2), 3, 4, 4), // 3.5 -> floor 3, ceil 4, round 4
        (Fixed::from_ratio(5, 2), 2, 3, 3), // 2.5
        (Fixed::from_ratio(-5, 2), -3, -2, -3), // -2.5
        (Fixed::from_int(4), 4, 4, 4),
    ];
    for (v, fl, ce, ro) in cases {
        assert_eq!(v.floor(), Fixed::from_int(fl), "floor {v:?}");
        assert_eq!(v.ceil(), Fixed::from_int(ce), "ceil {v:?}");
        assert_eq!(v.round(), Fixed::from_int(ro), "round {v:?}");
    }
    // fract is always in [0, 1).
    let f = Fixed::from_ratio(-7, 2);
    assert_eq!(f.floor() + f.fract(), f);
    assert!(f.fract() >= Fixed::ZERO && f.fract() < Fixed::ONE);
}

#[test]
fn sqrt_exact_and_approx() {
    assert_eq!(Fixed::from_int(4).sqrt(), Fixed::from_int(2));
    assert_eq!(Fixed::from_int(144).sqrt(), Fixed::from_int(12));
    assert_eq!(Fixed::ZERO.sqrt(), Fixed::ZERO);
    assert_eq!(Fixed::ONE.sqrt(), Fixed::ONE);
    // sqrt(2)^2 ≈ 2; sqrt is monotonic.
    let two = Fixed::from_int(2);
    close(two.sqrt() * two.sqrt(), two, 1 << 12);
    let mut prev = Fixed::ZERO;
    let mut x = Fixed::ZERO;
    for _ in 0..500 {
        x += Fixed::from_ratio(1, 3);
        let r = x.sqrt();
        assert!(r >= prev, "sqrt not monotonic at {x:?}");
        prev = r;
    }
}

#[test]
fn trig_landmarks_and_identity() {
    // Landmark values, within a couple of LUT-lerp steps.
    let eps = 1 << 16; // ~1.5e-5
    close(sin(Fixed::ZERO), Fixed::ZERO, eps);
    close(sin(FRAC_PI_2), Fixed::ONE, eps);
    close(sin(PI), Fixed::ZERO, eps);
    close(cos(Fixed::ZERO), Fixed::ONE, eps);
    close(cos(FRAC_PI_2), Fixed::ZERO, eps);
    close(cos(PI), Fixed::NEG_ONE, eps);

    // sin^2 + cos^2 = 1 across a full turn, including negative angles.
    let mut a = -TAU;
    let step = TAU / Fixed::from_int(360);
    while a < TAU {
        let s = sin(a);
        let c = cos(a);
        close(s * s + c * c, Fixed::ONE, 1 << 18);
        a += step;
    }
}

#[test]
fn trig_periodicity() {
    let mut st = 4242;
    for _ in 0..2000 {
        let a = Fixed::from_ratio(lcg(&mut st) % 100, 7);
        // sin is τ-periodic.
        close(sin(a), sin(a + TAU), 4);
        close(sin(a), sin(a - TAU), 4);
    }
}

#[test]
fn determinism_is_value_stable() {
    // The whole point: the same inputs must produce the same bits, run
    // to run, so this golden is allowed to be exact.
    assert_eq!(
        (Fixed::from_int(3) / Fixed::from_int(7)).to_bits(),
        1_840_700_269
    );
    // 3/7 * 7 lands one step short of 3 under truncating division —
    // exact and identical everywhere, which is what matters.
    assert_eq!(
        (Fixed::from_int(3) / Fixed::from_int(7) * Fixed::from_int(7)).to_bits(),
        Fixed::from_int(3).to_bits() - 5
    );
}

#[test]
fn mul_rounds_to_nearest_without_directional_bias() {
    // 1 ulp * 1.5 = 1.5 ulp. A truncating (toward −∞) shift would give
    // 1; round-to-nearest gives 2. The mirror negative case is the
    // point: truncation drifts toward −∞ (−2), round-to-nearest stays
    // symmetric (−1). This is the bias the choice eliminates.
    let one_and_half = Fixed::from_ratio(3, 2);
    assert_eq!((Fixed::from_bits(1) * one_and_half).to_bits(), 2);
    assert_eq!((Fixed::from_bits(-1) * one_and_half).to_bits(), -1);

    // Identities still hold exactly under rounding.
    let mut s = 1357;
    for _ in 0..10_000 {
        let a = Fixed::from_bits(i64::from(lcg(&mut s)) << 4);
        assert_eq!(a * Fixed::ONE, a);
        assert_eq!(a * Fixed::ZERO, Fixed::ZERO);
    }
}

#[test]
fn rem_is_build_invariant_and_documented_panics() {
    // MIN % -1 is the classic divergence: panic in debug, wrap in
    // release for a bare `%`. wrapping_rem makes it a defined 0.
    assert_eq!(Fixed::MIN % Fixed::NEG_ONE, Fixed::ZERO);
    // Ordinary remainder shares the dividend's sign.
    assert_eq!(Fixed::from_int(7) % Fixed::from_int(3), Fixed::ONE);
    assert_eq!(Fixed::from_int(-7) % Fixed::from_int(3), Fixed::NEG_ONE);
    // 7 mod 2.5 = 2.0 (7 = 2·2.5 + 2.0).
    assert_eq!(
        Fixed::from_int(7) % Fixed::from_ratio(5, 2),
        Fixed::from_int(2)
    );
    // checked variants surface the zero divisor instead of panicking.
    assert_eq!(Fixed::ONE.checked_rem(Fixed::ZERO), None);
    assert_eq!(Fixed::MIN.checked_rem(Fixed::NEG_ONE), Some(Fixed::ZERO));
}

#[test]
fn checked_div_surfaces_zero_and_overflow() {
    assert_eq!(Fixed::ONE.checked_div(Fixed::ZERO), None);
    assert_eq!(
        Fixed::from_int(6).checked_div(Fixed::from_int(2)),
        Some(Fixed::from_int(3))
    );
    // MAX / EPSILON overflows the i64 narrowing → None (vs. a silent
    // wrap from the panicking `/`).
    assert_eq!(Fixed::MAX.checked_div(Fixed::EPSILON), None);
}

#[test]
fn vec2_geometry() {
    let v = FixedVec2::new(Fixed::from_int(3), Fixed::from_int(4));
    assert_eq!(v.length_squared(), Fixed::from_int(25));
    assert_eq!(v.length(), Fixed::from_int(5)); // exact 3-4-5
    let w = FixedVec2::new(Fixed::from_int(1), Fixed::from_int(2));
    assert_eq!(v.dot(w), Fixed::from_int(11));
    assert_eq!((v + w) - w, v);
    assert_eq!(v.scale(Fixed::from_int(2)), v * Fixed::from_int(2));
}

#[test]
fn vec3_geometry() {
    let x = FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO);
    let y = FixedVec3::new(Fixed::ZERO, Fixed::ONE, Fixed::ZERO);
    // x cross y = z (right-handed).
    assert_eq!(
        x.cross(y),
        FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE)
    );
    assert_eq!(x.dot(y), Fixed::ZERO);
    let v = FixedVec3::new(Fixed::from_int(2), Fixed::from_int(3), Fixed::from_int(6));
    assert_eq!(v.length(), Fixed::from_int(7)); // 2-3-6-7 Pythagorean quadruple
}
