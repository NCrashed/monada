# Plan: the ship demo — grid placement (snap, ghost, validity)

Status: **built** (P-0 … P-5; see §7 for what shipped and where it differs
from the design below). Point at a deck cell, see the grid you are snapping
to, see a ghost of the thing you are about to put down, see whether it will
be accepted, turn it, and let go.

The request, from players of the demo:

> кстати насчет отдельных механик, вообще удобная установка блоков, с
> вращениями, сеточкой, прозрачностю во время установки, индикацией
> возможности установки и тд, отдельная тасочка
>
> м, в демке перс просто берет ближайший ящик, не по курсору

Two asks, and they are the same ask: **the map cannot see the cursor.** The
second is not a bug in `nearest_crate` — it is the only thing that function
could have been. Everything below follows from closing that.

Locked stance, unchanged from the demos before it (DESIGN.md §3.1–3.2):
genre lives in the map, the engine gains only neutral primitives. "A crate
goes on a deck cell and a wall does not" is the ship map's opinion; "the
cursor hits *that* voxel of *that* grid" is the engine's.

## 0. Why the demo cannot do this today

`nearest_crate` (`map/scripts/main.rhai:500`) picks by distance because
distance is the only thing the map can measure. The cursor is not available
to it, in three separate ways:

1. **`pick_ground()` answers `()` on this map, always.** The ship declares
   `terrain = "volume"` for the physics, so the cursor takes the volume path
   (`map_render.rs:2544`), and that path opens with `let id = self.world_grid?`
   (`map_render.rs:2783`). The ship has no world grid — every voxel it owns
   lives in its own hull grid — so the pick returns `None` before it starts.
   The same `?` is the reason the camera-collision pass is gated off for this
   map (`map_render.rs:2632`), and that comment is the honest statement of
   the situation: *this map's geometry is not the world's*.
2. **Even a working `pick_ground` answers the wrong question.** It returns an
   `xy` with `z` dropped (`map_render.rs:4570`) against the `z = 0` plane. A
   two-deck hull has two floors at different heights, and it is a rigid body
   — under thrust, at any attitude, in a frame that turns. A world plane
   cannot name a cell of it.
3. **`pick_entity` derives from the same hit** (`map_render.rs:2569`), so it
   is dead here for the same reason. It also measures world-`xy` distance,
   which stops meaning anything once the hull is tumbling.

And there is nothing a map may *draw* for a placement preview. `ui_*` is 2D
HUD pixels. `voxel_fill_in` paints the hull itself: cell-quantised (a cubic
cell is `16³` voxels, so there is no such thing as a thin grid line), opaque
by construction (a voxel colour's high byte is brightness, not alpha —
`roxlap_formats::color`), and a mesh edit rather than an overlay. The desert
draws its build ghost by painting an overlay grid
(`monada-desert-rules/src/lib.rs:1206`), which works because its ghost is a
whole flat cell on static ground; a crate-sized outline on a moving hull is
not that.

So: two primitives are missing, and both already exist one layer down.

## 1. The two primitives (`host_api` 24)

Additive; `HOST_API_OLDEST` stays 1. Both are **local-layer, render-side,
never hashed**, and both are no-ops on a headless peer — the same contract
`ui_*` and the camera verbs have.

| Verb | Layer | Meaning |
|---|---|---|
| `pick_grid()` | local | script handle of the grid the cursor ray first meets, or `-1` |
| `pick_cell(grid)` | local | the sim **cell** of that hit, in `grid`'s own cells; `()` if the cursor misses that grid |
| `pick_face(grid)` | local | unit face normal of the hit, in the same cells (`cell + face` = the empty cell in front of the surface); `()` on a miss |
| `gizmo_style(width_px, on_top)` | local | line width and depth behaviour for the gizmos that follow (the `ui_scale` idiom) |
| `gizmo_box(grid, x0,y0,z0, x1,y1,z1, color)` | local | wireframe of an inclusive cell box in `grid`'s frame; `color` is `0xAARRGGBB` — **real alpha** |
| `gizmo_line(grid, a, b, color)` | local | one segment between two sim-space points of `grid`'s frame |

