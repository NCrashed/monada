//! The real-time input + terrain-collision core (action-RPG demo, M-B): a
//! fixed-rate map whose `tick` integrates a per-tick input command and keeps
//! the mover out of painted voxels via the `voxel_solid` / `ground_height`
//! queries. Proves (1) two peers fed an identical input stream stay
//! bit-identical (the lockstep contract for continuous WASD-style input),
//! and (2) the terrain query actually blocks movement — so collision is part
//! of the deterministic, hashed sim.
//!
//! Input command shape (the generic real-time primitive the host injects per
//! tick): `verb == 0`, `target` = button bitmask, `arg = vec3(move_x,
//! move_y, aim)`. Here only the move axis is exercised.

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_sim::{Command, EntityId, PlayerId};
use monada_net::SimDriver;
use monada_script::{shared_world, RhaiDriver, SharedBridge, TerrainBridge};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const P0: PlayerId = PlayerId(0);
const VERB_INPUT: u32 = 0;
const MOVER: EntityId = EntityId(0);

/// A fixed-rate mover that walks by its per-tick input intent and is stopped
/// by a wall painted at column `x == 10`.
const REALTIME_SCRIPT: &str = r#"
    fn init() {
        let a = archetype(["ix", "iy"]);
        let e = entity_create(a);
        entity_set_position(e, vec3(fixed(5), fixed(5), fixed(0)));
        // A wall: column x=10, y in 0..20, z in 0..3.
        voxel_fill(10, 0, 0, 10, 20, 3, 0x80808080);
    }

    fn command(player, verb, target, arg) {
        if verb == 0 {
            let es = entities();
            for e in es {
                entity_set_field(e, "ix", arg.x);
                entity_set_field(e, "iy", arg.y);
            }
        }
    }

    fn tick() {
        let speed = ratio(1, 2);
        let es = entities();
        for e in es {
            let p = entity_position(e);
            let ix = entity_field(e, "ix");
            let iy = entity_field(e, "iy");
            let nx = p.x + ix * speed;
            let ny = p.y + iy * speed;
            let cx = to_int(nx);
            let cy = to_int(ny);
            if !voxel_solid(cx, cy, 1) {
                let gz = ground_height(cx, cy);
                entity_set_position(e, vec3(nx, ny, fixed(gz)));
            }
        }
    }
"#;

fn driver() -> RhaiDriver {
    let bridge: SharedBridge = Arc::new(Mutex::new(TerrainBridge::new()));
    RhaiDriver::with_bridge(shared_world(SEED), REALTIME_SCRIPT, &bridge).expect("compile realtime")
}

/// Move `+x` every tick (`arg = (1, 0, 0)`, no buttons).
fn move_east() -> Command {
    Command::on(
        VERB_INPUT,
        EntityId(0),
        FixedVec3::new(Fixed::from_int(1), Fixed::ZERO, Fixed::ZERO),
    )
}

fn mover_x(d: &RhaiDriver) -> Fixed {
    d.world()
        .lock()
        .expect("world")
        .position(MOVER)
        .expect("mover exists")
        .x
}

#[test]
fn two_peers_agree_on_realtime_input_stream() {
    let mut a = driver();
    let mut b = driver();
    let cmd = move_east();

    for tick in 0..40u64 {
        // Both peers apply the same input for this tick, then step.
        a.apply_command(P0, &cmd);
        b.apply_command(P0, &cmd);
        a.step();
        b.step();
        assert_eq!(
            a.state_hash(),
            b.state_hash(),
            "peers diverged on realtime input at tick {tick}"
        );
    }
}

#[test]
fn terrain_query_blocks_movement() {
    let mut d = driver();
    let cmd = move_east();

    let start = mover_x(&d);
    assert_eq!(start, Fixed::from_int(5), "mover starts at x=5");

    for _ in 0..40 {
        d.apply_command(P0, &cmd);
        d.step();
    }

    // It walked east from 5 but the wall at x=10 stops it short — it never
    // enters the solid column.
    let end = mover_x(&d);
    assert!(end > Fixed::from_int(5), "mover advanced from its start");
    assert!(
        end < Fixed::from_int(10),
        "mover stopped before the wall at x=10 (got {end:?})"
    );
}
