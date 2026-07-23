//! Fixed-point 3×3 matrix [`FixedMat3`].
//!
//! The inertia-tensor primitive for voxel rigid bodies
//! (docs/plans/voxel-physics.md §2): body-space inertia is a constant
//! `FixedMat3`, its world-space inverse is `R · I⁻¹ · Rᵀ` with `R` from
//! [`FixedMat3::from_quat`]. Column-major with [`FixedVec3`] columns,
//! matching glam's `Mat3` layout the same way [`FixedQuat`] matches
//! glam's `Quat`.
//!
//! **Range**: entries go through plain [`Fixed`] arithmetic — products
//! widen through `i128` per multiply, sums wrap per the crate's
//! determinism contract. Rotation matrices and voxel-body inertia
//! tensors are far from the Q32.32 ceiling; callers with entries near
//! `2^31` are responsible for their own scaling, as with
//! [`FixedVec3`].

use core::ops::{Add, Mul, Neg, Sub};

use crate::{Fixed, FixedQuat, FixedVec3};

/// A 3×3 matrix of [`Fixed`] entries, stored as three column vectors.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FixedMat3 {
    pub x_axis: FixedVec3,
    pub y_axis: FixedVec3,
    pub z_axis: FixedVec3,
}

impl FixedMat3 {
    /// The zero matrix — the additive identity, and the natural
    /// accumulator seed for summing per-voxel inertia contributions.
    pub const ZERO: FixedMat3 = FixedMat3 {
        x_axis: FixedVec3::ZERO,
        y_axis: FixedVec3::ZERO,
        z_axis: FixedVec3::ZERO,
    };

    /// The identity matrix.
    pub const IDENTITY: FixedMat3 = FixedMat3 {
        x_axis: FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO),
        y_axis: FixedVec3::new(Fixed::ZERO, Fixed::ONE, Fixed::ZERO),
        z_axis: FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE),
    };

    /// Construct from three column vectors.
    #[inline]
    #[must_use]
    pub const fn from_cols(x_axis: FixedVec3, y_axis: FixedVec3, z_axis: FixedVec3) -> FixedMat3 {
        FixedMat3 {
            x_axis,
            y_axis,
            z_axis,
        }
    }

    /// A diagonal matrix with `d` on the main diagonal (e.g. the inertia
    /// tensor of an axis-aligned box in its principal frame).
    #[inline]
    #[must_use]
    pub const fn from_diagonal(d: FixedVec3) -> FixedMat3 {
        FixedMat3 {
            x_axis: FixedVec3::new(d.x, Fixed::ZERO, Fixed::ZERO),
            y_axis: FixedVec3::new(Fixed::ZERO, d.y, Fixed::ZERO),
            z_axis: FixedVec3::new(Fixed::ZERO, Fixed::ZERO, d.z),
        }
    }

    /// The rotation matrix of a unit quaternion: `from_quat(q) * v`
    /// equals `q * v` up to rounding (each path rounds independently, so
    /// the two are close but not bit-identical — pick one per code path
    /// and stay with it).
    #[must_use]
    pub fn from_quat(q: FixedQuat) -> FixedMat3 {
        let two = Fixed::from_int(2);
        let (x2, y2, z2) = (q.x * two, q.y * two, q.z * two);
        let (xx, xy, xz) = (q.x * x2, q.x * y2, q.x * z2);
        let (yy, yz, zz) = (q.y * y2, q.y * z2, q.z * z2);
        let (wx, wy, wz) = (q.w * x2, q.w * y2, q.w * z2);
        FixedMat3 {
            x_axis: FixedVec3::new(Fixed::ONE - (yy + zz), xy + wz, xz - wy),
            y_axis: FixedVec3::new(xy - wz, Fixed::ONE - (xx + zz), yz + wx),
            z_axis: FixedVec3::new(xz + wy, yz - wx, Fixed::ONE - (xx + yy)),
        }
    }

    /// The transpose. For a pure rotation this is also the inverse,
    /// without the division cost of [`inverse`](FixedMat3::inverse).
    #[inline]
    #[must_use]
    pub const fn transpose(self) -> FixedMat3 {
        FixedMat3 {
            x_axis: FixedVec3::new(self.x_axis.x, self.y_axis.x, self.z_axis.x),
            y_axis: FixedVec3::new(self.x_axis.y, self.y_axis.y, self.z_axis.y),
            z_axis: FixedVec3::new(self.x_axis.z, self.y_axis.z, self.z_axis.z),
        }
    }

    /// The determinant, as the scalar triple product of the columns.
    #[inline]
    #[must_use]
    pub fn determinant(self) -> Fixed {
        self.x_axis.dot(self.y_axis.cross(self.z_axis))
    }

    /// The inverse, via the adjugate over the determinant.
    ///
    /// Intended for the well-conditioned matrices physics actually
    /// inverts — the inertia tensor of a non-empty voxel body is
    /// symmetric positive-definite, so its determinant is comfortably
    /// non-zero. Entries of a near-singular matrix wrap like any other
    /// out-of-range [`Fixed`] quotient.
    ///
    /// # Panics
    /// Panics if the determinant is zero (the matrix is singular).
    #[must_use]
    pub fn inverse(self) -> FixedMat3 {
        // For column-major M = [a b c] the inverse's *rows* are the
        // cross products (b×c, c×a, a×b) over det = a · (b×c).
        let r0 = self.y_axis.cross(self.z_axis);
        let r1 = self.z_axis.cross(self.x_axis);
        let r2 = self.x_axis.cross(self.y_axis);
        let inv_det = Fixed::ONE / self.x_axis.dot(r0);
        FixedMat3 {
            x_axis: FixedVec3::new(r0.x, r1.x, r2.x).scale(inv_det),
            y_axis: FixedVec3::new(r0.y, r1.y, r2.y).scale(inv_det),
            z_axis: FixedVec3::new(r0.z, r1.z, r2.z).scale(inv_det),
        }
    }

    /// Scale every entry by a scalar (e.g. density onto a unit-mass
    /// inertia contribution).
    #[inline]
    #[must_use]
    pub fn scale(self, s: Fixed) -> FixedMat3 {
        FixedMat3 {
            x_axis: self.x_axis.scale(s),
            y_axis: self.y_axis.scale(s),
            z_axis: self.z_axis.scale(s),
        }
    }
}

