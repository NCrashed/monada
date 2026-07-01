//! Dual-grid marching-squares autotiling for textured terrain.
//!
//! A terrain is a per-cell grid of **type ids** (higher id = higher render
//! priority — it sits "on top"). Each ordered type pair `(low, high)` can
//! register a 16-tile transition sheet showing `high` blended over `low`.
//!
//! Rendering is **dual-grid**: a tile is drawn centred on every grid *vertex*
//! (the corner shared by four cells), so boundaries fall between cells, not
//! through them. The vertex's four surrounding cells give a 4-corner config;
//! the sheet tile for that config draws the blend. Where three+ types meet, we
//! pick the two highest-priority and approximate; where no sheet exists for a
//! pair, we fall back to a sharp per-quadrant edge.
//!
//! The sheet layout is the marching-squares convention reverse-engineered from
//! the reference art ([`CONFIG_TO_TILE`]): a 4×4 sheet whose sub-tile at the
//! mapped position has the matching corner fill. `config` bit order is
//! `TL=8, TR=4, BR=2, BL=1` (1 = the `high`/foreground type).

use std::collections::BTreeMap;

/// `config (0..16)` → linear sub-tile index (`row*4 + col`) in a 4×4 sheet.
/// Bit order `TL=8 TR=4 BR=2 BL=1`. Derived from the reference `grass-dirt`
/// sheet; both demo sheets share this layout. If a sheet uses a different
/// layout the transitions look scrambled — re-derive with the inspector.
pub const CONFIG_TO_TILE: [usize; 16] = [
    6, 10, 7, 9, 2, 4, 11, 15, 5, 1, 14, 8, 3, 13, 0, 12,
];

/// `config` from the four corner fills `[TL, TR, BR, BL]` (true =
/// `high`/foreground type).
#[must_use]
pub fn config(c: [bool; 4]) -> usize {
    usize::from(c[0]) << 3 | usize::from(c[1]) << 2 | usize::from(c[2]) << 1 | usize::from(c[3])
}

/// A loaded transition sheet: 16 `cell×cell` colour tiles indexed by config.
pub struct Transition {
    /// `tiles[config]` is a `cell*cell` row-major voxlap-colour grid.
    tiles: Vec<Vec<u32>>,
}

impl Transition {
    /// Slice a 4×4 sheet (`w×h` RGBA8, sub-tile = `w/4`) into the 16 configs,
    /// nearest-resampled to `cell×cell`. Pixels become voxlap `0x80RRGGBB`.
    #[must_use]
    pub fn from_sheet(rgba: &[u8], w: u32, h: u32, cell: usize) -> Transition {
        let sub = w / 4; // sub-tile size in px
        let at = |px: u32, py: u32| -> u32 {
            let i = ((py.min(h - 1) * w + px.min(w - 1)) * 4) as usize;
            0x8000_0000
                | (u32::from(rgba[i]) << 16)
                | (u32::from(rgba[i + 1]) << 8)
                | u32::from(rgba[i + 2])
        };
        let c = cell as u32;
        let mut tiles = vec![Vec::new(); 16];
        for (cfg, &lin) in CONFIG_TO_TILE.iter().enumerate() {
            let (tcol, trow) = ((lin % 4) as u32, (lin / 4) as u32);
            let (ox, oy) = (tcol * sub, trow * sub);
            let mut t = Vec::with_capacity(cell * cell);
            for ly in 0..c {
                for lx in 0..c {
                    t.push(at(ox + lx * sub / c, oy + ly * sub / c));
                }
            }
            tiles[cfg] = t;
        }
        Transition { tiles }
    }

    /// The solid colour grid of the `high` type (all-foreground config).
    #[must_use]
    pub fn solid_high(&self) -> &[u32] {
        &self.tiles[config([true, true, true, true])]
    }
    /// The solid colour grid of the `low` type (all-background config).
    #[must_use]
    pub fn solid_low(&self) -> &[u32] {
        &self.tiles[config([false, false, false, false])]
    }
    #[must_use]
    fn tile(&self, cfg: usize) -> &[u32] {
        &self.tiles[cfg]
    }
}

