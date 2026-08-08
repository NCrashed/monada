//! The harvester loop, the silos and the power scalar
//! (docs/plans/desert-game.md §7) — the D-4 gate.
//!
//! The gate is "a scripted schedule mines an exact credit total at an
//! exact tick", and that is what
//! [`a_scripted_schedule_mines_an_exact_total`] asserts. The rest of the
//! file is the reason that number is worth asserting: the spice is
//! terrain, so a field wears away, a buried vein is worth nothing until
//! somebody digs it up, and every credit banked corresponds to a cell
//! that left the ground.
//!
//! Everything runs on a hand-laid plate rather than the generated map:
//! the point is exact arithmetic, and 65k columns of dunes make an exact
//! number hard to reason about without making it any more true.

use std::sync::{Arc, Mutex};

use monada_runtime::{
    shared_physics, shared_world, Host, MapRules, NativeBackend, NullBridge, ScriptBackend,
    SharedBridge,
};
use monada_sim::EntityId;

use monada_desert_rules::economy::{
    Economy, Player, Structure, BASE_CAPACITY, CREDITS_PER_CELL, REFINERY_CAPACITY, SILO_CAPACITY,
};
use monada_desert_rules::harvest::{Fleet, Task};
use monada_desert_rules::mover::Router;
use monada_desert_rules::terraform::{Terraform, Work, CELLS_PER_TICK};
use monada_desert_rules::{material, Building, SAND_REPOSE, VEHICLE, VEHICLE_MAX_STEP};

const SIZE: i64 = 40;
const TOP: i64 = 12;
/// Where the refinery stands, and where the spice patch is.
const HOME: (i64, i64) = (4, 4);
const PATCH: (i64, i64) = (30, 30);
const PATCH_R: i64 = 3;

/// A plate with a small surface spice patch and a buried vein under it.
struct Field;

impl MapRules for Field {
    fn init(&mut self, host: &dyn Host) {
        host.volume_fill(
            (0, 0, 0),
            (SIZE - 1, SIZE - 1, TOP),
            material::SAND,
            material::color(material::SAND),
        );
        // A surface patch: one cell deep, so its cell count is exactly
        // the number of columns in it and the arithmetic below is
        // countable by hand.
        for (x, y) in disc(PATCH, PATCH_R) {
            host.volume_fill(
                (x, y, TOP),
                (x, y, TOP),
                material::SPICE,
                material::color(material::SPICE),
            );
        }
        // A vein four cells down under a different patch, with sand over
        // it. Nothing may harvest this until the sand is gone.
        for (x, y) in disc(VEIN, PATCH_R) {
            host.volume_fill(
                (x, y, TOP - 4),
                (x, y, TOP - 4),
                material::SPICE,
                material::color(material::SPICE),
            );
        }
        for (x, y) in disc(THICK, PATCH_R) {
            host.volume_fill(
                (x, y, TOP - 3),
                (x, y, TOP),
                material::SPICE,
                material::color(material::SPICE),
            );
        }
        host.granular_register(material::SAND, SAND_REPOSE);
        host.granular_register(material::SPICE, SAND_REPOSE);
    }
}

const VEIN: (i64, i64) = (30, 8);
/// A seam four cells thick, lying open at the surface: the shape that
/// tempts a harvester into digging its own grave.
const THICK: (i64, i64) = (10, 30);

fn disc(centre: (i64, i64), radius: i64) -> Vec<(i64, i64)> {
    let mut cells = Vec::new();
    for y in (centre.1 - radius)..=(centre.1 + radius) {
        for x in (centre.0 - radius)..=(centre.0 + radius) {
            let (dx, dy) = (x - centre.0, y - centre.1);
            if dx * dx + dy * dy <= radius * radius {
                cells.push((x, y));
            }
        }
    }
    cells
}

