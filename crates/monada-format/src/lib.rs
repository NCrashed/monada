//! monada map / mod archive format (DESIGN.md §3.4).
//!
//! A map is a single `tar.zst` archive: `manifest.toml` (engine
//! version, name, players, `sim_hz`, deps) plus `scripts/`, `assets/`,
//! `audio/`, `locale/`. Map identity is the SHA-256 of the canonical-
//! serialized archive, which is part of every replay file so opening a
//! replay against the wrong map version fails loudly instead of
//! desyncing silently.
//!
//! Skeleton only — lands at **M4** (DESIGN.md §7).
#![forbid(unsafe_code)]
