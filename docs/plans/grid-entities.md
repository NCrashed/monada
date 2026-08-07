# Plan: dynamic grid membership — entities that join and leave a grid

Status: **shipped** — S-0 (cubic cells, `host_api` 15) through S-5 landed
2026-08-06; what remains is §8's deferred slice 2 (grids that collide) and a
live-display pass on the demo. An additive host-API slice that makes a
`grid_spawn` grid a place entities can be *put into* and *taken out of* at run
time — a crate set down on a deck, a crew member stepping from the hull onto a
station, a shuttle that undocks and later stops existing. Requested by the
people extending the ship demo (`crates/monada-ship`); designed here so the
next slice of that demo is script work, not engine work.

Genre stays in the map (DESIGN.md §3.2): everything below is a neutral
primitive — a *frame* and a *membership*, not a door, an item or a cargo bay.

## 0. Where we are

`host_api` 7 gave maps a second voxel grid (`grid_spawn` / `voxel_fill_in`),
12 let an entity ride one (`entity_set_grid`, its position read as
grid-local), 13 gave the grid a pivot to turn about. The ship demo uses all
three: one hull grid, crew bound to it, the hull tumbling from a hashed `spin`
field (`crates/monada-ship/map/scripts/main.rhai:249-259`, `:359`, `:411`).

Everything about a grid lives on the **render** side
(`monada-host/src/map_render.rs`): `grids: Vec<GridId>`, `grid_anchors`
(spawn origin + pivot, `f64`), and `entity_grid: BTreeMap<EntityId, GridId>`.
The seat is composed in `place_in()` — `rotation · world_of(p) + origin` —
and `build_instances` prunes bindings whose entity has despawned.

That is enough to *ride* a grid. It is not enough to *join or leave* one.

## 1. What is missing, by failing scenario

| Scenario | What breaks today |
|---|---|
| Crew steps off the hull onto the station floor | `entity_set_grid(c, -1)` unbinds but does **not** convert coordinates: the crew's grid-local position is re-read as a world position, so it teleports (and by the hull's full rotation, not a fixed offset) |
| Crew boards a docked shuttle | same, mirrored: no way to express "keep the world pose, change the frame" |
| A crate is placed at the cell the player clicked | `pick_ground()` answers in *world* sim coords; there is no world→grid-local conversion, so the map cannot name the hull cell it hit |
| "Which hull is this entity on?" in `tick()` | `entity_set_grid` is write-only, and the binding lives render-side where hashed logic must not read it |
| "Everyone aboard" (a hull loses power, a shuttle launches) | no way to enumerate a grid's riders |
| The shuttle flies | a grid's origin is fixed at `grid_spawn`; only its rotation can change |
| The shuttle is destroyed / undocks and is gone | no `grid_despawn`; its voxels and its riders' bindings live forever |
| A door in the hull opens | `voxel_fill_in` has no inverse — a grid's voxels can be painted but never erased |

Every one of these is a *frame* problem wearing a different hat. The design
below adds the frame, then the eight verbs fall out of it.

## 2. Decisions

**D1 — the frame becomes sim-truthful, in fixed-point.** A grid's pose
(origin, pivot, rotation) is kept a second time in `monada-script`, in
`Fixed`/`FixedQuat`, computed from the exact arguments the script passed to
`grid_spawn` / `grid_move` / `grid_orient` / `grid_pivot`. This is what makes
conversion *deterministic*: the render transform is `f64` (glam quaternion
normalise, `sin`/`cos`), so anything derived from it may differ in the last
bits between peers and can never flow back into hashed state. The fixed-point
frame can — same contract `VoxelStore` already carries (`monada-script/src/lib.rs`,
`voxel_solid` / `nav_path`): *host-side state fed only by deterministic script
calls, so every peer answers identically and results may steer `tick()`.*

