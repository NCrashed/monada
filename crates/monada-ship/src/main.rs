//! Spaceship crew-sim demo launcher. `cargo run -p monada-ship` loads the
//! bundled `ship.monada` archive (packed from `map/` by build.rs) and hands it
//! to the shared host CLI ([`monada_host::cli`]), which interprets
//! `--listen`/`--connect` (LAN co-op) and `--replay` exactly as `monada-host
//! --map` does. The demo *map* is scripts + GIF assets only — no engine code.
//!
//! What it is now: a two-deck hull on its own CUBIC voxel grid, tumbling and
//! swaying as one body while the crew walk it (WASD, view-relative), climb the
//! fore-starboard stairwell between decks, carry cargo crates (E) and cycle the
//! starboard airlock (F). Per-crew fog of war, a deck cutaway and the follow
//! camera come from docs/plans/ship-visibility.md; the cargo half — a crate
//! that rides the ship's frame until it is released through the airlock, and
//! then stays behind in space while the ship turns away — is the demo side of
//! docs/plans/grid-entities.md.

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
