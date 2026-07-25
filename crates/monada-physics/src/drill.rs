//! Drill coupling (docs/plans/voxel-physics.md §4, §6 P6).
//!
//! The contract, verbatim from the plan: physics reports what the
//! tool face overlaps ([`PhysicsWorld::drill_query`]); the ENGINE
//! decides what actually gets cut (hardness tables, tool power, wear
//! — none of that lives here); physics applies the reaction from what
//! WAS cut ([`PhysicsWorld::drill_reaction`]). Body-vs-body drilling
//! deliberately has no API of its own — the engine composes it from
//! `raycast` → `remove_voxels` → `apply_impulse_at`, all three
//! already tested.
//!
//! [`PhysicsWorld::drill_query`]: crate::PhysicsWorld::drill_query
//! [`PhysicsWorld::drill_reaction`]: crate::PhysicsWorld::drill_reaction

use monada_fixed::{FixedQuat, FixedVec3};

use crate::material::MaterialId;

/// A drill tool: an oriented box riding the body (like a wheel, the
/// anchor is body-frame, CoM-relative). Overlap is tested against
/// CELL CENTRES, boundary INCLUSIVE (a centre exactly on the box face
/// counts as overlapped) — a half-voxel chunky allowance at the rim,
/// deliberate at voxel granularity.
#[derive(Clone, Copy, Debug)]
pub struct DrillTool {
    /// Box centre, body frame (CoM-relative).
    pub anchor: FixedVec3,
    /// Half-extents along the TOOL axes (the body axes rotated by
    /// [`orientation`](DrillTool::orientation)). Must be positive —
    /// asserted at use (a degenerate box is a map-author bug, not a
    /// data condition).
    pub half_extents: FixedVec3,
    /// Tool-in-body rotation (digger plan §1c, D3 amendment): the box
    /// pivots about `anchor` — a pitched drill nose — composing with
    /// the body orientation in [`drill_query`]. `IDENTITY` is the P6
    /// behaviour, bit for bit. The REACTION stays orientation-agnostic
    /// (anchor point + hardness — plan §4), so only the query reads
    /// this.
    ///
    /// [`drill_query`]: crate::PhysicsWorld::drill_query
    pub orientation: FixedQuat,
}

/// One terrain cell the tool face currently overlaps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrillSample {
    pub cell: (i64, i64, i64),
    pub material: MaterialId,
}
