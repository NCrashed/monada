//! The Q32.32 scalar type [`Fixed`].

use core::cmp::Ordering;
use core::fmt;
use core::ops::{Add, AddAssign, Div, DivAssign, Mul, MulAssign, Neg, Rem, Sub, SubAssign};

/// Number of fractional bits in the Q32.32 representation.
pub(crate) const FRAC_BITS: u32 = 32;

/// `1.0` as a raw `i64` (`2^32`).
const ONE_BITS: i64 = 1 << FRAC_BITS;

/// Mask of the fractional bits.
const FRAC_MASK: i64 = ONE_BITS - 1;

/// A Q32.32 fixed-point number: a real value stored as `raw / 2^32`
/// in an `i64`.
///
/// `Ord`/`Eq` are derived from the raw `i64` and agree with numeric
/// ordering, so `Fixed` is a valid `BTreeMap` key — useful for the
/// deterministic-iteration containers in `monada-sim` (DESIGN.md §3.1).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Fixed(i64);

impl Fixed {
    /// The value `0`.
    pub const ZERO: Fixed = Fixed(0);
    /// The value `1`.
    pub const ONE: Fixed = Fixed(ONE_BITS);
    /// The value `-1`.
    pub const NEG_ONE: Fixed = Fixed(-ONE_BITS);
    /// The value `0.5`.
    pub const HALF: Fixed = Fixed(ONE_BITS >> 1);
    /// The smallest representable positive step (`2^-32`).
    pub const EPSILON: Fixed = Fixed(1);
    /// The largest representable value.
    pub const MAX: Fixed = Fixed(i64::MAX);
    /// The most negative representable value.
    pub const MIN: Fixed = Fixed(i64::MIN);

    /// Construct from a whole-number integer.
    #[inline]
    #[must_use]
    pub const fn from_int(i: i32) -> Fixed {
        // `i` is at most 31 bits, so `(i as i64) << 32` never overflows.
        Fixed((i as i64) << FRAC_BITS)
    }

    /// Construct `num / den`, rounded to the nearest Q32.32 step toward
    /// zero. Exact when `den` divides `num * 2^32` (e.g. `from_ratio(3,
    /// 2)`); otherwise truncated — `from_ratio(1, 3)` is the nearest
    /// representable value below `1/3`, not the exact rational.
    ///
    /// # Panics
    /// Panics if `den == 0`.
    #[inline]
    #[must_use]
    pub const fn from_ratio(num: i32, den: i32) -> Fixed {
        assert!(den != 0, "Fixed::from_ratio: division by zero");
        Fixed(((num as i64) << FRAC_BITS) / den as i64)
    }

    /// Reinterpret a raw Q32.32 bit pattern as a `Fixed`.
    #[inline]
    #[must_use]
    pub const fn from_bits(bits: i64) -> Fixed {
        Fixed(bits)
    }

    /// The raw Q32.32 bit pattern.
    #[inline]
    #[must_use]
    pub const fn to_bits(self) -> i64 {
        self.0
    }

    /// The integer part, rounded toward negative infinity.
    #[inline]
    #[must_use]
    pub const fn floor_to_int(self) -> i32 {
        (self.0 >> FRAC_BITS) as i32
    }

    /// Largest integer value `<= self` (toward negative infinity).
    #[inline]
    #[must_use]
    pub const fn floor(self) -> Fixed {
        Fixed(self.0 & !FRAC_MASK)
    }

    /// Smallest integer value `>= self` (toward positive infinity).
    #[inline]
    #[must_use]
    pub const fn ceil(self) -> Fixed {
        Fixed((self.0 + FRAC_MASK) & !FRAC_MASK)
    }

    /// Round to the nearest integer (ties away from zero).
    #[inline]
    #[must_use]
    pub const fn round(self) -> Fixed {
        if self.0 >= 0 {
            Fixed((self.0 + (ONE_BITS >> 1)) & !FRAC_MASK)
        } else {
            Fixed(-(((-self.0) + (ONE_BITS >> 1)) & !FRAC_MASK))
        }
    }

