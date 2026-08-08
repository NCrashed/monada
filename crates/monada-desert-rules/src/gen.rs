//! The desert generator (docs/plans/desert-game.md §9): a map from a
//! seed and a parameter block, not a hand-drawn level.
//!
//! Twenty-seven campaign missions plus infinite skirmish maps are not
//! authorable by hand without an editor, and the editor is deliberately
//! off this project's path. So the terrain is *computed*, which buys
//! three things beyond saving the drawing: the walk-rule invariants can
//! be asserted for every seed rather than eyeballed per level, a mission
//! is a parameter block small enough to read, and a skirmish map is free.
//!
//! Everything here is integer arithmetic. Not because floats would be
//! slow — because this runs inside `init` on every peer, and a map that
//! generates differently on two machines is a desync before the first
//! tick (DESIGN.md §3.1).

use crate::{CELLS_PER_TILE, MAP_CELLS};

/// What a column is made of, in the plan's material vocabulary (§4b).
/// The *shape* of the desert is a height per column; this is what that
/// height means for movement, for worms, and for what may be built on it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Surface {
    /// Worms swim through it, digs fast, bears nothing heavy.
    Sand,
    /// Rolling relief over sand — same material, slower going.
    Dune,
    /// Native bedrock: stable, buildable, slow to bore.
    Rock,
    /// Rises in steps a vehicle cannot climb but infantry can (§4b).
    Mountain,
    /// The harvestable surface layer.
    Spice,
}

impl Surface {
    /// `0xBB_RR_GG_BB` — the high byte is BRIGHTNESS, not alpha
    /// (a mix-up here is the engine's classic "everything is black").
    #[must_use]
    pub fn color(self) -> i64 {
        match self {
            Surface::Sand => 0x80c8_b48c,
            Surface::Dune => 0x80d8_c49c,
            Surface::Rock => 0x8078_6c60,
            Surface::Mountain => 0x8060_5850,
            Surface::Spice => 0x80c8_7830,
        }
    }

    /// Whether a worm can travel through it (§6e): loose sand only.
    /// This one predicate is why the factions' terraforming matters —
    /// packed fill, glass and rock are all worm-proof, and each faction
    /// reaches that state a different way.
    #[must_use]
    pub fn passable_to_worms(self) -> bool {
        matches!(self, Surface::Sand | Surface::Dune | Surface::Spice)
    }
}

/// A mission's terrain, as the numbers a designer actually tunes.
#[derive(Clone, Copy, Debug)]
pub struct DesertParams {
    pub seed: u32,
    /// How much of the map is rock shelf, in percent — buildable ground,
    /// so this is the map's "how much base can anyone have" dial.
    pub rock_percent: i64,
    /// Half-width of the mountain ridge in cells; `0` leaves the map open.
    pub ridge_half_width: i64,
    /// How many spice fields to scatter.
    pub spice_fields: i64,
}

impl Default for DesertParams {
    fn default() -> Self {
        DesertParams {
            seed: 0x51CE,
            rock_percent: 35,
            ridge_half_width: 14,
            spice_fields: 6,
        }
    }
}

/// Bedrock, mean surface, and sky — the vertical budget of §4a: room for
/// ~30 cells of digging below and ~30 of building above.
pub const BEDROCK_Z: i64 = 0;
pub const MEAN_SURFACE_Z: i64 = 32;
pub const SKY_Z: i64 = 64;

/// The step a mountain rises per cell. Larger than a vehicle's
/// `MAX_STEP` (2) and no larger than infantry's (4), which is the whole
/// of "mountains wall out armour and admit infantry" — no cliff markup,
/// no separate obstacle layer, just the heightmap and the walk rule.
pub const MOUNTAIN_STEP: i64 = 3;

/// How many candidate centres a spice field tries before giving up on
/// finding sand. Small: the map is mostly sand, so a handful of tries
/// finds one, and giving up quietly beats looping on a pathological
/// parameter block.
pub const SPICE_PLACEMENT_TRIES: u32 = 24;

