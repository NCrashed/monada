//! Digger demo launcher. `cargo run -p monada-digger` loads the bundled
//! `digger.monada` archive (packed from `map/` by build.rs) and hands it
//! to the shared host CLI ([`monada_host::cli`]). The demo *map* is a
//! script only — no engine code: the core gained the generic volume
//! terrain store and the `phys_*` sim-physics verbs
//! (docs/plans/digger-demo.md §1), and this map merely uses them.

use monada_format::Map;
use monada_host::{cli, run};

/// The archive build.rs packed from `map/` into `OUT_DIR`.
const DIGGER_MONADA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/digger.monada"));

fn main() {
    let map = Map::read(DIGGER_MONADA).expect("bundled digger.monada is a valid archive");
    assert!(
        map.entry_script().is_some(),
        "digger map declares an entry script"
    );
    eprintln!(
        "monada-digger: {:?} ({} players, sim_hz {})",
        map.manifest.name, map.manifest.players, map.manifest.sim_hz,
    );
    run(cli::config_for_map(map));
}
