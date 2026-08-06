# Plan: the desert game — a volumetric Dune II-class RTS on monada

Status: **design**. Co-designed decisions in §1 are locked; everything else
is a proposal to argue with before D-0 starts.

This is the first monada title that is **not a demo**. Chess, the RPG, the
ship, the digger and the RTS each existed to force one engine seam out of
hiding and were done when the seam shipped. This one exists to be *played to
completion* — a full campaign plus skirmish, at the content density of
Westwood's Dune II (1992), on a map that is genuinely three-dimensional.

The design premise, in one sentence: **Dune II assumed a flat world because
1992 had no other option, and every one of its systems — base layout, worm
danger, unit counters, faction identity — changes when the desert gains a
third axis.** This project takes that premise seriously rather than porting
the original into voxels as decoration. The factions are built *out of* what
a voxel landscape can do (§6), which is also why the content is original
rather than a reskin.

Neutral naming as always: Dune II is the reference genre, not the title.
Working crate names are `monada-desert` (game binary + map archive) and
`monada-desert-rules` (the rules library).

## 0. Why this is a good stress test

| Dimension | Best demo to date | This game |
|---|---|---|
| Sim entities | ~40 (RTS, supply 12/side) | 150–400 units + ~60 buildings + projectiles |
| World | 48×48 column heightmap (RTS) / 160×160×45 volume (digger) | **256×256×64 volume** — 4× the digger, permanently edited by gameplay |
| Terrain mutation | drilling one bore (digger) | three factions whose *core mechanic* is reshaping terrain, continuously, on both sides |
| Navigation | flat 8-neighbour A\* on a heightmap | layered 3D stand graph over a volume store, with tunnels |
| Rules size | 1141 lines of Rhai (RTS) | ~10–15k lines of game logic |
| Session | a 600-tick golden | 30–60 minute missions, 27-mission campaign |
| Persistence | replays | replays **and** saved games |
| Shell | none — one map, one match | menu, faction select, region map, briefing, score |
| Opponent | the other human | 1–2 scripted factions + neutral wildlife |

Three engine payloads fall out, any one of which would have justified a demo
of its own: **a second script runtime** (§3), **three-dimensional
navigation** (§4c), and **granular terrain that settles** (§4d).

## 1. Locked decisions (co-designed)

| # | Decision | Consequence |
|---|---|---|
| L1 | **Rules are Rust, not Rhai** — a second `ScriptBackend` running a compiled rules crate natively, the same source later compiled to wasm | DESIGN.md §5.5's "the runtime is swappable" claim finally gets a second implementation. Rhai stays the runtime for every existing demo |
| L2 | **Clean-room content only** — no importer, no dependency on the original game's files | All art, audio, missions and text are ours. §6's factions make this a design asset rather than a chore |
| L3 | **Skirmish vertical slice first, campaign second** | Engine risk (volume scale, 3D nav, terraforming throughput, perf) surfaces by D-9; shell and missions layer onto a proven core |
| L4 | **True 3D voxel with a free-orbit camera** — oriented voxel vehicles, independently turning turrets | Forces `add_sprite_instance_posed` through to the script API; model silhouette, not sprite art, is the visual identity |
| L5 | **The map is volumetric, not a heightmap** — `terrain = "volume"`, overhangs, tunnels and undercuts are first-class | Kills `nav_path` (column-only today): 3D navigation becomes a from-scratch subsystem. Enables L6 |
| L6 | **Factions are terrain verbs** — each faction's identity is one way of manipulating the voxel landscape: **Surflings** build up (additive), **Dwellers** dig down (subtractive), **Binders** convert material in place (transmutative) | The reason this is not a Dune II reskin. The three verbs close a set: shape added, shape removed, shape left alone but meaning changed. Detailed in §6 |
| L7 | Runtime host context lives in a new **`monada-runtime`** crate, not in `monada-script` | The wasm backend needs it too; naming a shared crate after one runtime was the wrong home |
| L8 | **The AI plays under fog**, unlike the original's omniscient one | Scouting becomes real, and AI-vs-AI headless runs become a meaningful test of the shroud |

## 2. Gap analysis

| Ingredient | Have today | Gap |
|---|---|---|
| Lockstep, fixed-point, replays, desync hashes | yes | none — a campaign is one input stream; LAN skirmish nearly free |
| Volume terrain, hashed, editable | yes (digger: chunked 16³ `u16` material store, `voxel_fill`/`voxel_clear`, `phys_solid`) | scale (4×), and a bulk generation path |
| Pathfinding | `monada-nav` A\* over the **column** store only | **3D layered navigation** — the project's second payload (§4c) |
| Terrain that settles / collapses | nothing | **granular settle pass** (§4d) — the third payload |
| Units with facing | billboard actors, `.rkc` characters | freely-oriented KV6 models (roxlap has the posed-instance path; monada exposes none) |
| Fog of war | roxlap `FogOfWar`: one observer, cone, LOS — but **deck bands already exist** (ship demo) | N-observer permanent reveal, two decks (surface / underground) |
| Camera slicing into terrain | `deck_clip`, `camera_cutout` (ship demo) | wiring to selection depth; no new engine concept |
| Explosions, dust, rubble | nothing exposed | roxlap ships `ParticleSystem` + `DebrisSystem`; monada bridges neither |
| Rigid-body wrecks, collapses | `monada-physics` (digger) | a live-body cap and spawn policy, not new mechanics |
| Drilling for deep resources | `phys_drill` (digger) | reuse as-is for deep spice veins |
| Per-cell gameplay data | hashed state is entities + voxels | dissolved by L1 (rules own typed state), not by a new subsystem |
| Radar, sidebar, model icons | `ui_*` immediate mode | canvas texture, model-rendered icons, panels, hover, clipped lists |
| Saved games | replays only | snapshot/restore of the whole hashed state |
| Game shell | one map = one match | a rules-side state machine; no new engine concept |
| Editor | `monada-editor` is a 10-line skeleton | **off this project's path** — content is procedural + data files (§9) |

