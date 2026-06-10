//! Voxel-native physics (DESIGN.md §3.6).
//!
//! Rigid bodies are voxel grids with a fixed-point centre-of-mass,
//! mass tensor, and linear/angular velocity; collision is voxel-vs-
//! voxel grid intersection; destruction carves voxels via roxlap's
//! edit API. All arithmetic is fixed-point (`monada-fixed`).
//!
//! Present in the workspace from M0 as an (almost) empty crate so the
//! sim's archetype design does not foreclose it. Implementation
//! begins no earlier than **M7** (DESIGN.md §7).
#![forbid(unsafe_code)]
