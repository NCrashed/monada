//! P4 acceptance (docs/plans/voxel-physics.md §6 P4 + amendments):
//! split-in-half mass bookkeeping (incremental vs full recompute), the
//! full no-teleport invariant (positions AND point velocities), debris
//! clusters, wheel re-anchor/auto-detach, edge cases, determinism.

use monada_fixed::{Fixed, FixedMat3, FixedQuat, FixedVec3};
use monada_physics::{
    Material, MaterialId, PhysicsWorld, RigidBody, VoxelBodyDef, VoxelField, VoxelShape, WheelDef,
};

fn close(a: Fixed, b: Fixed, eps_bits: i64) {
    let d = (a.to_bits() - b.to_bits()).abs();
    assert!(d <= eps_bits, "‖{a:?} - {b:?}‖ = {d} bits > {eps_bits}");
}

fn close_vec(a: FixedVec3, b: FixedVec3, eps_bits: i64) {
    close(a.x, b.x, eps_bits);
    close(a.y, b.y, eps_bits);
    close(a.z, b.z, eps_bits);
}

fn vec3(x: i32, y: i32, z: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::from_int(z))
}

struct Floor;
impl VoxelField for Floor {
    fn occupied(&self, _x: i64, _y: i64, z: i64) -> bool {
        z < 0
    }
    fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
        MaterialId(0)
    }
}

/// World with one unit-density material.
fn new_world() -> (PhysicsWorld, MaterialId) {
    let mut world = PhysicsWorld::new(25);
    let mat = world.register_material(Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
    });
    (world, mat)
}

fn cube_body(
    world: &mut PhysicsWorld,
    mat: MaterialId,
    n: i32,
    def: &VoxelBodyDef,
) -> monada_physics::BodyId {
    let mut shape = VoxelShape::new(n, n, n);
    shape.fill_box((0, 0, 0), (n - 1, n - 1, n - 1), mat);
    world.spawn_voxels(&VoxelBodyDef {
        shape,
        ..def.clone()
    })
}

fn base_def() -> VoxelBodyDef {
    VoxelBodyDef {
        shape: VoxelShape::new(1, 1, 1), // replaced by callers
        position: FixedVec3::ZERO,
        orientation: FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    }
}

/// Test-side reference: full mass-property recompute from a body's
/// (public) shape — the same solid-cube convention the crate
/// documents, with per-material densities indexed by `MaterialId`
/// (the incremental path's trickiest case is mixed densities). The
/// acceptance's independent yardstick.
fn reference_properties(body: &RigidBody, densities: &[Fixed]) -> (Fixed, FixedVec3, FixedMat3) {
    let shape = body.shape().expect("voxel body");
    let (dx, dy, dz) = shape.dims();
    let mut mass = Fixed::ZERO;
    let mut weighted = FixedVec3::ZERO;
    let mut cells = Vec::new();
    for z in 0..dz {
        for y in 0..dy {
            for x in 0..dx {
                if let Some(mat) = shape.get(x, y, z) {
                    let density = densities[usize::from(mat.0)];
                    let c = FixedVec3::new(
                        Fixed::from_int(x) + Fixed::HALF,
                        Fixed::from_int(y) + Fixed::HALF,
                        Fixed::from_int(z) + Fixed::HALF,
                    );
                    mass += density;
                    weighted += c.scale(density);
                    cells.push((c, density));
                }
            }
        }
    }
    // Component-wise division, mirroring the crate (reciprocal
    // rounding is amplified by |weighted| — the P4 review find).
    let com = FixedVec3::new(weighted.x / mass, weighted.y / mass, weighted.z / mass);
    let sixth = Fixed::ONE / Fixed::from_int(6);
    let mut inertia = FixedMat3::ZERO;
    for (c, density) in cells {
        let d = c - com;
        let dd = d.dot(d);
        let outer = FixedMat3::from_cols(
            FixedVec3::new(d.x * d.x, d.y * d.x, d.z * d.x),
            FixedVec3::new(d.x * d.y, d.y * d.y, d.z * d.y),
            FixedVec3::new(d.x * d.z, d.y * d.z, d.z * d.z),
        );
        inertia = inertia
            + (FixedMat3::from_diagonal(FixedVec3::new(dd, dd, dd)) - outer
                + FixedMat3::from_diagonal(FixedVec3::new(sixth, sixth, sixth)))
            .scale(density);
    }
    (mass, com, inertia)
}

