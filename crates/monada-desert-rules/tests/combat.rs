//! What the ground does to a firefight (docs/plans/desert-game.md §7) —
//! the D-6 gate.
//!
//! The gate is "a scripted skirmish resolves identically every run", and
//! [`a_scripted_skirmish_resolves_the_same_every_run`] is it. The rest is
//! the reason the slice exists: on a volumetric map, whether a gun can
//! hit a tank is a question about the voxels between them, and every
//! terraforming verb of D-3 is therefore also a combat verb.

use std::sync::{Arc, Mutex};

use monada_runtime::{
    shared_physics, shared_world, Host, MapRules, NativeBackend, NullBridge, ScriptBackend,
    SharedBridge, WorldRead,
};
use monada_sim::EntityId;

use monada_desert_rules::combat::{Armour, Battle, Report, Weapon, MUZZLE};
use monada_desert_rules::terraform::{Terraform, Work, CELLS_PER_TICK};
use monada_desert_rules::{material, SAND_REPOSE};

const SIZE: i64 = 80;
const GROUND: i64 = 20;

/// A flat plain to shoot across.
struct Plain;

impl MapRules for Plain {
    fn init(&mut self, host: &dyn Host) {
        host.volume_fill(
            (0, 0, 0),
            (SIZE - 1, SIZE - 1, GROUND),
            material::SAND,
            material::color(material::SAND),
        );
        host.granular_register(material::SAND, SAND_REPOSE);
    }
}

fn plain() -> NativeBackend {
    let mut backend = NativeBackend::new(shared_world(3), Box::new(Plain));
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    backend.set_volume(&shared_physics(30));
    backend.on_init().expect("init");
    backend
}

/// Put a fighter on the ground at a cell.
fn deploy(
    host: &dyn Host,
    battle: &mut Battle,
    owner: u8,
    at: (i64, i64),
    armour: Armour,
    weapon: Weapon,
) -> EntityId {
    let kind = host.archetype(&["owner"]);
    let e = host.entity_create(kind);
    let z = host.volume_top(at.0, at.1).map_or(GROUND, |(z, _)| z) + 1;
    host.entity_set_position(
        e,
        monada_fixed::FixedVec3::new(
            monada_fixed::Fixed::from_int(i32::try_from(at.0).unwrap()),
            monada_fixed::Fixed::from_int(i32::try_from(at.1).unwrap()),
            monada_fixed::Fixed::from_int(i32::try_from(z).unwrap()),
        ),
    );
    battle.enlist(e, owner, armour, weapon);
    e
}

/// Run the battle, totalling what happened.
fn fight(host: &dyn Host, battle: &mut Battle, ticks: u32) -> Report {
    // No structures in these tests: a fight between units alone.
    let mut yards = monada_desert_rules::Yards::new();
    let mut total = Report::default();
    for _ in 0..ticks {
        let r = battle.run(host, &mut yards);
        total.shots += r.shots;
        total.hits += r.hits;
        total.kills += r.kills;
        total.cratered += r.cratered;
    }
    total
}

/// Raise a wall of packed fill across the plain at `x`, `height` cells
/// proud of the ground. Packed fill, so it does not slump.
fn wall(host: &dyn Host, x: i64, height: i64) {
    host.volume_fill(
        (x, 0, GROUND + 1),
        (x, SIZE - 1, GROUND + height),
        material::PACKED_FILL,
        material::color(material::PACKED_FILL),
    );
}

// --- the gate -------------------------------------------------------------

#[test]
fn a_scripted_skirmish_resolves_the_same_every_run() {
    // Four against four, fixed positions, no orders — everything that
    // happens is the rules resolving. Twice, and the two runs must agree
    // down to the last tenth of health.
    let once = || {
        let backend = plain();
        let host = backend.host();
        let mut battle = Battle::new();
        let mut units = Vec::new();
        for i in 0..4 {
            units.push(deploy(
                host,
                &mut battle,
                0,
                (20, 30 + i * 3),
                Armour::Heavy,
                Weapon::Cannon,
            ));
            units.push(deploy(
                host,
                &mut battle,
                1,
                (28, 30 + i * 3),
                Armour::Light,
                Weapon::Gun,
            ));
        }
        let report = fight(host, &mut battle, 600);
        let health: Vec<Option<u32>> = units
            .iter()
            .map(|&u| battle.get(u).map(|f| f.health))
            .collect();
        (report, health, battle.len())
    };
    let (a, ha, na) = once();
    let (b, hb, nb) = once();
    assert_eq!((a, ha, na), (b, hb, nb));

    // And it was a fight, not a stalemate: the cannons should have
    // finished the lighter side.
    assert!(a.shots > 0 && a.hits > 0, "nobody fired: {a:?}");
    assert!(a.kills > 0, "six hundred ticks and nobody died: {a:?}");
}

