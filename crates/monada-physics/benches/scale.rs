//! The P5 acceptance benchmark (docs/plans/voxel-physics.md §6 P5):
//! 32 four-wheeled vehicles driving + 256 loose wreck bodies over
//! bumpy terrain, measured per `step()` against the confirmed budget
//! of **4 ms out of the 40 ms tick**.
//!
//! Reference hardware for the recorded numbers: 12th Gen Intel Core
//! i7-12700H (author's machine), Linux, `--release`. Criterion runs
//! locally only — the number is a local acceptance recorded in the
//! plan, not a CI gate (CI machines vary too much to gate on wall
//! time; determinism is gated by the oracle instead).
//!
//! The scene validates itself before measuring: a healthy chunk of
//! the wrecks must actually be ASLEEP at measurement time — if
//! sleeping ever breaks, this benchmark must fail loudly rather than
//! silently time a different (all-awake) scene.

use criterion::{criterion_group, criterion_main, Criterion};
use monada_fixed::{Fixed, FixedQuat, FixedVec3};
use monada_physics::{
    Material, MaterialId, PhysicsWorld, VoxelBodyDef, VoxelField, VoxelShape, WheelDef, WheelInput,
};

/// Flat floor with a deterministic 0–2 voxel bump field.
struct Bumpy;
impl VoxelField for Bumpy {
    fn occupied(&self, x: i64, y: i64, z: i64) -> bool {
        let bump =
            (x.div_euclid(3).wrapping_mul(7) + y.div_euclid(3).wrapping_mul(5)).rem_euclid(3);
        z < bump - 1
    }
    fn material(&self, _x: i64, _y: i64, _z: i64) -> MaterialId {
        MaterialId(0)
    }
}

fn vec3(x: i32, y: i32, z: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::from_int(z))
}

fn build_scene() -> PhysicsWorld {
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(vec3(0, 0, -10));
    let mat = world.register_material(Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(50),
    });

    // 32 vehicles on an 8×4 grid, all driving +x (the test-suite
    // stance: 6×4×2 chassis, wheelbase ±3.5, track ±2.5).
    for vy in 0..4i32 {
        for vx in 0..8i32 {
            let mut chassis = VoxelShape::new(6, 4, 2);
            chassis.fill_box((0, 0, 0), (5, 3, 1), mat);
            let body = world.spawn_voxels(&VoxelBodyDef {
                shape: chassis,
                position: vec3(vx * 40 - 160, vy * 40 - 80, 4),
                orientation: FixedQuat::IDENTITY,
                linear_velocity: FixedVec3::ZERO,
                angular_velocity: FixedVec3::ZERO,
            });
            let com = world.body(body).unwrap().com_in_shape();
            for (sx, sy) in [(13, 9), (13, -1), (-1, 9), (-1, -1)] {
                let id = world.attach_wheel(
                    body,
                    &WheelDef {
                        anchor: FixedVec3::new(
                            Fixed::from_ratio(sx, 2),
                            Fixed::from_ratio(sy, 2),
                            Fixed::ZERO,
                        ) - com,
                        rest_length: Fixed::from_ratio(3, 2),
                        radius: Fixed::HALF,
                        stiffness: Fixed::from_int(240),
                        damping: Fixed::from_int(80),
                        friction: Fixed::from_ratio(4, 5),
                    },
                );
                world.set_wheel_input(
                    body,
                    id,
                    WheelInput {
                        steer: Fixed::ZERO,
                        drive: Fixed::from_int(20),
                        brake: Fixed::ZERO,
                    },
                );
            }
        }
    }

    // 256 wrecks: 2³ cubes dropped in 64 piles of 4.
    for py in 0..8i32 {
        for px in 0..8i32 {
            for tier in 0..4i32 {
                let mut wreck = VoxelShape::new(2, 2, 2);
                wreck.fill_box((0, 0, 0), (1, 1, 1), mat);
                world.spawn_voxels(&VoxelBodyDef {
                    shape: wreck,
                    position: vec3(px * 24 - 96 + tier, py * 24 - 96 + 12, 3 + tier * 3),
                    orientation: FixedQuat::IDENTITY,
                    linear_velocity: FixedVec3::ZERO,
                    angular_velocity: FixedVec3::ZERO,
                });
            }
        }
    }

    // Warm-up: piles land, settle, and sleep; vehicles reach cruise.
    for _ in 0..500 {
        world.step(&Bumpy);
    }
    // Scene validity: sleeping must be working, or this benchmark is
    // silently timing the wrong scene.
    let asleep = world.bodies().iter().filter(|b| b.asleep()).count();
    assert!(
        asleep >= 128,
        "scene invalid: only {asleep} of 288 bodies asleep at measurement time"
    );
    world
}

fn bench_step(c: &mut Criterion) {
    let world = build_scene();
    c.bench_function("step: 32 vehicles + 256 wrecks", |b| {
        b.iter_batched(
            || world.clone(),
            |mut w| {
                w.step(&Bumpy);
                w
            },
            criterion::BatchSize::LargeInput,
        );
    });
}

criterion_group!(benches, bench_step);
criterion_main!(benches);
