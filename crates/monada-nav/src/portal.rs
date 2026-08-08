//! Hierarchical navigation: a coarse graph of block-to-block portals over
//! the stand graph (docs/plans/desert-game.md §4c, §13a).
//!
//! The flat volume search is fine until a long barrier stands between a
//! unit and its goal. Then the octile heuristic — which points straight
//! at the goal, through the barrier — makes A\* expand nearly everything
//! on the near side before it rounds the end. Measured on the desert:
//! infantry crosses the map in 1.4 ms *over* the ridge, armour needs
//! 41 ms to go *around* it, which is more than a tick.
//!
//! The fix is the classical one. Cut the map into blocks; find where a
//! mover can cross each block border; pay for the expensive search once
//! per block instead of once per query; and plan over that small graph.
//! A route is then a handful of block hops refined into cells, rather
//! than a hundred thousand cell expansions.
//!
//! Two properties are deliberately kept and one is deliberately given up.
//! Kept: determinism (blocks, portals and edges are all built in a fixed
//! order, with `BTreeMap` throughout) and the walk rule (every refined
//! step comes from the same concrete search, so a path a mover cannot
//! walk cannot be produced). Given up: optimality — a hierarchical path
//! is a good path, not the shortest one, which is the trade every RTS
//! makes and no player has ever noticed.

use std::collections::{BTreeMap, BinaryHeap};

use crate::volume::{NavVolume, VolumeLimits, VolumeWorld};

/// A node in the concrete graph: column plus stand height.
type Node = (i64, i64, i64);

/// Two adjacent blocks, in a fixed order so a border is stored once
/// however it is approached.
type BorderKey = ((i64, i64), (i64, i64));

/// Cells per block edge.
///
/// Sixteen, so a block holds 256 columns: small enough that an intra-block
/// search is a fraction of a millisecond, large enough that a 256-cell map
/// is 256 blocks rather than thousands. The plan said eight *tiles*, which
/// is thirty-two cells; measurement preferred the smaller block, because
/// the cost of building one grows with its area while the number to build
/// grows only with the route's length.
pub const BLOCK: i64 = 16;

/// How many nodes the direct search may spend before the hierarchy takes
/// over. Two thousand is roughly two milliseconds — cheap enough to risk
/// on every query, and enough to cross an unobstructed map.
pub const SCOUT_BUDGET: usize = 2_000;

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
#[derive(Default)]
struct Block {
    /// Portal nodes that sit inside this block, in a fixed order.
    portals: Vec<Node>,
    /// `(i, j) -> cost` between this block's portals, for pairs that are
    /// mutually reachable *without leaving the block*. Missing means the
    /// two are not connected inside it — a block cut in half by a ridge
    /// is two separate places, and the abstract graph has to know.
    intra: BTreeMap<(usize, usize), i64>,
}

/// The coarse graph. Blocks are built on first use and dropped when the
/// terrain under them changes.
#[derive(Default)]
pub struct PortalGraph {
    blocks: BTreeMap<(i64, i64), Block>,
    /// Portal pairs across block borders: `(near, far)`, one step apart.
    /// Held per border so an edit invalidates exactly the borders it
    /// touches.
    borders: BTreeMap<BorderKey, Vec<(Node, Node)>>,
}

impl PortalGraph {
    #[must_use]
    pub fn new() -> PortalGraph {
        PortalGraph::default()
    }

    /// Drop every block and border the box touches, plus their immediate
    /// neighbours — a wall raised at a block's edge changes where its
    /// border can be crossed, not only what is inside it.
    ///
    /// Removed by key rather than by sweeping the graph, and that is a
    /// measured difference, not a preference. A terraforming faction edits
    /// thousands of cells a tick (docs/plans/desert-game.md §4e) and every
    /// one of them lands here; two `retain` passes over every built block
    /// and every border, three thousand times, cost 16 ms of a 33 ms tick
    /// on the desert. A border only ever joins two orthogonally adjacent
    /// blocks, so the keys to drop are enumerable: nine blocks and their
    /// four borders each, whatever the size of the graph.
    pub fn invalidate(&mut self, lo: (i64, i64), hi: (i64, i64)) {
        let (b0, b1) = (block_of(lo.0, lo.1), block_of(hi.0, hi.1));
        for by in (b0.1 - 1)..=(b1.1 + 1) {
            for bx in (b0.0 - 1)..=(b1.0 + 1) {
                self.blocks.remove(&(bx, by));
                for (dx, dy) in [(1_i64, 0_i64), (-1, 0), (0, 1), (0, -1)] {
                    self.borders.remove(&border_key((bx, by), (bx + dx, by + dy)));
                }
            }
        }
    }

