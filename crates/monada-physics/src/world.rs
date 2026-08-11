//! [`PhysicsWorld`] — the physics state root (docs/plans/voxel-physics.md
//! §3 `world`).
//!
//! The tick pipeline (gravity → wheels → narrowphase → velocity solve
//! → integrate → NGS → cache) plus the out-of-tick mutation surface:
//! spawns, impulses, wheel management, and P4 destruction.

use monada_fixed::{Fixed, FixedMat3, FixedQuat, FixedVec3};
use monada_sim::{StateHash, StateHasher};

use crate::body::{BodyDef, RigidBody, VoxelBodyDef};
use crate::broadphase;
use crate::contact::{self, ContactCacheEntry};
use crate::destruct::{self, DebrisCluster, Removal};
use crate::drill::{DrillSample, DrillTool};
use crate::field::VoxelField;
use crate::ids::BodyId;
use crate::material::{Material, MaterialId};
use crate::raycast;
use crate::shape::VoxelShape;
use crate::solver;
use crate::wheels::{self, Wheel, WheelDef, WheelId, WheelInput};

/// Default speed ceiling: 2000 voxels/s = 80 voxels/tick at 25 Hz.
const DEFAULT_MAX_SPEED: Fixed = Fixed::from_int(2000);

/// Sim-side rest thresholds: a body whose speeds sit below these is
/// "at rest" for acceptance purposes (actual sleeping arrives with
/// islands in P5). Linear in voxels/s, angular in rad/s.
pub const SLEEP_LINEAR: Fixed = Fixed::from_ratio(1, 8);
/// See [`SLEEP_LINEAR`].
pub const SLEEP_ANGULAR: Fixed = Fixed::from_ratio(1, 8);
/// Consecutive still ticks (1 s at 25 Hz) before an island may sleep.
pub const SLEEP_TICKS: u32 = 25;

/// The physics state root. One instance per sim; stepped once per sim
/// tick, hashed into the same desync digest as the rest of the sim.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhysicsWorld {
    /// Ticks stepped since construction. Part of the hashed state so a
    /// paused world and a stepped-but-empty world cannot collide.
    tick: u64,
    /// The fixed timestep, `1 / sim_hz` (docs/plans/voxel-physics.md:
    /// no `dt` parameter anywhere downstream of construction).
    dt: Fixed,
    /// Uniform gravity, voxels/s² in sim space (z-up — so a falling
    /// world sets a negative z). Defaults to zero: the map opts in.
    gravity: FixedVec3,
    /// Speed ceiling, applied right after gravity each tick. A hashed
    /// config field (no setter until a demo needs one — plan, P2
    /// amendments).
    max_speed: Fixed,
    /// The next id [`spawn`](PhysicsWorld::spawn) hands out. Monotonic,
    /// hashed (rule 3: id allocation is simulation state).
    next_body_id: u64,
    /// All live bodies, ascending by id — spawn pushes fresh (larger)
    /// ids, P4 despawn/split removes and appends in order, so the Vec
    /// stays sorted through every mutation.
    bodies: Vec<RigidBody>,
    /// Registered materials; `MaterialId` indexes this in registration
    /// order (the cross-crate contract on [`VoxelField`]).
    materials: Vec<Material>,
    /// Warm-start impulse cache, sorted by contact key — canonical by
    /// construction (generation order is lexicographic) and hashed:
    /// accumulated impulses feed the next tick's solve (plan, P2
    /// amendments).
    impulse_cache: Vec<ContactCacheEntry>,
    /// Connected components SMALLER than this many voxels become
    /// debris instead of bodies — including the would-be survivor
    /// (a body degrades to debris and despawns). Hashed config;
    /// default 3. Values ≤ 1 disable debris entirely.
    debris_threshold: u32,
}

impl PhysicsWorld {
    /// A world stepping at `sim_hz` ticks per second (the map's
    /// `sim_hz`; the engine default is 25).
    ///
    /// # Panics
    /// Panics if `sim_hz` is zero.
    #[must_use]
    pub fn new(sim_hz: u32) -> PhysicsWorld {
        assert!(sim_hz > 0, "PhysicsWorld::new: sim_hz must be non-zero");
        PhysicsWorld {
            tick: 0,
            // Lossless: manifest sim_hz is far below i32::MAX.
            dt: Fixed::from_ratio(1, i32::try_from(sim_hz).expect("sim_hz fits i32")),
            gravity: FixedVec3::ZERO,
            max_speed: DEFAULT_MAX_SPEED,
            next_body_id: 0,
            bodies: Vec::new(),
            materials: Vec::new(),
            impulse_cache: Vec::new(),
            debris_threshold: 3,
        }
    }

    /// Set the debris threshold (see the field: components smaller
    /// than this become [`DebrisCluster`]s, survivors included).
    pub fn set_debris_threshold(&mut self, voxels: u32) {
        self.debris_threshold = voxels;
    }

    /// Ticks stepped since construction.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The fixed timestep in seconds.
    #[must_use]
    pub fn dt(&self) -> Fixed {
        self.dt
    }

    /// Uniform gravity, voxels/s².
    #[must_use]
    pub fn gravity(&self) -> FixedVec3 {
        self.gravity
    }

    /// Set uniform gravity (hashed state — a mid-run change re-keys
    /// the stream, as it must). Wakes EVERY sleeping body: a gravity
    /// flip must not leave sleepers hanging mid-air (P5 amendments).
    pub fn set_gravity(&mut self, gravity: FixedVec3) {
        if gravity != self.gravity {
            for body in &mut self.bodies {
                body.asleep = false;
                body.sleep_timer = 0;
            }
        }
        self.gravity = gravity;
    }