/// A terrain to autotile: per-cell types, registered transitions, and the
/// derived per-type solid textures.
#[derive(Default)]
pub struct Terrain {
    pub cells: BTreeMap<(i64, i64), i64>,
    /// Keyed by `(low, high)` (low < high by priority).
    pub transitions: BTreeMap<(i64, i64), Transition>,
    pub solids: BTreeMap<i64, Vec<u32>>,
    pub cell: usize,
}

impl Terrain {
    #[must_use]
    pub fn new(cell: usize) -> Terrain {
        Terrain {
            cell,
            ..Terrain::default()
        }
    }

    /// Register a transition sheet for `high` over `low`, and remember both
    /// types' solid textures.
    pub fn add_transition(&mut self, low: i64, high: i64, t: Transition) {
        self.solids.entry(low).or_insert_with(|| t.solid_low().to_vec());
        self.solids.entry(high).or_insert_with(|| t.solid_high().to_vec());
        self.transitions.insert((low.min(high), low.max(high)), t);
    }

    fn type_at(&self, x: i64, y: i64, base: i64) -> i64 {
        self.cells.get(&(x, y)).copied().unwrap_or(base)
    }

    /// Paint the floor over the cell bounding box (inclusive), invoking
    /// `put(floor_px_x, floor_px_y, colour)` for every floor voxel. Floor
    /// pixel space is sim-aligned: cell `c` spans `[c*cell, (c+1)*cell)`, a
    /// vertex tile is offset half a cell. `base` is the type of out-of-range
    /// cells (the surrounding fill).
    pub fn paint<F: FnMut(i64, i64, u32)>(
        &self,
        x0: i64,
        y0: i64,
        x1: i64,
        y1: i64,
        base: i64,
        mut put: F,
    ) {
        let cell = self.cell as i64;
        let half = cell / 2;
        // A tile is drawn at each vertex (i, j): the corner of cells (i-1,*)/(i,*).
        for j in y0..=y1 + 1 {
            for i in x0..=x1 + 1 {
                // Four cells around vertex (i, j): TL TR BR BL.
                let tl = self.type_at(i - 1, j - 1, base);
                let tr = self.type_at(i, j - 1, base);
                let br = self.type_at(i, j, base);
                let bl = self.type_at(i - 1, j, base);
                let tile = self.vertex_tile(tl, tr, br, bl, base);
                let (ox, oy) = (i * cell - half, j * cell - half);
                for ly in 0..cell {
                    for lx in 0..cell {
                        let color = match &tile {
                            Tile::Flat(t) => t[(ly * cell + lx) as usize],
                            // Sharp fallback: colour each quadrant by its cell.
                            Tile::Sharp([qtl, qtr, qbr, qbl]) => {
                                let q = if lx < half {
                                    if ly < half { qtl } else { qbl }
                                } else if ly < half {
                                    qtr
                                } else {
                                    qbr
                                };
                                self.solids.get(q).map_or(0x8080_8080, |s| {
                                    s[(ly * cell + lx) as usize]
                                })
                            }
                        };
                        put(ox + lx, oy + ly, color);
                    }
                }
            }
        }
    }

    fn vertex_tile(&self, tl: i64, tr: i64, br: i64, bl: i64, _base: i64) -> Tile {
        let mut types = [tl, tr, br, bl];
        types.sort_unstable();
        let distinct: Vec<i64> = {
            let mut d = types.to_vec();
            d.dedup();
            d
        };
        if distinct.len() == 1 {
            // Uniform vertex: solid of that type.
            if let Some(s) = self.solids.get(&distinct[0]) {
                return Tile::Flat(s.clone());
            }
            return Tile::Sharp([tl, tr, br, bl]);
        }
        // Two highest-priority types present.
        let high = *distinct.last().unwrap();
        let low = distinct[distinct.len() - 2];
        let bit = |t: i64| t == high;
        let cfg = config([bit(tl), bit(tr), bit(br), bit(bl)]);
        match self.transitions.get(&(low.min(high), low.max(high))) {
            Some(t) => Tile::Flat(t.tile(cfg).to_vec()),
            None => Tile::Sharp([tl, tr, br, bl]),
        }
    }
}

enum Tile {
    Flat(Vec<u32>),
    Sharp([i64; 4]),
}
