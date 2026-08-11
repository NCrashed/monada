# Plan: the ship is a rigid body — a hull that flies, and riders that don't judder

Status: **DONE, S-0 – S-5, 2026-08-11** — render-rate pose smoothing (§4), the
ship map on the physics gate, freeform body shapes, a grid driven by a body,
engines that push and turn it (`host_api` 23), and a demo that flies. What
remains is §8's optional S-6 (interpolating riders' own motion), the live
display pass §9 owes, and whatever the open questions in §10 turn into.
Requested by the people extending the ship demo
(`crates/monada-ship`), who are stuck at the first step: making the hull itself
a physics body, with everything standing on it inheriting the pose smoothly.
Engines that push and turn the ship come after — and fall out of it almost for
free, so §7 designs them too rather than leaving a cliff at the end.

Genre stays in the map (DESIGN.md §3.2): everything the engine gains here is a
neutral primitive — a *body*, a *frame binding*, an *impulse at a point*. A
thruster, a gyro, an RCS quad and a fuel budget are the map's business.

## 0. Where we are

Two complete halves that have never been introduced to each other.

**The frame half.** `grid_spawn_cubic` gives a map a voxel grid whose cells are
cubes, so its rotation is exact for any axis (docs/plans/grid-entities.md §6).
Entities bound to it hold grid-**local** positions
(`entity_set_grid` / `entity_attach`), and `GridStore`
(`monada-script/src/grids.rs`) keeps the frame a second time in fixed point, so
`grid_world` / `grid_local` are deterministic and may steer hashed logic. The
render mirror lives in `MapRender` (`grid_anchors`, `apply_grid_pose`), and
*everything* downstream — a rider's seat (`place_in`, `map_render.rs:648`), a
prop's basis (`sync_props`), an actor's facing (`actor_pose`), the fog twin
(`update_fow`), the deck cutaway, the camera's orbit frame (`camera_grid`) —
reads that one transform. The ship demo drives it from a hashed `spin` field:
`grid_orient` + `grid_move` once per tick (`main.rhai:567-583`).

**The dynamics half.** `monada-physics` is a complete fixed-point rigid-body
sim: voxel bodies with derived CoM and inertia, contacts, sleep, destruction,
raycast wheels, a canonical `state_hash` folded into `RhaiDriver::state_hash`,
and the `phys@` golden gating it. Maps reach it through the `phys_*` verbs
(`monada-script/src/physics.rs`).

They have never met because of one line: `volume_physics`
(`monada-host/src/lib.rs:284`) hands a map a `PhysicsWorld` **only** if its
manifest says `terrain = "volume"`. The ship is a column map. So today the hull
is kinematic-by-script: a pose the map computes, not a pose that emerges.

## 1. What is missing, by failing scenario

| Scenario | What breaks today |
|---|---|
| The ship demo asks for a body at all | no physics on a column map (`volume_physics`); `PhysicsSim` is gated on the terrain mode, not on wanting dynamics |
| The hull's pose comes from the body | nothing copies a body pose into a grid frame: the map would have to read the body's quaternion, and there is no verb that returns one (`phys_yaw` / `phys_pitch` flatten it — and flattening a tumble is exactly the bug BB.6 fixed) |
| The hull renders as itself | `sync_physics` auto-mirrors **every** body with a shape into its own grid (`map_render.rs:2174`), so the ship would be drawn twice: once as the painted hull, once as a material-coloured block |
| Its mass and inertia mean something | `phys_box` is the only body-spawning verb — a solid block. A hull is a shell; a shell's inertia is not a block's, and engine torque is exactly where the difference shows |
| The hull turns about the right point | `grid_pivot` is a hand-authored literal (`main.rhai:280`); a body turns about its CoM. Two truths, drifting apart the moment the shape changes |
| It moves smoothly | the pose is written once per tick and rendered as-is: map scenes pass `alpha = 1.0` (`lib.rs:1788-1795`) and nothing interpolates. At 30 Hz over 60+ fps the hull steps — and so, rigidly attached to it, does every rider. See §4 |
| An engine pushes off-centre | `phys_impulse` applies at the CoM only. `PhysicsWorld::apply_impulse_at` exists (`world.rs:226`) and is unregistered; there is no angular-impulse entry point at all |
| A map counter-spins its own tumble | no `phys_angvel` read, so a script cannot write `τ = −k·ω` |

## 2. The shape of the answer