fn assert_mat_close(a: FixedMat3, b: FixedMat3, eps: i64) {
    close_vec(a.x_axis, b.x_axis, eps);
    close_vec(a.y_axis, b.y_axis, eps);
    close_vec(a.z_axis, b.z_axis, eps);
}

/// The P4 headline: cut a 5³ cube in half. Two bodies whose masses sum
/// to the original minus the removed plane (bit-exact — density sums
/// are exact adds), survivor's incremental tensor vs the full
/// recompute within a documented ulp bound, fragment vs full directly.
#[test]
fn cut_in_half_masses_and_tensors_add_up() {
    let (mut world, mat) = new_world();
    let id = cube_body(&mut world, mat, 5, &base_def());
    let plane: Vec<(i32, i32, i32)> = (0..5)
        .flat_map(|y| (0..5).map(move |z| (2, y, z)))
        .collect();
    let outcome = world.remove_voxels(id, &plane);

    assert_eq!(outcome.removed, 25);
    // Equal-mass halves: the tie breaks to the lexicographically
    // smaller min_cell in the parent grid — the low-x half keeps the
    // identity.
    assert_eq!(outcome.survivor, Some(id));
    assert_eq!(outcome.split_off.len(), 1);
    assert!(outcome.debris.is_empty());

    let survivor = world.body(id).unwrap();
    let fragment = world.body(outcome.split_off[0]).unwrap();
    assert_eq!(
        survivor.mass() + fragment.mass(),
        Fixed::from_int(125 - 25),
        "mass bookkeeping is exact"
    );
    // Incremental survivor vs full recompute: 75 departed voxels, a
    // couple of ulps each, plus one parallel-axis transfer — 2⁻²⁰
    // per entry is a comfortable roof.
    let (m_ref, com_ref, i_ref) = reference_properties(survivor, &[Fixed::ONE]);
    assert_eq!(survivor.mass(), m_ref);
    close_vec(survivor.com_in_shape(), com_ref, 1 << 10);
    assert_mat_close(survivor.inertia_body(), i_ref, 1 << 12);
    // Fragment was fully computed at spawn — tighter agreement.
    let (fm, fcom, fi) = reference_properties(fragment, &[Fixed::ONE]);
    assert_eq!(fragment.mass(), fm);
    close_vec(fragment.com_in_shape(), fcom, 1 << 4);
    assert_mat_close(fragment.inertia_body(), fi, 1 << 8);
}

/// A run of LCG-random carves on a TWO-MATERIAL body (the incremental
/// path's trickiest input — mixed densities): after every carve the
/// survivor's incremental properties track the full recompute; drift
/// accumulates linearly in the carve count (documented bound: 2⁻²⁰
/// per entry per carve round, density-scaled).
#[test]
fn incremental_tracks_full_recompute_over_many_carves() {
    let (mut world, light) = new_world();
    let dense = world.register_material(Material {
        density: Fixed::from_int(4),
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
    });
    // 6³ with dense voxels sprinkled deterministically through the
    // light bulk.
    let mut shape = VoxelShape::new(6, 6, 6);
    for z in 0..6 {
        for y in 0..6 {
            for x in 0..6 {
                let mat = if (x + 2 * y + 3 * z) % 5 == 0 {
                    dense
                } else {
                    light
                };
                shape.set(x, y, z, mat);
            }
        }
    }
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape,
        ..base_def()
    });
    let densities = [Fixed::ONE, Fixed::from_int(4)];
    let mut lcg: u64 = 0x00DE_FACE;
    let mut next = |m: i32| {
        lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        i32::try_from(i64::try_from(lcg >> 33).expect("31 bits") % i64::from(m)).expect("small")
    };
    for round in 1..=10 {
        let cells: Vec<(i32, i32, i32)> = (0..4).map(|_| (next(6), next(6), next(6))).collect();
        let outcome = world.remove_voxels(id, &cells);
        if outcome.survivor.is_none() {
            break; // degraded away — fine for this test's purpose
        }
        let survivor = world.body(id).unwrap();
        let (m_ref, com_ref, i_ref) = reference_properties(survivor, &densities);
        assert_eq!(survivor.mass(), m_ref, "round {round}");
        close_vec(survivor.com_in_shape(), com_ref, 1 << 10);
        assert_mat_close(survivor.inertia_body(), i_ref, i64::from(round) << 12);
    }
}

