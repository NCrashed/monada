//! Base building in three dimensions (docs/plans/desert-game.md §7) —
//! the D-5 gate.
//!
//! The gate is "a headless build order produces an exact base on both a
//! plateau and in a pit", and
//! [`the_same_build_order_works_on_a_plateau_and_in_a_pit`] is it. The
//! rest is why that is worth asserting: a volumetric map asks questions
//! of a build site that a tile map cannot, and each of them has to have
//! an answer a player can act on.

use std::sync::{Arc, Mutex};

use monada_runtime::{
    shared_physics, shared_world, Host, MapRules, NativeBackend, NullBridge, ScriptBackend,
    SharedBridge,
};
use monada_sim::EntityId;

use monada_desert_rules::build::{
    bears, blueprint, Exposure, Refusal, Yards, ADJACENCY, MAX_GRADE,
};
use monada_desert_rules::economy::Economy;
use monada_desert_rules::{material, Structure, SAND_REPOSE};

const SIZE: i64 = 84;
const GROUND: i64 = 20;

/// Where the test terrain puts its two build sites.
const PLATEAU: (i64, i64) = (14, 14);
const PIT: (i64, i64) = (56, 56);
/// How high the plateau stands and how deep the pit is cut.
const RELIEF: i64 = 6;

/// A backend with the two sites cut into it. (Written by hand rather
/// than through `MapRules` so the shapes stay readable.)
fn ground() -> NativeBackend {
    struct Flat;
    impl MapRules for Flat {
        fn init(&mut self, host: &dyn Host) {
            host.volume_fill(
                (0, 0, 0),
                (SIZE - 1, SIZE - 1, GROUND),
                material::SAND,
                material::color(material::SAND),
            );
            // A rock plateau: level, bearing, and standing proud of the
            // sand around it.
            host.volume_fill(
                (PLATEAU.0 - 6, PLATEAU.1 - 6, 0),
                (PLATEAU.0 + 30, PLATEAU.1 + 30, GROUND + RELIEF),
                material::ROCK,
                material::color(material::ROCK),
            );
            // A pit with a rock floor, cut into the sand.
            host.volume_fill(
                (PIT.0 - 6, PIT.1 - 6, 0),
                (PIT.0 + 22, PIT.1 + 22, GROUND - RELIEF),
                material::ROCK,
                material::color(material::ROCK),
            );
            for y in (PIT.1 - 6)..=(PIT.1 + 22) {
                for x in (PIT.0 - 6)..=(PIT.0 + 22) {
                    for z in (GROUND - RELIEF + 1)..=GROUND {
                        host.volume_clear(x, y, z);
                    }
                }
            }
            host.granular_register(material::SAND, SAND_REPOSE);
        }
    }
    let mut backend = NativeBackend::new(shared_world(5), Box::new(Flat));
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    backend.set_volume(&shared_physics(30));
    backend.on_init().expect("init");
    backend
}

/// The build order the gate runs: a yard, then the three structures that
/// hang off it, each placed at a fixed offset from the yard.
const ORDER: [(Structure, (i64, i64)); 4] = [
    (Structure::Yard, (0, 0)),
    (Structure::WindTrap, (10, 0)),
    (Structure::Refinery, (0, 10)),
    (Structure::Silo, (10, 6)),
];

/// Run the order at `origin`; returns what was built, in order.
fn build_base(
    host: &dyn Host,
    yards: &mut Yards,
    economy: &mut Economy,
    origin: (i64, i64),
    first_id: u64,
) -> Vec<(Structure, i64, Exposure, bool)> {
    let mut built = Vec::new();
    for (n, (kind, offset)) in (first_id..).zip(ORDER) {
        let at = (origin.0 + offset.0, origin.1 + offset.1);
        let bp = blueprint(kind).expect("in the catalogue");
        assert!(
            economy.player(0).charge(bp.cost),
            "ran out of credits before {kind:?}"
        );
        let site = yards
            .survey(host, 0, kind, at)
            .unwrap_or_else(|why| panic!("{kind:?} at {at:?} refused: {why:?}"));
        let entity = EntityId(n);
        yards.raise(host, 0, kind, site, entity);
        built.push((kind, site.pad_z, site.exposure, site.firm));
    }
    built
}

