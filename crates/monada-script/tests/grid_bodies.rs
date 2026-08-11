//! A grid driven by a rigid body (`grid_body`, docs/plans/ship-physics.md
//! S-3): the frame a map used to compute becomes one the physics produces,
//! and everything that rides the frame follows without knowing.
//!
//! These run the REAL tick order through `RhaiDriver` — script `tick`, then
//! the physics step, then the pose sync — because the order is the contract.
//! A sync that ran before the step would hand the drawn world a hull pose one
//! tick stale, and nothing in the frame math would notice.

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_net::SimDriver;
use monada_physics::BodyId;
use monada_script::{shared_physics, shared_world, NullBridge, RhaiDriver, SharedBridge};

const SEED: u64 = 0x6D_6F_6E_61_64_61_00_03;
/// Fixed-point poses agree to rounding, not bit-exactly.
const EPS: f64 = 1e-6;

/// The ship demo in miniature: a cubic hull grid, a shell body of the same
/// cells, the two bound, and a crew member riding the hull at its centre of
/// mass. The rider is the point of the whole slice — its position is
/// hull-local and never changes, so wherever it is DRAWN is entirely the
/// frame's doing.
const HULL: &str = r"
fn init() {
    archetype([]);
    phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));

    let grid = grid_spawn_cubic(0, 0, 0);
    let shape = phys_shape(20, 20, 6);
    phys_shape_fill(shape, 0, 0, 0, 19, 19, 5, 0);
    phys_shape_clear(shape, 1, 1, 1, 18, 18, 4);
    let body = phys_body(shape, vec3(fixed(0), fixed(0), fixed(0)));
    grid_body(grid, body);

    // A crew member standing on the hull's centre of mass.
    let c = entity_create(0);
    entity_set_position(c, vec3(fixed(10), fixed(10), fixed(3)));
    entity_set_grid(c, grid);
}
";

fn driver(script: &str) -> (RhaiDriver, monada_script::SharedPhysics) {
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let phys = shared_physics(30);
    let driver = RhaiDriver::with_physics(shared_world(SEED), script, &bridge, &phys)
        .expect("compile");
    (driver, phys)
}

/// Shove the hull off-centre so it both translates and TUMBLES — the case a
/// map cannot yet produce for itself (`phys_thrust` lands at S-4), and the
/// only one that tells a right frame from a nearly-right one.
fn kick(phys: &monada_script::SharedPhysics) {
    phys.lock()
        .expect("physics mutex")
        .world
        .apply_impulse_at(
            BodyId(0),
            FixedVec3::new(Fixed::from_int(20_000), Fixed::ZERO, Fixed::from_int(6_000)),
            FixedVec3::new(Fixed::from_int(4), Fixed::from_int(-6), Fixed::from_int(2)),
        );
}

fn body_pose(phys: &monada_script::SharedPhysics) -> (FixedVec3, monada_fixed::FixedQuat) {
    let sim = phys.lock().expect("physics mutex");
    let b = sim.world.body(BodyId(0)).expect("the hull body");
    (b.position(), b.orientation())
}

fn close(a: FixedVec3, b: FixedVec3) -> bool {
    (a.x.to_f64() - b.x.to_f64()).abs() < EPS
        && (a.y.to_f64() - b.y.to_f64()).abs() < EPS
        && (a.z.to_f64() - b.z.to_f64()).abs() < EPS
}

/// The frame follows the body, every tick, in both halves of a pose. The
/// decisive assertion is the RIDER: a crew member standing on the hull's
/// centre of mass is drawn exactly at the body's position, however the hull
/// tumbles — which is only true if origin, pivot and rotation all agree.
#[test]
fn a_bound_grid_rides_its_body() {
    let (mut driver, phys) = driver(HULL);
    kick(&phys);

    for tick in 1..=30 {
        driver.step();
        let (position, orientation) = body_pose(&phys);
        let grids = driver.grids().lock().expect("grids mutex");

        // The pivot is the CoM, so the frame's origin is `position − pivot`.
        assert!(
            close(grids.origin(0), position - grids.pivot(0)),
            "tick {tick}: the frame's origin trails the body's CoM"
        );
        assert_eq!(
            grids.rotation(0),
            orientation.normalize(),
            "tick {tick}: the frame carries the body's attitude"
        );

        // …and therefore the rider on the CoM never leaves the body's
        // position, at any attitude.
        let com = FixedVec3::new(Fixed::from_int(10), Fixed::from_int(10), Fixed::from_int(3));
        assert!(
            close(grids.to_world(0, com), position),
            "tick {tick}: the rider on the centre of mass rides it exactly"
        );
    }

    // The kick did move it — otherwise every assertion above is vacuous.
    let (position, orientation) = body_pose(&phys);
    assert!(
        position.x.to_f64().abs() > 1.0,
        "the hull travelled, got {position:?}"
    );
    assert!(
        (orientation.w.to_f64().abs() - 1.0).abs() > 1e-4,
        "and tumbled, got {orientation:?}"
    );
}