/// The FULL no-teleport invariant on a spinning, moving body: marked
/// voxels keep world position AND world point velocity through the
/// split — in the survivor and in the fragment.
#[test]
fn split_preserves_voxel_positions_and_velocities() {
    let (mut world, mat) = new_world();
    let def = VoxelBodyDef {
        position: vec3(10, -4, 30),
        linear_velocity: vec3(1, 2, 3),
        angular_velocity: FixedVec3::new(
            Fixed::from_ratio(3, 10),
            Fixed::from_ratio(7, 10),
            -Fixed::from_ratio(1, 5),
        ),
        ..base_def()
    };
    let id = cube_body(&mut world, mat, 5, &def);

    // World state of a voxel-centre point on a rigid body.
    let point_state = |b: &RigidBody, cell_center_shape: FixedVec3| {
        let world_pos = b.position() + b.orientation() * (cell_center_shape - b.com_in_shape());
        let vel = b.linear_velocity() + b.angular_velocity().cross(world_pos - b.position());
        (world_pos, vel)
    };
    let half = Fixed::HALF;
    let low_mark = FixedVec3::new(half, half, half); // cell (0,0,0)
    let high_mark = vec3(4, 4, 4) + FixedVec3::new(half, half, half); // cell (4,4,4)

    let (low_pos0, low_vel0) = point_state(world.body(id).unwrap(), low_mark);
    let (high_pos0, high_vel0) = point_state(world.body(id).unwrap(), high_mark);

    let plane: Vec<(i32, i32, i32)> = (0..5)
        .flat_map(|y| (0..5).map(move |z| (2, y, z)))
        .collect();
    let outcome = world.remove_voxels(id, &plane);

    // Survivor keeps cell (0,0,0) at the same shape coords.
    let (low_pos1, low_vel1) = point_state(world.body(id).unwrap(), low_mark);
    close_vec(low_pos1, low_pos0, 1 << 10);
    close_vec(low_vel1, low_vel0, 1 << 10);
    // Fragment holds cell (4,4,4), rebased: its tight grid starts at
    // parent x = 3, so the marked cell is (1,4,4) there.
    let frag = world.body(outcome.split_off[0]).unwrap();
    let frag_mark = vec3(1, 4, 4) + FixedVec3::new(half, half, half);
    let (high_pos1, high_vel1) = point_state(frag, frag_mark);
    close_vec(high_pos1, high_pos0, 1 << 10);
    close_vec(high_vel1, high_vel0, 1 << 10);
}

/// Debris: a 5×1×1 spinning rod loses cell 3 — the single-voxel tip
/// (below threshold 3) comes off as a cluster with rigid-body point
/// velocity `v + ω×r`, correct count and material.
#[test]
fn sub_threshold_fragment_becomes_debris() {
    let (mut world, mat) = new_world();
    let mut shape = VoxelShape::new(5, 1, 1);
    shape.fill_box((0, 0, 0), (4, 0, 0), mat);
    let omega = FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::ONE);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape,
        position: vec3(0, 0, 10),
        orientation: FixedQuat::IDENTITY,
        linear_velocity: vec3(2, 0, 0),
        angular_velocity: omega,
    });
    let com_old = world.body(id).unwrap().com_in_shape();
    let pos_old = world.body(id).unwrap().position();

    let outcome = world.remove_voxels(id, &[(3, 0, 0)]);
    assert_eq!(outcome.removed, 1);
    assert_eq!(outcome.survivor, Some(id));
    assert!(outcome.split_off.is_empty());
    assert_eq!(outcome.debris.len(), 1);

    let cluster = &outcome.debris[0];
    assert_eq!(cluster.voxels.len(), 1);
    assert_eq!(cluster.voxels[0].1, mat);
    // Cluster CoM: cell (4,0,0) centre → shape (4.5, 0.5, 0.5).
    let r_world = FixedVec3::new(
        Fixed::from_ratio(9, 2) - com_old.x,
        Fixed::HALF - com_old.y,
        Fixed::HALF - com_old.z,
    );
    close_vec(cluster.position, pos_old + r_world, 1 << 8);
    close_vec(
        cluster.linear_velocity,
        vec3(2, 0, 0) + omega.cross(r_world),
        1 << 8,
    );
    // The lone voxel sits at its cluster's CoM.
    close_vec(cluster.voxels[0].0, FixedVec3::ZERO, 1 << 8);
}

