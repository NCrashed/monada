//! Engines, as a map bolts them on (docs/plans/ship-physics.md §5, S-4):
//! `phys_thrust` pushes at a point in the hull's own frame, `phys_torque`
//! turns without pushing, `phys_angvel` reads the tumble back so the map can
//! fight it.
//!
//! Nothing here is an engine-side concept. A thruster is a force with a place
//! and a direction; fuel, throttle, gimbal and which key fires it stay in the
//! map. What the engine owes is the physics: that an off-centre push turns the
//! ship, that it keeps pushing along the hull as the hull turns, and that a
//! couple turns without shoving.

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_net::SimDriver;
use monada_physics::{BodyId, RigidBody};
use monada_script::{
    shared_physics, shared_world, NullBridge, RhaiDriver, SharedBridge, SharedPhysics,
};

const SEED: u64 = 0x6D_6F_6E_61_64_61_00_04;

/// A 4×4×4 block hull at the origin in free space, plus whatever `tick` the
/// caller supplies. Mass 64 at density 1, `CoM` at (2, 2, 2) in shape cells.
fn ship(tick_body: &str) -> (RhaiDriver, SharedPhysics) {
    let script = format!(
        r"
        fn init() {{
            phys_gravity(fixed(0), fixed(0), fixed(0));
            phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));
            let s = phys_shape(4, 4, 4);
            phys_shape_fill(s, 0, 0, 0, 3, 3, 3, 0);
            phys_body(s, vec3(fixed(0), fixed(0), fixed(0)));
        }}
        fn tick() {{
            {tick_body}
        }}
    "
    );
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let phys = shared_physics(30);
    let driver = RhaiDriver::with_physics(shared_world(SEED), &script, &bridge, &phys)
        .expect("compile");
    (driver, phys)
}

fn read<R>(phys: &SharedPhysics, f: impl FnOnce(&RigidBody) -> R) -> R {
    let sim = phys.lock().expect("physics mutex");
    f(sim.world.body(BodyId(0)).expect("the hull"))
}

/// A thruster on the centreline pushes and nothing else. The `CoM` anchor is
/// the control case for every off-centre one below: if this one spun the
/// hull, the anchor rebase would be wrong and every other result meaningless.
#[test]
fn a_centred_thrust_pushes_without_turning() {
    let (mut driver, phys) = ship(
        "phys_thrust(0, vec3(fixed(2), fixed(2), fixed(2)), \
                     vec3(fixed(1), fixed(0), fixed(0)), fixed(640));",
    );
    for _ in 0..30 {
        driver.step();
    }
    let (vel, spin) = read(&phys, |b| (b.linear_velocity(), b.angular_velocity()));
    // 640 force on mass 64 for one second: 10 cells/s along +x.
    assert!(
        (vel.x.to_f64() - 10.0).abs() < 0.05,
        "a second of thrust is a known Δv, got {vel:?}"
    );
    assert!(
        vel.y.to_f64().abs() < 1e-6 && vel.z.to_f64().abs() < 1e-6,
        "and only along the axis it fired: {vel:?}"
    );
    assert!(
        spin.length().to_f64() < 1e-9,
        "a centred thrust imparts no spin, got {spin:?}"
    );
}

/// The same thruster moved off the centreline turns the ship as well as
/// pushing it — and the sign is the right-hand rule's, not a coin toss. An
/// engine mounted to starboard (+y of the `CoM`) firing forward (+x) yaws the
/// nose to port: `τ = r × F` with `r = (0, +1, 0)`, `F = (+1, 0, 0)` gives
/// `−z`.
#[test]
fn an_off_centre_thrust_turns_the_ship() {
    let (mut driver, phys) = ship(
        "phys_thrust(0, vec3(fixed(2), fixed(3), fixed(2)), \
                     vec3(fixed(1), fixed(0), fixed(0)), fixed(640));",
    );
    for _ in 0..30 {
        driver.step();
    }
    let (vel, spin) = read(&phys, |b| (b.linear_velocity(), b.angular_velocity()));
    assert!(
        vel.length().to_f64() > 5.0,
        "it still pushes hard, got {vel:?}"
    );
    assert!(
        spin.z.to_f64() < -0.1,
        "and yaws by the right-hand rule (r × F = −z), got {spin:?}"
    );
    assert!(
        spin.x.to_f64().abs() < 1e-3 && spin.y.to_f64().abs() < 1e-3,
        "about z alone — the anchor is offset in y only: {spin:?}"
    );
}