    /// Register a material; ids are handed out in call order (which is
    /// therefore part of the determinism contract, like body ids).
    ///
    /// # Panics
    /// Panics past 65534 materials (65535 is the shape sentinel).
    pub fn register_material(&mut self, material: Material) -> MaterialId {
        let id = u16::try_from(self.materials.len()).expect("material table fits u16");
        assert!(id != u16::MAX, "material id 65535 is reserved");
        self.materials.push(material);
        MaterialId(id)
    }

    /// A registered material, by id — the read half of
    /// [`register_material`](PhysicsWorld::register_material). The
    /// engine-side drill cut policy consumes this (plan §4: physics
    /// reports overlaps, the ENGINE decides what gets cut from
    /// hardness — so hardness must be readable across the wall).
    ///
    /// # Panics
    /// Panics on an unregistered id (a cross-crate contract violation,
    /// like the terrain-material assert in narrowphase).
    #[must_use]
    pub fn material(&self, id: MaterialId) -> Material {
        *self
            .materials
            .get(usize::from(id.0))
            .unwrap_or_else(|| panic!("material {} is not registered", id.0))
    }

    /// Spawn a ghost body — explicit mass properties, **no collision
    /// skin** (it falls through terrain by design; see `BodyDef`).
    ///
    /// # Panics
    /// Panics on an invalid [`BodyDef`] (non-positive mass, inertia
    /// tensor that is not positive-definite).
    pub fn spawn(&mut self, def: &BodyDef) -> BodyId {
        let id = self.next_id();
        self.bodies.push(RigidBody::from_def(id, def));
        id
    }

    /// Spawn a voxel body: mass/CoM/inertia derived from the shape and
    /// material densities (solid-cube convention), surface voxels as
    /// the collision skin. `def.position` places the derived `CoM`.
    ///
    /// # Panics
    /// Panics on an empty shape or a material id the world has not
    /// registered.
    pub fn spawn_voxels(&mut self, def: &VoxelBodyDef) -> BodyId {
        let id = self.next_id();
        self.bodies
            .push(RigidBody::from_voxels(id, def, &self.materials));
        id
    }

    fn next_id(&mut self) -> BodyId {
        let id = BodyId(self.next_body_id);
        self.next_body_id += 1;
        id
    }

    /// The body with `id`, if it is (still) alive.
    #[must_use]
    pub fn body(&self, id: BodyId) -> Option<&RigidBody> {
        self.bodies
            .binary_search_by_key(&id, RigidBody::id)
            .ok()
            .map(|i| &self.bodies[i])
    }

    /// All live bodies, ascending by id — the canonical iteration
    /// order (also the hash fold order).
    #[must_use]
    pub fn bodies(&self) -> &[RigidBody] {
        &self.bodies
    }

    /// Apply a world-frame impulse at the centre of mass: `Δv = J/m`.
    ///
    /// # Panics
    /// Panics on an unknown body id.
    pub fn apply_impulse(&mut self, id: BodyId, impulse: FixedVec3) {
        let body = self.body_mut(id);
        body.linear_velocity += impulse.scale(body.inv_mass);
    }

    /// Apply a world-frame impulse at a world-space `point`:
    /// `Δv = J/m`, `Δω = I⁻¹_world·((point − com) × J)`.
    ///
    /// # Panics
    /// Panics on an unknown body id.
    pub fn apply_impulse_at(&mut self, id: BodyId, impulse: FixedVec3, point: FixedVec3) {
        let index = self
            .bodies
            .binary_search_by_key(&id, RigidBody::id)
            .unwrap_or_else(|_| panic!("apply_impulse_at: no body {id:?}"));
        let inv_inertia = solver::inv_inertia_world(&self.bodies[index]);
        let body = &mut self.bodies[index];
        body.asleep = false;
        body.sleep_timer = 0;
        let r = point - body.position;
        body.linear_velocity += impulse.scale(body.inv_mass);
        body.angular_velocity += inv_inertia * r.cross(impulse);
    }

    /// Apply a world-frame ANGULAR impulse: `Δω = I⁻¹_world·L`.
    ///
    /// The couple that [`apply_impulse_at`](PhysicsWorld::apply_impulse_at)
    /// cannot express. An off-centre impulse always turns *and* shoves;
    /// a gyro, a reaction wheel or an attitude thruster pair turns
    /// without shoving, and expressing that as two opposing impulses at
    /// ±r would round twice and drift. `L` is `torque · dt` — the same
    /// impulse convention the linear pair uses (docs/plans/ship-physics.md
    /// D6).
    ///
    /// # Panics
    /// Panics on an unknown body id.
    pub fn apply_angular_impulse(&mut self, id: BodyId, angular_impulse: FixedVec3) {
        let index = self
            .bodies
            .binary_search_by_key(&id, RigidBody::id)
            .unwrap_or_else(|_| panic!("apply_angular_impulse: no body {id:?}"));
        // The world inertia depends on the CURRENT orientation, so read it
        // before taking the mutable borrow — the same order `apply_impulse_at`
        // keeps.
        let inv_inertia = solver::inv_inertia_world(&self.bodies[index]);
        let body = &mut self.bodies[index];
        body.asleep = false;
        body.sleep_timer = 0;
        body.angular_velocity += inv_inertia * angular_impulse;
    }

    /// The external mutation surface funnels through here — and every
    /// external mutation wakes (P5): impulses, wheel attach/detach,
    /// wheel input. `remove_voxels` and `set_gravity` wake on their
    /// own paths.
    fn body_mut(&mut self, id: BodyId) -> &mut RigidBody {
        let index = self
            .bodies
            .binary_search_by_key(&id, RigidBody::id)
            .unwrap_or_else(|_| panic!("no body {id:?}"));
        let body = &mut self.bodies[index];
        body.asleep = false;
        body.sleep_timer = 0;
        body
    }

