//! Thin CLI over the host library ([`monada_host::run`]): parse argv into
//! a [`RunConfig`] and run the event loop.
//!
//! - no args — the local walk-in-a-circle demo;
//! - `--listen <addr>` / `--connect <addr>` — a two-process lockstep match
//!   (of `command_demo`, or of a `--map` if one is given);
//! - `--map <path.monada>` — load a map archive (e.g. chess); the map flags
//!   (`--listen`/`--connect`/`--replay`) are then handled by the shared
//!   [`monada_host::cli`] surface, the same one `monada-chess` uses.

use std::net::SocketAddr;
use std::process::exit;

use monada_format::Map;
use monada_host::{cli, run, NetRole, RunConfig};

fn main() {
    run(parse_args());
}

/// `--map` routes through the shared map CLI; otherwise `--listen` /
/// `--connect` is the `command_demo` net mode and nothing is local play.
fn parse_args() -> RunConfig {
    let mut map_path: Option<String> = None;
    let mut role: Option<NetRole> = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--map" => map_path = Some(value(&flag, args.next())),
            "--listen" => role = Some(NetRole::Listen(addr(&flag, args.next()))),
            "--connect" => role = Some(NetRole::Connect(addr(&flag, args.next()))),
            _ => {}
        }
    }
    match map_path {
        Some(path) => cli::config_for_map(load_map(&path)),
        None => match role {
            Some(r) => RunConfig::Net(r),
            None => RunConfig::Local,
        },
    }
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

/// Load + validate a `.monada` archive (logged), ready for [`cli`].
fn load_map(path: &str) -> Map {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("monada-host: cannot read {path}: {e}");
        exit(2);
    });
    let map = Map::read(&bytes).unwrap_or_else(|e| {
        eprintln!("monada-host: {path}: {e}");
        exit(2);
    });
    if map.entry_script().is_none() {
        eprintln!(
            "monada-host: {path}: entry script {:?} is not in the archive",
            map.manifest.entry
        );
        exit(2);
    }
    eprintln!(
        "monada-host: loaded {:?} ({} players, sim_hz {})",
        map.manifest.name, map.manifest.players, map.manifest.sim_hz,
    );
    map
}
