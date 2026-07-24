//! `FixedMat3` invariants and accuracy bounds (docs/plans/voxel-physics.md
//! §2 / P0). Randomized samples use the house LCG (no `rand` dep); the
//! `f64` reference comparisons carry explicit bit-count tolerances.

use monada_fixed::{Fixed, FixedMat3, FixedQuat, FixedVec3};

/// Assert two `Fixed` are within `eps` raw steps of each other.
fn close(a: Fixed, b: Fixed, eps_bits: i64) {
    let d = (a.to_bits() - b.to_bits()).abs();
    assert!(d <= eps_bits, "‖{a:?} - {b:?}‖ = {d} bits > {eps_bits}");
}

/// Componentwise [`close`] on vectors.
fn close_vec(a: FixedVec3, b: FixedVec3, eps_bits: i64) {
    close(a.x, b.x, eps_bits);
    close(a.y, b.y, eps_bits);
    close(a.z, b.z, eps_bits);
}

/// Componentwise [`close`] on matrices.
fn close_mat(a: FixedMat3, b: FixedMat3, eps_bits: i64) {
    close_vec(a.x_axis, b.x_axis, eps_bits);
    close_vec(a.y_axis, b.y_axis, eps_bits);
    close_vec(a.z_axis, b.z_axis, eps_bits);
}

/// A tiny deterministic LCG so sampling tests need no `rand` dep and
/// stay reproducible. Unlike `arithmetic.rs`'s helper this keeps the
/// sign bit (`>> 32`, reinterpreted as `i32`) — the matrix tests must
/// sample all octants, not just the positive one.
fn lcg(state: &mut u64) -> i32 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    (*state >> 32) as i32
}

/// A random `Fixed` in roughly `(-8, 8)` — the well-conditioned regime
/// rotation/inertia entries live in.
fn small_fixed(state: &mut u64) -> Fixed {
    Fixed::from_bits(i64::from(lcg(state) % (8 << 16)) << 16)
}

/// A random unit quaternion (random scaled axis through `from_scaled_axis`).
fn random_unit_quat(state: &mut u64) -> FixedQuat {
    FixedQuat::from_scaled_axis(FixedVec3::new(
        small_fixed(state),
        small_fixed(state),
        small_fixed(state),
    ))
}

#[test]
fn identity_and_zero_behave() {
    let mut s = 0xA11C_E0F5;
    for _ in 0..100 {
        let v = FixedVec3::new(
            small_fixed(&mut s),
            small_fixed(&mut s),
            small_fixed(&mut s),
        );
        assert_eq!(FixedMat3::IDENTITY * v, v);
        assert_eq!(FixedMat3::ZERO * v, FixedVec3::ZERO);
    }
    assert_eq!(FixedMat3::IDENTITY.determinant(), Fixed::ONE);
    assert_eq!(FixedMat3::IDENTITY.transpose(), FixedMat3::IDENTITY);
    assert_eq!(FixedMat3::default(), FixedMat3::IDENTITY);
}

#[test]
fn transpose_is_involutive_and_diagonal_symmetric() {
    let mut s = 0xBEEF;
    for _ in 0..100 {
        let m = FixedMat3::from_cols(
            FixedVec3::new(
                small_fixed(&mut s),
                small_fixed(&mut s),
                small_fixed(&mut s),
            ),
            FixedVec3::new(
                small_fixed(&mut s),
                small_fixed(&mut s),
                small_fixed(&mut s),
            ),
            FixedVec3::new(
                small_fixed(&mut s),
                small_fixed(&mut s),
                small_fixed(&mut s),
            ),
        );
        assert_eq!(m.transpose().transpose(), m);
    }
    let d = FixedMat3::from_diagonal(FixedVec3::new(
        Fixed::from_int(2),
        Fixed::from_int(3),
        Fixed::from_int(5),
    ));
    assert_eq!(d.transpose(), d);
    assert_eq!(d.determinant(), Fixed::from_int(30));
}

/// `from_quat(q) * v` tracks quaternion rotation `q * v`. Both paths are
/// exact fixed-point chains that round independently; a few ulps of
/// disagreement in the last place is the expected envelope. Bound: 2⁻²⁴
/// per component (`1 << 8` raw bits) for |v| < 8.
#[test]
fn from_quat_matches_quaternion_rotation() {
    let mut s = 0x0123_4567;
    for _ in 0..200 {
        let q = random_unit_quat(&mut s);
        let v = FixedVec3::new(
            small_fixed(&mut s),
            small_fixed(&mut s),
            small_fixed(&mut s),
        );
        close_vec(FixedMat3::from_quat(q) * v, q * v, 1 << 8);
    }
}

