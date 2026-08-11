//! Freeform body shapes as a MAP writes them (docs/plans/ship-physics.md §5,
//! S-2): open a shape, write cell boxes into it, spawn a body, and get mass
//! properties derived from the geometry rather than from a block that merely
//! bounds it.
//!
//! The point of the slice is the difference between a SHELL and a BLOCK. A
//! hull is a shell; an engine bolted off its centreline feels the shell's
//! inertia tensor, not the block's, and the two differ by a lot. So the tests
//! below build both through real Rhai and compare what the physics world
//! derived — mass, centre of mass, inertia — rather than asserting that the
//! verbs merely resolved.

use std::sync::{Arc, Mutex};

use monada_fixed::Fixed;
use monada_physics::{BodyId, RigidBody};
use monada_script::{
    shared_physics, shared_world, NullBridge, RhaiBackend, ScriptBackend, SharedBridge,
    SharedPhysics, SharedWorld,
};

const SEED: u64 = 0x6D_6F_6E_61_64_61_00_02;

/// A backend with physics embedded (what a `terrain = "volume"` map gets) and
/// `script`'s `init` run, plus the two handles to inspect afterwards.
fn boot(script: &str) -> (SharedWorld, SharedPhysics) {
    let world = shared_world(SEED);
    let phys = shared_physics(30);
    let mut backend = RhaiBackend::new(world.clone());
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    backend.set_physics(&phys);
    backend.load(script).expect("compile");
    backend.on_init().expect("init");
    (world, phys)
}

/// The physics half alone — what most of these tests want.
fn run(script: &str) -> SharedPhysics {
    boot(script).1
}

fn with_body<R>(phys: &SharedPhysics, id: u64, f: impl FnOnce(&RigidBody) -> R) -> R {
    let sim = phys.lock().expect("physics mutex");
    f(sim.world.body(BodyId(id)).expect("body exists"))
}

/// A 20×20×6 hull authored as a shell — the ship's own dimensions — against a
/// solid block of the same bounds, both at density 1.
///
/// Mass is the honest half: a shell weighs its walls, not its air. Inertia is
/// the half that matters for engines — mass pushed out to the skin resists
/// turning far more per kilo than mass smeared through the middle, so the
/// shell's tensor must exceed the block's *relative to its own mass*.
const SHELL_VS_BLOCK: &str = r"
fn init() {
    phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4)); // id 0, density 1

    // The shell: a solid block with its inside taken out. Body 0.
    let hull = phys_shape(20, 20, 6);
    phys_shape_fill(hull, 0, 0, 0, 19, 19, 5, 0);
    phys_shape_clear(hull, 1, 1, 1, 18, 18, 4);
    phys_body(hull, vec3(fixed(0), fixed(0), fixed(0)));

    // The block it would have been under `phys_box`. Body 1.
    let solid = phys_shape(20, 20, 6);
    phys_shape_fill(solid, 19, 19, 5, 0, 0, 0, 0); // corners either way round
    phys_body(solid, vec3(fixed(0), fixed(0), fixed(0)));
}
";

#[test]
fn a_shell_weighs_its_walls_and_turns_like_one() {
    let phys = run(SHELL_VS_BLOCK);

    // 20·20·6 = 2400 cells, minus the 18·18·4 = 1296 cleared: 1104 walls.
    let shell_mass = with_body(&phys, 0, RigidBody::mass).to_f64();
    let block_mass = with_body(&phys, 1, RigidBody::mass).to_f64();
    assert!(
        (shell_mass - 1104.0).abs() < 1e-6,
        "the shell weighs its 1104 wall cells, got {shell_mass}"
    );
    assert!(
        (block_mass - 2400.0).abs() < 1e-6,
        "the block weighs all 2400, got {block_mass}"
    );

    // Both are symmetric about the same box, so both centres of mass sit at
    // its middle — the point `grid_body` will hang a hull's pivot on (D3).
    let com = with_body(&phys, 0, RigidBody::com_in_shape);
    assert!(
        (com.x.to_f64() - 10.0).abs() < 1e-6
            && (com.y.to_f64() - 10.0).abs() < 1e-6
            && (com.z.to_f64() - 3.0).abs() < 1e-6,
        "a symmetric shell's CoM is the box centre, got {com:?}"
    );

    // Inertia PER UNIT MASS: the shell's mass lives at the skin, so it resists
    // turning harder than the same mass smeared through a solid block. Compare
    // about z (the yaw axis an RCS quad fights).
    let izz = |id: u64| with_body(&phys, id, |b| b.inertia_body().z_axis.z.to_f64());
    let shell = izz(0) / shell_mass;
    let block = izz(1) / block_mass;
    assert!(
        shell > block * 1.2,
        "a shell resists yaw more per unit mass than a block: {shell} vs {block}"
    );
}

