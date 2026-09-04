//! Hierarchical navigation over the heightfield: the flat twin of
//! [`crate::portal`].
//!
//! **The same problem and the same answer, in two dimensions.** A flat
//! octile A\* is fine until a long barrier stands between a walker and its
//! goal; then the heuristic points straight at the goal, through the
//! barrier, and the search expands nearly everything on the near side
//! before it rounds the end. Measured on a field with one wall across it
//! and a gap at the far end: 4.6 ms at sixty-four cells, 19 ms at a
//! hundred and twenty-eight, and past two hundred and fifty-six the
//! budget runs out and the walker gives up halfway.
//!
//! So: cut the map into blocks, find where a walker can cross each block
//! border, pay for the expensive search once per block instead of once
//! per query, and plan over that small graph.
//!
//! # Why this is not [`crate::portal`] with a different node
//!
//! That one plans over the volume world's stand graph -- a column can be
//! stood on at several heights, and a tunnel is a place. This one plans
//! over a heightfield, where a column has one surface. The node is a cell
//! rather than a cell and a height, the concrete search is [`astar`]
//! rather than a stand search, and there is one thing the volume version
//! does not have to say at all:
//!
//! **Every edge here is directed.** [`NavLimits`] tells climbing from
//! dropping, so a walker may come off a ledge it cannot come back up.
//! A border can therefore be crossable one way only, and so can a route
//! between two portals of the same block -- which means the abstract
//! graph is directed, and so is every cost in it. An undirected version
//! would either forbid one-way ground (losing the ledge) or promise
//! routes back up it (worse: a walker that sets off and never arrives).
//!
//! # What is kept and what is given up
//!
//! Kept: determinism -- blocks, portals and edges are built in a fixed
//! order with `BTreeMap` throughout -- and the walk rule, since every
//! refined step comes out of the same [`astar`] the flat search uses, so
//! a path a walker cannot walk cannot be produced. Given up: optimality.
//! A hierarchical path is a good path, not the shortest one.

use std::collections::{BTreeMap, BinaryHeap};

use crate::{astar, NavLimits, NavWorld};

/// Cells per block edge. Sixteen, as [`crate::portal::BLOCK`] is, and for
/// the same reason: a block of two hundred and fifty-six cells searches
/// in a fraction of a millisecond, and a map of any size is then hundreds
/// of blocks rather than thousands.
pub const BLOCK: i64 = 16;

/// How many nodes the direct search may spend before the hierarchy takes
/// over.
///
/// **The hierarchy is not free** -- the blocks along a route cost several
/// milliseconds to build the first time -- so the flat answer is tried
/// cheaply first, and blocks are paid for only when the terrain proves it
/// needs them. Which is also the honest description of when a hierarchy
/// helps at all: an open field never touches this module.
pub const SCOUT_BUDGET: usize = 2_000;

/// A node in the concrete graph: a cell.
type Cell = (i64, i64);

/// Two adjacent blocks in a fixed order, so a border is stored once
/// however it is approached.
type BorderKey = ((i64, i64), (i64, i64));

/// A block belongs to one walk rule: what a ledge is depends on who is
/// walking off it. `(max_step, max_drop, bx, by)`.
type BlockKey = (i64, i64, i64, i64);

/// Which block a cell belongs to.
fn block_of(x: i64, y: i64) -> (i64, i64) {
    (x.div_euclid(BLOCK), y.div_euclid(BLOCK))
}

/// The inclusive cell bounds of a block.
fn block_bounds(bx: i64, by: i64) -> (i64, i64, i64, i64) {
    (
        bx * BLOCK,
        by * BLOCK,
        bx * BLOCK + BLOCK - 1,
        by * BLOCK + BLOCK - 1,
    )
}

/// One block's portals and what it costs to walk between them.
#[derive(Clone, Debug, Default)]
struct Block {
    /// Portal cells inside this block, ascending.
    portals: Vec<Cell>,
    /// `(i, j) -> cost` of walking from portal `i` to portal `j` without
    /// leaving the block. **Ordered**: a ledge inside a block is one-way
    /// like any other, so `(i, j)` and `(j, i)` are different questions.
    intra: BTreeMap<(usize, usize), i64>,
}

