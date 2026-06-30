//! action-RPG demo launcher. `cargo run -p monada-rpg` loads the bundled
//! `rpg.monada` archive (packed from `map/` by build.rs) and hands it to
//! the shared host CLI ([`monada_host::cli`]), which interprets
//! `--listen`/`--connect` (LAN co-op) and `--replay` exactly as `monada-host
//! --map` does. The demo *map* is scripts + GIF assets only — no engine code:
//! the core gained only generic real-time primitives (fixed-rate tick,
//! per-tick input, voxel queries, animated billboard actors).

use monada_format::Map;
use monada_host::{cli, run};

/// The archive build.rs packed from `map/` into `OUT_DIR`.
const RPG_MONADA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rpg.monada"));

fn main() {
    let map = Map::read(RPG_MONADA).expect("bundled rpg.monada is a valid archive");
    assert!(
        map.entry_script().is_some(),
        "rpg map declares an entry script"
    );
    eprintln!(
        "monada-rpg: {:?} ({} players, sim_hz {})",
        map.manifest.name, map.manifest.players, map.manifest.sim_hz,
    );
    run(cli::config_for_map(map));
}