    /// The fractional part, `self - self.floor()` (always in `[0, 1)`).
    #[inline]
    #[must_use]
    pub const fn fract(self) -> Fixed {
        Fixed(self.0 & FRAC_MASK)
    }

    /// Absolute value (wraps for [`Fixed::MIN`], matching `i64::abs`'s
    /// two's-complement edge — deterministic, never panics).
    #[inline]
    #[must_use]
    pub const fn abs(self) -> Fixed {
        Fixed(self.0.wrapping_abs())
    }

    /// `-1`, `0`, or `1` as a `Fixed`, matching the sign of `self`.
    #[inline]
    #[must_use]
    pub const fn signum(self) -> Fixed {
        match self.0 {
            0 => Fixed::ZERO,
            n if n > 0 => Fixed::ONE,
            _ => Fixed::NEG_ONE,
        }
    }

    /// The smaller of two values.
    #[inline]
    #[must_use]
    pub const fn min(self, other: Fixed) -> Fixed {
        if self.0 <= other.0 {
            self
        } else {
            other
        }
    }

    /// The larger of two values.
    #[inline]
    #[must_use]
    pub const fn max(self, other: Fixed) -> Fixed {
        if self.0 >= other.0 {
            self
        } else {
            other
        }
    }

    /// Clamp into `[lo, hi]`.
    ///
    /// # Panics
    /// Panics if `lo > hi`.
    #[inline]
    #[must_use]
    pub const fn clamp(self, lo: Fixed, hi: Fixed) -> Fixed {
        assert!(lo.0 <= hi.0, "Fixed::clamp: lo > hi");
        self.max(lo).min(hi)
    }

    /// Wrapping addition (two's complement). The non-panicking default
    /// used by `+`.
    #[inline]
    #[must_use]
    pub const fn wrapping_add(self, rhs: Fixed) -> Fixed {
        Fixed(self.0.wrapping_add(rhs.0))
    }

    /// Wrapping subtraction (two's complement). The default used by `-`.
    #[inline]
    #[must_use]
    pub const fn wrapping_sub(self, rhs: Fixed) -> Fixed {
        Fixed(self.0.wrapping_sub(rhs.0))
    }

    /// Checked addition: `None` on overflow.
    #[inline]
    #[must_use]
    pub const fn checked_add(self, rhs: Fixed) -> Option<Fixed> {
        match self.0.checked_add(rhs.0) {
            Some(v) => Some(Fixed(v)),
            None => None,
        }
    }

    /// Checked subtraction: `None` on overflow.
    #[inline]
    #[must_use]
    pub const fn checked_sub(self, rhs: Fixed) -> Option<Fixed> {
        match self.0.checked_sub(rhs.0) {
            Some(v) => Some(Fixed(v)),
            None => None,
        }
    }

    /// Fixed-point multiplication via an `i128` intermediate.
    ///
    /// **Rounds the 64-bit product to nearest** (ties toward `+∞`):
    /// `(a * b + 2^31) >> 32`. This is a deliberate choice over a bare
    /// truncating shift — truncation always biases toward `−∞`, and that
    /// bias is *systematic*, so per-tick velocity integration in a long
    /// sim drifts in one direction. Round-to-nearest halves the error
    /// for the cost of a single add and leaves no directional bias
    /// (only exact ½-ulp ties, which are measure-zero in practice, lean
    /// `+∞`). It is exactly as deterministic as truncation — one fixed
    /// rounding mode on every platform — which is all lockstep requires.
    ///
    /// One consequence of the toward-`+∞` tie: negation does not
    /// distribute *at* a ½-ulp tie, i.e. `(−a)·b` can differ from
    /// `−(a·b)` by 1 ulp. Harmless (deterministic, ties measure-zero);
    /// round-half-away-from-zero would restore sign symmetry but only by
    /// adding the sign branch this deliberately avoids.
    ///
    /// The final narrowing to `i64` wraps on overflow (see
    /// [`checked_mul`](Fixed::checked_mul)).
    #[inline]
    #[must_use]
    pub const fn mul(self, rhs: Fixed) -> Fixed {
        let full = self.0 as i128 * rhs.0 as i128;
        let p = (full + (1 << (FRAC_BITS - 1))) >> FRAC_BITS;
        Fixed(p as i64)
    }