/// Where a border may be crossed, and which way.
#[derive(Clone, Copy, Debug)]
struct Crossing {
    near: Cell,
    far: Cell,
    /// Whether the step is allowed each way. At least one is true or the
    /// crossing is not recorded at all.
    out: bool,
    back: bool,
}

/// The coarse graph: blocks built on demand, and the borders between
/// them.
///
/// A cache and nothing else. Every answer is a function of the terrain
/// and the query -- a block that happens to be built already changes how
/// long an answer takes, never what it is -- which is what lets two peers
/// hold different halves of this and stay in step.
#[derive(Clone, Debug, Default)]
pub struct FieldGraph {
    blocks: BTreeMap<BlockKey, Block>,
    borders: BTreeMap<(i64, i64, BorderKey), Vec<Crossing>>,
}

impl FieldGraph {
    #[must_use]
    pub fn new() -> FieldGraph {
        FieldGraph::default()
    }

    /// Forget every block and border the inclusive cell box touches.
    ///
    /// **Whoever changes the ground must call this.** A stale portal is
    /// not a visible bug -- it is a walker setting off confidently through
    /// a wall that was raised two seconds ago.
    pub fn invalidate(&mut self, lo: (i64, i64), hi: (i64, i64)) {
        let (b0, b1) = (block_of(lo.0, lo.1), block_of(hi.0, hi.1));
        // A border is between two blocks and either of them may have
        // moved, so a block's neighbours go with it.
        let touched = |b: (i64, i64)| {
            b.0 >= b0.0 - 1 && b.0 <= b1.0 + 1 && b.1 >= b0.1 - 1 && b.1 <= b1.1 + 1
        };
        self.blocks
            .retain(|&(_, _, bx, by), _| !touched((bx, by)));
        self.borders
            .retain(|&(_, _, (a, b)), _| !touched(a) && !touched(b));
    }

    /// How many blocks are built. A diagnostic: what this module costs is
    /// what it has had to build, and a test that could not see that would
    /// be asserting on a black box.
    #[must_use]
    pub fn built_blocks(&self) -> usize {
        self.blocks.len()
    }

    /// A path from `from` to `to`, planned over blocks and refined into
    /// cells.
    ///
    /// Falls back to the concrete search when the two ends share a block
    /// (there is no hierarchy to exploit over sixteen cells), when the
    /// flat search answers inside its scouting budget, and when the
    /// abstract graph has no route -- a genuinely unreachable goal, where
    /// the flat search's closest-approach contract is what a player
    /// expects.
    pub fn path(
        &mut self,
        world: &impl NavWorld,
        from: Cell,
        to: Cell,
        limits: &NavLimits,
    ) -> Vec<Cell> {
        if from == to {
            return Vec::new();
        }
        let (fb, tb) = (block_of(from.0, from.1), block_of(to.0, to.1));
        if fb == tb {
            return astar(world, from, to, limits);
        }

        let scout = astar(
            world,
            from,
            to,
            &NavLimits {
                budget: limits.budget.min(SCOUT_BUDGET),
                ..*limits
            },
        );
        if scout.last() == Some(&to) {
            return scout;
        }

        self.build_block(world, fb, limits);
        self.build_block(world, tb, limits);
        let start = self.reach_out(world, fb, from, limits);
        let goal = self.reach_in(world, tb, to, limits);
        if start.is_empty() || goal.is_empty() {
            return astar(world, from, to, limits);
        }
        let Some(hops) = self.abstract_route(world, from, to, &start, &goal, limits) else {
            return astar(world, from, to, limits);
        };
        refine(world, &hops, limits)
    }

