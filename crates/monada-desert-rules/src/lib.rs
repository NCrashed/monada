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

use monada_fixed::{Fixed, FixedVec3};
use monada_runtime::{
    Host, LocalHost, LocalRules, MapRules, MoverProfile, Repose,
};
use monada_sim::{ArchetypeId, EntityId};
use std::collections::BTreeMap;

pub mod economy;
pub mod gen;
pub mod harvest;
pub mod mover;
pub mod terraform;

pub use economy::{Building, Economy, Player, PlayerNo, Structure};
pub use gen::{Desert, DesertParams, Surface};
pub use harvest::{Fleet, Yield};
pub use mover::Router;
pub use terraform::{JobId, Spent, Terraform, Work};

/// What a player starts a skirmish with. Dune II's number.
pub const STARTING_CREDITS: u32 = 1_000;

/// Sand's angle of repose, as the steepest drop it will hold: one cell
/// (§4d). Spice behaves the same — it is dust. Everything else on the
/// map is stable at any slope, which is precisely what makes rock worth
/// standing on, packed fill worth manufacturing and glass worth firing.
pub const SAND_REPOSE: Repose = Repose { max_drop: 1 };

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

    /// What a material is painted as.
    ///
    /// The store holds the material and the screen needs a colour; a
    /// terraform verb that changed one without the other would leave a
    /// lie on the ground — glass you can drive on but that still looks
    /// like sand.
    #[must_use]
    pub fn color(material: MaterialId) -> i64 {
        match material.0 {
            1 => 0x8078_6c60, // rock
            2 => 0x80c8_7830, // spice
            3 => 0x80a8_a098, // packed fill: grey, deliberately man-made
            4 => 0x8090_c8d8, // glass: pale blue, and unmistakable
            _ => 0x80c8_b48c, // sand
        }
    }

    /// What comes out of the ground when you dig it up.
    ///
    /// **Works break back into sand.** Packed fill and glass are states
    /// sand was put into, and excavating them undoes that — so a Dweller
    /// bore through a Surfling causeway leaves a loose heap that slumps,
    /// not a neat stack of blocks. Rock and spice come out as themselves:
    /// rubble still will not flow, and spice is still worth collecting
    /// wherever it ends up.
    #[must_use]
    pub fn spoil_of(material: MaterialId) -> MaterialId {
        match material.0 {
            1 => ROCK,
            2 => SPICE,
            _ => SAND,
        }
    }
}

/// Armour: three cells of clearance, climbs two, no tunnels — the
/// profile that makes the ridge a wall (§4b) and Dweller bores a private
/// road (§6b).
pub const VEHICLE: MoverProfile = MoverProfile {
    height: 3,
    max_step: VEHICLE_MAX_STEP,
    tunnels: false,
};

/// Infantry: short, climbs four, and may use a bore.
pub const INFANTRY: MoverProfile = MoverProfile {
    height: 2,
    max_step: INFANTRY_MAX_STEP,
    tunnels: true,
};

/// The desert game.
pub struct DesertRules {
    desert: Desert,
    /// Where the generator put spice, surface and buried — the discs the
    /// harvesters search. Derived from the parameter block, so every peer
    /// computes the same list; cached because it is the same every tick.
    sites: Vec<harvest::Site>,
    /// Retained routes, per mover — the state a Rhai map could not hold
    /// (§3c). Hashed like everything else reachable from the rules.
    router: Router,
    /// Terraform orders in flight and the allowance they share (§4e).
    terraform: Terraform,
    /// Credits, storage and power, per player (§7).
    economy: Economy,
    /// Every structure standing, and whose it is.
    buildings: BTreeMap<EntityId, Building>,
    /// Every harvester in service.
    fleet: Fleet,
    /// Which corner the D-1 patrol vehicle is currently crossing to.
    patrol_goal: Option<(i64, i64)>,
    /// The last tick's terraform charge and harvest, for the HUD to show
    /// and a test to assert on. Derived, not state: recomputed each tick.
    spent: Spent,
    harvested: Yield,
    /// Archetypes and model ids `init` registered — handles, re-derived
    /// identically on every peer, not hashed state.
    vehicle_kind: Option<ArchetypeId>,
    unit_kind: Option<ArchetypeId>,
    building_kind: Option<ArchetypeId>,
}

impl Default for DesertRules {
    fn default() -> Self {
        Self::new(DesertParams::default())
    }
}

