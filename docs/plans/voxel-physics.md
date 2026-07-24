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
- Audit (done, P0): `FixedVec3::length_squared`/`dot` are exact for
  Euclidean norms up to `⌊√(2^31)⌋ = 46340 ≈ 2^15.5` — note this bounds the
  *norm*, so a fully diagonal `2^16`-per-axis offset is already out of
  range. Documented on the methods; broadphase locality (P2/P5) must keep
  relative offsets under ~46 000 voxels, which it does by orders of
  magnitude.

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

**P1 amendments (approved 2026-07-23).** Surfaced per §7 and folded in:

- **No gyroscopic term in v1.** The integrator holds ω constant for a free
  body; `ω̇ = I⁻¹(ω × Iω)` is omitted. Explicit gyroscopic integration
  under semi-implicit Euler pumps energy and would need an implicit step
  (Catto) or damping — pure budget loss for the target feel. Bonus: constant
  ω makes the P1 rotation/energy tests exact rather than approximate.
  Revisit only if a demo visibly misses precession.
- **`BodyDef` carries explicit mass properties** (`mass`, `inertia_body`).
  Voxel-derived mass arrives with `shape` in P2; the explicit path stays
  forever as the tests' ground truth for P4's incremental-vs-full recompute
  check.
- **Derived caches (`inv_mass`, `inv_inertia_body`) are serialized, not
  hashed.** Serializing them keeps snapshot → restore → step bit-equal by
  construction (rule 7) with a plain derive; recompute-on-deserialize would
  risk a one-bit divergence from the spawn path. They are excluded from the
  hash fold as pure functions of hashed fields. P4 splits must update the
  caches together with mass/inertia — documented on `RigidBody`.
- **Energy drift is pinned to the closed form**, not a measured bound: for
  semi-implicit Euler in a uniform field, `E_n − E_0 = −½·m·|g|²·dt²·n`
  exactly (linear, known slope); the P1 test asserts this with an
  ulp-tolerance. Velocity is asserted bit-exact (`g·dt` rounds identically
  every tick, and additions are exact).
- Rotation-accuracy comparisons stay under one total revolution so they
  test the integrator, not the LUT's angle reduction; the 10k-tick run
  checks only unit-norm retention.

**P2 — Voxel field contact + solver.**
Sphere-skin vs `VoxelField` narrowphase, sequential impulse solver, friction,
restitution ≈ 0 default. Single-body scenarios against an in-crate test field.
*Accept:* a box body dropped on flat voxel ground comes to rest (velocities
under sleep threshold) within N ticks and stays; body on a 30° voxel slope
slides or holds according to friction coefficient; goldens.

**P2 amendments (approved 2026-07-24).** Surfaced per §7 and folded in:

- **The 30° acceptance runs as tilted gravity over a flat voxel floor.**
  A literal voxel slope is a staircase; the skin catches on step edges and
  the test would measure step geometry, not Coulomb friction. Tilting `g`
  by 30° isolates exactly `μ` vs `tan 30° ≈ 0.577` (test points 0.7 / 0.3,
  same material on body and floor so `√(μ·μ) = μ` keeps the documented
  boundary honest). The literal stepped slope is exercised by P3's bumpy
  terrain.
- **Per-voxel inertia uses the solid-cube convention**: each voxel
  contributes `ρ·[(|d|²·E − d⊗d) + E/6]` about the CoM (own term + parallel
  axis). Point masses would hand a singular tensor to legal input (a single
  voxel, a 1×1×n rod); the cube term makes every non-empty body SPD by
  construction.
- **Explicit-mass bodies (`BodyDef`) have no collision skin** — free-flying
  ghosts for tests and lumped modules; only voxel bodies collide.
- **Position correction is full-K NGS (split impulse)**: velocity pass has
  no Baumgarte bias (restitution only); the position pass applies
  pseudo-impulses through the full K operator — translation *and*
  orientation — so angular-origin penetration resolves as rotation, not as
  body-wide shift.
- **The warm-start impulse cache is hashed sim state** (accumulated
  impulses feed the next tick), stored as a `Vec<ContactCacheEntry>` sorted
  by `(body, sphere, cell)` rather than a `BTreeMap`: the cell loops in
  contact generation iterate x → y → z outermost-first, *deliberately
  matched* to `ContactKey`'s derived `Ord` (a `debug_assert` at cache
  rebuild guards the pairing), so the Vec is canonical by construction,
  hashes through the existing slice impl, and stays serde-friendly (JSON
  cannot encode struct map keys). NB the original wording claimed the
  match came for free — it does not; the first cut nested the loops
  z-outermost and warm starts silently missed cell-straddling spheres
  (caught in review, fixed with a re-bless 2026-07-24).