    /// Attach a raycast wheel to `body` (see the `wheels` module for
    /// the suspension model). Wheel ids are per-body, monotonic, never
    /// reused. Ghost bodies may carry wheels — the suspension raycasts
    /// terrain and needs no collision skin (a hover-cart; useful for
    /// isolating suspension in tests).
    ///
    /// # Panics
    /// Panics on an unknown body id.
    pub fn attach_wheel(&mut self, body: BodyId, def: &WheelDef) -> WheelId {
        let body = self.body_mut(body);
        let id = WheelId(body.next_wheel_id);
        body.next_wheel_id += 1;
        body.wheels.push(Wheel {
            id,
            def: *def,
            input: WheelInput::default(),
        });
        id
    }

    /// Detach a wheel (the P3 lost-wheel acceptance path). The id is
    /// retired, not reused.
    ///
    /// # Panics
    /// Panics on an unknown body id or a wheel that is not attached.
    pub fn detach_wheel(&mut self, body: BodyId, wheel: WheelId) {
        let body = self.body_mut(body);
        let index = body
            .wheel_index(wheel)
            .unwrap_or_else(|| panic!("detach_wheel: no wheel {wheel:?} on body"));
        body.wheels.remove(index);
    }

    /// Set a wheel's control input. Retained (hashed) state — it holds
    /// until the next call, matching the lockstep command model.
    ///
    /// # Panics
    /// Panics on an unknown body id or a wheel that is not attached.
    pub fn set_wheel_input(&mut self, body: BodyId, wheel: WheelId, input: WheelInput) {
        let body = self.body_mut(body);
        let index = body
            .wheel_index(wheel)
            .unwrap_or_else(|| panic!("set_wheel_input: no wheel {wheel:?} on body"));
        body.wheels[index].input = input;
    }

