//! Arithmetic-invariant tests for the Q32.32 core. These guard the
//! determinism contract: defined overflow, fixed rounding, and
//! integer-only trig (DESIGN.md §3.1).

use monada_fixed::trig::{atan2, cos, sin, FRAC_PI_2, PI, TAU};
use monada_fixed::{Fixed, FixedQuat, FixedVec2, FixedVec3};

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
fn vec3_normalize_clamp_reject() {
    let eps = 1 << 16;

    // normalize: result is unit length
    let v = FixedVec3::new(Fixed::from_int(3), Fixed::from_int(4), Fixed::ZERO);
    close(v.normalize().length(), Fixed::ONE, eps);
    // direction preserved: normalized x/y ratio matches original 3:4
    let n = v.normalize();
    close(n.x * Fixed::from_int(4), n.y * Fixed::from_int(3), eps);
    // zero input returns zero, no panic
    assert_eq!(FixedVec3::ZERO.normalize(), FixedVec3::ZERO);

    // clamp_length_max: short vector unchanged
    let short = FixedVec3::new(Fixed::from_int(1), Fixed::ZERO, Fixed::ZERO);
    assert_eq!(short.clamp_length_max(Fixed::from_int(5)), short);
    // long vector clamped to max length
    let long = FixedVec3::new(Fixed::from_int(10), Fixed::ZERO, Fixed::ZERO);
    let clamped = long.clamp_length_max(Fixed::from_int(3));
    close(clamped.length(), Fixed::from_int(3), eps);
    // exact length is unchanged
    let at_max = FixedVec3::new(Fixed::from_int(3), Fixed::ZERO, Fixed::ZERO);
    assert_eq!(at_max.clamp_length_max(Fixed::from_int(3)), at_max);

    // reject: component perpendicular to rhs
    // rejecting (3, 4, 0) from x-axis leaves only the y component
    let a = FixedVec3::new(Fixed::from_int(3), Fixed::from_int(4), Fixed::ZERO);
    let x_axis = FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO);
    let r = a.reject(x_axis);
    close(r.x, Fixed::ZERO, 1 << 12);
    close(r.y, Fixed::from_int(4), 1 << 12);
    close(r.z, Fixed::ZERO, 1 << 12);
    // reject is perpendicular to rhs: dot product ≈ 0
    close(r.dot(x_axis), Fixed::ZERO, 1 << 12);
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

#[test]
fn quat_rotation() {
    let eps = 1 << 18; // LUT-driven; two trig calls per quat construction

    // Identity leaves every vector unchanged — bit-exact (no trig involved).
    let v = FixedVec3::new(Fixed::from_int(3), Fixed::from_int(4), Fixed::from_int(5));
    assert_eq!(FixedQuat::IDENTITY * v, v);
    assert_eq!(FixedQuat::default() * v, v);

    // Rotate x-axis 90° around z-axis → y-axis.
    let z = FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE);
    let q90z = FixedQuat::from_axis_angle(z, FRAC_PI_2);
    let x_axis = FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO);
    let y_axis = FixedVec3::new(Fixed::ZERO, Fixed::ONE, Fixed::ZERO);
    let rotated = q90z * x_axis;
    close(rotated.x, y_axis.x, eps);
    close(rotated.y, y_axis.y, eps);
    close(rotated.z, y_axis.z, eps);

    // Isometry: rotation preserves vector length.
    close(rotated.length(), x_axis.length(), eps);
    let w = FixedVec3::new(Fixed::from_int(3), Fixed::from_int(4), Fixed::ZERO);
    let rotated_w = q90z * w;
    close(rotated_w.length(), w.length(), 1 << 20);

    // Composition: (q1 * q2) * v == q1 * (q2 * v).
    let x = FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO);
    let q90x = FixedQuat::from_axis_angle(x, FRAC_PI_2);
    let composed = q90z * q90x;
    let diag = FixedVec3::new(Fixed::ONE, Fixed::ONE, Fixed::ONE);
    let via_composed = composed * diag;
    let via_sequential = q90z * (q90x * diag);
    close(via_composed.x, via_sequential.x, eps);
    close(via_composed.y, via_sequential.y, eps);
    close(via_composed.z, via_sequential.z, eps);

    // Round-trip: q.inverse() * (q * v) ≈ v.
    let rt = q90z.inverse() * (q90z * v);
    close(rt.x, v.x, eps);
    close(rt.y, v.y, eps);
    close(rt.z, v.z, eps);

    // from_scaled_axis matches from_axis_angle.
    let q_sa = FixedQuat::from_scaled_axis(z.scale(FRAC_PI_2));
    let r_aa = q90z * x_axis;
    let r_sa = q_sa * x_axis;
    close(r_sa.x, r_aa.x, eps);
    close(r_sa.y, r_aa.y, eps);
    close(r_sa.z, r_aa.z, eps);

    // normalize brings a slightly drifted quaternion back to unit length.
    let drifted = FixedQuat::new(q90z.w + Fixed::from_bits(1 << 20), q90z.x, q90z.y, q90z.z);
    close(drifted.normalize().length(), Fixed::ONE, 1 << 16);

    // Zero axis and zero scaled-axis return identity.
    assert_eq!(
        FixedQuat::from_axis_angle(FixedVec3::ZERO, FRAC_PI_2),
        FixedQuat::IDENTITY
    );
    assert_eq!(
        FixedQuat::from_scaled_axis(FixedVec3::ZERO),
        FixedQuat::IDENTITY
    );
}

#[test]
fn atan2_landmarks() {
    let eps = 1 << 16; // ~1.5e-5, two lerp steps

    // Cardinal directions.
    close(atan2(Fixed::ZERO, Fixed::ONE), Fixed::ZERO, eps); // +x axis → 0
    close(atan2(Fixed::ONE, Fixed::ZERO), FRAC_PI_2, eps); // +y axis → π/2
    close(atan2(Fixed::ZERO, Fixed::NEG_ONE), PI, eps); // −x axis → π
    close(atan2(Fixed::NEG_ONE, Fixed::ZERO), -FRAC_PI_2, eps); // −y axis → −π/2

    // 45° diagonals.
    let frac_pi_4 = FRAC_PI_2 / Fixed::from_int(2);
    close(atan2(Fixed::ONE, Fixed::ONE), frac_pi_4, eps); // Q1 diagonal
    close(atan2(Fixed::ONE, Fixed::NEG_ONE), PI - frac_pi_4, eps); // Q2
    close(atan2(Fixed::NEG_ONE, Fixed::NEG_ONE), frac_pi_4 - PI, eps); // Q3
    close(atan2(Fixed::NEG_ONE, Fixed::ONE), -frac_pi_4, eps); // Q4

    // Consistent with sin/cos: atan2(sin(a), cos(a)) == a for a ∈ (−π, π].
    let mut st = 1234u64;
    for _ in 0..500 {
        let raw = (lcg(&mut st) % 6_283) as i64; // raw steps, range < 2π
        let a = Fixed::from_bits(raw * (1 << 16)) - PI; // a ∈ (−π, π]
        let recovered = atan2(sin(a), cos(a));
        close(recovered, a, 1 << 18);
    }
}
