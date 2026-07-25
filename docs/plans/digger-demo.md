# monada-digger — the physics demo: drive, jump, drill

Status: **in progress — D1 + D2 landed** (D1: volume store,
physics-in-sim, `phys_*` verbs, `digger@` golden; D2: automatic body
mirror with full-quaternion grids, wheel cylinders, chase cam, ramp
field + jump beat); D3–D4 pending. The demo that pays off
`monada-physics` P0–P6
(docs/plans/voxel-physics.md): a drill-nosed vehicle in a voxel arena —
drive it, jump it off ramps, and bore tunnels through walls and floor,
pitching the drill up and down to slope the tunnel. Neutral naming
throughout: the crate is `monada-digger`, the map is "the digger demo".

Read alongside voxel-physics.md; this document plans the ENGINE seams
and the map, not the physics (which is done). Like the RTS and ship
plans, each slice lands with its own oracle golden.

---

## Locked decisions

- **This demo forces the 3D terrain store.** Tunnels and overhangs are
  the whole point, and `VoxelStore`'s column heightmap cannot represent
  them — the follow-up flagged since physics P2 comes due here. Column
  maps (RPG/ship/RTS) are untouched: the volume store is a per-map
  opt-in (`terrain = "volume"` in the manifest), not a migration.
- **Physics state lives beside the entity `World` and folds into the
  same `state_hash`.** One sim, one digest, one desync stream — the
  lockstep model is unchanged, so LAN comes free later even though the
  demo ships single-player.
- **The render mirror of physics bodies is engine-side and automatic.**
  The host iterates `bodies()`, keeps a roxlap grid per `BodyId`
  (`grid_spawn` + the e7ecdff off-origin/rotation re-basing), pushes
  poses per frame, and carves mirrors on `remove_voxels` outcomes. Map
  scripts never hand-mirror a body.
- **The drill loop is one host function.** Rhai calls
  `phys_drill(body, tool…, budget)`; the HOST runs
  query → cut-policy → carve store → `notify_terrain_edit` →
  `drill_reaction` in one deterministic sweep. The physics-plan §4
  contract ("the engine decides what gets cut") is satisfied — the host
  IS the engine; the policy (front-to-back within a hardness budget) is
  host code, parameterized by the script, exactly like the P6 test
  policy.
- **One small physics API addition: `DrillTool` grows an
  `orientation: FixedQuat`** (identity default — P6 behaviour
  unchanged, amendment to the physics plan on approval). Pitching the
  drill is the demo's core verb; a tool box locked to the body axes
  could only fake slopes by sweeping the anchor, and the reaction math
  is orientation-agnostic anyway (point + hardness). Query composes one
  extra rotation.
- **Debris is a render-side puff.** `DebrisCluster`s feed a local-layer
  particle effect (existing sprite path); nothing debris-shaped enters
  the sim. The falling-sand layer remains future work (physics plan
  §8), and this demo is its first honest consumer stub.

## §0 Problem & core idea

Everything below the surface is proven: deterministic voxel rigid
bodies, wheels, destruction, sleeping, drilling — 65 tests and a
`phys@` golden deep. What does NOT exist yet is the seam between that
crate and a playable map: physics inside the scripted sim state, a
terrain store tunnels can live in, bodies on screen, input on wheels.

| Layer | Owner | Hashed? |
|---|---|---|
| `PhysicsWorld` (bodies, wheels, cache, sleep) | sim (embedded in the driver state) | yes |
| Volume terrain (chunked voxels + materials) | sim (new store) | yes (per-chunk cached hashes) |
| Body render grids, wheel spin, debris puffs | render (host `MapRender`) | no |
| Camera, drill-pitch UI feedback, HUD | local layer | no |

## §1 New engine surface

### 1a. Volume terrain store (the headline)

`monada-script` (or a new `monada-terrain` module inside it — decide at
implementation by size): `VolumeStore`, a chunked 3D voxel store:

```rust
pub struct VolumeStore {
    /// 16³ chunks, keyed by chunk coords. BTreeMap — canonical walk.
    chunks: BTreeMap<(i64, i64, i64), Chunk>,
}
struct Chunk {
    /// Dense 16³ material grid (u16, 65535 = empty), same encoding
    /// as VoxelShape.
    cells: Box<[u16; 4096]>,
    /// Cached FNV of `cells`, refreshed on edit — the store's
    /// state_hash folds chunk hashes, not cells, so an unedited world
    /// hashes in O(chunks), not O(voxels).
    hash: u64,
}
```

- Implements `monada_physics::VoxelField` directly (occupied/material).
- Implements `monada_nav::NavWorld`? NO — out of scope; nav stays on
  column maps (non-goal below).
- Script fills it via the existing `voxel_fill`/`voxel_set`/
  `voxel_clear` verbs — on a `terrain = "volume"` map those route to
  the volume store (and the render world-grid, as today). Same script
  vocabulary, deeper semantics; the mapmakers-book gets a note, not a
  new chapter.
