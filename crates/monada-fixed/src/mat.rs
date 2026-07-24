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

/// One Q32.32 product kept wide: an `i128` still scaled by 2³².
/// The wide determinant/inverse paths build on this so no
/// intermediate ever narrows to `i64`.
#[inline]
fn wide_mul(a: Fixed, b: Fixed) -> i128 {
    (i128::from(a.to_bits()) * i128::from(b.to_bits())) >> 32
}

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
    ///
    /// **Range**: the value wraps past `±2^31` like every [`Fixed`]
    /// chain — an inertia tensor of a body barely 6 voxels across
    /// already exceeds it (`1296³ ≈ 2.2e9`). Sign tests and inversion
    /// must go through the wide-arithmetic paths
    /// ([`leading_minors_positive`](FixedMat3::leading_minors_positive),
    /// [`inverse`](FixedMat3::inverse)), which never narrow the
    /// determinant to `i64`.
    #[inline]
    #[must_use]
    pub fn determinant(self) -> Fixed {
        self.x_axis.dot(self.y_axis.cross(self.z_axis))
    }

    /// The determinant as a Q32.32-scaled `i128` — exact sign and
    /// magnitude for any tensor physics builds (entries below `2^28`
    /// keep every intermediate inside `i128` with room to spare).
    fn determinant_wide(self) -> i128 {
        let (a, b, c) = (self.x_axis, self.y_axis, self.z_axis);
        // (b × c) per component, Q32.32-scaled i128.
        let cx = wide_mul(b.y, c.z) - wide_mul(b.z, c.y);
        let cy = wide_mul(b.z, c.x) - wide_mul(b.x, c.z);
        let cz = wide_mul(b.x, c.y) - wide_mul(b.y, c.x);
        // a · (b × c), still Q32.32-scaled.
        ((i128::from(a.x.to_bits()) * cx) >> 32)
            + ((i128::from(a.y.to_bits()) * cy) >> 32)
            + ((i128::from(a.z.to_bits()) * cz) >> 32)
    }

    /// Sylvester's criterion on the leading principal minors, computed
    /// in wide arithmetic so large inertia tensors cannot wrap the
    /// third minor into a false (or worse, falsely passing) sign.
    #[must_use]
    pub fn leading_minors_positive(self) -> bool {
        let m1 = self.x_axis.x > Fixed::ZERO;
        let m2 =
            wide_mul(self.x_axis.x, self.y_axis.y) - wide_mul(self.y_axis.x, self.x_axis.y) > 0;
        m1 && m2 && self.determinant_wide() > 0
    }

    /// The inverse, via the adjugate over the determinant — the
    /// determinant and every adjugate entry are carried in `i128`, so
    /// tensors whose determinant exceeds the Q32.32 ceiling (any body
    /// ≳ 6 voxels across) invert correctly; only the *result* narrows,
    /// and inverse-inertia entries are small by nature.
    ///
    /// Intended for the well-conditioned matrices physics actually
    /// inverts — the inertia tensor of a non-empty voxel body is
    /// symmetric positive-definite, so its determinant is comfortably
    /// non-zero.
    ///
    /// # Panics
    /// Panics if the determinant is zero (the matrix is singular).
    #[must_use]
    pub fn inverse(self) -> FixedMat3 {
        let det = self.determinant_wide();
        assert!(
            det != 0,
            "FixedMat3::inverse: singular matrix (determinant is zero)"
        );
        let (a, b, c) = (self.x_axis, self.y_axis, self.z_axis);
        // For column-major M = [a b c] the inverse's rows are the
        // cross products (b×c, c×a, a×b) over the determinant. Each
        // entry: (cross_raw << 32) / det_raw, truncating toward zero
        // like Fixed::div.
        let entry = |p: Fixed, q: Fixed, r: Fixed, s: Fixed| {
            let cross = wide_mul(p, q) - wide_mul(r, s);
            Fixed::from_bits(((cross << 32) / det) as i64)
        };
        // Columns collect the x/y/z components of the rows
        // (b×c, c×a, a×b) — same layout as the narrow version had.
        FixedMat3 {
            x_axis: FixedVec3::new(
                entry(b.y, c.z, b.z, c.y), // (b×c).x
                entry(c.y, a.z, c.z, a.y), // (c×a).x
                entry(a.y, b.z, a.z, b.y), // (a×b).x
            ),
            y_axis: FixedVec3::new(
                entry(b.z, c.x, b.x, c.z), // (b×c).y
                entry(c.z, a.x, c.x, a.z), // (c×a).y
                entry(a.z, b.x, a.x, b.z), // (a×b).y
            ),
            z_axis: FixedVec3::new(
                entry(b.x, c.y, b.y, c.x), // (b×c).z
                entry(c.x, a.y, c.y, a.x), // (c×a).z
                entry(a.x, b.y, a.y, b.x), // (a×b).z
            ),
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