    /// How many blocks are built — for tests and for a host watching the
    /// cache rather than guessing at it.
    #[must_use]
    pub fn built_blocks(&self) -> usize {
        self.blocks.len()
    }

    /// A path from `from` to `to`, planned over blocks and refined into
    /// cells.
    ///
    /// Falls back to the concrete search when the two ends share a block
    /// (there is no hierarchy to exploit over sixteen cells) and when the
    /// abstract graph has no route (a genuinely unreachable goal, where
    /// the flat search's closest-approach contract is what a player
    /// expects).
    pub fn path(
        &mut self,
        nav: &mut NavVolume,
        world: &impl VolumeWorld,
        from: Node,
        to: Node,
        limits: &VolumeLimits,
    ) -> Vec<Node> {
        if from == to {
            return Vec::new();
        }
        let (fb, tb) = (block_of(from.0, from.1), block_of(to.0, to.1));
        if fb == tb {
            return nav.path(world, from, to, limits);
        }

        // Try the flat search cheaply first.
        //
        // The hierarchy is not free: the blocks along a route cost about a
        // hundred milliseconds to build the first time. For a route with
        // no barrier across it that is a bad trade — infantry crossing the
        // desert *over* the ridge took 1.4 ms flat and 112 ms coarse, a
        // clean regression on the case that was already fine. So: spend a
        // small budget on the direct answer, and only pay for blocks when
        // the terrain proves it needs them. Which is also the honest
        // description of when a hierarchy helps at all.
        let scout = nav.path(
            world,
            from,
            to,
            &VolumeLimits {
                budget: SCOUT_BUDGET,
                ..*limits
            },
        );
        if scout.last() == Some(&to) {
            return scout;
        }

        // Endpoints join the abstract graph through their own blocks'
        // portals, which is the only place a concrete search is still
        // needed at query time.
        self.build_block(nav, world, fb, limits);
        self.build_block(nav, world, tb, limits);
        let start_edges = self.connect(nav, world, fb, from, limits);
        let goal_edges = self.connect(nav, world, tb, to, limits);
        if start_edges.is_empty() || goal_edges.is_empty() {
            return nav.path(world, from, to, limits);
        }

        let Some(hops) = self.abstract_route(nav, world, from, to, &start_edges, &goal_edges, limits)
        else {
            return nav.path(world, from, to, limits);
        };
        Self::refine(nav, world, &hops, limits)
    }

