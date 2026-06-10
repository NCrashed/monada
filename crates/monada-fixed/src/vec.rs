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
