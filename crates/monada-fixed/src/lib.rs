//! `monada-fixed` — Q32.32 deterministic fixed-point arithmetic.
//!
//! Every simulation coordinate, vector, and RNG-derived quantity in
//! monada is fixed-point, never IEEE float. The reasoning (x87 80-bit
//! intermediates, fused `fma`, compiler reordering, libm `sin`/`sqrt`
//! variance) is laid out in DESIGN.md §3.1 — fixed-point sidesteps the
//! entire class of cross-platform divergence that breaks lockstep.
//!
//! ## Why Q32.32
//!
//! 32 integer bits + 32 fractional bits, stored in an `i64`. Overkill
//! for a chess board but the right floor for an RTS with sub-tile
//! precision over a multi-kilometre map: range `±2^31` with a
//! resolution of `2^-32 ≈ 2.33e-10`. The 64-bit primitive cost is
//! negligible against the cost of debugging platform divergence.
//!
//! ## Determinism contract
//!
//! - **All arithmetic is defined on overflow.** `+`, `-`, and unary
//!   `-` wrap (two's complement) rather than panicking, so debug and
//!   release builds agree bit-for-bit. `*` and `/` go through `i128`
//!   intermediates and truncate with a single, fixed rounding mode.
//!   Sim code is responsible for staying in range; see
//!   [`Fixed::checked_add`] and friends when overflow must be caught.
//! - **No float ever feeds back into a `Fixed`.** The `f32`/`f64`
//!   conversions exist for the render side of the wall (DESIGN.md
//!   §3.1) and for tests; they are not used inside the sim.
//! - **Trig is integer-only at runtime** (see [`trig`]).
//!
//! The crate is `#![no_std]` (std is pulled in only under `cfg(test)`)
//! and has no dependencies beyond an optional `serde` derive.

#![cfg_attr(not(test), no_std)]
// Q32.32 is a deliberate, audited sea of `as` casts between i64/i128
// and the float boundary; the truncation/precision pedantic lints are
// noise here and each conversion is reasoned about at its site.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::must_use_candidate,
    clippy::inline_always
)]

mod scalar;
pub mod trig;
mod vec;

pub use scalar::Fixed;
pub use vec::{FixedVec2, FixedVec3};
