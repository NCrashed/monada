//! Pack `map/` into a deterministic `chess.monada` archive in `OUT_DIR`,
//! so `main.rs` can `include_bytes!` it. Walking the tree here (rather
//! than `monada_format::pack_dir`) lets us emit a `rerun-if-changed` per
//! file, so editing a script or the manifest re-bundles the archive.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let map_dir = Path::new(&manifest_dir).join("map");

    let mut files = BTreeMap::new();
    collect(&map_dir, &map_dir, &mut files);

    let bytes = monada_format::pack(&files).expect("pack chess.monada");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    fs::write(Path::new(&out_dir).join("chess.monada"), bytes).expect("write chess.monada");
}

fn collect(root: &Path, cur: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
    for entry in fs::read_dir(cur).expect("read map dir") {
        let path = entry.expect("map dir entry").path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
            let rel = path
                .strip_prefix(root)
                .expect("under map root")
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(rel, fs::read(&path).expect("read map file"));
        }
    }
}
