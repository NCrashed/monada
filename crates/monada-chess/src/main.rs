//! Chess 2.0 launcher (DESIGN.md §6, §5.3). The `cargo run -p
//! monada-chess` "hello world" path: load the bundled `chess.monada`
//! archive (packed from `map/` by build.rs) and hand it to the shared host
//! CLI ([`monada_host::cli`]), which interprets `--listen`/`--connect`
//! (LAN) and `--replay` exactly as `monada-host --map` does. The demo *map*
//! is scripts + assets only — no engine code (DESIGN.md §4).

use monada_format::Map;
use monada_host::{cli, run};

/// The archive build.rs packed from `map/` into `OUT_DIR`.
const CHESS_MONADA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/chess.monada"));

fn main() {
    let map = Map::read(CHESS_MONADA).expect("bundled chess.monada is a valid archive");
    assert!(
        map.entry_script().is_some(),
        "chess map declares an entry script"
    );
    eprintln!(
        "monada-chess: {:?} ({} players, sim_hz {})",
        map.manifest.name, map.manifest.players, map.manifest.sim_hz,
    );
    run(cli::config_for_map(map));
}