**D2 — the store is not (yet) in the desync hash.** Frames are derived from
state that is already hashed (the ship's `spin` field), so folding them in
adds little detection and costs a re-bless of every grid-using golden.
`GridStore::state_hash()` ships from day one so `RhaiDriver::state_hash` can
fold it later in one line (the physics precedent, `driver.rs:147-165`) — a
deliberate ceremony, not a silent one.

**D3 — attach/detach preserve the world pose; `entity_set_grid` keeps its
raw semantics.** Changing what v12's verb does would be a breaking change
(both `HOST_API_*` constants catch up, every shipped manifest migrates). So
the pose-preserving pair is *new* verbs, and the raw one stays for maps that
author positions directly in a grid's frame (what the ship does at spawn).

Rejected alternative: a third `keep_pose: bool` argument on
`entity_set_grid`. One concept, one verb family is tidier on paper, but a bare
`true` at the call site hides which of two very different behaviours you got.

**D4 — despawning a grid detaches its riders to the world; it never
despawns them.** Entity lifetime is hashed sim state and belongs to the map: a
render-side handle going away must not kill crew. The map that wants the crew
to die with the shuttle writes that rule itself.

**D5 — grid handles are monotonic and never reused** (the `EntityId`
argument, `monada-sim/src/entity.rs`): `grid_despawn` tombstones the slot.
A stale handle is inert — paints, orients and attachments through it are
ignored — never a silent hit on someone else's hull.

**D6 — the render side stays a mirror.** The host keeps its `f64` grid
transform and its `entity_grid` map exactly as they are; the new verbs
dual-write store + bridge. Nothing about the existing render path changes,
so the fog twin, the deck cutaway, picking and `camera_focus_entity` compose
as they do today.

## 3. The new surface (`host_api` 15)

All additive. Layers use the book's vocabulary: *simulation* = registered in
the sim backend (may mutate hashed state), *any* = registered in both the sim
and the local layer (read-only).

| Verb | Layer | Meaning |
|---|---|---|
| `grid_move(grid, point)` | simulation | set the grid's origin to sim point `point` (fixed-point, unlike `grid_spawn`'s integer cells) — a hull that flies. Riders and fog follow, exactly as they follow `grid_orient` |
| `grid_despawn(grid)` | simulation | retire a grid: riders are detached **keeping their world pose** (D4), its voxels are dropped render-side, the handle is dead forever (D5) |
| `grid_world(grid, p)` | any | grid-local sim point → world sim point |
| `grid_local(grid, p)` | any | world sim point → grid-local sim point |
| `entity_attach(entity, grid)` | simulation | bind *and* rewrite the entity's position to the grid's frame, so it does not move in the world |
| `entity_detach(entity)` | simulation | the inverse: rewrite to world coordinates and unbind |
| `entity_grid(entity)` | any | the grid an entity rides, or `-1` |
| `grid_riders(grid)` | any | ascending ids of the entities riding it |
| `voxel_set_in(grid, x, y, z, color)` | presentation | one cell, completing `voxel_fill_in` |
| `voxel_clear_in(grid, x, y, z)` | presentation | erase one cell of a grid — the door / breach primitive. Render-only, like every `*_in` verb (§8 gives it collision) |

Sketch of the `HostBridge` side (`monada-script/src/lib.rs`), in the house
style — every method defaulted, so headless bridges and every existing map are
untouched:

```rust
/// Set a `grid_spawn` grid's origin, in SIM coordinates — the frame
/// `grid_spawn` placed it in, now movable. Entities bound to it via
/// `entity_set_grid` / `entity_attach`, its fog and its `deck_clip` ride the
/// new origin. Unlike `grid_spawn`'s integer cells this is fixed-point, so a
/// hull can drift a fraction of a cell per tick. An out-of-range or despawned
/// handle is ignored. The default ignores it.
fn grid_move(&mut self, _grid: i64, _origin: FixedVec3) {}

/// Retire a `grid_spawn` grid: its voxels leave the scene and its handle dies
/// (handles are never reused, so a stale one is inert rather than a hit on a
/// later grid). Riders are NOT despawned — the sim owns entity lifetime; they
/// are detached keeping their world pose, as if the map had called
/// `entity_detach` on each. The default ignores it.
fn grid_despawn(&mut self, _grid: i64) {}

/// Erase one sim cell of a `grid_spawn` grid — a door opening, a hull breach.
/// The `voxel_fill_in` inverse, same coordinate convention. Render-side only
/// (no collision store behind a dynamic grid yet — see the plan's §8). The
/// default ignores it.
fn voxel_clear_in(&mut self, _grid: i64, _x: i64, _y: i64, _z: i64) {}
```