    /// Carve `cells` (SHAPE-local coordinates — the body's as-authored
    /// grid; note a split-off fragment gets a rebased tight grid, so a
    /// later `remove_voxels` on the fragment speaks the fragment's own
    /// coordinates, not the parent's) out of a voxel body,
    /// synchronously: incremental mass update of the survivor,
    /// 6-connected flood fill, splits, debris, skin re-derive, wheel
    /// re-anchor/auto-detach, warm-start cache purge.
    ///
    /// Empty / out-of-bounds cells are skipped silently and duplicates
    /// collapse (contract, not accident — the P6 drill may
    /// over-approximate freely). `removed == 0` guarantees zero state
    /// change.
    ///
    /// **No-teleport invariant**: every surviving voxel keeps both its
    /// world position AND its world point velocity across the call —
    /// only the `CoM` bookkeeping moves (`position += R·Δcom`,
    /// `v += ω × R·Δcom`, wheel anchors and skin offsets shift by
    /// `−Δcom`).
    ///
    /// # Panics
    /// Panics on an unknown body id or a ghost body (no shape).
    #[allow(clippy::too_many_lines, clippy::missing_panics_doc)]
    pub fn remove_voxels(&mut self, body: BodyId, cells: &[(i32, i32, i32)]) -> Removal {
        let index = self
            .bodies
            .binary_search_by_key(&body, RigidBody::id)
            .unwrap_or_else(|_| panic!("remove_voxels: no body {body:?}"));
        assert!(
            self.bodies[index].shape.is_some(),
            "remove_voxels: body {body:?} is a ghost (no shape)"
        );

        // Pre-carve snapshot: pose, mass properties, occupied cells.
        let position = self.bodies[index].position;
        let orientation = self.bodies[index].orientation;
        let velocity = self.bodies[index].linear_velocity;
        let omega = self.bodies[index].angular_velocity;
        let mass_old = self.bodies[index].mass;
        let inertia_old = self.bodies[index].inertia_body;
        let com_old = self.bodies[index].com_local;
        let pre_cells: Vec<(i32, i32, i32)> = {
            let shape = self.bodies[index].shape.as_ref().expect("checked above");
            shape
                .occupied_cells()
                .map(|(x, y, z, _)| (x, y, z))
                .collect()
        };

        // Carve. `(centre − com_old, density)` per removed voxel feeds
        // the incremental update below.
        let mut departed: Vec<(FixedVec3, Fixed)> = Vec::new();
        {
            let body_ref = &mut self.bodies[index];
            let shape = body_ref.shape.as_mut().expect("checked above");
            for &(x, y, z) in cells {
                if let Some(mat) = shape.clear(x, y, z) {
                    departed.push((
                        destruct::cell_center((x, y, z)) - com_old,
                        self.materials[usize::from(mat.0)].density,
                    ));
                }
            }
        }
        let removed = u32::try_from(departed.len()).expect("bounded by shape size");
        if removed == 0 {
            return Removal {
                removed: 0,
                survivor: Some(body),
                split_off: Vec::new(),
                debris: Vec::new(),
                detached_wheels: Vec::new(),
            };
        }

        // Connectivity over what remains (parent as-authored grid).
        let comps = {
            let shape = self.bodies[index].shape.as_ref().expect("checked above");
            destruct::components(shape, &self.materials)
        };
        let threshold = usize::try_from(self.debris_threshold).expect("small");

        // Identity: the heaviest at-or-above-threshold component keeps
        // the BodyId; ties break to the lexicographically smallest
        // min_cell. Sub-threshold components — the would-be survivor
        // included — degrade to debris.
        let keeper = comps
            .iter()
            .enumerate()
            .filter(|(_, c)| c.cells.len() >= threshold)
            .max_by(|(_, a), (_, b)| {
                a.mass.cmp(&b.mass).then(b.min_cell.cmp(&a.min_cell)) // smaller cell wins ties
            })
            .map(|(i, _)| i);

        // Rigid-body point kinematics for a departing cluster at
        // shape-space CoM `c`.
        let kinematics = |com_shape: FixedVec3| {
            let r_world = orientation * (com_shape - com_old);
            (position + r_world, velocity + omega.cross(r_world))
        };

        // Fragment defs and debris clusters, both built against the
        // still-intact post-carve shape, ordered by min_cell (comps
        // arrive sorted).
        let mut fragment_defs: Vec<VoxelBodyDef> = Vec::new();
        let mut debris: Vec<DebrisCluster> = Vec::new();
        for (i, comp) in comps.iter().enumerate() {
            if Some(i) == keeper {
                continue;
            }
            let shape = self.bodies[index].shape.as_ref().expect("checked above");
            let (pos, vel) = kinematics(comp.com);
            if comp.cells.len() >= threshold {
                let min = comp.cells.iter().fold(comp.cells[0], |m, &c| {
                    (m.0.min(c.0), m.1.min(c.1), m.2.min(c.2))
                });
                let max = comp.cells.iter().fold(comp.cells[0], |m, &c| {
                    (m.0.max(c.0), m.1.max(c.1), m.2.max(c.2))
                });
                let mut tight =
                    VoxelShape::new(max.0 - min.0 + 1, max.1 - min.1 + 1, max.2 - min.2 + 1);
                for &(x, y, z) in &comp.cells {
                    tight.set(
                        x - min.0,
                        y - min.1,
                        z - min.2,
                        shape.get(x, y, z).expect("component cell occupied"),
                    );
                }
                fragment_defs.push(VoxelBodyDef {
                    shape: tight,
                    position: pos,
                    orientation,
                    linear_velocity: vel,
                    angular_velocity: omega,
                });
            } else {
                debris.push(DebrisCluster {
                    position: pos,
                    orientation,
                    linear_velocity: vel,
                    voxels: comp
                        .cells
                        .iter()
                        .map(|&(x, y, z)| {
                            (
                                destruct::cell_center((x, y, z)) - comp.com,
                                shape.get(x, y, z).expect("component cell occupied"),
                            )
                        })
                        .collect(),
                });
            }
        }

        // Survivor update or despawn.
        let mut detached_wheels = Vec::new();
        let survivor = if let Some(keeper_index) = keeper {
            let keeper_comp = &comps[keeper_index];
            let keeper_cells: std::collections::BTreeSet<(i32, i32, i32)> =
                keeper_comp.cells.iter().copied().collect();

            // Everything not in the keeper departs: already-carved
            // voxels are in `departed`; add the other components'.
            {
                let body_ref = &mut self.bodies[index];
                let shape = body_ref.shape.as_mut().expect("checked above");
                for (i, comp) in comps.iter().enumerate() {
                    if i == keeper_index {
                        continue;
                    }
                    for &(x, y, z) in &comp.cells {
                        let mat = shape.clear(x, y, z).expect("component cell occupied");
                        departed.push((
                            destruct::cell_center((x, y, z)) - com_old,
                            self.materials[usize::from(mat.0)].density,
                        ));
                    }
                }
            }

            // Incremental mass properties (plan §6 P4): subtract each
            // departed voxel's contribution about com_old, then
            // parallel-axis the tensor to com_new. The full-recompute
            // path stays in tests as the reference.
            // Rule 6 in miniature: the CoM update works entirely in
            // relative offsets — `com_new = com_old − Σρ·d / M'` with
            // d = centre − com_old (already in hand for the tensor).
            // Reconstructing `com·mass` instead would amplify the
            // stored CoM's rounding by the body mass.
            let mut mass_new = mass_old;
            let mut moment = FixedVec3::ZERO;
            let mut inertia = inertia_old;
            for &(d, density) in &departed {
                mass_new -= density;
                moment += d.scale(density);
                inertia = inertia - crate::shape::voxel_inertia(density, d);
            }
            let shift = -crate::shape::div_by(moment, mass_new);
            let com_new = com_old + shift;
            let dd = shift.dot(shift);
            inertia = inertia
                - (FixedMat3::from_diagonal(FixedVec3::new(dd, dd, dd))
                    - crate::shape::outer(shift))
                .scale(mass_new);
            // The survivor bypasses RigidBody::build — re-assert SPD
            // here, where incremental drift would first surface.
            debug_assert!(
                inertia.leading_minors_positive(),
                "incremental inertia update lost positive-definiteness"
            );

            let world_shift = orientation * shift;
            let body_ref = &mut self.bodies[index];
            body_ref.mass = mass_new;
            body_ref.inv_mass = Fixed::ONE / mass_new;
            body_ref.inertia_body = inertia;
            body_ref.inv_inertia_body = inertia.inverse();
            body_ref.com_local = com_new;
            body_ref.skin =
                crate::shape::derive_skin(body_ref.shape.as_ref().expect("survivor"), com_new);
            body_ref.bounding_radius = body_ref
                .skin
                .iter()
                .map(|s| s.offset.length())
                .max()
                .map_or(Fixed::ZERO, |m| m + crate::shape::SKIN_RADIUS);
            // Whatever carved it certainly wakes it (P5): the woken
            // body re-settles with correct fresh bookkeeping.
            body_ref.asleep = false;
            body_ref.sleep_timer = 0;
            // No-teleport: the shape stays put in the world; the CoM
            // bookkeeping moves around it — including each voxel's
            // point VELOCITY (the new CoM point moved at v + ω×Δ).
            body_ref.position += world_shift;
            body_ref.linear_velocity += omega.cross(world_shift);
            // Wheels: bolted to structure, not to the CoM. Re-anchor
            // survivors by −Δcom; detach wheels whose nearest occupied
            // pre-carve cell departed.
            let mut kept = Vec::with_capacity(body_ref.wheels.len());
            for mut wheel in std::mem::take(&mut body_ref.wheels) {
                let anchor_shape = wheel.def.anchor + com_old;
                let home = destruct::nearest_cell(anchor_shape, &pre_cells);
                if keeper_cells.contains(&home) {
                    wheel.def.anchor -= shift;
                    kept.push(wheel);
                } else {
                    detached_wheels.push(wheel.id);
                }
            }
            body_ref.wheels = kept;
            Some(body)
        } else {
            // A fully-degraded body takes no wheels with it: they ALL
            // detach, and the engine hears about every one — the
            // most spectacular case (a vehicle blown to debris) must
            // not be the one that reports nothing.
            detached_wheels.extend(self.bodies[index].wheels.iter().map(|w| w.id));
            self.bodies.remove(index);
            None
        };

        // New bodies (ids monotonic, in min_cell order — the Vec stays
        // sorted because fresh ids exceed every live one).
        let split_off: Vec<BodyId> = fragment_defs
            .iter()
            .map(|def| {
                let id = self.next_id();
                self.bodies
                    .push(RigidBody::from_voxels(id, def, &self.materials));
                id
            })
            .collect();

        // Skin indices (and the body itself) may be gone — stale
        // warm-start keys are purged, not left to miss deterministically.
        self.impulse_cache.retain(|e| e.key.body != body);

        Removal {
            removed,
            survivor,
            split_off,
            debris,
            detached_wheels,
        }
    }

