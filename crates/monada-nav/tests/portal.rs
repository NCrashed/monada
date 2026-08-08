//! What the portal graph promises (docs/plans/desert-game.md §4c).
//!
//! A hierarchical path is allowed to be worse than optimal — that is the
//! trade it exists to make. It is NOT allowed to be unwalkable, to skip
//! cells, or to disagree with the flat search about whether a place can
//! be reached at all. Those three are what these tests hold.

use monada_nav::{MoverProfile, NavVolume, PortalGraph, VolumeLimits, VolumeWorld};

struct World<F: Fn(i64, i64, i64) -> bool>(F);

impl<F: Fn(i64, i64, i64) -> bool> VolumeWorld for World<F> {
    fn solid(&self, x: i64, y: i64, z: i64) -> bool {
        (self.0)(x, y, z)
    }
}

const SPAN: i64 = 128;

fn limits() -> VolumeLimits {
    VolumeLimits {
        bounds: (0, 0, SPAN - 1, SPAN - 1),
        z_range: (0, 30),
        budget: 200_000,
    }
}

/// Open sand with a long wall down the middle, gapped near one end — the
/// shape that makes a flat search expand everything: the goal is due
/// east, the only way there is far to the south.
fn walled() -> World<impl Fn(i64, i64, i64) -> bool> {
    World(|x, y, z| {
        if x == 64 && y > 12 {
            return z <= 25; // the wall, with a gap at y <= 12
        }
        z <= 10
    })
}

/// Every step of a path must be adjacent and within the mover's climb —
/// the property a hierarchy could quietly break by stitching two legs
/// that do not meet.
fn assert_walkable(path: &[(i64, i64, i64)], from: (i64, i64, i64), max_step: i64) {
    let mut prev = from;
    for &(x, y, z) in path {
        let (dx, dy) = ((x - prev.0).abs(), (y - prev.1).abs());
        assert!(
            dx <= 1 && dy <= 1 && dx + dy > 0,
            "waypoints must be adjacent: {prev:?} → {:?}",
            (x, y, z)
        );
        assert!(
            (z - prev.2).abs() <= max_step,
            "step {prev:?} → {:?} climbs {}",
            (x, y, z),
            (z - prev.2).abs()
        );
        prev = (x, y, z);
    }
}

#[test]
fn a_hierarchical_path_is_walkable_and_arrives() {
    let world = walled();
    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let mut portals = PortalGraph::new();
    let (from, to) = ((10, 60, 10), (120, 60, 10));
    let path = portals.path(&mut nav, &world, from, to, &limits());
    assert_eq!(path.last(), Some(&to), "the goal must be reached");
    assert_walkable(&path, from, MoverProfile::vehicle().max_step);
    assert!(
        path.iter().any(|&(x, y, _)| x == 64 && y <= 12),
        "the route has to use the gap: {} waypoints",
        path.len()
    );
}

#[test]
fn it_agrees_with_the_flat_search_about_reachability() {
    // Same world, same ends, both searches: either both arrive or neither
    // does. A hierarchy that quietly gives up is worse than a slow one.
    let world = walled();
    let (from, to) = ((10, 60, 10), (120, 60, 10));

    let mut flat_nav = NavVolume::new(MoverProfile::vehicle());
    let flat = flat_nav.path(&world, from, to, &limits());

    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let mut portals = PortalGraph::new();
    let coarse = portals.path(&mut nav, &world, from, to, &limits());

    assert_eq!(flat.last(), Some(&to));
    assert_eq!(coarse.last(), Some(&to));
    // Not optimal, but not absurd either: a detour twice the length of the
    // best route would mean the abstraction is choosing badly.
    assert!(
        coarse.len() <= flat.len() * 2,
        "hierarchical route {} vs optimal {}",
        coarse.len(),
        flat.len()
    );
}

#[test]
fn a_sealed_goal_is_still_refused() {
    // A goal walled off entirely: the portal graph must fall back to the
    // flat search's contract — walk as far as you can — rather than
    // inventing a way through.
    let world = World(|x, _y, z| if x == 64 { z <= 25 } else { z <= 10 });
    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let mut portals = PortalGraph::new();
    let (from, to) = ((10, 60, 10), (120, 60, 10));
    let path = portals.path(&mut nav, &world, from, to, &limits());
    assert!(path.last() != Some(&to), "there is no way through");
    if let Some(&(lx, _, _)) = path.last() {
        assert!(lx < 64, "and it should stop at the wall: {lx}");
    }
}

#[test]
fn the_same_query_gives_the_same_path() {
    let world = walled();
    let (from, to) = ((10, 60, 10), (120, 60, 10));
    let run = || {
        let mut nav = NavVolume::new(MoverProfile::vehicle());
        let mut portals = PortalGraph::new();
        portals.path(&mut nav, &world, from, to, &limits())
    };
    assert_eq!(run(), run());
}

#[test]
fn a_short_hop_inside_one_block_needs_no_hierarchy() {
    let world = walled();
    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let mut portals = PortalGraph::new();
    let path = portals.path(&mut nav, &world, (4, 4, 10), (9, 4, 10), &limits());
    assert_eq!(path.len(), 5, "five steps east: {path:?}");
    assert_eq!(
        portals.built_blocks(),
        0,
        "a hop inside one block should not build the coarse graph at all"
    );
}

#[test]
fn invalidation_drops_the_neighbours_too() {
    // A wall raised at a block's edge changes where its border can be
    // crossed, not only what is inside it — so the neighbours have to go.
    let world = walled();
    let mut nav = NavVolume::new(MoverProfile::vehicle());
    let mut portals = PortalGraph::new();
    let (from, to) = ((10, 60, 10), (120, 60, 10));
    portals.path(&mut nav, &world, from, to, &limits());
    let built = portals.built_blocks();
    assert!(built > 4, "a crossing should have built several blocks");

    portals.invalidate((64, 60), (64, 60));
    assert!(
        portals.built_blocks() < built,
        "the edited block and its neighbours should be gone"
    );
}
