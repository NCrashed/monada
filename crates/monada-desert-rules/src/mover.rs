//! Getting from here to there: retained routes over the 3D stand graph
//! (docs/plans/desert-game.md §4c, §3c).
//!
//! **The route lives in the rules, not in entity fields.** A Rhai map
//! could not hold one — script functions are pure, so the RTS demo kept
//! a destination plus one waypoint and re-planned every cell. Typed
//! state is exactly what decision L1 bought: plan once, walk the list,
//! re-plan when the goal changes or the ground under the next step does.

use std::collections::BTreeMap;

use monada_fixed::{trig, Fixed, FixedVec3};
use monada_runtime::{Host, MoverProfile, VolumeLimits};
use monada_sim::EntityId;

use crate::gen::{BEDROCK_Z, SKY_Z};
use crate::MAP_CELLS;

/// A plan: where it was going, and what is left of it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Route {
    goal: (i64, i64),
    waypoints: Vec<(i64, i64, i64)>,
}

/// Everyone's retained routes.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Router {
    routes: BTreeMap<EntityId, Route>,
}

/// What one call to [`Router::step`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Still going.
    Moving,
    /// Standing on the goal cell.
    Arrived,
    /// The plan ran out short of the goal: this is as close as the mover
    /// gets. A caller that cares (a harvester whose field turned out to
    /// be behind a ridge) should pick a different goal rather than
    /// re-issuing the same one, which would only re-plan forever.
    Stuck,
}

impl Router {
    #[must_use]
    pub fn new() -> Router {
        Router::default()
    }

    /// Forget a mover's plan — it died, or its orders changed under it.
    pub fn forget(&mut self, mover: EntityId) {
        self.routes.remove(&mover);
    }

    /// Move `mover` one tick's worth toward `goal`.
    pub fn step(
        &mut self,
        host: &dyn Host,
        mover: EntityId,
        goal: (i64, i64),
        profile: MoverProfile,
        speed: Fixed,
    ) -> Step {
        let pos = host.entity_position(mover);
        let (cx, cy) = (
            i64::from(pos.x.floor_to_int()),
            i64::from(pos.y.floor_to_int()),
        );
        if (cx, cy) == goal {
            return Step::Arrived;
        }

        if self.stale(host, mover, goal, profile) {
            let from_z = ground(host, cx, cy);
            let to_z = ground(host, goal.0, goal.1);
            let waypoints = host.nav_path3(
                (cx, cy, from_z),
                (goal.0, goal.1, to_z),
                profile,
                &limits(),
            );
            self.routes.insert(mover, Route { goal, waypoints });
        }

        let Some(route) = self.routes.get_mut(&mover) else {
            return Step::Stuck;
        };
        let Some(&(wx, wy, wz)) = route.waypoints.first() else {
            // The plan is spent and we are not on the goal: either the
            // search could only reach a nearby stand, or the last hop
            // landed a cell short. Either way, re-issuing is the caller's
            // decision, not ours.
            return Step::Stuck;
        };

        let target = FixedVec3::new(
            Fixed::from_int(i32::try_from(wx).unwrap_or(0)),
            Fixed::from_int(i32::try_from(wy).unwrap_or(0)),
            Fixed::from_int(i32::try_from(wz + 1).unwrap_or(0)),
        );
        let (dx, dy) = (target.x - pos.x, target.y - pos.y);
        if abs(dx) <= speed && abs(dy) <= speed {
            route.waypoints.remove(0);
            host.entity_set_position(mover, target);
            return if (wx, wy) == goal {
                Step::Arrived
            } else {
                Step::Moving
            };
        }
        let heading = trig::atan2(dy, dx);
        let nx = pos.x + trig::cos(heading) * speed;
        let ny = pos.y + trig::sin(heading) * speed;
        let seat = ground(
            host,
            i64::from(nx.floor_to_int()),
            i64::from(ny.floor_to_int()),
        ) + 1;
        host.entity_set_position(
            mover,
            FixedVec3::new(nx, ny, Fixed::from_int(i32::try_from(seat).unwrap_or(0))),
        );
        host.entity_set_facing(mover, heading);
        Step::Moving
    }

    /// Whether the retained plan has to be thrown away.
    ///
    /// Two reasons, and only two. The goal moved — a new order. Or the
    /// ground under the very next step moved: settling and terraforming
    /// both reshape the map continuously (§4d), and a plan made across
    /// a dune that has since slumped is a mover walking through it.
    /// Checking only the next step, rather than revalidating the whole
    /// list, is what keeps this a handful of reads per mover per tick.
    fn stale(
        &self,
        host: &dyn Host,
        mover: EntityId,
        goal: (i64, i64),
        profile: MoverProfile,
    ) -> bool {
        let Some(route) = self.routes.get(&mover) else {
            return true;
        };
        if route.goal != goal {
            return true;
        }
        let Some(&(wx, wy, wz)) = route.waypoints.first() else {
            return false; // spent, not stale — `step` reports Stuck
        };
        ground(host, wx, wy) != wz && (ground(host, wx, wy) - wz).abs() > profile.max_step
    }
}

/// The ground height under a cell, from the store — one host call, and
/// bedrock where a column holds nothing at all.
#[must_use]
pub fn ground(host: &dyn Host, x: i64, y: i64) -> i64 {
    host.volume_top(x, y).map_or(BEDROCK_Z, |(z, _)| z)
}

/// The whole map, as a search's bounds.
#[must_use]
pub fn limits() -> VolumeLimits {
    VolumeLimits {
        bounds: (0, 0, MAP_CELLS - 1, MAP_CELLS - 1),
        z_range: (BEDROCK_Z, SKY_Z),
        // Generous: 65k columns is a big graph, and a vehicle that gives
        // up early looks broken. Tightening this is a D-9 profiling
        // question, not a guess to make now (§4c).
        budget: 40_000,
    }
}

fn abs(v: Fixed) -> Fixed {
    if v < Fixed::ZERO {
        -v
    } else {
        v
    }
}