    /// Dijkstra over portal nodes. Small enough that a heuristic would
    /// buy nothing and cost a reason to doubt the ordering.
    #[allow(clippy::too_many_arguments)]
    fn abstract_route(
        &mut self,
        nav: &mut NavVolume,
        world: &impl VolumeWorld,
        from: Node,
        to: Node,
        start_edges: &[(Node, i64)],
        goal_edges: &[(Node, i64)],
        limits: &VolumeLimits,
    ) -> Option<Vec<Node>> {
        let goal_set: BTreeMap<Node, i64> = goal_edges.iter().copied().collect();
        let mut dist: BTreeMap<Node, i64> = BTreeMap::new();
        let mut parent: BTreeMap<Node, Node> = BTreeMap::new();
        let mut open: BinaryHeap<std::cmp::Reverse<(i64, Node)>> = BinaryHeap::new();

        for &(node, cost) in start_edges {
            if dist.get(&node).map_or(true, |&d| cost < d) {
                dist.insert(node, cost);
                parent.insert(node, from);
                open.push(std::cmp::Reverse((cost, node)));
            }
        }

        let mut best: Option<(i64, Node)> = None;
        while let Some(std::cmp::Reverse((d, node))) = open.pop() {
            if dist.get(&node).is_some_and(|&best_d| d > best_d) {
                continue; // stale entry
            }
            if let Some(&tail) = goal_set.get(&node) {
                let total = d + tail;
                if best.map_or(true, |(b, _)| total < b) {
                    best = Some((total, node));
                }
            }
            if best.is_some_and(|(b, _)| d >= b) {
                break; // nothing left can beat the route we have
            }
            for (next, step) in self.neighbours(nav, world, node, limits) {
                let nd = d + step;
                if dist.get(&next).map_or(true, |&old| nd < old) {
                    dist.insert(next, nd);
                    parent.insert(next, node);
                    open.push(std::cmp::Reverse((nd, next)));
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
    /// portal of its own block it can reach without leaving.
    fn neighbours(
        &mut self,
        nav: &mut NavVolume,
        world: &impl VolumeWorld,
        node: Node,
        limits: &VolumeLimits,
    ) -> Vec<(Node, i64)> {
        let b = block_of(node.0, node.1);
        self.build_block(nav, world, b, limits);
        let mut out = Vec::new();
        if let Some(block) = self.blocks.get(&b) {
            if let Some(i) = block.portals.iter().position(|&p| p == node) {
                for (j, &other) in block.portals.iter().enumerate() {
                    if let Some(&cost) = block.intra.get(&(i, j)) {
                        out.push((other, cost));
                    }
                }
            }
        }
        // Crossing a border is one step, and the pair is symmetric.
        for pairs in self.borders.values() {
            for &(a, c) in pairs {
                if a == node {
                    out.push((c, 10));
                } else if c == node {
                    out.push((a, 10));
                }
            }
        }
        out
    }

    /// The cost from a free-standing node to each portal of its block.
    fn connect(
        &mut self,
        nav: &mut NavVolume,
        world: &impl VolumeWorld,
        b: (i64, i64),
        node: Node,
        limits: &VolumeLimits,
    ) -> Vec<(Node, i64)> {
        let portals = self.blocks.get(&b).map(|x| x.portals.clone()).unwrap_or_default();
        let inner = VolumeLimits {
            bounds: block_bounds(b.0, b.1),
            ..*limits
        };
        portals
            .into_iter()
            .filter_map(|p| {
                let path = nav.path(world, node, p, &inner);
                (path.last() == Some(&p)).then(|| (p, cost_of(&path)))
            })
            .collect()
    }

    /// Turn a sequence of portal hops into cells, one concrete search per
    /// hop. Every waypoint therefore comes out of the same walk rule the
    /// flat search applies — the hierarchy chooses *which* corridors to
    /// use, never what a step is allowed to be.
    fn refine(
        nav: &mut NavVolume,
        world: &impl VolumeWorld,
        hops: &[Node],
        limits: &VolumeLimits,
    ) -> Vec<Node> {
        let mut out = Vec::new();
        for pair in hops.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let same_block = block_of(a.0, a.1) == block_of(b.0, b.1);
            let inner = if same_block {
                VolumeLimits {
                    bounds: block_bounds(block_of(a.0, a.1).0, block_of(a.0, a.1).1),
                    ..*limits
                }
            } else {
                // A border crossing: one step, but let the search confirm
                // it rather than asserting it.
                VolumeLimits {
                    bounds: span_bounds(a, b),
                    ..*limits
                }
            };
            let leg = nav.path(world, a, b, &inner);
            if leg.last() != Some(&b) {
                // The world moved under the plan. Give the caller what a
                // flat search would: the best route to where it can get.
                return out;
            }
            out.extend(leg);
        }
        out
    }

    /// Build a block's portals and intra-block costs, if it is not built.
    fn build_block(
        &mut self,
        nav: &mut NavVolume,
        world: &impl VolumeWorld,
        b: (i64, i64),
        limits: &VolumeLimits,
    ) {
        if self.blocks.contains_key(&b) {
            return;
        }
        // Borders first: a block's portals ARE its share of the four
        // borders around it.
        let mut portals: Vec<Node> = Vec::new();
        for (other, axis) in [
            ((b.0 + 1, b.1), Axis::X),
            ((b.0 - 1, b.1), Axis::X),
            ((b.0, b.1 + 1), Axis::Y),
            ((b.0, b.1 - 1), Axis::Y),
        ] {
            let key = border_key(b, other);
            if let std::collections::btree_map::Entry::Vacant(slot) = self.borders.entry(key) {
                slot.insert(find_portals(nav, world, key.0, key.1, axis, limits));
            }
            for &(a, c) in &self.borders[&key] {
                for node in [a, c] {
                    if block_of(node.0, node.1) == b && !portals.contains(&node) {
                        portals.push(node);
                    }
                }
            }
        }
        portals.sort_unstable();

        let inner = VolumeLimits {
            bounds: block_bounds(b.0, b.1),
            ..*limits
        };
        let mut intra = BTreeMap::new();
        for i in 0..portals.len() {
            for j in 0..portals.len() {
                if i == j {
                    continue;
                }
                let path = nav.path(world, portals[i], portals[j], &inner);
                if path.last() == Some(&portals[j]) {
                    intra.insert((i, j), cost_of(&path));
                }
            }
        }
        self.blocks.insert(b, Block { portals, intra });
    }
}

/// Which way a border runs.
#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
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

/// Where a mover can cross the border between two blocks: one portal per
/// contiguous run of crossable cells, placed at the run's middle.
///
/// One per run rather than one per cell is the whole economy of the
/// scheme — a wide-open border becomes a single node instead of sixteen —
/// and the middle rather than an end because a corner crossing tends to
/// be the one a corner-cut ban then refuses.
fn find_portals(
    nav: &mut NavVolume,
    world: &impl VolumeWorld,
    a: (i64, i64),
    b: (i64, i64),
    axis: Axis,
    limits: &VolumeLimits,
) -> Vec<(Node, Node)> {
    let (bx0, by0, bx1, by1) = limits.bounds;
    let in_bounds = |x: i64, y: i64| x >= bx0 && x <= bx1 && y >= by0 && y <= by1;
    let mut run: Vec<(Node, Node)> = Vec::new();
    let mut out = Vec::new();
    let mut flush = |run: &mut Vec<(Node, Node)>| {
        if !run.is_empty() {
            out.push(run[run.len() / 2]);
            run.clear();
        }
    };

    // The two adjacent cell lines that face each other across the border.
    let span: Vec<((i64, i64), (i64, i64))> = match axis {
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

    for ((ax, ay), (cx, cy)) in span {
        if !in_bounds(ax, ay) || !in_bounds(cx, cy) {
            flush(&mut run);
            continue;
        }
        // A crossing exists when one cell's stand can step onto the
        // other's — asked of the same search the mover will use, so the
        // portal cannot promise a step the walk rule forbids.
        let Some(here) = nav.stand_at(world, ax, ay, i64::MIN / 4, limits.z_range) else {
            flush(&mut run);
            continue;
        };
        let step = nav.path(
            world,
            (ax, ay, here.z),
            (cx, cy, here.z),
            &VolumeLimits {
                bounds: (ax.min(cx), ay.min(cy), ax.max(cx), ay.max(cy)),
                budget: 8,
                ..*limits
            },
        );
        match step.last() {
            Some(&(lx, ly, lz)) if (lx, ly) == (cx, cy) => {
                run.push(((ax, ay, here.z), (cx, cy, lz)));
            }
            _ => flush(&mut run),
        }
    }
    flush(&mut run);
    out
}

/// The bounding box of two nodes, one cell wider all round so a border
/// step has room to be confirmed.
fn span_bounds(a: Node, b: Node) -> (i64, i64, i64, i64) {
    (
        a.0.min(b.0) - 1,
        a.1.min(b.1) - 1,
        a.0.max(b.0) + 1,
        a.1.max(b.1) + 1,
    )
}

/// The octile cost of a concrete path: 10 per orthogonal step, 14 per
/// diagonal — the same currency the flat search spends.
fn cost_of(path: &[Node]) -> i64 {
    let mut cost = 0;
    for pair in path.windows(2) {
        let (dx, dy) = ((pair[1].0 - pair[0].0).abs(), (pair[1].1 - pair[0].1).abs());
        cost += if dx + dy == 2 { 14 } else { 10 };
    }
    cost
}
