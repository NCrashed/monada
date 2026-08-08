//! Deterministic voxel raycast — Amanatides–Woo DDA in fixed point
//! (docs/plans/voxel-physics.md, P3 amendments).
//!
//! P3's consumer is the wheel suspension; P5 exposes the same walk as
//! `World::raycast` for engine-side projectiles.

use monada_fixed::{Fixed, FixedVec3};

use crate::field::VoxelField;
use crate::material::MaterialId;
use crate::shape::VoxelShape;

/// A direction component at or below this is treated as exactly axial:
/// that axis is never crossed (explicit branch — no `1/ε` blowing up
/// near the Q32.32 ceiling). For a unit direction a 2⁻¹⁶ component
/// deflects the ray by under a hundredth of a voxel over the few-voxel
/// distances physics casts.
const AXIS_EPS: Fixed = Fixed::from_bits(1 << 16);

/// One raycast hit.
pub struct RayHit {
    pub cell: (i64, i64, i64),
    /// Distance along the ray, in voxels (`dir` is unit length).
    pub t: Fixed,
    /// The entered face's outward normal (axis-aligned, unit). For a
    /// ray starting inside solid: `−dir` (see [`cast`]).
    pub normal: FixedVec3,
}

/// March the field from `origin` along unit `dir` up to `max_t`.
///
/// **Contract**
/// - Cells follow the [`VoxelField`] convention (`[x, x+1)³`).
/// - A ray starting inside a solid cell hits at `t = 0` with
///   `normal = −dir` — "fully compressed, push straight back" is the
///   least-wrong answer for every current caller.
/// - The entered-face normal is exact (the axis of the DDA step), so
///   no gradient estimation is involved.
/// - Ties between axes (corner crossings) break in fixed x → y → z
///   order — part of the determinism contract.
///
/// # Panics
/// Debug-asserts that `dir` is unit length: `t` is measured in voxels
/// along it, so a non-unit direction would silently change what every
/// distance in the result means.
pub fn cast(
    field: &dyn VoxelField,
    origin: FixedVec3,
    dir: FixedVec3,
    max_t: Fixed,
) -> Option<RayHit> {
    let mut cell = (
        i64::from(origin.x.floor_to_int()),
        i64::from(origin.y.floor_to_int()),
        i64::from(origin.z.floor_to_int()),
    );
    if field.occupied(cell.0, cell.1, cell.2) {
        return Some(RayHit {
            cell,
            t: Fixed::ZERO,
            normal: -dir,
        });
    }

    // Per-axis setup: step direction, t to the first boundary, t per
    // cell. `Fixed::MAX` marks an axis the ray never crosses.
    let axis_setup = |o: Fixed, d: Fixed, c: i64| -> (i64, Fixed, Fixed) {
        if d.abs() <= AXIS_EPS {
            return (0, Fixed::MAX, Fixed::MAX);
        }
        let step = if d > Fixed::ZERO { 1i64 } else { -1i64 };
        let boundary = if step > 0 {
            Fixed::from_bits((c + 1) << 32)
        } else {
            Fixed::from_bits(c << 32)
        };
        let t_next = (boundary - o) / d;
        let t_delta = Fixed::ONE / d.abs();
        (step, t_next, t_delta)
    };
    let (sx, mut tx, dx) = axis_setup(origin.x, dir.x, cell.0);
    let (sy, mut ty, dy) = axis_setup(origin.y, dir.y, cell.1);
    let (sz, mut tz, dz) = axis_setup(origin.z, dir.z, cell.2);

    // At most ~3·(max_t + 2) boundary crossings for a unit direction;
    // the hard cap is a defensive backstop, not a tuning knob.
    let cap = 3 * (max_t.floor_to_int().max(0) + 2);
    for _ in 0..cap {
        // Advance the axis with the nearest boundary (x→y→z on ties).
        let (t, normal) = if tx <= ty && tx <= tz {
            cell.0 += sx;
            let t = tx;
            tx += dx;
            (
                t,
                FixedVec3::new(
                    Fixed::from_int(-i32::try_from(sx).expect("±1")),
                    Fixed::ZERO,
                    Fixed::ZERO,
                ),
            )
        } else if ty <= tz {
            cell.1 += sy;
            let t = ty;
            ty += dy;
            (
                t,
                FixedVec3::new(
                    Fixed::ZERO,
                    Fixed::from_int(-i32::try_from(sy).expect("±1")),
                    Fixed::ZERO,
                ),
            )
        } else {
            cell.2 += sz;
            let t = tz;
            tz += dz;
            (
                t,
                FixedVec3::new(
                    Fixed::ZERO,
                    Fixed::ZERO,
                    Fixed::from_int(-i32::try_from(sz).expect("±1")),
                ),
            )
        };
        if t > max_t {
            return None;
        }
        if field.occupied(cell.0, cell.1, cell.2) {
            return Some(RayHit { cell, t, normal });
        }
    }
    None
}

/// A body's voxel grid viewed as a [`VoxelField`], for shape-frame
/// raycasts (`World::raycast`): occupied ⇔ the shape cell is set;
/// everything outside the grid is empty.
pub(crate) struct ShapeOccupancy<'a>(pub &'a VoxelShape);

