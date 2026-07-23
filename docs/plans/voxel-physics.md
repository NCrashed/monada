# monada-physics — Deterministic Voxel Rigid-Body Physics

Status: **approved, in progress (P0)**. Target crate: `crates/monada-physics`
(in the workspace as a stub since M0, DESIGN.md §3.6/§4). This plan is the
"M7+ voxel physics" slot from DESIGN.md §7. Prompt-plan for Claude Code: read
fully before writing code, work milestone by milestone (P0 → P6), do not
start a milestone until the previous one's acceptance criteria pass in CI.

All open points were confirmed by the author on 2026-07-23: the
`monada-sim` dependency for `StateHash`, the P5 budget (4 ms of the 40 ms
tick), and the DESIGN.md §3.6 amendment (applied the same day).

Milestones are numbered **P0–P6** to avoid colliding with the engine's M0–M7.

---

## Locked decisions

- **Numeric format is `monada-fixed`** — Q32.32 in `i64` (`Fixed`,
  `FixedVec2/3`, `FixedQuat`). The physics crate defines no scalar types of
  its own; anything missing (notably `FixedMat3`) is added to `monada-fixed`
  and reused engine-wide. This resolves the "where does fixmath live" TODO.
- **Angles are radians**, not binary turns. `monada-fixed::trig` already bakes
  `sin`/`cos`/`atan2`/`acos` LUTs at build time (reproducible tables, pure
  integer lookup at runtime) with `PI`/`TAU`/`FRAC_PI_2` constants. The
  original turns idea bought LUT exactness at quadrant boundaries; the baked
  radian LUT already delivers bit-identical results everywhere, so there is
  no reason to fork the engine's angle convention.
- **Tick rate follows the map's `sim_hz`** (manifest.toml), engine default
  25 Hz. `PhysicsWorld` takes the tick rate at construction and stores
  `dt = Fixed::from_ratio(1, hz)`; `step()` has no `dt` parameter.
- **Target feel: Cortex Command / Teardown** — chunky, material, per-voxel
  destruction feedback. Explicitly NOT Besiege-grade constraint simulation
  and NOT a kinematic arcade model.
- **Simulated objects**: player-built vehicles (discrete functional modules +
  freeform voxel armor shell), faction vehicles in the same format, detached
  wreckage clusters. Fine debris is NOT simulated here (see Non-Goals).
- **The crate stays render-blind.** It depends on `monada-fixed` (and, per
  the decision below, `monada-sim` for `StateHash`), never on roxlap. Bodies
  are mirrored to render as per-body grids (`grid_spawn` + pose updates, the
  seam the ship demo already exercises) by the engine, outside this crate.

## Deviations from DESIGN.md §3.6 (surface before P0)

DESIGN.md §3.6 sketched bodies as `roxlap_scene::Grid`s with broadphase from
roxlap's chunk occupancy bitmaps and narrowphase marching one grid's voxels
through the other's chunks. Two things changed since that sketch:

1. **The sim/render wall (S3, HostBridge).** Sim code must not read render
   state, so physics cannot use roxlap grids or chunk bitmaps as its source
   of truth. Bodies own a sim-side voxel occupancy (same duality as
   `VoxelStore` vs the painted world grid today); roxlap grids are a mirror.
2. **Narrowphase is a surface-voxel sphere skin**, not full voxel marching.
   Each body caches its surface voxels as a sphere set (still voxel-native,
   updated incrementally on edits); contacts come from sphere-set vs
   `VoxelField` and sphere-set vs sphere-set. This is a concrete refinement
   of §3.6's marching, chosen for contact-count control and warm-starting.

Both deviations were approved; DESIGN.md §3.6 is amended accordingly.

---

## 1. Hard Rules (violations are bugs, enforce in CI)

1. **No floats in the simulation path.** Adopt `monada-sim`'s crate-root
   stance verbatim: `#![deny(clippy::float_arithmetic)]` and
   `#![deny(clippy::disallowed_types)]` (the workspace `clippy.toml` already
   disallows `HashMap`/`HashSet`). `Fixed::to_f32/to_f64` are for the render
   side of the wall only, never read back into sim state.
2. **Fixed timestep only.** Public step API is `world.step(&inputs)`; tick
   duration fixed at world construction (see Locked decisions).