- **`max_speed` is a hashed world field** (no setter until a demo needs
  one), defaulting to 2000 voxels/s = 80 voxels/tick at 25 Hz — which is
  also the minimum obstacle thickness the clamp guarantees against
  tunnelling; documented at the clamp as the continuation of P1's rule-5
  note. Fast projectiles stay engine-side raycasts (non-goal).
- **`VoxelField::material` ids are a cross-crate contract**: they must
  come from this world's `register_material` order; the solver asserts
  with a legible message on an out-of-range id rather than crashing
  data-dependently mid-solve.

**P3 — Vehicle: raycast wheels.**
Wheel module (suspension spring-damper, longitudinal/lateral friction
impulses, drive/brake torque), vehicle assembled as one body + K wheels.
*Accept:* scripted-input replay of a 4-wheel vehicle over bumpy voxel terrain
is hash-stable; high-CoM vehicle rolls over in a scripted hard turn while
low-CoM does not; losing one wheel (script-removed) produces sag + pull
measurable in trajectory.

**P3 amendments (approved 2026-07-24).** Surfaced per §7 and folded in:

- **Wheels are chassis attachments, no spin DOF in v1**: drive torque
  becomes a longitudinal contact force (`τ/radius`) directly; the render
  side derives wheel rotation from `v·f/radius`. Keeps wheels free of
  dynamic state (no history, no warm-start questions) — compression and
  damping both derive from the current raycast and velocity.
- **Suspension: compression and damping measure along the ray; the force
  acts along the CONTACT NORMAL, cosine-projected** (`J = n·N·(−d·n)·dt`).
  The sketch had the force along the ray itself; testing showed that
  self-locks tall vehicles — drive torque pitches the body, the tilted
  spring axis gains a tangential component that cancels 100% of the drive
  (a perfect anti-squat), and on flat stair treads the same component
  shoves a parked pitched body downhill. The normal direction is the
  surface's actual reaction; on a riser face it pushes the wheel *out*,
  which is the legitimate half of the original "no sideways springs"
  concern. Tire friction stays in the contact-face plane.
- **All wheel impulses apply at the raycast hit point** — lateral friction
  below the CoM is what produces the roll moment the rollover acceptance
  measures (no Jolt-style roll-centre lift; tipping over is the feature).
- **Damper sign and sampling**: compression rate is `+v·d` (anchor
  approaching contact), so `N = max(0, k·x + c·(v·d))` — the damper opposes
  both compression and rebound (the sketch had the sign flipped; caught in
  review). The velocity sample is **pre-gravity** (`v − g·dt`): the
  integrator adds gravity before the wheel pass, and a damper fed that
  per-tick kick fights gravity permanently, shifting the static ride
  height by `c·|g|·dt/k` per wheel; subtracting it lets stiffness alone
  set the ride height.
- **Wheel pass is order-independent, with two stabilizers**: all wheels of
  a body compute their impulses from a pre-pass velocity snapshot and
  apply in bulk, so wheel №2 never reads wheel №1's impulse (`WheelId`
  order stays purely an iteration/hashing canonicality, and the lost-wheel
  baseline carries no parasitic pull). Bulk application is Jacobi, which
  demands: (1) **load-weighted slip kills** — K wheels each killing the
  body's shared slip from one snapshot would overcorrect K-fold and ring
  the roll axis (observed as a self-accelerating tumble), so
  constraint-type impulses (lateral kill, brake) are weighted `N_i/ΣN`;
  the drive term is a real force and is never scaled; (2) **static-
  friction feed-forward** — the suspension impulses land after the
  snapshot, so any tangential component they retain would re-feed a creep
  no velocity kill can hold (a braked vehicle tobogganing down stairs);
  tires react that known impulse up front, laterally always,
  longitudinally under braking. Gravity's feed is already in the snapshot
  and is excluded (double-count otherwise).
- **Tire friction is a friction circle**: desired `(J_long, J_lat)` clamped
  as a 2-vector to `μ·N·dt` — full throttle honestly eats lateral grip.
- **Degenerate steer branch**: if the steered forward direction projects to
  ~zero on the contact plane (near-vertical face), friction is skipped that
  tick — a deterministic branch, never a zero-normalize.
