# Plan: the RTS demo — terrain levels, unit orders, shared pathfinding

Status: **design**. A new demo map (`crates/monada-rts`) — a Warcraft-III-class
RTS prototype two players can actually play over the existing QUIC lockstep.
Genre lives entirely in the map (the chess / RPG / ship rule); the core gains
one substantial *sim-side* system — a generic deterministic navigation service
(`monada-nav`) — plus a handful of neutral render-side seams. Navigation is to
this demo what visibility was to the ship demo: the engineering payload the
genre exists to force out of us.

Locked decisions (co-designed):

- **Pathfinding is a host-side deterministic system, not rhai A\*.** A pure
  `monada-nav` crate (grid A\*, zero deps) driven from the same voxel heightmap
  the sim already collides against, exposed to scripts as `nav_path` /
  `nav_block`. This is the piece the user explicitly wants extractable as a
  generic Rust system.
- **Selection is local-layer only, never hashed.** The shared sim only ever
  sees explicit unit-addressed commands (`MOVE unit → dest`). Lockstep already
  carries `Vec<Command>` per player per tick (`lockstep.rs:26` —
  `BTreeMap<u64, BTreeMap<PlayerId, Vec<Command>>>`), so a group order is just
  N commands in one tick. No net-layer change.
- **Terrain is the existing VoxelStore heightmap.** Walkability is a pure
  function of `ground_height` deltas plus explicit nav blockers. No new
  terrain representation; the single-heightmap limitation that hurt the ship
  (no air under floors) is exactly right for an RTS (no overhangs).
- **Camera is WC3-style**: fixed steep pitch via `camera_angle`, keyboard pan
  of `camera_focus`, host mouse-wheel zoom (already generic,
  `monada-host/src/lib.rs:1827`).

Neutral naming as always: "monada-rts" / "the RTS demo". WC3 is the reference
genre, not the title.

## 0. Problem and core idea

What a WC3-style map needs, versus what the engine has after chess/RPG/ship:

| Ingredient | Have today | Gap |
|---|---|---|
| Tile terrain | `tile`/`tile_fill`, `transition`+`terrain_blit` autotiles | autotile path is flat-floor-only; heightfield paints per-column `tile_fill` (crisp voxel steps — accepted, on-brand) |
| In-level height + cliffs | `VoxelStore` heightmap, `ground_height(x,y)` | no walkability rule; define `|Δh| ≤ MAX_STEP` |
| Destructible trees | entities + voxels | needs `voxel_clear` (designed in ship plan §1c, unbuilt) + nav unblock |
| Gold mine / hall / workers / barracks | pure script over entities+commands | none — script work |
| Click select | `pick_entity`, `highlight` | `highlight` is single (`Option<EntityId>`, map_render.rs) — need a set |
| Box select | `action_down` on a MouseLeft-bound action already reports held state (`dispatch_input` forwards press *and* release); local layer already has world reads (`local_backend.rs:79`) | only a way to *draw* the drag rectangle |
| Group orders | `submit_command`, multi-command ticks | none — works as-is |
| Pathfinding | nothing | **the new system** |
| Ground picking | `pick_ground` intersects the z=0 plane (`map_render.rs:703`) | must raycast the real heightfield |

### The sim / render split for an RTS

| Layer | Owns | Hashed? |
|---|---|---|
| **Sim** (lockstep, shared) | unit/building/tree positions & hp, gold, orders & destinations, production queues, nav blockers, mine reserves, winner, RNG | **yes** (Q32.32) |
| **Render / local** (per client) | selection set, drag rectangle, camera, highlight markers, team tint, HUD, sounds | **no** |

Selection is per-client for the same reason the ship's fog of war is: it is
what *this* player is doing with the mouse, not a property of the world. The
moment it becomes a world effect, it has already been flattened into explicit
per-unit commands that cross the wire.

## 1. New engine surface

### 1a. Navigation (sim-side — the demo's engineering payload)

Registered next to `voxel_solid`/`ground_height` in the deterministic sim API:

```
/// Deterministic grid path between cell centers. Walk rule: 8-neighborhood,
/// |ground_height(a) − ground_height(b)| ≤ max_step, both cells unblocked;
/// diagonals additionally require both orthogonal neighbors passable (no
/// corner cutting past a cliff edge or tree). Returns waypoint cell centers
/// as Vec3 (sim coords, z = ground), [] if unreachable. Node budget caps the
/// search; on exhaustion returns the best partial path toward the closest
/// reached cell (WC3 behavior: the unit walks as far as it can).
fn nav_path(x0, y0, x1, y1, max_step) -> [Vec3]

/// Explicitly block/unblock a cell for navigation (building footprints,
/// trees). Deterministic sim state: same command stream ⇒ same blocker set
/// on every peer — the same argument that already covers voxel_fill.
fn nav_block(x, y, on: bool)
```