3. **Canonical iteration order everywhere.** Sorted `Vec`, `BTreeMap`, or
   stable-indexed arenas; all ID allocation monotonic and part of sim state.
   `monada-nav` is the house precedent (fixed neighbor order, `BTreeMap`,
   monotone `seq` tie-breaking) — copy its discipline.
4. **No randomness in v1.** The solver and integrator are RNG-free by
   design. If a use case appears (e.g. debris velocity jitter), take a
   `DeterministicRng` fork from the caller (`monada-sim`'s xoshiro256** with
   `fork(stream_id)`); never own a seed, never touch `rand`.
5. **Arithmetic policy is `monada-fixed`'s.** `Fixed::mul`/`div` already
   widen through `i128`; use `checked_*` variants wherever overflow is
   plausible and document the per-call-site policy (saturate vs debug-panic).
   Never rely on wrapping arithmetic implicitly.
6. **Relative-offset rule:** never compute squared distances or dot products
   on absolute world positions — only on relative offsets between nearby
   objects (broadphase guarantees locality). This bounds i128 intermediates.
7. **State is serializable and byte-stable.** `PhysicsWorld` derives
   serde like `monada_sim::World` does (`Fixed` serializes as transparent
   `i64` bits). Snapshot → restore → step must equal step without the
   round-trip, bit-for-bit.
8. **No new dependencies without approval.** Runtime deps (confirmed):
   `monada-fixed`, `monada-sim` (for `StateHash`/`StateHasher`), `serde`.
   Randomized tests follow the house pattern — hand-rolled LCG + `f64`
   reference with bit-count tolerances, as in
   `monada-fixed/tests/arithmetic.rs` — so no `proptest` dep is needed
   (it stays pre-approved as a dev-dep if shrinking ever earns its keep);
   `criterion` pre-approved for P5 benchmarks. Everything else — ask
   first. No `glam`/`nalgebra`/`rapier`.
9. **`#![forbid(unsafe_code)]` stays** (already in the stub). If profiling
   ever argues for `unsafe`, that is a conversation, not a commit.

---

## 2. Numeric Foundation — additions to `monada-fixed`

Already present (do not reimplement): `Fixed` with checked/wrapping ops and
`const fn sqrt`; `FixedVec2/3` (dot, cross, length, normalize,
`clamp_length_max`, `reject`); `FixedQuat` (normalize, inverse, nlerp/slerp,
`from_axis_angle`, `from_scaled_axis`); `trig::{sin, cos, atan2, acos}` via
baked LUTs.

To add, in `monada-fixed` (each with randomized accuracy tests against an
`f64` reference and documented error bounds, in the crate's existing
`tests/arithmetic.rs` style):

- `FixedMat3` — 3×3 matrix for inertia tensors: mul (mat·vec, mat·mat),
  transpose, `from_quat` (rotation matrix), inverse for symmetric
  positive-definite matrices (needed for world-space inverse inertia). All
  intermediates widened to `i128`.
- Quaternion integration helper if not expressible cleanly at call sites:
  `q.integrate(omega: FixedVec3, dt: Fixed)` =
  `(FixedQuat::from_scaled_axis(omega.scale(dt)) * q).normalize()`.
- Audit: `FixedVec3::length_squared` on typical relative offsets (≤ ~2^16
  voxels) must not saturate; document the safe input domain.

Determinism of all of the above is inherited from the existing "integer-only
at runtime, reproducible tables at build time" contract; the cross-platform
proof is the oracle golden matrix (§5), not per-fn tests.

---

## 3. Architecture (modules in `monada-physics`, in dependency order)

```
ids          — BodyId, ContactId, monotonic arenas
field        — VoxelField trait (§4): occupancy + material queries, impl'd by engine
body         — RigidBody: pose (FixedVec3 + FixedQuat), velocities, derived
               mass properties (mass, CoM, FixedMat3 inertia) from voxel occupancy
shape        — collision skin: surface-voxel sphere set, incremental update on edits
broadphase   — uniform spatial hash on integer cell coords, sorted-key iteration
narrowphase  — sphere-set vs VoxelField, sphere-set vs sphere-set; contact manifolds
solver       — sequential impulses: N velocity iters + M position (Baumgarte or NGS),
               warm starting keyed by canonical (body_a, body_b, feature) ids
wheels       — raycast suspension: spring-damper along ray vs VoxelField,
               friction as clamped impulse, drive/brake torque inputs
destruct     — voxel removal intake → incremental mass-property recompute →
               connectivity flood-fill → body split events / debris-spawn events
islands      — union-find over contact graph (deterministic order), sleeping
events       — ordered per-tick event Vec out: contacts, splits, debris, wheel state
world        — PhysicsWorld: state root, step(), snapshot/restore, StateHash
```