/// The property that makes a thruster a thruster rather than a world-space
/// shove: the direction is in the SHIP's frame, so it follows the hull round.
/// Without it a ship under way could only ever accelerate along the axis it
/// was built on.
///
/// The same off-centre engine shows it without any staging: the hull yaws
/// while it burns, so the velocity it piles up must LEAN THE SAME WAY as the
/// nose it is pushing along — a body-frame thrust ends up off the world +x it
/// started on, and off it in the direction the nose went.
#[test]
fn thrust_follows_the_nose_it_pushes_along() {
    let (mut driver, phys) = ship(
        "phys_thrust(0, vec3(fixed(2), fixed(3), fixed(2)), \
                     vec3(fixed(1), fixed(0), fixed(0)), fixed(640));",
    );
    for _ in 0..30 {
        driver.step();
    }
    let (vel, nose) = read(&phys, |b| {
        (
            b.linear_velocity(),
            b.orientation() * FixedVec3::new(Fixed::ONE, Fixed::ZERO, Fixed::ZERO),
        )
    });
    assert!(
        nose.y.to_f64() < -0.05,
        "the hull's nose has yawed off +x, got {nose:?}"
    );
    assert!(
        vel.y.to_f64() < -0.5,
        "and the velocity leans the SAME way — the push is in the hull's \
         frame, not the world's: {vel:?}"
    );
    // A world-frame thrust would have piled up pure +x. Pin the size of the
    // lean so "leans a little" cannot pass for "follows the nose".
    assert!(
        (vel.y.to_f64() / vel.x.to_f64()).abs() > 0.1,
        "by a real fraction of the burn, got {vel:?}"
    );
}

/// A couple turns without pushing — the gyro / RCS primitive. Off-centre
/// impulses cannot express it: they always shove as well.
#[test]
fn a_torque_turns_without_pushing() {
    let (mut driver, phys) = ship("phys_torque(0, vec3(fixed(0), fixed(0), fixed(120)));");
    for _ in 0..30 {
        driver.step();
    }
    let (vel, spin) = read(&phys, |b| (b.linear_velocity(), b.angular_velocity()));
    assert!(
        spin.z.to_f64() > 0.5,
        "the hull is turning, got {spin:?}"
    );
    assert!(
        vel.length().to_f64() < 1e-9,
        "and has not moved an inch: {vel:?}"
    );
}

/// The whole reason `phys_angvel` exists: a map writes its own stabiliser as
/// `τ = −k·ω` and the tumble dies. Three lines of Rhai, no engine knob — the
/// genre stays in the map (DESIGN.md §3.2).
#[test]
fn a_map_can_write_its_own_rcs() {
    let (mut driver, phys) = ship(
        "let w = phys_angvel(0);
         phys_torque(0, vec3(-w.x * fixed(200), -w.y * fixed(200), -w.z * fixed(200)));",
    );
    {
        let mut sim = phys.lock().expect("physics mutex");
        sim.world.apply_angular_impulse(
            BodyId(0),
            FixedVec3::new(Fixed::from_int(10), Fixed::ZERO, Fixed::from_int(30)),
        );
    }
    let before = read(&phys, RigidBody::angular_velocity).length().to_f64();
    assert!(before > 0.1, "the hull starts out tumbling: {before}");

    for _ in 0..90 {
        driver.step();
    }
    let after = read(&phys, RigidBody::angular_velocity).length().to_f64();
    assert!(
        after < before * 0.1,
        "the map's stabiliser killed the tumble: {before} → {after}"
    );
}

/// A direction of no length names no direction: ignored, not silently turned
/// into `+x`. The same contract `grid_orient` gives its axis.
#[test]
fn a_zero_direction_fires_nothing() {
    let (mut driver, phys) = ship(
        "phys_thrust(0, vec3(fixed(2), fixed(2), fixed(2)), \
                     vec3(fixed(0), fixed(0), fixed(0)), fixed(640));",
    );
    for _ in 0..10 {
        driver.step();
    }
    let vel = read(&phys, RigidBody::linear_velocity);
    assert!(
        vel.length().to_f64() < 1e-9,
        "nothing fired, got {vel:?}"
    );
}
