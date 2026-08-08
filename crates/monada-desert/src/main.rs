//! The desert game's launcher: load the bundled archive, hand the host
//! its compiled rules, and get out of the way.
//!
//! Everything that is the *game* lives in `monada-desert-rules`; this
//! binary exists because a map still ships assets, a manifest and input
//! bindings, and because a native map's rules have to be linked by
//! someone (docs/plans/desert-game.md decision L1).

use monada_desert_rules::{DesertLocal, DesertRules};
use monada_format::Map;
use monada_host::{cli, MapRun, RunConfig};

/// The archive `build.rs` packed from `map/`.
const DESERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/desert.monada"));

fn main() {
    let map = Map::read(DESERT).expect("read the bundled desert.monada");
    let config = cli::config_for_map(map);
    // The CLI parses the flags every map shares (`--listen`, `--connect`,
    // `--replay`); only the rules are ours to supply.
    let config = match config {
        RunConfig::Map { run, net } => RunConfig::Map {
            run: MapRun::native(
                run.map,
                Box::new(DesertRules::default()),
                Box::new(DesertLocal::default()),
            ),
            net,
        },
        other => other,
    };
    monada_host::run(config);
}
