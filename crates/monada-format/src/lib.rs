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

/// The map's terrain model, declared as `terrain` in the manifest
/// (docs/plans/digger-demo.md §1a).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Terrain {
    /// The per-column heightmap collision store (the default — every map
    /// that predates the declaration).
    #[default]
    Column,
    /// The chunked 3D volume store: true tunnels and overhangs, plus an
    /// embedded physics sim hashed alongside the entity world. Requires
    /// a fixed `sim_hz` (physics `dt` is the tick).
    Volume,
}

/// Kind of a map-declared input action (docs/plans/input-bindings.md §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    /// A digital press/release input.
    Button,
    /// A 1D axis composed from a positive / negative input pair.
    Axis,
    /// A 2D axis composed from four directional inputs (WASD-style).
    Axis2,
}

/// An action's default binding. The shape must match the action's
/// [`ActionKind`]: `button` takes a list of input names, `axis` a
/// `{ pos, neg }` pair, `axis2` an `{ up, down, left, right }` quad.
/// Input names are winit `KeyCode` variants (`"KeyW"`, `"Space"`) or
/// `"MouseLeft"` / `"MouseRight"` / `"MouseMiddle"`; the *host*
/// validates them (the format crate stays renderer/window agnostic).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ActionDefault {
    /// `default = ["KeyQ"]` — a `button`'s input list.
    Inputs(Vec<String>),
    /// `default = { pos = "KeyE", neg = "KeyQ" }` — an `axis` pair.
    AxisPair { pos: String, neg: String },
    /// `default = { up = "KeyW", ... }` — an `axis2` quad.
    Axis2Quad {
        up: String,
        down: String,
        left: String,
        right: String,
    },
}

/// One `[[action]]` declaration: a named input the map's local script
/// layer consumes, with an optional default binding the host applies
/// under the user's `bindings.toml` overrides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionDecl {
    /// Stable id (`"cast_fireball"`) — the rebind-config / script name.
    pub id: String,
    pub kind: ActionKind,
    /// Default binding; `None` = bound only via user config.
    #[serde(default)]
    pub default: Option<ActionDefault>,
    /// Binding context. v0 accepts only `"gameplay"` (the default); the
    /// context stack (plan §3) will widen this.
    #[serde(default)]
    pub context: Option<String>,
    /// Human-readable labels by language code (`en`, `ru`, …) for the
    /// host's rebind UI. Falls back to `id` when absent.
    #[serde(default)]
    pub label: BTreeMap<String, String>,
}

impl ActionDecl {
    /// Check one declaration's internal consistency (id, context, and
    /// the default's shape against `kind`).
    fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() {
            return Err("action with empty id".into());
        }
        match &self.context {
            None => {}
            Some(c) if c == "gameplay" => {}
            Some(c) => {
                return Err(format!(
                    "action `{}`: unknown context {c:?} (v0 supports only \"gameplay\")",
                    self.id
                ));
            }
        }
        let ok = matches!(
            (self.kind, &self.default),
            (_, None)
                | (ActionKind::Button, Some(ActionDefault::Inputs(_)))
                | (ActionKind::Axis, Some(ActionDefault::AxisPair { .. }))
                | (ActionKind::Axis2, Some(ActionDefault::Axis2Quad { .. }))
        );
        if ok {
            Ok(())
        } else {
            Err(format!(
                "action `{}`: default shape does not match kind {:?}",
                self.id, self.kind
            ))
        }
    }
}

/// Serde default for [`Manifest::host_api`]: maps that predate the
/// declaration require v1. The format keeps that reading — parsing is
/// not gating — but v1 now sits below the host's supported floor
/// (`monada_script::HOST_API_OLDEST`), so such a map is refused at load
/// with a version message rather than a missing-field one.
fn default_host_api() -> u32 {
    1
}

/// `manifest.toml` — the map's declared identity and runtime needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Human-readable map name.
    pub name: String,
    /// Engine version the map was authored against.
    pub engine_version: String,
    /// Host script-API version the map's scripts require — the version
    /// of the registered function surface (`monada-script`'s
    /// `HOST_API_VERSION`), not the engine build. The host refuses to
    /// run a map whose requirement falls outside its supported range.
    /// Absent = 1, the surface as of the first declaration.
    #[serde(default = "default_host_api")]
    pub host_api: u32,
    /// Player count (chess = 2).
    pub players: u32,
    /// Tick model (`"on_command"` or a Hz number).
    pub sim_hz: SimHz,
    /// Terrain model (`"column"` or `"volume"`). Absent = column, the
    /// store every pre-volume map was written against.
    #[serde(default)]
    pub terrain: Terrain,
    /// Script runtime the entry targets (`"rhai"` for v0).
    pub script_runtime: String,
    /// Archive-relative path to the entry script.
    pub entry: String,
    /// Optional dedicated script for the local (unsynchronized) layer —
    /// input gestures, camera, UI (docs/plans/input-bindings.md §1).
    /// Absent = the local layer loads `entry` too (shared helper
    /// functions, separate global scope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_entry: Option<String>,
    /// Map-declared input actions (`[[action]]` tables — plan §2).
    /// Absent for maps that predate the binding system.
    #[serde(default, rename = "action", skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionDecl>,
}