- Physics materials: the map declares them once
  (`phys_material(...)`), and terrain paints carry a material id (new
  optional argument to `voxel_fill`, defaulting to material 0).

### 1b. Physics in the sim state

`RhaiDriver`'s sim state grows `PhysicsWorld` + `VolumeStore` when the
manifest opts in. `state_hash` = entity-world fold ⊕ physics fold ⊕
terrain fold (append order fixed and documented — the same append-only
discipline as the physics crate). `step()` order per tick: commands →
script `on_tick` (inputs, drill calls) → `physics.step(&volume)` →
hash. Snapshot/replay: all three serialize together. *(D1 note:
"together" landed as "each of the three is serde-serializable and
round-trips bit-equal" — there is no unified snapshot container type,
and the persisted format remains the input replay. If the net layer
ever needs a state snapshot — late join — the container gets built
then; later slices must not assume it exists.)*

### 1c. Script API (sim layer, the deterministic wall)

Minimal verb set, mirroring the crate 1:1 where possible:

| Rhai | Backs onto |
|---|---|
| `phys_gravity(gx, gy, gz)` | `set_gravity` (D1 amendment: §1f gives the map the feel constants, so the map needs the verb — the crate's gravity is opt-in ZERO) |
| `phys_material(density, friction, restitution, hardness) -> int` | `register_material` |
| `phys_box(sx, sy, sz, mat, x, y, z) -> int` | `VoxelShape::fill_box` + `spawn_voxels` (box bodies only in v1; freeform authoring is v2) |
| `phys_wheel(body, ax, ay, az, rest, radius, k, c, mu) -> int` | `attach_wheel` (anchor in shape coords, host rebases via `com_in_shape`) |
| `phys_wheel_input(body, wheel, steer, drive, brake)` | `set_wheel_input` |
| `phys_impulse(body, jx, jy, jz)` | `apply_impulse` |
| `phys_pos(body) -> vec3` / `phys_vel(body) -> vec3` | pose reads for game logic |
| `phys_yaw(body) -> fixed` | heading about +z (D2 amendment: the chase cam follows the body's yaw, and the local layer cannot see physics — the sim reads it and aims the camera, the RPG `follow_camera` pattern) |
| `phys_drill(body, pitch, budget) -> int` | the one-call drill loop (locked decision); returns voxels cut for HUD/score |
| `phys_carve(body, cells…)` | `remove_voxels` (vehicle damage v2 — stub now) |

The drill tool geometry (anchor, half-extents) is map data set once at
vehicle spawn (`phys_drill_tool(body, …)`), pitch is the per-call
degree of freedom.

### 1d. Render mirror (host)

- Per `BodyId`: one grid (`grid_spawn`), voxels blitted from
  `shape()` + material→colour table at spawn/carve, pose from
  `position()`/`orientation()` per frame with the `com_in_shape`
  rebase. **Assumption to verify in D2**: e7ecdff's rotation re-basing
  covers arbitrary per-frame quaternions, not just yaw — if it is
  yaw-only, the fallback is yaw-from-quat for the body grid (visually
  fine for a vehicle) and a plan amendment.
  **D2 verdict: VERIFIED** — `GridTransform.rotation` is a full `DQuat`
  consumed by both CPU and GPU paths; per-frame transform writes are
  free (voxels upload once, keyed by chunk versions). Two roxlap
  caveats applied: `mip_levels_override = Some(1)` and
  `render_sky = false` on small rotating grids.
  **D2 amendment — isotropic volume rendering**: the assumption this
  plan DIDN'T flag was the render convention. Column maps render
  anisotropically (x/y ×SCALE, z unscaled), and a rotating grid
  supports rotation + uniform scale only — a tumbling body is
  unrepresentable in that convention. Volume maps therefore render
  isotropically: the world grid takes `voxel_world_size = SCALE` with
  ONE grid voxel per sim cell, and the world-X mirror + z-down flip
  compose to `diag(-1,1,-1)` — a proper rotation (`R_y(π)`) — so
  `world = (0,0,GROUND_Z) + SCALE·R_y(π)·sim` and a body's world
  orientation is `R_y(π)∘q`, exact for any `q`. Dividend: terrain
  paints shrink from `SCALE²` world voxels per cell to one.
- Wheels render as small kv6/voxel cylinders on their anchors, spin
  derived render-side from ground speed (the stateless-wheel dividend).
- Terrain carves mirror through the existing world-grid edit path.
- Debris: `DebrisCluster` → a short-lived local particle burst at the
  cluster position, coloured by material. Nothing persists.

### 1e. Input & camera (local layer)

Bindings (existing resolver): `drive` axis (W/S), `steer` axis (A/D),
`brake` (space), `drill` (hold LMB / E), `drill_pitch` axis (mouse-Y or
R/F) in [−45°, +45°], `camera` = chase cam behind the vehicle (yaw
follows body, pitch fixed), reusing the RPG camera seam. Inputs flow as
one per-tick command (verb 0, packed axes) — the RPG/ship pattern.

### 1f. Feel constants

Owned by the map script, not the engine: gravity −10, a vehicle tuned
from the physics test-suite stance (wheelbase ±3.5, track ±2.5,
k = 240, c = 80 — already characterized by 10 vehicle tests), drill
budget ~120/tick. Ramp jumps need no new physics: suspension + ballistic
flight are P1/P3 territory.

## §2 The map

One arena, authored in `main.rhai`:

- **Flat apron** with a start pad.
- **Ramp field**: 3–4 stepped-voxel ramps of increasing grade (the P3
  staircase lesson applies: run-ups matter and are part of the fun).
- **The mountain**: a solid block ~60×40×20 of layered materials —
  soft sandstone (hardness 10) wrapping granite veins (hardness 100)
  wrapping a hollow **crystal chamber** at the core.
- **The basement**: bedrock floor under the apron (hardness 40) hiding
  a second chamber below — reachable only by pitching the drill DOWN,
  driving down your own sloped tunnel (this is the acceptance for
  drill pitch; drilling back OUT is the acceptance for pitching up).
- **Objective (light)**: touch the 3 crystals (entity sensors on
  `phys_pos`); a HUD counter; no fail state. A sandbox with a finish
  line, not a game design document.

## §3 Slices

**D1 — physics-in-sim + volume terrain (headless).**
`VolumeStore` (+ chunk-hash caching), manifest opt-in, `PhysicsWorld`
embedded and hashed, script verbs `phys_material/box/wheel/
wheel_input/pos`, flat-floor map where a scripted box-on-wheels drives.
*Accept:* `digger@` oracle golden (headless, scripted inputs, 600
ticks); snapshot round-trip; combined-hash tripwire (terrain edit,
body spawn, and entity change each re-key).

**D2 — on screen.**
Automatic body mirror (rotation assumption verified or amended), wheel
cylinders, chase camera, bindings; the ramp field in the map.
*Accept:* human-playable drive + jumps; `digger@` unchanged (render
adds nothing hashed); the book gets a screenshotless "volume maps"
note. *(D2 note: "unchanged" holds for the RENDER — the mirror, wheels
and camera touch nothing hashed — but the ramp field is real terrain
and the golden schedule grew its jump beat, so `digger@` was re-blessed
once with the D2 map, like every map-growth slice.)*

**D3 — the drill.**
`DrillTool::orientation` (physics amendment), `phys_drill` host loop,
drill-pitch input, terrain carve mirror, debris puffs, the mountain +
basement in the map.
*Accept:* tunnel INTO the mountain, DOWN into the basement, and back
UP to the surface, all human-driven; `digger@` grows a scripted drill
sequence (including a pitched descent); hash-stable with streaming
edits (the P6 tunnel test, now through the full engine stack).

**D4 — objective + polish.**
Crystals + counter HUD, drill feedback from `phys_drill`'s return
(cut-rate as a crude "bite" indicator), material colours pass, feel
tuning.
*Accept:* the demo is completable start-to-crystals; `digger@`
re-blessed once with the final map; README paragraph + demo entry in
the workspace table.

## §4 Determinism

`digger@` joins the oracle at the standard checkpoints, driven like
`rpg@`: one packed input command per tick from a fixed schedule (drive,
steer, jump a ramp, drill in, drill down, drill up). The volume store
hashes via cached chunk digests; the scenario carves enough to cross
chunk boundaries (the caching's own tripwire).

## §5 Risks & assumptions

- **Grid rotation** (D2): see 1d — verify early, amend if yaw-only.
- **Volume-store hash cost**: chunk caching should make it negligible;
  if the golden run shows otherwise, hash granularity is the knob
  (per-chunk → per-region), not the design.
- **Terrain render edits at drill rate** (~6 voxels/tick): the roxlap
  edit path has handled RTS tree-felling; a tunnel is denser — D3
  measures, and batching per-tick edits into one region update is the
  fallback.
- **Column/volume duality**: two terrain modes in `monada-script` is
  real complexity; the mitigation is that the script VOCABULARY stays
  identical and the mode is a manifest flag resolved at load.

## §6 Non-goals

- Nav/pathfinding on volume maps (no AI drives here).
- Vehicle damage/carving the PLAYER's body (`phys_carve` stubs only).
- Freeform voxel body authoring in Rhai (boxes suffice; v2).
- Falling sand / persistent debris.
- Multiplayer shipping in the demo (the command stream keeps the door
  open; nothing more).
- Water, explosives, tool wear, fuel — the sandbox stays a sandbox.