impl DesertRules {
    #[must_use]
    pub fn new(params: DesertParams) -> DesertRules {
        let desert = Desert::new(params);
        DesertRules {
            sites: desert.spice_sites(),
            desert,
            router: Router::new(),
            terraform: Terraform::new(),
            economy: Economy::new(),
            buildings: BTreeMap::new(),
            fleet: Fleet::new(),
            patrol_goal: None,
            spent: Spent::default(),
            harvested: Yield::default(),
            vehicle_kind: None,
            unit_kind: None,
            building_kind: None,
        }
    }

    /// The terraform queue, for orders and for tests.
    pub fn terraform(&mut self) -> &mut Terraform {
        &mut self.terraform
    }

    /// The economy, for the HUD and for tests.
    #[must_use]
    pub fn economy(&self) -> &Economy {
        &self.economy
    }

    /// The harvester fleet, for the HUD and for tests.
    #[must_use]
    pub fn fleet(&self) -> &Fleet {
        &self.fleet
    }

    /// What the last tick's terraforming cost.
    #[must_use]
    pub fn spent(&self) -> Spent {
        self.spent
    }

    /// What the last tick's harvesting brought in.
    #[must_use]
    pub fn harvested(&self) -> Yield {
        self.harvested
    }

    /// Where a vehicle standing on cell `(x, y)` sits: the first solid
    /// cell scanned down from the sky. The STORE is asked, not the
    /// generator, because terraforming will make them differ the moment
    /// a faction touches the ground (§6).
    fn seat(host: &dyn Host, x: i64, y: i64) -> FixedVec3 {
        let z = Self::ground_under(host, x, y);
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
                let base = match surface {
                    Surface::Sand | Surface::Dune | Surface::Spice => material::SAND,
                    Surface::Rock | Surface::Mountain => material::ROCK,
                };
                // Spice is a CRUST over sand, not a column of spice down
                // to bedrock (§7). Painted the other way, one cell of
                // field was worth thirty cells of ore — and, worse, a
                // harvester working it cut a thirty-deep shaft under
                // itself and stranded, because how deep the seam runs is
                // also how deep the hole is.
                let crust = if surface == Surface::Spice {
                    gen::SPICE_CRUST
                } else {
                    0
                };
                let vein = self.desert.vein_at(x, y);
                if crust == 0 && vein.is_none() {
                    host.volume_fill(
                        (x, y, gen::BEDROCK_Z),
                        (x, y, height),
                        base,
                        surface.color(),
                    );
                    continue;
                }
                // A layered column is painted in its runs, not as one box
                // overpainted. The store would take the overpaint — a
                // later write wins — but the render grid would not: a
                // paint over an existing solid does not recolour it, so
                // the seam would be ore you can harvest and sand you can
                // see. Only the columns that have a seam pay for this.
                let seam = |z: i64| {
                    z > height - crust || vein.is_some_and(|(lo, hi)| z >= lo && z <= hi)
                };
                let mut lo = gen::BEDROCK_Z;
                let mut ore = seam(gen::BEDROCK_Z);
                for z in (gen::BEDROCK_Z + 1)..=(height + 1) {
                    if z <= height && seam(z) == ore {
                        continue;
                    }
                    let (mat, color) = if ore {
                        (material::SPICE, Surface::Spice.color())
                    } else {
                        (base, surface.color())
                    };
                    host.volume_fill((x, y, lo), (x, y, z - 1), mat, color);
                    lo = z;
                    ore = z <= height && seam(z);
                }
            }
        }
    }
}

