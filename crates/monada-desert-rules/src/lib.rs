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
use monada_runtime::{
    Host, LocalHost, LocalRules, MapRules, MoverProfile, Repose,
};
use monada_sim::{ArchetypeId, EntityId};
use std::collections::BTreeMap;

pub mod build;
pub mod combat;
pub mod economy;
pub mod gen;
pub mod harvest;
pub mod shroud;
pub mod mover;
pub mod terraform;

pub use combat::{Armour, Battle, Weapon};
pub use build::{Blueprint, Exposure, Queue, Refusal, Standing, Yards};
pub use economy::{Building, Economy, Player, PlayerNo, Structure};
pub use gen::{Desert, DesertParams, Surface};
pub use harvest::{Fleet, Yield};
pub use mover::Router;
pub use shroud::Shroud;
pub use terraform::{JobId, Spent, Terraform, Work};

/// What a player starts a skirmish with. Dune II's number.
pub const STARTING_CREDITS: u32 = 1_000;

/// Health mended per repair command, in tenths of a point.
pub const REPAIR_RATE: u32 = 40;

/// How far an MCV will look for a pad it can deploy on.
pub const DEPLOY_SEARCH: i64 = 12;

/// The command verbs this map's local layer submits (§7).
pub mod verb {
    /// Put a structure on the build line. `arg.z` names the kind.
    pub const ORDER: u32 = 1;
    /// Place whatever is finished at `arg`.
    pub const PLACE: u32 = 2;
    /// Deploy the MCV named by `target`.
    pub const DEPLOY: u32 = 3;
    /// Mend the structure named by `target`.
    pub const REPAIR: u32 = 4;
    /// Send `target` to `arg` — the right-click order.
    pub const MOVE: u32 = 5;
}

/// The fields the simulation publishes for the HUD to read.
///
/// A local layer cannot see simulation state and must not model it: a
/// sidebar that kept its own credit total would be a second, disagreeing
/// account of the money. So the numbers ride an entity, which both
/// layers read through the same `entity_field`, and the HUD becomes a
/// view of the world rather than a parallel copy of it.
pub mod dash {
    /// Marks the entity that carries the rest. Scanned for by value
    /// rather than by archetype, because looking an archetype up is the
    /// simulation's business.
    pub const MARK: &str = "hud";
    pub const CREDITS: &str = "credits";
    pub const CAPACITY: &str = "capacity";
    pub const MADE: &str = "made";
    pub const USED: &str = "used";
    /// The catalogue index waiting to be placed, or `-1`.
    pub const READY: &str = "ready";
    /// Percent complete of whatever is on the line, or `-1`.
    pub const PROGRESS: &str = "progress";
}

/// The catalogue index a command carries, back to a kind.
#[must_use]
fn kind_of(index: i32) -> Option<Structure> {
    build::CATALOGUE.get(usize::try_from(index).ok()?).map(|b| b.kind)
}

/// `(w, h, d, colour)` of each structure's placeholder model. Real art is
/// D-11; what these have to do today is be different from each other at
/// a glance.
fn model_of(kind: Structure) -> (i64, i64, i64, i64) {
    match kind {
        Structure::Yard => (56, 56, 26, 0x80b0_a890),
        Structure::Refinery => (56, 56, 20, 0x80a8_a098),
        Structure::Silo => (28, 28, 16, 0x8090_8078),
        Structure::WindTrap => (28, 28, 30, 0x8060_8ca0),
    }
}

const MODEL_MCV: (i64, i64, i64, i64) = (32, 24, 16, 0x80b8_9060);
const MODEL_HARVESTER: (i64, i64, i64, i64) = (28, 20, 12, 0x80c8_9840);
const MODEL_SOLDIER: (i64, i64, i64, i64) = (14, 14, 18, 0x80c0_5040);

/// The middle of a site's footprint.
fn site_centre(site: build::Site) -> (i64, i64) {
    (site.at.0 + site.span / 2, site.at.1 + site.span / 2)
}

