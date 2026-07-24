//! [`PhysicsWorld`] — the physics state root (docs/plans/voxel-physics.md
//! §3 `world`).
//!
//! The tick pipeline (gravity → wheels → narrowphase → velocity solve
//! → integrate → NGS → cache) plus the out-of-tick mutation surface:
//! spawns, impulses, wheel management, and P4 destruction.

use monada_fixed::{Fixed, FixedMat3, FixedQuat, FixedVec3};
use monada_sim::{StateHash, StateHasher};

use crate::body::{BodyDef, RigidBody, VoxelBodyDef};
use crate::contact::{self, ContactCacheEntry};
use crate::destruct::{self, DebrisCluster, Removal};
use crate::field::VoxelField;
use crate::ids::BodyId;
use crate::material::{Material, MaterialId};
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
    /// the stream, as it must).
    pub fn set_gravity(&mut self, gravity: FixedVec3) {
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
        let r = point - body.position;
        body.linear_velocity += impulse.scale(body.inv_mass);
        body.angular_velocity += inv_inertia * r.cross(impulse);
    }

    fn body_mut(&mut self, id: BodyId) -> &mut RigidBody {
        let index = self
            .bodies
            .binary_search_by_key(&id, RigidBody::id)
            .unwrap_or_else(|_| panic!("no body {id:?}"));
        &mut self.bodies[index]
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
        // 1. Integrate velocities.
        let g_dt = self.gravity.scale(self.dt);
        for body in &mut self.bodies {
            body.linear_velocity = (body.linear_velocity + g_dt).clamp_length_max(self.max_speed);
        }

        // 1½. Wheel pass — before the contact solver, so a chassis
        // bottoming out on a step is still corrected by NGS.
        wheels::wheel_pass(&mut self.bodies, field, &self.materials, g_dt, self.dt);

        // 2. Narrowphase at pre-step poses.
        let mut contacts = contact::generate(&self.bodies, field, &self.materials);

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
        solver::prepare(&mut self.bodies, &inv_inertias, &mut contacts);

        // 4. Velocity solve.
        solver::solve_velocities(&mut self.bodies, &inv_inertias, &mut contacts);

        // 5. Integrate poses (P1 integrator; no gyroscopic term).
        for body in &mut self.bodies {
            body.position += body.linear_velocity.scale(self.dt);
            body.orientation = (FixedQuat::from_scaled_axis(body.angular_velocity.scale(self.dt))
                * body.orientation)
                .normalize();
        }

        // 6. Position correction.
        solver::solve_positions(&mut self.bodies, &contacts);

        // 7. Persist accumulated impulses (contacts are in key order —
        // the generation loops iterate x→y→z outermost-first to match
        // `ContactKey`'s derived `Ord` — so the rebuilt cache is
        // sorted by construction).
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
        // Tripwire for the loop-order ↔ Ord pairing above: the
        // binary_search warm-start lookup silently degrades if this
        // ever breaks (strictly ascending — duplicates are impossible
        // by key construction).
        debug_assert!(
            self.impulse_cache.windows(2).all(|w| w[0].key < w[1].key),
            "impulse cache no longer sorted — generation order diverged from ContactKey::Ord"
        );

        self.tick += 1;
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
