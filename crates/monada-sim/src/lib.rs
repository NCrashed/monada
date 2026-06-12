//! `monada-sim` — the deterministic simulation core (DESIGN.md §3.1,
//! §4, §4.1).
//!
//! This crate is the load-bearing half of monada's determinism pillar.
//! Everything here is built to produce bit-identical results on every
//! machine given identical inputs:
//!
//! - **No floats.** All spatial state is Q32.32 ([`monada_fixed`]); the
//!   crate never imports `f32`/`f64`.
//! - **No hash-order iteration.** Entities live in [`ArchetypeStorage`]
//!   sorted by a monotonic [`EntityId`]; iteration and hashing are a
//!   single fixed walk.
//! - **One seeded RNG.** [`DeterministicRng`] is advanced only inside
//!   the sim; its seed is part of the replay.
//! - **Canonical hashing.** [`StateHasher`] folds state in fixed field
//!   order for desync detection (DESIGN.md §3.1).
//!
//! The crate depends only on [`monada_fixed`] + `serde` (DESIGN.md §4).
//! [`scenario::CircleSim`] is the M0 proof-of-life that
//! `monada-oracle` golden-gates in CI.

#![forbid(unsafe_code)]
// Determinism guardrails promised by DESIGN.md §3.1/§8/§9, enforced
// here as hard errors (not just the workspace-wide warnings):
//   * no float *arithmetic* in the sim — the real cross-platform hazard
//     (x87 vs SSE2, fused fma, libm variance). There is no lint that
//     bans the f32/f64 *type*, so this is the correct mechanism.
//   * no hash-ordered containers — `disallowed-types` (clippy.toml)
//     lists them; this turns the match into an error inside the sim.
#![deny(clippy::float_arithmetic, clippy::disallowed_types)]
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
// The docs are full of prose acronyms (`SoA`) and algorithm names
// (`SplitMix64`, `xoshiro256`) that `doc_markdown` mistakes for code
// identifiers; backticking each one reads worse than the prose.
#![allow(clippy::doc_markdown)]

mod entity;
mod hash;
mod rng;
mod sim;
mod storage;
mod world;

pub use entity::{EntityAllocator, EntityId};
pub use hash::{StateHash, StateHasher};
pub use rng::DeterministicRng;
pub use sim::{advance, Simulation};
pub use storage::{ArchetypeStorage, Columns};
pub use world::{ArchetypeId, World};