/// A refusal, as a player should read it.
fn refusal(why: build::Refusal) -> &'static str {
    match why {
        build::Refusal::NoGround => "there is no ground there",
        build::Refusal::TooSteep => "the ground is too steep — grade a pad first",
        build::Refusal::Obstructed => "something is in the way",
        build::Refusal::Unconnected => "too far from the rest of the base",
        build::Refusal::Occupied => "something already stands there",
    }
}

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
    /// Every structure standing, plus each player.s build line (§7).
    yards: Yards,
    /// Undeployed MCVs, and whose they are.
    mcvs: BTreeMap<EntityId, PlayerNo>,
    /// Every harvester in service.
    fleet: Fleet,
    /// Everyone who can shoot, and everything in the air (§7).
    battle: Battle,
    /// Which corner the D-1 patrol vehicle is currently crossing to.
    patrol_goal: Option<(i64, i64)>,
    /// Where each player-ordered mover is headed, if anywhere.
    orders: BTreeMap<EntityId, (i64, i64)>,
    /// The entity whose fields carry this player's numbers to the HUD.
    ///
    /// **A local layer cannot read simulation state**, and must not: a
    /// sidebar that computed its own credit total would be a second,
    /// disagreeing account of the money. So the simulation *publishes* —
    /// onto entity fields, which both layers read through the same
    /// `entity_field` — and the HUD is a view of the world rather than a
    /// parallel model of it.
    dash: Option<EntityId>,
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
            yards: Yards::new(),
            mcvs: BTreeMap::new(),
            fleet: Fleet::new(),
            battle: Battle::new(),
            patrol_goal: None,
            orders: BTreeMap::new(),
            dash: None,
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

        // The dashboard: an entity that carries this player's numbers to
        // the HUD. No model, so it never draws; parked under the base so
        // nothing that scans by position trips over it at the origin.
        let board = host.archetype(&[
            dash::MARK,
            dash::CREDITS,
            dash::CAPACITY,
            dash::MADE,
            dash::USED,
            dash::READY,
            dash::PROGRESS,
        ]);
        let board = host.entity_create(board);
        host.entity_set_field(board, dash::MARK, Fixed::from_int(1));
        host.entity_set_position(board, Self::seat(host, sx, sy));
        self.dash = Some(board);

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

    fn command(&mut self, host: &dyn Host, player: monada_sim::PlayerId, command: &monada_sim::Command) {
        let who = PlayerNo::try_from(player.0).unwrap_or(0);
        let at = (
            i64::from(command.arg.x.floor_to_int()),
            i64::from(command.arg.y.floor_to_int()),
        );
        match command.verb {
            verb::ORDER => {
                if let Some(kind) = kind_of(command.arg.z.floor_to_int()) {
                    self.yards.queue(who).order(&mut self.economy, who, kind);
                }
            }
            verb::PLACE => self.place_ready(host, who, at),
            verb::DEPLOY => self.deploy(host, command.target),
            verb::REPAIR => {
                self.yards.repair(&mut self.economy, command.target, REPAIR_RATE);
            }
            verb::MOVE => {
                // A player order overrides whatever the unit was doing,
                // which for a harvester means it stops working until it
                // arrives — the classic behaviour, and the reason a
                // right-click has to clear the retained route too.
                self.orders.insert(command.target, at);
                self.router.forget(command.target);
            }
            _ => {}
        }
    }

    fn tick(&mut self, host: &dyn Host, _dt: Fixed) {
        // Power before work, because power is what work costs (§4e): the
        // buildings standing at the top of the tick decide how fast the
        // engineers dig during it.
        self.economy.begin_tick();
        self.economy.count(self.yards.economy_view());
        let allowance = self.economy.player(0).allowance();
        self.yards.queue(0).tick(&mut self.economy, 0);
        for gone in self.yards.weather(&mut self.economy) {
            host.entity_despawn(gone);
        }

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
        // Shells land and guns fire AFTER the ground has finished moving:
        // a line of fire is a question about the terrain, and asking it
        // before this tick.s craters and slumps have settled answers
        // about a map that no longer exists.
        self.battle.run(host, &mut self.yards);
        self.march(host);
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
            &self.yards,
            &self.mcvs,
            &self.orders,
            &self.fleet,
            &self.battle,
        ))
        .unwrap_or_default()
    }

    fn restore(&mut self, bytes: &[u8]) {
        if let Ok((router, terraform, economy, yards, mcvs, orders, fleet, battle)) =
            postcard::from_bytes(bytes)
        {
            self.router = router;
            self.terraform = terraform;
            self.economy = economy;
            self.yards = yards;
            self.mcvs = mcvs;
            self.orders = orders;
            self.fleet = fleet;
            self.battle = battle;
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

        // A duel across the berm, so D-6's rule is visible rather than
        // merely tested: two guns that cannot see each other through
        // packed fill, and a mortar that does not care. The berm going up
        // beside them is the same one the Surfling order above builds.
        let gun = |rules: &mut DesertRules, owner: PlayerNo, at: (i64, i64), weapon: Weapon| {
            let e = rules.spawn_unit(host, owner, at, MODEL_SOLDIER);
            rules
                .battle
                .enlist(e, owner, combat::Armour::Light, weapon);
        };
        gun(self, 0, (sx + 8, sy - 2), Weapon::Cannon);
        gun(self, 0, (sx + 8, sy + 2), Weapon::Mortar);
        gun(self, 1, (sx + 18, sy - 2), Weapon::Cannon);
        gun(self, 1, (sx + 18, sy + 2), Weapon::Cannon);
    }

    /// Land a player's opening force: an MCV, a harvester, and the
    /// credits to build with.
    ///
    /// Dune II's opening, and the reason it is the right one here: the
    /// MCV has to *find a pad*. On a dune sea nothing is level, so where
    /// a player deploys is already a decision about terrain, before a
    /// single terraform order is given.
    fn found_base(&mut self, host: &dyn Host, owner: PlayerNo, at: (i64, i64)) {
        self.economy.found(owner, STARTING_CREDITS);
        self.mcv(host, owner, at);
        // The refinery comes later, so the harvester's first home is the
        // MCV's cell; deploying re-homes the fleet.
        let harvester = self.spawn_unit(host, owner, (at.0 + 4, at.1 + 4), MODEL_HARVESTER);
        self.fleet.enlist(harvester, owner, at);
    }

    /// Spawn an MCV, the mobile thing a base starts as.
    fn mcv(&mut self, host: &dyn Host, owner: PlayerNo, at: (i64, i64)) -> EntityId {
        let mcv = self.spawn_unit(host, owner, at, MODEL_MCV);
        self.mcvs.insert(mcv, owner);
        mcv
    }

    fn spawn_unit(
        &mut self,
        host: &dyn Host,
        owner: PlayerNo,
        at: (i64, i64),
        model: (i64, i64, i64, i64),
    ) -> EntityId {
        let kind = *self
            .unit_kind
            .get_or_insert_with(|| host.archetype(&["owner", "load"]));
        let e = host.entity_create(kind);
        host.entity_set_model(e, host.model_box(model.0, model.1, model.2, model.3));
        host.entity_set_field(e, "owner", Fixed::from_int(i32::from(owner)));
        host.entity_set_position(e, Self::seat(host, at.0, at.1));
        e
    }

    /// Turn an MCV into a construction yard where it stands.
    ///
    /// It looks for a pad near itself rather than demanding the exact
    /// cell: an eight-cell footprint on a dune almost never happens to be
    /// level, and "deploy failed, drive one cell east and try again" is
    /// not a game.
    fn deploy(&mut self, host: &dyn Host, mcv: EntityId) {
        let Some(&owner) = self.mcvs.get(&mcv) else {
            return;
        };
        let pos = host.entity_position(mcv);
        let at = (
            i64::from(pos.x.floor_to_int()),
            i64::from(pos.y.floor_to_int()),
        );
        let Some(site) = self
            .yards
            .find_site(host, owner, Structure::Yard, at, DEPLOY_SEARCH)
        else {
            host.status("no level ground to deploy on — grade a pad first");
            return;
        };
        self.mcvs.remove(&mcv);
        host.entity_despawn(mcv);
        self.router.forget(mcv);
        let yard = self.raise(host, owner, Structure::Yard, site);
        // Harvesters serve the yard until a refinery exists.
        let _ = yard;
    }

    /// Put a finished structure down, if the site will take it.
    fn place_ready(&mut self, host: &dyn Host, who: PlayerNo, at: (i64, i64)) {
        let Some(kind) = self.yards.queue(who).take() else {
            return;
        };
        match self.yards.survey(host, who, kind, at) {
            Ok(site) => {
                let e = self.raise(host, who, kind, site);
                if kind == Structure::Refinery {
                    self.fleet.rehome(who, site_centre(site));
                }
                let _ = e;
            }
            Err(why) => {
                self.yards.queue(who).put_back(kind);
                host.status(&format!("cannot build there: {}", refusal(why)));
            }
        }
    }

    /// Grade the pad, spawn the entity, and record the structure.
    fn raise(
        &mut self,
        host: &dyn Host,
        who: PlayerNo,
        kind: Structure,
        site: build::Site,
    ) -> EntityId {
        let archetype = *self
            .building_kind
            .get_or_insert_with(|| host.archetype(&["owner", "health"]));
        let e = host.entity_create(archetype);
        let model = model_of(kind);
        host.entity_set_model(e, host.model_box(model.0, model.1, model.2, model.3));
        host.entity_set_field(e, "owner", Fixed::from_int(i32::from(who)));
        self.yards.raise(host, who, kind, site, e);
        let (cx, cy) = site_centre(site);
        host.entity_set_position(e, Self::seat(host, cx, cy));
        e
    }

    /// Walk everything a player has ordered somewhere, and forget the
    /// order once it arrives or proves it cannot.
    fn march(&mut self, host: &dyn Host) {
        let going: Vec<(EntityId, (i64, i64))> =
            self.orders.iter().map(|(&e, &to)| (e, to)).collect();
        for (unit, to) in going {
            let step = self
                .router
                .step(host, unit, to, VEHICLE, Fixed::from_ratio(1, 6));
            if step != mover::Step::Moving {
                self.orders.remove(&unit);
            }
        }
    }

    /// The HUD line.
    ///
    /// **The build line's state comes from here, not from the sidebar.**
    /// The queue is simulation state, so the local layer cannot see it —
    /// and it must not guess, because a HUD that estimated its own
    /// progress would be a second, disagreeing account of how far along a
    /// refinery is. Without this line the whole flow is invisible: an
    /// order takes a second to finish, a click before then does nothing,
    /// and the player has no way to tell "not yet" from "broken".
    fn report(&self, host: &dyn Host) {
        let Some(p) = self.economy.get(0) else {
            return;
        };
        let queue = self.yards.queue_of(0);
        let line = if let Some(kind) = queue.and_then(Queue::ready) {
            format!("{} READY — click the ground to place it", name_of(kind))
        } else if let Some((kind, done)) = queue.and_then(Queue::building) {
            let total = build::blueprint(kind).map_or(1, |b| b.ticks.max(1));
            format!("building {} — {}%", name_of(kind), done * 100 / total)
        } else if !self.mcvs.is_empty() {
            "select the MCV and press G to deploy".to_string()
        } else if self.spent.total() > 0 {
            format!(
                "terraforming — {} cells edited, {} settling, {} orders",
                self.spent.edited,
                self.spent.settled,
                self.terraform.pending()
            )
        } else {
            "1 wind trap   2 refinery   3 silo".to_string()
        };
        let (credits, capacity, made, used) = (p.credits, p.capacity, p.made, p.used);
        host.status(&format!(
            "{credits} credits / {capacity} — power {made}/{used} — {line}"
        ));

        // The same numbers onto the dashboard entity, for the sidebar.
        let Some(dash) = self.dash else {
            return;
        };
        let put = |name: &str, v: u32| {
            host.entity_set_field(dash, name, Fixed::from_int(i32::try_from(v).unwrap_or(0)));
        };
        put(dash::CREDITS, credits);
        put(dash::CAPACITY, capacity);
        put(dash::MADE, made);
        put(dash::USED, used);
        let ready = queue
            .and_then(Queue::ready)
            .and_then(|k| build::CATALOGUE.iter().position(|b| b.kind == k))
            .map_or(-1, |i| i32::try_from(i).unwrap_or(-1));
        host.entity_set_field(dash, dash::READY, Fixed::from_int(ready));
        let progress = queue.and_then(Queue::building).map_or(-1, |(kind, done)| {
            let total = build::blueprint(kind).map_or(1, |b| b.ticks.max(1));
            i32::try_from(done * 100 / total).unwrap_or(0)
        });
        host.entity_set_field(dash, dash::PROGRESS, Fixed::from_int(progress));
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
    /// Which catalogue entry the player has picked, if any: the next
    /// ground click places it. Purely local — where a player is pointing
    /// is not part of the world.
    picked: Option<usize>,
    /// Edge detection for the click and key actions, so a held button is
    /// one order rather than thirty a second.
    held: [bool; 7],
    /// Where the camera is looking, in sim cells.
    ///
    /// **The camera has to be the player's, not the simulation's.** It
    /// followed whatever entity happened to be first in the world, which
    /// meant a demo you could not look away from: the base was built off
    /// screen while the view chased a patrol vehicle across the map.
    /// Following is now something the player asks for by selecting a
    /// unit, and stops the moment they touch a pan key.
    look: Option<(Fixed, Fixed)>,
    /// The entity the view is riding, if the player asked for one.
    following: Option<EntityId>,
    /// The dashboard entity, once found (see [`dash`]).
    board: Option<EntityId>,
    /// What this client has explored (§4f). Local, per-client, and
    /// never in the digest — two players see different shrouds and are
    /// not desynced.
    shroud: Shroud,
    /// The overlay grid the placement ghost is painted into, and the
    /// footprint painted last frame so it can be rubbed out.
    ghost: Option<i64>,
    ghost_at: Option<((i64, i64), i64, i64)>,
}

/// One edge-detection slot per action, so a held key is one order and
/// two actions never share a latch.
const SLOT_SELECT: usize = 0;
const SLOT_ORDER: usize = 1;
const SLOT_DEPLOY: usize = 2;
const SLOT_REPAIR: usize = 3;
const SLOT_BUILD: usize = 4;

/// The placement ghost.s colours: a pad that will be taken, a pad on
/// ground that will not bear it, and a site the terrain refuses.
const GHOST_GOOD: i64 = 0x8060_d878;
const GHOST_POOR: i64 = 0x80d8_c060;
const GHOST_REFUSED: i64 = 0x80d8_5850;

/// Cells per second the camera pans at, and radians per second it turns.
const PAN_SPEED: i64 = 40;
const TURN_SPEED: (i32, i32) = (3, 2); // 3/2 rad per second
/// How close and how far the view may get.
const ZOOM: (i64, i64) = (200, 1_400);

impl Default for DesertLocal {
    fn default() -> Self {
        DesertLocal {
            // A steep three-quarter view: WC3's angle, which is what makes
            // a voxel silhouette read as a vehicle rather than a smudge.
            yaw: Fixed::from_ratio(9, 10),
            pitch: Fixed::from_ratio(105, 100),
            dist: Fixed::from_int(700),
            clip: (gen::BEDROCK_Z, gen::SKY_Z),
            picked: None,
            held: [false; 7],
            look: None,
            following: None,
            board: None,
            shroud: Shroud::new(),
            ghost: None,
            ghost_at: None,
        }
    }
}

impl DesertLocal {
    /// Whether an action went down this frame.
    fn pressed(&mut self, host: &dyn LocalHost, slot: usize, id: &str) -> bool {
        let now = host.action_down(id);
        let edge = now && !self.held[slot];
        self.held[slot] = now;
        edge
    }
}

impl LocalRules for DesertLocal {
    fn local_init(&mut self, host: &dyn LocalHost) {
        host.camera_angle(self.yaw, self.pitch);
        host.camera_dist(self.dist);
        host.deck_clip(self.clip.0, self.clip.1);
        host.status("the desert — D-5");
    }

    fn local_frame(&mut self, host: &dyn LocalHost, dt: Fixed) {
        self.drive_camera(host, dt);

        // The build list: a structure is chosen with a number key, then
        // placed with a ground click. Icons and panels are D-11 — what
        // this owes today is a flow a player can actually use.
        //
        // Every action gets its own edge-detection slot. They shared one
        // for a moment and the symptom was a select that only worked on
        // alternate clicks, which is the sort of thing that reads as "the
        // mouse is broken".
        for (slot, index, id) in [(SLOT_BUILD, 1_usize, "build1"), (SLOT_BUILD + 1, 2, "build2"), (SLOT_BUILD + 2, 3, "build3")] {
            if self.pressed(host, slot, id) {
                self.picked = Some(index);
                host.submit_command(
                    verb::ORDER,
                    EntityId(0),
                    FixedVec3::new(
                        Fixed::ZERO,
                        Fixed::ZERO,
                        Fixed::from_int(i32::try_from(index).unwrap_or(0)),
                    ),
                );
            }
        }

        if self.pressed(host, SLOT_SELECT, "select") {
            // A click on a unit selects it; a click on bare ground places
            // whatever is waiting. Both, in that order, because clicking
            // your own refinery to place a silo on top of it is not what
            // anybody means.
            if let Some(e) = host.pick_entity() {
                host.highlight(e);
                self.following = Some(e);
            } else if let Some(point) = host.pick_ground() {
                host.submit_command(verb::PLACE, EntityId(0), point);
                self.picked = None;
            }
        }
        if self.pressed(host, SLOT_ORDER, "order") {
            // Right-click: send whatever is selected. The MCV in
            // particular is useless without this — you cannot choose
            // where to found a base if you cannot drive there.
            if let (Some(e), Some(point)) = (host.highlighted(), host.pick_ground()) {
                host.submit_command(verb::MOVE, e, point);
            }
        }
        if self.pressed(host, SLOT_DEPLOY, "deploy") {
            if let Some(e) = host.highlighted() {
                host.submit_command(verb::DEPLOY, e, FixedVec3::ZERO);
            }
        }
        if self.pressed(host, SLOT_REPAIR, "repair") {
            if let Some(e) = host.highlighted() {
                host.submit_command(verb::REPAIR, e, FixedVec3::ZERO);
            }
        }

        self.find_board(host);
        self.lift_shroud(host);
        self.place_ghost(host);
        self.sidebar(host);
    }
}

impl DesertLocal {
    /// Pan, turn and zoom the view.
    ///
    /// Everything here is `f64`-free but not fixed-point-free: the camera
    /// is local state, so it may use whatever arithmetic reads best —
    /// what it may never do is feed a number back into the simulation.
    fn drive_camera(&mut self, host: &dyn LocalHost, dt: Fixed) {
        let (px, py) = host.action_axis2("pan");
        let turn = host.action_axis("turn");
        let zoom = host.action_axis("zoom");

        if px != 0 || py != 0 {
            // Touching a pan key is how a player says "stop following".
            self.following = None;
        }
        if let Some(e) = self.following {
            let p = host.entity_position(e);
            self.look = Some((p.x, p.y));
        }

        let here = self.look.get_or_insert_with(|| {
            // Open on the player's own corner rather than on the map's
            // origin, which is a corner of empty sand.
            let start = Fixed::from_int(i32::try_from(MAP_CELLS / 8).unwrap_or(32));
            (start, start)
        });
        if px != 0 || py != 0 {
            let speed = Fixed::from_int(i32::try_from(PAN_SPEED).unwrap_or(40)) * dt;
            let (dx, dy) = pan_in_view(self.yaw, px, py);
            here.0 += dx * speed;
            here.1 += dy * speed;
            let (lo, hi) = (
                Fixed::ZERO,
                Fixed::from_int(i32::try_from(MAP_CELLS - 1).unwrap_or(255)),
            );
            here.0 = here.0.clamp(lo, hi);
            here.1 = here.1.clamp(lo, hi);
        }
        if turn != 0 {
            // Subtracted, not added: the map is mirrored in x on the way
            // to the screen, so a yaw that turns the world one way turns
            // the *picture* the other. E has to swing the view right.
            let spin = TURN_SPEED.0 * i32::try_from(turn).unwrap_or(0);
            self.yaw -= Fixed::from_ratio(spin, TURN_SPEED.1) * dt;
        }
        if zoom != 0 {
            let rate = Fixed::from_int(600) * dt;
            self.dist = (self.dist - rate * Fixed::from_int(i32::try_from(zoom).unwrap_or(0))).clamp(
                Fixed::from_int(i32::try_from(ZOOM.0).unwrap_or(200)),
                Fixed::from_int(i32::try_from(ZOOM.1).unwrap_or(1400)),
            );
        }

        let (x, y) = *here;
        let z = Fixed::from_int(i32::try_from(gen::MEAN_SURFACE_Z).unwrap_or(32));
        host.camera_focus(FixedVec3::new(x, y, z));
        host.camera_angle(self.yaw, self.pitch);
        host.camera_dist(self.dist);
    }

    /// Draw the build list.
    ///
    /// Immediate mode: cleared and redrawn every frame, so "grey out what
    /// you cannot afford" is a comparison rather than a widget that has
    /// to be told when the money changed. The prices come from the same
    /// [`CATALOGUE`](build::CATALOGUE) the simulation charges from, which
    /// is the only way the two can never disagree.
    fn sidebar(&self, host: &dyn LocalHost) {
        let (w, _h) = host.ui_size();
        if w == 0 {
            return; // headless, or a host that draws no HUD
        }
        let board = self.board;
        let read = |name: &str| board.map_or(0, |e| host.entity_field(e, name).floor_to_int());

        host.ui_clear();
        let x = w - 220;
        // The money first, because it is the number a player looks at
        // most and the one that decides whether the rest is even worth
        // reading. Published by the simulation, not computed here.
        host.ui_text(
            x,
            12,
            &format!("{} / {}", read(dash::CREDITS), read(dash::CAPACITY)),
            20,
        );
        host.ui_text(
            x,
            36,
            &format!("power {} / {}", read(dash::MADE), read(dash::USED)),
            14,
        );

        host.ui_text(x, 64, "BUILD", 16);
        let purse = read(dash::CREDITS);
        for (i, bp) in build::CATALOGUE.iter().enumerate().skip(1) {
            let y = 84 + i64::try_from(i - 1).unwrap_or(0) * 20;
            let mark = if self.picked == Some(i) { ">" } else { " " };
            let afford = if i64::from(purse) >= i64::from(bp.cost) {
                ""
            } else {
                "  (too dear)"
            };
            host.ui_text(
                x,
                y,
                &format!("{mark}{i}  {}  {}{afford}", name_of(bp.kind), bp.cost),
                14,
            );
        }

        let progress = read(dash::PROGRESS);
        let ready = read(dash::READY);
        let line = if ready >= 0 {
            let kind = kind_of(ready).map_or("something", name_of);
            format!("{kind} READY — click the ground")
        } else if progress >= 0 {
            format!("building… {progress}%")
        } else {
            String::new()
        };
        host.ui_text(x, 150, &line, 14);
        host.ui_text(x, 176, "WASD pan   QE turn   ZX zoom", 12);
        host.ui_text(x, 192, "LMB select/place   RMB move", 12);
        host.ui_text(x, 208, "G deploy   R repair", 12);
    }

    /// Find the dashboard entity, and remember it.
    ///
    /// By its marker field rather than by its archetype: looking an
    /// archetype up is a simulation-side operation, and the local layer
    /// has no business doing one. Re-scanned only when the remembered
    /// entity has gone.
    fn find_board(&mut self, host: &dyn LocalHost) {
        let alive = self
            .board
            .is_some_and(|e| host.entity_field(e, dash::MARK).floor_to_int() == 1);
        if alive {
            return;
        }
        self.board = host
            .entities()
            .into_iter()
            .find(|&e| host.entity_field(e, dash::MARK).floor_to_int() == 1);
    }

    /// Push the shroud back around everything this client owns.
    ///
    /// Walks the world rather than being told: the local layer has no
    /// list of "my units", and building one would be a second account of
    /// something the world already knows. Ownership comes off the same
    /// `owner` field the simulation set.
    ///
    /// Hotseat — `local_player()` of `None` — reveals for everybody,
    /// which is the right answer for one window driving every side.
    fn lift_shroud(&mut self, host: &dyn LocalHost) {
        self.shroud.lay(host);
        let mine = host.local_player();
        for e in host.entities() {
            let owner = i64::from(host.entity_field(e, "owner").floor_to_int());
            if mine.is_some_and(|me| me != owner) {
                continue;
            }
            let p = host.entity_position(e);
            let at = (
                i64::from(p.x.floor_to_int()),
                i64::from(p.y.floor_to_int()),
            );
            // A structure watches further than a unit; it is taller and
            // it is not going anywhere.
            let reach = if host.entity_field(e, "health").floor_to_int() > 0 {
                shroud::BASE_SIGHT
            } else {
                shroud::UNIT_SIGHT
            };
            self.shroud.reveal(host, at, reach);
        }
    }

    /// Show where the finished structure would go, on the ground.
    ///
    /// A coordinate readout is not an answer to "where am I putting
    /// this": an RTS placement has to sit in the world, at the height the
    /// pad will be graded to, in a colour that says whether it will be
    /// accepted. It is painted into an overlay grid of the local layer's
    /// own — real geometry, on any map, touching nothing hashed.
    ///
    /// The terrain half of the verdict comes from the same
    /// [`build::grade`] the simulation's `survey` uses, so the ghost and
    /// the answer cannot drift apart. The rest — adjacency, overlap — is
    /// the simulation's to know, and it says so when it refuses.
    fn place_ghost(&mut self, host: &dyn LocalHost) {
        let grid = *self.ghost.get_or_insert_with(|| host.grid_overlay());
        if grid < 0 {
            return;
        }
        if let Some((at, span, z)) = self.ghost_at.take() {
            for y in at.1..(at.1 + span) {
                for x in at.0..(at.0 + span) {
                    host.overlay_clear(grid, x, y, z);
                }
            }
        }

        let ready = self
            .board
            .map(|e| host.entity_field(e, dash::READY).floor_to_int());
        let Some(kind) = ready.filter(|&r| r >= 0).and_then(kind_of) else {
            return;
        };
        let Some(bp) = build::blueprint(kind) else {
            return;
        };
        let Some(point) = host.pick_ground() else {
            return;
        };
        // The cursor names the middle of the footprint, which is where a
        // player thinks they are pointing.
        let at = (
            i64::from(point.x.floor_to_int()) - bp.span / 2,
            i64::from(point.y.floor_to_int()) - bp.span / 2,
        );
        let (z, color) = match build::grade(host, at, bp.span) {
            Ok((pad_z, firm)) if firm => (pad_z, GHOST_GOOD),
            Ok((pad_z, _)) => (pad_z, GHOST_POOR),
            Err(_) => (
                host.volume_top(at.0, at.1).map_or(0, |(z, _)| z),
                GHOST_REFUSED,
            ),
        };
        let z = z + 1;
        host.overlay_fill(
            grid,
            (at.0, at.1, z),
            (at.0 + bp.span - 1, at.1 + bp.span - 1, z),
            color,
        );
        self.ghost_at = Some((at, bp.span, z));
    }
}

/// A screen-relative pan, in sim cells per unit of speed.
///
/// `right` and `up` are the pan action's two axes as the player pressed
/// them (D is `right = 1`, W is `up = 1`); the result is which way that
/// is *on the ground*, given where the camera is looking.
///
/// **The world is mirrored in x on the way to the screen**, and that is
/// the whole difficulty. `world_of` maps sim `+x` to world `−x`, so the
/// camera basis — which is in world axes — cannot be used on sim
/// coordinates unchanged. Derived rather than guessed:
///
/// - roxlap's screen-right is world `(−sin yaw, cos yaw, 0)`, which
///   through the mirror is sim `(sin yaw, cos yaw)`;
/// - screen-up along the ground is the horizontal part of the view
///   direction, world `(cos yaw, sin yaw, 0)`, which is sim
///   `(−cos yaw, sin yaw)`.
///
/// Getting the mirror wrong negates the x term of *both* axes, which
/// reads as "A and D are swapped, and panning does not follow the
/// camera" — two complaints, one sign.
#[must_use]
pub fn pan_in_view(yaw: Fixed, right: i64, up: i64) -> (Fixed, Fixed) {
    let (c, s) = (trig::cos(yaw), trig::sin(yaw));
    let (fx, fy) = (
        Fixed::from_int(i32::try_from(right).unwrap_or(0)),
        Fixed::from_int(i32::try_from(up).unwrap_or(0)),
    );
    (s * fx - c * fy, c * fx + s * fy)
}

/// A structure's name, as a player reads it.
#[must_use]
pub fn name_of(kind: Structure) -> &'static str {
    match kind {
        Structure::Yard => "yard",
        Structure::Refinery => "refinery",
        Structure::Silo => "silo",
        Structure::WindTrap => "wind trap",
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
