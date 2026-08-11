//! Axis-aligned facing (DESIGN.md sim-space convention: +x east, +y
//! north, z-up — `dir_yaw` in the map scripts) as a discrete, hashable
//! value instead of a continuous yaw. Six states, one per grid face.

/// One of the six axis-aligned directions.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    X,
    NegX,
    Y,
    NegY,
    Z,
    NegZ,
}

/// A quarter-turn roll around whichever axis a [`Direction`] currently
/// points along. Paired with a `Direction`, `6 * 4 = 24` combinations,
/// every one valid: unlike a second `Direction`, a roll never names an
/// axis, so there is no perpendicularity constraint to violate.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Roll {
    Deg0,
    Deg90,
    Deg180,
    Deg270,
}

impl Direction {
    /// The direction facing the opposite way along the same axis.
    #[must_use]
    pub fn opposite(self) -> Direction {
        match self {
            Direction::X => Direction::NegX,
            Direction::NegX => Direction::X,
            Direction::Y => Direction::NegY,
            Direction::NegY => Direction::Y,
            Direction::Z => Direction::NegZ,
            Direction::NegZ => Direction::Z,
        }
    }

    /// `self` rotated 90° around `axis` (right-hand rule: looking from
    /// `axis` back toward the origin, the turn is counter-clockwise). A
    /// direction parallel to `axis` — `axis` itself or its opposite — is
    /// unchanged, since rotating an axis around itself does nothing.
    #[must_use]
    pub fn rotate(self, axis: Direction) -> Direction {
        use Direction::{NegX, NegY, NegZ, X, Y, Z};
        match axis {
            X => match self {
                X => X,
                NegX => NegX,
                Y => Z,
                Z => NegY,
                NegY => NegZ,
                NegZ => Y,
            },
            NegX => match self {
                X => X,
                NegX => NegX,
                Y => NegZ,
                NegZ => NegY,
                NegY => Z,
                Z => Y,
            },
            Y => match self {
                Y => Y,
                NegY => NegY,
                Z => X,
                X => NegZ,
                NegZ => NegX,
                NegX => Z,
            },
            NegY => match self {
                Y => Y,
                NegY => NegY,
                Z => NegX,
                NegX => NegZ,
                NegZ => X,
                X => Z,
            },
            Z => match self {
                Z => Z,
                NegZ => NegZ,
                X => Y,
                Y => NegX,
                NegX => NegY,
                NegY => X,
            },
            NegZ => match self {
                Z => Z,
                NegZ => NegZ,
                X => NegY,
                NegY => NegX,
                NegX => Y,
                Y => X,
            },
        }
    }
}

/// Every `Direction`, for exhaustive tests.
#[cfg(test)]
const ALL_DIRECTIONS: [Direction; 6] = [
    Direction::X,
    Direction::NegX,
    Direction::Y,
    Direction::NegY,
    Direction::Z,
    Direction::NegZ,
];

/// Every `Roll`, for exhaustive tests.
#[cfg(test)]
const ALL_ROLLS: [Roll; 4] = [Roll::Deg0, Roll::Deg90, Roll::Deg180, Roll::Deg270];

impl Roll {
    /// One quarter-turn clockwise (looking down the `Direction` axis).
    #[must_use]
    pub fn cw(self) -> Roll {
        match self {
            Roll::Deg0 => Roll::Deg90,
            Roll::Deg90 => Roll::Deg180,
            Roll::Deg180 => Roll::Deg270,
            Roll::Deg270 => Roll::Deg0,
        }
    }