// --- the gate -------------------------------------------------------------

#[test]
fn the_same_build_order_works_on_a_plateau_and_in_a_pit() {
    let backend = ground();
    let host = backend.host();
    let mut yards = Yards::new();
    let mut economy = Economy::new();
    economy.found(0, 5_000);

    let high = build_base(host, &mut yards, &mut economy, PLATEAU, 1);
    let low = build_base(host, &mut yards, &mut economy, PIT, 100);

    // The same four structures, in the same order, on both sites.
    let kinds: Vec<Structure> = high.iter().map(|b| b.0).collect();
    assert_eq!(kinds, low.iter().map(|b| b.0).collect::<Vec<_>>());
    assert_eq!(kinds, ORDER.iter().map(|o| o.0).collect::<Vec<_>>());

    // Every pad is exactly where the ground was, and all on rock.
    for b in &high {
        assert_eq!(b.1, GROUND + RELIEF, "{:?} graded to the wrong level", b.0);
        assert!(b.3, "{:?} is not on bearing ground", b.0);
        assert_ne!(b.2, Exposure::Buried, "{:?} on a plateau reads as buried", b.0);
    }
    for b in &low {
        assert_eq!(b.1, GROUND - RELIEF, "{:?} graded to the wrong level", b.0);
        assert!(b.3, "{:?} is not on bearing ground", b.0);
        assert_ne!(b.2, Exposure::Elevated, "{:?} in a pit reads as a berm", b.0);
    }
    // And the two bases read as what they are. Not *every* structure —
    // a silo tucked in the middle of a base has base all around it and
    // is level with respect to anything that could shoot at it, which is
    // the honest answer and the one D-6 will want.
    assert!(
        high.iter().any(|b| b.2 == Exposure::Elevated),
        "nothing on the plateau reads as elevated"
    );
    assert!(
        low.iter().any(|b| b.2 == Exposure::Buried),
        "nothing in the pit reads as buried"
    );
    assert_eq!(yards.len(), 8, "eight structures, four on each site");
}

#[test]
fn the_base_is_the_same_on_every_peer() {
    let once = || {
        let backend = ground();
        let mut yards = Yards::new();
        let mut economy = Economy::new();
        economy.found(0, 5_000);
        let built = build_base(backend.host(), &mut yards, &mut economy, PLATEAU, 1);
        // The terrain digest too: a pad is a terrain edit, so two peers
        // that agree about the base and not about the ground under it
        // have not agreed about anything.
        (built, economy.get(0).expect("player").credits)
    };
    assert_eq!(once(), once());
}

// --- what a volumetric site refuses ---------------------------------------

#[test]
fn a_dune_is_too_steep_to_build_on_and_says_so() {
    let backend = ground();
    let host = backend.host();
    // A ridge of sand the pad would have to span.
    for y in 0..SIZE {
        host.volume_fill(
            (48, y, GROUND + 1),
            (48, y, GROUND + MAX_GRADE + 2),
            material::SAND,
            material::color(material::SAND),
        );
    }
    let yards = Yards::new();
    assert_eq!(
        yards.survey(host, 0, Structure::Yard, (46, 10)),
        Err(Refusal::TooSteep)
    );
}