/// A cheap integer hash. The generator's only source of variety, and the
/// reason it needs no floats.
///
/// Coordinates are truncated to their low 32 bits on the way in, which is
/// deliberate rather than tolerated: the map is 256 cells across, the
/// lattice is queried one cell outside it at most, and wrapping is what
/// makes a negative coordinate hash as well as a positive one.
#[must_use]
pub fn hash2(x: i64, y: i64, seed: u32) -> u32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (x, y) = (x as u32, y as u32);
    let mut h = x
        .wrapping_mul(0x8da6_b343)
        ^ y.wrapping_mul(0xd816_3841)
        ^ seed;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^ (h >> 15)
}

/// Bilinear value noise on a `period`-cell lattice, in `0..=255`.
/// Integer throughout: the interpolation is a weighted sum divided by
/// `period²`, so two machines cannot disagree about a dune.
#[must_use]
pub fn noise(x: i64, y: i64, period: i64, seed: u32) -> i64 {
    let (x0, y0) = (x.div_euclid(period), y.div_euclid(period));
    let (fx, fy) = (x.rem_euclid(period), y.rem_euclid(period));
    let corner = |dx: i64, dy: i64| i64::from(hash2(x0 + dx, y0 + dy, seed) & 0xff);
    let top = corner(0, 0) * (period - fx) + corner(1, 0) * fx;
    let bottom = corner(0, 1) * (period - fx) + corner(1, 1) * fx;
    (top * (period - fy) + bottom * fy) / (period * period)
}

/// The generated desert: what every peer computes from the same block.
pub struct Desert {
    params: DesertParams,
}

impl Desert {
    #[must_use]
    pub fn new(params: DesertParams) -> Desert {
        Desert { params }
    }

    #[must_use]
    pub fn params(&self) -> DesertParams {
        self.params
    }

    /// The surface height and material of one column.
    #[must_use]
    pub fn column(&self, x: i64, y: i64) -> (i64, Surface) {
        let p = self.params;
        let ridge = (x - MAP_CELLS / 2).abs();
        let ridge_span = MAP_CELLS / 6;
        if p.ridge_half_width > 0
            && ridge < p.ridge_half_width
            && y > ridge_span
            && y < MAP_CELLS - ridge_span
        {
            // The ridge climbs in MOUNTAIN_STEP stairs above a base that
            // is interpolated between its two feet — the open ground just
            // outside it on either side.
            //
            // Not the local dune height: a step built on that inherits the
            // column's ±1 of relief and can shrink to 2, which armour
            // climbs. Not one foot either — anchoring the whole ridge to
            // its left foot leaves the right flank hanging several cells
            // above the sand it meets, a wall nobody can climb. Sliding
            // between the two keeps every step exactly MOUNTAIN_STEP in
            // the middle and flush at both edges.
            let (left_x, right_x) = (MAP_CELLS / 2 - p.ridge_half_width, MAP_CELLS / 2 + p.ridge_half_width);
            let (left, right) = (
                self.open_column(left_x, y).0,
                self.open_column(right_x, y).0,
            );
            let span = right_x - left_x;
            let base = (left * (right_x - x) + right * (x - left_x)) / span;
            let steps = (p.ridge_half_width - 1 - ridge) / MOUNTAIN_STEP;
            return (base + steps * MOUNTAIN_STEP, Surface::Mountain);
        }
        self.open_column(x, y)
    }

    /// Everything that is not the ridge: rock shelves and dune seas. Split
    /// out because the ridge needs to ask what the ground beside it does
    /// without recursing into itself.
    fn open_column(&self, x: i64, y: i64) -> (i64, Surface) {
        let (height, rock) = self.ground(x, y);
        if rock {
            return (height, Surface::Rock);
        }
        if self.spice_at(x, y) {
            return (height, Surface::Spice);
        }
        if noise(x, y, 24, self.params.seed) - 128 > 40 {
            (height, Surface::Dune)
        } else {
            (height, Surface::Sand)
        }
    }

