//! The shroud (docs/plans/desert-game.md §4f) — the D-7 slice.
//!
//! # The spike, and why this is a lid rather than roxlap's fog
//!
//! §4f left the implementation open between two options. Both were
//! looked at; the answer is that they are built for different games.
//!
//! **roxlap's `FogOfWar` is a first-person vision model.** It has
//! exactly the state machine an RTS shroud wants — `Unseen` / `Memory`
//! / `Visible`, per cell, with decks — and monada already drives it for
//! the ship demo. But `FogOfWar::update` takes **one** observer, with a
//! facing cone, LOS occlusion and light gating, because that is what a
//! crew member looking down a corridor needs. An RTS shroud is the
//! opposite: fifty observers, no cone, no LOS, and permanent once
//! explored. Getting there means either an N-observer stamp upstream —
//! real work, in another crate, and not this slice's — or calling a full
//! LOS pass once per unit per frame.
//!
//! **A lid needs neither.** Opaque voxels over unexplored columns,
//! carved as you explore: one edit per cell, *once per match*. It
//! occludes correctly under a rotating camera because it is geometry, it
//! hides units behind it for the same reason, and it costs the renderer
//! nothing it was not already doing.
//!
//! What a lid cannot express is `Memory` — the dimmed, last-seen look of
//! ground you explored and are no longer watching. For this game that is
//! not a loss: **Dune II has no re-fog** (§4f), so explored ground stays
//! plainly visible, and a binary lid is exactly the model.
//!
//! # Where it lives
//!
//! Entirely in the **local** layer, and that is the whole of "a no-op
//! headless": the shroud is per-client presentation, it is painted
//! through bridge verbs that do nothing without a bridge, and no part of
//! it is reachable from the simulation. Two players on one lockstep
//! stream see different shrouds and are not desynced, because neither
//! peer's shroud is in the digest.
//!
//! Frame-rate independence falls out of the same shape: revealing is
//! idempotent and cumulative, so a cell explored at 30 Hz is explored at
//! 240 Hz, and the number of frames a unit stood somewhere changes
//! nothing.
//!
//! # Not yet
//!
//! The second deck of §4f — underground, revealed only where you have
//! presence — is not here. A lid is a surface idea; hiding an enemy
//! tunnel under explored ground needs the fog's per-deck mask, which
//! brings back the observer question above. Named rather than stubbed.

use monada_runtime::LocalHost;

use crate::MAP_CELLS;

/// What an unexplored column is painted as: black, and opaque.
pub const SHROUD_COLOR: i64 = 0x2010_1014;

/// How far a unit and a structure push the shroud back, in cells.
pub const UNIT_SIGHT: i64 = 10;
pub const BASE_SIGHT: i64 = 16;

/// One client's view of what it has explored.
#[derive(Clone, Debug)]
pub struct Shroud {
    /// One bit per column, `MAP_CELLS` wide.
    explored: Vec<u64>,
    /// The overlay grid the lid is painted into.
    lid: Option<i64>,
    /// The `z` each column's lid voxel sits at, so a reveal can rub out
    /// the cell it actually painted.
    ///
    /// Remembered rather than recomputed: the ground moves all match
    /// (§4d), and a lid cleared at today's surface height would leave
    /// yesterday's voxel behind — a black speck floating over a dune
    /// that has since slumped.
    height: Vec<i32>,
    laid: bool,
}

impl Default for Shroud {
    fn default() -> Self {
        Shroud::new()
    }
}

impl Shroud {
    #[must_use]
    pub fn new() -> Shroud {
        let columns = usize::try_from(MAP_CELLS * MAP_CELLS).unwrap_or(0);
        Shroud {
            explored: vec![0; columns.div_ceil(64)],
            lid: None,
            height: vec![0; columns],
            laid: false,
        }
    }

    /// Whether this client has ever seen a column.
    #[must_use]
    pub fn seen(&self, x: i64, y: i64) -> bool {
        Shroud::index(x, y)
            .is_some_and(|i| self.explored[i / 64] >> (i % 64) & 1 == 1)
    }

    /// How much of the map is explored, in columns — for the HUD and for
    /// tests.
    #[must_use]
    pub fn explored(&self) -> u32 {
        self.explored.iter().map(|w| w.count_ones()).sum()
    }

    /// Cover the map. Idempotent: the second call does nothing.
    ///
    /// The lid hugs the ground — one voxel a cell above each column's
    /// surface — rather than floating at a fixed ceiling. A high slab
    /// would work as occlusion and look wrong from a three-quarter view:
    /// the revealed parts would be holes you peer through at an angle,
    /// with the ground behind them still hidden. Hugging the terrain, the
    /// shroud simply looks like black ground, and exploring peels it off.
    pub fn lay(&mut self, host: &dyn LocalHost) {
        if self.laid {
            return;
        }
        self.laid = true;
        let grid = host.grid_overlay();
        if grid < 0 {
            return; // headless: nothing to paint on, and nothing to do
        }
        self.lid = Some(grid);
        for y in 0..MAP_CELLS {
            // Runs of equal height fill in one call. Dune relief changes
            // every few cells, so this is a handful of calls a row rather
            // than two hundred and fifty-six.
            let mut run_x = 0;
            let mut run_z = self.lid_z(host, 0, y);
            for x in 1..=MAP_CELLS {
                let z = if x < MAP_CELLS {
                    self.lid_z(host, x, y)
                } else {
                    i64::MIN
                };
                if z == run_z {
                    continue;
                }
                host.overlay_fill(
                    grid,
                    (run_x, y, run_z),
                    (x - 1, y, run_z),
                    SHROUD_COLOR,
                );
                run_x = x;
                run_z = z;
            }
        }
    }

    /// Push the shroud back around a point.
    pub fn reveal(&mut self, host: &dyn LocalHost, at: (i64, i64), radius: i64) {
        for y in (at.1 - radius)..=(at.1 + radius) {
            for x in (at.0 - radius)..=(at.0 + radius) {
                let (dx, dy) = (x - at.0, y - at.1);
                if dx * dx + dy * dy > radius * radius {
                    continue;
                }
                let Some(i) = Shroud::index(x, y) else { continue };
                if self.explored[i / 64] >> (i % 64) & 1 == 1 {
                    continue;
                }
                self.explored[i / 64] |= 1 << (i % 64);
                if let Some(grid) = self.lid {
                    host.overlay_clear(grid, x, y, i64::from(self.height[i]));
                }
            }
        }
    }

    /// The column index of a cell, or `None` off the map.
    fn index(x: i64, y: i64) -> Option<usize> {
        if !(0..MAP_CELLS).contains(&x) || !(0..MAP_CELLS).contains(&y) {
            return None;
        }
        usize::try_from(y * MAP_CELLS + x).ok()
    }

    /// Where this column's lid voxel goes, remembering it as we do.
    fn lid_z(&mut self, host: &dyn LocalHost, x: i64, y: i64) -> i64 {
        let z = host.volume_top(x, y).map_or(0, |(z, _)| z) + 1;
        if let Some(i) = Shroud::index(x, y) {
            self.height[i] = i32::try_from(z).unwrap_or(0);
        }
        z
    }
}
