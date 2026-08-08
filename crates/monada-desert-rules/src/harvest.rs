//! The harvester loop (docs/plans/desert-game.md §7) — find spice, cut
//! it out of the ground, drive it home, empty it, repeat.
//!
//! The loop is the oldest one in the genre and it is here almost
//! unchanged, but one thing about it is new: **the spice is the terrain**
//! (§4b). A harvester does not decrement a counter on a tile, it calls
//! `volume_clear` on the top cell of the column it is standing on. So a
//! field visibly wears away as it is worked, a deep vein is unharvestable
//! until somebody digs the sand off it, and a crater through a field
//! scatters spice the way it scatters everything else.
//!
//! What that costs the rules is one extra rule and no engine surface: a
//! harvester cuts the top cell of a column *when that cell is spice*, and
//! everything else — depletion, veins, spice thrown about by shellfire —
//! falls out of it.

use std::collections::BTreeMap;

use monada_fixed::Fixed;
use monada_runtime::{Host, MoverProfile};
use monada_sim::EntityId;

use crate::economy::{
    Economy, PlayerNo, CREDITS_PER_CELL, HARVESTER_CAPACITY, HARVEST_RATE, UNLOAD_RATE,
};
use crate::material;
use crate::mover::{Router, Step};

/// How fast a loaded harvester crawls, in cells per tick.
pub const HARVESTER_SPEED: Fixed = Fixed::from_bits(1 << 29); // 1/8 cell

/// What a harvester is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Task {
    /// Driving to a cell believed to hold spice.
    Seek((i64, i64)),
    /// Standing over spice, cutting.
    Cut,
    /// Driving home, loaded.
    Return,
    /// Standing at the refinery, emptying.
    Unload,
    /// Nothing reachable left to work. Not an error state — on a mission
    /// whose fields are exhausted it is the correct end of the economy,
    /// and the HUD should say so rather than showing a unit that looks
    /// broken.
    Idle,
}

/// One harvester's state, which is the rules' and not the world's: a
/// Rhai map would have had to flatten it into numbered fields (§3c).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Harvester {
    pub owner: PlayerNo,
    /// Cells aboard.
    pub load: u32,
    pub task: Task,
    /// The refinery this one serves.
    pub home: (i64, i64),
    /// Ticks to wait before searching again after a search found
    /// nothing.
    ///
    /// Not politeness — a bound. A failed search sweeps every site on the
    /// map, and an idle harvester repeating that thirty times a second
    /// is ten thousand column reads a tick spent learning the same thing
    /// it learned last tick. On a mission whose fields are exhausted
    /// *every* harvester is in that state at once.
    waiting: u32,
}

/// How long a harvester that found nothing waits before looking again.
/// One second: fast enough that a bore breaking into a vein is noticed
/// promptly, slow enough that failing costs nothing.
pub const IDLE_RESCAN: u32 = 30;

/// A spice site as the search knows one: centre and radius, from the
/// generator. Searching the *known* discs rather than sweeping the map
/// is what keeps a re-target to about a thousand column reads instead of
/// sixty-five thousand.
pub type Site = (i64, i64, i64);

/// Every harvester in the game.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Fleet {
    units: BTreeMap<EntityId, Harvester>,
}

/// What one tick of harvesting did — for the HUD, and for the tests that
/// have to assert an exact number at an exact tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Yield {
    /// Cells cut out of the ground.
    pub cut: u32,
    /// Credits banked at a refinery.
    pub banked: u32,
}

impl Fleet {
    #[must_use]
    pub fn new() -> Fleet {
        Fleet::default()
    }

    /// Put a harvester into service, serving the refinery at `home`.
    pub fn enlist(&mut self, unit: EntityId, owner: PlayerNo, home: (i64, i64)) {
        self.units.insert(
            unit,
            Harvester {
                owner,
                load: 0,
                task: Task::Idle,
                home,
                waiting: 0,
            },
        );
    }

    /// Point every one of a player.s harvesters at a new refinery — what
    /// happens the moment one is built, and the moment one dies.
    pub fn rehome(&mut self, owner: PlayerNo, home: (i64, i64)) {
        for unit in self.units.values_mut() {
            if unit.owner == owner {
                unit.home = home;
            }
        }
    }

    /// Take one out of service.
    pub fn discharge(&mut self, unit: EntityId) {
        self.units.remove(&unit);
    }