> The hull grid stops being a pose the map computes and becomes the **render
> frame of a rigid body**. Riders are untouched: their positions are already
> hull-local, their collision is already a hull-local predicate. The engine
> gains one binding (`grid_body`), one truthful shape, and two ways to push.

That is the whole idea, and its consequence is the part worth stating loudly:
**making the ship a body changes nothing about walking on it.** `blocked()`,
`reachable`, `try_move`, the stairs, the deck bands, the airlock — all of it is
in hull cells and stays exact while the hull tumbles, accelerates or is rammed.

## 3. Decisions

**D1 — the ship map declares `terrain = "volume"`.** It is the existing,
tested gate for an embedded `PhysicsSim`, and it costs the ship nothing: the
ship paints *no world terrain*, so the world grid it makes isotropic is empty.
Two dividends: `MapSim::advance` and `RhaiDriver::step` already step and hash
physics for such a map, and the world frame's z convention becomes `SCALE`-scaled
— the same convention the cubic hull uses — which quietly fixes a latent bug
today (a crate `entity_detach`ed above deck 0 is drawn at z voxels where the hull
means z cells).

Rejected: a new manifest key (`physics = true`) decoupling dynamics from the
terrain store. Cleaner on paper — a ship in space has no terrain and would carry
an empty `VolumeStore` for nothing — but it forks a gate that every existing
map, golden and test reads, to save one allocation. Revisit if a *column* map
ever wants bodies.

Two visible consequences to check on a real display, not argue about: volume
maps light through the dynamic `LightRig` instead of the column `side_shades`
path, and the third-person camera-collision branch in `MapRender::camera()`
starts running (against empty terrain, so it should be inert).

**D2 — one binding verb, `grid_body(grid, body)`, and the engine does the
copying.** Set once at init. After each physics step the engine writes the
body's pose into that grid's `GridStore` frame (fixed point, exact) *and* into
the render mirror, through the same path a scripted `grid_move`/`grid_orient`
takes. The map never sees a quaternion.

Rejected: exposing `phys_quat` + a `grid_orient_quat` and letting the map copy
in `tick()`. It looks more honest — no hidden engine phase — but the script tick
runs *before* the physics step, so the map could only ever copy last tick's
pose; and it would put a quaternion type into the Rhai surface for one caller.

**D3 — a bound grid's pivot IS the body's CoM.** `grid_body` sets it from
`com_in_shape()`, so the hull turns about the point the dynamics turn about, and
a map can never let the two drift. A hull breach that moves the CoM moves the
render pivot with it, for free.

**D4 — a bound body is not auto-mirrored.** `sync_physics` skips bodies that a
`grid_body` binding claims: the map's painted hull *is* the body's picture. This
is what keeps the airlock, the deck plating and the stair brass instead of a
palette-coloured block.

**D5 — the shape is authored from the same calls that paint the hull.** New
`phys_shape_*` verbs build a `VoxelShape` in hull-cell coordinates; `build_hull`
dual-writes paint + shape through one map-side helper. The hull's real geometry
therefore sets its mass, CoM and inertia — a shell, not a brick — and later
gives body-vs-body collision (another ship, an asteroid, debris) for free.

Rejected for v1-only use: a `phys_box` stand-in sized to the hull. It is
invisible under D4 and would ship faster, but it silently lies about inertia in
the one slice — engines — where inertia is the whole feel. If S-2 is late, the
stand-in is the fallback, not the plan.

**D6 — engines are per-tick impulses, and add no hashed state.**
`phys_thrust(body, anchor, dir, force)` applies `force · dt` at a point through
`apply_impulse_at`; `phys_torque(body, τ)` applies `I⁻¹·τ·dt`. The map calls
them from `tick()` for whatever it considers an engine. No retained thruster
table means nothing new to serialize, snapshot or fold into a digest — the
`phys_wheel_input` precedent (retained state, hashed) is the heavier pattern and
is not needed here.

**D7 — stabilisation is the map's rule, not the engine's.** With
`phys_angvel(body)` readable, an RCS that kills tumble is `τ = −k·ω` in Rhai —
three lines, tunable per ship class, and the genre stays in the map. No engine
damping knob.

**D8 — smoothing is render-side and interpolates, never extrapolates.** The
drawn pose lags the sim by up to one tick and never predicts. The tick-exact
`GridStore` frame remains the only thing scripts and the local layer can see, so
nothing hashed moves a bit (§4).

## 4. The judder, and the one write that removes it

