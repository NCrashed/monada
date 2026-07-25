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
/// verb-0 contract): drive/steer/pitch axes, brake bit 0, drill bit 1.
fn input(drive: i32, steer: i32, pitch: i32, brake: u64, drill: u64) -> Command {
    Command::on(
        0,
        EntityId(brake | (drill << 1)),
        FixedVec3::new(
            Fixed::from_int(drive),
            Fixed::from_int(steer),
            Fixed::from_int(pitch),
        ),
    )
}

/// The fixed 900-tick golden schedule: settle → a steer S at low speed →
/// straight run over the jump ramp (launch ~t165, land ~t210) → brake at
/// the mountain → bore level through the granite vein into the crystal
/// chamber → pitch down ON THE MOVE → ride the descending bore through
/// the apron slab into the basement vault → brake underground. KEEP IN
/// SYNC with the oracle's `digger_input` — this test asserts the
/// behaviour the golden hashes.
fn schedule(t: u64) -> Command {
    // Identical arm bodies are distinct beats, mirroring the oracle's
    // schedule.
    #[allow(clippy::match_same_arms)]
    match t {
        0..=29 => input(0, 0, 0, 0, 0),
        // +1 = steer RIGHT (the screen convention; the script negates
        // into physics yaw).
        30..=35 => input(1, 1, 0, 0, 0),
        36..=41 => input(1, -1, 0, 0, 0),
        42..=209 => input(1, 0, 0, 0, 0),
        210..=259 => input(0, 0, 0, 1, 0),
        260..=474 => input(1, 0, 0, 0, 1),
        475..=489 => input(1, 0, -1, 0, 1),
        490..=599 => input(1, 0, 0, 0, 1),
        600..=819 => input(1, 0, 0, 0, 0),
        _ => input(0, 0, 0, 1, 0),
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
    assert_eq!(map.manifest.host_api, 10);
    // The oracle's digger golden hardcodes the tick rate (it embeds the
    // script via include_str!, not the archive) — this pin is what
    // catches a manifest sim_hz change before the golden silently runs
    // the map at the wrong rate.
    assert_eq!(map.manifest.sim_hz, monada_format::SimHz::Fixed(SIM_HZ));
}

#[test]
#[allow(clippy::too_many_lines)] // one linear story, one assert per beat
fn the_vehicle_jumps_bores_the_mountain_and_descends_to_the_vault() {
    let (mut driver, phys) = fresh();
    {
        let sim = phys.lock().expect("physics mutex");
        assert_eq!(sim.world.bodies().len(), 1, "init spawns the one vehicle");
        assert_eq!(sim.world.bodies()[0].wheels().len(), 4);
        assert!(!sim.terrain.is_empty(), "init painted the apron");
        // The mountain face stands where the golden will bore.
        assert!(
            sim.terrain.get(125, 120, 4).is_some(),
            "mountain sandstone at (125, 120, 4)"
        );
    }
    let start = body_pos(&phys);
    let mut settled = FixedVec3::ZERO;
    let mut apex = Fixed::ZERO;
    for t in 1..=900u64 {
        driver.apply_command(P0, &schedule(t));
        driver.step();
        if t == 30 {
            settled = body_pos(&phys);
        }
        // The jump-ramp flight window: past the launch lip, before landing.
        if (160..=205).contains(&t) {
            apex = apex.max(body_pos(&phys).z);
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
    // The jump: the ramp launches the vehicle well above its ride height
    // (CoM ~4.5 on the flat, ramp top surface at z 6) — ballistic flight
    // through the full engine stack.
    assert!(
        apex > Fixed::from_int(8),
        "the ramp should launch the vehicle: flight apex z {apex:?}"
    );
    // The bore: the mountain face cell the vehicle drilled through is
    // carved out of the HASHED volume store.
    {
        let sim = phys.lock().expect("physics mutex");
        assert!(
            sim.terrain.get(125, 120, 4).is_none(),
            "the level bore should carve the mountain face at (125, 120, 4)"
        );
    }
    // The pitched descent: parked INSIDE the basement vault, underground.
    assert!(
        end.z < Fixed::ZERO,
        "the descent should end underground, got z {:?}",
        end.z
    );
    assert!(
        end.x > Fixed::from_int(128) && end.x < Fixed::from_int(157),
        "parked inside the vault x-range, got x {:?}",
        end.x
    );
    assert!(
        end.y > Fixed::from_int(110) && end.y < Fixed::from_int(127),
        "parked inside the vault y-range, got y {:?}",
        end.y
    );
    // The drill fed the score (the D4 HUD's "bite" number).
    let entity = driver.world().lock().expect("world mutex").all_entities()[0];
    let score = driver
        .world()
        .lock()
        .expect("world mutex")
        .field(entity, "score")
        .expect("score field");
    assert!(
        score > Fixed::from_int(400),
        "the bore should cut hundreds of voxels, scored {score:?}"
    );
    // The objective (D4): the golden's route touches the chamber crystal
    // (during the bore) and the vault crystal (at the arrival) — two of
    // three collected, their entities despawned; the ramp-top crystal
    // stays for the human.
    let found = driver
        .world()
        .lock()
        .expect("world mutex")
        .field(entity, "found")
        .expect("found field");
    assert_eq!(
        found,
        Fixed::from_int(2),
        "the golden route collects the chamber + vault crystals"
    );
    assert_eq!(
        driver.world().lock().expect("world mutex").all_entities().len(),
        2,
        "vehicle + the one uncollected crystal remain"
    );
    // Braked to (nearly) a stop in the vault.
    let vel = {
        let sim = phys.lock().expect("physics mutex");
        sim.world.bodies()[0].linear_velocity()
    };
    assert!(
        vel.length() < Fixed::ONE,
        "the brake phase should stop the vehicle, got |v| = {:?}",
        vel.length()
    );

    // The script mirrors the pose onto its entity every tick — one
    // physics step BEHIND by construction (per-tick order is script
    // `tick` first, then `physics.step`). So after one more tick the
    // entity carries exactly the pose we sampled at t = 900.
    driver.apply_command(P0, &schedule(901));
    driver.step();
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
    for t in 1..=900u64 {
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

#[test]
fn a_pitched_up_bore_climbs_back_toward_the_surface() {
    // The other half of the D3 pitch acceptance: from the level bore, an
    // UP-pitched drill cuts an ascending shaft the vehicle climbs — z
    // rises well above the tunnel level before the schedule ends.
    let (mut driver, phys) = fresh();
    let mut apex = Fixed::ZERO;
    for t in 1..=780u64 {
        // Identical arm bodies are distinct beats (level bore vs the
        // post-nudge ascending bore).
        #[allow(clippy::match_same_arms)]
        let cmd = match t {
            0..=29 => input(0, 0, 0, 0, 0),
            30..=209 => input(1, 0, 0, 0, 0),
            210..=259 => input(0, 0, 0, 1, 0),
            260..=474 => input(1, 0, 0, 0, 1),
            475..=489 => input(1, 0, 1, 0, 1),
            _ => input(1, 0, 0, 0, 1),
        };
        driver.apply_command(P0, &cmd);
        driver.step();
        if t > 490 {
            apex = apex.max(body_pos(&phys).z);
        }
    }
    assert!(
        apex > Fixed::from_int(8),
        "the ascending bore should climb the vehicle well above the \
         tunnel level (z ~4.7), got apex {apex:?}"
    );
}
