//! monada render bridge: walks deterministic sim state each frame and
//! computes the `roxlap_scene::Scene` for that frame, interpolating
//! between the last two sim ticks (DESIGN.md §3.2).
//!
//! This crate sits on the render side of the sim/render wall: the
//! fixed-point (`monada-fixed`) -> float conversion happens here, and
//! sim state is read-only from this side. Owns camera modes, picking,
//! and the egui HUD compositor over the roxlap framebuffer.
//!
//! Skeleton only — lands at **M1** (DESIGN.md §7).
#![forbid(unsafe_code)]
