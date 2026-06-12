//! Thin CLI over the host library ([`monada_host::run`]): parse argv into
//! a [`RunConfig`] and run the event loop.
//!
//! - no args — the local walk-in-a-circle demo;
//! - `--listen <addr>` / `--connect <addr>` — a two-process lockstep match;
//! - `--map <path.monada>` — load a map archive (e.g. chess) and play it
//!   locally. `monada-chess` is the same path with a bundled archive.

use std::net::SocketAddr;
use std::process::exit;

use monada_format::Map;
use monada_host::{run, MapRun, NetRole, RunConfig};

fn main() {
    run(parse_args());
}

/// First recognised flag wins; no flag is local play.
fn parse_args() -> RunConfig {
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--map" => return load_map(&value(&flag, args.next())),
            "--listen" => {
                return RunConfig::Net(NetRole::Listen(addr(&flag, args.next())));
            }
            "--connect" => {
                return RunConfig::Net(NetRole::Connect(addr(&flag, args.next())));
            }
            _ => {}
        }
    }
    RunConfig::Local
}

/// A required flag value, or a usage error.
fn value(flag: &str, v: Option<String>) -> String {
    v.unwrap_or_else(|| {
        eprintln!("monada-host: {flag} needs a value");
        exit(2);
    })
}

/// Parse a `--listen`/`--connect` socket address, or a usage error.
fn addr(flag: &str, v: Option<String>) -> SocketAddr {
    let raw = value(flag, v);
    raw.parse().unwrap_or_else(|e| {
        eprintln!("monada-host: {flag} {raw:?}: {e} (expected e.g. 127.0.0.1:5000)");
        exit(2);
    })
}

/// Load a `.monada` archive and turn its entry script into a [`RunConfig`].
fn load_map(path: &str) -> RunConfig {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("monada-host: cannot read {path}: {e}");
        exit(2);
    });
    let map = Map::read(&bytes).unwrap_or_else(|e| {
        eprintln!("monada-host: {path}: {e}");
        exit(2);
    });
    let script = map
        .entry_script()
        .unwrap_or_else(|| {
            eprintln!(
                "monada-host: {path}: entry script {:?} is not in the archive",
                map.manifest.entry
            );
            exit(2);
        })
        .to_string();
    eprintln!(
        "monada-host: loaded {:?} ({} players, sim_hz {}) sha256 {}",
        map.manifest.name,
        map.manifest.players,
        map.manifest.sim_hz,
        short_hash(&map.hash),
    );
    RunConfig::Map(MapRun {
        name: map.manifest.name,
        script,
    })
}

/// First 6 bytes of a map hash, hex — enough to eyeball the identity.
fn short_hash(h: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(13);
    for byte in &h[..6] {
        let _ = write!(s, "{byte:02x}");
    }
    s.push('…');
    s
}