## 3. The runtime: Rust rules

### 3a. `monada-runtime` — one definition of the host surface (L7)

Today the ~90 host functions exist exactly once, as closures registered into
a Rhai `Engine`, each holding the logic that touches `SharedWorld`, the
terrain store and the `HostBridge`. There is no callable surface underneath
them, so a second backend has nothing to call.

The refactor lifts that logic into a **host context** described by traits in
a new crate that neither runtime owns:

```rust
// monada-runtime — depends on monada-fixed, monada-sim, monada-nav, monada-physics
pub trait WorldApi {
    fn archetype(&mut self, fields: &[&str]) -> ArchetypeId;
    fn entity_create(&mut self, a: ArchetypeId) -> EntityId;
    fn entity_set_position(&mut self, e: EntityId, p: FixedVec3);
    fn entities_of(&self, a: ArchetypeId) -> &[EntityId];
    fn rng_below(&mut self, n: i64) -> i64;
    // …
}
pub trait TerrainApi {                       // volume semantics (L5)
    fn solid(&self, c: Cell) -> bool;
    fn material(&self, c: Cell) -> MaterialId;
    fn fill(&mut self, lo: Cell, hi: Cell, mat: MaterialId, color: u32);
    fn carve(&mut self, lo: Cell, hi: Cell);
    fn surface_z(&self, x: i32, y: i32) -> i32; // topmost solid — the cheap query
}
pub trait NavApi   { fn path(&mut self, from: Cell, to: Cell, p: &MoverProfile) -> &[Cell]; }
pub trait RenderApi { /* models, orientation, grids, camera, deck clip, light */ }
pub trait HudApi    { /* text, images, canvas, buttons, icons, clip */ }
pub trait AudioApi  { /* one-shots, loops, music */ }
pub trait FxApi     { /* emitters, bursts, debris */ }

pub trait Host: WorldApi + TerrainApi + NavApi + RenderApi + AudioApi + FxApi {}
pub trait LocalHost: WorldApi + TerrainApi + RenderApi + HudApi + AudioApi + FxApi {
    fn action_down(&self, id: &str) -> bool;
    fn pick_ground(&self) -> Option<FixedVec3>;
    fn submit_command(&mut self, verb: u32, target: EntityId, arg: FixedVec3);
}
```

`monada-script` becomes a thin registration layer over `monada-runtime`
(each Rhai function is one call into the context); `monada-desert-rules`
calls the same traits directly. The sub-trait split is not cosmetic: it is
how the local layer is *prevented* from mutating the world — today that
guarantee is a convention (a different function list per scope), here it is a
type.

### 3b. `MapRules` — what a Rust map implements

```rust
pub trait MapRules: 'static {
    fn init(&mut self, h: &mut dyn Host);
    fn command(&mut self, h: &mut dyn Host, player: PlayerId, cmd: Command);
    fn tick(&mut self, h: &mut dyn Host);
    fn snapshot(&self) -> Vec<u8>;
    fn restore(&mut self, bytes: &[u8]);
}
pub trait LocalRules: 'static {
    fn local_tick(&mut self, h: &mut dyn LocalHost);
    fn action(&mut self, h: &mut dyn LocalHost, id: &str, down: bool);
}
```

`NativeBackend` holds a `Box<dyn MapRules>` and implements the existing
`ScriptBackend`, so the lockstep session, the oracle and the host loop drive
it exactly as they drive `RhaiBackend`. The manifest gains
`script_runtime = "native"`.

### 3c. Rules-owned hashed state — the reason this is worth doing

Under Rhai, script functions are pure and hold nothing: all mutable state
lives in the World as named fixed-point entity fields. That is why the RTS
demo stores a path as "destination + next waypoint" and re-plans per cell,
and why a per-tile spice field has no home short of a new engine subsystem.

A Rust rules object holds typed state directly:

```rust
pub struct Desert {
    spice: SpiceField,               // surface tiles + deep veins
    factions: [Faction; 4],          // credits, power, queues, tech, terraform budget
    units: BTreeMap<EntityId, Unit>, // typed per-unit state, retained paths included
    tunnels: TunnelNet,              // Dweller graph: shafts, bores, collapse state
    ai: [AiBrain; 4],
    worms: WormSet,
}
```

**The contract that makes that legal:** everything reachable from the rules
object is hashed simulation state. `snapshot()` returns canonical bytes
(postcard over a `Serialize` tree with no `HashMap`, no floats, no pointers)
and the driver folds those bytes into `state_hash` beside `World` and the
terrain digest. Rules state and world state become indistinguishable to
desync detection, replays and saves.

Discipline, enforced the way `monada-sim` enforces it on itself:
`#![deny(clippy::float_arithmetic, clippy::disallowed_types)]` in the rules
crate; dependencies limited to `monada-runtime` + `monada-fixed` + `serde`;
no `std::time`, no `rand`, no threads, no I/O; the oracle runs the CI
platform matrix.

This is strictly weaker than Rhai's `no_float` wall, where the runtime
*cannot* express nondeterminism. It is the price of L1, the honest argument
for finishing the wasm backend, and it is in the risk register (§13).

Entities do not disappear under this scheme. An entity stays the shared
identity of "a thing at a position with a render model" — what `pick_entity`,
`highlight`, the render mirror and the fog observer speak. Rules keep typed
state *beside* the entity and mirror position (only position) into the World
when it changes: one write per moving unit per tick, the traffic the RTS demo
already generates.

### 3d. Snapshot and restore — saved games fall out

```
save = { engine_version, rules_hash, map_hash, tick, world, terrain, rng, rules_bytes }
```

`World`, the RNG and the volume store already derive serde (the volume store
even serialises its cached per-chunk hashes, so restore→hash is bit-equal by
construction). Two engine items follow:

