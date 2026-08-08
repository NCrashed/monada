//! What the three faction verbs promise, and what the budget promises
//! (docs/plans/desert-game.md §6, §4d, §4e) — the D-3 gate.
//!
//! These are behaviour tests, not implementation tests: a berm stands, a
//! trench conserves what it removes, glass keeps the shape and changes
//! the substance, a crater slumps to the angle of repose, and no tick
//! spends more than the allowance. How the cursor walks the footprint is
//! free to change.
//!
//! They run against a small hand-painted plate rather than the generated
//! desert: 65k columns is a slow way to learn nothing extra, and a flat
//! plate makes "what moved" readable at a glance.

use std::sync::{Arc, Mutex};

use monada_runtime::{
    shared_physics, shared_world, Host, MapRules, MaterialId, NativeBackend, NullBridge,
    ScriptBackend, SharedBridge, SharedPhysics, VolumeLimits,
};

use monada_desert_rules::terraform::{Terraform, Work, CELLS_PER_TICK};
use monada_desert_rules::{material, Desert, DesertParams, Surface, SAND_REPOSE, VEHICLE};

/// The plate: `SIZE`×`SIZE` columns of one material, `TOP` cells tall.
const SIZE: i64 = 24;
const TOP: i64 = 12;

/// A flat plate to terraform, with sand declared granular.
struct Plate {
    material: MaterialId,
}

impl MapRules for Plate {
    fn init(&mut self, host: &dyn Host) {
        host.volume_fill(
            (0, 0, 0),
            (SIZE - 1, SIZE - 1, TOP),
            self.material,
            material::color(self.material),
        );
        // After the paint, exactly as the real rules do: the plate is at
        // rest by construction, and what moves is what a test touches.
        host.granular_register(material::SAND, SAND_REPOSE);
        host.granular_register(material::SPICE, SAND_REPOSE);
    }
}

/// A backend over a painted plate, plus the physics handle to read the
/// terrain digest and the settle state back out of.
fn plate(mat: MaterialId) -> (NativeBackend, SharedPhysics) {
    let mut backend = NativeBackend::new(shared_world(7), Box::new(Plate { material: mat }));
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    let phys = shared_physics(30);
    backend.set_volume(&phys);
    backend.on_init().expect("init");
    (backend, phys)
}

/// Run terraform ticks until the orders are done and the ground has come
/// to rest. Returns how many ticks it took.
fn run_out(work: &mut Terraform, host: &dyn Host) -> u32 {
    let mut ticks = 0;
    loop {
        let spent = work.run(host);
        assert!(
            spent.total() <= CELLS_PER_TICK,
            "tick {ticks} spent {} of {CELLS_PER_TICK}",
            spent.total()
        );
        if work.pending() == 0 && spent.total() == 0 {
            return ticks;
        }
        ticks += 1;
        assert!(ticks < 4_000, "the ground never came to rest");
    }
}

/// Every solid cell in the plate's neighbourhood, counted. Wide enough
/// that anything sliding off the edge of the worked area is still caught.
fn mass(host: &dyn Host) -> i64 {
    let mut n = 0;
    for y in -4..SIZE + 4 {
        for x in -4..SIZE + 4 {
            if let Some((top, _)) = host.volume_top(x, y) {
                for z in 0..=top {
                    if host.volume_material(x, y, z).is_some() {
                        n += 1;
                    }
                }
            }
        }
    }
    n
}

/// The steepest drop from any granular column to a neighbour.
fn steepest_granular_drop(host: &dyn Host) -> i64 {
    let mut worst = 0;
    for y in -2..SIZE + 2 {
        for x in -2..SIZE + 2 {
            let Some((top, mat)) = host.volume_top(x, y) else {
                continue;
            };
            if mat != material::SAND && mat != material::SPICE {
                continue;
            }
            for (dx, dy) in [(1_i64, 0_i64), (-1, 0), (0, 1), (0, -1)] {
                // A neighbour with no ground at all is the edge of the
                // painted plate, not a slope: nothing can slide there.
                let Some((there, _)) = host.volume_top(x + dx, y + dy) else {
                    continue;
                };
                worst = worst.max(top - there);
            }
        }
    }
    worst
}

// --- Surflings: additive (§6a) -------------------------------------------