- Contact normal against the voxel field: occupancy gradient over the 3×3×3
  neighborhood of the penetrating sphere (26-direction normal LUT indexed by
  neighborhood mask if profiling demands).
- Mass properties: armor voxels contribute per-material density; discrete
  functional modules contribute lumped mass at their mount pose. Incremental
  update on voxel removal (subtract contributions); full recompute kept as
  the verification path in tests.
- Body voxel storage: dense bitset + material ids over the body's local AABB
  (bodies are vehicle-sized, not terrain-sized); revisit only if profiling
  demands chunking.

---

## 4. Engine Integration Boundary

Physics does not own terrain. Define in `field`:

```rust
pub trait VoxelField {
    fn occupied(&self, p: (i64, i64, i64)) -> bool;
    fn material(&self, p: (i64, i64, i64)) -> MaterialId;
    // bulk/region variants added when profiling asks for them
}
```

- **Terrain implementor today**: `monada-script`'s `VoxelStore` (column
  heightmap: `occupied` = `z <= top`). It cannot represent overhangs or
  tunnels; a true 3D terrain store is an engine-side follow-up that this
  trait deliberately does not block on. P2 acceptance runs against a test
  implementor inside the crate; wiring `VoxelStore` in is demo work.
- Coordinate space is **sim space** (the space `VoxelStore` and `monada-nav`
  live in). The world-X mirror and Z-flip belong to `monada-render`'s
  `MapRender`, never here.
- Terrain edits (drilling, explosions) happen engine-side; physics receives a
  per-tick list of edited regions to refresh cached contacts and broadphase
  cells.
- Drilling: physics exposes `DrillQuery { body, tool_pose, torque }` results —
  which voxels the tool face overlaps and their materials. The ENGINE decides
  removal (hardness table, tool wear) and feeds edits back. Physics applies
  reaction forces from the hardness of what was actually cut this tick.
- Debris: `destruct` emits `DebrisSpawn { voxels, velocity }` events for
  clusters below the rigid-body threshold. The falling-sand consumer does not
  exist yet engine-side; until it does, the demo script consumes the events
  (e.g. spawns short-lived render-only effects). Physics never simulates
  particles.
- **Render mirror**: each body maps to a render grid via `grid_spawn` /
  `voxel_fill_in`; the engine pushes poses per frame and carves the mirror on
  destruction events. The multi-grid + off-origin/rotation re-basing work
  from the ship demo is exactly this seam.
- **Script surface**: v1 keeps physics engine-side. The Rhai API (spawn body
  from voxel box, read pose, apply impulse, wheel inputs) is designed
  together with the demo map, not in this document.

---

## 5. Determinism Harness (wired in P0, gates every milestone)

Reuse the engine's existing harness — do not build a parallel one:

- **Hashing**: implement `monada-sim`'s `StateHash` (FNV-1a 64, canonical
  field order, length-prefixed slices) for `PhysicsWorld` and every state
  type. `PhysicsWorld::state_hash() -> u64` available in release builds.
- **Goldens**: add a `phys@` scenario to `monada-oracle` — pure-Rust like
  `kernel@` (no Rhai), hashed at the standard `TICK_CHECKPOINTS`
  `[0, 1, 30, 150, 600]`, committed to `monada-hashes.txt`. Later milestones
  extend the scenario (P3 adds a vehicle, P4 a destruction script) or add
  sibling scenarios; every acceptance criterion below lands as a golden or a
  test.
- **CI matrix**: the existing `determinism` job (ubuntu/macos/windows,
  `--release -- --check`) already crosses x86_64 and aarch64 —
  `macos-latest` runners are arm64. Add a debug-profile oracle run for the
  physics scenarios if runtime allows; no new workflow needed.
- **Snapshot property**: for random ticks of the `phys@` scenario,
  serialize → deserialize → continue must hash-match the uninterrupted run.
- **Randomized invariant tests** (house LCG style): on `FixedMat3`
  (accuracy vs f64 reference) and on solver invariants — no saturation
  events in nominal scenarios, penetration depth bounded after solve,
  momentum conserved within documented bounds in zero-friction two-body
  tests.

---

## 6. Milestones

