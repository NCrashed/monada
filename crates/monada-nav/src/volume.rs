//! Three-dimensional navigation over a volumetric world
//! (docs/plans/desert-game.md §4c) — the desert game's second engineering
//! payload.
//!
//! The flat [`astar`](crate::astar) answers "which cells, in what order"
//! over a heightfield, where a column has exactly one walkable surface.
//! A volume world does not: a column pierced by a tunnel has two, and a
//! stack of galleries has more. Everything else follows from that one
//! change — a node is a **stand** rather than a cell, and a bore becomes
//! ordinary passable ground instead of a scripted teleport, which is what
//! makes it findable, occupiable and collapsible (§6b).
//!
//! Determinism is by construction, exactly as in the flat search: integer
//! octile costs, a fixed neighbour order, a monotone insertion counter as
//! the only tie-break, `BTreeMap` throughout, no floats anywhere.

use std::collections::{BTreeMap, BinaryHeap};

use crate::NEIGHBOURS;

/// The world a volumetric path is planned against. Implementations must
/// be pure functions of their state — the same cell always answers the
/// same — or determinism is forfeit.
pub trait VolumeWorld {
    /// Whether the cell is solid ground.
    fn solid(&self, x: i64, y: i64, z: i64) -> bool;
}

/// A place a mover can stand: a solid cell with clear space above it.
///
/// `z` is the SOLID cell's own height; a mover occupies `z + 1 ..`. That
/// convention matches how a map seats a unit (scan down for ground, sit
/// on top of it) and keeps the walk rule a comparison of ground heights,
/// the same quantity the flat search compares.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stand {
    pub z: i64,
    /// Air cells above `z`, capped at the profile's height. A stand under
    /// a ceiling reports exactly the profile's height, so "is this a
    /// tunnel?" is a question about the cell above, not about this
    /// number.
    pub headroom: i64,
    /// Whether something solid caps this stand within the mover's reach —
    /// the difference between the open desert and a bore.
    pub enclosed: bool,
}

/// What a mover needs from the ground (§4c).
///
/// The three unit classes of the desert game differ only here: infantry
/// is short and climbs well, armour is tall and climbs badly, a harvester
/// is taller still. Mountains then wall out armour and admit infantry
/// with no obstacle markup at all — the walk rule alone decides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MoverProfile {
    /// Cells of clearance the mover needs above the ground it stands on.
    pub height: i64,
    /// Maximum |Δ ground height| a single step may climb or drop.
    pub max_step: i64,
    /// Whether the mover may use stands that are capped — a bore, a
    /// gallery, anything with a roof. A surface-only mover sees just the
    /// topmost stand of each column.
    pub tunnels: bool,
}

impl MoverProfile {
    /// Infantry: two cells of clearance, climbs four.
    #[must_use]
    pub fn infantry() -> MoverProfile {
        MoverProfile {
            height: 2,
            max_step: 4,
            tunnels: true,
        }
    }

    /// Armour: three cells of clearance, climbs two.
    #[must_use]
    pub fn vehicle() -> MoverProfile {
        MoverProfile {
            height: 3,
            max_step: 2,
            tunnels: false,
        }
    }
}

/// Search limits: where the search may look and how hard it may try.
#[derive(Clone, Copy, Debug)]
pub struct VolumeLimits {
    /// Inclusive cell bounds `(x0, y0, x1, y1)`.
    pub bounds: (i64, i64, i64, i64),
    /// Inclusive vertical range to extract stands from `(z_lo, z_hi)`.
    pub z_range: (i64, i64),
    /// Maximum nodes to pop before giving up with a partial path.
    pub budget: usize,
}

/// A node: a column plus which stand of it.
type Node = (i64, i64, i64);

/// One open-set entry: `(f, seq, x, y, z)`, ordered by f then by the
/// monotone push counter — the tie-break that makes the search's answer
/// a function of its input and nothing else.
type OpenEntry = std::cmp::Reverse<(i64, u64, i64, i64, i64)>;

/// The stand graph over a volume world, cached per column.
///
/// One instance per **mover profile**: what counts as a stand depends on
/// how much clearance the mover needs, so a shared cache would answer a
/// harvester's question with infantry's ground. Holding one each is
/// cheaper than re-deriving, and it is what makes
/// [`invalidate`](NavVolume::invalidate) a bounded operation — a berm
/// dirties the columns it covers and nothing else, which matters when
/// three factions reshape the map continuously (§4e).
pub struct NavVolume {
    profile: MoverProfile,
    stands: BTreeMap<(i64, i64), Vec<Stand>>,
}

