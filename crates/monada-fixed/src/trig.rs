//! Deterministic fixed-point trigonometry.
//!
//! `sin`/`cos` take an angle in radians as a [`Fixed`] and look it up
//! in a baked sine table (`build.rs`), linearly interpolating between
//! samples. The table holds Q32.32 integer constants and the lookup +
//! interpolation is *pure integer arithmetic* — no libm `sin` is ever
//! called at runtime, so the result is bit-identical on every platform
//! (DESIGN.md §3.1). The build-time table itself is reproducible; see
//! the argument in `build.rs`.
//!
//! Functions: `sin`, `cos`, `atan2`, `acos`. `tan` is deferred. `cos` uses
//! the same `SIN_LUT` shifted by `LUT_LEN/4` (≡ π/2) in index space.
//! `atan2` uses a first-octant `atan` LUT over `t ∈ [0, 1]` (4 097
//! entries, both endpoints exact), exploiting `atan(|y|/|x|)` symmetry
//! and quadrant mirroring to cover all of `(-π, π]`. `acos` uses a
//! direct LUT over `x ∈ [0, 1]` (4 097 entries) with the identity
//! `acos(-x) = π − acos(x)` for the negative half; both avoid `sqrt`
//! at runtime. The same "integer-only at runtime, reproducible table at
//! build time" contract applies throughout.

use crate::Fixed;

// The baked table is thousands of separator-less integer literals;
// pedantic lints (`unreadable_literal`, etc.) on generated code are
// noise, so quarantine the include in its own module.
#[allow(clippy::all, clippy::pedantic)]
mod tables {
    include!(concat!(env!("OUT_DIR"), "/trig_tables.rs"));
}
use tables::{
    ACOS_LUT, ACOS_LUT_LEN, ATAN_LUT, ATAN_LUT_LEN, FRAC_PI_2_BITS, LUT_LEN, PI_BITS, SIN_LUT,
    TAU_BITS,
};

/// π as a [`Fixed`].
pub const PI: Fixed = Fixed::from_bits(PI_BITS);
/// τ = 2π as a [`Fixed`].
pub const TAU: Fixed = Fixed::from_bits(TAU_BITS);
/// π/2 as a [`Fixed`].
pub const FRAC_PI_2: Fixed = Fixed::from_bits(FRAC_PI_2_BITS);

/// Linear interpolation between two LUT entries, all in i128 to avoid overflow.
#[inline]
fn lut_lerp(a: i128, b: i128, rem: i128, denom: i128) -> i64 {
    (a + (b - a) * rem / denom) as i64
}

/// Look up `SIN_LUT` for a reduced angle `r ∈ [0, TAU_BITS)` with an
/// optional LUT index offset. `offset = 0` gives sin; `offset = LUT_LEN/4`
/// gives cos (equivalent to a π/2 phase shift in index space).
fn sin_lookup(r: i64, offset: usize) -> Fixed {
    // Sample position split into integer index + lerp remainder.
    // `num` peaks near TAU_BITS * LUT_LEN ≈ 2^45, safe in i128.
    let tau = i128::from(TAU_BITS);
    let num = i128::from(r) * LUT_LEN as i128;
    let idx0 = ((num / tau) as usize + offset) % LUT_LEN;
    let rem = num % tau;
    let idx1 = if idx0 + 1 == LUT_LEN { 0 } else { idx0 + 1 };
    Fixed::from_bits(lut_lerp(
        i128::from(SIN_LUT[idx0]),
        i128::from(SIN_LUT[idx1]),
        rem,
        tau,
    ))
}

/// Sine of `angle` (radians).
#[must_use]
pub fn sin(angle: Fixed) -> Fixed {
    sin_lookup(angle.to_bits().rem_euclid(TAU_BITS), 0)
}

/// Cosine of `angle` (radians).
///
/// Uses `SIN_LUT` shifted by `LUT_LEN/4` (≡ π/2 phase) to avoid a
/// separate `Fixed` addition and the rounding it would introduce.
#[must_use]
pub fn cos(angle: Fixed) -> Fixed {
    sin_lookup(angle.to_bits().rem_euclid(TAU_BITS), LUT_LEN / 4)
}

/// Angle of the vector `(x, y)` in radians, in `(-π, π]`.
///
/// Follows the standard `atan2(y, x)` convention: positive x-axis is 0,
/// angles increase counter-clockwise. Returns `Fixed::ZERO` when both
/// arguments are zero.
#[must_use]
pub fn atan2(y: Fixed, x: Fixed) -> Fixed {
    let x_bits = x.to_bits();
    let y_bits = y.to_bits();

    if x_bits == 0 && y_bits == 0 {
        return Fixed::ZERO;
    }

    let abs_x = x_bits.abs();
    let abs_y = y_bits.abs();

    // Compute atan(min/max) ∈ [0, π/4], then adjust to [0, π/2] via
    // the identity atan(a/b) = π/2 − atan(b/a) when a > b.
    let angle = if abs_y <= abs_x {
        atan_first_octant(abs_y, abs_x)
    } else {
        FRAC_PI_2 - atan_first_octant(abs_x, abs_y)
    };

    // Mirror into the correct quadrant.
    match (x_bits >= 0, y_bits >= 0) {
        (true, true) => angle,
        (false, true) => PI - angle,
        (false, false) => angle - PI,
        (true, false) => -angle,
    }
}

/// Arccosine of `x` in radians, result in `[0, π]`.
///
/// `x` is clamped to `[-1, 1]`; values outside that range (e.g. due to
/// rounding) are silently clamped rather than producing garbage.
#[must_use]
pub fn acos(x: Fixed) -> Fixed {
    let negate = x.to_bits() < 0;
    // Work in the positive half; clamp overshoot from rounding.
    let abs_x = if negate { -x } else { x };
    let abs_x = if abs_x > Fixed::ONE {
        Fixed::ONE
    } else {
        abs_x
    };

    // Index into ACOS_LUT: map abs_x ∈ [0, 1] → [0, ACOS_LUT_LEN].
    // denom = Fixed::ONE.to_bits() = 2^32 (the Q32.32 scale factor).
    let n = ACOS_LUT_LEN as i128;
    let denom = i128::from(Fixed::ONE.to_bits());
    let num = i128::from(abs_x.to_bits()) * n;
    let idx0 = (num / denom) as usize;
    let rem = num % denom;
    let idx1 = (idx0 + 1).min(ACOS_LUT_LEN);

    let angle = Fixed::from_bits(lut_lerp(
        i128::from(ACOS_LUT[idx0]),
        i128::from(ACOS_LUT[idx1]),
        rem,
        denom,
    ));

    if negate {
        PI - angle
    } else {
        angle
    }
}

/// `atan(a / b)` for `0 ≤ a ≤ b`, `b > 0` — result in `[0, π/4]`.
///
/// Looks up `atan(t)` with `t = a/b ∈ [0, 1]` in the first-octant table
/// and linearly interpolates. All arithmetic is integer-only.
fn atan_first_octant(a_bits: i64, b_bits: i64) -> Fixed {
    let n = ATAN_LUT_LEN as i128;
    let num = i128::from(a_bits) * n;
    let denom = i128::from(b_bits);
    let idx0 = (num / denom) as usize;
    let rem = num % denom;
    // idx0 ≤ ATAN_LUT_LEN because a_bits ≤ b_bits; clamp idx1 to stay in bounds.
    let idx1 = (idx0 + 1).min(ATAN_LUT_LEN);

    Fixed::from_bits(lut_lerp(
        i128::from(ATAN_LUT[idx0]),
        i128::from(ATAN_LUT[idx1]),
        rem,
        denom,
    ))
}