    /// Advance one fixed tick against the terrain `field` (a read-only
    /// per-tick input — physics never owns terrain).
    ///
    /// Pipeline, in canonical order (plan §3, P2 amendments):
    ///
    /// ```text
    /// 1. v += g·dt; v clamped to max_speed
    /// 2. narrowphase: bodies by id → skin spheres by index → cells
    /// 3. warm start from the impulse cache
    /// 4. velocity iterations: normal impulses + Coulomb friction
    /// 5. integrate positions/orientations (P1 integrator, ω constant)
    /// 6. full-K NGS position iterations
    /// 7. rebuild the impulse cache from surviving contacts
    /// ```
    ///
    /// **Overflow policy (rule 5)**: plain wrapping ops past the
    /// clamp. The `max_speed` ceiling (80 voxels/tick at defaults) is
    /// simultaneously the tunnelling bound — a step never moves a body
    /// further than that, so terrain thinner than `max_speed·dt` can
    /// be skipped over. Today's column terrain is solid to the bottom;
    /// for future walls/overhangs the map must pick `max_speed` (or
    /// wall thickness) accordingly. Fast projectiles stay engine-side
    /// raycasts (non-goal).
    pub fn step(&mut self, field: &dyn VoxelField) {
        // 1. Integrate velocities (awake bodies only — sleepers are
        // frozen).
        let g_dt = self.gravity.scale(self.dt);
        for body in &mut self.bodies {
            if !body.asleep {
                body.linear_velocity =
                    (body.linear_velocity + g_dt).clamp_length_max(self.max_speed);
            }
        }

        // 1½. Wheel pass — before the contact solver, so a chassis
        // bottoming out on a step is still corrected by NGS.
        wheels::wheel_pass(&mut self.bodies, field, &self.materials, g_dt, self.dt);

        // 2. Narrowphase at pre-step poses — PAIRS FIRST, so a sleeper
        // woken by a real contact gets its terrain contacts in the
        // same tick (terrain-first left a struck sleeper floorless for
        // one tick: the impact drove it into the ground and NGS heaved
        // it back — a flicker-and-pump cycle). A MIXED pair (awake +
        // asleep) runs narrowphase, and only a real contact wakes the
        // sleeper — broadphase adjacency alone never does (no
        // sleep-thrash from drive-bys). Sleeping-sleeping pairs are
        // skipped whole.
        let mut contacts = Vec::new();
        for (a, b) in broadphase::candidate_pairs(&self.bodies) {
            if self.bodies[a].asleep && self.bodies[b].asleep {
                continue;
            }
            let pair = contact::generate_pair(&self.bodies, &self.materials, a, b);
            if !pair.is_empty() {
                for index in [a, b] {
                    let body = &mut self.bodies[index];
                    if body.asleep {
                        // Woken by a real contact: participates fully
                        // in this tick's solve, terrain included (it
                        // skipped gravity at phase 1 — invisible for
                        // one tick). A stack wakes as a wave, one
                        // layer per tick: sleeping neighbours make no
                        // contacts until awake.
                        body.asleep = false;
                        body.sleep_timer = 0;
                    }
                }
                contacts.extend(pair);
            }
        }
        contacts.extend(contact::generate_terrain(
            &self.bodies,
            field,
            &self.materials,
        ));
        // Canonical order: with pair contacts the generation order is
        // no longer globally lexicographic — one deterministic sort
        // restores it (P5 revision of "sorted by construction").
        contacts.sort_unstable_by_key(|x| x.key);

        // Per-body world-frame inverse inertia for this tick's solve.
        let inv_inertias: Vec<_> = self.bodies.iter().map(solver::inv_inertia_world).collect();

        // 3. Load accumulated impulses from the previous tick's cache.
        for c in &mut contacts {
            if let Ok(i) = self.impulse_cache.binary_search_by_key(&c.key, |e| e.key) {
                let e = &self.impulse_cache[i];
                c.accumulated_normal = e.normal_impulse;
                c.accumulated_tangent = e.tangent_impulse;
            }
        }
        solver::prepare(&mut self.bodies, &inv_inertias, &mut contacts, self.dt);

        // 4. Velocity solve.
        solver::solve_velocities(&mut self.bodies, &inv_inertias, &mut contacts);

        // 5. Integrate poses (P1 integrator; no gyroscopic term).
        for body in &mut self.bodies {
            if body.asleep {
                continue;
            }
            body.position += body.linear_velocity.scale(self.dt);
            body.orientation = (FixedQuat::from_scaled_axis(body.angular_velocity.scale(self.dt))
                * body.orientation)
                .normalize();
        }

        // 6. Position correction.
        solver::solve_positions(&mut self.bodies, &contacts);

        // 7. Persist accumulated impulses.
        self.impulse_cache.clear();
        self.impulse_cache.extend(
            contacts
                .iter()
                .filter(|c| {
                    c.accumulated_normal != Fixed::ZERO
                        || c.accumulated_tangent.0 != Fixed::ZERO
                        || c.accumulated_tangent.1 != Fixed::ZERO
                })
                .map(|c| ContactCacheEntry {
                    key: c.key,
                    normal_impulse: c.accumulated_normal,
                    tangent_impulse: c.accumulated_tangent,
                }),
        );
        // Tripwire for the post-generation sort: the binary_search
        // warm-start lookup silently degrades if this ever breaks
        // (strictly ascending — duplicate keys are impossible by
        // construction).
        debug_assert!(
            self.impulse_cache.windows(2).all(|w| w[0].key < w[1].key),
            "impulse cache not sorted — the contact sort lost canonical order"
        );

        // 8. Sleep bookkeeping (P5): per-body low-motion timers, then
        // island-wide sleep — a contact island (union-find over this
        // tick's pair contacts) sleeps only when EVERY member has been
        // still for SLEEP_TICKS; velocities zero on the way down
        // (kills micro-drift in the hashed state).
        self.update_sleep(&contacts);

        self.tick += 1;
    }

