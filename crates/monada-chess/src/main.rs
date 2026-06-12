//! Chess 2.0 launcher (DESIGN.md §6, §5.3). The `cargo run -p
//! monada-chess` "hello world" path: load the bundled `chess.monada`
//! archive (packed from `map/` by build.rs) and hand its script to the
//! host. The demo *map* is scripts + assets only — no engine code
//! (DESIGN.md §4); this binary is the thin entry point.

use monada_format::Map;
use monada_host::{run, MapRun, RunConfig};

/// The archive build.rs packed from `map/` into `OUT_DIR`.
const CHESS_MONADA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/chess.monada"));

fn main() {
    let map = Map::read(CHESS_MONADA).expect("bundled chess.monada is a valid archive");
    let script = map
        .entry_script()
        .expect("chess map declares an entry script")
        .to_string();
    eprintln!(
        "monada-chess: {:?} ({} players, sim_hz {}) sha256 {}",
        map.manifest.name,
        map.manifest.players,
        map.manifest.sim_hz,
        short_hash(&map.hash),
    );
    run(RunConfig::Map(MapRun {
        name: map.manifest.name,
        script,
    }));
}

/// First 6 bytes of the map hash, hex — the visible map identity.
fn short_hash(h: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(13);
    for byte in &h[..6] {
        let _ = write!(s, "{byte:02x}");
    }
    s.push('…');
    s
}
