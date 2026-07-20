//! Fixed-point unit quaternion [`FixedQuat`].
//!
//! Stored as `(w, x, y, z)` where `w` is the scalar part. For a rotation
//! by angle `θ` around unit axis `n`: `w = cos(θ/2)`, `(x,y,z) = n·sin(θ/2)`.
//!
//! **Invariant**: constructors always yield unit quaternions. Repeated [`Mul`]
//! accumulates rounding error; call [`FixedQuat::normalize`] periodically to
//! correct drift.
//!
//! **Vector components** passed to `Mul<FixedVec3>` should be well within
//! the Q32.32 range: the rotation formula crosses the vector twice (`t =
//! 2(q_xyz × v)`, then `q_xyz × t`), and intermediates can approach the
//! ceiling even when the final result would be in-range.

use core::ops::Mul;

use crate::trig::{cos, sin};
use crate::{Fixed, FixedVec3};

/// A unit quaternion representing a 3-D rotation, stored as `(w, x, y, z)`.
///
/// Note: field order is `(w, x, y, z)` — the inverse of glam's `(x, y, z, w)`.
/// Serde layout reflects this order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FixedQuat {
    pub w: Fixed,
    pub x: Fixed,
    pub y: Fixed,
    pub z: Fixed,
}

impl FixedQuat {
    /// The identity rotation.
    pub const IDENTITY: FixedQuat = FixedQuat {
        w: Fixed::ONE,
        x: Fixed::ZERO,
        y: Fixed::ZERO,
        z: Fixed::ZERO,
    };

    /// Construct directly from components. Caller must ensure the result is
    /// unit length.
    #[inline]
    #[must_use]
    pub const fn new(w: Fixed, x: Fixed, y: Fixed, z: Fixed) -> FixedQuat {
        FixedQuat { w, x, y, z }
    }

    /// Rotation by `angle` radians around `axis`. `axis` need not be unit
    /// length — it is normalised internally. Returns [`FixedQuat::IDENTITY`]
    /// if `axis` is the zero vector.
    #[must_use]
    pub fn from_axis_angle(axis: FixedVec3, angle: Fixed) -> FixedQuat {
        let n = axis.normalize();
        if n == FixedVec3::ZERO {
            return FixedQuat::IDENTITY;
        }
        let half = angle / Fixed::from_int(2);
        let s = sin(half);
        FixedQuat {
            w: cos(half),
            x: n.x * s,
            y: n.y * s,
            z: n.z * s,
        }
    }

    /// Rotation from a scaled-axis vector: direction = rotation axis,
    /// magnitude = rotation angle in radians. Returns [`FixedQuat::IDENTITY`]
    /// for the zero vector.
    #[must_use]
    pub fn from_scaled_axis(v: FixedVec3) -> FixedQuat {
        let angle = v.length();
        if angle == Fixed::ZERO {
            return FixedQuat::IDENTITY;
        }
        let inv_len = Fixed::ONE / angle;
        let half = angle / Fixed::from_int(2);
        let s = sin(half);
        FixedQuat {
            w: cos(half),
            x: v.x * inv_len * s,
            y: v.y * inv_len * s,
            z: v.z * inv_len * s,
        }
    }

    /// Squared norm of the four components.
    #[inline]
    #[must_use]
    pub fn length_squared(self) -> Fixed {
        self.w * self.w + self.x * self.x + self.y * self.y + self.z * self.z
    }

    /// Norm of the four components.
    #[inline]
    #[must_use]
    pub fn length(self) -> Fixed {
        self.length_squared().sqrt()
    }

    /// Returns the unit quaternion in the same orientation, or
    /// [`FixedQuat::IDENTITY`] for the zero quaternion.
    #[must_use]
    pub fn normalize(self) -> FixedQuat {
        let len = self.length();
        if len == Fixed::ZERO {
            return FixedQuat::IDENTITY;
        }
        let inv = Fixed::ONE / len;
        FixedQuat {
            w: self.w * inv,
            x: self.x * inv,
            y: self.y * inv,
            z: self.z * inv,
        }
    }

    /// The inverse rotation. Assumes `self` is a unit quaternion; for unit
    /// quaternions the inverse equals the conjugate `(w, -x, -y, -z)`.
    #[inline]
    #[must_use]
    pub fn inverse(self) -> FixedQuat {
        FixedQuat {
            w: self.w,
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
    }
}

impl Default for FixedQuat {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Hamilton product: compose two rotations.
impl Mul<FixedQuat> for FixedQuat {
    type Output = FixedQuat;
    #[inline]
    fn mul(self, r: FixedQuat) -> FixedQuat {
        FixedQuat {
            w: self.w * r.w - self.x * r.x - self.y * r.y - self.z * r.z,
            x: self.w * r.x + self.x * r.w + self.y * r.z - self.z * r.y,
            y: self.w * r.y - self.x * r.z + self.y * r.w + self.z * r.x,
            z: self.w * r.z + self.x * r.y - self.y * r.x + self.z * r.w,
        }
    }
}

/// Rotate a vector by this unit quaternion.
///
/// Uses `v' = v + 2w(q_xyz × v) + 2(q_xyz × (q_xyz × v))`, which needs two
/// cross products and avoids a full quaternion multiply + division.
impl Mul<FixedVec3> for FixedQuat {
    type Output = FixedVec3;
    #[inline]
    fn mul(self, v: FixedVec3) -> FixedVec3 {
        let two = Fixed::from_int(2);
        let q = FixedVec3::new(self.x, self.y, self.z);
        let t = q.cross(v).scale(two);
        v + t.scale(self.w) + q.cross(t)
    }
}