The *queries* (`grid_world` / `grid_local` / `entity_grid` / `grid_riders`)
and the pose-preserving *pair* (`entity_attach` / `entity_detach`) are **not**
bridge methods: they are answered by the shared store in `monada-script` (§4),
because a bridge is a render-side sink with no handle on the `World` (the
`camera_focus_entity` note already spells this out) and because a headless
bridge answering "no grid" while a live one answers "grid 0" would desync the
oracle against the real host.

## 4. `GridStore` — the deterministic frame table

New: `crates/monada-script/src/grids.rs`, alongside `VoxelStore` /
`VolumeStore`.

```rust
/// One `grid_spawn` grid's rigid frame, in SIM coordinates and fixed-point.
/// The render side keeps its own f64 copy (map_render's `GridAnchor`); this is
/// the one a script may compute against.
struct GridFrame {
    /// Where the grid sits (`grid_spawn`'s cell offset, then `grid_move`).
    origin: FixedVec3,
    /// The grid-local point `rot` turns about (`grid_pivot`), ZERO until named.
    pivot: FixedVec3,
    /// The pose `grid_orient` last set — replaced whole, never accumulated.
    rot: FixedQuat,
    /// `grid_despawn` tombstones the slot; the handle is never reused.
    alive: bool,
}

pub struct GridStore {
    grids: Vec<GridFrame>,
    /// entity → grid handle. `BTreeMap`, so every walk is deterministic.
    riders: BTreeMap<EntityId, u32>,
}
```

The two functions everything else is built from:

```
to_world(h, p) = origin + pivot + rot · (p − pivot)
to_local(h, p) = pivot + rot⁻¹ · (p − origin − pivot)
```

Round-tripping is exact to fixed-point rounding, and both agree with the host's
`apply_grid_pose` (`origin = spawn_origin + (I − R)·pivot`,
`map_render.rs:1675`) — the same transform, once in sim space and once mapped
through `world_of`.

Then:

- `attach(world, e, h)` = `set_position(e, to_local(h, position(e)))` + `riders.insert`
- `detach(world, e)` = `set_position(e, to_world(h, position(e)))` + `riders.remove`
- `despawn(world, h)` = `detach` every rider of `h`, then `alive = false`
- `retain(world)` — drop riders whose entity is gone, run once per tick from
  `RhaiBackend::on_tick` (which holds both the world and the store), mirroring
  what `build_instances` already does render-side
- `state_hash()` — canonical digest (count, then per grid: origin, pivot, rot,
  alive; then the rider map), unused until D2's ceremony

### Wiring

`register_grid_api(engine, &grids, bridge)` in the physics registration's
shape (`physics.rs`, `register_physics_api`): Rhai resolves at call time and
later registrations shadow earlier ones, so

- `RhaiBackend::new` registers the store-only verbs (queries, attach/detach) —
  a bridgeless headless backend answers correctly;
- `set_bridge` re-registers `grid_spawn` / `grid_move` / `grid_orient` /
  `grid_pivot` / `grid_despawn` / `entity_set_grid` to **dual-write** store and
  bridge (store first, so the handle the store allocates is the handle the
  script sees; the bridge mirror keeps its own `handle → GridId` map);
- `LocalBackend` gets `register_grid_read_api` only — the same split
  `register_world_read_api` already draws, so the unsynced layer can turn a
  `pick_ground()` into a hull cell but can never move a hull.

`RhaiDriver` exposes `grids()` next to `physics()`, so the host can read the
store between ticks if it ever wants to (it does not need to for this slice).

## 5. Host mirror changes (`monada-host/src/map_render.rs`)

Small, and all of it in the existing shape:

- `grids: Vec<GridId>` becomes `Vec<Option<GridId>>` (tombstones), or keeps
  `GridId` plus a `dead: BTreeSet<usize>` — either way `usize::try_from(grid)
  .and_then(|i| self.grids.get(i))`, the resolve idiom every grid verb already
  uses, gains a liveness check in one helper.
