//! monada scripting runtime and the engine-side API surface scripts
//! call into (DESIGN.md §3.3, §5).
//!
//! This crate is the **strict wall** between script-language types and
//! sim types: it is the only place where a `ScriptBackend` (Rhai in
//! v0, WASM post-v0) is allowed to touch `monada-sim`. The runtime is
//! swappable behind the `ScriptBackend` trait so the Rhai -> WASM
//! migration (§5.5) does not cascade into engine code.
//!
//! Skeleton only — lands at **M2** (DESIGN.md §7).
#![forbid(unsafe_code)]
