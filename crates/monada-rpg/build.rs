//! Pack `map/` into a deterministic `rpg.monada` archive in `OUT_DIR`,
//! so `main.rs` can `include_bytes!` it. Walking the tree here emits a
//! `rerun-if-changed` per file, so editing a script / asset re-bundles.
//! (Identical to monada-chess's build.rs but for `rpg.monada`.)

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let map_dir = Path::new(&manifest_dir).join("map");
    // Watch the tree itself so newly-added assets (e.g. generated GIFs)
    // re-trigger the pack, not just edits to files that already existed.
    println!("cargo:rerun-if-changed={}", map_dir.display());

    let mut files = BTreeMap::new();
    collect(&map_dir, &map_dir, &mut files);

    let bytes = monada_format::pack(&files).expect("pack rpg.monada");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    fs::write(Path::new(&out_dir).join("rpg.monada"), bytes).expect("write rpg.monada");
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