- `grid_move`: write `GridAnchor::spawn_origin` from the sim point (the
  `world_of`-style map `grid_spawn` already does, minus the cell rounding),
  then `apply_grid_pose` with the current rotation — one call site, so a move
  after an orient and an orient after a move land the same pose.
- `grid_despawn`: `Scene::remove_grid` (verified present in roxlap-scene
  0.31.1, and its `GridId`s are never reissued — so the mirror inherits D5's
  safety for free), drop its `GridAnchor`, drop every `entity_grid` entry
  pointing at it, and re-target the fog if the observer was riding it
  (`retarget_vision` / `drop_fow`, the path `entity_set_grid` already uses —
  the fog twin is `attach`ed to a specific grid and must not outlive it).
- `voxel_set_in` / `voxel_clear_in`: `sim_box_to_world` on a degenerate box,
  `grid.set_rect(lo, hi, Some(..) | None)` — exactly `voxel_set` / the volume
  branch of `voxel_clear`, against the named grid.

**Headless consequence to verify:** with the store in place, `grid_spawn`
returns a real handle even under `TerrainBridge`/`NullBridge`. The ship map
today bails out of `init` when it gets `-1` (`main.rhai:250`), so the `ship@`
golden currently hashes a *hull-less* run; once the handle is real, `init`
runs to the end. Everything past that bail is render-side (`build_hull` →
`voxel_fill_in`, `grid_pivot`, `model_actor`, camera, `vision_config`,
`status`), so the world hash should be **unchanged** — but this is exactly the
kind of "should" the oracle exists to check: run `cargo run -p monada-oracle`
before and after, and if `ship@` moves, find out why before blessing.

## 6. The coordinate caveat — RESOLVED by cubic cells (`host_api` 15, landed)

On a **column** cell the sim→world map is anisotropic: `world_of` scales x/y by
`SCALE` and leaves z unscaled (`map_render.rs`, the (C) resolution in
docs/plans/ship-visibility.md). A sim-space rotation `R_s` and the render's
world rotation `R_w` agree only if `R_w = M R_s M⁻¹` with `M = diag(−S, S, −1)`
— and conjugation by a *non-uniform* diagonal carries a rotation to a rotation
only when the axis is along z. So on a column grid only yaw is exact, and
`grid_world` / `grid_local` could not be trusted on a tilted hull — which is
exactly what the ship demo tumbles.

**What landed instead of a `terrain = "volume"` migration.** The exactness comes
from *cubic cells*, not from the volume terrain mode: `grid_spawn_cubic` spawns
a grid whose cell is a `SCALE³` cube, making `M = S · diag(−1, 1, −1)` — a
uniform scale times a proper half-turn — so `W ∘ R_sim = R_world ∘ W` holds for
any axis. The cell shape is per-grid (`GridAnchor::cubic`), so `voxel_fill_in`,
`grid_pivot`, a bound entity's seat, `deck_clip` and the fog band all follow the
grid they were handed and every pre-15 map is byte-unaffected (the goldens agree:
only `ship@150`/`ship@600` moved, and only because the ship's own geometry was
re-tuned).

Why not `terrain = "volume"`, which was the first instinct: that mode renders one
voxel per cell, and roxlap's fog of war hard-codes its eye-level opacity band at
`EYE_HALF = 2` **voxels** (`roxlap-scene/src/fow.rs`). At cell-sized voxels that
band is ±2 *cells*, so the floor under the crew's feet falls inside it and every
column with a floor reads opaque — the fog collapses unless decks are ~7 cells
tall (a crew is 1.4). A cubic grid keeps `voxel_world_size = 1`, so the FoW,
`z_clip` and the cutout stay in the units they were tuned in. Volume remains the
right mode for a *digging* map; it is not what a rotating hull needed.

The one real cost: **vertical geometry is cell-quantised** on a cubic grid — a
wall is a whole number of cells tall and the finest stair step is one cell. The
ship's hull was re-tuned accordingly (deck stride 28 unscaled units → 3 cells,
walls → 2 cells, the stair → 4 one-cell steps), and the fog's eye offset became
`SCALE + 2·EYE_HALF` above the feet, so the band clears a full-cell riser
instead of being blocked by it.

## 7. What the ship demo writes with it

