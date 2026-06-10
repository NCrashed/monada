//! Chess 2.0 launcher (DESIGN.md §6).
//!
//! The demo *map* itself is scripts + assets only — no engine code
//! (DESIGN.md §4). This crate is just the thin `cargo run -p
//! monada-chess` entry point (the "hello world" path from §5.3) that
//! loads the bundled `chess.monada` archive and hands it to the host.
//! The map content lands under `map/` as M4 approaches.
//!
//! Skeleton only — lands at **M4** (DESIGN.md §7).

fn main() {
    eprintln!("monada-chess: not yet implemented (M4). See DESIGN.md §6, §7.");
}