    /// Union-find islands over this tick's pair contacts, then the
    /// all-still rule per island. See `step` phase 8.
    fn update_sleep(&mut self, contacts: &[contact::Contact]) {
        fn find(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }

        for body in &mut self.bodies {
            if body.asleep {
                continue;
            }
            // A POWERED wheel keeps its body awake (digger D3): retained
            // drive is intent — a vehicle grinding against a wall (or a
            // drill reaction) at near-zero velocity must not doze off
            // mid-throttle, because nothing would ever wake it to act on
            // that throttle again. Brake/steer at rest are fine to sleep
            // through — they hold, they don't move.
            let powered = body.wheels.iter().any(|w| w.input().drive != Fixed::ZERO);
            if !powered
                && body.linear_velocity.length() < SLEEP_LINEAR
                && body.angular_velocity.length() < SLEEP_ANGULAR
            {
                body.sleep_timer = body.sleep_timer.saturating_add(1);
            } else {
                body.sleep_timer = 0;
            }
        }

        // Union-find (transient, index-based, deterministic).
        let mut parent: Vec<usize> = (0..self.bodies.len()).collect();
        for c in contacts {
            if let Some(other) = c.other_index {
                let (ra, rb) = (find(&mut parent, c.body_index), find(&mut parent, other));
                parent[ra.max(rb)] = ra.min(rb);
            }
        }
        // A root's island may sleep only if every member is eligible.
        // Ghosts (empty skin) NEVER sleep: nothing can ever touch one
        // awake again, so a sleeping mid-air ghost would hang frozen
        // forever — they stay integrating instead.
        let mut island_ready = vec![true; self.bodies.len()];
        for i in 0..self.bodies.len() {
            let root = find(&mut parent, i);
            let body = &self.bodies[i];
            if !body.asleep && (body.sleep_timer < SLEEP_TICKS || body.skin.is_empty()) {
                island_ready[root] = false;
            }
        }
        for i in 0..self.bodies.len() {
            let root = find(&mut parent, i);
            let body = &mut self.bodies[i];
            if !body.asleep && island_ready[root] && body.sleep_timer >= SLEEP_TICKS {
                body.asleep = true;
                body.linear_velocity = FixedVec3::ZERO;
                body.angular_velocity = FixedVec3::ZERO;
            }
        }
    }

    /// A terrain edit happened in the INCLUSIVE cell box `[min, max]`
    /// (the engine calls this after `voxel_clear`-style changes):
    ///
    /// - wakes sleeping bodies whose bounding sphere
    ///   (`position ± bounding_radius`) overlaps the box — the ground
    ///   may have vanished from under a sleeper;
    /// - purges warm-start cache entries against terrain cells inside
    ///   the box (P6). A stale entry is inert while its cell stays
    ///   empty, but if the engine BUILDS the cell back, a regenerated
    ///   contact would warm-start with another era's impulse. Only
    ///   awake bodies can have cache entries at all — a sleeper's
    ///   entries drop at the first cache rebuild after it sleeps
    ///   (P5 mechanics) — so both halves of this method concern the
    ///   same boundary between the box and live state.
    ///
    /// Both halves read the box with the same inclusive convention:
    /// cells `min ..= max` on every axis.
    pub fn notify_terrain_edit(&mut self, min: (i64, i64, i64), max: (i64, i64, i64)) {
        for body in &mut self.bodies {
            if !body.asleep {
                continue;
            }
            let r = body.bounding_radius;
            // Closest point of the inclusive cell box to the CoM: the
            // box spans [min, max+1) in world units per axis.
            let clamp = |v: Fixed, lo: i64, hi: i64| {
                v.clamp(Fixed::from_bits(lo << 32), Fixed::from_bits((hi + 1) << 32))
            };
            let closest = FixedVec3::new(
                clamp(body.position.x, min.0, max.0),
                clamp(body.position.y, min.1, max.1),
                clamp(body.position.z, min.2, max.2),
            );
            let d = body.position - closest;
            if d.dot(d) <= r * r {
                body.asleep = false;
                body.sleep_timer = 0;
            }
        }
        let cell_in_box = |c: (i64, i64, i64)| {
            (min.0..=max.0).contains(&c.0)
                && (min.1..=max.1).contains(&c.1)
                && (min.2..=max.2).contains(&c.2)
        };
        self.impulse_cache
            .retain(|e| e.key.other.is_some() || !cell_in_box(e.key.cell));
    }