Placing an item on the deck the player clicked (local layer picks, sim layer
places — the input path is unchanged):

```rhai
// local: the cursor's world ground point, expressed in the hull's frame
fn local_tick(dt) {
    let g = pick_ground();
    if g != () {
        submit_command(1, 0, grid_local(hull_grid(), g));
    }
}

// sim: a crate entity that rides the hull
fn command(player, verb, target, arg) {
    if verb == 1 {
        let crate_e = entity_create(2);          // CRATE archetype
        entity_set_position(crate_e, arg);        // already grid-local
        entity_set_model(crate_e, crate_model()); // a kv6
        entity_set_grid(crate_e, hull_grid());    // raw bind: authored in-frame
    }
}
```

Picking it up and dropping it in the corridor (both frames, no teleport):

```rhai
fn pick_up(c, item) {
    entity_detach(item);                 // world pose preserved
    entity_set_field(item, "carrier", fixed(c));
}

fn drop_at(item, c) {
    entity_set_position(item, grid_world(entity_grid(c), entity_position(c)));
    entity_attach(item, entity_grid(c)); // world pose preserved, back on the hull
}
```

Crew stepping onto a docked shuttle, and the shuttle leaving with whoever is
aboard:

```rhai
fn board(c, shuttle_grid) { entity_attach(c, shuttle_grid); }

fn launch(shuttle_grid) {
    // everyone aboard rides the move for free; nothing else to update
    grid_move(shuttle_grid, vec3(fixed(40), fixed(0), fixed(0)));
    for e in grid_riders(shuttle_grid) { entity_set_field(e, "aboard", fixed(1)); }
}

fn scuttle(shuttle_grid) {
    grid_despawn(shuttle_grid);   // riders detach to the world, alive (D4)
}
```

A door: `voxel_clear_in(hull, x, y, z)` on open, `voxel_fill_in` on close.
Until §8 lands that is the eye only — the map's own `blocked()` predicate stays
the collision truth, which is what the ship demo already does for walls.

## 8. Slice 2 (designed, deferred): grids that collide

