//! The digger demo's D1 acceptance, headless (docs/plans/digger-demo.md
//! §3): load the map through the real archive path, drive it through the
//! real [`RhaiDriver`] physics seam (script `tick` → `PhysicsWorld::step`
//! against the volume terrain → combined hash), and assert the box-on-
//! wheels actually drives. The seed of the `digger@` oracle golden.

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_net::SimDriver;
use monada_physics::{BodyDef, MaterialId};
use monada_script::{
    shared_physics, shared_world, NullBridge, PhysicsSim, RhaiDriver, SharedBridge, SharedPhysics,
};
use monada_sim::{Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const SIM_HZ: u32 = 30;
const P0: PlayerId = PlayerId(0);

/// The map's entry script, through the real archive path (pack `map/`,
/// read back, take the entry).
fn script() -> String {
    let map_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack digger map");
    let map = monada_format::Map::read(&bytes).expect("read digger map");
    map.entry_script()
        .expect("digger map has an entry script")
        .to_string()
}

/// A fresh driver with the embedded physics sim — the exact construction
/// the host uses for a `terrain = "volume"` map.
fn fresh() -> (RhaiDriver, SharedPhysics) {
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let phys = shared_physics(SIM_HZ);
    let mut driver = RhaiDriver::with_physics(shared_world(SEED), &script(), &bridge, &phys)
        .expect("compile main.rhai");
    driver.set_tick_hz(SIM_HZ);
    (driver, phys)
}

/// One packed input command: drive/steer axes + brake bit (the map's
/// verb-0 contract).
fn input(drive: i32, steer: i32, brake: u64) -> Command {
    Command::on(
        0,
        EntityId(brake),
        FixedVec3::new(Fixed::from_int(drive), Fixed::from_int(steer), Fixed::ZERO),
    )
}

/// The fixed 600-tick schedule: settle → straight run → steered arc →
/// straighten → brake. KEEP IN SYNC with the oracle's `digger_input` —
/// this test asserts the behaviour the golden hashes.
fn schedule(t: u64) -> Command {
    // Identical arm bodies are distinct beats (straight run vs
    // straighten-after-the-arc), mirroring the oracle's schedule.
    #[allow(clippy::match_same_arms)]
    match t {
        0..=29 => input(0, 0, 0),
        30..=119 => input(1, 0, 0),
        120..=299 => input(1, -1, 0),
        300..=419 => input(1, 0, 0),
        _ => input(0, 0, 1),
    }
}

fn body_pos(phys: &SharedPhysics) -> FixedVec3 {
    let sim = phys.lock().expect("physics mutex");
    sim.world.bodies()[0].position()
}

#[test]
fn manifest_declares_the_volume_map() {
    let map_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack digger map");
    let map = monada_format::Map::read(&bytes).expect("read digger map");
    assert_eq!(map.manifest.terrain, monada_format::Terrain::Volume);
    assert_eq!(map.manifest.players, 1);
    assert_eq!(map.manifest.host_api, 8);
}

#[test]
fn the_vehicle_settles_drives_arcs_and_brakes() {
    let (mut driver, phys) = fresh();
    {
        let sim = phys.lock().expect("physics mutex");
        assert_eq!(sim.world.bodies().len(), 1, "init spawns the one vehicle");
        assert_eq!(sim.world.bodies()[0].wheels().len(), 4);
        assert!(!sim.terrain.is_empty(), "init painted the apron");
    }
    let start = body_pos(&phys);
    let mut settled = FixedVec3::ZERO;
    let mut arc_entry = FixedVec3::ZERO;
    for t in 1..=600u64 {
        driver.apply_command(P0, &schedule(t));
        driver.step();
        if t == 30 {
            settled = body_pos(&phys);
        }
        if t == 120 {
            arc_entry = body_pos(&phys);
        }
    }
    let end = body_pos(&phys);

    // Settled on the apron: suspension holds the CoM near spawn height.
    assert!(
        (settled.z - start.z).abs() < Fixed::ONE,
        "settle phase should not sink or launch: start z {:?}, settled z {:?}",
        start.z,
        settled.z
    );
    // The straight run drove the nose (+x) a real distance.
    assert!(
        arc_entry.x - start.x > Fixed::from_int(10),
        "straight run should cover ground: {:?} -> {:?}",
        start.x,
        arc_entry.x
    );
    // The steered arc bent the path off the start row.
    assert!(
        (end.y - start.y).abs() > Fixed::from_int(2),
        "steering should curve the track: y {:?} -> {:?}",
        start.y,
        end.y
    );
    // Braked to (nearly) a stop, still on the floor.
    let vel = {
        let sim = phys.lock().expect("physics mutex");
        sim.world.bodies()[0].linear_velocity()
    };
    assert!(
        vel.length() < Fixed::ONE,
        "the brake phase should stop the vehicle, got |v| = {:?}",
        vel.length()
    );
    assert!(end.z > Fixed::from_int(2) && end.z < Fixed::from_int(8));

    // The script mirrors the pose onto its entity every tick — one
    // physics step BEHIND by construction (per-tick order is script
    // `tick` first, then `physics.step`). So after one more tick the
    // entity carries exactly the pose we sampled at t = 600.
    driver.apply_command(P0, &schedule(601));
    driver.step();
    let entity = driver.world().lock().expect("world mutex").all_entities()[0];
    let mirrored = driver
        .world()
        .lock()
        .expect("world mutex")
        .position(entity)
        .expect("vehicle entity has a position");
    assert_eq!(mirrored, end);
}

#[test]
fn identical_runs_hash_identically() {
    // A cheap in-process determinism canary; the cross-platform gate is
    // the `digger@` oracle golden.
    let (mut a, _pa) = fresh();
    let (mut b, _pb) = fresh();
    assert_eq!(a.state_hash(), b.state_hash(), "init state diverged");
    for t in 1..=600u64 {
        a.apply_command(P0, &schedule(t));
        b.apply_command(P0, &schedule(t));
        a.step();
        b.step();
        if t % 100 == 0 || t == 600 {
            assert_eq!(a.state_hash(), b.state_hash(), "diverged at tick {t}");
        }
    }
}

#[test]
fn terrain_body_and_entity_edits_each_re_key_the_combined_hash() {
    // The D1 tripwire: all three folds — entity world, physics, volume
    // terrain — are live in the one digest.
    let (driver, phys) = fresh();
    let h0 = driver.state_hash();

    phys.lock()
        .expect("physics mutex")
        .terrain
        .set(5, 5, 10, MaterialId(0));
    let h1 = driver.state_hash();
    assert_ne!(h0, h1, "a terrain edit must re-key the combined hash");

    phys.lock()
        .expect("physics mutex")
        .world
        .spawn(&BodyDef::default());
    let h2 = driver.state_hash();
    assert_ne!(h1, h2, "a body spawn must re-key the combined hash");

    let entity = driver.world().lock().expect("world mutex").all_entities()[0];
    driver
        .world()
        .lock()
        .expect("world mutex")
        .set_position(entity, FixedVec3::new(Fixed::ONE, Fixed::ONE, Fixed::ONE));
    let h3 = driver.state_hash();
    assert_ne!(h2, h3, "an entity change must re-key the combined hash");
}

#[test]
fn snapshot_round_trip_is_bit_equal_and_steps_identically() {
    // Drive mid-scenario, snapshot the physics sim + entity world over
    // serde, and check the restored copy hashes — and STEPS — exactly
    // like the original (the physics crate's derived-cache discipline,
    // now through the full embedded stack).
    let (mut driver, phys) = fresh();
    for t in 1..=300u64 {
        driver.apply_command(P0, &schedule(t));
        driver.step();
    }

    let world_snapshot = {
        let world = driver.world().lock().expect("world mutex");
        serde_json::to_string(&*world).expect("serialize world")
    };
    let restored_world: monada_sim::World =
        serde_json::from_str(&world_snapshot).expect("deserialize world");
    assert_eq!(
        restored_world.state_hash(),
        driver.world().lock().expect("world mutex").state_hash()
    );

    let mut original: PhysicsSim = phys.lock().expect("physics mutex").clone();
    let json = serde_json::to_string(&original).expect("serialize physics sim");
    let mut restored: PhysicsSim = serde_json::from_str(&json).expect("deserialize physics sim");
    assert_eq!(original.world.state_hash(), restored.world.state_hash());
    assert_eq!(original.terrain.state_hash(), restored.terrain.state_hash());

    // Restore → step must be bit-equal to step.
    original.world.step(&original.terrain);
    restored.world.step(&restored.terrain);
    assert_eq!(original.world.state_hash(), restored.world.state_hash());
}