#[test]
fn a_structure_must_touch_the_base_but_a_yard_need_not() {
    let backend = ground();
    let host = backend.host();
    let mut yards = Yards::new();

    // Nothing standing: a yard is fine, a refinery is not.
    assert!(yards.survey(host, 0, Structure::Yard, PLATEAU).is_ok());
    assert_eq!(
        yards.survey(host, 0, Structure::Refinery, PLATEAU),
        Err(Refusal::Unconnected)
    );

    let site = yards.survey(host, 0, Structure::Yard, PLATEAU).expect("pad");
    yards.raise(host, 0, Structure::Yard, site, EntityId(1));

    let close = (PLATEAU.0 + 8 + ADJACENCY - 1, PLATEAU.1);
    assert!(
        yards.survey(host, 0, Structure::Refinery, close).is_ok(),
        "a refinery beside the yard was refused"
    );
    let far = (PLATEAU.0 + 8 + ADJACENCY + 2, PLATEAU.1);
    assert_eq!(
        yards.survey(host, 0, Structure::Refinery, far),
        Err(Refusal::Unconnected)
    );
}

#[test]
fn two_structures_may_not_share_a_cell() {
    let backend = ground();
    let host = backend.host();
    let mut yards = Yards::new();
    let site = yards.survey(host, 0, Structure::Yard, PLATEAU).expect("pad");
    yards.raise(host, 0, Structure::Yard, site, EntityId(1));
    assert_eq!(
        yards.survey(host, 0, Structure::Silo, (PLATEAU.0 + 4, PLATEAU.1 + 4)),
        Err(Refusal::Occupied)
    );
}

#[test]
fn a_pad_is_graded_flat_and_the_navigation_hears_about_it() {
    // A building is a terrain edit, and it has to be: a route planned
    // across the cell a refinery now stands on is a unit driving through
    // a refinery.
    let backend = ground();
    let host = backend.host();
    // Scoop one corner of the plateau out so the pad has work to do.
    host.volume_clear(PLATEAU.0, PLATEAU.1, GROUND + RELIEF);
    assert_eq!(
        host.volume_top(PLATEAU.0, PLATEAU.1).expect("column").0,
        GROUND + RELIEF - 1
    );

    let mut yards = Yards::new();
    let site = yards.survey(host, 0, Structure::Yard, PLATEAU).expect("pad");
    yards.raise(host, 0, Structure::Yard, site, EntityId(1));

    for y in PLATEAU.1..(PLATEAU.1 + 8) {
        for x in PLATEAU.0..(PLATEAU.0 + 8) {
            let (top, mat) = host.volume_top(x, y).expect("pad column");
            assert_eq!(top, GROUND + RELIEF, "({x}, {y}) is not level");
            assert!(bears(mat), "({x}, {y}) is not bearing material");
        }
    }
}

// --- poor ground ----------------------------------------------------------

#[test]
fn sand_bears_a_building_and_then_gives_way() {
    // Dune II's concrete rule, made literal (§6a): a structure on raw
    // sand goes up and then settles. The counter is a Surfling pad, which
    // is a material in the world rather than a rule in the code.
    let backend = ground();
    let host = backend.host();
    let mut yards = Yards::new();
    let mut economy = Economy::new();
    economy.found(0, 5_000);

    // Out on the open sand, well away from either prepared site.
    let sandy = (60, 8);
    let site = yards
        .survey(host, 0, Structure::Yard, sandy)
        .expect("sand still takes a building");
    assert!(!site.firm, "raw sand should not count as bearing ground");
    yards.raise(host, 0, Structure::Yard, site, EntityId(1));

    let full = yards.get(EntityId(1)).expect("standing").health;
    for _ in 0..100 {
        yards.weather(&mut economy);
    }
    let worn = yards.get(EntityId(1)).expect("standing").health;
    assert!(worn < full, "sand held it up perfectly");

    // The same building on the rock plateau does not move.
    let firm = yards.survey(host, 0, Structure::Yard, PLATEAU).expect("pad");
    assert!(firm.firm);
    yards.raise(host, 0, Structure::Yard, firm, EntityId(2));
    let before = yards.get(EntityId(2)).expect("standing").health;
    for _ in 0..100 {
        yards.weather(&mut economy);
    }
    assert_eq!(
        yards.get(EntityId(2)).expect("standing").health,
        before,
        "rock gave way"
    );
}

