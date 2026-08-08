//! The bundled archive, read back the way the launcher reads it.
//!
//! A native map is the first kind whose archive carries **no script**, so
//! this pins the parts that are easy to get wrong once `entry` stops
//! being compiled: the manifest still has to declare what the host needs
//! (a fixed rate, volume terrain, a host-API range this build honours),
//! and the archive still has to load without the entry file existing.

use monada_format::{Map, SimHz, Terrain};

const DESERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/desert.monada"));

fn desert() -> Map {
    Map::read(DESERT).expect("the bundled archive reads")
}

#[test]
fn the_archive_loads_without_an_entry_script() {
    let map = desert();
    assert_eq!(
        map.entry_script(),
        None,
        "a native map compiles nothing — the rules are linked"
    );
}

#[test]
fn the_manifest_declares_what_a_volume_map_must() {
    let map = desert();
    assert_eq!(map.manifest.terrain, Terrain::Volume);
    assert_eq!(map.manifest.script_runtime, "native");
    // A volume map needs a fixed rate: the physics dt IS the tick, which
    // `Manifest::validate` enforces and this restates as intent.
    assert!(matches!(map.manifest.sim_hz, SimHz::Fixed(30)));
}

#[test]
fn this_build_can_run_the_map_it_ships() {
    let map = desert();
    monada_script::check_host_api(map.manifest.host_api)
        .expect("the launcher ships a map its own host supports");
}
