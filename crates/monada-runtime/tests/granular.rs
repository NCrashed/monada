//! What settling promises (docs/plans/desert-game.md §4d).
//!
//! The automaton exists so terraforming has a cost: a trench that stays
//! vertical forever is Lego, not terrain. So the tests are about the
//! material's behaviour — a steep pile collapses to its angle, rock does
//! not, a collapse is bounded per tick and identical on every peer — and
//! not about the sweep's internals, which are free to change.

use monada_runtime::{Granular, MaterialId, Repose, VolumeStore};

const SAND: MaterialId = MaterialId(0);
const ROCK: MaterialId = MaterialId(1);

/// Sand: one cell of drop is the most it will hold.
fn sandy() -> Granular {
    let mut g = Granular::new();
    g.register(SAND, Repose { max_drop: 1 });
    g
}

/// Flat ground with a tower of `height` cells standing on one column.
fn tower(material: MaterialId, height: i64) -> VolumeStore {
    let mut store = VolumeStore::new();
    store.fill(0, 0, 0, 20, 20, 0, material);
    store.fill(10, 10, 1, 10, 10, height, material);
    store
}

/// Settle until nothing moves, or the guard trips.
fn settle_out(g: &mut Granular, store: &mut VolumeStore, budget: u32) -> u32 {
    let mut ticks = 0;
    while !g.settle(store, budget).is_empty() {
        ticks += 1;
        assert!(ticks < 10_000, "the pile never came to rest");
    }
    ticks
}

#[test]
fn a_tower_of_sand_collapses_into_a_cone() {
    let mut store = tower(SAND, 8);
    let mut g = sandy();
    g.disturb((10, 10), (10, 10));
    settle_out(&mut g, &mut store, 64);

    // Nowhere may a column stand more than one cell above a neighbour.
    for y in 1..20 {
        for x in 1..20 {
            let here = store.column_top(x, y).expect("ground").0;
            for (dx, dy) in [(1_i64, 0_i64), (0, 1)] {
                let there = store.column_top(x + dx, y + dy).expect("ground").0;
                assert!(
                    (here - there).abs() <= 1,
                    "({x}, {y}) stands {here} beside {there} — steeper than sand holds"
                );
            }
        }
    }
    assert!(
        store.column_top(10, 10).expect("ground").0 < 8,
        "the tower should be shorter than it was"
    );
}

#[test]
fn rock_does_not_flow() {
    // The same tower in a material nobody declared granular: a Surfling's
    // packed fill and a Binder's glass are supposed to stand exactly like
    // this.
    let mut store = tower(ROCK, 8);
    let before = store.state_hash();
    let mut g = sandy();
    g.disturb((10, 10), (10, 10));
    assert!(g.settle(&mut store, 1000).is_empty());
    assert_eq!(store.state_hash(), before, "rock moved");
}

#[test]
fn nothing_is_conjured_or_destroyed() {
    // Mass is conserved: a slump moves cells, it does not create them.
    // Dwellers' whole economy rests on this (§6b) — a trench here is a
    // spoil heap there.
    let count = |s: &VolumeStore| {
        let mut n = 0;
        for y in -2_i64..24 {
            for x in -2_i64..24 {
                if let Some((top, _)) = s.column_top(x, y) {
                    for z in 0..=top {
                        if s.get(x, y, z).is_some() {
                            n += 1;
                        }
                    }
                }
            }
        }
        n
    };
    let mut store = tower(SAND, 10);
    let before = count(&store);
    let mut g = sandy();
    g.disturb((10, 10), (10, 10));
    settle_out(&mut g, &mut store, 64);
    assert_eq!(
        count(&store),
        before,
        "the slump changed how much sand exists"
    );
}

#[test]
fn a_collapse_is_the_same_on_every_peer() {
    let run = || {
        let mut store = tower(SAND, 9);
        let mut g = sandy();
        g.disturb((10, 10), (10, 10));
        settle_out(&mut g, &mut store, 7); // an awkward budget, on purpose
        store.state_hash()
    };
    assert_eq!(run(), run());
}

#[test]
fn the_budget_bounds_the_work() {
    let mut store = tower(SAND, 12);
    let mut g = sandy();
    g.disturb((10, 10), (10, 10));
    for _ in 0..5 {
        let moved = g.settle(&mut store, 3).len();
        assert!(moved <= 3, "a budget of three moved {moved} cells");
    }
    assert!(g.pending() > 0, "the pile should still be slumping");
}

#[test]
fn quiet_terrain_costs_nothing() {
    let mut store = tower(SAND, 1);
    let mut g = sandy();
    // Never disturbed: the automaton has no reason to look at anything.
    assert!(g.settle(&mut store, 1000).is_empty());
    assert_eq!(g.pending(), 0);
}

#[test]
fn sand_does_not_slide_off_the_edge_of_the_world() {
    // A painted island — a test plate, a floating platform — has columns
    // of pure air beside it. The store is unbounded below, so an empty
    // column is not "very low ground", it is *no ground*: read the other
    // way, the edge grains slide into the void, land near `i64::MIN`, and
    // the whole island drains away one cell at a time.
    let mut store = VolumeStore::new();
    store.fill(0, 0, 0, 5, 5, 9, SAND); // a tall block standing in nothing
    let mut g = sandy();
    g.disturb((0, 0), (5, 5));
    settle_out(&mut g, &mut store, 64);

    for y in 0..=5 {
        for x in 0..=5 {
            assert_eq!(
                store.column_top(x, y),
                Some((9, SAND.0)),
                "({x}, {y}) left the plate"
            );
        }
    }
    assert_eq!(store.column_top(-1, 0), None, "sand appeared beside it");
}

#[test]
fn a_map_with_no_granular_material_is_unaffected() {
    // The canonical-form rule that lets this ship without re-blessing
    // every existing golden: an inert automaton contributes nothing.
    let g = Granular::new();
    assert!(g.is_inert());
    let mut g2 = Granular::new();
    g2.disturb((0, 0), (100, 100));
    assert!(
        g2.is_inert(),
        "disturbing an inert automaton must not wake it"
    );
}