impl Manifest {
    /// Validate the manifest's own consistency: `host_api` is a real
    /// version and the action declarations hold (unique ids, default
    /// shapes). Called by [`Map::read`]; exposed for tools that build
    /// manifests.
    ///
    /// # Errors
    /// A human-readable description of the first invalid declaration.
    pub fn validate(&self) -> Result<(), String> {
        if self.host_api == 0 {
            return Err("host_api 0 is invalid (versions start at 1)".into());
        }
        if self.terrain == Terrain::Volume && !matches!(self.sim_hz, SimHz::Fixed(_)) {
            return Err(
                "terrain = \"volume\" requires a fixed sim_hz (physics dt is the tick)".into(),
            );
        }
        for (i, action) in self.actions.iter().enumerate() {
            action.validate()?;
            if self.actions[..i].iter().any(|a| a.id == action.id) {
                return Err(format!("duplicate action id `{}`", action.id));
            }
        }
        Ok(())
    }
}

/// A loaded map: the parsed manifest, its script sources, and the
/// archive's SHA-256 identity. Assets land alongside scripts in a later
/// slice; for now a map is manifest + scripts.
#[derive(Clone, Debug)]
pub struct Map {
    pub manifest: Manifest,
    /// Archive-relative path -> UTF-8 script source (`scripts/`).
    pub scripts: BTreeMap<String, String>,
    /// Archive-relative path -> raw bytes (`assets/`: KV6 sprites, voxel
    /// worlds, … — whatever the map ships for the host to load).
    pub assets: BTreeMap<String, Vec<u8>>,
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