- **The DDA raycast is a shared module** (P5's `World::raycast` seam).
  Contract: near-zero direction components take an explicit "axis never
  crossed" branch (no 1/ε at the Q32.32 ceiling); a ray starting inside
  solid hits at `t = 0` with `normal = −dir` (documented on `cast`).
- **Wheels on ghost bodies are allowed** and documented: suspension raycasts
  terrain and needs no skin — a hover-cart with no chassis collision,
  useful for isolating suspension in tests.

**P4 — Destruction: split + mass recompute.**
Voxel removal intake on bodies, incremental mass properties, connectivity
flood-fill, split into new bodies, debris-spawn events, collision-skin update.
*Accept:* cutting a body in half yields two bodies whose summed mass/inertia
match the original minus removed voxels (verified against full recompute);
sub-threshold fragments emit debris events; hash-stable destruction golden.

P3 leaves one noted rough edge for P5/P6: the tire friction budget and
the slip-kill load weights use the raw ray-compression `N`, so a wheel
whose ray hits a vertical face (riser, wall) claims grip and load share
with zero suspension push behind it. Harmless on stair treads; revisit
as `N_eff = N·cos` when walls/3D terrain arrive.

P5-benchmark candidates carried over from P4's correctness-first cuts:
incremental skin updates (currently a full re-derive per carve) and the
`destruct::components` flood fill (a fresh `BTreeSet` walk per call,
O(n log n)) — optimize either only if the 4 ms budget says so.

**P4 amendments (approved 2026-07-24).** Surfaced per §7 and folded in:

- **Destruction outcomes are a return value, not a world event buffer**
  (deviation from §3's `events` module): `remove_voxels` reports splits,
  debris, and detached wheels synchronously through `Removal`. A pending
  event queue would be hashed, serialized sim state with drain-order
  rules; the single caller wants the outcome immediately. The general
  per-tick event list is deferred until a real consumer of tick events
  exists (contacts/wheel state — P5+/demo work).
- **Identity goes to the heaviest surviving component**, ties broken by
  the lexicographically smallest occupied cell — all comparisons in the
  PARENT's as-authored grid, before fragments are rebased. The debris
  threshold applies to the survivor too: a body whose largest component
  falls under it degrades entirely to debris (`survivor: None`).
- **The survivor's shape keeps its grid (holes punched); fragments get
  tight rebased grids** — so a later `remove_voxels` on a fragment speaks
  the fragment's own coordinates. The FULL no-teleport invariant: every
  surviving voxel keeps its world position AND its world point velocity;
  the CoM bookkeeping moves around the shape (`position += R·Δcom`,
  `v += ω × R·Δcom` — the velocity half was caught in review; fragments
  get `v + ω×r`, `ω` unchanged), and wheel anchors shift by `−Δcom`
  (bolted to structure, not to the CoM).
- **Wheels whose structure departs auto-detach** (`Removal::
  detached_wheels`): a wheel's home is the occupied pre-carve cell
  nearest its anchor (anchors may be virtual/overhanging, so "the
  containing cell" is undefined; ties break lexicographically). The
  engine decides what to spawn for a detached wheel.
- **Mass properties: survivor incremental, fragments fresh.** The
  incremental CoM update works in relative offsets
  (`com_new = com_old − Σρ·d/M'`) — reconstructing `com·mass` amplifies
  the stored CoM's rounding by the body mass. Same lesson applied
  crate-wide: CoM divisions are component-wise (`shape::div_by`), never
  `scale(ONE/mass)`, whose reciprocal rounding scales with |weighted|
  (found as an exact 75×287-ulp signature in the split test). A
  `debug_assert` re-checks Sylvester after every incremental update —
  the survivor bypasses `RigidBody::build`.
- **Skin re-derives in full on every carve**; the §3 "incremental update
  on voxel edits" for the skin is deferred until the P5 benchmark says
  it hurts. The carved body's warm-start cache entries are purged (skin
  indices shift), not left to miss deterministically.
- **`FixedMat3` grew wide-arithmetic paths** (`inverse`,
  `leading_minors_positive`): P4's fragments surfaced that a body barely
  6 voxels across has an inertia determinant past the Q32.32 ceiling
  (`1296³ ≈ 2.2e9`) — the narrow determinant wrapped negative, and
  `inverse()` divided by the wrapped value. Both now carry `i128`
  intermediates; only results narrow. (A latent P0-era bug that P2/P3
  bodies were simply too small to hit.)

**P5 — Scale: broadphase, islands, sleeping, raycast.**
Spatial hash broadphase, union-find islands, sleeping/waking, body-vs-body
contacts (vehicle collisions, wreck piles), `PhysicsWorld::raycast` (for
engine-side projectiles).
*Accept:* benchmark scene — 32 vehicles driving + 256 loose wreck bodies —
under the per-tick budget (confirmed: 4 ms of the 40 ms tick on reference
hardware, criterion-tracked); determinism matrix green; sleeping bodies
wake on contact and on nearby voxel edits.

**P5 amendments (approved 2026-07-24).** Surfaced per §7 and folded in:

- **Pair narrowphase is asymmetric**: the body with the smaller skin
  contributes spheres, the other its voxel grid (queried in its shape
  frame; rotation preserves distances). Ties to the lower id. Known
  properties: a carve can flip a pair's owner (its warm-start keys re-key
  for one deterministically cold tick), and coverage has chunky seams (a
  corner entering between four face spheres goes unnoticed to ~⅓ voxel —
  the same envelope as the P2 terrain path).
- **The warm-start cache is sorted by one explicit `sort_unstable` after
  generation** — with pair contacts the generation order cannot be made
  globally lexicographic by loop nesting (the P2 "sorted by construction"
  story ends here; the strict-ascending `debug_assert` remains the
  tripwire).
- **Pairs run BEFORE terrain in narrowphase** (found in testing): a
  sleeper woken by an impact must get its terrain contacts the same tick,
  or the sandwich solves without its floor — the struck body was driven
  into the ground and NGS heaved it back, a flicker-and-pump cycle.
- **`CONTACT_MARGIN` grew to 1/8, paired with SPECULATIVE contacts**
  (found in testing): one tick of standstill free fall is
  `|g|·dt² ≈ 0.016` voxels, and the old 1/64 margin was *below* that
  window — stacked manifolds flickered on/off every other tick, breaking
  warm starts and pumping the stack. The generous margin keeps manifolds
  persistent; the speculative velocity bias (`vn` may close up to
  `separation/dt` per tick, impulses fire only on faster approach) keeps
  it from being sticky or leaving bodies hovering.
- **Contact normals: closest-point axis first, occupancy gradient as the
  deep fallback** (found in testing — a P2 revision): the gradient goes
  diagonal on the corner/edge cells of a *finite* body grid (terrain
  never showed it — side neighbours there are occupied), and a stacked
  cube received ~0.5z corner normals from its counterpart and ground
  itself sideways. The closest-point axis is the exact sphere-vs-box
  normal whenever the centre is outside the cell.
- **Sleep is island-wide with zeroed velocities**: per-body still-timers
  (`SLEEP_TICKS = 25`), union-find over this tick's pair contacts, a
  whole island sleeps only when every member is eligible; velocities zero
  on the way down (no micro-drift in hashed state). Waking: a REAL
  narrowphase contact from an awake body (broadphase adjacency alone
  never wakes — no sleep-thrash from drive-bys; a sleeping stack wakes as
  a wave, one layer per tick, documented and tested), any external
  mutation (`apply_impulse*`, wheel attach/detach/input — all funnel
  through the waking `body_mut`), `remove_voxels`, `set_gravity` (wakes
  ALL — a gravity flip must not leave sleepers hanging), and
  `notify_terrain_edit` (pulled forward from P6: wakes sleepers whose
  bounding sphere overlaps the inclusive edited cell box; P6 hangs
  cached-contact invalidation on the same seam).
- **`World::raycast`**: terrain + every voxel body (shape-frame DDA, min
  t; ties → terrain, then lowest id; ghosts invisible — consistent with
  having no skin; sleepers visible, waking nothing). `bounding_radius`
  joined the serialized-not-hashed cache family (refreshed on carve).
- **Benchmark recorded**: 32 driving vehicles + 256 wrecks (64 piles,
  self-validated ≥ 128 bodies asleep at measurement) step in
  **≈ 179 µs** on the reference machine (12th Gen Intel i7-12700H,
  Linux, release) — 22× under the 4 ms budget. The P4-era optimization
  candidates (incremental skin, flood-fill, buffer reuse) stay shelved
  until a real scene says otherwise. Criterion runs locally; CI gates
  determinism, not wall time.

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