/// Edge cases: empty/out-of-bounds/duplicate cells; a survivor that
/// itself falls below the threshold; total destruction; ghosts.
#[test]
fn edge_cases() {
    // Empty + out-of-bounds + duplicates.
    let (mut world, mat) = new_world();
    let id = cube_body(&mut world, mat, 3, &base_def());
    let h0 = world.state_hash();
    let miss = world.remove_voxels(id, &[(-5, 0, 0), (99, 99, 99)]);
    assert_eq!(miss.removed, 0);
    assert_eq!(world.state_hash(), h0, "removed == 0 leaves no trace");
    let dup = world.remove_voxels(id, &[(0, 0, 0), (0, 0, 0), (0, 0, 0)]);
    assert_eq!(dup.removed, 1, "duplicates collapse");

    // Survivor below the threshold: a 2-voxel remainder degrades to
    // debris and the body despawns — and its wheels ALL report as
    // detached (the blown-to-debris vehicle must not be the silent
    // case).
    let (mut world, mat) = new_world();
    let mut bar = VoxelShape::new(4, 1, 1);
    bar.fill_box((0, 0, 0), (3, 0, 0), mat);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape: bar,
        ..base_def()
    });
    let wheel = world.attach_wheel(
        id,
        &WheelDef {
            anchor: FixedVec3::ZERO,
            rest_length: Fixed::ONE,
            radius: Fixed::HALF,
            stiffness: Fixed::from_int(10),
            damping: Fixed::ONE,
            friction: Fixed::HALF,
        },
    );
    let outcome = world.remove_voxels(id, &[(0, 0, 0), (1, 0, 0)]);
    assert_eq!(outcome.survivor, None, "sub-threshold survivor degrades");
    assert_eq!(outcome.debris.len(), 1);
    assert_eq!(outcome.debris[0].voxels.len(), 2);
    assert_eq!(
        outcome.detached_wheels,
        vec![wheel],
        "a despawned body detaches every wheel"
    );
    assert!(world.body(id).is_none(), "body despawned");

    // Total destruction: nothing left at all.
    let (mut world, mat) = new_world();
    let id = cube_body(&mut world, mat, 2, &base_def());
    let all: Vec<(i32, i32, i32)> = (0..2)
        .flat_map(|x| (0..2).flat_map(move |y| (0..2).map(move |z| (x, y, z))))
        .collect();
    let outcome = world.remove_voxels(id, &all);
    assert_eq!(outcome.removed, 8);
    assert_eq!(outcome.survivor, None);
    assert!(outcome.debris.is_empty() && outcome.split_off.is_empty());
    assert!(world.body(id).is_none());
}

#[test]
#[should_panic(expected = "ghost")]
fn carving_a_ghost_panics() {
    let mut world = PhysicsWorld::new(25);
    let id = world.spawn(&monada_physics::BodyDef::default());
    let _ = world.remove_voxels(id, &[(0, 0, 0)]);
}