impl MapRules for DesertRules {
    fn init(&mut self, host: &dyn Host) {
        self.paint(host);

        // Declare what flows — AFTER the paint, and that order is the
        // design, not an optimisation. Painting is 65k edits, and every
        // edit disturbs its column; register first and the whole desert
        // wakes up on tick 1 with nothing to do, burning the allowance
        // for twenty ticks. Registering second states the rule plainly:
        // the desert as generated is at rest, and what moves is what
        // somebody touched. (The generator earns that: its dune gradient
        // never exceeds a cell, which `terraform.rs`'s repose test
        // checks rather than assumes.)
        host.granular_register(material::SAND, SAND_REPOSE);
        host.granular_register(material::SPICE, SAND_REPOSE);

        // One vehicle, so D-1 has something that moves over the dunes and
        // proves the seat-on-terrain maths. Orders and pathfinding are
        // D-2; this one crosses the map corner to corner forever.
        let kind = host.archetype(&["heading"]);
        self.vehicle_kind = Some(kind);
        let model = host.model_box(24, 16, 10, Surface::Rock.color());
        let (sx, sy) = self.desert.start_location(0);
        let vehicle = host.entity_create(kind);
        host.entity_set_model(vehicle, model);
        host.entity_set_field(vehicle, "heading", Fixed::ZERO);
        host.entity_set_position(vehicle, Self::seat(host, sx, sy));

        self.found_base(host, 0, (sx + 6, sy + 6));

        host.set_light(
            FixedVec3::new(
                Fixed::from_ratio(-7, 10),
                Fixed::from_ratio(-5, 10),
                Fixed::from_ratio(6, 10),
            ),
            Fixed::from_int(1),
        );

        if self.desert.params().proving_ground {
            self.proving_ground(host, sx, sy);
        }
    }

    fn tick(&mut self, host: &dyn Host, _dt: Fixed) {
        // Power before work, because power is what work costs (§4e): the
        // buildings standing at the top of the tick decide how fast the
        // engineers dig during it.
        self.economy.begin_tick();
        self.economy.count(self.buildings.values());
        let allowance = self.economy.player(0).allowance();

        // Then the ground: settling and the terraform orders both change
        // what the movers below are about to walk on, and a unit that
        // steps onto a cell the same tick it collapses should fall, not
        // hover. Doing it in the other order would hide that by one tick.
        self.spent = self.terraform.run(host, allowance);

        self.harvested = self.fleet.run(
            host,
            &mut self.economy,
            &mut self.router,
            &self.sites,
            VEHICLE,
        );
        self.economy.end_tick();
        self.report(host);

        let Some(kind) = self.vehicle_kind else {
            return;
        };
        for vehicle in host.entities_of(kind) {
            self.patrol(host, vehicle);
        }
    }

    /// The rules' own hashed state (§3c): retained routes, the terraform
    /// queue, the economy, the buildings and the fleet. Every one of them
    /// is something a Rhai map could not have held, and every one is
    /// something two peers must agree on to the cell — a route decides
    /// where a unit will be next second, a job decides what the ground
    /// will be, a credit decides what gets built.
    ///
    /// The generator is not in here: it is a pure function of the
    /// parameter block every peer already has, so serializing it would
    /// only be a slower way of agreeing on what is already agreed. Nor
    /// are the archetype and model handles, which are re-derived by
    /// `init` identically everywhere.
    fn snapshot(&self) -> Vec<u8> {
        postcard::to_stdvec(&(
            &self.router,
            &self.terraform,
            &self.economy,
            &self.buildings,
            &self.fleet,
        ))
        .unwrap_or_default()
    }

    fn restore(&mut self, bytes: &[u8]) {
        if let Ok((router, terraform, economy, buildings, fleet)) = postcard::from_bytes(bytes) {
            self.router = router;
            self.terraform = terraform;
            self.economy = economy;
            self.buildings = buildings;
            self.fleet = fleet;
        }
    }
}

impl DesertRules {
    /// Three faction verbs and a shell, laid out beside the starting
    /// position so the whole of D-3 is visible in the first second of a
    /// run: a Surfling berm going up with sheer sides, a Dweller trench
    /// going down beside its slumping spoil heap, a Binder road turning
    /// the dunes to glass without moving a grain of them, and a crater
    /// whose rim falls in while you watch.
    ///
    /// A demonstration, and honest about being one — the units that will
    /// order these are D-5's, and the orders are the same orders.
    fn proving_ground(&mut self, host: &dyn Host, sx: i64, sy: i64) {
        let ground = Self::ground_under(host, sx, sy);
        let work = &mut self.terraform;

        work.order(
            (sx + 10, sy - 6),
            (sx + 15, sy + 5),
            Work::Raise { level: ground + 5 },
        );
        work.order(
            (sx - 12, sy - 8),
            (sx - 11, sy + 7),
            Work::Dig {
                level: ground - 5,
                spoil: (sx - 16, sy),
            },
        );
        work.order((sx - 4, sy - 10), (sx - 1, sy + 9), Work::Vitrify { depth: 2 });
        Terraform::crater(host, (sx, sy + 22), 8);
    }

