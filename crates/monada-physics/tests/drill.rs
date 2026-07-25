//! P6 acceptance (docs/plans/voxel-physics.md §6 P6 + amendments):
//! drill query geometry, reaction closed form with the effective-mass
//! clamp, per-material penetration rates, hardness deceleration, the
//! hash-stable tunnel with streaming edits, cache invalidation.

use std::collections::BTreeSet;

use monada_fixed::{trig, Fixed, FixedQuat, FixedVec3};
use monada_physics::{
    BodyDef, DrillTool, Material, MaterialId, PhysicsWorld, VoxelBodyDef, VoxelField, VoxelShape,
};

fn close(a: Fixed, b: Fixed, eps_bits: i64) {
    let d = (a.to_bits() - b.to_bits()).abs();
    assert!(d <= eps_bits, "‖{a:?} - {b:?}‖ = {d} bits > {eps_bits}");
}

fn vec3(x: i32, y: i32, z: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::from_int(z))
}

fn material(hardness: i32) -> Material {
    Material {
        density: Fixed::ONE,
        friction: Fixed::HALF,
        restitution: Fixed::ZERO,
        hardness: Fixed::from_int(hardness),
    }
}

/// Flat floor (material 0) with a wall for x ≥ 8: soft layers
/// (material 1) and hard layers (material 2) alternating in bands of
/// 6 cells along x; carved cells live in the test-owned set.
struct Layered<'a> {
    carved: &'a BTreeSet<(i64, i64, i64)>,
}

impl Layered<'_> {
    fn base_material(x: i64, z: i64) -> Option<MaterialId> {
        if z < 0 {
            return Some(MaterialId(0)); // floor
        }
        if x >= 8 && z < 8 {
            let band = (x - 8).div_euclid(6);
            return Some(if band.rem_euclid(2) == 0 {
                MaterialId(1) // soft
            } else {
                MaterialId(2) // hard
            });
        }
        None
    }
}

impl VoxelField for Layered<'_> {
    fn occupied(&self, x: i64, y: i64, z: i64) -> bool {
        Self::base_material(x, z).is_some() && !self.carved.contains(&(x, y, z))
    }
    fn material(&self, x: i64, _y: i64, z: i64) -> MaterialId {
        Self::base_material(x, z).unwrap_or(MaterialId(0))
    }
}

fn layered_world() -> PhysicsWorld {
    let mut world = PhysicsWorld::new(25);
    world.set_gravity(vec3(0, 0, -10));
    let floor = world.register_material(material(1000));
    let soft = world.register_material(material(10));
    let hard = world.register_material(material(100));
    assert_eq!(
        (floor, soft, hard),
        (MaterialId(0), MaterialId(1), MaterialId(2))
    );
    world
}

/// A 4³ box "mole" with a full-face drill on its +x nose.
fn mole(
    world: &mut PhysicsWorld,
    position: FixedVec3,
    vx: i32,
) -> (monada_physics::BodyId, DrillTool) {
    let mut shape = VoxelShape::new(4, 4, 4);
    shape.fill_box((0, 0, 0), (3, 3, 3), MaterialId(1));
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape,
        position,
        orientation: FixedQuat::IDENTITY,
        linear_velocity: vec3(vx, 0, 0),
        angular_velocity: FixedVec3::ZERO,
    });
    let tool = DrillTool {
        anchor: FixedVec3::new(Fixed::from_ratio(5, 2), Fixed::ZERO, Fixed::ZERO),
        half_extents: FixedVec3::new(Fixed::ONE, Fixed::from_ratio(5, 2), Fixed::from_ratio(5, 2)),
        orientation: FixedQuat::IDENTITY,
    };
    (id, tool)
}

type CellBox = ((i64, i64, i64), (i64, i64, i64));