#[test]
fn a_surfling_berm_stands_where_it_was_built() {
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    let mut work = Terraform::new();
    work.order((8, 8), (13, 13), Work::Raise { level: TOP + 5 });
    run_out(&mut work, host);

    for y in 8..=13 {
        for x in 8..=13 {
            let (top, mat) = host.volume_top(x, y).expect("berm column");
            assert_eq!(top, TOP + 5, "({x}, {y}) came out {top} cells high");
            assert_eq!(mat, material::PACKED_FILL, "({x}, {y}) is not fill");
        }
    }
    // Sheer sides: five cells of drop, standing. That is the faction's
    // whole promise — manufactured fill is not sand and does not slump.
    let (edge, _) = host.volume_top(7, 10).expect("sand beside the berm");
    assert_eq!(edge, TOP, "the sand beside the berm moved");
}

#[test]
fn a_berm_costs_exactly_the_cells_it_adds() {
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    let before = mass(host);
    let mut work = Terraform::new();
    // 4×4 columns, three cells each: fill is manufactured, so the map
    // gains material — the one verb of the three that does.
    work.order((4, 4), (7, 7), Work::Raise { level: TOP + 3 });
    run_out(&mut work, host);
    assert_eq!(mass(host) - before, 4 * 4 * 3);
}

// --- Dwellers: subtractive (§6b) -----------------------------------------

#[test]
fn a_dweller_trench_conserves_what_it_removes() {
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    let before = mass(host);
    let mut work = Terraform::new();
    work.order(
        (10, 4),
        (11, 19),
        Work::Dig {
            level: TOP - 4,
            spoil: (2, 2),
        },
    );
    run_out(&mut work, host);
    assert_eq!(
        mass(host),
        before,
        "the excavation created or destroyed material"
    );
}

#[test]
fn the_spoil_heap_slumps_into_a_cone() {
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    let mut work = Terraform::new();
    work.order(
        (14, 14),
        (17, 17),
        Work::Dig {
            level: TOP - 3,
            spoil: (4, 4),
        },
    );
    run_out(&mut work, host);

    // Every dug cell went onto one column; if it stayed there it would be
    // a 48-cell needle. It does not: loose spoil finds its angle, which
    // is what makes a Dweller's excavation visible from across the map.
    let (heap, _) = host.volume_top(4, 4).expect("spoil column");
    assert!(
        heap > TOP && heap < TOP + 10,
        "the heap stands {heap} against a plate of {TOP}"
    );
    assert!(
        steepest_granular_drop(host) <= SAND_REPOSE.max_drop,
        "sand is standing steeper than it holds"
    );
}

#[test]
fn a_trench_in_sand_erodes_and_a_trench_in_fill_does_not() {
    // The same order, twice, into two materials — the reason a Surfling
    // causeway is worth its price and a Dweller's trench is not free.
    let depth_in = |mat: MaterialId| {
        let (backend, _phys) = plate(mat);
        let host = backend.host();
        let mut work = Terraform::new();
        work.order(
            (10, 4),
            (10, 19),
            Work::Dig {
                level: TOP - 5,
                spoil: (2, 2),
            },
        );
        run_out(&mut work, host);
        let (floor, _) = host.volume_top(10, 12).expect("trench floor");
        TOP - floor
    };
    let sand = depth_in(material::SAND);
    let fill = depth_in(material::PACKED_FILL);
    assert_eq!(fill, 5, "packed fill held a five-cell trench: got {fill}");
    assert!(
        sand < fill,
        "sand kept a {sand}-cell trench as well as fill kept {fill}"
    );
}

// --- Binders: transmutative (§6c) ----------------------------------------

#[test]
fn a_binder_keeps_the_shape_and_changes_the_substance() {
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    let heights: Vec<i64> = (8..=15)
        .flat_map(|y| (8..=15).map(move |x| (x, y)))
        .map(|(x, y)| host.volume_top(x, y).expect("plate").0)
        .collect();

    let mut work = Terraform::new();
    work.order((8, 8), (15, 15), Work::Vitrify { depth: 2 });
    run_out(&mut work, host);

    for (i, (x, y)) in (8..=15)
        .flat_map(|y| (8..=15).map(move |x| (x, y)))
        .enumerate()
    {
        let (top, mat) = host.volume_top(x, y).expect("glass column");
        assert_eq!(top, heights[i], "({x}, {y}) changed height");
        assert_eq!(mat, material::GLASS, "({x}, {y}) is not glass");
        assert_eq!(
            host.volume_material(x, y, top - 1),
            Some(material::GLASS),
            "({x}, {y}) is glass one cell deep, not two"
        );
        assert_eq!(
            host.volume_material(x, y, top - 2),
            Some(material::SAND),
            "({x}, {y}) went deeper than it was told"
        );
    }
}

