//! Deterministic grid pathfinding (docs/plans/rts-demo.md §1a).
//!
//! An integer octile A* over an abstract heightfield + blocker overlay
//! ([`NavWorld`]) — the walk rule an RTS shares between its movement
//! collision and its pathfinding: a step between neighbouring cells is
//! passable when the height difference is at most `max_step` and neither
//! cell is explicitly blocked. Cliffs (a tall delta) wall off exactly the
//! cells a mover couldn't climb; ramps (a run of small deltas) route
//! automatically.
//!
//! Determinism is by construction, not by luck:
//! - all-integer costs (octile 10/14) and heights — no floats;
//! - a fixed neighbour visit order;
//! - the open heap breaks f-cost ties by a monotone insertion counter;
//! - `BTreeMap` throughout (the workspace's determinism stance: no hash
//!   maps near the sim, even lookup-only ones).
//!
//! The search is budgeted: on exhaustion (or an unreachable goal) it
//! returns the best-effort path toward the cell that got *closest* to the
//! goal — the WC3 behaviour (the unit walks as far as it can), and the
//! reason a runaway search can't stall a sim tick.

use std::collections::{BTreeMap, BinaryHeap};

/// The world a path is planned against: a heightfield + explicit blockers.
/// Implementations must be pure functions of their state — the same cell
/// always answers the same — or determinism is forfeit.
pub trait NavWorld {
    /// The walkable surface height of cell `(x, y)`.
    fn height(&self, x: i64, y: i64) -> i64;
    /// Whether cell `(x, y)` is explicitly impassable (building footprint,
    /// prop) regardless of height.
    fn blocked(&self, x: i64, y: i64) -> bool;
}

/// Search limits: the walk rule's step tolerance, the inclusive cell
/// bounds the search may explore, and the node budget.
#[derive(Clone, Copy, Debug)]
pub struct NavLimits {
    /// Maximum |height delta| a single step may climb or drop.
    pub max_step: i64,
    /// Inclusive search bounds `(x0, y0, x1, y1)` — typically the painted
    /// world's bounding box. Keeps the search off an infinite plain.
    pub bounds: (i64, i64, i64, i64),
    /// Maximum nodes to pop before giving up with a partial path.
    pub budget: usize,
}

/// Fixed neighbour order (E, N, W, S, NE, NW, SW, SE): orthogonals first,
/// then diagonals — part of the determinism contract, do not reorder.
const NEIGHBOURS: [(i64, i64, i64); 8] = [
    (1, 0, 10),
    (0, 1, 10),
    (-1, 0, 10),
    (0, -1, 10),
    (1, 1, 14),
    (-1, 1, 14),
    (-1, -1, 14),
    (1, -1, 14),
];

/// Octile distance ×10 — admissible and consistent for the 10/14 costs.
fn octile(ax: i64, ay: i64, bx: i64, by: i64) -> i64 {
    let dx = (ax - bx).abs();
    let dy = (ay - by).abs();
    let (lo, hi) = if dx < dy { (dx, dy) } else { (dy, dx) };
    14 * lo + 10 * (hi - lo)
}