// --- the ground decides ---------------------------------------------------

#[test]
fn a_berm_stops_a_cannon_and_a_mortar_goes_over_it() {
    // The whole slice in one assertion. Same two units, same distance,
    // same six hundred ticks — the only difference is a wall of packed
    // fill between them, and the only difference in the answer is
    // whether the weapon has to travel in a straight line.
    let duel = |weapon: Weapon, walled: bool| {
        let backend = plain();
        let host = backend.host();
        if walled {
            wall(host, 24, 6);
        }
        let mut battle = Battle::new();
        deploy(host, &mut battle, 0, (20, 40), Armour::Heavy, weapon);
        let victim = deploy(host, &mut battle, 1, (28, 40), Armour::Light, Weapon::Gun);
        let report = fight(host, &mut battle, 600);
        (report, battle.get(victim).map(|f| f.health))
    };

    let (open, _) = duel(Weapon::Cannon, false);
    assert!(open.hits > 0, "a cannon on open ground hit nothing");

    let (blocked, alive) = duel(Weapon::Cannon, true);
    assert_eq!(blocked.shots, 0, "the cannon fired through a berm");
    assert!(alive.is_some(), "something killed the target behind a berm");

    let (lobbed, _) = duel(Weapon::Mortar, true);
    assert!(
        lobbed.hits > 0,
        "a mortar could not get over a six-cell wall"
    );
}

#[test]
fn a_trench_is_cover_until_somebody_fills_it_in() {
    // The other half of the same rule, and the Dweller's whole defensive
    // game: down in a cut, the ground you removed is what hides you.
    let backend = plain();
    let host = backend.host();
    let mut dig = Terraform::new();
    dig.order(
        (26, 36),
        (30, 44),
        Work::Dig {
            level: GROUND - 5,
            spoil: (60, 60),
        },
    );
    for _ in 0..400 {
        dig.run(host, CELLS_PER_TICK);
    }
    assert_eq!(dig.pending(), 0, "the trench was never finished");

    let mut battle = Battle::new();
    deploy(host, &mut battle, 0, (18, 40), Armour::Heavy, Weapon::Cannon);
    let hidden = deploy(host, &mut battle, 1, (28, 40), Armour::Light, Weapon::Gun);
    // Down in the cut, and the rim between is what stops the shot.
    let dug = host.volume_top(28, 40).expect("trench floor").0;
    assert!(dug < GROUND, "the trench is not below the plain");

    let sheltered = fight(host, &mut battle, 300);
    assert_eq!(sheltered.shots, 0, "the cannon shot into a trench");
    assert!(battle.get(hidden).is_some());

    // Fill the cut back in — the Surfling answer to a trench — and the
    // unit that was hiding in it is standing on the plain again with
    // nothing between it and the gun.
    let mut fill = Terraform::new();
    fill.order((26, 36), (30, 44), Work::Raise { level: GROUND });
    for _ in 0..800 {
        fill.run(host, CELLS_PER_TICK);
    }
    assert_eq!(fill.pending(), 0, "the cut was never filled");
    assert_eq!(
        host.volume_top(28, 40).expect("filled ground").0,
        GROUND,
        "the fill did not reach the plain"
    );
    // The unit rides up with the ground it is standing on.
    host.entity_set_position(
        hidden,
        monada_fixed::FixedVec3::new(
            monada_fixed::Fixed::from_int(28),
            monada_fixed::Fixed::from_int(40),
            monada_fixed::Fixed::from_int(i32::try_from(GROUND + 1).unwrap()),
        ),
    );

    let exposed = fight(host, &mut battle, 300);
    assert!(
        exposed.shots > 0,
        "filling the cut did not open the line of fire"
    );
}