1. `Sim::snapshot() -> Vec<u8>` / `Sim::restore(&[u8])` on the driver.
2. An oracle test worth more than the feature: run N ticks, snapshot, restore
   into a fresh process, run M more, assert the state hash equals an
   uninterrupted N+M run. That catches every piece of hidden derived state in
   the engine — including, deliberately, the nav cache of §4c and any settle
   bookkeeping of §4d.

### 3e. Replays under a native backend — the honest trade-off

A replay is `(seed, inputs, engine version, map hash)`. With Rhai the rules
live in the archive, so `map_hash` pins them. With native rules the logic
lives in the binary, so a replay is only valid against the build that made
it. Mitigation until wasm lands: `build.rs` emits a `RULES_HASH` over the
rules crate's source tree, the replay header records it, and `Replay::verify`
refuses a mismatch loudly instead of desyncing quietly.

### 3f. The wasm step

Nothing above is native-specific except linkage. The wasm backend is: flatten
`Host` to an ABI (scalars plus a shared linear-memory buffer for arrays and
strings), compile the rules crate to `wasm32-unknown-unknown`, load with
wasmtime; snapshot becomes a linear-memory dump, which is structurally
deterministic. The game does not change; the packaging does. Slice D-12.

## 4. The volumetric desert

### 4a. Scale and coordinates

| Quantity | Value | Rationale |
|---|---|---|
| Gameplay tile | 4×4 sim cells | The rules reason in tiles (a building is 2×2 or 3×2 tiles, a unit occupies one); 4 cells per tile is enough resolution for trench walls, berm slopes and bore mouths |
| Map | 64×64 tiles = **256×256 cells** | Dune II's map size at 4× linear terrain resolution |
| Vertical | bedrock `z=0`, mean sand surface `z≈32`, sky to `z=64` | Room for ~30 cells of digging below and ~30 of building above — both factions get real headroom |
| Store cost | ~4.2M cells → ~1000 dense 16³ chunks ≈ 8 MB | The digger runs 1.15M cells today; chunk hashes are cached and folded, so the per-tick hash stays cheap |
| `sim_hz` | 30 | Matches the RTS demo and the digger; native rules make tick cost a non-issue |
| Players | 1 (campaign) / 2–4 (skirmish) | AI factions are not players — the rules simulate them on every peer |

Render mirroring is span-based (voxlap lineage), so a solid desert costs
little on screen: a filled column is one span, not 32 voxels. Terrain
generation emits box and column fills, never per-cell writes (the digger's
established pattern).

### 4b. Materials

The volume store is already material-tagged (`u16` per cell, `phys_material`
registers density/friction/restitution/hardness). The desert's palette:

| Material | Behaviour |
|---|---|
| Loose sand | slumps (§4d), worms swim through it, digs fast, cannot bear heavy structures |
| Packed fill | Surfling product: stable, worm-proof, bears structures, digs slowly |
| Rock | native bedrock and outcrops: stable, buildable, slow to bore |
| Glass / sinter | Binder product: fast movement surface, worm-proof, **brittle** — shatters under heavy vehicles or impact |
| Slag | Binder weapon product: destabilised fill/rock that slumps like sand |
| Spice crust | the harvestable surface layer |
| Deep vein | spice at depth, drill-only |

Material is the whole faction-interaction language of §6: the three factions
mostly do not shoot at each other's terrain, they *convert* it.

### 4c. Three-dimensional navigation (payload #2)

`monada-nav` today implements `NavWorld` for the column store only; on a
volume map `voxel_solid` reads an empty world by design. The new subsystem,
kept in `monada-nav` as a pure crate with no engine dependency:

```rust
/// A place a mover can stand: a solid cell with clear headroom above it.
pub struct Stand { pub z: i32, pub headroom: u8, pub material: MaterialId }

pub struct MoverProfile {
    pub height: u8,        // cells of clearance needed (infantry 2, tank 3, harvester 4)
    pub max_step: i8,      // climbable delta — the walk rule, per class
    pub max_slope_run: u8, // consecutive climbs before it counts as a wall
    pub tunnels: bool,     // may use bores below the surface
}

pub struct NavVolume { /* per-column stand lists + dirty set keyed on chunk version */ }
impl NavVolume {
    pub fn path(&mut self, w: &impl VolumeWorld, from: Cell, to: Cell,
                p: &MoverProfile, budget: u32) -> Vec<Cell>;
    pub fn invalidate(&mut self, lo: Cell, hi: Cell);
}
```

- **Stand extraction.** For each column, walk it once and record every solid
  cell with ≥ `height` clear cells above. A surface column yields one stand; a
  column pierced by a bore yields two or three. This is what makes tunnels
  first-class instead of a special case.
- **Edges.** 8-neighbourhood between stands with `|Δz| ≤ max_step` and
  sufficient headroom; diagonals require both orthogonals passable (the
  existing corner-cut ban); vertical shafts connect stacked stands where a
  ramp or lift exists.
- **Determinism** by construction, as today: integer octile costs, fixed
  neighbour visit order, monotone insertion counter as heap tie-break, no
  hash maps.
- **Incrementality is mandatory here, not optional.** Terraforming edits
  terrain constantly; stand lists are cached per column and invalidated by
  the volume store's chunk version, so a berm invalidates ~64 columns rather
  than the map.
- **Scale.** 65k columns × ~1.1 stands is a ~72k-node graph — the same order
  as the RTS demo's flat grid, so per-query cost is unchanged. What changes is
  query *count*: 150–400 units. Retained paths (a Rust rules object can hold
  the waypoint array Rhai could not) plus a node budget carry D-2; a coarse
  portal graph over 8×8-tile blocks is the designed escape hatch if D-9
  profiling says A\* dominates.

Property tests mirror the existing crate's: determinism, corner-cut ban,
budget partials, unreachable goals, plus new ones — a mover never routes
through a bore too small for its profile, and a collapsed tunnel invalidates
exactly the affected columns.

### 4d. Granular terrain (payload #3)