`grid = -1` means the world frame, so a column map (chess, RPG, RTS) gets the
same verbs without owning a grid.

**The engine work is glue, not invention.** roxlap 0.32 already has both
halves:

- `Scene::raycast_clipped(origin, dir, max)` → `RayHit { grid, voxel, world, t }`
  — the nearest solid voxel across every queryable grid, each ray transformed
  into that grid's own frame, so a rotated/translated hull is handled exactly.
  Its *clipped* variant reads each grid's `Grid::z_clip`, which
  `apply_deck_clip` (`map_render.rs:1699`) already sets on the crew's hull
  from the map's `deck_clip` call. **The cursor therefore lands on the deck
  the player can see, not the roof that is cut away, for free** — this is the
  reason to build the pick on the scene raycast rather than on any plane.
- `Line3 { a, b, color: OverlayColor, width_px, depth_test }` — world-space
  overlay segments with a genuine alpha byte, composited over the rendered
  frame. `map_render` already draws them twice: the drag rectangle
  (`map_render.rs:3332`) and the F1 collision footprints
  (`map_render.rs:3367`). `gizmo_*` is that, addressed in a map's cells and
  composed through a grid's pose.

Host-side shape:

| File | Change |
|---|---|
| `monada-host/src/map_render.rs` | store the cursor ray in `set_cursor_ray` (it currently keeps only the derived `cursor_ground`/`cursor_entity`); `pick_grid`/`pick_cell`/`pick_face` over `scene.raycast_clipped`; a `gizmos: Vec<Line3>` drained in `render_into` after `render`, alongside `draw_drag_rect` |
| `monada-host/src/map_render.rs` | `cell_of_voxel(grid, IVec3) -> (i64,i64,i64)` — the inverse of `cell_box_to_cubic` / `sim_box_to_world`, picked by the grid's own cell shape (`grid_is_cubic`), so a map reads back the numbers it painted with |
| `monada-runtime/src/host.rs` + `lib.rs` | the six bridge methods (defaulted, like every other), `HOST_API_VERSION = 24` |
| `monada-script/src/local_backend.rs` | register the six into the local layer **only** — the simulation must never see where a cursor is (`host.rs:353`) |
| `book/src/input.md`, `book/src/reference.md` | document them; `api_reference_matches_registered_functions` (`monada-oracle/src/lib.rs:1206`) fails CI until the reference matches |

Caveat worth one line in the book: the pick resolves against the pose in the
scene *this frame* (the smoothed one), while the sim acts on the tick-exact
pose — up to a tick of cursor offset on a hard-burning hull. Same asymmetry
`ship-physics.md §4.4` already accepted for the camera.

## 2. Who decides what

| | Local layer (per client, unsynced) | Simulation (lockstep, hashed) |
|---|---|---|
| Where the cursor points | ✔ `pick_cell` | never |
| What the ghost looks like | ✔ `gizmo_*` | never |
| Whether a cell is legal | asks `can_place()` | **decides** with `can_place()` |
| The prop actually moving | never | ✔ on the `place` verb |

The two columns cannot drift, and not by discipline: **the local layer
compiles the same script file** (`monada-script/src/local_backend.rs:130`)
and holds the world reads (`entity_position` / `entity_field` / `entities_of`)
and the frame reads (`grid_world` / `grid_local` / `entity_grid`). So
`can_place(cx, cy, deck)` is *one Rhai function*, called by the ghost to
colour itself and by `tick()` to accept or refuse. The ghost cannot lie, and
the sim still does not trust it — it re-runs the predicate on the command,
because a peer could be lying on purpose.

**Command shape.** The map already spends its one command per tick on
`vec3(move_x, move_y, buttons)` (`main.rhai:615`), and a `vec3` is full. Add
a second command per tick, verb 1 = *aim*: `arg = vec3(cell_x, cell_y, rot)`,
with the target deck implied by the crew member's own (a player can only
build on the deck they stand on). `command()` stores it on the crew entity;
`step_use` acts on the stored aim when the `use` bit rises. Two commands per
tick per player, both fixed-point, no floats in the stream.

## 3. What the map then writes

