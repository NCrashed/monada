//! The desert game's rules — compiled Rust against
//! [`monada_runtime::Host`] (docs/plans/desert-game.md, decision L1).
//!
//! This crate is the game; `monada-desert` beside it is a launcher that
//! bundles the assets and hands both to the host. Keeping the rules in a
//! library with no window, no renderer and no host loop is what lets the
//! whole of D-1 be tested headless — and is the shape that compiles to
//! wasm later (§3f) without touching a line of gameplay.
//!
//! **The determinism contract** (§3c). Everything reachable from
//! [`DesertRules`] is simulation state: no floats, no `HashMap`
//! iteration, no clock, no `rand`, no I/O, no threads, and randomness
//! only from the host's seeded generator. Rhai made those impossible to
//! express; compiled Rust only makes them forbidden, so the lints below
//! and the oracle stand in for what the interpreter used to guarantee.

#![deny(clippy::float_arithmetic)]
#![forbid(unsafe_code)]
// Terse coordinate names are the domain's own (`x`, `y`, `z`, `lo`, `hi`).
#![allow(clippy::many_single_char_names, clippy::similar_names)]

use monada_runtime::{Host, MapRules};

pub mod gen;

pub use gen::{Desert, DesertParams, Surface};

/// Sim cells per gameplay tile (§4a). The rules reason in tiles — a
/// building is 2×2 or 3×2 of them, a unit occupies one — while the
/// terrain is four times finer, which is the resolution a trench wall, a
/// berm slope and a bore mouth need to read as themselves.
pub const CELLS_PER_TILE: i64 = 4;

/// The map, in gameplay tiles: Dune II's size, unchanged.
pub const MAP_TILES: i64 = 64;

/// The map, in sim cells.
pub const MAP_CELLS: i64 = MAP_TILES * CELLS_PER_TILE;

/// Material ids the rules register, in registration order. The FIRST one
/// registered is the ground material and must precede any tick that can
/// bring a body into terrain contact — the material-0 contract the
/// physics sim asserts on (`host_api` 8).
pub mod material {
    use monada_runtime::MaterialId;

    pub const SAND: MaterialId = MaterialId(0);
    pub const ROCK: MaterialId = MaterialId(1);
    pub const SPICE: MaterialId = MaterialId(2);
    /// Surfling product: sand fixed into something that bears a refinery
    /// and turns a worm away (§6a).
    pub const PACKED_FILL: MaterialId = MaterialId(3);
    /// Binder product: fast, worm-proof and brittle (§6c).
    pub const GLASS: MaterialId = MaterialId(4);
}

/// The desert game.
pub struct DesertRules {
    desert: Desert,
}

impl Default for DesertRules {
    fn default() -> Self {
        Self::new(DesertParams::default())
    }
}

impl DesertRules {
    #[must_use]
    pub fn new(params: DesertParams) -> DesertRules {
        DesertRules {
            desert: Desert::new(params),
        }
    }

    /// The generated desert, for tests and for the launcher's preview.
    #[must_use]
    pub fn desert(&self) -> &Desert {
        &self.desert
    }

    /// Raise the whole map out of the volume store, one column at a time.
    ///
    /// A column is ONE `volume_fill`, not a cell-by-cell walk: the store
    /// batches its per-chunk rehash per call, and the spike measured the
    /// difference at two orders of magnitude — 0.07 µs a cell in bulk
    /// against 7.01 one at a time (§13a). At 65k columns that is the
    /// difference between a mission loading and a mission hanging.
    fn paint(&self, host: &dyn Host) {
        for y in 0..MAP_CELLS {
            for x in 0..MAP_CELLS {
                let (height, surface) = self.desert.column(x, y);
                let material = match surface {
                    Surface::Sand | Surface::Dune => material::SAND,
                    Surface::Rock | Surface::Mountain => material::ROCK,
                    Surface::Spice => material::SPICE,
                };
                host.volume_fill(
                    (x, y, gen::BEDROCK_Z),
                    (x, y, height),
                    material,
                    surface.color(),
                );
            }
        }
    }
}

impl MapRules for DesertRules {
    fn init(&mut self, host: &dyn Host) {
        self.paint(host);
    }
}

/// Whether a mover with this profile can step between two heights — the
/// **one** walk rule (§4b), shared by movement, by the pathfinder and by
/// the generator's own invariant tests, so what a unit can walk is
/// exactly what routes and exactly what the map promises.
#[must_use]
pub fn can_step(from_height: i64, to_height: i64, max_step: i64) -> bool {
    (to_height - from_height).abs() <= max_step
}

/// A vehicle's climb: two cells. Mountain stairs rise three, so armour
/// is walled out of them without a single line of obstacle markup.
pub const VEHICLE_MAX_STEP: i64 = 2;
/// Infantry's climb: four cells, which clears the same stairs.
pub const INFANTRY_MAX_STEP: i64 = 4;