    /// Checked fixed-point multiplication: `None` if the (round-to-
    /// nearest, see [`mul`](Fixed::mul)) Q32.32 result does not fit in
    /// `i64`.
    #[inline]
    #[must_use]
    pub const fn checked_mul(self, rhs: Fixed) -> Option<Fixed> {
        let full = self.0 as i128 * rhs.0 as i128;
        let p = (full + (1 << (FRAC_BITS - 1))) >> FRAC_BITS;
        if p < i64::MIN as i128 || p > i64::MAX as i128 {
            None
        } else {
            Some(Fixed(p as i64))
        }
    }

    /// Fixed-point division via an `i128` intermediate.
    ///
    /// Truncates toward zero (i128 integer division). Unlike
    /// multiplication this is *not* rounded to nearest: toward-zero is
    /// already symmetric about zero, so it carries no directional bias
    /// to accumulate. The narrowing to `i64` wraps on an out-of-range
    /// quotient (e.g. `MAX / EPSILON`); use
    /// [`checked_div`](Fixed::checked_div) when that must be caught.
    ///
    /// # Panics
    /// Panics if `rhs` is zero.
    #[inline]
    #[must_use]
    pub const fn div(self, rhs: Fixed) -> Fixed {
        assert!(rhs.0 != 0, "Fixed::div: division by zero");
        let q = ((self.0 as i128) << FRAC_BITS) / rhs.0 as i128;
        Fixed(q as i64)
    }

    /// Checked fixed-point division: `None` on a zero divisor or an
    /// out-of-range quotient. The non-panicking, non-wrapping
    /// counterpart to [`div`](Fixed::div), for parity with
    /// [`checked_mul`](Fixed::checked_mul).
    #[inline]
    #[must_use]
    pub const fn checked_div(self, rhs: Fixed) -> Option<Fixed> {
        if rhs.0 == 0 {
            return None;
        }
        let q = ((self.0 as i128) << FRAC_BITS) / rhs.0 as i128;
        if q < i64::MIN as i128 || q > i64::MAX as i128 {
            None
        } else {
            Some(Fixed(q as i64))
        }
    }

    /// Remainder via [`i64::wrapping_rem`], sharing the dividend's sign
    /// (truncated division). Used by the `%` operator.
    ///
    /// `wrapping_rem` makes `MIN % NEG_ONE` defined (it yields `0`)
    /// instead of panicking in debug / wrapping in release — closing the
    /// last build-divergence hole in the arithmetic surface.
    ///
    /// # Panics
    /// Panics if `rhs` is zero.
    #[inline]
    #[must_use]
    pub const fn rem(self, rhs: Fixed) -> Fixed {
        assert!(rhs.0 != 0, "Fixed::rem: division by zero");
        Fixed(self.0.wrapping_rem(rhs.0))
    }

    /// Checked remainder: `None` on a zero divisor.
    #[inline]
    #[must_use]
    pub const fn checked_rem(self, rhs: Fixed) -> Option<Fixed> {
        match self.0.checked_rem(rhs.0) {
            Some(v) => Some(Fixed(v)),
            None => None,
        }
    }

    /// Square root, computed entirely in integer arithmetic.
    ///
    /// For `v = raw / 2^32`, `sqrt(v) = isqrt(raw << 32) / 2^32`, and
    /// the `u128` integer square root is deterministic on every
    /// platform — unlike libm `sqrt` (DESIGN.md §3.1).
    ///
    /// # Panics
    /// Panics if `self` is negative.
    #[inline]
    #[must_use]
    pub const fn sqrt(self) -> Fixed {
        assert!(self.0 >= 0, "Fixed::sqrt: negative value");
        // raw << 32 can be up to 2^95, hence the u128 radicand.
        let radicand = (self.0 as u128) << FRAC_BITS;
        Fixed(isqrt_u128(radicand) as i64)
    }