/// A deterministic best-first path from `from` to `to` under `limits`.
///
/// Returns the waypoint cells from the first step *after* `from` up to and
/// including the goal — empty when already there (or nothing at all is
/// reachable). If the goal can't be reached (blocked, cliffed off, or the
/// budget runs dry), the path leads to the reachable cell closest to the
/// goal (octile), tie-broken toward the cheapest to reach — never an error.
///
/// The walk rule per step: the target cell is in bounds, not blocked, and
/// |Δheight| ≤ `max_step`. A diagonal additionally requires BOTH flanking
/// orthogonal cells to pass the same rule (no cutting a corner past a
/// cliff edge or a blocker). `from` itself is exempt from the blocked
/// check, so a unit standing where a footprint just landed can still walk
/// out.
pub fn astar(world: &impl NavWorld, from: (i64, i64), to: (i64, i64), limits: &NavLimits) -> Vec<(i64, i64)> {
    if from == to {
        return Vec::new();
    }
    let (bx0, by0, bx1, by1) = limits.bounds;
    let in_bounds = |x: i64, y: i64| x >= bx0 && x <= bx1 && y >= by0 && y <= by1;
    if !in_bounds(from.0, from.1) {
        return Vec::new();
    }

    // A step from height `hz` onto `(x, y)` under the walk rule.
    let passable = |hz: i64, x: i64, y: i64| {
        in_bounds(x, y) && !world.blocked(x, y) && (world.height(x, y) - hz).abs() <= limits.max_step
    };

    // g-cost + parent per visited cell.
    let mut best_g: BTreeMap<(i64, i64), i64> = BTreeMap::new();
    let mut parent: BTreeMap<(i64, i64), (i64, i64)> = BTreeMap::new();
    // Max-heap of Reverse-ordered (f, seq): the monotone `seq` makes every
    // f-tie break toward the earliest push — full ordering, zero luck.
    let mut open: BinaryHeap<std::cmp::Reverse<(i64, u64, i64, i64)>> = BinaryHeap::new();
    let mut seq: u64 = 0;

    best_g.insert(from, 0);
    open.push(std::cmp::Reverse((octile(from.0, from.1, to.0, to.1), seq, from.0, from.1)));

    // The fallback target: closest to the goal (octile), then cheapest.
    let mut best_cell = from;
    let mut best_key = (octile(from.0, from.1, to.0, to.1), 0_i64);

    let mut popped = 0_usize;
    while let Some(std::cmp::Reverse((f, _, cx, cy))) = open.pop() {
        let cur = (cx, cy);
        let g = best_g[&cur];
        let h = octile(cx, cy, to.0, to.1);
        // A stale heap entry (superseded by a cheaper push for the same
        // cell) — skip without charging the budget.
        if f > g + h {
            continue;
        }
        if (h, g) < best_key {
            best_key = (h, g);
            best_cell = cur;
        }
        if cur == to {
            return unwind(&parent, from, to);
        }
        popped += 1;
        if popped >= limits.budget {
            break;
        }

        let hz = world.height(cx, cy);
        for &(dx, dy, cost) in &NEIGHBOURS {
            let (nx, ny) = (cx + dx, cy + dy);
            if !passable(hz, nx, ny) {
                continue;
            }
            // No corner cutting: a diagonal needs both flanks open too.
            if dx != 0 && dy != 0 && !(passable(hz, cx + dx, cy) && passable(hz, cx, cy + dy)) {
                continue;
            }
            let ng = g + cost;
            if best_g.get(&(nx, ny)).map_or(true, |&old| ng < old) {
                best_g.insert((nx, ny), ng);
                parent.insert((nx, ny), cur);
                seq += 1;
                open.push(std::cmp::Reverse((ng + octile(nx, ny, to.0, to.1), seq, nx, ny)));
            }
        }
    }

    unwind(&parent, from, best_cell)
}