This is the part the demo's authors are actually stuck on, so it is worth being
exact about where the stepping comes from. Four candidates; only two are real.

1. **The hull pose is written once per tick and drawn as written.** Real, and
   the whole of the visible problem. `MapSim::advance` runs the tick on an
   accumulator and returns nothing about the remainder; `App::redraw` passes
   `alpha = 1.0` for every map scene (`lib.rs:1788`), so a 30 Hz pose is shown
   on a 144 Hz display as 30 distinct poses a second.
2. **Riders sliding on the deck.** *Not* a risk, and this is the load-bearing
   property of the current design: a rider's world seat is composed from
   `scene.grid(g).transform` at draw time (`place_in`, `map_render.rs:648`), and
   so are prop bases, actor facings, the fog twin, the deck clip and the camera
   frame. **One interpolated write into that transform smooths all of them,
   coherently, by construction.** Nothing can shear relative to the deck,
   because nothing keeps its own copy of the hull's pose.
3. **A rider's own motion still steps.** Real but separate: a walking crew
   member's hull-local position moves 30 times a second, and `build_instances`
   reads the live world. Standing riders are perfectly smooth once (1) is fixed;
   walking ones are exactly as smooth as they are today. Deferred to S-6.
4. **The camera.** Real, and a trap: `camera_focus_entity` composes the focus
   point through the grid at **tick** time and stores a world point
   (`map_render.rs:4165`). Interpolating the hull without fixing this makes the
   *ship* slide under a stale camera centre — worse than the judder it replaces.
   The fix is to store `(entity, local_point)` and compose in `camera()`, which
   already runs per frame and already reads the grid rotation there.

### The mechanism

Per grid, beside its `GridAnchor`:

```rust
struct PoseTrack {
    prev: (DVec3, DQuat),  // what was DRAWN when the new pose arrived
    curr: (DVec3, DQuat),  // the tick-exact pose
    age: f64,              // seconds since curr arrived
}
```

- every pose write (`grid_move`, `grid_orient`, `grid_pivot`, or the `grid_body`
  sync) does `prev = drawn_last_frame; curr = new; age = 0` — *not*
  `prev = curr`, so a frame that runs several catch-up ticks never rewinds;
- once per frame, before anything reads a transform:
  `age += dt; a = clamp(age / tick_dt, 0, 1);`
  `transform = (prev.origin.lerp(curr.origin, a), prev.rot.slerp(curr.rot, a))`.

`tick_dt` comes from the manifest — `MapRender` needs a `set_tick_hz`, the same
shape as `set_volume_terrain` (`map_render.rs:1525`).

**Ordering fix, required.** `update_scene` currently rebuilds instances *then*
syncs physics (`lib.rs:1588-1591`). With interpolation that seats riders on last
frame's hull and draws the hull on this frame's — a one-frame shear, i.e. the
exact bug §4.2 says cannot happen, reintroduced by call order. Pose first, then
`build_instances`.

**Teleports.** A pose jump the map means (a dock snap, a jump drive) must not be
smeared over a tick. Simplest honest rule: interpolate only when the origin
delta is below a threshold (say two cells) and the rotation delta below some
angle; otherwise snap and reset the track. Name it in the book so a map author
can predict it.

**Determinism.** All of this is `f64` in `MapRender`. `GridStore` keeps the
tick-exact frame, so `grid_world` / `grid_local`, `nearest_crate`, the local
layer's picks and every hashed decision are bit-identical to today. The only
observable asymmetry: a local-layer pick is resolved against the tick-exact pose
while the player sees the interpolated one — up to one tick of cursor offset on
a fast-moving hull. Acceptable; worth a sentence in the book.

## 5. The new surface (`host_api` 21)

All additive; `HOST_API_OLDEST` stays 1.