    /// Convert to `f64`. **Render side only** (DESIGN.md §3.1) — the
    /// result must never flow back into sim state.
    #[inline]
    #[must_use]
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / ONE_BITS as f64
    }

    /// Convert to `f32`. **Render side only.**
    #[inline]
    #[must_use]
    pub fn to_f32(self) -> f32 {
        self.0 as f32 / ONE_BITS as f32
    }

    /// Construct from `f64` by rounding to the nearest Q32.32 step.
    ///
    /// For literals and asset import only — a deterministic pipeline
    /// never round-trips sim state through this. Saturates to
    /// [`Fixed::MIN`]/[`Fixed::MAX`] for out-of-range / non-finite
    /// inputs (Rust's float-to-int `as` cast is saturating).
    #[inline]
    #[must_use]
    pub fn from_f64(x: f64) -> Fixed {
        let scaled = x * ONE_BITS as f64;
        let rounded = if scaled >= 0.0 {
            scaled + 0.5
        } else {
            scaled - 0.5
        };
        Fixed(rounded as i64)
    }
}

/// `u128` integer square root by the bit-by-bit (digit) method — the
/// classic non-restoring algorithm. `const fn`-friendly and identical
/// on every target (no `u128::isqrt` MSRV dependency).
const fn isqrt_u128(n: u128) -> u128 {
    // Start at the highest even-positioned bit (`2^126`) and walk it
    // down past the radicand's top set bit.
    let mut bit: u128 = 1 << 126;
    while bit > n {
        bit >>= 2;
    }
    let mut res: u128 = 0;
    let mut x = n;
    while bit != 0 {
        if x >= res + bit {
            x -= res + bit;
            res = (res >> 1) + bit;
        } else {
            res >>= 1;
        }
        bit >>= 2;
    }
    res
}

impl Add for Fixed {
    type Output = Fixed;
    #[inline]
    fn add(self, rhs: Fixed) -> Fixed {
        self.wrapping_add(rhs)
    }
}

impl Sub for Fixed {
    type Output = Fixed;
    #[inline]
    fn sub(self, rhs: Fixed) -> Fixed {
        self.wrapping_sub(rhs)
    }
}

impl Neg for Fixed {
    type Output = Fixed;
    #[inline]
    fn neg(self) -> Fixed {
        Fixed(self.0.wrapping_neg())
    }
}

impl Mul for Fixed {
    type Output = Fixed;
    #[inline]
    fn mul(self, rhs: Fixed) -> Fixed {
        Fixed::mul(self, rhs)
    }
}

impl Div for Fixed {
    type Output = Fixed;
    #[inline]
    fn div(self, rhs: Fixed) -> Fixed {
        Fixed::div(self, rhs)
    }
}

impl Rem for Fixed {
    type Output = Fixed;
    #[inline]
    fn rem(self, rhs: Fixed) -> Fixed {
        Fixed::rem(self, rhs)
    }
}

impl AddAssign for Fixed {
    #[inline]
    fn add_assign(&mut self, rhs: Fixed) {
        *self = *self + rhs;
    }
}

impl SubAssign for Fixed {
    #[inline]
    fn sub_assign(&mut self, rhs: Fixed) {
        *self = *self - rhs;
    }
}

impl MulAssign for Fixed {
    #[inline]
    fn mul_assign(&mut self, rhs: Fixed) {
        *self = *self * rhs;
    }
}

impl DivAssign for Fixed {
    #[inline]
    fn div_assign(&mut self, rhs: Fixed) {
        *self = *self / rhs;
    }
}

impl fmt::Debug for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Show the approximate decimal value plus the exact raw bits,
        // so a desync dump is both human-readable and exact.
        write!(f, "Fixed({} = {:#018x})", self.to_f64(), self.0)
    }
}

impl fmt::Display for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_f64())
    }
}

impl PartialEq<i32> for Fixed {
    #[inline]
    fn eq(&self, other: &i32) -> bool {
        self.0 == (i64::from(*other) << FRAC_BITS)
    }
}

impl PartialOrd<i32> for Fixed {
    #[inline]
    fn partial_cmp(&self, other: &i32) -> Option<Ordering> {
        Some(self.0.cmp(&(i64::from(*other) << FRAC_BITS)))
    }
}

impl From<i32> for Fixed {
    #[inline]
    fn from(i: i32) -> Fixed {
        Fixed::from_int(i)
    }
}
