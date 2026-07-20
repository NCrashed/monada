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
//! Functions: `sin`, `cos`, `atan2`. `tan` is deferred.
//! `atan2` uses a first-octant `atan` LUT over `t ∈ [0, 1]` (4 097
//! entries, both endpoints exact), exploiting `atan(|y|/|x|)` symmetry
//! and quadrant mirroring to cover all of `(-π, π]`; the same
//! "integer-only at runtime, reproducible table at build time" contract
//! applies.

use crate::Fixed;

// The baked table is thousands of separator-less integer literals;
// pedantic lints (`unreadable_literal`, etc.) on generated code are
// noise, so quarantine the include in its own module.
#[allow(clippy::all, clippy::pedantic)]
mod tables {
    include!(concat!(env!("OUT_DIR"), "/trig_tables.rs"));
}
use tables::{ATAN_LUT, ATAN_LUT_LEN, FRAC_PI_2_BITS, LUT_LEN, PI_BITS, SIN_LUT, TAU_BITS};

/// π as a [`Fixed`].
pub const PI: Fixed = Fixed::from_bits(PI_BITS);
/// τ = 2π as a [`Fixed`].
pub const TAU: Fixed = Fixed::from_bits(TAU_BITS);
/// π/2 as a [`Fixed`].
pub const FRAC_PI_2: Fixed = Fixed::from_bits(FRAC_PI_2_BITS);

/// Sine of `angle` (radians).
#[must_use]
pub fn sin(angle: Fixed) -> Fixed {
    // Reduce into one full turn `[0, TAU_BITS)`. `rem_euclid` keeps the
    // result non-negative regardless of the sign of `angle`.
    let r = angle.to_bits().rem_euclid(TAU_BITS);

    // Sample position `s = r / TAU * LUT_LEN`, split into an integer
    // index and a Q-of-TAU remainder used as the lerp weight. `num`
    // peaks near `TAU_BITS * LUT_LEN ≈ 2^45`, comfortably inside i128.
    let tau = i128::from(TAU_BITS);
    let num = i128::from(r) * LUT_LEN as i128;
    let idx0 = (num / tau) as usize;
    let rem = num % tau;

    let idx1 = if idx0 + 1 == LUT_LEN { 0 } else { idx0 + 1 };
    let a = i128::from(SIN_LUT[idx0]);
    let b = i128::from(SIN_LUT[idx1]);

    // a + (b - a) * rem / TAU_BITS, all in i128 to avoid overflow.
    let interpolated = a + (b - a) * rem / tau;
    Fixed::from_bits(interpolated as i64)
}

/// Cosine of `angle` (radians).
#[must_use]
pub fn cos(angle: Fixed) -> Fixed {
    sin(angle + FRAC_PI_2)
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

    let a = i128::from(ATAN_LUT[idx0]);
    let b = i128::from(ATAN_LUT[idx1]);
    Fixed::from_bits((a + (b - a) * rem / denom) as i64)
}