/// Wheels are bolted to structure: carving the middle out of a
/// chassis keeps the low-x half's wheels (anchors re-based, world
/// anchor positions invariant) and auto-detaches the wheels whose
/// nearest structure departed with the fragment.
#[test]
fn wheels_reanchor_or_detach_on_split() {
    let (mut world, mat) = new_world();
    let mut shape = VoxelShape::new(6, 4, 2);
    shape.fill_box((0, 0, 0), (5, 3, 1), mat);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape,
        position: vec3(0, 0, 10),
        ..base_def()
    });
    let com = world.body(id).unwrap().com_in_shape();
    let mut attach = |sx: i32, sy: i32| {
        world.attach_wheel(
            id,
            &WheelDef {
                anchor: FixedVec3::new(
                    Fixed::from_int(sx) + Fixed::HALF,
                    Fixed::from_int(sy) + Fixed::HALF,
                    Fixed::ZERO,
                ) - com,
                rest_length: Fixed::ONE,
                radius: Fixed::HALF,
                stiffness: Fixed::from_int(100),
                damping: Fixed::from_int(20),
                friction: Fixed::HALF,
            },
        )
    };
    let rear_left = attach(0, 3);
    let rear_right = attach(0, 0);
    let front_left = attach(5, 3);
    let front_right = attach(5, 0);

    // World anchor of a wheel.
    let anchor_world = |world: &PhysicsWorld, wheel: monada_physics::WheelId| {
        let b = world.body(id).unwrap();
        let w = b.wheels().iter().find(|w| w.id() == wheel).unwrap();
        b.position() + b.orientation() * w.def().anchor
    };
    let rear_anchor_before = anchor_world(&world, rear_left);

    // Carve the two middle planes: halves split, low-x keeps identity.
    let cells: Vec<(i32, i32, i32)> = (2..4)
        .flat_map(|x| (0..4).flat_map(move |y| (0..2).map(move |z| (x, y, z))))
        .collect();
    let outcome = world.remove_voxels(id, &cells);
    assert_eq!(outcome.survivor, Some(id));
    assert_eq!(outcome.split_off.len(), 1);
    assert_eq!(
        outcome.detached_wheels,
        vec![front_left, front_right],
        "front wheels' structure departed"
    );
    let kept: Vec<_> = world
        .body(id)
        .unwrap()
        .wheels()
        .iter()
        .map(monada_physics::Wheel::id)
        .collect();
    assert_eq!(kept, vec![rear_left, rear_right]);
    // Re-anchored: the kept wheel's world position is unchanged even
    // though the CoM (and `position`) moved.
    close_vec(anchor_world(&world, rear_left), rear_anchor_before, 1 << 10);
}

/// A resting cube carved on one side stays at rest (skin and cache
/// rebuild correctly): moderate carve so the `CoM` stays well inside
/// the support polygon, plus a few grace ticks for the cold cache.
#[test]
fn resting_cube_stays_resting_after_side_carve() {
    let (mut world, mat) = new_world();
    world.set_gravity(vec3(0, 0, -10));
    let id = cube_body(
        &mut world,
        mat,
        4,
        &VoxelBodyDef {
            position: vec3(0, 0, 6),
            ..base_def()
        },
    );
    for _ in 0..200 {
        world.step(&Floor);
    }
    // Shave a 1×4×2 sliver off one side's top half: 8 of 64 voxels.
    let cells: Vec<(i32, i32, i32)> = (0..4)
        .flat_map(|y| (2..4).map(move |z| (0, y, z)))
        .collect();
    let outcome = world.remove_voxels(id, &cells);
    assert_eq!(outcome.removed, 8);
    assert_eq!(outcome.survivor, Some(id));
    for _ in 0..10 {
        world.step(&Floor); // grace: warm the cold cache
    }
    for _ in 0..200 {
        world.step(&Floor);
        let b = world.body(id).unwrap();
        assert!(
            b.linear_velocity().length() < monada_physics::SLEEP_LINEAR
                && b.angular_velocity().length() < monada_physics::SLEEP_ANGULAR,
            "woke up after the carve"
        );
    }
}