/// Rebuild the waypoint list `from → goal` (exclusive of `from`).
fn unwind(parent: &BTreeMap<(i64, i64), (i64, i64)>, from: (i64, i64), goal: (i64, i64)) -> Vec<(i64, i64)> {
    let mut path = Vec::new();
    let mut cur = goal;
    while cur != from {
        path.push(cur);
        cur = parent[&cur];
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    // Tiny ASCII fixtures: index/coordinate casts are exact here.
    #![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation, clippy::cast_sign_loss)]

    use super::*;

    /// A test world from an ASCII map: digits are heights, `#` is a
    /// blocker (height 0). Row 0 is y = 0.
    struct Ascii {
        rows: Vec<Vec<u8>>,
    }

    impl Ascii {
        fn new(map: &[&str]) -> Ascii {
            Ascii {
                rows: map.iter().map(|r| r.bytes().collect()).collect(),
            }
        }
        fn bounds(&self) -> (i64, i64, i64, i64) {
            (0, 0, self.rows[0].len() as i64 - 1, self.rows.len() as i64 - 1)
        }
        fn limits(&self, max_step: i64) -> NavLimits {
            NavLimits {
                max_step,
                bounds: self.bounds(),
                budget: 10_000,
            }
        }
    }

    impl NavWorld for Ascii {
        fn height(&self, x: i64, y: i64) -> i64 {
            match self.rows.get(y as usize).and_then(|r| r.get(x as usize)) {
                Some(c) if c.is_ascii_digit() => i64::from(c - b'0'),
                _ => 0,
            }
        }
        fn blocked(&self, x: i64, y: i64) -> bool {
            self.rows
                .get(y as usize)
                .and_then(|r| r.get(x as usize))
                .is_some_and(|&c| c == b'#')
        }
    }

    fn steps_are_adjacent_and_legal(w: &Ascii, from: (i64, i64), path: &[(i64, i64)], max_step: i64) {
        let mut prev = from;
        for &wp in path {
            let (dx, dy) = ((wp.0 - prev.0).abs(), (wp.1 - prev.1).abs());
            assert!(dx <= 1 && dy <= 1 && (dx, dy) != (0, 0), "adjacent step {prev:?}→{wp:?}");
            assert!(
                (w.height(wp.0, wp.1) - w.height(prev.0, prev.1)).abs() <= max_step,
                "legal climb {prev:?}→{wp:?}"
            );
            assert!(!w.blocked(wp.0, wp.1), "no blocked cell on path: {wp:?}");
            prev = wp;
        }
    }

    #[test]
    fn straight_line_on_flat_ground() {
        let w = Ascii::new(&["0000", "0000", "0000"]);
        let path = astar(&w, (0, 0), (3, 0), &w.limits(1));
        assert_eq!(path, vec![(1, 0), (2, 0), (3, 0)]);
    }

    #[test]
    fn already_there_is_empty() {
        let w = Ascii::new(&["00"]);
        assert!(astar(&w, (0, 0), (0, 0), &w.limits(1)).is_empty());
    }

    #[test]
    fn detours_around_a_blocker_wall() {
        let w = Ascii::new(&[
            "00000", //
            "0###0", //
            "00000",
        ]);
        let path = astar(&w, (0, 1), (4, 1), &w.limits(1));
        assert_eq!(path.last(), Some(&(4, 1)));
        steps_are_adjacent_and_legal(&w, (0, 1), &path, 1);
    }

    #[test]
    fn cliffs_wall_and_ramps_route() {
        // A height-5 plateau on the right; the only legal way up is the
        // ramp row at y = 2 (0-1-2-3-4-5). A direct line must be refused
        // in favour of the ramp.
        let w = Ascii::new(&[
            "000555", //
            "000555", //
            "012345", //
            "000555",
        ]);
        let path = astar(&w, (0, 0), (5, 0), &w.limits(1));
        assert_eq!(path.last(), Some(&(5, 0)), "reached the plateau");
        assert!(path.contains(&(2, 2)), "took the ramp: {path:?}");
        steps_are_adjacent_and_legal(&w, (0, 0), &path, 1);
    }

    #[test]
    fn no_corner_cutting_past_a_blocker() {
        // Diagonal (0,0)→(1,1) with a blocker at (1,0) must not be taken
        // directly: the flank is closed, so the path bends through (0, 1).
        let w = Ascii::new(&[
            "0#0", //
            "000",
        ]);
        let path = astar(&w, (0, 0), (2, 0), &w.limits(1));
        assert_eq!(path.last(), Some(&(2, 0)));
        assert!(
            path.first() == Some(&(0, 1)) || path.first() == Some(&(1, 1)),
            "went around, not through the corner: {path:?}"
        );
        steps_are_adjacent_and_legal(&w, (0, 0), &path, 1);
    }

    #[test]
    fn unreachable_goal_gets_closest_approach() {
        // The goal is sealed inside a blocker ring: the path must end at a
        // reachable cell adjacent to the ring (closest octile approach).
        let w = Ascii::new(&[
            "00000", //
            "0###0", //
            "0#0#0", //
            "0###0", //
            "00000",
        ]);
        let path = astar(&w, (0, 0), (2, 2), &w.limits(1));
        let end = *path.last().expect("walks as close as it can");
        assert_ne!(end, (2, 2), "sealed goal not reached");
        assert_eq!(octile(end.0, end.1, 2, 2), 20, "parked ring-adjacent");
        steps_are_adjacent_and_legal(&w, (0, 0), &path, 1);
    }

    #[test]
    fn budget_exhaustion_yields_partial_progress() {
        let w = Ascii::new(&["0000000000", "0000000000", "0000000000"]);
        let tight = NavLimits {
            max_step: 1,
            bounds: w.bounds(),
            budget: 3,
        };
        let path = astar(&w, (0, 1), (9, 1), &tight);
        assert!(!path.is_empty(), "made some progress");
        let end = path.last().unwrap();
        assert!(end.0 < 9, "budget stopped short of the goal");
        steps_are_adjacent_and_legal(&w, (0, 1), &path, 1);
    }

    #[test]
    fn blocked_start_can_walk_out() {
        // A unit standing where a footprint just landed: the start cell is
        // blocked, but the path still leads out of it.
        let w = Ascii::new(&["#000"]);
        let path = astar(&w, (0, 0), (3, 0), &w.limits(1));
        assert_eq!(path, vec![(1, 0), (2, 0), (3, 0)]);
    }

    #[test]
    fn deterministic_across_runs() {
        let w = Ascii::new(&[
            "000000", //
            "0#00#0", //
            "000000", //
            "0#00#0", //
            "000000",
        ]);
        let a = astar(&w, (0, 0), (5, 4), &w.limits(1));
        let b = astar(&w, (0, 0), (5, 4), &w.limits(1));
        assert_eq!(a, b, "same query, same path — always");
        assert_eq!(a.last(), Some(&(5, 4)));
    }
}
