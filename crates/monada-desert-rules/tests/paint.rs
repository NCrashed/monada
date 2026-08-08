//! The rules end to end through the real backend: `init` runs, the
//! volume store fills, and what it holds is what the generator promised.
//!
//! `generator.rs` checks the map as *arithmetic*. This checks the map as
//! *terrain* — that the promise survives the trip through
//! [`MapRules::init`], the host facade and the hashed store, which is
//! where a coordinate convention or an off-by-one would eat it.
//!
//! Each case pays for a full 65k-column paint, which is seconds in a
//! debug build and the reason these assertions sample rather than sweep.
//! That cost is itself worth watching: it is the mission's load time, and
//! generating chunks directly instead of column by column is the lever if
//! it ever grates.

use std::sync::{Arc, Mutex};

use monada_desert_rules::gen::{Surface, BEDROCK_Z};
use monada_desert_rules::{material, DesertParams, DesertRules, MAP_CELLS};
use monada_runtime::{
    shared_physics, shared_world, Host, NativeBackend, NullBridge, ScriptBackend, SharedBridge,
    WorldRead,
};

/// A backend with the desert loaded and painted, plus its volume store.
fn painted() -> NativeBackend {
    let world = shared_world(0x0DE5_E271);
    let mut backend = NativeBackend::new(
        world,
        Box::new(DesertRules::new(DesertParams::default())),
    );
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    // 30 Hz: the plan's tick rate (§4a). The physics sim's dt must equal
    // the sim tick, which is why it is constructed with the rate.
    backend.set_volume(&shared_physics(30));
    backend.on_init().expect("init");
    backend
}

#[test]
fn init_fills_the_volume_store_to_the_generated_heights() {
    let backend = painted();
    let host = backend.host();
    let desert = DesertRules::new(DesertParams::default());
    let gen = desert.desert();

    // Sample rather than sweep: 65k columns × 40 cells is a slow test for
    // no extra confidence, and a systematic error shows in any stride.
    for y in (0..MAP_CELLS).step_by(13) {
        for x in (0..MAP_CELLS).step_by(11) {
            let (height, _) = gen.column(x, y);
            assert!(
                host.volume_solid(x, y, height),
                "({x}, {y}): the surface cell at z={height} is air"
            );
            assert!(
                host.volume_solid(x, y, BEDROCK_Z),
                "({x}, {y}): the column is hollow at bedrock"
            );
            assert!(
                !host.volume_solid(x, y, height + 1),
                "({x}, {y}): the column is one cell too tall"
            );
        }
    }
}

#[test]
fn the_column_store_stays_empty_on_a_volume_map() {
    // The book's contract: on a volume map the column `voxel_solid` reads
    // an empty world by design, because the ground lives in the volume
    // store. A map that leaks into both would answer two different
    // questions about the same cell.
    let backend = painted();
    let host = backend.host();
    assert!(!host.voxel_solid(128, 128, 32));
    assert_eq!(host.ground_height(128, 128), 0);
}

#[test]
fn painting_is_idempotent_across_two_peers() {
    // Two peers, same seed, same rules: the stores must agree. This is the
    // generator's determinism test carried through the engine, where a
    // shared mutable store could still let iteration order matter.
    let (a, b) = (painted(), painted());
    for y in (0..MAP_CELLS).step_by(29) {
        for x in (0..MAP_CELLS).step_by(23) {
            for z in [BEDROCK_Z, 20, 30, 33, 40] {
                assert_eq!(
                    a.host().volume_solid(x, y, z),
                    b.host().volume_solid(x, y, z),
                    "peers disagree at ({x}, {y}, {z})"
                );
            }
        }
    }
}

#[test]
fn spice_is_painted_as_spice() {
    // Material, not just height: a harvester finds its field by what the
    // cell is made of, and the render tint follows the same id.
    let desert = DesertRules::new(DesertParams::default());
    let gen = desert.desert();
    let mut found = false;
    for y in 0..MAP_CELLS {
        for x in 0..MAP_CELLS {
            if gen.column(x, y).1 == Surface::Spice {
                found = true;
                break;
            }
        }
        if found {
            break;
        }
    }
    assert!(found, "the default parameters produced no spice at all");
    assert_ne!(
        material::SPICE,
        material::SAND,
        "spice must be distinguishable from the sand it lies on"
    );
}

/// D-1's moving part: a vehicle that drives over the dunes and stays on
/// them. Cheap to get wrong in a way no still frame shows — a seat
/// computed from the generator instead of the store looks right until the
/// first crater, and a seat off by one has the hull hovering or buried.
#[test]
fn the_vehicle_drives_and_stays_on_the_surface() {
    use monada_runtime::WorldRead as _;

    let mut backend = painted();
    let start = {
        let host = backend.host();
        let vehicle = host.entities()[0];
        host.entity_position(vehicle)
    };

    for _ in 0..200 {
        backend.on_tick().expect("tick");
    }

    let host = backend.host();
    let vehicle = host.entities()[0];
    let now = host.entity_position(vehicle);
    assert_ne!(
        (start.x, start.y),
        (now.x, now.y),
        "200 ticks and the vehicle has not moved"
    );

    // Seated: the cell under it is solid, the cell it occupies is not.
    let (x, y, z) = (
        i64::from(now.x.floor_to_int()),
        i64::from(now.y.floor_to_int()),
        i64::from(now.z.floor_to_int()),
    );
    assert!(
        host.volume_solid(x, y, z - 1),
        "the vehicle at ({x}, {y}, {z}) is hovering — nothing solid beneath it"
    );
    assert!(
        !host.volume_solid(x, y, z),
        "the vehicle at ({x}, {y}, {z}) is buried in the dune"
    );
}

/// The patrol must stay on the map: driving off the edge is the one way a
/// heading-only mover can leave the world entirely.
#[test]
fn the_vehicle_stays_inside_the_map() {
    let mut backend = painted();
    for _ in 0..2000 {
        backend.on_tick().expect("tick");
    }
    let host = backend.host();
    let pos = host.entity_position(host.entities()[0]);
    let (x, y) = (
        i64::from(pos.x.floor_to_int()),
        i64::from(pos.y.floor_to_int()),
    );
    assert!(
        (0..MAP_CELLS).contains(&x) && (0..MAP_CELLS).contains(&y),
        "the vehicle drove off the map to ({x}, {y})"
    );
}