    /// Found a player's starting base: a refinery, a silo to overflow
    /// into, a wind trap to pay for the digging, and one harvester.
    ///
    /// The gate for this slice is "a scripted schedule mines an exact
    /// credit total at an exact tick" — this is the schedule, and D-5
    /// replaces it with an MCV the player deploys where they like.
    fn found_base(&mut self, host: &dyn Host, owner: PlayerNo, at: (i64, i64)) {
        let buildings = *self
            .building_kind
            .get_or_insert_with(|| host.archetype(&["owner", "kind"]));
        let units = *self
            .unit_kind
            .get_or_insert_with(|| host.archetype(&["owner", "load"]));

        self.economy.found(owner, STARTING_CREDITS);

        let refinery = (at.0, at.1);
        for (kind, cell, model) in [
            (
                Structure::Refinery,
                refinery,
                host.model_box(40, 40, 20, 0x80a8_a098),
            ),
            (
                Structure::Silo,
                (at.0 + 5, at.1),
                host.model_box(24, 24, 14, 0x8090_8078),
            ),
            (
                Structure::WindTrap,
                (at.0, at.1 + 5),
                host.model_box(24, 24, 26, 0x8060_8ca0),
            ),
        ] {
            let e = host.entity_create(buildings);
            host.entity_set_model(e, model);
            host.entity_set_field(e, "owner", Fixed::from_int(i32::from(owner)));
            host.entity_set_position(e, Self::seat(host, cell.0, cell.1));
            self.buildings.insert(e, Building { owner, kind });
        }

        let model = host.model_box(28, 20, 12, 0x80c8_9840);
        let harvester = host.entity_create(units);
        host.entity_set_model(harvester, model);
        host.entity_set_field(harvester, "owner", Fixed::from_int(i32::from(owner)));
        host.entity_set_position(harvester, Self::seat(host, at.0 + 3, at.1 + 3));
        self.fleet.enlist(harvester, owner, refinery);
    }

    /// The HUD line: whichever of the two loops is doing something.
    fn report(&self, host: &dyn Host) {
        let Some(p) = self.economy.get(0) else {
            return;
        };
        if self.spent.total() > 0 {
            host.status(&format!(
                "terraforming — {} cells cut and filled, {} settling, {} orders, {}% power",
                self.spent.edited,
                self.spent.settled,
                self.terraform.pending(),
                p.satisfaction()
            ));
        } else {
            host.status(&format!(
                "{} credits of {} — {} power made, {} drawn{}",
                p.credits,
                p.capacity,
                p.made,
                p.used,
                if p.spilled > 0 {
                    format!(" — {} spilled, build a silo", p.spilled)
                } else {
                    String::new()
                }
            ));
        }
    }

    /// D-1's patrol, now on the shared router: cross the map, corner to
    /// corner, forever — over the ridge for infantry, around it for
    /// armour, which is the whole point of having a pathfinder at all.
    fn patrol(&mut self, host: &dyn Host, vehicle: EntityId) {
        // **The goal is remembered, not recomputed.** Deriving it from
        // "the far corner I am not near" reads fine and is a trap: the
        // comparison flips at the midpoint of the map, so the vehicle
        // turns around there — and, because a changed goal invalidates
        // the retained route, re-plans a full-map path every single tick
        // for the rest of the match. Measured at 3.5 ms a tick, which is
        // ten percent of the budget spent on one demo vehicle changing
        // its mind. A patrol has an order, not a preference.
        let (a, b) = (self.desert.start_location(0), self.desert.start_location(1));
        let goal = *self.patrol_goal.get_or_insert(b);
        let step = self
            .router
            .step(host, vehicle, goal, VEHICLE, Fixed::from_ratio(1, 8));
        if step != mover::Step::Moving {
            self.patrol_goal = Some(if goal == b { a } else { b });
        }
    }

    /// The ground height under a cell, from the store.
    ///
    /// One host call, not the sixty-four a scan down from the sky used to
    /// cost: the store walks its own chunks from the top of the column it
    /// actually has (`host_api` 18). On a column of nothing but air the
    /// answer is bedrock, which keeps a unit driven off the map's edge on
    /// the floor rather than at `SKY_Z`.
    fn ground_under(host: &dyn Host, x: i64, y: i64) -> i64 {
        host.volume_top(x, y).map_or(gen::BEDROCK_Z, |(z, _)| z)
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