    /// Read-only: the terrain cells the drill tool currently overlaps
    /// (cell CENTRE inside the tool's oriented box, boundary
    /// INCLUSIVE — a centre exactly on a face counts), in canonical
    /// (x, y, z) order with empty cells skipped. Terrain material ids
    /// obey the same cross-crate contract as narrowphase. The ENGINE
    /// decides what actually gets cut — hardness tables, tool power,
    /// and wear all live on its side of the wall (plan §4).
    ///
    /// # Panics
    /// Panics on an unknown body id, non-positive `half_extents`, or
    /// an unregistered terrain material. Ghost bodies are fine — the
    /// query needs a pose, not a skin.
    #[must_use]
    pub fn drill_query(
        &self,
        body: BodyId,
        tool: &DrillTool,
        field: &dyn VoxelField,
    ) -> Vec<DrillSample> {
        let body = self
            .body(body)
            .unwrap_or_else(|| panic!("drill_query: no body {body:?}"));
        assert!(
            tool.half_extents.x > Fixed::ZERO
                && tool.half_extents.y > Fixed::ZERO
                && tool.half_extents.z > Fixed::ZERO,
            "DrillTool: half_extents must be positive, got {:?}",
            tool.half_extents
        );
        let center = body.position() + body.orientation() * tool.anchor;
        // The box frame composes the tool-in-body rotation (D3: a
        // pitched nose) with the body orientation; the anchor point
        // itself stays body-frame (the tool pivots about it).
        let box_rot = body.orientation() * tool.orientation;
        let inv_rot = box_rot.inverse();
        // World AABB of the oriented box: per world axis, the reach is
        // Σ |R column| · half_extent.
        let rot = FixedMat3::from_quat(box_rot);
        let reach = |ax: Fixed, ay: Fixed, az: Fixed| {
            ax.abs() * tool.half_extents.x
                + ay.abs() * tool.half_extents.y
                + az.abs() * tool.half_extents.z
        };
        let rx = reach(rot.x_axis.x, rot.y_axis.x, rot.z_axis.x);
        let ry = reach(rot.x_axis.y, rot.y_axis.y, rot.z_axis.y);
        let rz = reach(rot.x_axis.z, rot.y_axis.z, rot.z_axis.z);

        let lo = |v: Fixed, r: Fixed| i64::from((v - r).floor_to_int());
        let hi = |v: Fixed, r: Fixed| i64::from((v + r).floor_to_int());
        let mut out = Vec::new();
        for cx in lo(center.x, rx)..=hi(center.x, rx) {
            for cy in lo(center.y, ry)..=hi(center.y, ry) {
                for cz in lo(center.z, rz)..=hi(center.z, rz) {
                    if !field.occupied(cx, cy, cz) {
                        continue;
                    }
                    let cell_center = FixedVec3::new(
                        Fixed::from_bits(cx << 32) + Fixed::HALF,
                        Fixed::from_bits(cy << 32) + Fixed::HALF,
                        Fixed::from_bits(cz << 32) + Fixed::HALF,
                    );
                    // Into the tool's box frame (body axes).
                    let local = inv_rot * (cell_center - center);
                    if local.x.abs() <= tool.half_extents.x
                        && local.y.abs() <= tool.half_extents.y
                        && local.z.abs() <= tool.half_extents.z
                    {
                        let material = field.material(cx, cy, cz);
                        assert!(
                            usize::from(material.0) < self.materials.len(),
                            "terrain returned material {}, world has {} registered",
                            material.0,
                            self.materials.len()
                        );
                        out.push(DrillSample {
                            cell: (cx, cy, cz),
                            material,
                        });
                    }
                }
            }
        }
        out
    }