The demo's real ceiling is not membership, it is that **a dynamic grid feeds no
collision at all** — `voxel_fill_in` is render-only, so the ship hand-writes
`blocked()` and keeps it in sync with what it paints (`main.rhai:142-153`),
and the heightmap `VoxelStore` cannot represent stacked decks anyway (the
"real engine gap" in docs/plans/ship-visibility.md's open questions).

The pieces are already in the tree: `VolumeStore` (`monada-script/src/volume.rs`)
is a chunked, hashed, 3D sparse voxel store with materials — exactly the shape a
per-grid solidity store needs, and it hole-punches (no heightmap truncation).
So slice 2 is mostly plumbing, not invention:

- one `VolumeStore` per grid in `GridStore`, fed by `voxel_fill_in` /
  `voxel_set_in` / `voxel_clear_in`;
- `grid_solid(grid, x, y, z)` — the per-grid `voxel_solid`, with the same
  determinism contract;
- decks and doors become truthful, `blocked()` retires, and a crate placed at a
  cell can block movement instead of only decorating it;
- it *is* hashed state (a `VolumeStore` already hashes), so this is the slice
  where D2's ceremony happens: fold `GridStore::state_hash()` into
  `RhaiDriver::state_hash` and re-bless `ship@`.

Also deferred, and worth naming so nobody designs around its absence: **there is
no ray-pick against a dynamic grid.** `pick_ground` intersects the world ground
plane, so §7's `grid_local(pick_ground())` is honest only while the hull's deck
is near that plane. A `pick_grid_cell(grid)` verb (ray vs. the grid's voxels,
grid-local cell out) is the right answer, and roxlap's picking already resolves
grid/voxel hits (DESIGN.md §3.2) — a separate, small slice.

## 9. Build order

Each step is a headless test; nothing needs a display until S-4.

- **S-0 — cubic cells. DONE (2026-08-06).** `grid_spawn_cubic` +
  `GridAnchor::cubic` + the per-grid branches in `voxel_fill_in` / `grid_pivot` /
  `place_in` / `apply_deck_clip` / `update_fow` (host), its Rhai registration and
  `HOST_API_VERSION` 15 (additive — `HOST_API_OLDEST` stays 1), the ship hull
  re-tuned to cells, `book/src/reference.md`, `ship@` re-blessed. Four new host
  tests, one of which pins the whole point: a cubic hull turned about a tilted
  sim axis seats a bound entity exactly where the sim-space prediction says,
  while the column-cell grid provably cannot. This is a prerequisite of the
  frame math below, not part of it — §4's `to_world`/`to_local` are only
  meaningful on a grid whose cells are cubes.

- **S-1 — `GridStore` + math. DONE (2026-08-06).**
  `crates/monada-script/src/grids.rs`: the frame table, `to_world`/`to_local`,
  `attach`/`detach`/`despawn`/`retain`, `set_grid` (the raw v12 bind),
  `state_hash`; 9 unit tests. One finding worth keeping: `from_axis_angle`
  builds the quaternion from fixed-point `sin`/`cos`, so it is only NEARLY
  unit — and `inverse` is the conjugate, whose product with the original scales
  by `|q|²`. A world→local→world trip was therefore a small dilation about the
  origin (~1.2e-6 of a cell at hull scale, and compounding per round trip).
  `orient` now normalises on the way in, which puts the trip back inside
  rounding. The "convert at moments, not every tick" advice stands regardless.
- **S-2 — script surface. DONE (2026-08-06).** `register_grid_api` (dual-write
  store + bridge, registered after `register_bridge_api` so it shadows the
  bridge-only grid verbs) + `register_grid_read_api` for the local layer;
  `RhaiBackend` owns the store and prunes bindings once per tick;
  `RhaiDriver::grids()` / `LocalBackend::set_grids` hand it to the host.
  `HOST_API_VERSION` 16 (`HOST_API_OLDEST` stays 1 — additive). Five
  script-level tests in `tests/grid_frames.rs`, including the wall: the local
  layer resolves `grid_world` but raises on `entity_attach`. Two side-findings:
  (a) the book's "reference can't drift" gate scraped only three files, so every
  `grid_*` verb passed it vacuously — `grids.rs` is now scraped too; (b) a Rhai
  function yields its last statement's value even with a semicolon, so an `init`
  ending in `entity_attach(...)` died with "Output type incorrect: bool
  (expecting ())" — the trigger callers now take a `Dynamic` and drop it, which
  also retires the same trap for `entity_create` / `entity_despawn`.
- **S-3 — host mirror. DONE (2026-08-06).** `grid_move` / `grid_despawn` /
  `voxel_set_in` / `voxel_clear_in` in `map_render.rs`; the handle table became
  `Vec<Option<GridId>>` (tombstones) behind one `grid_id` resolver, and
  `grid_despawn` tears the fog down BEFORE `Scene::remove_grid` — `retarget_vision`
  un-clips the grid it is leaving and detaches the twin, both of which reach
  into the scene for a grid that is about to stop existing. Four tests, the
  important one being `the_sim_frame_and_the_drawn_frame_agree`: feed the same
  script calls to `monada-script`'s fixed-point `GridStore` and to `MapRender`,
  then require the store's `to_world` (mapped sim→world) to land where `place`
  renders. Measured agreement ~1e-6 world voxels over a 20-cell hull — the whole
  slice rests on that number, so it is now a test rather than an argument.
- **S-4 — the demo uses it. DONE (2026-08-06).** Ship map: a `CRATE`
  archetype, two crates stowed on the lower deck, `use` (E) to pick up / set
  down and `door` (F) to cycle a starboard airlock, both folded into the
  existing per-tick input command's spare `arg.z` as a bit mask and acted on at
  the RISING EDGE (the local layer is stateless, so the debounce is a hashed
  `prev` field). The rule that makes the slice visible: setting a crate down
  inside the ship leaves it in the ship's frame, releasing it while standing in
  the open airlock `entity_detach`es it — so it hangs in space exactly where it
  was let go while the hull turns and sways away from it. The hull now also
  `grid_move`s (a slow sway derived from the same hashed spin), so riders are
  carried by a frame that translates as well as rotates. Manifest `host_api =
  16`. Four new tests in `smoke.rs`, incl. the release-into-space one, plus the
  airlock gating passability both ways.
- **S-5 — goldens. DONE (2026-08-06).** `ship@` was unchanged through S-3, as
  predicted (§5), and moved at S-4 — re-blessed. The oracle's `ship_input` now
  presses the two buttons on fixed ticks (2 = pick up, 240 = airlock, 320 = set
  down), so grid MEMBERSHIP is under the golden rather than beside it. Every
  other scenario stayed byte-identical throughout. `book/src/reference.md`
  carries the new verbs plus a "Grid frames" section, and the drift gate now
  scrapes `grids.rs` so it can never miss a `grid_*` verb again.

## 9b. Found on the first live run

- **A prop rode its grid's position but not its ROTATION** (fixed). roxlap's
  static sprite instance (`SpriteInstanceDesc`) carries a position and nothing
  else, so a crate on the tumbling hull stayed world-axis-aligned while the ship
  rolled under it. Orientation exists only on the renderer's DYNAMIC layer
  (`DynSpriteTransform`'s right/up/forward basis, what `.rkc` characters already
  use), so `build_instances` now routes a sprite bound to a TURNING grid there
  (`prop_targets` → `sync_props`) and leaves everything else on the cheap static
  path. The pivot drop turns with it — it is a model-space offset, so on a hull
  rolled onto its side it has to push the crate sideways, not down.
- **A prop left a FROZEN GHOST of itself** (fixed, and the sharper half of the
  same bug). In a dynamic-layer map the static sprite set is uploaded exactly
  once — re-uploading resets the actors — so a static instance is nailed to
  wherever it stood on the first rendered frame. Routing only *turning* grids to
  the dynamic layer meant every prop was baked in on frame 0 (the hull has not
  turned yet, `grid_orient` runs in `tick`) and then ALSO drawn posed: one copy
  riding the ship, one hanging in space. On screen the crate you carried looked
  right while the one you left behind "flew past" the hull. The test is now "does
  it ride a grid", not "is that grid turning".
- **The camera did not ride the hull, so the CONTROLS drifted** (fixed, new verb
  `camera_grid`, `host_api` 17). This one was mis-filed as a look: a crew member
  bound to a grid has a grid-LOCAL position, so with a world-fixed camera the
  map's view-relative input steered in the ship's frame while the player watched
  the world's — "forward" pointed somewhere new every tick the hull turned.
  `camera_grid(grid)` turns the whole orbit frame (basis + eye offset) by the
  grid's rotation, which re-aligns the two and needs nothing from the map's
  movement math. Consequence to weigh on the next live pass: with a TILTED tumble
  axis the camera now rolls too — the deck holds still and the starfield sweeps.
  If that reads badly, the demo's tumble axis becomes pure `+z` (one literal) and
  the camera only yaws.
- **Still upright on a rolling hull, by the same mechanism:** a `.rkc`
  character (`CharacterModel::transform` takes a yaw, not the grid's basis) and
  the selection ring (`sync_rings` places unposed instances). Neither is in the
  ship demo's path — the crew are billboards, which cannot tilt at all — so they
  are noted, not fixed. A billboard actor riding a rolling hull is a deeper
  question than a bug: it has no "tilted" art to show.

## 10. Open questions

- ~~**Tumbling hull vs. exact frames** (§6)~~ — resolved: cubic cells
  (`grid_spawn_cubic`), landed ahead of S-1. The ship keeps its tilted tumble
  AND gets an exact frame; the demo's vertical geometry is now cell-quantised.
  The live pass is still owed: the hull is 5 cells (80 world units) tall where
  it was 52, so the follow camera's `camera_dist(60)` and the stair's one-cell
  steps want eyes on a real display.
- **Should `grid_move` interpolate?** It sets a pose per tick, like
  `grid_orient`; at 30 Hz a fast hull will visibly step. The render side could
  lerp between the last two poses (it has `dt`), which is pure presentation and
  never hashed. Probably yes, but as its own change — it applies to
  `grid_orient` equally, and rotation wants a slerp.
- **Rider count.** `GridStore::riders` is one `BTreeMap` walk per query;
  `grid_riders` on a hull with hundreds of items is fine, a per-grid reverse
  index is the fix if an SS13-scale map ever needs it. Not now.