    /// A\* over portal cells, by the same octile distance the concrete
    /// search uses.
    ///
    /// **The volume twin runs this uninformed, and it is right to: its
    /// graph is small and a heuristic would buy nothing.** Here what a
    /// visit costs is not the visit -- it is that reaching an unbuilt
    /// block BUILDS it, portals, intra-costs and all. Uninformed, the
    /// search spreads evenly and pays for nearly every block on the map:
    /// three hundred milliseconds on a five-hundred-cell field, against
    /// eleven once they are built. Pointed at the goal it pays for the
    /// corridor it actually walks.
    ///
    /// Admissible, so the answer does not change: the octile distance
    /// undercounts every real route, portal costs are real path costs and
    /// a border step is one orthogonal step.
    fn abstract_route(
        &mut self,
        world: &impl NavWorld,
        from: Cell,
        to: Cell,
        start: &[(Cell, i64)],
        goal: &[(Cell, i64)],
        limits: &NavLimits,
    ) -> Option<Vec<Cell>> {
        let tail: BTreeMap<Cell, i64> = goal.iter().copied().collect();
        let mut dist: BTreeMap<Cell, i64> = BTreeMap::new();
        let mut parent: BTreeMap<Cell, Cell> = BTreeMap::new();
        // : the cell breaks every tie, so the order is total
        // and owes nothing to how the heap happened to fill.
        let mut open: BinaryHeap<std::cmp::Reverse<(i64, Cell)>> = BinaryHeap::new();
        let ahead = |c: Cell| crate::octile(c.0, c.1, to.0, to.1);

        for &(node, cost) in start {
            if dist.get(&node).map_or(true, |&d| cost < d) {
                dist.insert(node, cost);
                parent.insert(node, from);
                open.push(std::cmp::Reverse((cost + ahead(node), node)));
            }
        }

        let mut best: Option<(i64, Cell)> = None;
        while let Some(std::cmp::Reverse((f, node))) = open.pop() {
            let Some(&d) = dist.get(&node) else { continue };
            if f > d + ahead(node) {
                continue; // stale entry, superseded by a cheaper push
            }
            if let Some(&rest) = tail.get(&node) {
                let total = d + rest;
                if best.map_or(true, |(b, _)| total < b) {
                    best = Some((total, node));
                }
            }
            if best.is_some_and(|(b, _)| f >= b) {
                break; // nothing left can beat the route in hand
            }
            for (next, step) in self.neighbours(world, node, limits) {
                let nd = d + step;
                if dist.get(&next).map_or(true, |&old| nd < old) {
                    dist.insert(next, nd);
                    parent.insert(next, node);
                    open.push(std::cmp::Reverse((nd + ahead(next), next)));
                }
            }
        }

        let (_, last) = best?;
        let mut hops = vec![to];
        let mut cur = last;
        while cur != from {
            hops.push(cur);
            cur = *parent.get(&cur)?;
        }
        hops.push(from);
        hops.reverse();
        Some(hops)
    }

    /// A portal's neighbours: the twin across its border, and every other
    /// portal of its own block it can reach without leaving. Both
    /// directed -- see the module docs.
    fn neighbours(
        &mut self,
        world: &impl NavWorld,
        node: Cell,
        limits: &NavLimits,
    ) -> Vec<(Cell, i64)> {
        let b = block_of(node.0, node.1);
        self.build_block(world, b, limits);
        let mut out = Vec::new();
        if let Some(block) = self.blocks.get(&key(limits, b)) {
            if let Some(i) = block.portals.iter().position(|&p| p == node) {
                for (j, &other) in block.portals.iter().enumerate() {
                    if let Some(&cost) = block.intra.get(&(i, j)) {
                        out.push((other, cost));
                    }
                }
            }
        }
        // Crossing a border is one step, and only the way it is allowed.
        //
        // Its own block's four borders, not every border on the map: a
        // node can only sit on one of those, and the block was just built,
        // so all four are there. Scanning the lot was how the volume twin
        // wrote it and it is quadratic in a map's blocks -- invisible on
        // the desert, a third of a second on a five-hundred-cell field.
        for other in [
            (b.0 + 1, b.1),
            (b.0 - 1, b.1),
            (b.0, b.1 + 1),
            (b.0, b.1 - 1),
        ] {
            let at = (limits.max_step, limits.max_drop, border_key(b, other));
            let Some(crossings) = self.borders.get(&at) else {
                continue;
            };
            for c in crossings {
                if c.near == node && c.out {
                    out.push((c.far, 10));
                } else if c.far == node && c.back {
                    out.push((c.near, 10));
                }
            }
        }
        out
    }

    /// What it costs to get from a free-standing cell to each portal of
    /// its block…
    fn reach_out(
        &mut self,
        world: &impl NavWorld,
        b: (i64, i64),
        node: Cell,
        limits: &NavLimits,
    ) -> Vec<(Cell, i64)> {
        self.legs(world, b, node, limits, true)
    }

    /// …and from each portal to it, which is a different question on
    /// one-way ground.
    fn reach_in(
        &mut self,
        world: &impl NavWorld,
        b: (i64, i64),
        node: Cell,
        limits: &NavLimits,
    ) -> Vec<(Cell, i64)> {
        self.legs(world, b, node, limits, false)
    }