Loose sand must not stand in vertical walls: a trench erodes, a berm slumps,
an explosion craters and the crater's rim settles. Without this, terraforming
is just voxel Lego and the factions of §6 have no cost or counterplay.

Proposal: a deterministic **settle pass** in `monada-physics`, beside the
existing voxel materials, rather than in the rules — it is generic granular
behaviour (any future map wants gravel and snow), it needs the volume store's
dirty tracking, and keeping it engine-side means the same hashed result on
every peer without the rules crate reimplementing an automaton.

```
phys_granular(mat, repose, rate)   // per-material angle of repose + cells/tick
```

- Cells of a granular material whose supporting neighbourhood is thinner than
  `repose` allows move one cell downhill, in a canonically ordered sweep over
  a **dirty set** (cells edited this tick plus their neighbourhood), with a
  per-tick budget. Non-granular materials never move.
- The pass is hashed state, so a slump is identical on every peer, and it is
  bounded: quiet terrain costs nothing, an active battlefield costs a fixed
  ceiling.
- Fallback if the engine-side pass proves contentious: the same automaton
  lives in the rules crate over `TerrainApi`, at the cost of one host call per
  candidate cell. Decide at D-3 with numbers, not taste.

### 4e. Terraform work as a resource

Terrain edits are not free actions: every faction verb (fill, bore, vitrify)
is performed by a *unit* at a bounded cells-per-second rate, costing power or
spice. This is simultaneously the game's pacing knob and the engine's safety
valve — it caps voxel edits per tick, which caps render re-uploads, nav
invalidation and settle-pass work. One number, three problems.

### 4f. Decks: shroud and camera

roxlap's `VisionConfig::for_decks` already models z-bands (the ship demo's
decks), which maps onto this game exactly:

- **Deck 1 — surface.** Revealed by units and buildings, radius-based, no
  LOS occlusion, **permanent** once explored (Dune II has no re-fog).
- **Deck 0 — underground.** Revealed only where you have presence: your own
  bores, a unit inside one, a seismic sensor. An enemy tunnel under explored
  ground stays invisible — which is what makes Dwellers frightening and
  scouting for spoil heaps meaningful.

Script surface stays small:

```
shroud_reveal(deck, x, y, radius)     // per-client, per-frame, idempotent
shroud_visible(deck, x, y) -> bool    // local reads, for the radar
```

Implementation is a D-7 spike between (a) teaching roxlap's `FogOfWar` an
N-observer permanent-reveal stamp — its `hear()` blob stamp is nearly the
shape already — and (b) a **black lid**: opaque voxels above unexplored
columns, carved on reveal. (b) needs no roxlap change, occludes correctly
under a rotating camera, hides sprites geometrically, and costs one edit per
cell *once per match*.

Camera: free orbit (yaw free, pitch 35–70°, zoom from base-fills-screen to
two-tiles-fill-screen), plus `deck_clip` driven by selection depth — select a
unit underground and the surface slab lifts away — and `camera_cutout` as the
keyhole for peering into a trench without leaving the surface view. Both
verbs already exist and were proven by the ship demo.

## 5. New engine surface

| Area | Proposed script surface | Notes |
|---|---|---|
| Oriented models | `entity_set_facing(e, yaw)` extended to `model_kv6`; `entity_set_orient(e, yaw, pitch, roll)` | A tank is hull + turret as two entities at one position — no new parenting concept. Ornithopters and worm segments need pitch/roll |
| 3D nav | `nav_path3(from, to, profile) -> [Cell]`, `nav_profile(height, max_step, tunnels) -> id` | §4c |
| Granular terrain | `phys_granular(mat, repose, rate)` | §4d |
| FX | `fx_def(...) -> id`, `fx_burst(id, pos, n)`, `fx_emit(id, pos, on)`, `fx_debris(pos, radius, color)` | Bridges roxlap's existing `ParticleSystem` / `DebrisSystem`; render-side, unhashed |
| Shroud | `shroud_reveal(deck, x, y, r)`, `shroud_visible(deck, x, y)` | §4f |
| HUD | `ui_canvas(id, w, h, pixels)`, `ui_model_icon(model, yaw, pitch)`, `ui_rect`, `ui_cursor()`, `ui_clip`/`ui_clip_end` | `ui_model_icon` deletes an entire icon-art task by rendering the shipped models |
| Volume store | an **incremental (XOR) chunk hash** so a single-voxel write is O(1), plus a batched `carve(lo, hi)` | Not a script verb but a store fix the spike (§13a) proved is on the D-3 critical path: per-voxel writes currently re-hash 8 KB each |
| Data | `asset_bytes(path) -> bytes` | Mission and balance tables; the same call the wasm build will make |
| Saves | `save_write(slot, bytes)`, `save_read(slot)` | The one sanctioned persistent I/O, host-mediated, map-namespaced |
| Snapshot | `Sim::snapshot()` / `restore()` (host-side, not script) | §3d |

## 6. The factions — a war of terrain verbs (L6)

Classic Dune II factions differ in a unit roster and a superweapon. Here they
differ in **what they can do to the desert**, which changes base layout,
army composition, map control and the worm relationship all at once.

### 6a. Surflings — additive

They ride the surface and refuse to be dictated to by it. Sand is fixed into
**packed fill**: berms, ramps, causeways across soft dune seas, and platform
foundations driven into the sand so a refinery can stand where nothing should.

- **Verb:** create elevation. Fill is manufactured from spice + power, so it
  costs economy but conserves nothing — they can add net material.
