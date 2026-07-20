//! Fixed-point unit quaternion [`FixedQuat`].
//!
//! Stored as `(x, y, z, w)` matching glam's field order. `w` is the scalar
//! part; for a rotation by angle `θ` around unit axis `n`:
//! `w = cos(θ/2)`, `(x,y,z) = n·sin(θ/2)`.
//!
//! **Invariant**: constructors always yield unit quaternions. Repeated [`Mul`]
//! accumulates rounding error; call [`FixedQuat::normalize`] periodically to
//! correct drift.
//!
//! **Vector components** passed to `Mul<FixedVec3>` should be well within
//! the Q32.32 range: the rotation formula crosses the vector twice (`t =
//! 2(q_xyz × v)`, then `q_xyz × t`), and intermediates can approach the
//! ceiling even when the final result would be in-range.

use core::ops::{Mul, Neg};

use crate::trig::{acos, cos, sin};
use crate::{Fixed, FixedVec3};

/// A unit quaternion representing a 3-D rotation, stored as `(x, y, z, w)`.
///
/// Field order matches glam's `Quat`: `(x, y, z, w)` where `w` is the scalar
/// part. Serde layout reflects this order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FixedQuat {
    pub x: Fixed,
    pub y: Fixed,
    pub z: Fixed,
    pub w: Fixed,
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
    pub const fn new(x: Fixed, y: Fixed, z: Fixed, w: Fixed) -> FixedQuat {
        FixedQuat { x, y, z, w }
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

    /// Dot product of the four components. Used to measure alignment between
    /// two quaternions (positive = same hemisphere = same rotation direction).
    #[inline]
    #[must_use]
    pub fn dot(self, rhs: FixedQuat) -> Fixed {
        self.w * rhs.w + self.x * rhs.x + self.y * rhs.y + self.z * rhs.z
    }

    /// Normalised linear interpolation from `self` to `rhs` at parameter `t ∈ [0, 1]`.
    ///
    /// Takes the shortest arc (negates `rhs` if the dot product is negative).
    /// Cheaper than [`slerp`](FixedQuat::slerp) but not constant angular speed.
    #[must_use]
    pub fn nlerp(self, rhs: FixedQuat, t: Fixed) -> FixedQuat {
        let rhs = if self.dot(rhs).to_bits() < 0 {
            -rhs
        } else {
            rhs
        };
        FixedQuat {
            w: self.w + (rhs.w - self.w) * t,
            x: self.x + (rhs.x - self.x) * t,
            y: self.y + (rhs.y - self.y) * t,
            z: self.z + (rhs.z - self.z) * t,
        }
        .normalize()
    }

    /// Spherical linear interpolation from `self` to `rhs` at parameter `t ∈ [0, 1]`.
    ///
    /// Constant angular speed along the great-circle arc. Falls back to
    /// [`nlerp`](FixedQuat::nlerp) when the quaternions are nearly identical
    /// (avoids division by `sin(ω) ≈ 0`).
    #[must_use]
    pub fn slerp(self, rhs: FixedQuat, t: Fixed) -> FixedQuat {
        let d = self.dot(rhs);
        // Shortest-path: if quats are in opposite hemispheres, negate one.
        let (rhs, d) = if d.to_bits() < 0 {
            (-rhs, -d)
        } else {
            (rhs, d)
        };
        // Clamp to [-1, 1] against rounding overshoot.
        let d = if d > Fixed::ONE { Fixed::ONE } else { d };

        // Near-identical quaternions: sin(ω) → 0, fall back to nlerp.
        // Threshold 1 − 2⁻²⁰ ≈ 1 − 9.5e-7, same ballpark as glam.
        let threshold = Fixed::ONE - Fixed::from_bits(1 << 12);
        if d >= threshold {
            return FixedQuat {
                w: self.w + (rhs.w - self.w) * t,
                x: self.x + (rhs.x - self.x) * t,
                y: self.y + (rhs.y - self.y) * t,
                z: self.z + (rhs.z - self.z) * t,
            }
            .normalize();
        }

        let omega = acos(d);
        let sin_omega = sin(omega);
        let scale0 = sin(omega * (Fixed::ONE - t)) / sin_omega;
        let scale1 = sin(omega * t) / sin_omega;
        FixedQuat {
            w: self.w * scale0 + rhs.w * scale1,
            x: self.x * scale0 + rhs.x * scale1,
            y: self.y * scale0 + rhs.y * scale1,
            z: self.z * scale0 + rhs.z * scale1,
        }
    }

    /// The shortest rotation that takes unit vector `from` to unit vector `to`.
    ///
    /// Uses the half-angle trick `q = (1 + from·to, from×to).normalize()` —
    /// no `acos` or `sin` at runtime. Handles the anti-parallel case by
    /// choosing a perpendicular axis and rotating by π.
    ///
    /// Both arguments must be unit vectors.
    #[must_use]
    pub fn from_rotation_arc(from: FixedVec3, to: FixedVec3) -> FixedQuat {
        let d = from.dot(to);

        // Anti-parallel (from ≈ -to): rotate 180° around any perpendicular axis.
        // Threshold: dot ≤ -1 + ~1.5e-5.
        let anti_threshold = Fixed::NEG_ONE + Fixed::from_bits(1 << 16);
        if d <= anti_threshold {
            // Pick the cardinal axis most orthogonal to `from` (smallest component).
            let (ax, ay, az) = (
                from.x.to_bits().abs(),
                from.y.to_bits().abs(),
                from.z.to_bits().abs(),
            );
            let perp = if ax <= ay && ax <= az {
                from.cross(FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO))
            } else if ay <= az {
                from.cross(FixedVec3::new(Fixed::ZERO, Fixed::ONE, Fixed::ZERO))
            } else {
                from.cross(FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE))
            }
            .normalize();
            return FixedQuat {
                w: Fixed::ZERO,
                x: perp.x,
                y: perp.y,
                z: perp.z,
            };
        }

        // General case: unnormalised quat (1 + d, from × to), then normalise.
        let c = from.cross(to);
        FixedQuat {
            w: Fixed::ONE + d,
            x: c.x,
            y: c.y,
            z: c.z,
        }
        .normalize()
    }
}

impl Default for FixedQuat {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// Negation: `−q` represents the same rotation as `q` but lives in the
/// opposite hemisphere. Used internally for shortest-path `nlerp`/`slerp`.
impl Neg for FixedQuat {
    type Output = FixedQuat;
    #[inline]
    fn neg(self) -> FixedQuat {
        FixedQuat {
            w: -self.w,
            x: -self.x,
            y: -self.y,
            z: -self.z,
        }
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