    fn legs(
        &mut self,
        world: &impl NavWorld,
        b: (i64, i64),
        node: Cell,
        limits: &NavLimits,
        outward: bool,
    ) -> Vec<(Cell, i64)> {
        let portals = self
            .blocks
            .get(&key(limits, b))
            .map(|x| x.portals.clone())
            .unwrap_or_default();
        let inner = NavLimits {
            bounds: block_bounds(b.0, b.1),
            ..*limits
        };
        portals
            .into_iter()
            .filter_map(|p| {
                let (a, z) = if outward { (node, p) } else { (p, node) };
                let path = astar(world, a, z, &inner);
                (path.last() == Some(&z)).then(|| (p, cost_of(&path)))
            })
            .collect()
    }

    /// Build a block's portals and intra-block costs, if it is not built.
    fn build_block(&mut self, world: &impl NavWorld, b: (i64, i64), limits: &NavLimits) {
        if self.blocks.contains_key(&key(limits, b)) {
            return;
        }
        // Borders first: a block's portals ARE its share of the four
        // borders around it.
        let mut portals: Vec<Cell> = Vec::new();
        for (other, axis) in [
            ((b.0 + 1, b.1), Axis::X),
            ((b.0 - 1, b.1), Axis::X),
            ((b.0, b.1 + 1), Axis::Y),
            ((b.0, b.1 - 1), Axis::Y),
        ] {
            let border = border_key(b, other);
            let at = (limits.max_step, limits.max_drop, border);
            if let std::collections::btree_map::Entry::Vacant(slot) = self.borders.entry(at) {
                slot.insert(find_portals(world, border.0, border.1, axis, limits));
            }
            for c in &self.borders[&at] {
                for node in [c.near, c.far] {
                    if block_of(node.0, node.1) == b && !portals.contains(&node) {
                        portals.push(node);
                    }
                }
            }
        }
        portals.sort_unstable();

        let inner = NavLimits {
            bounds: block_bounds(b.0, b.1),
            ..*limits
        };
        let mut intra = BTreeMap::new();
        for i in 0..portals.len() {
            for j in 0..portals.len() {
                if i == j {
                    continue;
                }
                let path = astar(world, portals[i], portals[j], &inner);
                if path.last() == Some(&portals[j]) {
                    intra.insert((i, j), cost_of(&path));
                }
            }
        }
        self.blocks.insert(key(limits, b), Block { portals, intra });
    }
}

/// Turn a sequence of portal hops into cells, one concrete search per
/// hop. Every waypoint therefore comes out of the same walk rule the flat
/// search applies -- the hierarchy chooses *which* corridors to use,
/// never what a step is allowed to be.
fn refine(world: &impl NavWorld, hops: &[Cell], limits: &NavLimits) -> Vec<Cell> {
    let mut out = Vec::new();
    for pair in hops.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let inner = if block_of(a.0, a.1) == block_of(b.0, b.1) {
            let block = block_of(a.0, a.1);
            NavLimits {
                bounds: block_bounds(block.0, block.1),
                ..*limits
            }
        } else {
            // A border crossing: one step, but let the search confirm it
            // rather than asserting it.
            NavLimits {
                bounds: span_bounds(a, b),
                ..*limits
            }
        };
        let leg = astar(world, a, b, &inner);
        if leg.last() != Some(&b) {
            // The ground moved under the cache, or a refinement will not
            // go where the coarse graph promised. Hand back what walks:
            // the same closest-approach contract the flat search has.
            return out;
        }
        out.extend(leg);
    }
    out
}

/// Which way a border runs.
#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
}

/// A block's cache key under one walk rule.
fn key(limits: &NavLimits, b: (i64, i64)) -> BlockKey {
    (limits.max_step, limits.max_drop, b.0, b.1)
}