    /// Apply the drill reaction from what the engine actually cut
    /// this tick: `Σ hardness(cut)` acts as a force OPPOSING the tool
    /// point's velocity, applied at the tool centre (regardless of
    /// which overlapped cells were the ones cut — an asymmetric cut
    /// exerts no side torque; another documented chunky allowance).
    /// The impulse is clamped through the point's EFFECTIVE mass along
    /// the motion direction — the P3 brake-clamp math — so it can stop
    /// the tool point but never reverse it (a naive `m·|v|` clamp
    /// would fling an off-CoM nose backwards). A still tool point
    /// (zero velocity) takes no reaction — a deterministic branch,
    /// like the degenerate steer. Wakes the body unconditionally,
    /// zero-impulse branches included. Returns the applied impulse
    /// magnitude (engine-side feedback: tool wear, sparks, audio).
    ///
    /// # Panics
    /// Panics on an unknown body id, non-positive `half_extents`, or
    /// an unregistered material in `cut`.
    pub fn drill_reaction(&mut self, body: BodyId, tool: &DrillTool, cut: &[MaterialId]) -> Fixed {
        assert!(
            tool.half_extents.x > Fixed::ZERO
                && tool.half_extents.y > Fixed::ZERO
                && tool.half_extents.z > Fixed::ZERO,
            "DrillTool: half_extents must be positive, got {:?}",
            tool.half_extents
        );
        let mut force = Fixed::ZERO;
        for mat in cut {
            force += self
                .materials
                .get(usize::from(mat.0))
                .unwrap_or_else(|| {
                    panic!(
                        "drill_reaction: cut material {}, world has {} registered",
                        mat.0,
                        self.materials.len()
                    )
                })
                .hardness;
        }
        let index = self
            .bodies
            .binary_search_by_key(&body, RigidBody::id)
            .unwrap_or_else(|_| panic!("drill_reaction: no body {body:?}"));
        let inv_inertia = solver::inv_inertia_world(&self.bodies[index]);
        let body_ref = &mut self.bodies[index];
        body_ref.asleep = false;
        body_ref.sleep_timer = 0;
        let point = body_ref.position + body_ref.orientation * tool.anchor;
        let r = point - body_ref.position;
        let v_point = body_ref.linear_velocity + body_ref.angular_velocity.cross(r);
        let dir = v_point.normalize();
        if dir == FixedVec3::ZERO || force == Fixed::ZERO {
            return Fixed::ZERO;
        }
        let m_eff = solver::effective_mass(body_ref, &inv_inertia, r, dir);
        let applied = (force * self.dt).min(m_eff * v_point.length());
        let impulse = dir.scale(-applied);
        body_ref.linear_velocity += impulse.scale(body_ref.inv_mass);
        body_ref.angular_velocity += inv_inertia * r.cross(impulse);
        applied
    }

    /// Cast a ray against terrain and every voxel body (P5): the
    /// engine-side seam for fast projectiles (plan §8 — no CCD).
    ///
    /// Contract:
    /// - `dir` must be unit length; `t` is in voxels along it.
    /// - Nearest hit wins; exact ties go to terrain, then to the
    ///   lowest body id (bodies are scanned ascending).
    /// - Ghost bodies are invisible to rays (no shape — consistent
    ///   with having no skin).
    /// - Sleeping bodies ARE visible (read-only query, wakes nothing).
    /// - `cell` is a terrain cell for `body: None`, otherwise the hit
    ///   body's SHAPE cell (its own grid coordinates).
    #[must_use]
    pub fn raycast(
        &self,
        field: &dyn VoxelField,
        origin: FixedVec3,
        dir: FixedVec3,
        max_t: Fixed,
    ) -> Option<WorldRayHit> {
        let mut best = raycast::cast(field, origin, dir, max_t).map(|hit| WorldRayHit {
            body: None,
            cell: hit.cell,
            t: hit.t,
            normal: hit.normal,
            position: origin + dir.scale(hit.t),
        });

        for body in &self.bodies {
            let Some(shape) = body.shape.as_ref() else {
                continue; // ghosts are invisible to rays
            };
            let limit = best.as_ref().map_or(max_t, |b| b.t);
            // Prune: closest approach of the ray to the bounding
            // sphere (relative offsets only — rule 6).
            let to_body = body.position - origin;
            let along = to_body.dot(dir).clamp(Fixed::ZERO, limit);
            let off = to_body - dir.scale(along);
            let reach = body.bounding_radius;
            if off.dot(off) > reach * reach {
                continue;
            }
            // Into the body's shape frame (lengths preserved, so t
            // carries over unchanged).
            let inv_rot = body.orientation.inverse();
            let origin_s = inv_rot * (origin - body.position) + body.com_local;
            let dir_s = inv_rot * dir;
            let grid = crate::raycast::ShapeOccupancy(shape);
            if let Some(hit) = raycast::cast(&grid, origin_s, dir_s, limit) {
                // Strictly closer only: ties keep terrain / the lower
                // id (scanned first).
                if best.as_ref().map_or(true, |b| hit.t < b.t) {
                    best = Some(WorldRayHit {
                        body: Some(body.id),
                        cell: hit.cell,
                        t: hit.t,
                        normal: body.orientation * hit.normal,
                        position: origin + dir.scale(hit.t),
                    });
                }
            }
        }
        best
    }

    /// The canonical FNV-1a digest of the full physics state, for the
    /// per-tick desync exchange and the oracle goldens.
    #[must_use]
    pub fn state_hash(&self) -> u64 {
        let mut h = StateHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

/// One [`PhysicsWorld::raycast`] hit. `position` and `normal` are
/// world frame; `cell` is a terrain cell for `body: None`, otherwise
/// the hit body's shape cell.
#[derive(Clone, Copy, Debug)]
pub struct WorldRayHit {
    pub body: Option<BodyId>,
    pub cell: (i64, i64, i64),
    pub t: Fixed,
    pub normal: FixedVec3,
    pub position: FixedVec3,
}

impl StateHash for PhysicsWorld {
    /// Canonical fold order: `tick`, `dt`, `gravity`, `max_speed`,
    /// `next_body_id`, bodies in id order, materials in registration
    /// order, the impulse cache in key order (each Vec length-prefixed
    /// by the slice impl). Any change here — including appending a
    /// field — re-keys every `phys@` golden and requires an explicit
    /// bless; keeping the order append-only just makes the diff-time
    /// story legible (old fields keep their positions, the bless
    /// commit points at exactly what grew).
    fn hash(&self, h: &mut StateHasher) {
        self.tick.hash(h);
        self.dt.hash(h);
        self.gravity.hash(h);
        self.max_speed.hash(h);
        self.next_body_id.hash(h);
        self.bodies.hash(h);
        self.materials.hash(h);
        self.impulse_cache.hash(h);
        h.write_u64(u64::from(self.debris_threshold));
    }
}