/// Clearing is exact, not decorative: the cells a map takes out stop
/// contributing mass. Without that, "hollow" would be a lie the dynamics never
/// heard — the hull would weigh its bounding box and fly like a brick.
#[test]
fn cleared_cells_leave_the_body() {
    let phys = run(r"
        fn init() {
            phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));
            let s = phys_shape(4, 4, 4);
            phys_shape_fill(s, 0, 0, 0, 3, 3, 3, 0);   // 64
            phys_shape_clear(s, 1, 1, 1, 2, 2, 2);     // -8
            phys_body(s, vec3(fixed(0), fixed(0), fixed(0)));
        }
    ");
    let mass = with_body(&phys, 0, RigidBody::mass).to_f64();
    assert!((mass - 56.0).abs() < 1e-6, "64 cells less 8, got {mass}");
}

/// `phys_body` places the DERIVED centre of mass at the point it is given —
/// the convention `phys_box` documents, and the one `grid_body` will lean on
/// when it hangs a render grid off a body pose. An OFF-CENTRE shape is the
/// decisive case: place a lopsided body and the `CoM`, not the shape's corner,
/// lands on the point.
#[test]
fn a_body_is_placed_by_its_centre_of_mass() {
    let phys = run(r"
        fn init() {
            phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));
            let s = phys_shape(8, 2, 2);
            phys_shape_fill(s, 0, 0, 0, 1, 1, 1, 0);   // a stub at the low end
            phys_body(s, vec3(fixed(5), fixed(0), fixed(0)));
        }
    ");
    let (pos, com) = with_body(&phys, 0, |b| (b.position(), b.com_in_shape()));
    assert!(
        (pos.x.to_f64() - 5.0).abs() < 1e-6,
        "the body sits where it was placed, got {pos:?}"
    );
    assert!(
        (com.x.to_f64() - 1.0).abs() < 1e-6,
        "and the stub's CoM is at its own middle, not the shape's: {com:?}"
    );
}

// A shape is consumed by the body it becomes — the map owns the authoring copy
// until `phys_body`, the sim owns the voxels after it — and writing through a
// spent handle raises, like `phys_wheel` on an unknown body. There is no test
// for it here on purpose: Rhai wraps a host function's panic and re-raises it
// in a NON-UNWINDING context (rhai `func/call.rs`), so a raise inside a verb
// aborts the process rather than unwinding, and `catch_unwind` cannot see it.
// Pinning that would take a subprocess harness the repo does not have.

/// The shape table is scratch, not sim state, so it never reaches a digest —
/// but the BODY it produced must. Two peers authoring the same hull hash the
/// same, and a hull authored differently hashes differently; that is the whole
/// determinism contract this slice needs.
#[test]
fn the_body_a_shape_produces_is_hashed() {
    let hash = |script: &str| {
        run(script)
            .lock()
            .expect("physics mutex")
            .world
            .state_hash()
    };
    let shell = r"
        fn init() {
            phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));
            let s = phys_shape(4, 4, 4);
            phys_shape_fill(s, 0, 0, 0, 3, 3, 3, 0);
            phys_shape_clear(s, 1, 1, 1, 2, 2, 2);
            phys_body(s, vec3(fixed(0), fixed(0), fixed(0)));
        }
    ";
    let solid = r"
        fn init() {
            phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));
            let s = phys_shape(4, 4, 4);
            phys_shape_fill(s, 0, 0, 0, 3, 3, 3, 0);
            phys_body(s, vec3(fixed(0), fixed(0), fixed(0)));
        }
    ";
    assert_eq!(hash(shell), hash(shell), "the same hull hashes the same");
    assert_ne!(
        hash(shell),
        hash(solid),
        "a hollow hull is not a solid one, and the digest knows"
    );
}

/// `phys_mass` hands the derived mass back to the MAP, which is what lets a
/// script size thrust to the hull it actually authored instead of to a
/// constant that drifts the first time the hull changes. Read it where a map
/// would keep it: an entity field.
#[test]
fn phys_mass_reads_the_derived_mass() {
    let (world, phys) = boot(r#"
        fn init() {
            phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));
            archetype(["mass"]);
            let s = phys_shape(3, 3, 3);
            phys_shape_fill(s, 0, 0, 0, 2, 2, 2, 0);
            let b = phys_body(s, vec3(fixed(0), fixed(0), fixed(0)));
            let e = entity_create(0);
            entity_set_field(e, "mass", phys_mass(b));
        }
    "#);
    let stored = {
        let w = world.lock().expect("world mutex");
        w.field(monada_sim::EntityId(0), "mass").expect("field set")
    };
    assert_eq!(stored, Fixed::from_int(27), "3³ cells at density 1");
    assert_eq!(
        stored,
        with_body(&phys, 0, RigidBody::mass),
        "and it is the same number the sim derived"
    );
}