#[test]
fn height_is_range_the_hard_way() {
    // A gun on a berm shoots over a wall its twin on the flat cannot see
    // past — §6a's "height is range and sight", falling out of the ray
    // rather than being written as a rule.
    let backend = plain();
    let host = backend.host();
    wall(host, 24, 4);
    // A platform for one of them, well clear of the wall.
    host.volume_fill(
        (18, 36, GROUND + 1),
        (21, 44, GROUND + 6),
        material::PACKED_FILL,
        material::color(material::PACKED_FILL),
    );

    let mut battle = Battle::new();
    let high = deploy(host, &mut battle, 0, (20, 40), Armour::Heavy, Weapon::Cannon);
    let target = deploy(host, &mut battle, 1, (30, 40), Armour::Light, Weapon::Gun);
    let up = host.entity_position(high).z.floor_to_int();
    assert!(
        i64::from(up) > GROUND + 4,
        "the gun is not actually on the platform"
    );

    let over = fight(host, &mut battle, 300);
    assert!(over.hits > 0, "a gun on a berm could not shoot over a wall");
    assert!(battle.get(target).is_some_and(|f| f.health < f.max_health));
}

// --- splash ---------------------------------------------------------------

#[test]
fn a_shell_leaves_a_hole_in_the_map() {
    // Splash edits terrain, so a battlefield is a different place after
    // a battle — and, because spice is terrain, an artillery duel over a
    // field is an economic act as well as a military one.
    let backend = plain();
    let host = backend.host();
    let mut battle = Battle::new();
    deploy(host, &mut battle, 0, (20, 40), Armour::Heavy, Weapon::Mortar);
    deploy(host, &mut battle, 1, (40, 40), Armour::Light, Weapon::Gun);

    let before = host.volume_top(40, 40).expect("ground").0;
    let report = fight(host, &mut battle, 400);
    assert!(report.cratered > 0, "a mortar left the ground untouched");
    assert!(
        host.volume_top(40, 40).expect("ground").0 < before,
        "the impact point is no lower than it was"
    );
}

#[test]
fn a_shell_flies_at_a_place_not_at_a_unit() {
    // A committed shell is what makes a slow weapon dodgeable and a
    // mortar a positional threat rather than a homing missile.
    let backend = plain();
    let host = backend.host();
    let mut battle = Battle::new();
    deploy(host, &mut battle, 0, (20, 40), Armour::Heavy, Weapon::Mortar);
    let runner = deploy(host, &mut battle, 1, (40, 40), Armour::Light, Weapon::Gun);

    // One tick to fire, then the target leaves.
    let mut empty = monada_desert_rules::Yards::new();
    let fired = battle.run(host, &mut empty);
    assert_eq!(fired.shots, 1);
    assert_eq!(battle.shells(), 1, "the shell is not in the air");
    host.entity_set_position(
        runner,
        monada_fixed::FixedVec3::new(
            monada_fixed::Fixed::from_int(60),
            monada_fixed::Fixed::from_int(60),
            monada_fixed::Fixed::from_int(i32::try_from(GROUND + 1).unwrap()),
        ),
    );

    let after = fight(host, &mut battle, 40);
    assert_eq!(after.hits, 0, "the shell followed the target");
    assert!(
        battle.get(runner).is_some_and(|f| f.health == f.max_health),
        "the runner took damage it drove away from"
    );
    // It still landed, and still moved earth where it was aimed.
    assert!(after.cratered > 0, "the shell vanished instead of landing");
}

// --- the ray itself -------------------------------------------------------

#[test]
fn the_line_of_fire_is_the_voxels_between() {
    let backend = plain();
    let host = backend.host();
    let muzzle = (20, 40, GROUND + 1 + MUZZLE);
    let target = (40, 40, GROUND + 1 + MUZZLE);
    assert_eq!(
        host.volume_ray(muzzle, target),
        None,
        "open ground blocked a shot"
    );

    wall(host, 30, 6);
    let hit = host.volume_ray(muzzle, target).expect("the wall is there");
    assert_eq!(hit.0, 30, "the ray stopped somewhere other than the wall");

    // Over the top: the same wall, from high enough up.
    let high = (20, 40, GROUND + 12);
    let far = (40, 40, GROUND + 12);
    assert_eq!(host.volume_ray(high, far), None, "the wall grew");
}