New sim state on the crew archetype: `aimx`, `aimy`, `rot`. New archetype
field on a placeable: `rot`, and a `kind` once there is more than one.

```rhai
/// Is hull cell (cx, cy) on `deck` free for a prop of footprint `f`, for a
/// crew member standing at `(px, py)`? The ONE predicate: the ghost colours
/// itself with it, and `tick()` accepts or refuses with it.
fn can_place(cx, cy, deck, rot, px, py) {
    for c in footprint(cx, cy, rot) {          // 1x1 crate → itself; 2x1 → two cells
        if blocked(c.x, c.y, deck) { return false; }   // walls, rim, closed airlock
        if on_stairs(c.x, c.y)     { return false; }   // the stairwell stays walkable
        if occupied(c.x, c.y, deck) { return false; }  // another prop already there
    }
    reach2(cx, cy, px, py) <= ratio(9, 4)      // the same 1.5 cells `use` has always had
}
```

`occupied` scans the prop archetype for a hull cell match — the map's own
question, over entities it owns, in the frame they ride.

The local layer, once per **frame** (so the ghost tracks the cursor at render
rate rather than 30 Hz):

```rhai
fn local_frame(dt) {
    let hull = 0;
    let cell = pick_cell(hull);
    if cell == () { return; }
    let c = my_crew();                  // entities_of(0)[local_player()]
    let deck = to_int(entity_field(c, "deck"));
    let p = entity_position(c);
    let ok = can_place(to_int(cell.x), to_int(cell.y), deck, my_rot(), p.x, p.y);

    gizmo_style(2, false);
    grid_lattice(hull, p, deck);        // the cells within reach, faint white
    gizmo_style(3, true);
    ghost_box(hull, cell, deck, my_rot(), if ok { 0x9040_ff70 } else { 0x90ff_5050 });
}
```

`grid_lattice` outlines the cells around the crew member on their deck — the
"сеточка", drawn where the snap actually is rather than as a decorative
overlay. `ghost_box` outlines the footprint at the target cell, in green or
red, with alpha. Both are `gizmo_box` calls in the hull's frame, so they ride
a turning ship without the map doing any transform arithmetic.

**Rotation needs something that is not square to mean anything.** A 1×1 crate
looks identical at all four angles. So the slice adds one 2×1 prop — a floor
locker — and `rot` ∈ 0..3 turns its footprint; `R` cycles it. This is what
makes "с вращениями" a real feature rather than a stored integer.

**Transparency**, honestly: a voxel colour has no alpha (brightness byte), so
a *solid* see-through crate is not something this renderer does. What it does
have is alpha-blended overlay lines, which is what the ghost is. If a filled
ghost body is wanted later, the map can carry one ghost entity per player in
the sim with `entity_set_tint` — visible to everyone, which for a co-op crew
is arguably right ("Bob is about to put a locker there") and needs no engine
work at all. Deferred, not designed away.

## 4. Build order

| Slice | What | Accept |
|---|---|---|
| **P-0** | `pick_grid` / `pick_cell` / `pick_face`, host + local registration + book | a host unit test casts a ray down a known hull column and reads back the cell the map painted; a second proves the deck cutaway redirects the hit to the lower deck (the `deck_clip` probe at `map_render.rs:4883` is the model). `ship@` unmoved |
| **P-1** | `gizmo_style` / `gizmo_box` / `gizmo_line` + book | a box drawn in a rotated grid's frame lands at the composed corners (unit test on the line list, no window); on screen, an outline that stays on the hull as it turns. `ship@` unmoved |
| **P-2** | the crew picks up **what the cursor points at**: `nearest_crate` → `crate_at(cell)`, aim command, reach still enforced in the sim | the smoke canary picks the far crate while standing between two; out of reach still refuses. `ship@` re-blessed (new verb in the stream) |
| **P-3** | occupancy + `can_place` + place-at-aim; `use` sets down at the ghost cell instead of underfoot | canaries: place accepted, placed-on-a-wall refused, placed-out-of-reach refused, two props never share a cell. `ship@` re-blessed |
| **P-4** | the ghost + the lattice + the validity colour (`local_frame`) | live pass: the grid reads where you are pointing, the ghost turns red on a wall, and both ride the hull under burn. No hashed change — `ship@` unmoved |
| **P-5** | the 2×1 locker + `R` rotation, footprint-aware everywhere | rotating against a wall flips the ghost red before the press; a placed locker occupies both cells. `ship@` re-blessed |