#[test]
fn vitrifying_twice_is_free_the_second_time() {
    // Idempotence matters because a Binder beam will be pointed at ground
    // it has already treated all game long, and a verb that re-did the
    // work would burn the allowance for nothing.
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    let mut work = Terraform::new();
    work.order((8, 8), (11, 11), Work::Vitrify { depth: 2 });
    run_out(&mut work, host);

    work.order((8, 8), (11, 11), Work::Vitrify { depth: 2 });
    let spent = work.run(host);
    assert_eq!(spent.edited, 0, "re-glassing glass cost {} cells", spent.edited);
    assert_eq!(work.pending(), 0);
}

// --- craters and the budget ----------------------------------------------

#[test]
fn a_crater_slumps_into_a_bowl() {
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    let before = mass(host);
    let blasted = Terraform::crater(host, (12, 12), 5);
    assert!(blasted > 0, "the blast moved nothing");
    assert!(
        mass(host) < before,
        "a shell is not an excavation: the spoil should be gone"
    );
    // Fresh from the blast the wall is over-steep — a three-cell lip.
    // That is the point of the hemisphere: a gentler hole would come out
    // of the ground already at rest and never slump at all.
    assert!(
        steepest_granular_drop(host) > SAND_REPOSE.max_drop,
        "the blast left a hole sand could already hold"
    );
    // The outermost ring of the hemisphere is zero cells deep, so the
    // blast leaves the lip itself standing — and everything beyond it.
    let lip = host.volume_top(12, 17).expect("the lip").0;
    assert_eq!(lip, TOP, "the blast reached its own outermost ring");
    assert_eq!(
        host.volume_top(12, 19).expect("undisturbed sand").0,
        TOP,
        "the blast reached past its own radius"
    );

    let mut work = Terraform::new();
    run_out(&mut work, host);

    assert!(
        steepest_granular_drop(host) <= SAND_REPOSE.max_drop,
        "the crater wall is still standing steeper than sand holds"
    );
    assert!(
        host.volume_top(12, 12).expect("crater floor").0 < TOP,
        "the crater filled in completely"
    );
    // The rim fell inwards, so the hole is wider than the shell that made
    // it: ground the blast never touched has been drawn in.
    assert!(
        host.volume_top(12, 17).expect("the lip").0 < lip,
        "the rim did not fall in"
    );
}

#[test]
fn no_tick_spends_more_than_the_allowance() {
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    let mut work = Terraform::new();
    // Far more work than one tick can hold: the whole plate, ten cells up.
    work.order((0, 0), (SIZE - 1, SIZE - 1), Work::Raise { level: TOP + 10 });
    let first = work.run(host);
    assert!(first.total() <= CELLS_PER_TICK);
    assert!(work.pending() > 0, "the order finished in one tick");

    let ticks = 1 + run_out(&mut work, host);
    let cells = SIZE * SIZE * 10;
    assert!(
        i64::from(ticks) >= cells / i64::from(CELLS_PER_TICK),
        "{cells} cells of order fitted into {ticks} ticks of {CELLS_PER_TICK}"
    );
}

#[test]
fn settling_gets_its_share_of_a_busy_tick() {
    // A collapse must not be starved by an unbounded queue of orders, and
    // orders must not stop entirely for a collapse. Both halves of §4e's
    // "one number, three problems" in one assertion.
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    Terraform::crater(host, (12, 12), 6);
    let mut work = Terraform::new();
    work.order((0, 0), (SIZE - 1, SIZE - 1), Work::Raise { level: TOP + 10 });
    let spent = work.run(host);
    assert!(spent.settled > 0, "the crater did not settle");
    assert!(spent.edited > 0, "the order got no allowance");
    assert!(spent.total() <= CELLS_PER_TICK);
}

