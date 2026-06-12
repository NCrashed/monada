//! monada map / mod archive format (DESIGN.md §3.4).
//!
//! A map is a single `tar.zst` archive: `manifest.toml` (engine version,
//! name, players, `sim_hz`, runtime, entry script) plus `scripts/`, and
//! later `assets/` / `audio/` / `locale/`. Map identity is the SHA-256 of
//! the **canonical (uncompressed) tar** — hashing the tar rather than the
//! zstd bytes keeps the identity stable across zstd versions, while the
//! archive on disk is still compressed. That hash rides in every replay
//! and lockstep `MatchInfo` so opening a replay against the wrong map
//! version fails loudly instead of desyncing silently.
//!
//! Determinism: [`pack`] sorts entries (a `BTreeMap`) and zeroes mtime /
//! uid / gid / fixes the mode, so the same inputs always produce the same
//! tar bytes and therefore the same hash.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::str::FromStr;
use std::{fmt, fs, io};

use serde::de::{Deserializer, Error as _};
use serde::{Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};

/// zstd compression level for packed archives. Fixed so packing is
/// reproducible; the identity hash is over the *uncompressed* tar, so the
/// level only affects file size, never the map id.
const ZSTD_LEVEL: i32 = 19;

/// SHA-256 of arbitrary bytes — the 32-byte map identity primitive
/// (DESIGN.md §3.4). Used on the canonical tar for real archives, and on
/// a raw script source for the not-yet-archived demo maps.
#[must_use]
pub fn hash(bytes: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

/// The map's tick model, declared as `sim_hz` in the manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimHz {
    /// Advance only when a command arrives — turn-based (DESIGN.md §6).
    OnCommand,
    /// Fixed simulation rate in Hz (DESIGN.md §3.1, default 25).
    Fixed(u32),
}

impl fmt::Display for SimHz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimHz::OnCommand => f.write_str("on_command"),
            SimHz::Fixed(hz) => write!(f, "{hz}"),
        }
    }
}

impl FromStr for SimHz {
    type Err = String;
    fn from_str(s: &str) -> Result<SimHz, String> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("on_command") {
            return Ok(SimHz::OnCommand);
        }
        let num = t
            .strip_suffix("hz")
            .or_else(|| t.strip_suffix("Hz"))
            .unwrap_or(t);
        num.trim()
            .parse::<u32>()
            .map(SimHz::Fixed)
            .map_err(|_| format!("sim_hz must be \"on_command\" or a Hz number, got {s:?}"))
    }
}

// `sim_hz` is a TOML string ("on_command" / "25"); (de)serialise as one.
impl Serialize for SimHz {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SimHz {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<SimHz, D::Error> {
        let s = String::deserialize(d)?;
        SimHz::from_str(&s).map_err(D::Error::custom)
    }
}

/// `manifest.toml` — the map's declared identity and runtime needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Human-readable map name.
    pub name: String,
    /// Engine version the map was authored against.
    pub engine_version: String,
    /// Player count (chess = 2).
    pub players: u32,
    /// Tick model (`"on_command"` or a Hz number).
    pub sim_hz: SimHz,
    /// Script runtime the entry targets (`"rhai"` for v0).
    pub script_runtime: String,
    /// Archive-relative path to the entry script.
    pub entry: String,
}

/// A loaded map: the parsed manifest, its script sources, and the
/// archive's SHA-256 identity. Assets land alongside scripts in a later
/// slice; for now a map is manifest + scripts.
#[derive(Clone, Debug)]
pub struct Map {
    pub manifest: Manifest,
    /// Archive-relative path -> UTF-8 script source.
    pub scripts: BTreeMap<String, String>,
    /// SHA-256 of the canonical tar (DESIGN.md §3.4) — the map identity
    /// stored in replays / lockstep `MatchInfo`.
    pub hash: [u8; 32],
}