P-0 and P-1 are the engine; P-2..P-5 are entirely `main.rhai` + `manifest.toml`
(two new actions: `place` on `MouseLeft`, `rotate` on `KeyR` — mouse buttons
already route through the same binding path as keys, `lib.rs:2083`).

## 5. Determinism and tests

- The two new primitives are render-side and local-layer-only, so they cannot
  desync anything: the simulation is not offered them (`host.rs:353` is the
  wall, and the registration split is how it is enforced).
- What *does* move the hash is P-2/P-3/P-5: a new command verb, new fields,
  and a new rule. Each re-blesses `ship@` — expected, and the plan says so up
  front, the same way `ship-physics.md §S-1/S-5` did.
- Map canaries go in `crates/monada-ship/tests/smoke.rs` against the headless
  `RhaiDriver`, which already drives crates through pick-up / set-down /
  airlock release. The aim command is just another `Command` in the schedule.
- Host canaries go beside the existing `map_render` tests (the clip probe and
  the vehicle-mirror raycast at `map_render.rs:4818` are the shape).
- Nothing here touches the physics digest.

## 6. Open questions

1. **Does a placed prop block the crew?** Today crates are pure decoration —
   `blocked()` knows walls and the airlock, nothing else, so you walk through
   a crate. A placement system whose output does not obstruct anything is a
   toy. Recommendation: **yes** — `blocked()` consults `occupied()`, one line,
   and the demo immediately has a reason to care where things go (barricades,
   blocked doorways, a corridor you have to clear). It is also the first
   moment the ship's collision predicate becomes about *content* rather than
   architecture. Cost: `ship@` moves, and a crate dropped under your own feet
   must be handled (place-at-aim already forbids the crew's own cell).
2. **Does the pick belong to a grid, or to the scene?** `pick_cell(grid)`
   asks about one named grid; `pick_grid()` says which was hit. The
   alternative — a single `pick_cell()` answering "whatever is under the
   cursor, in its own cells" — is friendlier for a one-grid map and ambiguous
   for a two-ship one. Proposed: keep both, as above.
3. **Should `pick_entity` become ray-based?** It measures world-`xy` distance
   to the ground hit, which is meaningless at attitude, so it is dead on this
   map. The placement slice does not need it (props live on cells, so "the
   crate under the cursor" is "the crate at the cursor's cell"), but a map
   that wants to click a *crew member* will. Out of scope here; worth its own
   line in the engine's gap list.
4. **Reach.** The existing 1.5 cells is arm's length. Placement at arm's
   length is fine for cargo and wrong for a builder. Left at 1.5 for P-3;
   revisit if the demo grows a build mode with its own reach.

## 7. What shipped

All six slices, in one pass. The design above is what was built; the
differences are worth writing down, because every one of them is a thing the
plan got wrong on paper and playing it (or testing it) corrected.

**Answered:** open question 1 is **yes** — `blocked()` consults `occupied()`,
so a stowed prop is as solid as the bulkhead beside it.

- **`gizmo_clear` exists, and gizmos are NOT cleared per frame.** The plan
  said the engine would clear them each frame; that is wrong for a map that
  draws on the tick clock, whose ghost would then flicker at whatever fraction
  of frames carry a tick. They follow the HUD canvas instead — retained until
  the map says otherwise — which is also the contract a map author already
  knows. `draw_gizmos` takes `&self` so a frame cannot eat them by accident.
- **`crew_at` tests the cell somebody is IN, not the cells their footprint
  grazes.** A crew member is 0.8 of a cell wide and therefore almost always
  overlaps a neighbour by a hair; the strict version refused the cell directly
  in front of you about half the time. The graze is left to ordinary
  collision, which blocks the step into a prop and nothing else.
- **Where the demo parks its cargo became level design.** Once props block,
  a crate stowed in a walking line is a wall: the spawn-side crate moved out
  of the row the stairwell walk uses, and the locker is stowed beside it
  rather than aft. Three canaries that walk those lines were the ones that
  said so.
- **The pick-up fallback stayed "nearest".** With no cursor at all (the
  oracle, a headless peer) `use` still takes the nearest prop within reach —
  the behaviour the demo shipped with — while set-down falls back to the cell
  the crew faces. Uniform "always the facing cell" read worse: it made a
  keyboard-only session unable to pick up what it was standing next to.
- **The aim is a per-tick command, cleared after the verbs that read it.**
  So "no cursor" needs no sentinel value: silence is the signal, and a peer
  without a window is not a special case anywhere in the map.
- **`ship@` moved once**, not three times — the slices landed together. Its
  schedule now takes the crate the cursor is on (tick 1), turns it (tick 30)
  and puts it down on the cell the cursor names (tick 32), and a new
  `the_ship_schedule_actually_places_the_crate` test asserts that it really
  happens: a refused placement hashes just as reproducibly as an accepted one,
  so the golden alone could not tell the difference.
- **Not done:** the two-cell locker is not in the golden's schedule (getting
  the crew to it would restructure the walk); the map canaries cover it. A
  filled, translucent ghost body is still not possible — a voxel is opaque —
  so the ghost is an alpha-blended wireframe, and the per-player ghost entity
  that would give it a solid body remains available with no engine work.

Verified: 22 map canaries, 4 new host canaries, the full workspace suite, and
a live CPU-backend run of the demo (`ROXLAP_GPU=0 cargo run -p monada-ship`)
that draws the lattice and the ghost every frame without raising.

## 8. Merging with axis-aligned orientation (`entity_set_side`)

The orientation PR landed beside this one and the two met on the same crate.
What the merge settled, since both are now the demo's:

- **A prop keeps ONE orientation.** The placement rotation was a separate
  `rot` field; it is gone. A prop's `dir`/`roll` (the `entity_set_side`
  discriminants) are the whole story, and the footprint reads the horizontal
  quarter-turn back out of `dir` (`prop_rot`). Two fields would have been two
  accounts of the same fact, and `entity_set_side` wins over
  `entity_set_facing` in the renderer — so a map that set both would have been
  writing the placement rotation into the void.
