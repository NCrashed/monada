//! Chess 2.0 launcher (DESIGN.md §6, §5.3). The `cargo run -p
//! monada-chess` "hello world" path: load the bundled `chess.monada`
//! archive (packed from `map/` by build.rs) and hand its script to the
//! host. The demo *map* is scripts + assets only — no engine code
//! (DESIGN.md §4); this binary is the thin entry point.

use std::net::SocketAddr;
use std::process::exit;

use monada_format::Map;
use monada_host::{run, MapRun, NetRole, RunConfig};
use monada_net::Replay;

/// The archive build.rs packed from `map/` into `OUT_DIR`.
const CHESS_MONADA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/chess.monada"));

fn main() {
    let map = Map::read(CHESS_MONADA).expect("bundled chess.monada is a valid archive");
    assert!(
        map.entry_script().is_some(),
        "chess map declares an entry script"
    );
    eprintln!(
        "monada-chess: {:?} ({} players, sim_hz {}) sha256 {}",
        map.manifest.name,
        map.manifest.players,
        map.manifest.sim_hz,
        short_hash(&map.hash),
    );
    // `--replay <file>` watches a recorded game; else `--listen`/`--connect
    // <addr>` is a LAN match and no flag is local hotseat.
    let config = match replay_arg() {
        Some(path) => RunConfig::Replay {
            replay: load_replay(&path, &map),
            run: MapRun { map },
        },
        None => RunConfig::Map {
            run: MapRun { map },
            net: parse_role(),
        },
    };
    run(config);
}

/// The `--replay <file>` path, if given.
fn replay_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--replay" {
            return Some(args.next().unwrap_or_else(|| {
                eprintln!("monada-chess: --replay needs a <file>");
                exit(2);
            }));
        }
    }
    None
}

/// Decode + verify a replay against the bundled map (map hash + version).
fn load_replay(path: &str, map: &Map) -> Replay {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("monada-chess: cannot read {path}: {e}");
        exit(2);
    });
    let replay = Replay::decode(&bytes).unwrap_or_else(|e| {
        eprintln!("monada-chess: {path}: malformed replay: {e}");
        exit(2);
    });
    if replay.map_hash != map.hash {
        eprintln!("monada-chess: {path} was recorded against a different map");
        exit(2);
    }
    let version = env!("CARGO_PKG_VERSION");
    if replay.engine_version != version {
        eprintln!(
            "monada-chess: {path} engine version {:?} != {version:?}",
            replay.engine_version
        );
        exit(2);
    }
    replay
}

/// Parse `--listen <addr>` / `--connect <addr>` (player 0 / player 1).
fn parse_role() -> Option<NetRole> {
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let listen = match flag.as_str() {
            "--listen" => true,
            "--connect" => false,
            _ => continue,
        };
        let raw = args.next().unwrap_or_else(|| {
            eprintln!("monada-chess: {flag} needs an <addr> (e.g. 127.0.0.1:5000)");
            exit(2);
        });
        let addr: SocketAddr = raw.parse().unwrap_or_else(|e| {
            eprintln!("monada-chess: {flag} {raw:?}: {e}");
            exit(2);
        });
        return Some(if listen {
            NetRole::Listen(addr)
        } else {
            NetRole::Connect(addr)
        });
    }
    None
}

/// First 6 bytes of the map hash, hex — the visible map identity.
fn short_hash(h: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(13);
    for byte in &h[..6] {
        let _ = write!(s, "{byte:02x}");
    }
    s.push('…');
    s
}
