//! What three-dimensional navigation promises
//! (docs/plans/desert-game.md §4c).
//!
//! The flat search had one walkable surface per column and could be
//! checked by eye on a grid. This one cannot: the whole point is that a
//! column may hold several stands, so the interesting cases — a bore
//! under a ridge, a mover too tall for a gallery, a ceiling that turns
//! ground into a tunnel — are exactly the ones a diagram does not settle.

use monada_nav::{MoverProfile, NavVolume, VolumeLimits, VolumeWorld};

/// A test world built from a closure: solid where the predicate says so.
struct World<F: Fn(i64, i64, i64) -> bool>(F);

impl<F: Fn(i64, i64, i64) -> bool> VolumeWorld for World<F> {
    fn solid(&self, x: i64, y: i64, z: i64) -> bool {
        (self.0)(x, y, z)
    }
}

/// Flat ground at z = 10, everything above it air.
fn plain() -> World<impl Fn(i64, i64, i64) -> bool> {
    World(|_x, _y, z| z <= 10)
}

fn limits() -> VolumeLimits {
    VolumeLimits {
        bounds: (0, 0, 40, 40),
        z_range: (0, 30),
        budget: 20_000,
    }
}

#[test]
fn a_flat_plain_is_crossed_in_a_straight_line() {
    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let path = nav.path(&plain(), (2, 2, 10), (8, 2, 10), &limits());
    assert_eq!(path.len(), 6, "six steps east: {path:?}");
    assert_eq!(path.last(), Some(&(8, 2, 10)));
    assert!(
        path.iter().all(|&(_, y, z)| y == 2 && z == 10),
        "the plain is flat; the path should not wander: {path:?}"
    );
}

#[test]
fn the_same_query_gives_the_same_path() {
    // Determinism is the whole contract: two peers plan independently and
    // must agree, or a unit walks two different routes on two screens.
    let world = plain();
    let mut a = NavVolume::new(MoverProfile::vehicle());
    let mut b = NavVolume::new(MoverProfile::vehicle());
    let (pa, pb) = (
        a.path(&world, (1, 1, 10), (20, 17, 10), &limits()),
        b.path(&world, (1, 1, 10), (20, 17, 10), &limits()),
    );
    assert_eq!(pa, pb);
    assert!(!pa.is_empty());
}

/// A wall that rises three cells per step: armour (climbs two) must not
/// cross it, infantry (climbs four) must. This is §4b's whole mountain
/// rule, with no obstacle markup anywhere.
fn ridge() -> World<impl Fn(i64, i64, i64) -> bool> {
    World(|x, _y, z| {
        let ground = if x == 10 {
            13 // one column, three cells proud of the plain
        } else {
            10
        };
        z <= ground
    })
}

#[test]
fn a_ridge_walls_out_armour_and_admits_infantry() {
    let world = ridge();
    let mut armour = NavVolume::new(MoverProfile::vehicle());
    let path = armour.path(&world, (5, 5, 10), (15, 5, 10), &limits());
    assert!(
        path.last() != Some(&(15, 5, 10)),
        "armour climbed a three-cell step: {path:?}"
    );

    let mut foot = NavVolume::new(MoverProfile::infantry());
    let path = foot.path(&world, (5, 5, 10), (15, 5, 10), &limits());
    assert_eq!(
        path.last(),
        Some(&(15, 5, 10)),
        "infantry should cross a three-cell step: {path:?}"
    );
    assert!(
        path.iter().any(|&(x, _, z)| x == 10 && z == 13),
        "…over the ridge itself, not around it: {path:?}"
    );
}

#[test]
fn an_unreachable_goal_yields_the_closest_approach() {
    // The RTS contract: a misclick walks the unit as far as it can rather
    // than refusing to move.
    let world = World(|x, _y, z| if x == 10 { z <= 25 } else { z <= 10 });
    let mut armour = NavVolume::new(MoverProfile::vehicle());
    let path = armour.path(&world, (5, 5, 10), (15, 5, 10), &limits());
    assert!(!path.is_empty(), "the unit should still set off");
    let (lx, _, _) = *path.last().expect("non-empty");
    assert!(lx < 10, "it should stop at the wall, not pass it: {path:?}");
}

/// A ridge with a bore through it: solid from x=8..12 up to z=20, with a
/// tunnel at z=11..13 running east–west. The floor of the bore is the
/// rock at z=10; above the bore is a roof, which is what makes those
/// stands enclosed.
fn bored_ridge() -> World<impl Fn(i64, i64, i64) -> bool> {
    World(|x, y, z| {
        let in_ridge = (8..=12).contains(&x);
        if in_ridge && y == 5 && (11..=13).contains(&z) {
            return false; // the bore
        }
        if in_ridge {
            return z <= 20;
        }
        z <= 10
    })
}

