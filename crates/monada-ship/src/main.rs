//! Spaceship crew-sim demo launcher. `cargo run -p monada-ship` loads the
//! bundled `ship.monada` archive (packed from `map/` by build.rs) and hands it
//! to the shared host CLI ([`monada_host::cli`]), which interprets
//! `--listen`/`--connect` (LAN co-op) and `--replay` exactly as `monada-host
//! --map` does. The demo *map* is scripts + GIF assets only — no engine code.
//!
//! S-A is the skeleton: a hull floor, one walkable crew member (reusing the
//! RPG billboard actor), voxel collision, and a follow camera. Later slices
//! stretch the view/visibility features (wall cutout, deck clip, fog of war)
//! onto this frame — see docs/plans/ship-visibility.md.

use monada_format::Map;
use monada_host::{cli, run};

/// The archive build.rs packed from `map/` into `OUT_DIR`.
const SHIP_MONADA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ship.monada"));

fn main() {
    let map = Map::read(SHIP_MONADA).expect("bundled ship.monada is a valid archive");
    assert!(
        map.entry_script().is_some(),
        "ship map declares an entry script"
    );
    eprintln!(
        "monada-ship: {:?} ({} players, sim_hz {})",
        map.manifest.name, map.manifest.players, map.manifest.sim_hz,
    );
    run(cli::config_for_map(map));
}
