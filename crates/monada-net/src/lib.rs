//! monada lockstep transport (DESIGN.md §3.1, §3.4, M3).
//!
//! Per-tick input bundling, command-delay scheduling, the tick barrier,
//! desync-hash exchange, and replays. Only inputs travel the wire —
//! never state ([`wire`]). The deterministic core here is **sync and
//! transport-agnostic**: [`LockstepSession`] runs over any [`Transport`]
//! and drives any [`SimDriver`], so the same loop serves the headless
//! oracle (over [`LoopbackTransport`]) and the host (over QUIC, M3 Phase
//! B). The crate is **sim-only** — it never links rhai, keeping the
//! script<->sim wall intact; the script layer plugs in via [`SimDriver`].
//!
//! Reconnect, spectators, and the QUIC transport itself layer on next.
#![forbid(unsafe_code)]
// The docs cite prose acronyms / game names (`AoE2`, `QUIC`) that
// `doc_markdown` mistakes for code identifiers; backticking each reads
// worse than the prose (matches `monada-sim`'s stance).
#![allow(clippy::doc_markdown)]

mod lockstep;
#[cfg(feature = "quic")]
mod quic;
mod replay;
mod session;
mod transport;
mod wire;

pub use lockstep::Lockstep;
pub use replay::{Replay, ReplayError};
pub use session::{Desync, LockstepSession, MatchInfo, SessionConfig, SimDriver};
pub use transport::{LoopbackTransport, Transport};
pub use wire::{decode, encode, Checksum, InputBundle, NetMessage};

#[cfg(feature = "quic")]
pub use quic::{QuicError, QuicTransport};
