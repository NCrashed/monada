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
//! Scope: `sin`/`cos` only, which is all M0's circle scenario needs.
//! `tan`/`atan2` are deliberately deferred — they arrive with unit
//! facing and pathing, and `atan2` in particular is its own
//! deterministic-LUT (or fixed-point CORDIC) design question, with the
//! same "integer-only at runtime, reproducible table at build time"
//! contract this module establishes.

use crate::Fixed;

// The baked table is thousands of separator-less integer literals;
// pedantic lints (`unreadable_literal`, etc.) on generated code are
// noise, so quarantine the include in its own module.
#[allow(clippy::all, clippy::pedantic)]
mod tables {
    include!(concat!(env!("OUT_DIR"), "/trig_tables.rs"));
}
use tables::{FRAC_PI_2_BITS, LUT_LEN, PI_BITS, SIN_LUT, TAU_BITS};

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