/// One player, one refinery, one harvester, and a clock.
struct Mine {
    backend: NativeBackend,
    economy: Economy,
    fleet: Fleet,
    router: Router,
    sites: Vec<(i64, i64, i64)>,
    unit: EntityId,
}

impl Mine {
    fn new(sites: Vec<(i64, i64, i64)>) -> Mine {
        let mut backend = NativeBackend::new(shared_world(11), Box::new(Field));
        let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
        backend.set_bridge(&bridge);
        backend.set_volume(&shared_physics(30));
        backend.on_init().expect("init");

        let mut economy = Economy::new();
        economy.found(0, 0);

        let host = backend.host();
        let kind = host.archetype(&["owner"]);
        let unit = host.entity_create(kind);
        host.entity_set_position(unit, seat(host, HOME.0 + 1, HOME.1));

        let mut fleet = Fleet::new();
        fleet.enlist(unit, 0, HOME);

        Mine {
            backend,
            economy,
            fleet,
            router: Router::new(),
            sites,
            unit,
        }
    }

    /// One refinery's worth of storage, recounted the way the rules do.
    fn tick(&mut self) {
        let refinery = [Building {
            owner: 0,
            kind: Structure::Refinery,
        }];
        self.economy.begin_tick();
        self.economy.count(refinery.iter().copied());
        self.fleet.run(
            self.backend.host(),
            &mut self.economy,
            &mut self.router,
            &self.sites,
            VEHICLE,
        );
        self.economy.end_tick();
    }

    fn run(&mut self, ticks: u32) {
        for _ in 0..ticks {
            self.tick();
        }
    }

    fn credits(&self) -> u32 {
        self.economy.get(0).map_or(0, |p| p.credits)
    }

    fn task(&self) -> Task {
        self.fleet.get(self.unit).expect("harvester").task
    }

    fn load(&self) -> u32 {
        self.fleet.get(self.unit).expect("harvester").load
    }
}

fn seat(host: &dyn Host, x: i64, y: i64) -> monada_fixed::FixedVec3 {
    let z = host.volume_top(x, y).map_or(0, |(z, _)| z) + 1;
    monada_fixed::FixedVec3::new(
        monada_fixed::Fixed::from_int(i32::try_from(x).unwrap()),
        monada_fixed::Fixed::from_int(i32::try_from(y).unwrap()),
        monada_fixed::Fixed::from_int(i32::try_from(z).unwrap()),
    )
}

/// Every spice cell left on the plate.
fn spice_cells(host: &dyn Host) -> u32 {
    let mut n = 0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            for z in 0..=(TOP + 4) {
                if host.volume_material(x, y, z) == Some(material::SPICE) {
                    n += 1;
                }
            }
        }
    }
    n
}

// --- the gate -------------------------------------------------------------

#[test]
fn a_scripted_schedule_mines_an_exact_total() {
    // The schedule: one harvester, one surface patch, one refinery. The
    // assertion is not "roughly this much" — it is the exact number of
    // credits in the bank at an exact tick, which is the only kind of
    // number a lockstep economy is allowed to produce.
    let mut mine = Mine::new(vec![(PATCH.0, PATCH.1, PATCH_R)]);
    let patch = u32::try_from(disc(PATCH, PATCH_R).len()).unwrap();
    assert_eq!(patch, 29, "the hand-laid patch changed size");

    // Long enough for the drive out, the whole patch, the drive back and
    // the unload — and then some, so the number is stable rather than
    // caught mid-motion.
    mine.run(2_000);

    assert_eq!(
        mine.credits(),
        patch * CREDITS_PER_CELL,
        "every cell of the patch should be in the bank exactly once"
    );
    assert_eq!(mine.load(), 0, "the harvester is still holding something");
    assert_eq!(mine.task(), Task::Idle, "there is nothing left to work");
    // What is left in the ground is exactly the two seams this schedule
    // was not pointed at: the buried vein, one cell thick, and the thick
    // surface seam, four.
    let untouched = u32::try_from(disc(VEIN, PATCH_R).len() + 4 * disc(THICK, PATCH_R).len())
        .expect("cell count");
    assert_eq!(
        spice_cells(mine.backend.host()),
        untouched,
        "the harvester worked a seam it was not sent to"
    );
}