- **Structures:** platform-first construction. A pad is terraformed, then
  built on. Buildings on packed fill are stable; on raw sand they degrade
  (the design descendant of Dune II's concrete rule, now literal).
- **Military identity:** height is range and sight. A gun on a 6-cell berm
  out-ranges and out-sees the same gun on the flat; their doctrine is to
  build the firing position before the fight and make the enemy climb.
- **Signature works:** the rampart (a walled shooting gallery), the causeway
  (a fast road across sand that is also worm-proof), the shield curtain (a
  powered force wall that stops ground movement and direct fire until its
  generator dies).
- **Weakness:** everything is slow, expensive and immobile once built. A
  Surfling that has to relocate has thrown away its investment. Binders
  (§6c) can turn their fill back into something that flows.

### 6b. Dwellers — subtractive

They do not fight the desert's surface; they go under it. Trenches, bores,
shafts, spoil heaps, and a tunnel network that the enemy cannot see and can
barely reach.

- **Verb:** move elevation. Mass is conserved: a trench here is a spoil heap
  there. Digging costs only time and power, never spice — but they cannot
  create net material, and their spoil is *evidence*.
- **Structures:** buried. A factory in a pit is immune to direct fire and
  visible only by its surface vents — which are the enemy's target.
- **Military identity:** cover and surprise. Units in trenches are hittable
  only by arcing fire or from adjacent cells; tunnels move an army under a
  Surfling rampart and out behind it. Bores are navigable terrain (§4c), not
  a scripted teleport, so a tunnel can be found, occupied, or collapsed.
- **Signature works:** the bore (a 3×3 tunnel with shaft mouths), the trench
  line, the sand-blaster (redistributes a dune: subtract here, deposit
  there — their only way to build up).
- **Weakness:** loose spoil attracts worms, tunnels can be collapsed by
  bombardment or by a worm passing through, and a trench network is useless
  the moment the enemy owns the ground above it.

### 6c. Binders — transmutative

The third faction does not reshape the desert, it **changes what the desert
is made of**. Sand vitrified into glass plate. Enemy packed fill sintered
into slag that promptly slumps. Rock destabilised until the foundations
standing on it fail.

- **Verb:** convert material in place. Cheap in mass, expensive in power, and
  effective *against the other two factions' work* rather than against
  terrain in general.
- **Military identity:** denial and reversal. Glass plate is a fast road and
  a worm-proof corridor, but brittle: heavy vehicles crack it, and a cracked
  plate over a Dweller bore drops whatever stands on it into the tunnel.
- **Signature works:** the glass road, the sinter beam (Surfling rampart →
  slag → slump), the crust field (hardened dune that worms will not enter).
- **Weakness:** they build nothing of their own; without an enemy's works to
  subvert, they are the plainest of the three, and their glass is a liability
  under their own heavy units.

The identity is locked; the *name* is not (§14). *Binders* follows the
plain-word pattern Surflings/Dwellers set and names the verb — they bind the
desert into something else. Alternatives still on the table: Kilnborn,
Sinters, Glasswrights.

### 6d. The rock-paper-scissors of matter

| | vs Surflings | vs Dwellers | vs Binders |
|---|---|---|---|
| **Surflings** | — | fill a trench, bury a bore mouth, deny the surface | ramparts out-range, but slag undoes them |
| **Dwellers** | tunnel under any wall ever built | — | glass over a bore is a trapdoor waiting to open |
| **Binders** | sinter fill → slag → slump | crust denies the loose sand they need to dig fast | — |

No matchup is symmetric, and none is decided by unit stats alone.

### 6e. The worms are the third player

Worms swim through **loose sand** within a depth band and are drawn to
sustained ground vibration — moving units, and *especially* digging. They
cannot enter packed fill, glass, crust or rock.

This makes the neutral force interact with all three verbs at once: Surfling
causeways and Binder crust are worm-proof lanes bought with economy; Dweller
digging is a dinner bell, and a worm crossing a shallow bore collapses it;
luring a worm into an enemy harvest field is a legitimate strategy for
everyone. The worm is not decoration on the map — it is the reason the map's
material composition is a strategic question.

Rendering: a segment chain of oriented KV6 models following a spline through
the sand, with `fx_burst` geysers on breach and a temporary raised ridge in
its wake (which then settles per §4d). This is the set-piece that no
billboard sprite could do, and the clearest argument for L4.

## 7. Game model

Everything here lives in `monada-desert-rules`; the engine knows none of it.

- **Economy.** Spice only. Surface crust is harvested classically; **deep
  veins** require drilling (the digger's `phys_drill`, reused) and are the
  reason Dwellers' economy scales differently. Harvester → refinery →
  credits, silo caps, losses on overflow. Spice does not regrow; blooms are
  the only new source, so a mission is a finite resource race.
- **Power.** Wind traps (Surflings), geothermal shafts (Dwellers), kiln
  reactors (Binders) — the same scalar with different placement constraints
  (surface exposure / depth / adjacency to rock). Power scales build speed,
  gates radar, and pays for terraforming (§4e), which makes it the tightest
  resource in the game.
- **Buildings.** Yard, refinery, silo, radar, barracks, light/heavy/air
  factory, research lab, repair bay, starport, palace, turrets, walls — plus
  a faction terraformer building each. Placement rules are 3D: a footprint
  needs a stand with clearance and bearing material (§4b), and the classic
  adjacency rule survives.
- **Units.** Infantry (light, rocket, elite, saboteur), scouts, quads,
  harvester, MCV, main tank, siege tank, missile launcher, faction heavy,
  plus the faction terraform unit (fill-caster / bore-head / vitrifier), air
  (carryall, ornithopter, delivery frigate).
- **Combat.** Projectiles as entities; direct fire is blocked by terrain
  (a volume raycast — cheap against the chunked store) so berms and trenches
  genuinely matter; arcing weapons are the counter and the reason artillery
  exists; damage is a `(weapon class × armour class)` table; splash edits
  terrain (craters that then settle); wrecks spawn a physics body when the
  live-body budget allows (cap ~24) and otherwise a static hulk.
- **Air.** Flies at fixed clearance above `surface_z`, ignores nav, needs
  pitch/roll to look right; carryalls ferry harvesters between distant fields
  and retrieve damaged units.
- **Superweapons.** Palace-gated, one per faction, all ordinary rules code:
  a ballistic missile with scatter, an allied desert-fighter squad, a
  saboteur that destroys any one structure it reaches.
- **AI (L8).** A deterministic `AiBrain` per non-human faction, staggered
  across ticks, **playing under fog**: it scouts, remembers what it saw, and
  reasons over stale knowledge. Build-order table, base-layout planner aware
  of stands and materials, harvester management with worm avoidance,
  attack-group accumulation, defence reaction, plus faction-specific
  terraform doctrine (Surflings pre-build firing positions; Dwellers tunnel
  toward the enemy economy; Binders hunt for enemy works to subvert).
  Difficulty is a profile: decision cadence, group thresholds, income
  multiplier, terraform aggression.
- **Controls.** LMB select / drag-select, RMB context order, sidebar build and
  placement, edge/WASD pan, wheel zoom, Q/E camera yaw, a depth slider bound
  to `deck_clip`, Ctrl+1..9 control groups (pure local state).

## 8. Presentation

- **Models.** Buildings are static KV6 with 2–3 damage states; vehicles are
  hull + turret pairs; infantry stays billboard actors (small on screen, and
  cheap); worms are segment chains.
- **Faction colour.** `entity_set_tint` if it persists across frames (the RTS
  demo flagged this as a damage-flash seam); otherwise per-faction palette
  variants from the generator — the zero-API fallback.
- **Terrain reads as material.** Loose sand grainy and pale, packed fill
  banded and flat-topped, glass near-specular and dark, slag mottled, spice
  crust rust-orange. A player must be able to read the strategic state of the
  map from colour alone at max zoom-out.
- **Radar.** A `ui_canvas` written by the local layer: one pixel per tile,
  material colour + spice tint + owner blips + black where unexplored, with a
  viewport rectangle and a second underground layer toggled with the depth
  slider. Degrades to noise when power is short.
- **Audio.** Synthesised blips first (`play_blip` exists), then authored
  one-shots; a faction theme each and an advisor line set.

## 9. Content pipeline (clean-room)

- **Voxel art:** a `gen_models.rs` in the established `gen_*.rs` pattern
  builds every building and vehicle from parameterised primitives, emitting
  committed `.kv6`. ~40 recognisable silhouettes for a few hundred lines of
  generator, deterministic and diffable; hand-authored replacement (MagicaVoxel
  / demiurg `.rkc`) is an optional per-model upgrade.
- **Maps are generated, not drawn:** a seed plus a parameter block (rock
  coverage, ridge count, spice fields, dune amplitude, symmetry) feeds a
  fixed-point noise generator that lays bedrock, rock outcrops, dune seas,
  spice fields and start locations, honouring the walk-rule invariants. A
  hand-authored override file pins specific features. Skirmish gets infinite
  maps for free, and the campaign's 27 missions become parameter blocks
  rather than 27 hand-drawn levels.
- **Missions:** one TOML each — terrain block, starting forces, AI profile,
  objectives, briefing and debrief text.
- **Text:** ours, written to the same beats; the advisor is a voxel head
  rendered through the icon path or a pre-rendered GIF loop.

## 10. Campaign shell and saves

The shell is not an engine concept — it is a rules state machine:

```
Menu → FactionSelect → RegionMap → Briefing → Mission → Score → RegionMap …
```

Entering `Mission` despawns everything, regenerates terrain from the mission
block and spawns the starting forces. Because transitions are driven by
*commands*, a whole campaign playthrough is one replay file — a genuinely
novel thing to be able to say about a campaign game.

Saves use §3d's snapshot into three slots. Single-player for now; a
multiplayer save needs every peer to snapshot at the same tick (mechanically
easy, socially annoying) and is deferred.

## 11. Slice build order

Every slice lands with a headless test, and from D-1 with a `desert@{…}`
golden in `monada-hashes.txt`.

- **D-0 — the second runtime.** `monada-runtime`; host context lifted out of
  `rhai_backend.rs`; `NativeBackend`; rules-state hashing; `Sim::snapshot`/
  `restore`. *Gate:* every existing demo's goldens byte-identical after the
  refactor; chess ported to Rust rules as a second implementation; snapshot →
  restore → resume equals an uninterrupted run.
- **D-1 — the desert exists.** `monada-desert`; volume terrain generator;
  free-orbit camera + `deck_clip` depth slider; oriented KV6 models; one
  vehicle driving over dunes. *Gate:* **already measured and passed** by the
  spike (§13a) — 14.1 ms for the map plus 400 posed instances on the GPU,
  L4's free yaw costing ~20 % on the CPU path, mip-0 confirmed as the right
  pin. What D-1 owes is the same scene through monada's own render path
  (sim→render mirror included), which is the number the spike deliberately
  excludes.
- **D-2 — 3D navigation.** `monada-nav` stand graph, mover profiles,
  incremental invalidation, retained paths. *Gate:* property tests; a unit
  routes over a berm and through a bore; 200 concurrent movers inside budget.
- **D-3 — terraforming and settling.** The store's incremental chunk hash +
  batched carve (§13a) first, then the three verbs as terrain edits, the
  granular pass (§4d) and the terraform work budget (§4e). *Gate:* a berm, a
  trench and a crater settle to the same hashed state on every platform; a
  3000-cell terraform tick stays under 1 ms in the store (it costs 21 ms
  today) and inside the render and nav budgets.
- **D-4 — economy.** MCV deploy, refinery, harvester loop, surface crust
  depletion, deep veins via drill, silos, power. *Gate:* a scripted schedule
  mines an exact credit total at an exact tick.
- **D-5 — base building.** Sidebar (panels, model icons, hover, clipped
  list), 3D placement rules, buried and elevated structures, repair.
  *Gate:* a headless build order produces an exact base on both a plateau
  and in a pit.
- **D-6 — combat.** Units, turrets, projectiles, terrain-blocked direct fire,
  arcing weapons, cover, splash craters, wrecks; the FX bridge. *Gate:* a
  scripted skirmish resolves identically every run.
- **D-7 — shroud and radar.** Two-deck reveal (spike both implementations),
  radar canvas, sensor structures. *Gate:* shroud is frame-rate-independent
  and a no-op headless.
- **D-8 — worms and air.** Worm behaviour, material aversion, breach
  set-piece, tunnel collapse; carryall, ornithopter, starport frigate.
  *Gate:* a worm eats a scripted harvester at an exact tick and refuses a
  packed-fill causeway.
- **D-9 — AI, factions, skirmish. First playable milestone.** All three
  factions' verbs and doctrines, fog-limited AI, skirmish setup, 2–4 player
  LAN, victory conditions. *Gate:* AI vs AI runs 20 minutes headless without
  desync or deadlock; a 400-unit late game holds the frame budget.
- **D-10 — campaign shell.** Screens, region map, briefings, score,
  save/load, superweapons, 27 mission blocks, naming and text pass. *Gate:*
  a full campaign completes from one replay; a mid-mission save restores
  bit-exactly.
- **D-11 — content and balance.** Art, audio, balance, difficulty, a
  map-maker's book chapter on volume maps and 3D nav.
- **D-12 — wasm rules.** Flatten the ABI, compile the rules crate to wasm32,
  load via wasmtime, browser build. *Gate:* identical goldens under both
  backends — the real proof of DESIGN.md §5.5.

D-0 through D-9 answers "does monada support this at all". D-10 onward
answers "is it a game".

## 12. Determinism and tests

- Rules state, world state and volume terrain fold into one combined
  `state_hash`; the lockstep exchange is unchanged.
- Per-slice headless tests use `NullBridge`/`TerrainBridge`, so HUD, shroud,
  radar, FX and camera work never re-blesses a golden.
- New test classes this project introduces:
  1. **snapshot equivalence** (§3d) — the strongest determinism test in the
     repo once it exists, and the one that keeps the nav cache and settle
     bookkeeping honest;
  2. **backend equivalence** (D-0, D-12) — one map, two runtimes, one state;
  3. **long-run stability** — a 20-minute AI-vs-AI headless match nightly,
     catching the slow-accumulation class of bug that 600-tick goldens cannot;
  4. **terrain invariants** — generated maps satisfy the walk rule (every
     start can reach every surface spice field with a tank profile), and the
     settle pass is idempotent on quiet terrain.

## 13. Perf budget and risks

### 13a. Measured before building — the D-1/D-3 spike

`cargo run --release -p monada-host --example desert_perf_spike` builds the
256×256×64 desert for real and measures both halves of the wall. It exists
because L4 and L5 could each invalidate the D-0 refactor, and a day of
measurement is cheaper than finding out at D-3. All numbers are one
developer machine at 1280×720: the CPU tables are medians of 12 headless
frames, the GPU table comes from `--gpu` (windowed, since roxlap's
`SceneRenderer` needs a surface).

**Render — terrain is the cost, orientation is not.**

| Case | Frame |
|---|---|
| Terrain only, 48×48×32 (RTS-demo scale) | 31.0 ms |
| Terrain only, 96×96×32 | 43.3 ms |
| Terrain only, 256×256×64 (this game) | 61.6 ms |
| 256×256×64 at 1920×1080 / 1280×720 / 960×540 / 640×360 | 137.6 / 63.5 / 36.7 / **17.6** ms |
| + 60 vehicles in view | 64.7 ms (sprites 5.4) |
| + 120 vehicles in view | 70.3 ms (sprites 12.4) |
| + 400 vehicles alive | 80.9 ms (sprites 20.6) |
| + 400 vehicles, **axis-aligned** | 77.1 ms (sprites 17.2) |
| Strategic zoom, 800 vehicles alive | 58.9 ms (sprites 14.5) |

**Render — the GPU backend, which is what actually ships.**

400 posed instances on the full map, 1280×720, RTX 3070 Laptop (NVK /
Vulkan). "Pipelined" is 240 back-to-back `render`+`present` frames divided by
the count (what a real loop sees); "drained" adds `wait_idle` per frame (a
hard upper bound). A short burst measures nothing — `render` only records
commands and reads as 0.0 ms.

| Configuration | Tactical | Strategic |
|---|---|---|
| `gpu_mip_scan_dist: 8192` (monada today — mip-0 everywhere) | **14.06** / 13.68 ms | 12.16 / 11.75 ms |
| `gpu_mip_scan_dist: 64` (roxlap's LOD default) | 14.44 / 13.83 ms | 11.15 / 10.59 ms |

Four conclusions:

1. **L4 is cheap.** Free yaw costs ~20 % over axis-aligned sprites
   (17.2 → 20.6 ms), not the 2× that would have forced billboards. A
   battle's worth of oriented vehicles is 12–20 ms of *sprite* time, and at
   strategic zoom 800 of them cost 18 µs each. Oriented voxel vehicles are
   affordable; this decision is settled.
2. **The CPU raycaster is not the target — and never was.** It costs 31 ms
   at 720p on an *RTS-demo-sized* map, so every existing demo has been
   running on the GPU backend (`PreferGpu` is the host default). The
   desert's 2× over that is real but not the deciding term. What the CPU
   path *is* good for is a fallback: 640×360 lands at 17.6 ms, and cost is
   linear in pixels, so resolution is the fallback's honest knob.
3. **D-1's render gate passes, without much room.** 14.1 ms tactical /
   12.2 ms strategic on the GPU with 400 oriented vehicles — inside a 60 fps
   frame, but the sim→render mirror, HUD, shroud and the game's own tick
   still have to fit beside it. Treat 14 ms as the *terrain-and-army floor*
   and re-measure at D-6 and D-9 rather than assuming headroom.
4. **The mip-0 pin stays — and that is a risk retired, not deferred.**
   `monada-host` pins the GPU to mip-0 everywhere
   (`gpu_mip_scan_dist: 8192.0`), justified in-code by "monada's scenes are
   small; revisit if a map ever needs LOD". Measured on the map that was
   supposed to need it, LOD buys nothing (14.44 vs 14.06 tactical — it is
   marginally *slower*; 11.15 vs 12.16 strategic). That matters beyond
   perf: coarse mip chunks do not apply a grid's `z_clip`, so turning LOD on
   would have punched holes in the deck cutaway — which is the Dwellers'
   primary viewing mode (§4f). Staying at mip-0 keeps the cutaway intact for
   free.

**Sim — the hash is free, the terraform path is not.**

| Case | Cost |
|---|---|
| `VolumeStore` build, 2.2M solid cells | 1.21 s (mission load) |
| Store size | ≈ 4 MB of dense 16³ chunks |
| `state_hash`, quiet **and** after edits | **0.02 ms** per desync tick |
| Carve 1536 cells via `clear` (one voxel per call) | 10.8 ms — **7.01 µs/cell** |
| Raise 1536 cells via `fill`, one call per column | 2.3 ms — 1.50 µs/cell |
| Raise the same 1536 cells in **one** `fill` | 0.10 ms — **0.07 µs/cell** |
| Stand scan, whole map (per-cell `get`) | 56.8 ms (init only) |
| Stand scan, 64-column patch | **0.06 ms** — the per-edit nav invalidation |

The 100× spread across three ways of writing the same 1536 cells is the
spike's real find: **`VolumeStore::set`/`clear` re-hash the entire 16³ chunk
(8 KB of FNV) on every single-voxel write**, while `fill` batches one rehash
per dirty chunk per call. The cost is the rehash, not the write. For the
digger — one bounded drill sweep per tick — that was invisible. For a game
whose three factions all terraform continuously it is the difference between
a 3000-cell tick costing 21 ms of a 33 ms budget and costing 0.2 ms.

Two engine items follow, both now on the D-3 critical path:

- **An incremental chunk hash.** Fold each cell as an order-independent
  `h(index, value)` XOR into the chunk digest, so a single-voxel write is
  O(1) instead of O(4096). Canonical form is preserved (XOR of per-cell
  hashes is deterministic and order-free), and every existing golden must
  be re-blessed exactly once.
- **A batched carve verb.** `voxel_clear` hole-punches one cell; a bore, a
  trench or a crater is a box. The store wants `carve(lo, hi)` with `fill`'s
  dirty-set discipline.

Everything else on the sim side is comfortable: the desync hash is free at
this scale, incremental stand extraction (§4c) costs 0.06 ms per edited
patch, and the 4 MB store is a non-issue for snapshots. The full-map stand
scan wants a chunk-local walk rather than per-cell `get`, but it runs once
per mission load.

### 13b. Risks

| Risk | Checkpoint | Mitigation |
|---|---|---|
| Volume store at 4× the digger: memory, hash | **measured (§13a) — clear.** 4 MB, hash 0.02 ms/tick | Nothing owed; snapshots are cheap at this size |
| Terrain render cost at 256×256×64 | **measured (§13a) — passes on GPU.** 14.1 ms tactical with 400 posed vehicles; 62 ms on the CPU raycaster at 720p (31 ms even at RTS-demo scale) | The GPU backend is the target, as it already is for every demo; mip-0 stays pinned (LOD buys nothing and would break the deck cutaway). CPU fallback lives at 640×360. Re-measure at D-6 and D-9 — the 2.6 ms of headroom is where HUD, shroud and the sim mirror have to fit |
| Terraforming churn — edits per tick blowing up hash, render re-uploads, nav invalidation, settle work | **measured (§13a) — the store needs work.** 7.01 µs/cell for per-voxel writes vs 0.07 for a batched box | Incremental chunk hash + a batched carve verb (§13a), then the terraform work budget (§4e) caps the rest |
| 3D nav cost at 400 movers | D-2, D-9 (patch invalidation already measured at 0.06 ms) | Retained paths, node budget, incremental stands; coarse portal graph as the designed escape hatch |
| roxlap sprite path at 400 posed instances | **measured (§13a) — L4 is cheap.** Free yaw costs ~20 % over axis-aligned | Shroud culling (`fow_cull`), model voxel budget, infantry stays billboards |
| Granular pass is a new determinism surface inside the engine | D-3 | Canonical sweep order over a dirty set, per-tick budget, snapshot-equivalence test, rules-side fallback |
| Native rules can express nondeterminism Rhai structurally could not | continuous | Lint wall, restricted dependencies, oracle matrix, nightly long-run; wasm (D-12) restores the structural guarantee |
| Replay validity pinned to a binary, not a map hash | D-0 | `RULES_HASH` in the replay header; loud refusal, not silent desync |
| The `rhai_backend.rs` → `monada-runtime` refactor breaks a demo subtly | D-0 | Byte-identical goldens are the gate, not a nice-to-have |
| Three faction verbs is three times the design surface of one | D-9 | D-9 ships them together deliberately: an asymmetry that is only balanced against itself is not balanced |
| Scope — thirteen slices with a content tail | continuous | D-9 is a complete, playable, LAN-capable skirmish game. If the campaign never lands, the project still shipped something worth playing |

## 14. Open questions

1. **Naming.** The three faction *identities* are locked (§6). The third
   faction's name (*Binders* proposed), the shipped title and unit
   nomenclature are owed before D-10's text pass. Two identities that lost —
   a foundation-less nomad faction and a spice-symbiote faction that farms
   worms — remain the obvious raw material for a fourth faction or an
   expansion, so they are recorded here rather than discarded.
2. **Where the granular pass lives** (§4d) — `monada-physics` or the rules
   crate. Decide at D-3 with measurements.
3. **Underground radar.** A second radar layer is proposed; whether
   underground presence should be visible on it at all, or only through
   seismic structures, is a gameplay call worth prototyping at D-7.
4. **Multiplayer saves.** Deferred at D-10.
5. **Does the RTS demo migrate to native rules?** Current preference: no —
   it stays the living proof that the Rhai path still works.
6. **Clean-room audio** is the one content axis with no procedural shortcut
   identified. Owed before D-11.