impl Map {
    /// The entry script's source, per the manifest's `entry`.
    #[must_use]
    pub fn entry_script(&self) -> Option<&str> {
        self.scripts.get(&self.manifest.entry).map(String::as_str)
    }

    /// Read a `.monada` archive from its bytes: decompress, untar, parse
    /// `manifest.toml`, collect `scripts/`, and hash the canonical tar.
    ///
    /// # Errors
    /// [`FormatError`] on a decompress / tar / UTF-8 / manifest failure,
    /// or a missing `manifest.toml`.
    pub fn read(bytes: &[u8]) -> Result<Map, FormatError> {
        let tar = zstd::decode_all(bytes).map_err(FormatError::Decompress)?;
        let files = untar(&tar)?;

        let manifest_bytes = files.get("manifest.toml").ok_or(FormatError::NoManifest)?;
        let manifest_str =
            std::str::from_utf8(manifest_bytes).map_err(|_| FormatError::Utf8("manifest.toml"))?;
        let manifest: Manifest =
            toml::from_str(manifest_str).map_err(|e| FormatError::Manifest(e.to_string()))?;

        let mut scripts = BTreeMap::new();
        for (path, data) in &files {
            if path.starts_with("scripts/") {
                let src = String::from_utf8(data.clone())
                    .map_err(|_| FormatError::ScriptUtf8(path.clone()))?;
                scripts.insert(path.clone(), src);
            }
        }

        Ok(Map {
            manifest,
            scripts,
            hash: hash(&tar),
        })
    }
}

/// Pack archive-relative `files` into a deterministic `tar.zst`. Entries
/// are emitted in sorted order (the `BTreeMap`) with zeroed mtime / uid /
/// gid and a fixed mode, so identical inputs always yield byte-identical
/// output — and thus a stable identity [`hash`] over the inner tar.
///
/// # Errors
/// [`FormatError`] on a tar-build or compression failure.
pub fn pack(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, FormatError> {
    let mut tar = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar);
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_uid(0);
            header.set_gid(0);
            header.set_entry_type(tar::EntryType::Regular);
            builder
                .append_data(&mut header, path, data.as_slice())
                .map_err(FormatError::Tar)?;
        }
        builder.finish().map_err(FormatError::Tar)?;
    }
    zstd::encode_all(tar.as_slice(), ZSTD_LEVEL).map_err(FormatError::Compress)
}

/// Walk a directory tree into the file map [`pack`] expects: paths are
/// archive-relative with forward slashes. For build scripts that bundle a
/// `map/` directory into a `.monada` archive.
///
/// # Errors
/// [`FormatError::Io`] on a read failure, plus anything [`pack`] raises.
pub fn pack_dir(dir: &Path) -> Result<Vec<u8>, FormatError> {
    let mut files = BTreeMap::new();
    collect_dir(dir, dir, &mut files)?;
    pack(&files)
}

fn collect_dir(
    root: &Path,
    cur: &Path,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), FormatError> {
    for entry in fs::read_dir(cur).map_err(FormatError::Io)? {
        let path = entry.map_err(FormatError::Io)?.path();
        if path.is_dir() {
            collect_dir(root, &path, files)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(rel, fs::read(&path).map_err(FormatError::Io)?);
        }
    }
    Ok(())
}

