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

use monada_fixed::{trig, Fixed, FixedVec3};
use monada_runtime::{Host, LocalHost, LocalRules, MapRules};
use monada_sim::ArchetypeId;

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
    /// The archetype `init` registered for vehicles — a handle, re-derived
    /// identically on every peer, not hashed state.
    vehicle_kind: Option<ArchetypeId>,
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
            vehicle_kind: None,
        }
    }

    /// Where a vehicle standing on cell `(x, y)` sits: the first solid
    /// cell scanned down from the sky. The STORE is asked, not the
    /// generator, because terraforming will make them differ the moment
    /// a faction touches the ground (§6).
    fn seat(host: &dyn Host, x: i64, y: i64) -> FixedVec3 {
        let mut z = gen::SKY_Z;
        while z > gen::BEDROCK_Z && !host.volume_solid(x, y, z) {
            z -= 1;
        }
        FixedVec3::new(
            Fixed::from_int(i32::try_from(x).unwrap_or(0)),
            Fixed::from_int(i32::try_from(y).unwrap_or(0)),
            Fixed::from_int(i32::try_from(z + 1).unwrap_or(0)),
        )
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

        // One vehicle, so D-1 has something that moves over the dunes and
        // proves the seat-on-terrain maths. Orders and pathfinding are
        // D-2; this one drives a fixed heading and turns at the edges.
        let kind = host.archetype(&["heading"]);
        self.vehicle_kind = Some(kind);
        let model = host.model_box(24, 16, 10, Surface::Rock.color());
        let (sx, sy) = self.desert.start_location(0);
        let vehicle = host.entity_create(kind);
        host.entity_set_model(vehicle, model);
        host.entity_set_field(vehicle, "heading", Fixed::ZERO);
        host.entity_set_position(vehicle, Self::seat(host, sx, sy));

        host.set_light(
            FixedVec3::new(
                Fixed::from_ratio(-7, 10),
                Fixed::from_ratio(-5, 10),
                Fixed::from_ratio(6, 10),
            ),
            Fixed::from_int(1),
        );
    }

    fn tick(&mut self, host: &dyn Host, _dt: Fixed) {
        let Some(kind) = self.vehicle_kind else {
            return;
        };
        for vehicle in host.entities_of(kind) {
            let pos = host.entity_position(vehicle);
            let heading = host.entity_field(vehicle, "heading");
            let step = Fixed::from_ratio(1, 8); // cells per tick
            let (nx, ny) = (
                pos.x + trig::cos(heading) * step,
                pos.y + trig::sin(heading) * step,
            );
            // Turn rather than drive off the map. A quarter turn keeps the
            // patrol inside the sand without any pathfinding, which is
            // D-2's job.
            let margin = Fixed::from_int(8);
            let limit = Fixed::from_int(i32::try_from(MAP_CELLS).unwrap_or(i32::MAX)) - margin;
            if nx < margin || ny < margin || nx > limit || ny > limit {
                host.entity_set_field(vehicle, "heading", heading + trig::FRAC_PI_2);
                continue;
            }
            let (cx, cy) = (nx.floor_to_int(), ny.floor_to_int());
            let seat = Self::seat(host, i64::from(cx), i64::from(cy));
            host.entity_set_position(vehicle, FixedVec3::new(nx, ny, seat.z));
            host.entity_set_facing(vehicle, heading);
        }
    }
}

/// The desert's **local** layer: this client's camera and, later, its
/// selection and orders.
///
/// It holds the camera's own state, which is exactly the kind of thing
/// that must never be hashed — where a player is looking is not part of
/// the world, and two peers looking at different corners is not a desync.
/// The type system already enforces that: a [`LocalHost`] cannot reach a
/// mutator.
pub struct DesertLocal {
    yaw: Fixed,
    pitch: Fixed,
    dist: Fixed,
    /// The z band the camera shows. The whole world for now; the depth
    /// slider that cuts down to a Dweller bore is the same two numbers
    /// (§4f).
    clip: (i64, i64),
}

impl Default for DesertLocal {
    fn default() -> Self {
        DesertLocal {
            // A steep three-quarter view: WC3's angle, which is what makes
            // a voxel silhouette read as a vehicle rather than a smudge.
            yaw: Fixed::from_ratio(9, 10),
            pitch: Fixed::from_ratio(105, 100),
            dist: Fixed::from_int(700),
            clip: (gen::BEDROCK_Z, gen::SKY_Z),
        }
    }
}

impl LocalRules for DesertLocal {
    fn local_init(&mut self, host: &dyn LocalHost) {
        host.camera_angle(self.yaw, self.pitch);
        host.camera_dist(self.dist);
        host.deck_clip(self.clip.0, self.clip.1);
        host.status("the desert — D-1");
    }

    fn local_frame(&mut self, host: &dyn LocalHost, _dt: Fixed) {
        // Follow whatever is moving. One vehicle today; a selection
        // tomorrow (D-3), which is why the camera reads the world rather
        // than being told where to look.
        if let Some(&vehicle) = host.entities().first() {
            host.camera_focus(host.entity_position(vehicle));
        }
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