#[test]
fn repair_costs_credits_and_stops_at_full() {
    let backend = ground();
    let host = backend.host();
    let mut yards = Yards::new();
    let mut economy = Economy::new();
    economy.found(0, 5_000);
    let site = yards.survey(host, 0, Structure::Yard, (60, 8)).expect("pad");
    yards.raise(host, 0, Structure::Yard, site, EntityId(1));
    for _ in 0..200 {
        yards.weather(&mut economy);
    }

    let hurt = yards.get(EntityId(1)).expect("standing").health;
    let purse = economy.get(0).expect("player").credits;
    let spent = yards.repair(&mut economy, EntityId(1), 500);
    assert!(spent > 0, "repairing a damaged building cost nothing");
    assert_eq!(economy.get(0).expect("player").credits, purse - spent);
    assert!(yards.get(EntityId(1)).expect("standing").health > hurt);

    // Full: nothing more to do, nothing more to pay.
    yards.repair(&mut economy, EntityId(1), 5_000);
    let purse = economy.get(0).expect("player").credits;
    assert_eq!(yards.repair(&mut economy, EntityId(1), 500), 0);
    assert_eq!(economy.get(0).expect("player").credits, purse);
}

// --- the build line -------------------------------------------------------

#[test]
fn the_line_takes_one_at_a_time_and_power_sets_the_pace() {
    let mut economy = Economy::new();
    economy.found(0, 5_000);
    economy.begin_tick();
    economy.count(
        [
            monada_desert_rules::Building {
                owner: 0,
                kind: Structure::Yard,
            },
            monada_desert_rules::Building {
                owner: 0,
                kind: Structure::WindTrap,
            },
        ]
        .into_iter(),
    );

    let mut yards = Yards::new();
    assert!(yards.queue(0).order(&mut economy, 0, Structure::Refinery));
    assert!(
        !yards.queue(0).order(&mut economy, 0, Structure::Silo),
        "the line took a second item"
    );

    let bp = blueprint(Structure::Refinery).expect("catalogue");
    let mut ticks = 0;
    while yards.queue(0).ready().is_none() {
        yards.queue(0).tick(&mut economy, 0);
        ticks += 1;
        assert!(ticks < 10_000, "the line stalled");
    }
    assert_eq!(
        ticks,
        i32::try_from(bp.ticks).unwrap() / 10,
        "full power should be ten ticks of work per tick"
    );
    assert_eq!(yards.queue(0).take(), Some(Structure::Refinery));
    assert!(yards.queue(0).ready().is_none());
}

#[test]
fn a_blackout_slows_the_line_without_stalling_it() {
    let mut economy = Economy::new();
    economy.found(0, 5_000);
    // A yard drawing power with nothing generating: satisfaction zero.
    economy.begin_tick();
    economy.count(
        [monada_desert_rules::Building {
            owner: 0,
            kind: Structure::Yard,
        }]
        .into_iter(),
    );
    assert_eq!(economy.player(0).satisfaction(), 0);

    let mut yards = Yards::new();
    assert!(yards.queue(0).order(&mut economy, 0, Structure::Silo));
    let mut ticks = 0;
    while yards.queue(0).ready().is_none() {
        yards.queue(0).tick(&mut economy, 0);
        ticks += 1;
        assert!(ticks < 100_000, "the line stalled in the dark");
    }
    let bp = blueprint(Structure::Silo).expect("catalogue");
    assert_eq!(ticks, i32::try_from(bp.ticks).unwrap());
}

#[test]
fn an_unaffordable_order_is_refused_and_costs_nothing() {
    let mut economy = Economy::new();
    economy.found(0, 10);
    let mut yards = Yards::new();
    assert!(!yards.queue(0).order(&mut economy, 0, Structure::Refinery));
    assert_eq!(economy.get(0).expect("player").credits, 10);
    assert!(yards.queue(0).building().is_none());
}