#[test]
fn a_bore_is_ordinary_ground_to_a_mover_that_can_use_it() {
    let world = bored_ridge();
    let mut foot = NavVolume::new(MoverProfile::infantry());
    let path = foot.path(&world, (5, 5, 10), (15, 5, 10), &limits());
    assert_eq!(
        path.last(),
        Some(&(15, 5, 10)),
        "infantry should walk through the bore: {path:?}"
    );
    assert!(
        path.iter().any(|&(x, _, z)| (8..=12).contains(&x) && z == 10),
        "…through it at the bore's floor, not over a 20-cell ridge: {path:?}"
    );
}

#[test]
fn a_surface_only_mover_cannot_see_the_bore() {
    // The same world, a mover that does not tunnel: the bore might as well
    // not exist. This is what keeps "armour cannot use Dweller tunnels" a
    // property of the profile rather than a rule someone has to remember.
    let world = bored_ridge();
    let mut surface = NavVolume::new(MoverProfile {
        tunnels: false,
        ..MoverProfile::infantry()
    });
    let path = surface.path(&world, (5, 5, 10), (15, 5, 10), &limits());
    assert!(
        path.last() != Some(&(15, 5, 10)),
        "a surface-only mover walked through a tunnel: {path:?}"
    );
}

#[test]
fn a_mover_too_tall_for_the_bore_stays_out() {
    // The bore is three cells of air; a four-cell mover does not fit, and
    // the stand simply is not there for it.
    let world = bored_ridge();
    let mut tall = NavVolume::new(MoverProfile {
        height: 4,
        max_step: 4,
        tunnels: true,
    });
    let path = tall.path(&world, (5, 5, 10), (15, 5, 10), &limits());
    assert!(
        path.last() != Some(&(15, 5, 10)),
        "a four-cell mover squeezed through a three-cell bore: {path:?}"
    );
}

#[test]
fn stands_report_a_roof_as_enclosure() {
    let world = bored_ridge();
    let mut nav = NavVolume::new(MoverProfile::infantry());
    let inside = nav.stands(&world, 10, 5, (0, 30)).to_vec();
    assert_eq!(inside.len(), 2, "the ridge top and the bore floor: {inside:?}");
    assert!(!inside[0].enclosed, "the ridge top is open sky");
    assert_eq!(inside[0].z, 20);
    assert!(inside[1].enclosed, "the bore floor is under a roof");
    assert_eq!(inside[1].z, 10);
}

#[test]
fn a_diagonal_cannot_cut_a_corner() {
    // A single blocking column at (6, 6): a mover at (5, 5) may not slide
    // diagonally past it if either flank is impassable. Here both flanks
    // are open, so the diagonal is legal; raise them and it is not.
    let open = World(|_x, _y, z| z <= 10);
    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let through = nav.path(&open, (5, 5, 10), (6, 6, 10), &limits());
    assert_eq!(through, vec![(6, 6, 10)], "an open diagonal is one step");

    // Now wall both flanks to 20 — far beyond the climb. The goal is
    // still *reachable*, by walking around; what must not happen is the
    // one-step diagonal squeeze between the two walls.
    let pinched = World(|x, y, z| {
        if (x, y) == (6, 5) || (x, y) == (5, 6) {
            return z <= 20;
        }
        z <= 10
    });
    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let path = nav.path(&pinched, (5, 5, 10), (6, 6, 10), &limits());
    assert_eq!(
        path.last(),
        Some(&(6, 6, 10)),
        "the goal is reachable the long way round: {path:?}"
    );
    assert_ne!(
        path.first(),
        Some(&(6, 6, 10)),
        "the mover squeezed diagonally between two walls: {path:?}"
    );
    assert!(
        path.len() > 3,
        "going around should cost more than cutting the corner: {path:?}"
    );
}

#[test]
fn a_budget_stops_the_search_with_a_partial_path() {
    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let tight = VolumeLimits {
        budget: 12,
        ..limits()
    };
    let path = nav.path(&plain(), (0, 0, 10), (40, 40, 10), &tight);
    assert!(!path.is_empty(), "a budgeted search still sets off");
    assert!(
        path.last() != Some(&(40, 40, 10)),
        "twelve nodes cannot reach the far corner"
    );
}

#[test]
fn invalidation_is_bounded_and_actually_re_derives() {
    // Terraforming edits terrain constantly (§4e), so the cache has to
    // drop exactly the columns that changed — and the next query has to
    // see the change.
    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let before = nav.path(&plain(), (2, 2, 10), (8, 2, 10), &limits());
    assert_eq!(before.last(), Some(&(8, 2, 10)));
    let cached = nav.cached_columns();
    assert!(cached > 0, "the search should have cached what it walked");

    nav.invalidate((5, 1), (5, 3));
    assert_eq!(
        nav.cached_columns(),
        cached - 3,
        "invalidation should drop three columns and no others"
    );

    // A wall where the invalidated columns were: the re-derived stands
    // must show it.
    let walled = World(|x, _y, z| if x == 5 { z <= 25 } else { z <= 10 });
    let after = nav.path(&walled, (2, 2, 10), (8, 2, 10), &limits());
    assert!(
        after.last() != Some(&(8, 2, 10)),
        "the wall raised in the invalidated columns was not seen: {after:?}"
    );
}