impl NavVolume {
    #[must_use]
    pub fn new(profile: MoverProfile) -> NavVolume {
        NavVolume {
            profile,
            stands: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn profile(&self) -> MoverProfile {
        self.profile
    }

    /// Drop the cached stands of every column in the inclusive box, so the
    /// next query re-derives them. Call it wherever terrain changes.
    pub fn invalidate(&mut self, lo: (i64, i64), hi: (i64, i64)) {
        for y in lo.1..=hi.1 {
            for x in lo.0..=hi.0 {
                self.stands.remove(&(x, y));
            }
        }
    }

    /// How many columns are currently cached — for tests and for a host
    /// that wants to watch the cache rather than guess at it.
    #[must_use]
    pub fn cached_columns(&self) -> usize {
        self.stands.len()
    }

    /// Every stand in a column, highest first.
    pub fn stands(
        &mut self,
        world: &impl VolumeWorld,
        x: i64,
        y: i64,
        z_range: (i64, i64),
    ) -> &[Stand] {
        let profile = self.profile;
        self.stands
            .entry((x, y))
            .or_insert_with(|| extract(world, x, y, z_range, profile))
    }

    /// Make sure a column's stands are cached, then hand back a borrow of
    /// them.
    ///
    /// Split from the reads below on purpose. The obvious shape —
    /// "return the usable stands as a `Vec`" — allocates on every
    /// neighbour probe, and a search probes ten times per popped node,
    /// which measured as an entire order of magnitude: a corner-to-corner
    /// vehicle route cost 47 ms allocating and a few milliseconds not.
    fn ensure(&mut self, world: &impl VolumeWorld, x: i64, y: i64, z_range: (i64, i64)) {
        if !self.stands.contains_key(&(x, y)) {
            let stands = extract(world, x, y, z_range, self.profile);
            self.stands.insert((x, y), stands);
        }
    }

    /// The stands this mover may use in a cached column: all of them when
    /// it can tunnel, the topmost open one otherwise.
    fn usable_cached(&self, x: i64, y: i64) -> impl Iterator<Item = &Stand> + '_ {
        let tunnels = self.profile.tunnels;
        self.stands
            .get(&(x, y))
            .into_iter()
            .flatten()
            .filter(move |s| tunnels || !s.enclosed)
            .take(if tunnels { usize::MAX } else { 1 })
    }

    /// The stand of `(x, y)` a mover standing at ground height `z` would
    /// be on, if any — the entry point from a world position into the
    /// graph.
    pub fn stand_at(
        &mut self,
        world: &impl VolumeWorld,
        x: i64,
        y: i64,
        z: i64,
        z_range: (i64, i64),
    ) -> Option<Stand> {
        self.ensure(world, x, y, z_range);
        self.usable_cached(x, y).min_by_key(|s| (s.z - z).abs()).copied()
    }

    /// A deterministic best-first path from one stand to another.
    ///
    /// Returns the waypoints after `from` up to and including the goal;
    /// empty when already there. An unreachable goal yields the path to
    /// the closest reachable stand rather than an error — the same
    /// "walk as far as you can" contract the flat search keeps, and the
    /// behaviour an RTS player expects from a misclick.
    pub fn path(
        &mut self,
        world: &impl VolumeWorld,
        from: Node,
        to: Node,
        limits: &VolumeLimits,
    ) -> Vec<Node> {
        if from == to {
            return Vec::new();
        }
        let (bx0, by0, bx1, by1) = limits.bounds;
        let in_bounds = |x: i64, y: i64| x >= bx0 && x <= bx1 && y >= by0 && y <= by1;
        if !in_bounds(from.0, from.1) {
            return Vec::new();
        }

        let mut best_g: BTreeMap<Node, i64> = BTreeMap::new();
        let mut parent: BTreeMap<Node, Node> = BTreeMap::new();
        let mut open: BinaryHeap<OpenEntry> = BinaryHeap::new();
        let mut seq: u64 = 0;

        best_g.insert(from, 0);
        open.push(std::cmp::Reverse((
            octile(from, to),
            seq,
            from.0,
            from.1,
            from.2,
        )));

        let mut best_node = from;
        let mut best_key = (octile(from, to), 0_i64);
        let mut popped = 0_usize;

        while let Some(std::cmp::Reverse((f, _, cx, cy, cz))) = open.pop() {
            let cur = (cx, cy, cz);
            let g = best_g[&cur];
            let h = octile(cur, to);
            // A stale heap entry, superseded by a cheaper push for the
            // same node — skip it without charging the budget.
            if f > g + h {
                continue;
            }
            if (h, g) < best_key {
                best_key = (h, g);
                best_node = cur;
            }
            if cur == to {
                return unwind(&parent, from, to);
            }
            popped += 1;
            if popped >= limits.budget {
                break;
            }

            for &(dx, dy, cost) in &NEIGHBOURS {
                let (nx, ny) = (cx + dx, cy + dy);
                if !in_bounds(nx, ny) {
                    continue;
                }
                let Some(step) = self.step_to(world, cz, nx, ny, limits.z_range) else {
                    continue;
                };
                // No corner cutting: a diagonal needs both flanks to offer
                // a step of their own, or a mover slips between a bore
                // mouth and the rock beside it.
                if dx != 0
                    && dy != 0
                    && (self.step_to(world, cz, cx + dx, cy, limits.z_range).is_none()
                        || self.step_to(world, cz, cx, cy + dy, limits.z_range).is_none())
                {
                    continue;
                }
                let next = (nx, ny, step);
                let ng = g + cost;
                if best_g.get(&next).map_or(true, |&old| ng < old) {
                    best_g.insert(next, ng);
                    parent.insert(next, cur);
                    seq += 1;
                    open.push(std::cmp::Reverse((ng + octile(next, to), seq, nx, ny, step)));
                }
            }
        }

        unwind(&parent, from, best_node)
    }

    /// The stand of `(nx, ny)` a mover at ground height `cz` can step onto
    /// — the closest one within `max_step`, or `None`.
    fn step_to(
        &mut self,
        world: &impl VolumeWorld,
        cz: i64,
        nx: i64,
        ny: i64,
        z_range: (i64, i64),
    ) -> Option<i64> {
        // ONE map lookup, not two. A search probes this ten times per
        // popped node — eight neighbours plus two corner flanks — so a
        // "check, then fetch" pair doubles the tree walks that dominate
        // the search's cost. `entry` does both at once, and the profile is
        // copied out first so the borrow it holds does not collide.
        let profile = self.profile;
        let stands = self
            .stands
            .entry((nx, ny))
            .or_insert_with(|| extract(world, nx, ny, z_range, profile));
        stands
            .iter()
            .filter(|s| profile.tunnels || !s.enclosed)
            .take(if profile.tunnels { usize::MAX } else { 1 })
            .map(|s| s.z)
            .filter(|&z| (z - cz).abs() <= profile.max_step)
            .min_by_key(|&z| (z - cz).abs())
    }
}

/// Walk one column top-down and record every stand.
///
/// Top-down rather than bottom-up so the first entry is the open surface,
/// which is what a surface-only mover wants and what a `take(1)` then
/// gets for free.
fn extract(
    world: &impl VolumeWorld,
    x: i64,
    y: i64,
    (z_lo, z_hi): (i64, i64),
    profile: MoverProfile,
) -> Vec<Stand> {
    let mut out = Vec::new();
    let mut air = 0_i64;
    let mut capped = false;
    let mut z = z_hi;
    while z >= z_lo {
        if world.solid(x, y, z) {
            if air >= profile.height {
                out.push(Stand {
                    z,
                    headroom: air.min(profile.height),
                    enclosed: capped,
                });
            }
            // Everything below this cell is under a roof.
            capped = true;
            air = 0;
        } else {
            air += 1;
        }
        z -= 1;
    }
    out
}

/// Octile distance ×10 over the horizontal plane. Vertical travel is free
/// because a step's cost is the step, not the climb — a stair and a flat
/// run of the same length cost the same, which is admissible and keeps
/// the heuristic consistent.
fn octile(a: Node, b: Node) -> i64 {
    let dx = (a.0 - b.0).abs();
    let dy = (a.1 - b.1).abs();
    let (lo, hi) = if dx < dy { (dx, dy) } else { (dy, dx) };
    14 * lo + 10 * (hi - lo)
}

/// Rebuild the waypoint list `from → goal` (exclusive of `from`).
fn unwind(parent: &BTreeMap<Node, Node>, from: Node, goal: Node) -> Vec<Node> {
    let mut out = Vec::new();
    let mut cur = goal;
    while cur != from {
        out.push(cur);
        match parent.get(&cur) {
            Some(&p) => cur = p,
            None => return Vec::new(), // unreachable: no path recorded
        }
    }
    out.reverse();
    out
}