    /// The local layer's script source: the manifest's `local_entry` if
    /// declared, else the entry script (shared source, separate scope).
    #[must_use]
    pub fn local_script(&self) -> Option<&str> {
        match &self.manifest.local_entry {
            Some(path) => self.scripts.get(path).map(String::as_str),
            None => self.entry_script(),
        }
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
        manifest.validate().map_err(FormatError::Manifest)?;

        let mut scripts = BTreeMap::new();
        let mut assets = BTreeMap::new();
        for (path, data) in files {
            if path.starts_with("scripts/") {
                let src =
                    String::from_utf8(data).map_err(|_| FormatError::ScriptUtf8(path.clone()))?;
                scripts.insert(path, src);
            } else if path.starts_with("assets/") {
                assets.insert(path, data);
            }
        }

        Ok(Map {
            manifest,
            scripts,
            assets,
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
    fn host_api_defaults_to_v1_and_rejects_zero() {
        // A pre-declaration manifest (the `sample()` one) requires v1.
        let map = Map::read(&pack(&sample()).unwrap()).unwrap();
        assert_eq!(map.manifest.host_api, 1);
        // An explicit requirement is carried through.
        let mut files = sample();
        let declared =
            String::from_utf8(files["manifest.toml"].clone()).unwrap() + "\nhost_api = 3\n";
        files.insert("manifest.toml".to_string(), declared.into_bytes());
        let map = Map::read(&pack(&files).unwrap()).unwrap();
        assert_eq!(map.manifest.host_api, 3);
        // v0 never existed; a manifest claiming it is malformed.
        let mut files = sample();
        let zero = String::from_utf8(files["manifest.toml"].clone()).unwrap() + "\nhost_api = 0\n";
        files.insert("manifest.toml".to_string(), zero.into_bytes());
        assert!(Map::read(&pack(&files).unwrap()).is_err());
    }

    #[test]
    fn volume_terrain_requires_a_fixed_sim_hz() {
        // Absent = column, the store every pre-volume map was written
        // against.
        let map = Map::read(&pack(&sample()).unwrap()).unwrap();
        assert_eq!(map.manifest.terrain, Terrain::Column);
        // volume + on_command (the sample's tick model) is refused: the
        // embedded physics dt IS the tick.
        let mut files = sample();
        let volume =
            String::from_utf8(files["manifest.toml"].clone()).unwrap() + "\nterrain = \"volume\"\n";
        files.insert("manifest.toml".to_string(), volume.clone().into_bytes());
        assert!(Map::read(&pack(&files).unwrap()).is_err());
        // The same declaration under a fixed rate parses.
        let fixed = volume.replace("\"on_command\"", "\"30hz\"");
        files.insert("manifest.toml".to_string(), fixed.into_bytes());
        let map = Map::read(&pack(&files).unwrap()).unwrap();
        assert_eq!(map.manifest.terrain, Terrain::Volume);
        assert_eq!(map.manifest.sim_hz, SimHz::Fixed(30));
    }

    #[test]
    fn sim_hz_parses_fixed_rate() {
        assert_eq!(SimHz::from_str("25").unwrap(), SimHz::Fixed(25));
        assert_eq!(SimHz::from_str("25hz").unwrap(), SimHz::Fixed(25));
        assert_eq!(SimHz::from_str("on_command").unwrap(), SimHz::OnCommand);
        assert!(SimHz::from_str("sometimes").is_err());
    }

    #[test]
    fn assets_round_trip_as_raw_bytes() {
        let mut files = sample();
        // A KV6 sprite the map ships for model_kv6 — binary, non-UTF-8.
        let kv6 = vec![0x4b, 0x76, 0x78, 0x6c, 0x00, 0xff, 0x80, 0x01];
        files.insert("assets/pieces/king.kv6".to_string(), kv6.clone());
        let map = Map::read(&pack(&files).unwrap()).unwrap();
        assert_eq!(map.assets.get("assets/pieces/king.kv6"), Some(&kv6));
        // Assets stay out of `scripts`.
        assert!(!map.scripts.contains_key("assets/pieces/king.kv6"));
    }

    #[test]
    fn action_declarations_parse() {
        let manifest: Manifest = toml::from_str(
            r#"
            name = "t"
            engine_version = "0.1.0"
            players = 1
            sim_hz = "25"
            script_runtime = "rhai"
            entry = "scripts/main.rhai"

            [[action]]
            id = "move"
            kind = "axis2"
            default = { up = "KeyW", down = "KeyS", left = "KeyA", right = "KeyD" }
            label = { en = "Move", ru = "Движение" }

            [[action]]
            id = "cast"
            kind = "button"
            default = ["KeyQ", "MouseRight"]
            context = "gameplay"

            [[action]]
            id = "lean"
            kind = "axis"
            default = { pos = "KeyE", neg = "KeyR" }
        "#,
        )
        .unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.actions.len(), 3);
        assert_eq!(manifest.actions[0].kind, ActionKind::Axis2);
        assert_eq!(manifest.actions[0].label["ru"], "Движение");
        assert_eq!(
            manifest.actions[1].default,
            Some(ActionDefault::Inputs(vec![
                "KeyQ".into(),
                "MouseRight".into()
            ]))
        );
        assert!(matches!(
            manifest.actions[2].default,
            Some(ActionDefault::AxisPair { .. })
        ));
    }

    #[test]
    fn manifests_without_actions_stay_valid() {
        // Pre-binding maps (chess/rpg) must parse unchanged.
        let map = Map::read(&pack(&sample()).unwrap()).unwrap();
        assert!(map.manifest.actions.is_empty());
        map.manifest.validate().unwrap();
    }

    #[test]
    fn action_validation_rejects_bad_declarations() {
        let base = r#"
            name = "t"
            engine_version = "0.1.0"
            players = 1
            sim_hz = "25"
            script_runtime = "rhai"
            entry = "scripts/main.rhai"
        "#;
        let check = |extra: &str| {
            toml::from_str::<Manifest>(&format!("{base}{extra}"))
                .map_err(|e| e.to_string())
                .and_then(|m| m.validate())
        };
        // Default shape must match the kind.
        assert!(check("[[action]]\nid = \"a\"\nkind = \"axis2\"\ndefault = [\"KeyW\"]\n").is_err());
        // Duplicate ids.
        assert!(check(
            "[[action]]\nid = \"a\"\nkind = \"button\"\n[[action]]\nid = \"a\"\nkind = \"button\"\n"
        )
        .is_err());
        // Unknown context (v0 = gameplay only).
        assert!(check("[[action]]\nid = \"a\"\nkind = \"button\"\ncontext = \"menu\"\n").is_err());
        // A bad manifest fails Map::read loudly, not silently.
        let mut files = sample();
        let bad = format!(
            "{}\n[[action]]\nid = \"\"\nkind = \"button\"\n",
            String::from_utf8(files["manifest.toml"].clone()).unwrap()
        );
        files.insert("manifest.toml".to_string(), bad.into_bytes());
        assert!(matches!(
            Map::read(&pack(&files).unwrap()),
            Err(FormatError::Manifest(_))
        ));
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let mut files = BTreeMap::new();
        files.insert("scripts/main.rhai".to_string(), b"fn init() {}".to_vec());
        let bytes = pack(&files).unwrap();
        assert!(matches!(Map::read(&bytes), Err(FormatError::NoManifest)));
    }
}