#[test]
fn a_slump_is_a_terrain_edit_as_far_as_navigation_is_concerned() {
    // Settling is the only thing that reshapes the ground without going
    // through a paint verb, so it is the only place the "whoever changes
    // the ground invalidates what was derived from it" rule can be
    // forgotten. Forgetting it is not a visible bug — it is a vehicle
    // driving through the middle of a dune that slid there a second ago.
    let (backend, _phys) = plate(material::SAND);
    let host = backend.host();
    host.volume_fill(
        (10, 10, TOP + 1),
        (14, 14, TOP + 8),
        material::SAND,
        material::color(material::SAND),
    );
    let limits = VolumeLimits {
        bounds: (0, 0, SIZE - 1, SIZE - 1),
        z_range: (0, TOP + 20),
        budget: 20_000,
    };
    let route = || host.nav_path3((4, 4, TOP), (20, 20, TOP), VEHICLE, &limits);
    let before = route();
    assert!(!before.is_empty(), "no route across an open plate");

    // The needle collapses into a cone straddling that route.
    let mut work = Terraform::new();
    run_out(&mut work, host);
    assert!(
        host.volume_top(17, 12).expect("cone flank").0 > TOP,
        "the block did not spread"
    );

    assert_ne!(
        before,
        route(),
        "the route is unchanged: the stands under the cone were never dropped"
    );
}

// --- determinism (§12) ----------------------------------------------------

#[test]
fn the_same_terraforming_hashes_the_same_every_run() {
    // The D-3 gate: a berm, a trench and a crater settle to the same
    // hashed state. Integer arithmetic and a canonical sweep are what
    // make "every run" stand in for "every platform".
    let once = || {
        let (backend, phys) = plate(material::SAND);
        let host = backend.host();
        let mut work = Terraform::new();
        work.order((3, 3), (6, 6), Work::Raise { level: TOP + 4 });
        work.order(
            (16, 4),
            (17, 19),
            Work::Dig {
                level: TOP - 4,
                spoil: (20, 20),
            },
        );
        Terraform::crater(host, (10, 12), 5);
        run_out(&mut work, host);
        let sim = phys.lock().expect("physics mutex");
        sim.terrain.state_hash()
    };
    assert_eq!(once(), once());
}

#[test]
fn a_terraform_queue_survives_a_snapshot() {
    // Half-finished work is hashed state: a peer that restores mid-dig
    // and forgets the order has a different desert one second later.
    let mut rules = monada_desert_rules::DesertRules::new(DesertParams::default());
    rules
        .terraform()
        .order((0, 0), (9, 9), Work::Raise { level: TOP + 6 });
    let bytes = rules.snapshot();

    let mut restored = monada_desert_rules::DesertRules::new(DesertParams::default());
    restored.restore(&bytes);
    assert_eq!(restored.terraform().pending(), 1);
    // And an untouched one does not have it, so the assertion above is
    // about the bytes rather than about the constructor.
    assert_eq!(
        monada_desert_rules::DesertRules::new(DesertParams::default())
            .terraform()
            .pending(),
        0
    );
}

// --- the generator and the repose agree ----------------------------------

#[test]
fn the_generated_desert_is_already_at_rest() {
    // `init` registers sand as granular AFTER painting, which declares the
    // map at rest. That declaration has to be true, or the first shot
    // fired anywhere near a dune starts an avalanche that was always
    // pending. The generator earns it by construction — dune height moves
    // by at most a cell between neighbours — and this is the check that it
    // keeps earning it across seeds.
    for seed in 0..24_u32 {
        let desert = Desert::new(DesertParams {
            seed: seed.wrapping_mul(0x9E37_79B9) | 1,
            ..DesertParams::default()
        });
        for y in (1..monada_desert_rules::MAP_CELLS - 1).step_by(7) {
            for x in (1..monada_desert_rules::MAP_CELLS - 1).step_by(5) {
                let (here, surface) = desert.column(x, y);
                if !matches!(surface, Surface::Sand | Surface::Dune | Surface::Spice) {
                    continue; // rock and mountain hold any slope
                }
                for (dx, dy) in [(1_i64, 0_i64), (-1, 0), (0, 1), (0, -1)] {
                    let (there, _) = desert.column(x + dx, y + dy);
                    assert!(
                        here - there <= SAND_REPOSE.max_drop,
                        "seed {seed}: sand at ({x}, {y}) stands {here} beside {there}"
                    );
                }
            }
        }
    }
}