#[test]
fn the_number_is_the_same_on_every_peer() {
    let once = || {
        let mut mine = Mine::new(vec![(PATCH.0, PATCH.1, PATCH_R)]);
        mine.run(700);
        (mine.credits(), mine.load(), mine.task())
    };
    assert_eq!(once(), once());
}

// --- the spice is terrain -------------------------------------------------

#[test]
fn every_credit_is_a_cell_that_left_the_ground() {
    let mut mine = Mine::new(vec![(PATCH.0, PATCH.1, PATCH_R)]);
    let before = spice_cells(mine.backend.host());
    mine.run(700);
    let after = spice_cells(mine.backend.host());
    let banked = mine.credits() / CREDITS_PER_CELL;
    assert_eq!(
        before - after,
        banked + mine.load(),
        "cells left the ground that are neither banked nor aboard"
    );
}

#[test]
fn a_field_wears_away_where_it_was_worked() {
    let mut mine = Mine::new(vec![(PATCH.0, PATCH.1, PATCH_R)]);
    let all = disc(PATCH, PATCH_R).len();
    let dimples = |host: &dyn Host| {
        disc(PATCH, PATCH_R)
            .into_iter()
            .filter(|&(x, y)| host.volume_top(x, y) == Some((TOP - 1, material::SAND)))
            .count()
    };

    // Watched rather than sampled at a chosen tick: what matters is that
    // the field goes cell by cell — a working face that eats into the
    // patch — and never comes back, not that it is half gone at tick 420.
    let mut worked = 0;
    let mut partial = false;
    for _ in 0..1_800 {
        mine.tick();
        let now = dimples(mine.backend.host());
        assert!(now >= worked, "spice grew back: {worked} → {now}");
        partial |= now > 0 && now < all;
        worked = now;
    }
    assert!(partial, "the patch went all at once, or not at all");
    assert_eq!(worked, all, "the field was never finished");
}

#[test]
fn a_buried_vein_is_worth_nothing_until_it_is_dug_up() {
    // The whole of "the three factions' economies scale differently"
    // (§7), and it needs no rule of its own: the harvester takes the top
    // cell of a column when that cell is spice, and four cells of sand
    // are what stand between a Dweller and a fortune.
    let mut mine = Mine::new(vec![(VEIN.0, VEIN.1, PATCH_R)]);
    mine.run(600);
    assert_eq!(mine.credits(), 0, "it mined through solid sand");
    assert_eq!(mine.task(), Task::Idle, "it found something to work");

    // Now strip the overburden, the way a Dweller's bore crew would.
    let mut dig = Terraform::new();
    dig.order(
        (VEIN.0 - PATCH_R, VEIN.1 - PATCH_R),
        (VEIN.0 + PATCH_R, VEIN.1 + PATCH_R),
        Work::Dig {
            level: TOP - 4,
            spoil: (2, 20),
        },
    );
    let host = mine.backend.host();
    for _ in 0..400 {
        dig.run(host, CELLS_PER_TICK);
    }
    assert_eq!(dig.pending(), 0, "the excavation never finished");

    mine.run(1_500);
    assert!(
        mine.credits() > 0,
        "the vein is exposed and still nobody is mining it"
    );
}