A\* determinism is by construction: integer octile costs (10/14), fixed
neighbor visit order, a monotone insertion counter as the heap tie-break.
No floats, no hash maps, no iteration-order luck.

Implementation: new crate **`monada-nav`** — pure grid A\* over a
`NavCost`-style trait (height lookup + blocked lookup), zero engine deps,
property-tested in isolation (determinism, corner-cut ban, budget partials,
unreachable). `monada-script` owns the adapter over `VoxelStore` + a blocker
bitset and registers the rhai functions, mirroring how `voxel_solid` works —
so headless bridges (`TerrainBridge`, oracle) get identical paths for free.

Units do **not** nav-block (they'd churn the grid and gridlock chokepoints);
unit-vs-unit overlap is handled by the RPG's separation push plus
repath-when-stuck (§3). Prototype-grade and honest about it.

### 1b. Ground picking on a heightfield (host fix, render-side)

`pick_ground` today intersects the z=0 plane. Upgrade: unproject with
roxlap `view_ray`, `Scene::raycast` against the voxel scene, map the hit back
to sim coords through the existing world↔sim seam (mirror-X, `GROUND_Z − z`);
keep the plane intersection as the miss fallback. Chess/RPG (everything at
sim-z 0, hits are floor-voxel tops) are behaviorally unchanged; the RTS gets
correct picks on plateaus and ramps.

### 1c. Selection & orders (local layer + render-side)

- **Multi-highlight.** `highlight_add(entity)` accumulates into a set
  (`BTreeSet<EntityId>` in `MapRender`, marker sprite per member);
  `highlight_clear()` unchanged; `highlighted()` keeps returning one id (-1 /
  first) for chess compatibility.
- **Box select needs no new input API.** Manifest declares
  `select = ["MouseLeft"]`, `order = ["MouseRight"]`; the local layer polls
  `action_down("select")` per `local_tick` (press/release both dispatch —
  `lib.rs:1843`), records the `pick_ground()` point at the press edge as the
  drag anchor, and on release collects owned units whose (x, y) lie inside
  the ground-space rectangle — entity reads are already registered in the
  local backend.
- **Drag visual.** One render-side call:

```
/// Draw a ground-space selection rectangle between two sim points (host
/// renders the outline via roxlap draw_lines, hugging terrain height).
/// Call with a == b to hide. Render-side, per-client, never hashed.
fn select_rect(a: Vec3, b: Vec3)
```

  A ground rectangle (not a screen rect) is chosen deliberately: it is the
  region that actually selects, so the feedback is WYSIWYG under the tilted
  camera, and it needs no cursor-in-screen-points API.

### 1d. Destructible world (sim-side)

`voxel_clear(x, y, z)` from the ship plan §1c gets its first consumer: a
felled tree despawns, `nav_block(x, y, false)`, and clears its painted trunk
voxel so both collision and render agree the cell is open.

## 2. Terrain model

One grid, 96×96 cells, sim z unscaled (1 unit per z — the post-ship "option C"
coordinate rule). Numbers below are starting tunes, not law.

- **Three height levels**: lowland z=0, plateau z=12, high ground z=24.
  `LEVEL_STRIDE = 12` against the ~22-unit-tall unit sprite reads as a real
  cliff. **In-level relief**: ±1..2-unit bumps, walkable. `MAX_STEP = 2`, so
  bumps pass and cliffs (Δ12) wall off — walkability falls out of the
  heightmap with no separate cliff markup.
- **Ramps** are just terrain: a 6-cell run climbing 2 per cell connects
  levels, automatically walkable under the same rule, automatically the
  chokepoint (WC3's whole ramp meta, for free).
- **Paint**: per-column `tile_fill` at init — grass-variant top tile
  (gen_tiles style), dirt beneath, rock on cliff columns; `side_shades`
  darkens faces so steps read. The `transition`/`terrain_blit` autotile path
  is flat-floor-only and is simply not used here.
- **Layout**: 180°-rotationally symmetric. A start plateau in each opposite
  corner (town hall, 5 workers, gold mine); ramps down into a contested
  lowland; tree lines shaping two attack lanes; an optional unclaimed
  expansion mine mid-map.
- **Trees**: sim entity (hp) + kv6 sprite + `nav_block` on its cell (+ a
  painted trunk voxel). Death: despawn, unblock, `voxel_clear`.
- **Buildings**: rectangular footprints (hall 4×4, barracks 3×3, mine 3×3)
  nav-blocked at spawn, rendered as `model_box` volumes first (kv6 art
  later). Pre-placed at init; worker construction is an extension (§7).

## 3. Game model (all in `main.rhai`)

- **Archetypes**: `unit` (owner, kind, hp, order, order_target, dest_x/y,
  carry, cooldown), `building` (owner, kind, hp, queue, progress), `mine`
  (gold), `game` (gold per player, supply per player, winner).
- **Command verbs**: `MOVE(unit, dest)`, `ATTACK(unit, target)`,
  `HARVEST(unit, mine)`, `STOP(unit)`, `TRAIN(building, kind)`. One command
  per affected unit; a 12-unit group order is 12 commands in that tick's
  bundle. `on_command` validates ownership (chess-style gating) — a peer
  cannot order units it doesn't own.
- **Worker loop**: path to mine → mine N ticks → path to hall → deposit +10
  gold → repeat until the mine runs dry; any explicit order interrupts.
- **Combat**: soldiers auto-acquire enemies in aggro range, melee on
  cooldown; trees are attackable. `winner` set when a town hall dies; HUD
  `status` announces it.
- **Production**: `TRAIN` checks and deducts gold, enqueues; progress ticks;
  spawn at the first nav-reachable cell ringing the building. Supply cap
  ~20/player — authentic *and* the rhai per-tick budget guard.
- **Path storage**: the authoritative destination lives in hashed entity
  fields; the waypoint array is script-local (map entity→array), recomputed
  deterministically on order receipt or when stuck K ticks — the chess
  script-local `int[64]` precedent: derived state, same command stream ⇒
  same value on every peer.
- **Controls**: LMB click/drag select; RMB context order by what's under the
  cursor (enemy → ATTACK, mine → HARVEST, ground → MOVE); arrows/WASD pan
  `camera_focus` (units are mouse-driven, so keys are free for the camera);
  wheel zoom is already host-side; Q/hotkeys or `ui_button` for TRAIN.

Manifest sketch:

```toml
players = 2
sim_hz = "30hz"

[[action]] id = "select"  kind = "button" default = ["MouseLeft"]
[[action]] id = "order"   kind = "button" default = ["MouseRight"]
[[action]] id = "pan"     kind = "axis2"  default = { up="KeyW", down="KeyS", left="KeyA", right="KeyD" }
[[action]] id = "train1"  kind = "button" default = ["KeyQ"]   # worker / soldier
```

## 4. Host implementation sketch

- **`monada-nav`** (new crate): `astar(&impl NavCost, from, to, max_step,
  budget) -> Vec<(i32, i32)>` + the walk-rule helpers. No engine types.
- **`monada-script`**: blocker bitset stored beside `VoxelStore`; the
  `NavCost` adapter reads `ground_height` + blockers; `nav_path`/`nav_block`
  registered in the sim API (`rhai_backend.rs`), shared by every bridge the
  way `voxel_solid` already is. *Noted smell (also hit by the ship's deck
  collision): determinism-critical world services living bridge-side. Fine
  for a second consumer; a third should trigger the "runtime-owned world
  state" refactor — flagged in §Open questions.*
- **`monada-host/map_render.rs`**: `highlighted` → `BTreeSet`; `select_rect`
  stored as `Option<(Vec3, Vec3)>`, drawn each frame as four `Line3` edges
  following `ground_height` along the rectangle; `pick_ground` re-based on
  `view_ray` + `Scene::raycast` with the plane fallback.
- **Assets** (dev-time gen examples, committed output — the established
  pattern): `gen_units.rs` — worker + soldier 8-direction GIF billboards in
  two team palettes; reuse `gen_tiles.rs` grass/dirt/rock; `gen_trees.rs` —
  a couple of kv6 trees; buildings stay `model_box` until R-E polish.

Team color note: `entity_set_tint` is currently a damage-flash seam — verify
in R-A whether a persistent per-entity tint holds across frames; if not, two
pre-tinted sprite sets from `gen_units.rs` are the zero-API fallback.

## 5. Slice build order

Mirrors the RPG/ship cadence; every slice lands with a headless test.

- **R-A — skeleton + click-move.** Crate boilerplate (copy ship: build.rs,
  launcher, manifest). Flat 96×96 grass, WC3 camera (fixed pitch, key pan,
  wheel zoom), one worker per player, click-select via existing single
  `highlight`, RMB straight-line move (no nav yet). Proves the loop, input,
  and team-tint question.
- **R-B — nav + real terrain.** `monada-nav` crate with its unit/property
  tests; levels + ramps + bumps painted; `nav_path`/`nav_block` registered;
  `pick_ground` raycast fix; RMB orders path via nav; trees placed as
  blockers. Headless: a unit routed around a cliff takes the ramp,
  hash-stable across runs.
- **R-C — selection & group orders.** `highlight_add` + `select_rect` host
  work; local-layer drag FSM; group MOVE as N commands per tick; ownership
  gating. Headless: loopback two-peer burst of group orders, no desync.
- **R-D — economy.** Mine/hall entities, worker harvest FSM, gold HUD
  (`ui_text`), TRAIN worker (hotkey + `ui_button`), supply cap. Headless:
  scripted input mines an exact gold total at an exact tick.
- **R-E — combat + barracks.** TRAIN soldier, ATTACK + auto-acquire, tree
  felling (`voxel_clear` + unblock), win condition, sounds. Headless: a
  scripted skirmish resolves to the same hp/winner every run.
- **R-F — LAN + oracle.** `players = 2` over the existing lockstep (free);
  `rts@{0,1,30,150,600}` golden with a deterministic input schedule
  (harvest → train → attack → hall falls), loopback replay test, bless
  `monada-hashes.txt`.

## 6. Determinism & tests

- Everything strategic is hashed sim; selection/camera/rect/highlight are
  render-side no-ops in `NullBridge`/`TerrainBridge`, so UI tuning never
  re-blesses goldens — the same invariant that carried chess and the RPG
  through render churn.
- Nav determinism is triple-guarded: `monada-nav` property tests, the `rts@`
  golden, and the lockstep hash check every 30 ticks in the loopback test.
- Perf envelope: A\* on 96×96 is ≤ ~9k nodes worst-case in Rust — noise. Nav
  runs on order receipt / stuck-repath only, never per unit per tick. The
  real watch item is the rhai tick over ~40 units × FSM; the supply cap
  bounds it. If script ticking becomes the wall, that is a *finding* (the
  engine wants batched per-archetype systems), to be reported — not silently
  papered over by shrinking the demo.

## 7. Extension path (deferred)

- **Fog of war, RTS flavor**: per-client reveal by *owned units* — the ship's
  `vision_observer` generalized to a set of observers; roxlap's FoW mask is
  already per-grid, the seam grows `vision_observer_add`.
- **Minimap**: egui texture widget fed by a host-rendered top-down thumbnail.
- **Control groups** (Ctrl+1..9): pure local-layer state.
- **Worker-built buildings**: footprint ghost via `select_rect`-style
  preview, nav_block on placement, build progress — no new seams.
- **Lumber**: trees gain a `wood` field, workers a chop FSM — script only.
- **Attack-move / patrol / formations**: script verbs over the same nav.
- **Flow fields**: if armies outgrow per-unit A\*, `monada-nav` grows a flow
  field API — the reason it's a separate crate.

## Open questions / risks

- **Bridge-owned determinism state (2nd offender).** Nav blockers beside
  `VoxelStore` repeat the pattern the ship's deck collision already strained.
  Two consumers is tolerable; before a third (atmospherics, build ghosts…),
  world services should move into the script runtime proper and bridges
  become pure render sinks. Track it; don't fix it inside this demo.
- **`entity_set_tint` persistence** for team color — verify in R-A; fallback
  is pre-tinted sprite sets (zero engine work).
- **`pick_entity` radius vs. packed units** — `PICK_RADIUS` was tuned for
  chess pieces; dense unit blobs may want nearest-bias or a smaller radius.
  Tune in R-C.
- **Crowding at shared destinations.** N units ordered to one point all
  target the same cell; separation push resolves it ugly-but-stable. Cheap
  fix if it grates: ring-offset destinations around the click point in the
  local layer before command submission.
- **Init paint volume**: ~9.2k `tile_fill` column calls in rhai at init.
  Almost certainly fine (one-time); measure in R-B and only then consider a
  bulk terrain host helper.
- **Multiple commands per player per tick** is native to lockstep, but no
  demo has stressed 10–20-command bursts through the QUIC bundling — R-C's
  loopback test exists precisely to catch ordering or size assumptions.