- **The deck owns the horizontal, the player owns the roll.** Setting a prop
  down faces it along the placement rotation and keeps whatever roll it was
  turned to in hand. Which cells a thing takes is the deck's business; which
  face is up is not.
- **Bit numbering:** `use` 1, airlock 2, burn 4, turn 8/16, `rotate` (R) 32,
  the debug spin/roll (1/2) 64/128. The edge mask is now `btn & (btn ^ prev)`
  — the orientation PR's spelling, which generalises to any bit instead of
  naming them in a magic constant.
- **`host_api` 26**: 24 is this plan's cursor + gizmos (already on master),
  25 `entity_set_side`, 26 `model_box_sides`.

Two things the merge turned up that were not about either feature:

- **Rhai's call-depth limit depends on the build profile** — 64 levels in
  release, **8 in debug**. The ship's collision path
  (`tick → step_crew → try_move → reachable → blocked → occupied →
  prop_covers → …`) sits right at it, and one extra helper made every
  movement test raise `Stack overflow` while a release build ran fine. A
  limit that changes what a script *means* between two builds of the same
  engine is a lockstep divergence waiting to happen, so both backends now set
  it explicitly (`set_call_depth`, 64 everywhere).
- **`occupied` did not ask which frame a prop is in.** A crate released
  through the airlock is detached into the WORLD frame, where its coordinates
  are no longer hull cells — so the ship carried an invisible obstacle at
  whatever deck cell those numbers happened to name. Fixed by testing
  `entity_grid(k) != hull_grid()`. **Not covered by a canary**: at rest the
  airlock's world coordinates land outside the walkable box, so a released
  crate's phantom cell is out of bounds anyway, and constructing a release
  whose numbers land *inside* it needs a turn plus a long burn — a test that
  would break on the next engine retune. The clause is the guard.