/// The pivot a binding sets is the body's derived centre of mass (D3), so a
/// map can never let its hand-authored pivot drift from the point the
/// dynamics actually turn about. For this symmetric shell that is the middle
/// of its 20×20×6 cells.
#[test]
fn binding_pivots_the_grid_on_the_centre_of_mass() {
    let (driver, _phys) = driver(HULL);
    let grids = driver.grids().lock().expect("grids mutex");
    let pivot = grids.pivot(0);
    assert!(
        (pivot.x.to_f64() - 10.0).abs() < EPS
            && (pivot.y.to_f64() - 10.0).abs() < EPS
            && (pivot.z.to_f64() - 3.0).abs() < EPS,
        "the pivot is the shell's CoM, got {pivot:?}"
    );
    assert_eq!(grids.body_of(0), 0, "and the grid reads back its body");
}

/// Binding poses the grid immediately, not on the next tick: a hull bound
/// during `init` would otherwise sit at its spawn origin for one frame, which
/// is one visible frame of the ship in the wrong place.
#[test]
fn binding_poses_the_grid_at_once() {
    let (driver, _phys) = driver(
        r"
        fn init() {
            phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));
            let grid = grid_spawn_cubic(0, 0, 0);
            let shape = phys_shape(4, 4, 4);
            phys_shape_fill(shape, 0, 0, 0, 3, 3, 3, 0);
            let body = phys_body(shape, vec3(fixed(50), fixed(0), fixed(0)));
            grid_body(grid, body);
        }
    ",
    );
    let grids = driver.grids().lock().expect("grids mutex");
    // CoM at the 4³ block's middle (2, 2, 2), body placed at x = 50.
    let centre = FixedVec3::new(Fixed::from_int(2), Fixed::from_int(2), Fixed::from_int(2));
    assert!(
        close(
            grids.to_world(0, centre),
            FixedVec3::new(Fixed::from_int(50), Fixed::ZERO, Fixed::ZERO)
        ),
        "the hull is already at its body's place, got {:?}",
        grids.to_world(0, centre)
    );
}

/// Releasing a grid (`grid_body(g, -1)`) hands the frame back to the map: the
/// body keeps moving, the hull holds still. A docked shuttle whose engines
/// are cut should stop being driven, not keep drifting off its clamps.
#[test]
fn releasing_a_grid_stops_the_sync() {
    let (mut driver, phys) = driver(HULL);
    kick(&phys);
    driver.step();
    let moved = driver.grids().lock().expect("grids mutex").origin(0);

    driver
        .grids()
        .lock()
        .expect("grids mutex")
        .bind_body(0, -1);
    for _ in 0..10 {
        driver.step();
    }
    let held = driver.grids().lock().expect("grids mutex").origin(0);
    assert!(close(held, moved), "a released frame holds: {held:?}");
    let (position, _) = body_pose(&phys);
    assert!(
        (position.x.to_f64() - held.x.to_f64()).abs() > 1.0,
        "while the body it left keeps going"
    );
}

/// A hull with no body is still the map's to pose: `grid_move` / `grid_orient`
/// keep working exactly as they did, which is what makes the whole binding
/// additive rather than a change of meaning.
#[test]
fn an_unbound_grid_is_still_the_maps_to_pose() {
    let (driver, _phys) = driver(
        r"
        fn init() {
            let grid = grid_spawn_cubic(0, 0, 0);
            grid_move(grid, vec3(fixed(7), fixed(0), fixed(0)));
        }
    ",
    );
    let grids = driver.grids().lock().expect("grids mutex");
    assert_eq!(grids.body_of(0), -1, "nothing drives it");
    assert!(
        (grids.origin(0).x.to_f64() - 7.0).abs() < EPS,
        "and the map's own pose stands"
    );
}