#[test]
fn a_harvester_never_digs_a_hole_it_cannot_drive_out_of() {
    // The cell a harvester takes is the ground it is standing on, so a
    // rich seam is also a trap. Left unchecked it cuts straight down
    // through a thick vein and sits in a shaft, still loaded, forever —
    // which is exactly what the first run of this slice did.
    let mut mine = Mine::new(vec![(THICK.0, THICK.1, PATCH_R)]);
    mine.run(2_500);

    let host = mine.backend.host();
    for (x, y) in disc(THICK, PATCH_R + 2) {
        let Some((top, _)) = host.volume_top(x, y) else {
            continue;
        };
        let highest = [(1_i64, 0_i64), (-1, 0), (0, 1), (0, -1)]
            .into_iter()
            .filter_map(|(dx, dy)| host.volume_top(x + dx, y + dy).map(|(z, _)| z))
            .max()
            .unwrap_or(top);
        assert!(
            highest - top <= VEHICLE_MAX_STEP,
            "({x}, {y}) sits {} cells below its rim — a harvester in there is lost",
            highest - top
        );
    }
}

// --- silos and power ------------------------------------------------------

#[test]
fn what_will_not_fit_is_lost_and_says_so() {
    let mut p = Player {
        capacity: 100,
        ..Player::default()
    };
    assert_eq!(p.deposit(60), 0);
    assert_eq!(p.credits, 60);
    assert_eq!(p.deposit(60), 20, "the overflow was not reported");
    assert_eq!(p.credits, 100);
    assert_eq!(p.spilled, 20);
}

#[test]
fn losing_a_silo_spills_what_it_held() {
    let mut economy = Economy::new();
    economy.found(0, 0);
    let with_silo = [
        Building {
            owner: 0,
            kind: Structure::Refinery,
        },
        Building {
            owner: 0,
            kind: Structure::Silo,
        },
    ];
    economy.begin_tick();
    economy.count(with_silo.iter().copied());
    let full = BASE_CAPACITY + REFINERY_CAPACITY + SILO_CAPACITY;
    economy.player(0).deposit(full);
    economy.end_tick();
    assert_eq!(economy.get(0).expect("player").credits, full);

    // The shell lands. Next tick there is one less place to put things.
    economy.begin_tick();
    economy.count(with_silo[..1].iter().copied());
    economy.end_tick();
    let p = economy.get(0).expect("player");
    assert_eq!(p.credits, BASE_CAPACITY + REFINERY_CAPACITY);
    assert_eq!(p.spilled, SILO_CAPACITY);
}

#[test]
fn a_brownout_slows_the_engineers_without_stopping_them() {
    // §4e's knob is not a constant any more: it is what your generators
    // are actually delivering. This is the tie between the two slices —
    // D-3 built the budget, D-4 decides how big it is.
    let full = Player {
        made: 100,
        used: 100,
        ..Player::default()
    };
    let half = Player {
        made: 50,
        used: 100,
        ..Player::default()
    };
    let dark = Player {
        made: 0,
        used: 100,
        ..Player::default()
    };
    assert_eq!(full.allowance(), CELLS_PER_TICK);
    assert_eq!(half.allowance(), CELLS_PER_TICK / 2);
    assert_eq!(dark.allowance(), 1, "a blackout must not deadlock a dig");

    // Spare capacity is not a speed bonus — you cannot overclock a
    // trench by building wind traps you do not need.
    let rich = Player {
        made: 400,
        used: 100,
        ..Player::default()
    };
    assert_eq!(rich.allowance(), CELLS_PER_TICK);
}

#[test]
fn the_loop_holds_together_end_to_end() {
    // Not an exact-number test: a shape test. Fill, drive, empty, repeat
    // — the thing that is actually broken when a harvester sits still.
    let mut mine = Mine::new(vec![(PATCH.0, PATCH.1, PATCH_R)]);
    let mut seen_cut = false;
    let mut seen_return = false;
    let mut seen_unload = false;
    for _ in 0..1_800 {
        mine.tick();
        match mine.task() {
            Task::Cut => seen_cut = true,
            Task::Return => seen_return = true,
            Task::Unload => seen_unload = true,
            _ => {}
        }
    }
    assert!(seen_cut && seen_return && seen_unload, "the loop stalled");
}
