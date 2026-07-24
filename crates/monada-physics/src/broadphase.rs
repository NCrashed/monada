//! Broadphase: a transient uniform spatial hash over body bounding
//! volumes (docs/plans/voxel-physics.md §3 `broadphase`, P5).
//!
//! Rebuilt from scratch every tick — it is a pure function of body
//! poses, so it is neither hashed nor serialized. Sleeping bodies ARE
//! inserted (that is how awake bodies find them to wake them through
//! a real contact); ghosts are not (nothing to collide with).
//!
//! Candidate pairs are exactly that — candidates. Adjacency here has
//! no side effects: waking decisions belong to narrowphase, which
//! must find an actual contact (P5 amendments — a vehicle driving ten
//! voxels past a sleeping pile must not thrash its sleep state).

use monada_fixed::Fixed;
use std::collections::{BTreeMap, BTreeSet};

use crate::body::RigidBody;
use crate::contact::CONTACT_MARGIN;

/// Spatial-hash cell edge, in voxels. Bodies larger than a cell are
/// inserted into every cell their bounding box overlaps.
const CELL: i64 = 16;

/// Candidate body pairs `(i, j)`, `i < j`, ascending — indices into
/// the body Vec. Coarse cell co-occupancy filtered by the exact
/// bounding-sphere test (on relative offsets — rule 6).
pub(crate) fn candidate_pairs(bodies: &[RigidBody]) -> Vec<(usize, usize)> {
    let mut grid: BTreeMap<(i64, i64, i64), Vec<usize>> = BTreeMap::new();
    for (index, body) in bodies.iter().enumerate() {
        if body.skin.is_empty() {
            continue; // ghosts
        }
        let r = body.bounding_radius + CONTACT_MARGIN;
        let cell_of = |v: Fixed| i64::from(v.floor_to_int()).div_euclid(CELL);
        let lo = (
            cell_of(body.position.x - r),
            cell_of(body.position.y - r),
            cell_of(body.position.z - r),
        );
        let hi = (
            cell_of(body.position.x + r),
            cell_of(body.position.y + r),
            cell_of(body.position.z + r),
        );
        for cx in lo.0..=hi.0 {
            for cy in lo.1..=hi.1 {
                for cz in lo.2..=hi.2 {
                    grid.entry((cx, cy, cz)).or_default().push(index);
                }
            }
        }
    }

    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    for members in grid.values() {
        for (n, &i) in members.iter().enumerate() {
            for &j in &members[n + 1..] {
                let (a, b) = if i < j { (i, j) } else { (j, i) };
                // Exact prune: bounding spheres, relative offset only.
                let d = bodies[a].position - bodies[b].position;
                let reach = bodies[a].bounding_radius + bodies[b].bounding_radius + CONTACT_MARGIN;
                if d.dot(d) < reach * reach {
                    pairs.insert((a, b));
                }
            }
        }
    }
    pairs.into_iter().collect()
}