**P0 — FixedMat3 + skeleton + harness wiring.**
`FixedMat3` in `monada-fixed` with proptest accuracy bounds. Crate layout,
crate-root lints, `PhysicsWorld` with no-op `step()`, serde snapshot,
`StateHash` impl, `phys@` golden of an empty world in `monada-oracle`.
*Accept:* FixedMat3 property tests pass with documented error bounds;
`phys@600` identical across the full CI OS matrix.

**P1 — Free rigid body.**
Semi-implicit Euler, gravity, angular velocity + quaternion integration
(`from_scaled_axis` + renormalize), no collisions.
*Accept:* ballistic trajectory matches closed-form within documented
tolerance; golden stable; energy drift over 10k ticks bounded and documented.

**P2 — Voxel field contact + solver.**
Sphere-skin vs `VoxelField` narrowphase, sequential impulse solver, friction,
restitution ≈ 0 default. Single-body scenarios against an in-crate test field.
*Accept:* a box body dropped on flat voxel ground comes to rest (velocities
under sleep threshold) within N ticks and stays; body on a 30° voxel slope
slides or holds according to friction coefficient; goldens.

**P3 — Vehicle: raycast wheels.**
Wheel module (suspension spring-damper, longitudinal/lateral friction
impulses, drive/brake torque), vehicle assembled as one body + K wheels.
*Accept:* scripted-input replay of a 4-wheel vehicle over bumpy voxel terrain
is hash-stable; high-CoM vehicle rolls over in a scripted hard turn while
low-CoM does not; losing one wheel (script-removed) produces sag + pull
measurable in trajectory.

**P4 — Destruction: split + mass recompute.**
Voxel removal intake on bodies, incremental mass properties, connectivity
flood-fill, split into new bodies, debris-spawn events, collision-skin update.
*Accept:* cutting a body in half yields two bodies whose summed mass/inertia
match the original minus removed voxels (verified against full recompute);
sub-threshold fragments emit debris events; hash-stable destruction golden.

**P5 — Scale: broadphase, islands, sleeping, raycast.**
Spatial hash broadphase, union-find islands, sleeping/waking, body-vs-body
contacts (vehicle collisions, wreck piles), `PhysicsWorld::raycast` (for
engine-side projectiles).
*Accept:* benchmark scene — 32 vehicles driving + 256 loose wreck bodies —
under the per-tick budget (confirmed: 4 ms of the 40 ms tick on reference
hardware, criterion-tracked); determinism matrix green; sleeping bodies
wake on contact and on nearby voxel edits.

**P6 — Drill coupling + materials.**
`DrillQuery` API, reaction forces from material hardness, edited-region
intake refreshing contacts, cached-contact invalidation on terrain edits.
*Accept:* scripted drill-through of layered materials shows per-material
penetration rates; vehicle drilling into a wall decelerates by hardness;
tunnel-scenario golden hash-stable while terrain edits stream in.

After P6: demo map (new `monada-*` demo crate + map script + oracle
scenario) is planned as its own document, like the RTS and ship demos.

---

## 7. Working Agreements for Claude Code

- Before each milestone: post a short API sketch (types + signatures) and the
  test list; wait for approval only if deviating from this document.
- TDD bias: acceptance scenarios exist as failing tests before implementation.
- Small commits, one concern each; every commit leaves CI green. Stage and
  propose the commit message — the author commits (GPG-signed) himself.
- Every perf-sensitive change lands with a criterion benchmark delta.
- Re-blessing `monada-hashes.txt` is an explicit, reviewed act — a golden
  change without a stated cause is a determinism bug.
- When this document conflicts with something discovered during
  implementation (or with DESIGN.md), stop and surface the conflict — do not
  silently reinterpret.

## 8. Non-Goals (do not implement)

- Joints/constraints between separate bodies (no Besiege contraptions).
- Particle/debris simulation (a future falling-sand layer owns it; until
  then debris events are consumed by the demo script).
- General triangle-mesh colliders; soft bodies; cloth; fluids (DESIGN.md
  §3.6 defers those to a voxel-CA approach, not this crate).
- Float compatibility mode.
- Networking/rollback logic (engine's responsibility; this crate only
  guarantees snapshot/restore and determinism).
- Continuous collision detection in v1 — fast projectiles are engine-side
  raycasts against bodies (`raycast` lands in P5).
- 3D terrain storage (replacing `VoxelStore`'s column model) — engine-side
  follow-up, tracked separately.