    /// One quarter-turn counter-clockwise (looking down the `Direction`
    /// axis).
    #[must_use]
    pub fn ccw(self) -> Roll {
        match self {
            Roll::Deg0 => Roll::Deg270,
            Roll::Deg90 => Roll::Deg0,
            Roll::Deg180 => Roll::Deg90,
            Roll::Deg270 => Roll::Deg180,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opposite_pairs_are_the_documented_axes() {
        assert_eq!(Direction::X.opposite(), Direction::NegX);
        assert_eq!(Direction::NegX.opposite(), Direction::X);
        assert_eq!(Direction::Y.opposite(), Direction::NegY);
        assert_eq!(Direction::NegY.opposite(), Direction::Y);
        assert_eq!(Direction::Z.opposite(), Direction::NegZ);
        assert_eq!(Direction::NegZ.opposite(), Direction::Z);
    }

    /// Flipping twice is a no-op, and flipping once never leaves you where
    /// you started — an axis is never its own opposite.
    #[test]
    fn opposite_is_an_involution_with_no_fixed_point() {
        for d in ALL_DIRECTIONS {
            assert_eq!(d.opposite().opposite(), d, "{d:?}");
            assert_ne!(d.opposite(), d, "{d:?}");
        }
    }

    /// An axis rotated around itself, or around its own opposite, never
    /// moves — there is nothing perpendicular for it to turn into.
    #[test]
    fn rotate_leaves_the_axis_and_its_opposite_fixed() {
        for axis in ALL_DIRECTIONS {
            assert_eq!(axis.rotate(axis), axis, "{axis:?}");
            assert_eq!(axis.opposite().rotate(axis), axis.opposite(), "{axis:?}");
        }
    }

    /// Four quarter-turns around any axis is a full turn: every direction
    /// (including the axis itself) returns to where it started.
    #[test]
    fn rotate_four_times_is_identity() {
        for axis in ALL_DIRECTIONS {
            for d in ALL_DIRECTIONS {
                let spun = d.rotate(axis).rotate(axis).rotate(axis).rotate(axis);
                assert_eq!(spun, d, "axis {axis:?}, dir {d:?}");
            }
        }
    }

    /// One turn around `axis` then the opposite turn around `axis` (same
    /// as one turn around `axis.opposite()`) undoes it — the two senses of
    /// a rotation are exact inverses of one another.
    #[test]
    fn rotate_around_opposite_axis_undoes_rotate() {
        for axis in ALL_DIRECTIONS {
            for d in ALL_DIRECTIONS {
                let there = d.rotate(axis);
                let back = there.rotate(axis.opposite());
                assert_eq!(back, d, "axis {axis:?}, dir {d:?}");
            }
        }
    }

    /// Locks in the actual convention (right-hand rule, `+X` cycle reads
    /// `Y -> Z -> NegY -> NegZ`), not just its algebraic properties: the
    /// round-trip/fixed-point tests above would pass for a left-handed
    /// convention too.
    #[test]
    fn rotate_matches_the_right_hand_rule() {
        assert_eq!(Direction::Y.rotate(Direction::X), Direction::Z);
        assert_eq!(Direction::Z.rotate(Direction::X), Direction::NegY);
        assert_eq!(Direction::NegY.rotate(Direction::X), Direction::NegZ);
        assert_eq!(Direction::NegZ.rotate(Direction::X), Direction::Y);

        assert_eq!(Direction::Z.rotate(Direction::Y), Direction::X);
        assert_eq!(Direction::X.rotate(Direction::Y), Direction::NegZ);

        assert_eq!(Direction::X.rotate(Direction::Z), Direction::Y);
        assert_eq!(Direction::Y.rotate(Direction::Z), Direction::NegX);

        // The negative axis is the exact inverse cycle of its positive
        // counterpart, not an independent convention.
        assert_eq!(Direction::Z.rotate(Direction::NegX), Direction::Y);
        assert_eq!(Direction::Y.rotate(Direction::NegX), Direction::NegZ);
    }

    #[test]
    fn roll_cw_cycles_through_all_four_quarters() {
        assert_eq!(Roll::Deg0.cw(), Roll::Deg90);
        assert_eq!(Roll::Deg90.cw(), Roll::Deg180);
        assert_eq!(Roll::Deg180.cw(), Roll::Deg270);
        assert_eq!(Roll::Deg270.cw(), Roll::Deg0);
    }

    #[test]
    fn roll_ccw_is_the_reverse_cycle() {
        assert_eq!(Roll::Deg0.ccw(), Roll::Deg270);
        assert_eq!(Roll::Deg270.ccw(), Roll::Deg180);
        assert_eq!(Roll::Deg180.ccw(), Roll::Deg90);
        assert_eq!(Roll::Deg90.ccw(), Roll::Deg0);
    }

    /// `cw` and `ccw` are inverses of one another at every position, and
    /// four of either in a row is a full turn back to the start.
    #[test]
    fn roll_cw_and_ccw_are_inverses() {
        for r in ALL_ROLLS {
            assert_eq!(r.cw().ccw(), r, "{r:?}");
            assert_eq!(r.ccw().cw(), r, "{r:?}");
            assert_eq!(r.cw().cw().cw().cw(), r, "{r:?}");
            assert_eq!(r.ccw().ccw().ccw().ccw(), r, "{r:?}");
        }
    }
}
