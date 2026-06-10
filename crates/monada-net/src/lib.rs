//! monada lockstep transport (DESIGN.md §3.1, §3.4).
//!
//! Per-tick input bundling, command-delay scheduling, desync-hash
//! exchange, reconnect, spectators. `tokio` + `quinn` (QUIC) on
//! native; WebTransport (WebSocket fallback) on wasm. Only inputs
//! travel the wire — never state.
//!
//! Skeleton only — lands at **M3** (DESIGN.md §7).
#![forbid(unsafe_code)]