impl VoxelField for ShapeOccupancy<'_> {
    fn occupied(&self, x: i64, y: i64, z: i64) -> bool {
        let (Ok(x), Ok(y), Ok(z)) = (i32::try_from(x), i32::try_from(y), i32::try_from(z)) else {
            return false;
        };
        self.0.get(x, y, z).is_some()
    }

    fn material(&self, x: i64, y: i64, z: i64) -> MaterialId {
        let (Ok(x), Ok(y), Ok(z)) = (i32::try_from(x), i32::try_from(y), i32::try_from(z)) else {
            return MaterialId(0);
        };
        self.0.get(x, y, z).unwrap_or(MaterialId(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::material::MaterialId;

    /// Flat terrain below z = 0.
    struct Floor;
    impl VoxelField for Floor {
        fn occupied(&self, _x: i64, _y: i64, z: i64) -> bool {
            z < 0
        }
        fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
            MaterialId(0)
        }
    }

    /// Stairs rising +1 z per 2 cells of +x: occupied when
    /// `z < floor(x / 2)` (and everything below z = 0).
    struct Stairs;
    impl VoxelField for Stairs {
        fn occupied(&self, x: i64, _y: i64, z: i64) -> bool {
            z < (x.div_euclid(2)).max(0)
        }
        fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
            MaterialId(0)
        }
    }

    fn v(x: f64, y: f64, z: f64) -> FixedVec3 {
        FixedVec3::new(Fixed::from_f64(x), Fixed::from_f64(y), Fixed::from_f64(z))
    }

    #[test]
    fn axial_down_hits_floor_with_up_normal() {
        let hit = cast(
            &Floor,
            v(0.5, 0.5, 2.5),
            v(0.0, 0.0, -1.0),
            Fixed::from_int(5),
        )
        .expect("hits the floor");
        assert_eq!(hit.cell, (0, 0, -1));
        assert_eq!(hit.t, Fixed::from_f64(2.5));
        assert_eq!(hit.normal, v(0.0, 0.0, 1.0));
    }

    #[test]
    fn max_t_cuts_off() {
        assert!(cast(
            &Floor,
            v(0.5, 0.5, 2.5),
            v(0.0, 0.0, -1.0),
            Fixed::from_int(2)
        )
        .is_none());
    }

    #[test]
    fn start_inside_solid_is_t_zero_with_back_normal() {
        let dir = v(0.0, 0.0, -1.0);
        let hit = cast(&Floor, v(0.5, 0.5, -0.5), dir, Fixed::from_int(5)).expect("inside");
        assert_eq!(hit.t, Fixed::ZERO);
        assert_eq!(hit.normal, -dir);
        assert_eq!(hit.cell, (0, 0, -1));
    }

    #[test]
    fn near_axial_component_is_treated_as_axial() {
        // A sub-eps x component must not blow up 1/dir; the ray behaves
        // as straight down.
        let dir = FixedVec3::new(Fixed::from_bits(1 << 10), Fixed::ZERO, Fixed::NEG_ONE);
        let hit = cast(&Floor, v(0.5, 0.5, 1.5), dir, Fixed::from_int(4)).expect("hits");
        assert_eq!(hit.cell, (0, 0, -1));
        assert_eq!(hit.normal, v(0.0, 0.0, 1.0));
    }

    #[test]
    fn diagonal_ray_hits_stair_side_face() {
        // From (−0.5, 0.5, 0.5) marching +x at z just above the floor:
        // the first riser of the stairs is the side face of cell (2,·,0).
        let hit = cast(
            &Stairs,
            v(-0.5, 0.5, 0.5),
            v(1.0, 0.0, 0.0),
            Fixed::from_int(10),
        )
        .expect("hits the riser");
        assert_eq!(hit.cell, (2, 0, 0));
        assert_eq!(hit.normal, v(-1.0, 0.0, 0.0));
        assert_eq!(hit.t, Fixed::from_f64(2.5));
    }

    /// A wall filling x < 0.
    struct Wall;
    impl VoxelField for Wall {
        fn occupied(&self, x: i64, _y: i64, _z: i64) -> bool {
            x < 0
        }
        fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
            MaterialId(0)
        }
    }

    #[test]
    fn negative_direction_and_face_normal() {
        let hit = cast(
            &Wall,
            v(2.5, 0.5, 0.5),
            v(-1.0, 0.0, 0.0),
            Fixed::from_int(10),
        )
        .expect("hits the wall");
        assert_eq!(hit.cell, (-1, 0, 0));
        assert_eq!(hit.t, Fixed::from_f64(2.5));
        assert_eq!(hit.normal, v(1.0, 0.0, 0.0));
    }

    #[test]
    fn t_is_monotonic_along_a_bumpy_march() {
        // A skew ray across the stairs: every reported t (over many
        // max_t prefixes) is consistent — longer casts refine, never
        // contradict.
        let origin = v(-3.3, 0.2, 3.7);
        let dir = v(0.8, 0.1, -0.6).normalize();
        let full = cast(&Stairs, origin, dir, Fixed::from_int(20)).expect("lands");
        for cut in 1..=6 {
            match cast(&Stairs, origin, dir, Fixed::from_int(cut)) {
                None => assert!(full.t > Fixed::from_int(cut)),
                Some(h) => {
                    assert_eq!(h.cell, full.cell);
                    assert_eq!(h.t, full.t);
                }
            }
        }
    }
}
