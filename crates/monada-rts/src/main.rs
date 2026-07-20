//! RTS demo launcher. `cargo run -p monada-rts` loads the bundled
//! `rts.monada` archive (packed from `map/` by build.rs) and hands it to the
//! shared host CLI ([`monada_host::cli`]), which interprets
//! `--listen`/`--connect` (LAN 1v1) and `--replay` exactly as `monada-host
//! --map` does. The demo *map* is scripts + GIF assets only — no engine code.
//!
//! R-A is the skeleton: a flat grass field, a handful of workers per player,
//! click-select + right-click move (straight-line — pathfinding lands in
//! R-B), and a free WC3-style pan/zoom camera. Later slices stretch terrain
//! levels, navigation, economy and combat onto this frame — see
//! docs/plans/rts-demo.md.

use monada_format::Map;
use monada_host::{cli, run};

/// The archive build.rs packed from `map/` into `OUT_DIR`.
const RTS_MONADA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rts.monada"));

fn main() {
    let map = Map::read(RTS_MONADA).expect("bundled rts.monada is a valid archive");
    assert!(
        map.entry_script().is_some(),
        "rts map declares an entry script"
    );
    eprintln!(
        "monada-rts: {:?} ({} players, sim_hz {})",
        map.manifest.name, map.manifest.players, map.manifest.sim_hz,
    );
    run(cli::config_for_map(map));
}