    #[must_use]
    pub fn get(&self, unit: EntityId) -> Option<&Harvester> {
        self.units.get(&unit)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.units.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// One tick of the whole fleet, in entity order.
    pub fn run(
        &mut self,
        host: &dyn Host,
        economy: &mut Economy,
        router: &mut Router,
        sites: &[Site],
        profile: MoverProfile,
    ) -> Yield {
        let mut total = Yield::default();
        let ids: Vec<EntityId> = self.units.keys().copied().collect();
        for id in ids {
            let step = self.step_one(host, economy, router, sites, profile, id);
            total.cut += step.cut;
            total.banked += step.banked;
        }
        total
    }

    fn step_one(
        &mut self,
        host: &dyn Host,
        economy: &mut Economy,
        router: &mut Router,
        sites: &[Site],
        profile: MoverProfile,
        id: EntityId,
    ) -> Yield {
        let mut done = Yield::default();
        let Some(unit) = self.units.get(&id).cloned() else {
            return done;
        };
        let pos = host.entity_position(id);
        let at = (
            i64::from(pos.x.floor_to_int()),
            i64::from(pos.y.floor_to_int()),
        );

        let next = match unit.task {
            Task::Idle | Task::Seek(_) if unit.load >= HARVESTER_CAPACITY => Task::Return,

            Task::Idle if unit.waiting > 0 => {
                self.units.get_mut(&id).expect("harvester").waiting -= 1;
                Task::Idle
            }

            Task::Idle => {
                if let Some(cell) = nearest_spice(host, sites, at, profile.max_step) {
                    Task::Seek(cell)
                } else {
                    self.units.get_mut(&id).expect("harvester").waiting = IDLE_RESCAN;
                    Task::Idle
                }
            }

            Task::Seek(cell) => match router.step(host, id, cell, profile, HARVESTER_SPEED) {
                Step::Moving => Task::Seek(cell),
                // Arrived, or as close as the terrain allows. Either way
                // the question is the same: is there spice under me?
                Step::Arrived | Step::Stuck => {
                    if workable(host, at, profile.max_step) {
                        Task::Cut
                    } else {
                        router.forget(id);
                        match nearest_spice(host, sites, at, profile.max_step) {
                            Some(next) if next != cell && next != at => Task::Seek(next),
                            // The search points back at the cell we just
                            // failed to reach — a field across the ridge,
                            // most likely. Re-issuing it is a FULL-MAP
                            // re-plan every tick for the rest of the
                            // match: measured at 3.5 ms a tick for one
                            // harvester, and D-9 wants dozens. Wait
                            // instead, and try again in a second: the
                            // ground changes, and one day it will be
                            // reachable.
                            _ => {
                                self.units.get_mut(&id).expect("harvester").waiting = IDLE_RESCAN;
                                Task::Idle
                            }
                        }
                    }
                }
            },

            Task::Cut => {
                if unit.load >= HARVESTER_CAPACITY {
                    Task::Return
                } else if workable(host, at, profile.max_step) {
                    let (top, _) = host.volume_top(at.0, at.1).expect("spice column");
                    let take = HARVEST_RATE.min(HARVESTER_CAPACITY - unit.load);
                    for k in 0..i64::from(take) {
                        host.volume_clear(at.0, at.1, top - k);
                    }
                    done.cut = take;
                    self.units.get_mut(&id).expect("harvester").load += take;
                    Task::Cut
                } else {
                    // This column is worked out. Step to the next cell of
                    // the field rather than driving home — a harvester
                    // that made the round trip for every single cell
                    // would spend its whole life on the road, which is
                    // both wrong and very slow to watch.
                    router.forget(id);
                    match nearest_spice(host, sites, at, profile.max_step) {
                        Some(cell) if cell != at => Task::Seek(cell),
                        Some(_) => Task::Cut,
                        None if unit.load > 0 => Task::Return,
                        None => Task::Idle,
                    }
                }
            }

            Task::Return => match router.step(host, id, unit.home, profile, HARVESTER_SPEED) {
                Step::Arrived => Task::Unload,
                // Still going — or the refinery is unreachable, because
                // it was destroyed or the ground between it and here
                // changed. Sit on the load either way rather than
                // spinning on a goal that will not resolve: D-6 gives
                // this one a carryall.
                Step::Moving | Step::Stuck => Task::Return,
            },

            Task::Unload => {
                let moved = UNLOAD_RATE.min(unit.load);
                if moved == 0 {
                    Task::Idle
                } else {
                    let player = economy.player(unit.owner);
                    player.deposit(moved * CREDITS_PER_CELL);
                    done.banked = moved * CREDITS_PER_CELL;
                    let held = &mut self.units.get_mut(&id).expect("harvester").load;
                    *held -= moved;
                    if *held == 0 {
                        Task::Idle
                    } else {
                        Task::Unload
                    }
                }
            }
        };

        if let Some(u) = self.units.get_mut(&id) {
            u.task = next;
        }
        done
    }
}

/// How far around itself a harvester looks before sweeping a whole
/// field: enough to find the next cell of the face it is working.
const WORKING_FACE: i64 = 3;

/// Whether there is spice here that a harvester could actually take.
///
/// The two halves are inseparable: a cell of ore in a pit too deep to
/// drive out of is not a cell of ore, it is bait. Both searches use this
/// rather than bare solidity, because a search that can return an
/// unworkable cell puts the machine in a loop — arrive, decline to cut,
/// search, get the same answer.
fn workable(host: &dyn Host, at: (i64, i64), max_step: i64) -> bool {
    is_spice(host, at) && can_work(host, at, max_step)
}

/// Whether a harvester may take a cell here without stranding itself.
///
/// **A harvester digs its own exit ramp or it does not dig.** The cell it
/// removes is the ground it is standing on, so a rich seam is also a
/// trap: cut four cells out of a deep vein and the machine is sitting in
/// a hole with walls it cannot climb, still full, forever. The rule is
/// the walk rule (§4b) applied one step ahead — after this cut, can it
/// still reach the highest ground beside it?
///
/// It is also the mechanic that makes a Dweller's excavation matter: to
/// work a thick vein you have to open the pit *wide*, not just deep.
fn can_work(host: &dyn Host, at: (i64, i64), max_step: i64) -> bool {
    let Some((top, _)) = host.volume_top(at.0, at.1) else {
        return false;
    };
    let mut highest = i64::MIN;
    for (dx, dy) in [(1_i64, 0_i64), (-1, 0), (0, 1), (0, -1)] {
        if let Some((z, _)) = host.volume_top(at.0 + dx, at.1 + dy) {
            highest = highest.max(z);
        }
    }
    highest == i64::MIN || highest - (top - 1) <= max_step
}

/// The nearest exposed spice within `radius` of a point, by walking the
/// square around it.
fn spice_within(
    host: &dyn Host,
    from: (i64, i64),
    radius: i64,
    max_step: i64,
) -> Option<(i64, i64)> {
    let mut best: Option<((i64, i64), i64)> = None;
    for y in (from.1 - radius)..=(from.1 + radius) {
        for x in (from.0 - radius)..=(from.0 + radius) {
            if !workable(host, (x, y), max_step) {
                continue;
            }
            let d = (x - from.0).abs() + (y - from.1).abs();
            if best.map_or(true, |(_, near)| d < near) {
                best = Some(((x, y), d));
            }
        }
    }
    best.map(|(cell, _)| cell)
}

/// Whether the top of this column is spice — the one rule the whole
/// economy rests on.
fn is_spice(host: &dyn Host, at: (i64, i64)) -> bool {
    host.volume_top(at.0, at.1)
        .is_some_and(|(_, mat)| mat == material::SPICE)
}

/// The nearest exposed spice cell.
///
/// Two searches, cheap one first, and the split is the difference
/// between a playable frame and a slideshow. A harvester working a face
/// re-targets after **every cell it cuts**, and what it wants is almost
/// always the cell next to it — so look under its nose before sweeping a
/// field. The sweep behind it scans only the generator's own discs,
/// which is exhaustive by construction (every cell of spice on the map
/// is inside one, including the deep veins a Dweller has just dug down
/// to) and still a thousand reads rather than sixty-five thousand.
fn nearest_spice(
    host: &dyn Host,
    sites: &[Site],
    from: (i64, i64),
    max_step: i64,
) -> Option<(i64, i64)> {
    if let Some(near) = spice_within(host, from, WORKING_FACE, max_step) {
        return Some(near);
    }
    let mut order: Vec<&Site> = sites.iter().collect();
    order.sort_by_key(|(cx, cy, _)| (cx - from.0).abs() + (cy - from.1).abs());
    for &&(cx, cy, radius) in &order {
        let mut best: Option<((i64, i64), i64)> = None;
        for y in (cy - radius)..=(cy + radius) {
            for x in (cx - radius)..=(cx + radius) {
                let (dx, dy) = (x - cx, y - cy);
                if dx * dx + dy * dy > radius * radius || !workable(host, (x, y), max_step) {
                    continue;
                }
                let d = (x - from.0).abs() + (y - from.1).abs();
                if best.map_or(true, |(_, near)| d < near) {
                    best = Some(((x, y), d));
                }
            }
        }
        if let Some((cell, _)) = best {
            return Some(cell);
        }
    }
    None
}
