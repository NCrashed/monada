//! The shared CLI surface for running a loaded map. Both `monada-host
//! --map <archive>` and `monada-chess` (a bundled archive) parse the same
//! flags through [`config_for_map`], so the option set is defined once.

use std::net::SocketAddr;
use std::process::exit;

use monada_format::Map;
use monada_net::Replay;

use crate::{MapRun, NetRole, RunConfig};

/// Build a [`RunConfig`] for an already-loaded `map` from this process's
/// argv: `--listen` / `--connect <addr>` is a two-process LAN match,
/// `--replay <file>` watches a recorded game, and no flag is a local
/// hotseat. Exits with a usage message on a malformed flag or a bad /
/// mismatched replay.
#[must_use]
pub fn config_for_map(map: Map) -> RunConfig {
    let mut role = None;
    let mut replay_path = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--listen" => role = Some(NetRole::Listen(addr(&flag, args.next()))),
            "--connect" => role = Some(NetRole::Connect(addr(&flag, args.next()))),
            "--replay" => replay_path = Some(value(&flag, args.next())),
            _ => {}
        }
    }
    if let Some(path) = replay_path {
        let replay = load_replay(&path, &map);
        return RunConfig::Replay {
            run: MapRun { map },
            replay,
        };
    }
    RunConfig::Map {
        run: MapRun { map },
        net: role,
    }
}

/// A required flag value, or a usage error.
fn value(flag: &str, v: Option<String>) -> String {
    v.unwrap_or_else(|| {
        eprintln!("monada: {flag} needs a value");
        exit(2);
    })
}

/// Parse a socket address, or a usage error.
fn addr(flag: &str, v: Option<String>) -> SocketAddr {
    let raw = value(flag, v);
    raw.parse().unwrap_or_else(|e| {
        eprintln!("monada: {flag} {raw:?}: {e} (expected e.g. 127.0.0.1:5000)");
        exit(2);
    })
}

/// Read + decode a replay and run the single determinism gate
/// ([`Replay::verify`]) against `map`, exiting loud on a wrong / mismatched
/// replay rather than letting it desync (DESIGN.md §3.4).
fn load_replay(path: &str, map: &Map) -> Replay {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("monada: cannot read {path}: {e}");
        exit(2);
    });
    let replay = Replay::decode(&bytes).unwrap_or_else(|e| {
        eprintln!("monada: {path}: malformed replay: {e}");
        exit(2);
    });
    if let Err(e) = replay.verify(map.hash, env!("CARGO_PKG_VERSION")) {
        eprintln!("monada: {path}: {e}");
        exit(2);
    }
    replay
}
