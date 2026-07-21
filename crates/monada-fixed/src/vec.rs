//! Fixed-point vector types [`FixedVec2`] and [`FixedVec3`].
//!
//! These are the sim-side spatial primitives (DESIGN.md §3.3's
//! `FixedVec3`). The render bridge converts them to `glam`'s float
//! vectors on its side of the wall; sim code never holds an `f64` pose.

use core::ops::{Add, AddAssign, Mul, Neg, Sub, SubAssign};

use crate::Fixed;

/// A 2D vector of [`Fixed`] components.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FixedVec2 {
    pub x: Fixed,
    pub y: Fixed,
}

/// A 3D vector of [`Fixed`] components.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FixedVec3 {
    pub x: Fixed,
    pub y: Fixed,
    pub z: Fixed,
}

impl FixedVec2 {
    pub const ZERO: FixedVec2 = FixedVec2 {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
    };

    #[inline]
    #[must_use]
    pub const fn new(x: Fixed, y: Fixed) -> FixedVec2 {
        FixedVec2 { x, y }
    }

    /// Dot product.
    #[inline]
    #[must_use]
    pub fn dot(self, rhs: FixedVec2) -> Fixed {
        self.x * rhs.x + self.y * rhs.y
    }

    /// Squared length (no `sqrt`; exact and cheap for comparisons).
    #[inline]
    #[must_use]
    pub fn length_squared(self) -> Fixed {
        self.dot(self)
    }

    /// Euclidean length.
    #[inline]
    #[must_use]
    pub fn length(self) -> Fixed {
        self.length_squared().sqrt()
    }

    /// Scale every component by a scalar.
    #[inline]
    #[must_use]
    pub fn scale(self, s: Fixed) -> FixedVec2 {
        FixedVec2 {
            x: self.x * s,
            y: self.y * s,
        }
    }
}

impl FixedVec3 {
    pub const ZERO: FixedVec3 = FixedVec3 {
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        z: Fixed::ZERO,
    };

    #[inline]
    #[must_use]
    pub const fn new(x: Fixed, y: Fixed, z: Fixed) -> FixedVec3 {
        FixedVec3 { x, y, z }
    }

    /// Dot product.
    #[inline]
    #[must_use]
    pub fn dot(self, rhs: FixedVec3) -> Fixed {
        self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    /// Cross product.
    #[inline]
    #[must_use]
    pub fn cross(self, rhs: FixedVec3) -> FixedVec3 {
        FixedVec3 {
            x: self.y * rhs.z - self.z * rhs.y,
            y: self.z * rhs.x - self.x * rhs.z,
            z: self.x * rhs.y - self.y * rhs.x,
        }
    }

    /// Squared length (no `sqrt`).
    #[inline]
    #[must_use]
    pub fn length_squared(self) -> Fixed {
        self.dot(self)
    }

    /// Euclidean length.
    #[inline]
    #[must_use]
    pub fn length(self) -> Fixed {
        self.length_squared().sqrt()
    }

    /// Scale every component by a scalar.
    #[inline]
    #[must_use]
    pub fn scale(self, s: Fixed) -> FixedVec3 {
        FixedVec3 {
            x: self.x * s,
            y: self.y * s,
            z: self.z * s,
        }
    }

    /// Unit vector in the same direction, or [`FixedVec3::ZERO`] for the
    /// zero vector.
    ///
    /// Handles components up to `i64::MAX / 2^32` by scaling the vector
    /// down a power of two before squaring, preserving direction.
    #[must_use]
    pub fn normalize(self) -> FixedVec3 {
        // In Q32.32 the safe squaring threshold is ≈ 46340 (sqrt of
        // i64::MAX in Q32.32 units). Shift down until all components fit.
        const MAX_SAFE: i64 = 46340_i64 << 32;
        let max_abs = self
            .x
            .to_bits()
            .abs()
            .max(self.y.to_bits().abs())
            .max(self.z.to_bits().abs());
        if max_abs > MAX_SAFE {
            return FixedVec3::new(
                Fixed::from_bits(self.x.to_bits() >> 16),
                Fixed::from_bits(self.y.to_bits() >> 16),
                Fixed::from_bits(self.z.to_bits() >> 16),
            )
            .normalize();
        }
        let len = self.length();
        if len == Fixed::ZERO {
            return FixedVec3::ZERO;
        }
        self.scale(Fixed::ONE / len)
    }

    /// Returns `self` unchanged if its length is ≤ `max`, otherwise scales
    /// it down so its length equals `max`.
    #[must_use]
    pub fn clamp_length_max(self, max: Fixed) -> FixedVec3 {
        let len_sq = self.length_squared();
        // `max * max` overflows Q32.32 for max > ~46340; use checked_mul
        // and treat overflow as "definitely larger than any real length".
        let max_sq = max.checked_mul(max).unwrap_or(Fixed::MAX);
        if len_sq <= max_sq {
            self
        } else {
            self.scale(max / len_sq.sqrt())
        }
    }

    /// Vector rejection of `self` from `rhs`: the component of `self`
    /// perpendicular to `rhs`. Returns `self` unchanged when `rhs` is zero.
    #[must_use]
    pub fn reject(self, rhs: FixedVec3) -> FixedVec3 {
        let len_sq = rhs.length_squared();
        if len_sq == Fixed::ZERO {
            return self;
        }
        self - rhs.scale(self.dot(rhs) / len_sq)
    }
}

macro_rules! impl_vec_ops {
    ($t:ty { $($field:ident),+ }) => {
        impl Add for $t {
            type Output = $t;
            #[inline]
            fn add(self, rhs: $t) -> $t {
                <$t>::new($(self.$field + rhs.$field),+)
            }
        }
        impl Sub for $t {
            type Output = $t;
            #[inline]
            fn sub(self, rhs: $t) -> $t {
                <$t>::new($(self.$field - rhs.$field),+)
            }
        }
        impl Neg for $t {
            type Output = $t;
            #[inline]
            fn neg(self) -> $t {
                <$t>::new($(-self.$field),+)
            }
        }
        impl AddAssign for $t {
            #[inline]
            fn add_assign(&mut self, rhs: $t) {
                $(self.$field += rhs.$field;)+
            }
        }
        impl SubAssign for $t {
            #[inline]
            fn sub_assign(&mut self, rhs: $t) {
                $(self.$field -= rhs.$field;)+
            }
        }
        // Scalar multiply: `v * s`.
        impl Mul<Fixed> for $t {
            type Output = $t;
            #[inline]
            fn mul(self, s: Fixed) -> $t {
                self.scale(s)
            }
        }
    };
}

impl_vec_ops!(FixedVec2 { x, y });
impl_vec_ops!(FixedVec3 { x, y, z });