/// Decompress + untar a `.monada` archive into its file map (the inverse
/// of [`pack`], without manifest parsing).
///
/// # Errors
/// [`FormatError`] on a decompress / tar failure.
pub fn unpack(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, FormatError> {
    let tar = zstd::decode_all(bytes).map_err(FormatError::Decompress)?;
    untar(&tar)
}

fn untar(tar: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, FormatError> {
    let mut archive = tar::Archive::new(tar);
    let mut files = BTreeMap::new();
    for entry in archive.entries().map_err(FormatError::Tar)? {
        let mut entry = entry.map_err(FormatError::Tar)?;
        let path = entry
            .path()
            .map_err(FormatError::Tar)?
            .to_string_lossy()
            .into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(FormatError::Io)?;
        files.insert(path, data);
    }
    Ok(files)
}

/// A failure reading or writing a map archive.
#[derive(Debug)]
pub enum FormatError {
    Io(io::Error),
    Compress(io::Error),
    Decompress(io::Error),
    Tar(io::Error),
    /// The archive has no `manifest.toml`.
    NoManifest,
    /// A file that must be UTF-8 was not (the named entry).
    Utf8(&'static str),
    /// A script entry was not valid UTF-8.
    ScriptUtf8(String),
    /// `manifest.toml` failed to parse (message).
    Manifest(String),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Io(e) => write!(f, "map archive io error: {e}"),
            FormatError::Compress(e) => write!(f, "map archive compress error: {e}"),
            FormatError::Decompress(e) => write!(f, "map archive decompress error: {e}"),
            FormatError::Tar(e) => write!(f, "map archive tar error: {e}"),
            FormatError::NoManifest => write!(f, "map archive has no manifest.toml"),
            FormatError::Utf8(what) => write!(f, "map archive {what} is not valid UTF-8"),
            FormatError::ScriptUtf8(p) => write!(f, "map script {p} is not valid UTF-8"),
            FormatError::Manifest(e) => write!(f, "map manifest parse error: {e}"),
        }
    }
}

impl std::error::Error for FormatError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BTreeMap<String, Vec<u8>> {
        let manifest = r#"
            name = "test"
            engine_version = "0.0.1"
            players = 2
            sim_hz = "on_command"
            script_runtime = "rhai"
            entry = "scripts/main.rhai"
        "#;
        let mut files = BTreeMap::new();
        files.insert("manifest.toml".to_string(), manifest.as_bytes().to_vec());
        files.insert("scripts/main.rhai".to_string(), b"fn init() {}".to_vec());
        files
    }

    #[test]
    fn round_trips_manifest_and_scripts() {
        let bytes = pack(&sample()).unwrap();
        let map = Map::read(&bytes).unwrap();
        assert_eq!(map.manifest.name, "test");
        assert_eq!(map.manifest.players, 2);
        assert_eq!(map.manifest.sim_hz, SimHz::OnCommand);
        assert_eq!(map.entry_script(), Some("fn init() {}"));
    }

    #[test]
    fn packing_is_deterministic() {
        // Same inputs -> identical archive bytes -> identical identity.
        let a = pack(&sample()).unwrap();
        let b = pack(&sample()).unwrap();
        assert_eq!(a, b, "pack must be byte-reproducible");
        assert_eq!(Map::read(&a).unwrap().hash, Map::read(&b).unwrap().hash);
    }

    #[test]
    fn hash_is_over_the_tar_not_the_zstd_frame() {
        // The identity is stable content; a different map changes it.
        let map_a = Map::read(&pack(&sample()).unwrap()).unwrap();
        let mut other = sample();
        other.insert("scripts/main.rhai".to_string(), b"fn init() { 1 }".to_vec());
        let map_b = Map::read(&pack(&other).unwrap()).unwrap();
        assert_ne!(map_a.hash, map_b.hash);
    }

    #[test]
    fn sim_hz_parses_fixed_rate() {
        assert_eq!(SimHz::from_str("25").unwrap(), SimHz::Fixed(25));
        assert_eq!(SimHz::from_str("25hz").unwrap(), SimHz::Fixed(25));
        assert_eq!(SimHz::from_str("on_command").unwrap(), SimHz::OnCommand);
        assert!(SimHz::from_str("sometimes").is_err());
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let mut files = BTreeMap::new();
        files.insert("scripts/main.rhai".to_string(), b"fn init() {}".to_vec());
        let bytes = pack(&files).unwrap();
        assert!(matches!(Map::read(&bytes), Err(FormatError::NoManifest)));
    }
}