/// One engine-side tick of the test's drill policy: cut overlapped
/// cells front-to-back while the hardness budget lasts, mutate the
/// terrain set, notify physics, apply the reaction.
fn drill_tick(
    world: &mut PhysicsWorld,
    body: monada_physics::BodyId,
    tool: &DrillTool,
    carved: &mut BTreeSet<(i64, i64, i64)>,
    budget: Fixed,
) {
    let samples = {
        let field = Layered { carved };
        world.drill_query(body, tool, &field)
    };
    let mut cut = Vec::new();
    let mut spent = Fixed::ZERO;
    let mut edited: Option<CellBox> = None;
    for s in samples {
        // Engine policy: never drill the floor (material 0).
        let hardness = match s.material {
            MaterialId(0) => continue,
            MaterialId(1) => Fixed::from_int(10),
            _ => Fixed::from_int(100),
        };
        if spent + hardness > budget {
            continue;
        }
        spent += hardness;
        carved.insert(s.cell);
        cut.push(s.material);
        edited = Some(match edited {
            None => (s.cell, s.cell),
            Some((lo, hi)) => (
                (lo.0.min(s.cell.0), lo.1.min(s.cell.1), lo.2.min(s.cell.2)),
                (hi.0.max(s.cell.0), hi.1.max(s.cell.1), hi.2.max(s.cell.2)),
            ),
        });
    }
    if let Some((lo, hi)) = edited {
        world.notify_terrain_edit(lo, hi);
    }
    let _ = world.drill_reaction(body, tool, &cut);
    let field = Layered { carved };
    world.step(&field);
}

/// Reaction closed form at the `CoM`: J = Σh·dt below the clamp, exact
/// Δv; and the OFF-CoM stretch — a giant hardness with the tool far
/// from the `CoM` may stop the TOOL POINT but never reverse it (the
/// clamp runs through the point's effective mass, not the body mass).
#[test]
fn reaction_closed_form_and_effective_mass_clamp() {
    // At the CoM: pure linear, below clamp.
    let mut world = layered_world();
    world.set_gravity(FixedVec3::ZERO);
    let id = world.spawn(&BodyDef {
        position: vec3(0, 0, 10),
        linear_velocity: vec3(10, 0, 0),
        mass: Fixed::from_int(4),
        ..BodyDef::default()
    });
    let tool = DrillTool {
        anchor: FixedVec3::ZERO,
        half_extents: vec3(1, 1, 1),
        orientation: FixedQuat::IDENTITY,
    };
    let cut = [MaterialId(1); 3]; // Σh = 30
    let applied = world.drill_reaction(id, &tool, &cut);
    close(applied, Fixed::from_int(30) * world.dt(), 4); // 1.2
                                                         // Δv = J/m along −x.
    close(
        world.body(id).unwrap().linear_velocity().x,
        Fixed::from_int(10) - applied / Fixed::from_int(4),
        8,
    );

    // Off-CoM + giant hardness: the tool point stops, sign intact.
    let mut world = layered_world();
    world.set_gravity(FixedVec3::ZERO);
    let id = world.spawn(&BodyDef {
        position: vec3(0, 0, 10),
        linear_velocity: vec3(10, 0, 0),
        mass: Fixed::from_int(4),
        ..BodyDef::default()
    });
    let nose = DrillTool {
        anchor: vec3(0, 0, 3),
        half_extents: vec3(1, 1, 1),
        orientation: FixedQuat::IDENTITY,
    };
    let giant = [MaterialId(2); 100]; // Σh = 10 000 — deep past the clamp
    let _ = world.drill_reaction(id, &nose, &giant);
    let b = world.body(id).unwrap();
    let point_v = b.linear_velocity() + b.angular_velocity().cross(b.orientation() * nose.anchor);
    // The point's velocity along its old direction (+x): stopped, not
    // reversed — within a few ulps of the m_eff arithmetic.
    assert!(
        point_v.x.abs() < Fixed::from_ratio(1, 100),
        "tool point should stop: {:?}",
        point_v.x
    );
    assert!(
        point_v.x >= -Fixed::from_ratio(1, 100),
        "tool point must never reverse"
    );

    // Zero velocity: no reaction, deterministic branch — and the wake
    // is unconditional, so prove it on a body that is ACTUALLY asleep
    // (its velocities are zeroed by the sleep itself).
    let mut world = layered_world();
    let mut shape = VoxelShape::new(2, 2, 2);
    shape.fill_box((0, 0, 0), (1, 1, 1), MaterialId(1));
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape,
        position: vec3(0, 0, 3),
        orientation: FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let carved = BTreeSet::new();
    for _ in 0..200 {
        let field = Layered { carved: &carved };
        world.step(&field);
    }
    assert!(world.body(id).unwrap().asleep(), "setup: must sleep");
    assert_eq!(world.drill_reaction(id, &tool, &cut), Fixed::ZERO);
    assert!(
        !world.body(id).unwrap().asleep(),
        "a zero-impulse reaction still wakes"
    );
}