    /// Height and rock-ness of the open desert.
    ///
    /// **Elevation comes from the dunes and the ridge; rock adds none.**
    /// Shelves used to sit five cells above the sand, which read well on
    /// the default seed and made every shelf edge a cliff that neither
    /// armour *nor infantry* could climb — a map cut into islands by its
    /// own ground cover, which only a test across seeds was going to
    /// catch. Rock is a material, not a plateau: it is what a base may
    /// stand on and what a worm will not enter, and it lies a single step
    /// proud of the sand so the edge still reads on screen.
    fn ground(&self, x: i64, y: i64) -> (i64, bool) {
        let p = self.params;
        let dunes = noise(x, y, 24, p.seed) - 128; // −128..127
        let height = MEAN_SURFACE_Z - 2 + dunes / 32;
        let shelf = noise(x, y, 96, p.seed ^ 0x0f00_d1ce);
        let rock = shelf > 255 - (p.rock_percent * 255 / 100);
        (height + i64::from(rock), rock)
    }

    /// Whether a spice field covers this cell.
    ///
    /// Fields are round patches, and their centres are **resampled onto
    /// sand**: a centre that lands on a rock shelf is retried, up to
    /// [`SPICE_PLACEMENT_TRIES`] times, before the field is given up on.
    /// Scattering them blind looked fine on the default seed and left
    /// seed 1 with two harvestable cells on the whole map — an economy
    /// that cannot start is a map that cannot be played, and only a test
    /// over many seeds was ever going to notice.
    #[must_use]
    pub fn spice_at(&self, x: i64, y: i64) -> bool {
        for i in 0..self.params.spice_fields {
            let Some((cx, cy, radius)) = self.spice_field(i) else {
                continue;
            };
            let (dx, dy) = (x - cx, y - cy);
            if dx * dx + dy * dy <= radius * radius {
                return true;
            }
        }
        false
    }

    /// The `i`-th spice field's centre and radius, or `None` if no
    /// candidate centre found sand.
    fn spice_field(&self, i: i64) -> Option<(i64, i64, i64)> {
        let margin = 20;
        let span = u32::try_from(MAP_CELLS - 2 * margin).expect("the map is 256 cells wide");
        for attempt in 0..SPICE_PLACEMENT_TRIES {
            let h = hash2(i, i64::from(attempt), self.params.seed ^ 0x5217_0000);
            let cx = i64::from(h % span) + margin;
            let cy = i64::from((h >> 12) % span) + margin;
            // `open_column`, not `column`: asking `column` here would
            // recurse through `spice_at` and never terminate.
            let (_, surface) = self.open_ground_ignoring_spice(cx, cy);
            if matches!(surface, Surface::Sand | Surface::Dune) {
                let radius = 8 + i64::from((h >> 24) % 10);
                return Some((cx, cy, radius));
            }
        }
        None
    }

    /// The shelf-or-dune classification alone — the recursion guard for
    /// spice placement, which must know what the ground *would* be
    /// before spice is laid on it.
    fn open_ground_ignoring_spice(&self, x: i64, y: i64) -> (i64, Surface) {
        let (height, rock) = self.ground(x, y);
        if rock {
            (height, Surface::Rock)
        } else if noise(x, y, 24, self.params.seed) - 128 > 40 {
            (height, Surface::Dune)
        } else {
            (height, Surface::Sand)
        }
    }

    /// A player's start location, in cells: the centre of the `n`-th
    /// corner plateau. Rotationally symmetric so a skirmish is fair.
    #[must_use]
    pub fn start_location(&self, player: u32) -> (i64, i64) {
        let inset = MAP_CELLS / 8;
        match player % 4 {
            0 => (inset, inset),
            1 => (MAP_CELLS - inset, MAP_CELLS - inset),
            2 => (MAP_CELLS - inset, inset),
            _ => (inset, MAP_CELLS - inset),
        }
    }

    /// The gameplay tile a cell belongs to (§4a: four cells to a tile).
    #[must_use]
    pub fn tile_of(cell: i64) -> i64 {
        cell.div_euclid(CELLS_PER_TILE)
    }
}