/// `from_quat` against an `f64` reference rotation matrix. The trig in
/// `from_scaled_axis` (LUT + lerp) dominates the error; bound: 2⁻¹⁶
/// absolute per entry (`1 << 16` raw bits) — comfortably beyond what any
/// physics consumer resolves.
#[test]
fn from_quat_matches_f64_reference() {
    let mut s = 0x89AB_CDEF;
    for _ in 0..200 {
        let q = random_unit_quat(&mut s);
        let m = FixedMat3::from_quat(q);
        let (qx, qy, qz, qw) = (q.x.to_f64(), q.y.to_f64(), q.z.to_f64(), q.w.to_f64());
        let reference = [
            [
                1.0 - 2.0 * (qy * qy + qz * qz),
                2.0 * (qx * qy + qw * qz),
                2.0 * (qx * qz - qw * qy),
            ],
            [
                2.0 * (qx * qy - qw * qz),
                1.0 - 2.0 * (qx * qx + qz * qz),
                2.0 * (qy * qz + qw * qx),
            ],
            [
                2.0 * (qx * qz + qw * qy),
                2.0 * (qy * qz - qw * qx),
                1.0 - 2.0 * (qx * qx + qy * qy),
            ],
        ];
        for (col, reference_col) in [m.x_axis, m.y_axis, m.z_axis].iter().zip(&reference) {
            for (got, want) in [col.x, col.y, col.z].iter().zip(reference_col) {
                close(*got, Fixed::from_f64(*want), 1 << 16);
            }
        }
    }
}

/// A rotation matrix is orthonormal: `R · Rᵀ ≈ I`, `det R ≈ 1`, and its
/// transpose inverts it. The trig LUT in `from_scaled_axis` leaves the
/// quaternion's norm ~2⁻²⁰ off unit, which lands squared on the
/// diagonal; bound: 2⁻¹⁸ per entry (`1 << 14` raw bits).
#[test]
fn rotation_is_orthonormal() {
    let mut s = 0xFEED_F00D;
    for _ in 0..200 {
        let r = FixedMat3::from_quat(random_unit_quat(&mut s));
        close_mat(r * r.transpose(), FixedMat3::IDENTITY, 1 << 14);
        close(r.determinant(), Fixed::ONE, 1 << 14);
    }
}

/// `m * m.inverse() ≈ I` for well-conditioned matrices — the rotated
/// diagonal `R · D · Rᵀ` shape of a real inertia tensor. Bound: 2⁻¹⁶ per
/// entry, dominated by the `1/det` division of adjugate entries.
#[test]
fn inverse_of_inertia_shape() {
    let mut s = 0x5EED_1E55;
    for _ in 0..200 {
        // Diagonal in (1, 9): safely positive-definite.
        let one_to_nine = |s: &mut u64| Fixed::ONE + small_fixed(s).abs();
        let d = FixedMat3::from_diagonal(FixedVec3::new(
            one_to_nine(&mut s),
            one_to_nine(&mut s),
            one_to_nine(&mut s),
        ));
        let r = FixedMat3::from_quat(random_unit_quat(&mut s));
        let m = r * d * r.transpose();
        close_mat(m * m.inverse(), FixedMat3::IDENTITY, 1 << 16);
        // The world-space inverse-inertia identity physics relies on:
        // (R · D · Rᵀ)⁻¹ = R · D⁻¹ · Rᵀ.
        close_mat(m.inverse(), r * d.inverse() * r.transpose(), 1 << 16);
    }
}

#[test]
#[should_panic(expected = "singular matrix")]
fn inverse_of_singular_panics() {
    let _ = FixedMat3::ZERO.inverse();
}

/// Additive ops and scalar scaling agree with entrywise expectations.
#[test]
fn additive_ops_are_entrywise() {
    let mut s = 0xC0FF_EE00;
    for _ in 0..100 {
        let m = FixedMat3::from_quat(random_unit_quat(&mut s));
        assert_eq!(m + FixedMat3::ZERO, m);
        assert_eq!(m - m, FixedMat3::ZERO);
        assert_eq!(-(-m), m);
        assert_eq!(m + m, m.scale(Fixed::from_int(2)));
        assert_eq!(m * Fixed::ONE, m);
    }
}

/// Large inertia tensors — the P4 regression: a body barely 6 voxels
/// across has det ≈ 1296³ ≈ 2.2e9, past the Q32.32 ceiling. The wide
/// paths must still invert it and read its definiteness correctly.
#[test]
fn wide_paths_survive_large_tensors() {
    let d = FixedMat3::from_diagonal(FixedVec3::new(
        Fixed::from_int(1296),
        Fixed::from_int(1400),
        Fixed::from_int(1522),
    ));
    assert!(d.leading_minors_positive(), "large SPD reads positive");
    close_mat(d * d.inverse(), FixedMat3::IDENTITY, 1 << 12);
    // Rotated large tensor: R·D·Rᵀ inverts through the same path.
    let mut s = 0x1BAD_B002;
    for _ in 0..50 {
        let r = FixedMat3::from_quat(random_unit_quat(&mut s));
        let m = r * d * r.transpose();
        assert!(m.leading_minors_positive());
        close_mat(m * m.inverse(), FixedMat3::IDENTITY, 1 << 14);
    }
    // Indefinite large tensor reads negative despite the wrap regime.
    let bad = FixedMat3::from_diagonal(FixedVec3::new(
        Fixed::from_int(1296),
        Fixed::from_int(1400),
        Fixed::from_int(-1522),
    ));
    assert!(!bad.leading_minors_positive());
}