| Verb | Layer | Meaning |
|---|---|---|
| `phys_shape(sx, sy, sz)` | simulation | a new empty voxel shape, in cells; returns a shape handle |
| `phys_shape_fill(shape, x0,y0,z0, x1,y1,z1, mat)` | simulation | fill a cell box of a shape with a material |
| `phys_shape_clear(shape, x0,y0,z0, x1,y1,z1)` | simulation | erase a cell box (the hollow, the corridors, the airlock) |
| `phys_body(shape, point)` | simulation | spawn a body from the shape at sim `point`; mass, CoM and inertia are derived. The shape handle is consumed |
| `grid_body(grid, body)` | simulation | bind a `grid_spawn_cubic` grid to a body: after each step the body's pose becomes the grid's frame, the grid's pivot is the body's CoM (D3), and the body is not auto-mirrored (D4). `-1` unbinds |
| `phys_thrust(body, anchor, dir, force)` | simulation | apply `force · dt` along body-frame `dir` at shape-coordinate `anchor` — the thruster primitive. Off-centre thrust yields torque, as it must |
| `phys_torque(body, τ)` | simulation | apply `τ · dt` of angular impulse in the world frame — gyros, RCS, a map's stabiliser |
| `phys_angvel(body)` | any | angular velocity (rad/s, world frame); `ZERO` for an unknown id, like `phys_vel` |
| `phys_mass(body)` | any | derived mass — for a HUD, and for a map that sizes thrust to its ship |

Engine-side additions behind them:

- `PhysicsWorld::apply_angular_impulse(id, L)` — `Δω = I⁻¹_world · L`, waking
  the body through the same `body_mut` funnel every external mutation uses
  (`world.rs:240`). The one genuinely missing primitive in `monada-physics`;
  `apply_impulse_at` already covers thrust.
- `RigidBody::angular_velocity` / `mass` are already public — the reads are pure
  registration.

## 6. Seams to change

