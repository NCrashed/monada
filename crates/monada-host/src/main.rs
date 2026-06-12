//! Thin CLI over the host library ([`monada_host::run`]): parse argv into
//! a [`RunConfig`] and run the event loop.
//!
//! - no args — the local walk-in-a-circle demo;
//! - `--listen <addr>` / `--connect <addr>` — a two-process lockstep match
//!   (of `command_demo`, or of a `--map` if one is given);
//! - `--map <path.monada>` — load a map archive (e.g. chess); local hotseat
//!   on its own, or a two-process match when combined with `--listen` /
//!   `--connect`. `monada-chess` is the same path with a bundled archive.

use std::net::SocketAddr;
use std::process::exit;

use monada_format::Map;
use monada_host::{run, MapRun, NetRole, RunConfig};
use monada_net::Replay;

fn main() {
    run(parse_args());
}

/// Collect `--map` + an optional `--listen`/`--connect` role; a map with a
/// role is a networked match, a map alone is hotseat, a role alone is the
/// `command_demo` net mode, and nothing is local play.
fn parse_args() -> RunConfig {
    let mut map_path: Option<String> = None;
    let mut role: Option<NetRole> = None;
    let mut replay_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--map" => map_path = Some(value(&flag, args.next())),
            "--replay" => replay_path = Some(value(&flag, args.next())),
            "--listen" => role = Some(NetRole::Listen(addr(&flag, args.next()))),
            "--connect" => role = Some(NetRole::Connect(addr(&flag, args.next()))),
            _ => {}
        }
    }
    if let Some(rp) = replay_path {
        let mp = map_path.unwrap_or_else(|| {
            eprintln!("monada-host: --replay needs --map <archive> to render against");
            exit(2);
        });
        return load_replay(&mp, &rp);
    }
    match map_path {
        Some(path) => RunConfig::Map {
            run: load_map(&path),
            net: role,
        },
        None => match role {
            Some(r) => RunConfig::Net(r),
            None => RunConfig::Local,
        },
    }
}

/// Decode a `.replay`, verify its map hash + engine version against the
/// map, and build a [`RunConfig::Replay`] — the same loud-on-mismatch check
/// `Replay::playback_verified` makes (DESIGN.md §3.4).
fn load_replay(map_path: &str, replay_path: &str) -> RunConfig {
    let run = load_map(map_path);
    let bytes = std::fs::read(replay_path).unwrap_or_else(|e| {
        eprintln!("monada-host: cannot read {replay_path}: {e}");
        exit(2);
    });
    let replay = Replay::decode(&bytes).unwrap_or_else(|e| {
        eprintln!("monada-host: {replay_path}: malformed replay: {e}");
        exit(2);
    });
    if replay.map_hash != run.map.hash {
        eprintln!("monada-host: {replay_path} was recorded against a different map");
        exit(2);
    }
    let version = env!("CARGO_PKG_VERSION");
    if replay.engine_version != version {
        eprintln!(
            "monada-host: {replay_path} engine version {:?} != {version:?}",
            replay.engine_version
        );
        exit(2);
    }
    RunConfig::Replay { run, replay }
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

/// Load a `.monada` archive into a [`MapRun`] (validated + logged).
fn load_map(path: &str) -> MapRun {
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
        "monada-host: loaded {:?} ({} players, sim_hz {}) sha256 {}",
        map.manifest.name,
        map.manifest.players,
        map.manifest.sim_hz,
        short_hash(&map.hash),
    );
    MapRun { map }
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