impl Default for FixedMat3 {
    #[inline]
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Add for FixedMat3 {
    type Output = FixedMat3;
    #[inline]
    fn add(self, rhs: FixedMat3) -> FixedMat3 {
        FixedMat3 {
            x_axis: self.x_axis + rhs.x_axis,
            y_axis: self.y_axis + rhs.y_axis,
            z_axis: self.z_axis + rhs.z_axis,
        }
    }
}

impl Sub for FixedMat3 {
    type Output = FixedMat3;
    #[inline]
    fn sub(self, rhs: FixedMat3) -> FixedMat3 {
        FixedMat3 {
            x_axis: self.x_axis - rhs.x_axis,
            y_axis: self.y_axis - rhs.y_axis,
            z_axis: self.z_axis - rhs.z_axis,
        }
    }
}

impl Neg for FixedMat3 {
    type Output = FixedMat3;
    #[inline]
    fn neg(self) -> FixedMat3 {
        FixedMat3 {
            x_axis: -self.x_axis,
            y_axis: -self.y_axis,
            z_axis: -self.z_axis,
        }
    }
}

/// Scalar multiply: `m * s`.
impl Mul<Fixed> for FixedMat3 {
    type Output = FixedMat3;
    #[inline]
    fn mul(self, s: Fixed) -> FixedMat3 {
        self.scale(s)
    }
}

/// Matrix–vector product (the linear map applied to `v`).
impl Mul<FixedVec3> for FixedMat3 {
    type Output = FixedVec3;
    #[inline]
    fn mul(self, v: FixedVec3) -> FixedVec3 {
        self.x_axis.scale(v.x) + self.y_axis.scale(v.y) + self.z_axis.scale(v.z)
    }
}

/// Matrix–matrix product: compose two linear maps.
impl Mul<FixedMat3> for FixedMat3 {
    type Output = FixedMat3;
    #[inline]
    fn mul(self, rhs: FixedMat3) -> FixedMat3 {
        FixedMat3 {
            x_axis: self * rhs.x_axis,
            y_axis: self * rhs.y_axis,
            z_axis: self * rhs.z_axis,
        }
    }
}