| File | Change |
|---|---|
| `monada-physics/src/world.rs` | `apply_angular_impulse` (+ its test) |
| `monada-script/src/physics.rs` | register the eight verbs; a shape table (`Vec<VoxelShape>`) beside `tools` in `PhysicsSim`, consumed by `phys_body` |
| `monada-script/src/grids.rs` | `GridStore` gains `bodies: BTreeMap<u32, BodyId>` + `set_pose(grid, origin, rot)` (a quaternion write, bypassing axis-angle) |
| `monada-script/src/lib.rs` (new fn) | `sync_grid_bodies(phys, grids, bridge)` — called right after `world.step(...)` at both step sites (`driver.rs`, host `MapSim::advance`) |
| `monada-runtime/src/lib.rs` | `HostBridge::grid_pose(grid, origin, quat)` (defaulted, like every bridge method) + `HOST_API_VERSION = 21` |
| `monada-host/src/map_render.rs` | `PoseTrack` + `advance_grid_poses(dt)` + `set_tick_hz`; `apply_grid_pose` records instead of writing; `sync_physics` skips bound bodies; `camera_focus_entity` stores `(entity, local)` and composes in `camera()` |
| `monada-host/src/lib.rs` | `update_scene`: advance poses → sync physics → build instances |
| `monada-oracle/src/lib.rs` | `ship_checkpoints` switches to `RhaiDriver::with_physics` (the digger's line 899-900 is the model); re-bless `ship@` |
| `book/src/reference.md` | the new verbs, a "Bodies and frames" section, the teleport rule. The drift gate (`api_reference_matches_registered_functions`) already scrapes `physics.rs` and `grids.rs`, so every verb below fails CI until it is documented — nothing to add to the gate itself |

## 7. What the ship map writes with it

Init — the hull, once, in the same loop that paints it:

```rhai
fn init() {
    phys_gravity(fixed(0), fixed(0), fixed(0));         // space
    let steel = phys_material(fixed(1), ratio(6,10), ratio(1,10), fixed(4));

    let grid  = grid_spawn_cubic(0, 0, 0);
    let shape = phys_shape(20, 20, 6);
    build_hull(grid, shape, steel);                     // paints AND fills
    let body = phys_body(shape, vec3(fixed(0), fixed(0), fixed(0)));
    grid_body(grid, body);                              // pivot := the CoM
    camera_grid(grid);
    // ... models, crew, crates, vision_config — all unchanged
}

/// One authored cell box, to the eye and to the dynamics at once. Every
/// `voxel_fill_in` in `build_hull` becomes one of these; the two can no longer
/// drift, which is the same contract `blocked()` still owes the paint.
fn hull_box(grid, shape, x0,y0,z0, x1,y1,z1, col, mat) {
    voxel_fill_in(grid, x0,y0,z0, x1,y1,z1, col);
    phys_shape_fill(shape, x0,y0,z0, x1,y1,z1, mat);
}
```

An engine — an entity riding the hull at a cell, with a throttle field, whose
thrust is a bit of the per-tick input mask. Nothing about it is engine-side:

```rhai
/// Bolt a thruster at hull cell (cx, cy, cz) pointing along body-frame `dir`.
fn mount_engine(cx, cy, cz, dx, dy, dz) {
    let e = entity_create(3);                    // ENGINE archetype
    entity_set_position(e, vec3(fixed(cx), fixed(cy), fixed(cz)));
    entity_set_grid(e, hull_grid());             // authored in hull cells
    entity_set_field(e, "dx", fixed(dx));
    entity_set_field(e, "dy", fixed(dy));
    entity_set_field(e, "dz", fixed(dz));
    entity_set_field(e, "throttle", fixed(0));
}

fn step_engines() {
    let body = ship_body();
    for e in entities_of(3) {
        let t = entity_field(e, "throttle");
        if t > fixed(0) {
            let p = entity_position(e);          // hull cells == shape cells
            phys_thrust(body, p,
                        vec3(entity_field(e, "dx"),
                             entity_field(e, "dy"),
                             entity_field(e, "dz")),
                        t * engine_power());
        }
    }
    // RCS: kill the tumble the player did not ask for (D7).
    let w = phys_angvel(body);
    phys_torque(body, vec3(-w.x, -w.y, -w.z) * rcs_gain());
}
```

Two thrusters mounted aft push the ship forward; one mounted aft-port alone
yaws it — because the impulse lands off the CoM, not because anything in the
engine knows what a thruster is.

`step_ship` (the hashed `spin` field, `grid_orient`, `grid_move`) is deleted: the
pose is now an outcome. The `spin` archetype field goes with it, `door` stays.

## 8. Build order

Each step is a headless test; nothing needs a display until S-5.

- **S-0 — smoothing, alone. DONE (2026-08-11).** `PoseTrack` inside
  `GridAnchor`, `set_grid_pose` as the single writer, `advance_grid_poses(dt)`
  at the head of `update_scene`, `set_tick_hz`, the physics/instances reorder,
  and the `camera_focus_entity` fix. No physics anywhere: the demo still spins
  its hull from the `spin` field, and stops stepping while doing it. Five host
  tests, the load-bearing one being `a_rider_is_seated_through_the_pose_that_is_drawn`
  (mid-interpolation the rider sits where the deck IS, and provably not where it
  will be at the end of the tick). All 44 oracle checkpoints unmoved, `ship@`
  included — as a render-only slice must be. Three findings worth keeping:
  - **Smoothing is opt-in by manifest, and that is what makes it safe.**
    `tick_dt` is `None` until the host calls `set_tick_hz`, so a command-driven
    map and every host test that poses a grid and reads it back on the next line
    keep the old land-it-whole path, byte for byte. The host calls it *after*
    `init`, so a hull posed during setup does not ease in from its spawn frame
    and open the match with a 33 ms wobble.
  - **`prev` must be what was DRAWN, not the previous target.** A frame that
    runs several catch-up ticks writes several poses before anything is drawn;
    shifting `prev = curr` each time would rewind the hull to a pose the player
    already watched go past. Storing the on-screen pose makes catch-up degrade
    into a snap, which is the correct behaviour.
  - **The camera trap was real and needed both halves.** `camera_focus_entity`
    now keeps the *uncomposed* `(entity, point)` and `camera()` re-composes it
    per frame — while still writing the tick-exact world centre, because
    `camera_pan`, `camera_center_sim` and the cursor path read that one and want
    it tick-exact rather than eased.
- **S-1 — the ship map goes volume. DONE (2026-08-11).** Manifest
  `terrain = "volume"`, oracle switched to `with_physics` at `SHIP_HZ = 30`
  (the physics `dt` is folded into the digest, so the rate has to be the
  shipped map's), `ship@` re-blessed. No script changes. D1 is boring as
  advertised — with three things worth recording:
  - **Only the digest's SHAPE moved.** A throwaway run of the same 600 ticks
    with and without the physics embed had bit-identical *entity world* hashes
    at every single tick; what changed is `state_hash` going from the bare world
    digest to `FNV(world, physics, terrain, tools, granular)`. The check is
    recorded here rather than committed because it cannot survive S-2: once
    `main.rhai` calls a `phys_*` verb, the physics-less half of the comparison
    stops compiling at all.
  - **The camera-collision pass had to learn what it is for.** It fires on
    `volume` alone, and its job is keeping the eye out of *terrain* — so a map
    that declares volume for the physics and paints no world terrain got its own
    hull treated as rock, and the camera would have been yanked in every time
    the ray grazed a rim wall. Now gated on a world grid existing.
  - **The ship's LOOK changed and is owed an eyeball.** Volume maps light
    through roxlap's `LightRig` (sun + baked ambient + stylized shadows) where
    column maps use `side_shades`; that switch is keyed off the same flag. The
    numbers are gentle (ambient 0.62, shadow strength 0.42) and it is the newer
    path every other 3D demo uses, but nobody has seen the hull under it.
  - Note for S-2: `crates/monada-ship/tests/smoke.rs` builds a bare
    `RhaiBackend` with no physics. The first `phys_*` call in `main.rhai` will
    need `set_physics` there, or the canary fails on an unknown function.
- **S-2 — shapes and `phys_body`. DONE (2026-08-11).** `phys_shape` /
  `phys_shape_fill` / `phys_shape_clear` / `phys_body` / `phys_mass`,
  `VoxelShape::clear_box` (the public counterpart to `fill_box`, and the shell
  primitive), `HOST_API_VERSION` 21, six book rows, six tests in
  `monada-script/tests/body_shapes.rs`. The decisive one authors the ship's own
  20×20×6 as a shell and as the block `phys_box` would have given: 1104 wall
  cells against 2400, and — the number engines will feel — the shell resists yaw
  more than 20% harder *per unit mass*, because its mass lives at the skin.
  Goldens unmoved: the verbs exist, nobody calls them yet. Two findings:
  - **The shape table is not sim state, and that was a decision.** It lives in
    the closures `register_physics_api` builds, not in `PhysicsSim`: a shape is
    authoring scratch alive between the call that opens it and the call that
    spawns from it, so it is neither snapshotted nor hashed. What it *produces*
    is hashed — mass, CoM, inertia and skin all ride the physics digest — and a
    test pins that a hollow hull and a solid one hash differently.
  - **A raise inside a Rhai host function aborts the process.** Rhai catches the
    panic and re-raises it in a non-unwinding context (`rhai/src/func/call.rs`),
    so `catch_unwind` cannot see it and a map bug takes the whole game down with
    it. That is the existing house behaviour — `phys_wheel` and
    `phys_drill_tool` have always `expect`ed on an unknown body — and the new
    verbs match it. Worth knowing before writing the next verb: a handle that
    should degrade gracefully must return `-1` (the `grid_*` family's stance),
    because there is no middle ground between that and killing the process.
- **S-3 — `grid_body`. DONE (2026-08-11).** The binding
  (`GridStore::bind_body` + `set_rotation`, the quaternion twin of `orient`),
  the after-step sync (`ScriptBackend::sync_grid_bodies`, called at both places
  physics steps — `RhaiDriver::step` and `MapSim::advance`), two defaulted
  bridge methods (`grid_body`, `grid_pose`), the CoM pivot, and the auto-mirror
  skip. `HOST_API_VERSION` 22. Five script tests and two host tests, the
  load-bearing pair being `a_bound_grid_rides_its_body` (a crew member standing
  on the hull's centre of mass is drawn exactly at the body's position, at every
  attitude of a 30-tick tumble) and `a_body_driven_frame_agrees_with_what_is_drawn`.
  Goldens unmoved: nothing binds yet. Three findings:
  - **The rotation had to be CONJUGATED, not composed — and only a tilted pose
    tells the two apart.** A script grid's voxels were painted through
    `world_of` already, so its local frame is world-oriented and a sim attitude
    `q` becomes `M q M⁻¹` with `M = R_y(π)` — exactly what `grid_orient` does to
    an *axis* (map by `diag(-1, 1, -1)`, keep the angle). The body MIRROR grid
    wants the other spelling, `M ∘ q` (`body_grid_pose`), because its voxels are
    still shape-local. The two agree at identity and diverge everywhere else,
    which is why the agreement test tumbles about `(0.3, −0.2, 1)` and not
    about z.
  - **Binding poses the grid immediately.** Left to the first tick, a hull bound
    during `init` sits at its spawn origin for one frame — the ship visibly in
    the wrong place before it snaps into line. One extra call at bind time.
  - **A body that was already auto-mirrored leaves a ghost.** Binding has to
    RETIRE the mirror it replaces, not merely stop feeding it: the mirror's grid
    keeps whatever voxels were blitted into it, so the hull would be drawn
    inside the hull. `retire_mirror` (extracted with `clear_mirror` out of the
    existing dead-mirror path) does it at bind time.
  - Native maps are unaffected: `sync_grid_bodies` defaults to a no-op on the
    `ScriptBackend` trait, so `NativeBackend` keeps its behaviour until a native
    map wants the binding — at which point it writes the same three lines
    `RhaiBackend` does.
- **S-4 — thrust and torque. DONE (2026-08-11).**
  `PhysicsWorld::apply_angular_impulse` (+ two crate tests), and
  `phys_thrust` / `phys_torque` / `phys_angvel` on top of it and
  `apply_impulse_at`. `HOST_API_VERSION` 23, three book rows, six script tests.
  No new hashed state, per D6: a thrust is an impulse of `force · dt` applied
  the tick it is asked for, so there is no thruster table to serialize, snapshot
  or digest. Findings:
  - **The engine multiplies by `dt`, not the map.** A map that forgot would
    scale its whole drive-train by the tick rate, and the bug would present as
    "the ship feels wrong on a 60 Hz map" rather than as an error.
  - **A test that waits for a hull to turn will hang, because the hull falls
    asleep.** The first cut of `thrust_follows_the_nose_it_pushes_along` spun
    the body up and stepped until its nose reached +y; at an angular velocity
    below `SLEEP_ANGULAR` the island slept and the loop never ended. The
    rewritten test needs no staging at all — the off-centre engine yaws the hull
    while it burns, so the velocity it piles up leans the same way as the nose,
    which is the property itself rather than a proxy for it.
  - **`SLEEP_ANGULAR`/`SLEEP_LINEAR` do not apply to skinless bodies**
    (`world.rs`: an island with an empty skin never sleeps), so the wake test
    needs a real voxel body rather than a `BodyDef::default()` ghost.
  - D7 stands: the map writes its own stabiliser. `a_map_can_write_its_own_rcs`
    is three lines of Rhai (`τ = −k·ω`) killing a tumble to a tenth in three
    seconds, with no damping knob anywhere in the engine.
- **S-5 — the demo flies. DONE (2026-08-11).** `build_hull` dual-writes paint
  and shape through one `hull_box`, `init` spawns the body and binds the grid,
  `step_ship` stops posing the hull and starts flying it (two main engines, a
  reaction wheel on Q/E, and the map's own `τ = −k·ω` stabiliser), the HUD reads
  speed / spin / hull mass, and the hashed `spin` field is gone — the ship's
  pose is nobody's state now. Manifest `host_api = 23` plus two actions; three
  new canaries (15 in total, all green) and `ship@` re-blessed with the oracle
  pressing burn and both turns on fixed ticks. Four findings:
  - **The canaries were testing a ship whose engines never fired.** They drove
    `RhaiBackend` directly, and the physics step — plus the grid-frame sync
    after it — lives in `RhaiDriver`. Everything passed while the hull sat
    still. They now run through the driver, which is also the tick order the
    host uses.
  - **Torque constants are sized against INERTIA, not mass.** The first tuning
    (`rcs_force` 9000, `damp` 6000) was ~50× too weak: the hull's `I_zz` is
    ~90 000, so the stabiliser bled off 0.2% of the spin per tick and the ship
    span on for fifteen seconds after the key came up. Sized properly the gain
    gives back a tenth of the remaining spin per tick and the wheel's top rate
    is `rcs_force / damp`.
  - **There is no `Vec3 * Fixed` in the script surface** — only `Fixed * Fixed`.
    The map carries a three-line `scaled(v, s)`. Worth registering the operator
    the next time the number types are touched; not worth a `host_api` bump on
    its own.
  - **The key layout moved:** Q/E is the natural pair for turning a ship, and E
    was the crate verb, so crate went to F and the airlock to G. Defaults only —
    every one of them is rebindable by declaration.

### 8a. The live pass, and what it found instead

`cargo run -p monada-ship` **aborts on this box before drawing a frame**, and
the crash is not this plan's: it reproduces bit-identically at `e88ab9c`, the
commit before S-0. wgpu 29 rejects a buffer as invalid and roxlap maps it
anyway:

```
Buffer with 'roxlap-gpu scene.occupancy.page0' label is invalid
  <roxlap_gpu::scene::GpuSceneResident>::upload
  <roxlap_render::gpu::GpuBackend>::render
```

Scope, measured rather than guessed: **ship** and **digger** abort (the digger
on a different buffer, `sprite_reg.instances` — same shape of failure, same
first upload); **rpg** and **desert** run for as long as you leave them. So it
is neither the volume terrain mode nor physics — the desert has both. The
device is Mesa's NVK on an RTX 3070 under Vulkan.

`ROXLAP_GPU=0` forces roxlap's CPU backend, and the ship demo then runs
happily. That is how to look at this work today, and it is what the live pass
§9 owes should use until the GPU path is fixed upstream — where it belongs,
since nothing monada does can make a buffer roxlap created valid.

### 8b. What the live pass caught that no test could

**The turn keys did nothing, and nothing said so.** `action_axis` answers with
an INT (−1, 0, +1) — unlike `action_axis2`, whose vec3 carries `Fixed` — and
`local_tick` compared it against `fixed(0)`. Rhai has no `>` registered for
`(INT, Fixed)` and, rather than raising, **answers such a comparison with
`false`**: the burn key worked, the turn keys were inert, and the map ran
without a word. (The digger has always written `fixed(action_axis("drive"))`,
which is why its axes work.)

Why nothing caught it: **the goldens drive the SIM layer.** `ship_input`
synthesizes the command's button mask directly, so `local_tick` — the whole
key → command half — sits outside every golden and every canary this demo had.
Fixing the comparison without covering that would just move the next bug one
verb along, so the fix ships with `every_control_reaches_the_command`: a fake
bridge that holds keys, one `local_tick`, and an assertion on every bit of the
mask, including two held at once. `ship@` did not move, which is the same fact
from the other side.
- **S-6 (optional, separate) — rider interpolation.** §4.3: pass
  `(prev_pos, curr_pos, alpha)` into `build_instances` and lerp a rider's local
  position. The host already keeps both vectors for circle scenes
  (`lib.rs:1638-1642`); map scenes return an empty snapshot today
  (`positions`, `lib.rs:676`), so this is a real slice, not a flag. Needs a
  teleport rule of its own: `entity_attach` / `entity_detach` change frames, and
  interpolating across that smears a crate from the deck into space.

## 9. Traps found while reading, worth not rediscovering

- **The double draw.** Without D4 the ship is drawn twice, and the copies agree
  exactly — so the bug reads as "the hull looks wrong", not "there are two".
- **`update_scene`'s order** puts riders one frame behind the hull the moment
  anything interpolates (§4).
- **A stale camera centre** turns smoothing into a worse artifact than the
  judder (§4.4).
- **Sleep.** A ship at rest sleeps after `SLEEP_TICKS`; every external mutation
  wakes it (`world.rs:240`), and `apply_angular_impulse` must go through the same
  funnel or a stabiliser will silently fail on a parked ship.
- **The material-0 contract** (`physics.rs:47-60`): the first `phys_material`
  call is material 0, the id every un-materialed paint writes. On a ship with no
  terrain that is merely a convention — but the panic it guards is
  data-dependent and would fire at first contact, i.e. the first time two ships
  touch, long after the map "worked".
- **Momentum is not inherited on detach.** A crate released through the airlock
  keeps its world position and *zero* velocity while the ship coasts away — good
  drama, wrong physics. The honest fix is crates as bodies (their own slice);
  until then the demo should release only while station-keeping, or the map
  should own the lie knowingly.
- **`ship@` moves twice** — at S-1 (digest shape) and S-5 (behaviour) — and must
  not move at S-0 or S-3. A hash that moves at S-0 means render state leaked
  into the sim.

## 10. Open questions

- **Does the hull's own shape want to answer `blocked()`?** Once the body owns a
  truthful `VoxelShape` of the hull, the map is hand-maintaining a third copy of
  the same geometry (paint, shape, predicate). Slice 2 of the grid-entities plan
  (§8, a per-grid `VolumeStore` + `grid_solid`) would collapse paint and
  predicate; the body's shape is a fourth candidate for the same job. Worth
  deciding *before* S-2 whether `phys_shape_*` should write the grid's solidity
  store too, rather than after.
- **Crew inertia.** Riders are rigidly seated: a hard burn does not throw anyone
  down the corridor. Correct for an SS13-ish sim, wrong for a hard-burn one.
  Adding it means fictitious forces in hull-local coordinates — cheap to write,
  easy to make undeterministic. Not now.
- **How fast may a hull move before one tick of lag reads badly?** S-0's
  interpolation costs one tick (33 ms) of latency by design. A drifting station
  never notices; a fighter might. If it does, the answer is a higher `sim_hz`
  for that map, not extrapolation.
- **Multiple ships.** Everything above is per-body and per-grid, so two hulls
  work by construction — but each needs its own fog twin and the camera rides
  exactly one grid. First contact between two crewed hulls is where that gets
  tested.
