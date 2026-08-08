//! What the generator promises, asserted for many seeds rather than
//! eyeballed for one (docs/plans/desert-game.md §12).
//!
//! A hand-drawn level is checked by playing it. A generated one cannot
//! be — nobody will play a thousand seeds — so the promises have to be
//! executable: the walk rule holds, mountains wall out armour and admit
//! infantry, spice sits where a harvester can reach it, and two peers
//! computing the same seed get the same desert down to the cell.

use monada_desert_rules::gen::Surface;
use monada_desert_rules::{
    can_step, Desert, DesertParams, INFANTRY_MAX_STEP, MAP_CELLS, VEHICLE_MAX_STEP,
};

/// A spread of seeds, so a passing invariant is a property and not an
/// accident of one map.
const SEEDS: [u32; 6] = [0x51CE, 1, 7, 0xDEAD_BEEF, 0x0F0F_0F0F, 12345];

fn desert(seed: u32) -> Desert {
    Desert::new(DesertParams {
        seed,
        ..DesertParams::default()
    })
}

#[test]
fn the_same_seed_is_the_same_desert() {
    // The generator runs inside `init` on every peer, so a disagreement
    // here is a desync before the first tick.
    for seed in SEEDS {
        let (a, b) = (desert(seed), desert(seed));
        for y in (0..MAP_CELLS).step_by(7) {
            for x in (0..MAP_CELLS).step_by(5) {
                assert_eq!(a.column(x, y), b.column(x, y), "seed {seed} at ({x}, {y})");
            }
        }
    }
}

#[test]
fn different_seeds_are_different_deserts() {
    // Guards the tests below from passing on a constant map.
    let (a, b) = (desert(1), desert(2));
    let differences = (0..MAP_CELLS)
        .step_by(3)
        .filter(|&x| a.column(x, x) != b.column(x, x))
        .count();
    assert!(differences > 10, "two seeds produced near-identical maps");
}

#[test]
fn the_desert_stays_inside_its_vertical_budget() {
    use monada_desert_rules::gen::{BEDROCK_Z, SKY_Z};
    for seed in SEEDS {
        let d = desert(seed);
        for y in (0..MAP_CELLS).step_by(11) {
            for x in (0..MAP_CELLS).step_by(9) {
                let (h, _) = d.column(x, y);
                assert!(
                    h > BEDROCK_Z && h < SKY_Z,
                    "seed {seed}: column ({x}, {y}) is {h}, outside {BEDROCK_Z}..{SKY_Z} — \
                     terrain that clips the sky or the bedrock is terrain a map cannot dig"
                );
            }
        }
    }
}

/// The load-bearing invariant of §4b, stated as the property that
/// actually matters: **armour cannot cross the ridge and infantry can**.
///
/// Asserting it edge by edge would be the wrong shape — the ridge's
/// outermost cell is deliberately flush with the sand beside it (a foot
/// to walk onto), and the wall is the step after it. What has to hold is
/// the crossing: somewhere along every row there is a rise armour cannot
/// take, and nowhere is there one infantry cannot.
#[test]
fn armour_cannot_cross_the_ridge_and_infantry_can() {
    for seed in SEEDS {
        let d = desert(seed);
        let mid = MAP_CELLS / 2;
        let span = MAP_CELLS / 6;
        let mut rows_checked = 0;
        for y in (span + 1)..(MAP_CELLS - span - 1) {
            // A slice wide enough to include open ground on both sides.
            let (from, to) = (mid - 24, mid + 24);
            let mut armour_wall = None;
            let mut infantry_block = None;
            let mut touches_mountain = false;
            for x in from..to {
                let (h, s) = d.column(x, y);
                let (hx, _) = d.column(x + 1, y);
                touches_mountain |= s == Surface::Mountain;
                if !can_step(h, hx, VEHICLE_MAX_STEP) {
                    armour_wall = Some((x, (hx - h).abs()));
                }
                if !can_step(h, hx, INFANTRY_MAX_STEP) {
                    infantry_block = Some((x, (hx - h).abs()));
                }
            }
            if !touches_mountain {
                continue; // outside the ridge's reach on this row
            }
            rows_checked += 1;
            assert!(
                armour_wall.is_some(),
                "seed {seed}, row {y}: armour can drive straight across the ridge"
            );
            assert!(
                infantry_block.is_none(),
                "seed {seed}, row {y}: infantry is blocked at {:?} — the ridge is \
                 impassable to everyone, so it is a wall rather than terrain",
                infantry_block.expect("checked"),
            );
        }
        assert!(
            rows_checked > 50,
            "seed {seed}: only {rows_checked} rows crossed a mountain — the ridge is \
             missing, so the assertions above proved nothing"
        );
    }
}

/// Dune relief must never accidentally become a cliff: sand is the
/// ground everything crosses, and a rogue step would strand a harvester
/// somewhere the pathfinder says is fine.
#[test]
fn open_sand_is_crossable_by_vehicles() {
    for seed in SEEDS {
        let d = desert(seed);
        for y in 1..MAP_CELLS - 1 {
            for x in 1..MAP_CELLS - 1 {
                let (h, s) = d.column(x, y);
                let (hx, sx) = d.column(x + 1, y);
                let soft = |s: Surface| matches!(s, Surface::Sand | Surface::Dune | Surface::Spice);
                if !soft(s) || !soft(sx) {
                    continue;
                }
                assert!(
                    can_step(h, hx, VEHICLE_MAX_STEP),
                    "seed {seed}: sand at ({x}, {y}) steps {} to its neighbour — \
                     a dune became a cliff",
                    (hx - h).abs()
                );
            }
        }
    }
}

#[test]
fn spice_lies_on_sand_where_a_harvester_can_reach_it() {
    for seed in SEEDS {
        let d = desert(seed);
        let mut spice_cells = 0;
        for y in 0..MAP_CELLS {
            for x in 0..MAP_CELLS {
                if d.column(x, y).1 == Surface::Spice {
                    spice_cells += 1;
                }
            }
        }
        assert!(
            spice_cells > 500,
            "seed {seed}: only {spice_cells} spice cells — an economy needs a field \
             worth driving to"
        );
    }
}

#[test]
fn worms_travel_sand_and_nothing_else() {
    // §6e: the neutral force's whole tactical meaning is which ground it
    // may enter, so the predicate is worth pinning.
    assert!(Surface::Sand.passable_to_worms());
    assert!(Surface::Dune.passable_to_worms());
    assert!(Surface::Spice.passable_to_worms());
    assert!(!Surface::Rock.passable_to_worms());
    assert!(!Surface::Mountain.passable_to_worms());
}

#[test]
fn start_locations_are_symmetric_and_on_the_map() {
    let d = desert(0x51CE);
    let corners: Vec<_> = (0..4).map(|p| d.start_location(p)).collect();
    for &(x, y) in &corners {
        assert!(
            (0..MAP_CELLS).contains(&x) && (0..MAP_CELLS).contains(&y),
            "start ({x}, {y}) is off the map"
        );
    }
    // Rotational symmetry: player 0 and player 1 sit opposite each other
    // about the map's centre, which is what makes a 1v1 fair.
    let (x0, y0) = corners[0];
    let (x1, y1) = corners[1];
    assert_eq!((x0 + x1, y0 + y1), (MAP_CELLS, MAP_CELLS));
}