/// Determinism: identical carve sequences agree bit-for-bit; the
/// snapshot round-trip survives destruction; the debris threshold is
/// hashed config.
#[test]
fn destruction_is_deterministic_and_snapshotable() {
    let run = || {
        let (mut world, mat) = new_world();
        world.set_gravity(vec3(0, 0, -10));
        let id = cube_body(
            &mut world,
            mat,
            5,
            &VoxelBodyDef {
                position: vec3(0, 0, 8),
                angular_velocity: FixedVec3::new(Fixed::ZERO, Fixed::ZERO, Fixed::HALF),
                ..base_def()
            },
        );
        for _ in 0..100 {
            world.step(&Floor);
        }
        let plane: Vec<(i32, i32, i32)> = (0..5)
            .flat_map(|y| (0..5).map(move |z| (2, y, z)))
            .collect();
        let _ = world.remove_voxels(id, &plane);
        for _ in 0..100 {
            world.step(&Floor);
        }
        world
    };
    let a = run();
    let b = run();
    assert_eq!(a.state_hash(), b.state_hash());

    let json = serde_json::to_string(&a).expect("serialize");
    let mut restored: PhysicsWorld = serde_json::from_str(&json).expect("deserialize");
    let mut a = a;
    for _ in 0..100 {
        a.step(&Floor);
        restored.step(&Floor);
    }
    assert_eq!(a.state_hash(), restored.state_hash());

    // Tripwire: the threshold is part of the fold.
    let (mut with_threshold, _) = new_world();
    with_threshold.set_debris_threshold(5);
    let (base, _) = new_world();
    assert_ne!(with_threshold.state_hash(), base.state_hash());
}

/// The driving-on acceptance flavour: the vehicle sheds its nose
/// slice mid-run (skin, wheels, and mass bookkeeping all functional
/// after the split) and keeps driving on all four wheels. A literal
/// half-vehicle keeps only one axle and face-plants — correct wheel
/// physics, so the wheels here sit inside the surviving span.
#[test]
fn vehicle_sheds_its_nose_and_drives_on() {
    let (mut world, mat) = new_world();
    world.set_gravity(vec3(0, 0, -10));
    let mut shape = VoxelShape::new(6, 4, 2);
    shape.fill_box((0, 0, 0), (5, 3, 1), mat);
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape,
        position: vec3(0, 0, 3),
        ..base_def()
    });
    let com = world.body(id).unwrap().com_in_shape();
    let mut wheels = Vec::new();
    for (sx, sy) in [(0, 0), (0, 3), (3, 0), (3, 3)] {
        let anchor = FixedVec3::new(
            Fixed::from_int(sx) + Fixed::HALF,
            Fixed::from_int(sy) + Fixed::HALF,
            Fixed::ZERO,
        ) - com;
        wheels.push(world.attach_wheel(
            id,
            &WheelDef {
                anchor,
                rest_length: Fixed::from_ratio(3, 2),
                radius: Fixed::HALF,
                stiffness: Fixed::from_int(120),
                damping: Fixed::from_int(40),
                friction: Fixed::from_ratio(4, 5),
            },
        ));
    }
    for _ in 0..150 {
        world.step(&Floor);
    }
    // Blow the x = 4 plane out: the x = 5 nose slice splits off as a
    // new body, every wheel's structure stays with the survivor.
    let cells: Vec<(i32, i32, i32)> = (0..4)
        .flat_map(|y| (0..2).map(move |z| (4, y, z)))
        .collect();
    let outcome = world.remove_voxels(id, &cells);
    assert_eq!(outcome.survivor, Some(id));
    assert_eq!(outcome.split_off.len(), 1);
    assert!(outcome.detached_wheels.is_empty());
    assert_eq!(world.body(id).unwrap().wheels().len(), 4);
    for _ in 0..50 {
        world.step(&Floor); // settle the lightened rear
    }
    for wheel in world
        .body(id)
        .unwrap()
        .wheels()
        .iter()
        .map(monada_physics::Wheel::id)
        .collect::<Vec<_>>()
    {
        world.set_wheel_input(
            id,
            wheel,
            monada_physics::WheelInput {
                steer: Fixed::ZERO,
                drive: Fixed::from_int(10),
                brake: Fixed::ZERO,
            },
        );
    }
    let x0 = world.body(id).unwrap().position().x;
    for _ in 0..100 {
        world.step(&Floor);
    }
    assert!(
        world.body(id).unwrap().position().x > x0 + Fixed::ONE,
        "the half-vehicle should still drive"
    );
}