/// Borders are keyed by their two blocks in a fixed order, so the pair is
/// stored once however it is approached.
fn border_key(a: (i64, i64), b: (i64, i64)) -> BorderKey {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// Where a walker can cross the border between two blocks: one portal per
/// contiguous run of crossable cells, placed at the run's middle.
///
/// One per run rather than one per cell is the whole economy of the
/// scheme -- a wide-open border becomes a single node instead of sixteen
/// -- and the middle rather than an end because a corner crossing tends
/// to be the one a corner-cut ban then refuses.
///
/// A run breaks where the crossing is refused **or where its direction
/// changes**: a stretch of border you may only drop down is a different
/// gate from the stretch beside it you may walk both ways, and one portal
/// standing for both would promise the wrong one half the time.
fn find_portals(
    world: &impl NavWorld,
    a: (i64, i64),
    b: (i64, i64),
    axis: Axis,
    limits: &NavLimits,
) -> Vec<Crossing> {
    let (bx0, by0, bx1, by1) = limits.bounds;
    let in_bounds = |x: i64, y: i64| x >= bx0 && x <= bx1 && y >= by0 && y <= by1;
    let mut run: Vec<Crossing> = Vec::new();
    let mut out: Vec<Crossing> = Vec::new();
    let mut flush = |run: &mut Vec<Crossing>| {
        if !run.is_empty() {
            out.push(run[run.len() / 2]);
            run.clear();
        }
    };

    // The two adjacent cell lines that face each other across the border.
    let span: Vec<(Cell, Cell)> = match axis {
        Axis::X => {
            let (left, right) = if a.0 < b.0 { (a, b) } else { (b, a) };
            let x = left.0 * BLOCK + BLOCK - 1;
            (left.1 * BLOCK..left.1 * BLOCK + BLOCK)
                .map(|y| ((x, y), (right.0 * BLOCK, y)))
                .collect()
        }
        Axis::Y => {
            let (lower, upper) = if a.1 < b.1 { (a, b) } else { (b, a) };
            let y = lower.1 * BLOCK + BLOCK - 1;
            (lower.0 * BLOCK..lower.0 * BLOCK + BLOCK)
                .map(|x| ((x, y), (x, upper.1 * BLOCK)))
                .collect()
        }
    };

    for (near, far) in span {
        if !in_bounds(near.0, near.1) || !in_bounds(far.0, far.1) {
            flush(&mut run);
            continue;
        }
        // Asked of the same search the walker will use, so a portal
        // cannot promise a step the walk rule forbids.
        let step = |from: Cell, to: Cell| {
            let one = NavLimits {
                bounds: span_bounds(from, to),
                budget: 8,
                ..*limits
            };
            astar(world, from, to, &one).last() == Some(&to)
        };
        let cross = Crossing {
            near,
            far,
            out: step(near, far),
            back: step(far, near),
        };
        if !cross.out && !cross.back {
            flush(&mut run);
            continue;
        }
        if run
            .last()
            .is_some_and(|p| (p.out, p.back) != (cross.out, cross.back))
        {
            flush(&mut run);
        }
        run.push(cross);
    }
    flush(&mut run);
    out
}

/// The bounding box of two cells, one wider all round so a border step
/// has room to be confirmed.
fn span_bounds(a: Cell, b: Cell) -> (i64, i64, i64, i64) {
    (
        a.0.min(b.0) - 1,
        a.1.min(b.1) - 1,
        a.0.max(b.0) + 1,
        a.1.max(b.1) + 1,
    )
}

/// The octile cost of a concrete path: 10 per orthogonal step, 14 per
/// diagonal -- the same currency the flat search spends.
fn cost_of(path: &[Cell]) -> i64 {
    let mut cost = 0;
    for pair in path.windows(2) {
        let (dx, dy) = ((pair[1].0 - pair[0].0).abs(), (pair[1].1 - pair[0].1).abs());
        cost += if dx + dy == 2 { 14 } else { 10 };
    }
    cost
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{FieldGraph, BLOCK};
    use crate::{astar, NavLimits, NavWorld};

    /// A field with a wall down the middle and a gap at the far end --
    /// the shape a flat search is worst at.
    struct Walled {
        side: i64,
        wall: i64,
        gap: i64,
        /// Cells raised into a one-way ledge, if the test wants one.
        shelf: BTreeSet<(i64, i64)>,
    }

    impl NavWorld for Walled {
        fn height(&self, x: i64, y: i64) -> i64 {
            if x == self.wall && y < self.side - self.gap {
                40
            } else if self.shelf.contains(&(x, y)) {
                3
            } else {
                0
            }
        }
        fn blocked(&self, _x: i64, _y: i64) -> bool {
            false
        }
    }

    fn limits(side: i64, budget: usize) -> NavLimits {
        NavLimits {
            max_step: 1,
            max_drop: 1,
            bounds: (0, 0, side - 1, side - 1),
            budget,
        }
    }

    fn walled(side: i64) -> Walled {
        Walled {
            side,
            wall: side / 2,
            gap: 4,
            shelf: BTreeSet::new(),
        }
    }

    /// **The case the module exists for.** The flat search gives up on a
    /// map this size; the hierarchy arrives.
    #[test]
    fn it_rounds_a_wall_the_flat_search_gives_up_on() {
        let side = 256;
        let w = walled(side);
        let lim = limits(side, 20_000);
        let goal = (side - 2, 1);

        let flat = astar(&w, (1, 1), goal, &lim);
        assert_ne!(flat.last(), Some(&goal), "the flat search suddenly arrives");

        let mut graph = FieldGraph::new();
        let route = graph.path(&w, (1, 1), goal, &lim);
        assert_eq!(route.last(), Some(&goal), "the hierarchy did not arrive");
        walkable(&w, (1, 1), &route, &lim);
    }

    /// …and it does not reach for the hierarchy when it does not have to:
    /// an open field is answered by the scout, with no block built.
    #[test]
    fn open_ground_never_builds_a_block() {
        let w = Walled {
            side: 128,
            wall: -1,
            gap: 0,
            shelf: BTreeSet::new(),
        };
        let lim = limits(128, 20_000);
        let mut graph = FieldGraph::new();
        let route = graph.path(&w, (1, 1), (126, 126), &lim);
        assert_eq!(route.last(), Some(&(126, 126)));
        assert_eq!(graph.built_blocks(), 0, "it paid for blocks it did not need");
    }

    /// **Every step of a coarse route is a step the walk rule allows.**
    /// The hierarchy picks corridors; it may never invent a stride.
    fn walkable(w: &Walled, from: (i64, i64), route: &[(i64, i64)], lim: &NavLimits) {
        let mut at = from;
        for &next in route {
            let (dx, dy) = ((next.0 - at.0).abs(), (next.1 - at.1).abs());
            assert!(dx <= 1 && dy <= 1 && dx + dy > 0, "a jump from {at:?} to {next:?}");
            let rise = w.height(next.0, next.1) - w.height(at.0, at.1);
            assert!(
                rise <= lim.max_step && -rise <= lim.max_drop,
                "a step from {at:?} to {next:?} the walk rule refuses",
            );
            at = next;
        }
    }

    /// A one-way ledge stays one-way. The coarse graph is directed, so a
    /// border you may only drop through must not offer a way back up.
    #[test]
    fn a_ledge_is_still_one_way_through_the_coarse_graph() {
        // A shelf three high covering the block boundary at x = BLOCK,
        // with the wall forcing the hierarchy on.
        let mut shelf = BTreeSet::new();
        for y in 0..64 {
            for x in 0..BLOCK {
                shelf.insert((x, y));
            }
        }
        let w = Walled {
            side: 64,
            wall: 40,
            gap: 4,
            shelf,
        };
        // Drops three, climbs one: off the shelf freely, never back up.
        let lim = NavLimits {
            max_step: 1,
            max_drop: 3,
            bounds: (0, 0, 63, 63),
            budget: 20_000,
        };
        let mut graph = FieldGraph::new();

        let down = graph.path(&w, (1, 1), (60, 1), &lim);
        assert_eq!(down.last(), Some(&(60, 1)), "it could not come off the shelf");
        let up = graph.path(&w, (60, 1), (1, 1), &lim);
        assert_ne!(up.last(), Some(&(1, 1)), "it climbed back up a ledge");
    }

    /// Ground that moved is ground the graph has to forget.
    #[test]
    fn a_changed_block_is_forgotten() {
        let side = 256;
        let w = walled(side);
        let lim = limits(side, 20_000);
        let mut graph = FieldGraph::new();
        graph.path(&w, (1, 1), (side - 2, 1), &lim);
        assert!(graph.built_blocks() > 0, "it never built anything");

        graph.invalidate((0, 0), (BLOCK, BLOCK));
        let kept = graph.built_blocks();
        graph.invalidate((0, 0), (side, side));
        assert_eq!(graph.built_blocks(), 0, "it kept blocks over changed ground");
        assert!(kept > 0, "one corner dropped every block on the map");
    }
}