/// Query geometry: a 45°-rotated box catches the diagonal cells; an
/// axial box with a cell centre EXACTLY on its face includes it
/// (inclusive boundary, documented); empty cells never appear;
/// canonical order.
#[test]
fn query_geometry_rotation_and_inclusive_boundary() {
    let mut world = layered_world();
    // Wall cells at x ≥ 8 — park a ghost near it.
    let id = world.spawn(&BodyDef {
        position: vec3(6, 2, 2),
        ..BodyDef::default()
    });
    let carved = BTreeSet::new();
    let field = Layered { carved: &carved };

    // Axial: box face exactly on the cell-centre plane x = 8.5 —
    // half_extents.x = 2.5 from x = 6 → face at 8.5, INCLUSIVE.
    let tool = DrillTool {
        anchor: FixedVec3::ZERO,
        half_extents: FixedVec3::new(Fixed::from_ratio(5, 2), Fixed::HALF, Fixed::HALF),
        orientation: FixedQuat::IDENTITY,
    };
    // Inclusive boundary bites on EVERY axis: the x face sits on the
    // x = 8 centres, and the ±0.5 y/z faces sit exactly on both
    // neighbouring centres — 1×2×2 cells, not 1.
    let samples = world.drill_query(id, &tool, &field);
    let cells: Vec<_> = samples.iter().map(|s| s.cell).collect();
    assert_eq!(cells, vec![(8, 1, 1), (8, 1, 2), (8, 2, 1), (8, 2, 2)]);
    assert!(samples.iter().all(|s| s.material == MaterialId(1)));
    // Canonical order + rotation: yaw 45° with a wide flat box —
    // still only wall cells, sorted (x, y, z).
    let mut world = layered_world();
    let yaw = FixedQuat::from_axis_angle(vec3(0, 0, 1), trig::PI / Fixed::from_int(4));
    let mut shape = VoxelShape::new(2, 2, 2);
    shape.fill_box((0, 0, 0), (1, 1, 1), MaterialId(0));
    let id = world.spawn_voxels(&VoxelBodyDef {
        shape,
        position: vec3(7, 4, 2),
        orientation: yaw,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let diag_tool = DrillTool {
        anchor: vec3(2, 0, 0), // reaches into the wall along the yawed x
        half_extents: FixedVec3::new(Fixed::from_int(2), Fixed::HALF, Fixed::HALF),
        orientation: FixedQuat::IDENTITY,
    };
    let samples = world.drill_query(id, &diag_tool, &field);
    assert!(!samples.is_empty(), "the yawed tool reaches the wall");
    for pair in samples.windows(2) {
        assert!(pair[0].cell < pair[1].cell, "canonical order");
    }
    for s in &samples {
        assert!(s.cell.0 >= 8, "only wall cells are occupied here");
    }
}

/// The acceptance headline: per-material penetration rates. The mole
/// pushes through alternating soft/hard bands under a constant drive
/// impulse; soft bands take fewer ticks per cell than hard bands.
#[test]
fn pitched_tool_composes_with_the_body_orientation() {
    // D3 amendment: DrillTool::orientation pivots the box about its
    // anchor. With the anchor at the CoM, rotating the TOOL must equal
    // rotating the BODY — the query composes `body ∘ tool` into one box
    // frame.
    let pitch = FixedQuat::from_axis_angle(vec3(0, 1, 0), trig::PI / Fixed::from_int(4));
    let long_thin = |orientation: FixedQuat| DrillTool {
        anchor: FixedVec3::ZERO,
        half_extents: FixedVec3::new(Fixed::from_int(4), Fixed::HALF, Fixed::HALF),
        orientation,
    };
    let carved = BTreeSet::new();
    let field = Layered { carved: &carved };

    let mut world = layered_world();
    let pitched_body = world.spawn(&BodyDef {
        position: vec3(6, 2, 2),
        orientation: pitch,
        ..BodyDef::default()
    });
    let level_body = world.spawn(&BodyDef {
        position: vec3(6, 2, 2),
        ..BodyDef::default()
    });

    let via_body = world.drill_query(pitched_body, &long_thin(FixedQuat::IDENTITY), &field);
    let via_tool = world.drill_query(level_body, &long_thin(pitch), &field);
    assert_eq!(via_body, via_tool, "tool pitch ≡ body pitch at the CoM");

    // And the pitch genuinely changes the bite: the level tool stays in
    // the wall band at nose height; the pitched one leaves it (down the
    // slope toward the floor).
    let level = world.drill_query(level_body, &long_thin(FixedQuat::IDENTITY), &field);
    assert_ne!(level, via_tool, "a pitched nose bites different cells");
}

#[test]
fn drill_through_layers_shows_per_material_rates() {
    let mut world = layered_world();
    let (id, tool) = mole(&mut world, vec3(2, 2, 2), 0);
    let mut carved = BTreeSet::new();
    let budget = Fixed::from_int(120);

    // Tick loop with a constant forward push (engine-side thrust).
    let mut soft_ticks = 0u32;
    let mut hard_ticks = 0u32;
    for _ in 0..800 {
        world.apply_impulse(id, vec3(48, 0, 0)); // a = 0.75/tick vs m=64
        drill_tick(&mut world, id, &tool, &mut carved, budget);
        let x = world.body(id).unwrap().position().x;
        // Which band is the nose in? (nose = x + 4.5)
        let nose = x + Fixed::from_ratio(9, 2);
        let band_x = i64::from(nose.floor_to_int());
        if (8..14).contains(&band_x) {
            soft_ticks += 1;
        } else if (14..20).contains(&band_x) {
            hard_ticks += 1;
        }
        if band_x >= 20 {
            break;
        }
    }
    assert!(soft_ticks > 0 && hard_ticks > 0, "never crossed both bands");
    assert!(
        hard_ticks > soft_ticks * 2,
        "hard band ({hard_ticks} ticks) should be far slower than soft ({soft_ticks})"
    );
}

/// Deceleration by hardness: the same mole coasting into the wall
/// loses more speed against a harder first band (two worlds, only the
/// hardness differs).
#[test]
fn wall_hardness_sets_the_deceleration() {
    let run = |hardness: i32| {
        let mut world = PhysicsWorld::new(25);
        world.set_gravity(vec3(0, 0, -10));
        world.register_material(material(1000)); // floor
        world.register_material(material(hardness)); // "soft" band
        world.register_material(material(hardness)); // "hard" band — same here
        let (id, tool) = mole(&mut world, vec3(5, 2, 2), 12); // nose tool at the wall face
        let mut carved = BTreeSet::new();
        // Generous budget: everything overlapped is cut; resistance is
        // pure hardness. Six ticks — long enough for the reaction to
        // bite, short enough that neither run has coasted to a stop
        // (with both stopped the comparison would test zeros).
        for _ in 0..6 {
            drill_tick(&mut world, id, &tool, &mut carved, Fixed::from_int(100_000));
        }
        world.body(id).unwrap().linear_velocity().x
    };
    let after_soft = run(20);
    let after_hard = run(200);
    assert!(
        after_soft > after_hard + Fixed::ONE,
        "harder wall must bleed more speed: soft {after_soft:?} vs hard {after_hard:?}"
    );
}

/// The tunnel acceptance: the whole drill loop — query, engine policy,
/// terrain mutation, notify, reaction — is bit-stable across runs and
/// through a mid-tunnel snapshot.
#[test]
fn tunnel_is_hash_stable_with_streaming_edits() {
    let run = |ticks: u32| {
        let mut world = layered_world();
        let (id, tool) = mole(&mut world, vec3(2, 2, 2), 0);
        let mut carved = BTreeSet::new();
        for _ in 0..ticks {
            world.apply_impulse(id, vec3(48, 0, 0));
            drill_tick(&mut world, id, &tool, &mut carved, Fixed::from_int(120));
        }
        (world, carved)
    };
    let (a, carved_a) = run(300);
    let (b, carved_b) = run(300);
    assert_eq!(a.state_hash(), b.state_hash(), "tunnel diverged");
    assert_eq!(carved_a, carved_b, "carve streams diverged");

    // Snapshot mid-tunnel: restore and continue in lockstep with the
    // uninterrupted run (the carve set is engine state — the test
    // carries it alongside, as the engine would).
    let (mut w1, mut carved) = run(150);
    let json = serde_json::to_string(&w1).expect("serialize");
    let mut w2: PhysicsWorld = serde_json::from_str(&json).expect("deserialize");
    let mut carved2 = carved.clone();
    let (id, tool) = (w1.bodies()[0].id(), mole_tool());
    for _ in 0..150 {
        w1.apply_impulse(id, vec3(48, 0, 0));
        drill_tick(&mut w1, id, &tool, &mut carved, Fixed::from_int(120));
        w2.apply_impulse(id, vec3(48, 0, 0));
        drill_tick(&mut w2, id, &tool, &mut carved2, Fixed::from_int(120));
    }
    assert_eq!(w1.state_hash(), w2.state_hash(), "snapshot fork diverged");
}

fn mole_tool() -> DrillTool {
    DrillTool {
        anchor: FixedVec3::new(Fixed::from_ratio(5, 2), Fixed::ZERO, Fixed::ZERO),
        half_extents: FixedVec3::new(Fixed::ONE, Fixed::from_ratio(5, 2), Fixed::from_ratio(5, 2)),
        orientation: FixedQuat::IDENTITY,
    }
}

/// Cache invalidation on `notify_terrain_edit`: for an awake resting
/// body, a notify over its support purges terrain cache entries (hash
/// changes); a far notify touches nothing (hash unchanged). The
/// hardness field itself is hashed (tripwire).
#[test]
fn notify_invalidates_cache_and_hardness_is_hashed() {
    // Resting-but-awake cube: cache is warm, timers < SLEEP_TICKS.
    let mut world = layered_world();
    let mut shape = VoxelShape::new(2, 2, 2);
    shape.fill_box((0, 0, 0), (1, 1, 1), MaterialId(1));
    world.spawn_voxels(&VoxelBodyDef {
        shape,
        position: vec3(0, 0, 3),
        orientation: FixedQuat::IDENTITY,
        linear_velocity: FixedVec3::ZERO,
        angular_velocity: FixedVec3::ZERO,
    });
    let carved = BTreeSet::new();
    for _ in 0..20 {
        let field = Layered { carved: &carved };
        world.step(&field);
    }
    assert!(!world.bodies()[0].asleep(), "setup: still awake");
    let far = world.clone();
    let near = world.clone();

    let mut far = far;
    far.notify_terrain_edit((50, 50, -1), (52, 52, -1));
    assert_eq!(
        far.state_hash(),
        world.state_hash(),
        "far notify is a no-op"
    );

    let mut near = near;
    near.notify_terrain_edit((-2, -2, -1), (2, 2, -1));
    assert_ne!(
        near.state_hash(),
        world.state_hash(),
        "support-region notify must purge cache entries"
    );

    // Tripwire: hardness participates in the material fold.
    let mut a = PhysicsWorld::new(25);
    a.register_material(material(10));
    let mut b = PhysicsWorld::new(25);
    b.register_material(material(11));
    assert_ne!(a.state_hash(), b.state_hash(), "hardness must be hashed");
}

/// Degenerate tools panic loudly (a map-author bug, not data).
#[test]
#[should_panic(expected = "half_extents must be positive")]
fn zero_extent_tool_panics() {
    let mut world = layered_world();
    let id = world.spawn(&BodyDef::default());
    let _ = world.drill_reaction(
        id,
        &DrillTool {
            anchor: FixedVec3::ZERO,
            half_extents: FixedVec3::ZERO,
            orientation: FixedQuat::IDENTITY,
        },
        &[],
    );
}
