# Plan: the ship demo — third-person view + per-crew visibility

Status: **design**. A new demo map (`crates/monada-ship`) that exercises
the roxlap 0.30 view/visibility features so the engine's HostBridge grows
the generic primitives an SS13-class game would later need. Genre lives
entirely in the map (mirrors chess and the RPG demo — see
DESIGN.md §3.2 and the `monada-rpg` precedent); the core only gains
neutral render-side seams. Nothing is built yet; this document is the
frame we stretch the skin onto.

Locked decisions (co-designed):

- **Visibility is render-side and per-client.** Each client's fog of war
  is driven by *its own* crew member (`local_player()`), lives on the
  host, and is **never hashed**. This is the SS13-correct model (every
  player sees their own line of sight) and keeps the lockstep hash small
  — the shared sim is authoritative about the world; what each player
  *sees* of it is local, exactly like the camera.
- **Slice 1 = geometry + camera + decks + a working fog of war.** Point
  lights and light-gated blindness are deferred to slice 2 (the API is
  designed here so slice 1's shape doesn't have to change for them).
- **Camera is over-the-shoulder third person.** roxlap's `ViewCutout`
  keyhole (walls between the camera and the crew member dissolve) is what
  makes an enclosed multi-compartment ship playable from behind the
  shoulder; the FoW facing cone reads naturally as "where the character
  looks."

This is neutral demo groundwork, not a titled game (same rule the RPG
demo notes): keep names "monada-ship" / "the ship demo".

## 0. Problem and core idea

A spaceship interior breaks two assumptions the chess/RPG demos never
tested:

1. **The camera is *inside* solid geometry.** An orbit camera behind a
   crew member in a corridor stares at the back of a wall. The RPG demo
   dodged this — its arena is open-topped. A ship needs the camera to see
   *into* the room the character stands in.
2. **You should not see the whole ship at once.** The point of a crew
   sim is that each player has partial knowledge — the compartment they
   are in, what they have seen before (dimmed from memory), and what they
   currently hear. That is a per-observer property, not a world property.

roxlap 0.30 (the QE series) shipped exactly the primitives for both, and
they are already wired through both render backends (verified: `cpu.rs`
and `gpu.rs` both read `FrameParams::fow`; `Grid::z_clip` and `LightRig`
exist). What is missing is the **monada HostBridge surface** to reach
them from a map script. Designing that surface is the whole point of this
demo.

### The sim / render split for a crew sim

The single most important design call. It extends monada's existing rule
(hashed sim state vs. render-side-only state, DESIGN.md §3.1):

| Layer | Owns | Hashed? |
|---|---|---|
| **Sim** (lockstep, shared) | crew positions & facing, which deck each is on, door open/closed, ship-system state, collision (`voxel_solid`), who-did-what | **yes** (Q32.32) |
| **Render** (per client) | camera + wall cutout, deck clip, **the entire fog of war** (vision cone / memory / hearing / sprite culling), point lights, tints, animation state | **no** |

Every visibility computation is a pure function of (a) the shared,
already-synced grid and (b) the *local* player's render pose — so it
produces per-client output from deterministic inputs without ever
entering the hash. If two peers disagree about what is revealed, that is
correct: they are different crew members.

## 1. New HostBridge surface

All render-side, all defaulted to no-op so headless/oracle bridges and
the chess/RPG maps are unaffected (the M4-S3 agnostic-host contract). rhai
registrations mirror the existing ones in `monada-script/src/rhai_backend.rs`.

### 1a. Camera & decks (slice 1)

```
/// Turn on the third-person wall cutout: geometry between the camera and
/// the current camera focus dissolves inside a screen-space keyhole of
/// `radius_cells` (feathered by `feather_cells`), so a crew member never
/// hides behind the wall the camera is looking through. Primary rays
/// only — cut walls still cast shadows, still block collision and vision
/// raycasts. Call with radius 0 to disable. Host builds a roxlap
/// `ViewCutout` from the focus set by `camera_focus` each frame.
fn camera_cutout(&mut self, radius_cells: Fixed, feather_cells: Fixed) {}

/// Show only the deck band `z_lo..=z_hi` (sim z) of the ship grid, cutting
/// the ceiling/upper deck away above it. Maps to roxlap `Grid::z_clip`
/// (translated sim-z → world-z). The map picks the band the local crew
/// member stands on. Render-side only.
fn deck_clip(&mut self, z_lo: i64, z_hi: i64) {}
```

Reuses the existing `camera_focus` / `camera_angle` / `camera_dist`. The
map's per-frame local layer: `camera_focus(hero_pos)`, `camera_cutout(4,
2)` once, `deck_clip(band_lo, band_hi)` when the hero changes deck.

### 1b. Vision / fog of war (slice 1)

The observer is *derived by the host* from a named entity — the map does
not push a mask, it just says "these are the eyes":

```
/// Declare the local viewpoint: the host maintains a fog of war against
/// the ship grid, updated every frame from this entity's cell, facing,
/// deck and eye height, and applies it to the render (dims unseen cells to
/// the "known twin" last-seen look, hides actors outside the live view).
/// Pass -1 to clear. Per-client, never hashed — pass the local crew
/// member (`local_player()`'s entity). Render-side only.
fn vision_observer(&mut self, entity: i64) {}

/// Tune the observer's vision: facing-cone half-angle (degrees), cone
/// reach and 360° peripheral reach (cells). Sets roxlap `VisionConfig`.
/// A map calls it once after declaring decks. Render-side only.
fn vision_config(&mut self, cone_deg: i64, range_cells: i64, peripheral_cells: i64) {}

/// Reveal a cell briefly from a heard sound (SS13 "you hear something" —
/// live data, memory styling). Call it where a noise happens; pairs with
/// play_sound. `loudness` 0..1. Render-side only.
fn vision_hear(&mut self, x: i64, y: i64, z: i64, loudness: Fixed) {}
```

Deck bands are shared between `deck_clip` and the FoW (roxlap keys FoW
layers by deck index). The host derives the band list from the sequence of
`deck_clip`/a dedicated `vision_decks([...])` call — resolved in §3.

### 1c. Lights & structure (slice 2, designed now)

```
/// Add a dynamic point light (roxlap `LightRig.points`); returns an id.
fn add_light(&mut self, x: i64, y: i64, z: i64, color: i64, range_cells: i64) -> i64 { -1 }
/// Toggle / recolor / move a light by id. Render-side only.
fn light_enable(&mut self, id: i64, on: bool) {}
/// Carve a single voxel back to air (open a door / breach a hull). The
/// collision store clears with it, so movers and vision pass through.
fn voxel_clear(&mut self, x: i64, y: i64, z: i64) {}
```

Light-gated vision (blind in the dark) is `VisionConfig.light_gate`, set
by a slice-2 flag on `vision_config`.

## 2. Ship geometry model

**One grid, two deck bands.** roxlap's `FogOfWar` is built around
`DeckBand`s over a single grid (`VisionConfig.decks`, `FowObserver.deck`,
`Grid::z_clip`), so the ship is one `Scene` grid the script paints at
`init`, not one grid per deck.

- Sim z is height-up (the RPG demo already unified `voxel_fill` height
  `z` → world `g - z`, z-up in sim). Two decks are two sim-z bands, e.g.
  **lower** `z ∈ 0..3`, **upper** `z ∈ 4..7`, with a solid floor slab at
  each band's bottom and a connecting stair/ladder cell.
- roxlap `DeckBand { z_top, z_bottom }` is grid-local **z-down**
  (`z_top` = ceiling = smallest world-z). The host owns the one place
  that maps sim-band → world `DeckBand`, next to the existing sim-z→world
  flip in `map_render.rs`.
- Compartments = wall voxels dividing a deck; a central corridor; doors
  are single cells toggled with `voxel_clear` / `voxel_set` (slice 2).
  Outside the hull is unpainted void → renders the starfield `set_sky`.

Everything a mover collides with is already covered by the existing
`voxel_fill` → `VoxelStore` → `voxel_solid`/`ground_height` path, so crew
movement + collision is free from the RPG demo.

## 3. Host implementation sketch

Lives in `monada-host/src/map_render.rs` (the `MapRender` that already
owns the roxlap `Scene`, sprites, actors, side-shades, sky).

- **FoW ownership.** `MapRender` gains an `Option<FogOfWar>` + a
  `KnownTwin` attached to the ship grid (roxlap `KnownTwin::attach`),
  plus the resolved `Vec<DeckBand>`. `vision_observer(entity)` records
  the entity id; `vision_config`/`vision_decks` build the `VisionConfig`.
- **Per-frame update** (in `render_into`, after actors are posed): read
  the observer entity's *render* pose (world cell, facing yaw → grid-local
  `facing`, deck index from its z, `eye_z`), build a `FowObserver`, call
  `fow.update(grid, &observer, dt)`, `known_twin.sync(scene, &fow)`, and
  set `frame.fow = Some((grid_id, &fow))`. Sprite culling of other crew is
  automatic once `frame.fow` is set (both backends honor `hides_sprite`
  via `FogSpriteCull`).
- **Cutout** (`camera_cutout`): stored as `Option<(radius, feather)>`;
  each frame, if set, `frame.view_cutout = Some(ViewCutout::new(focus))`
  with the radius/feather in logical px derived from cells × the frame's
  focal length (or just pass cells×k and tune).
- **Deck clip** (`deck_clip`): `scene.grid_mut(ship).z_clip =
  Some(world_z)`.
- **Per-client** falls out for free: the map calls `vision_observer`
  only for `local_player()`'s crew member, so each client's host builds
  its own FoW. Headless/oracle `NullBridge` keeps all of this no-op, so
  goldens never see it.

## 4. Slice-1 build order

Mirrors the RPG demo's M-A..M-E cadence; each step gets a headless test.

- **S-A — crate skeleton.** `crates/monada-ship` (Cargo.toml + build.rs
  packing `ship.monada` + `src/main.rs` launcher + `map/manifest.toml`
  with a `sim_hz`). Empty-ish `main.rhai` that paints a flat floor and
  spawns one crew member reusing the RPG billboard actor + WASD movement
  + voxel collision. Proves the crate builds and runs.
- **S-B — hull geometry.** `init` paints two decks, a few compartments, a
  corridor, a connecting stair. Starfield `set_sky`. Determinism test:
  crew walks a fixed path, collision keeps it inside the hull.
- **S-C — camera + decks.** Add `camera_cutout` + `deck_clip` bridge
  methods (host side) and wire them: over-the-shoulder follow, walls
  dissolve, only the hero's deck shows. (Render-side; no golden change.)
- **S-D — fog of war.** Add `vision_observer` / `vision_config` /
  `vision_hear`, the `MapRender` FoW + KnownTwin plumbing (§3). The hero's
  cone/peripheral reveals cells, memory dims what it leaves, other crew
  vanish outside the live view. Headless test asserts the FoW mask
  responds to observer moves (render-side, so it gates on the host, not
  the sim hash).
- **S-E — oracle golden `ship@`.** Bless `ship@{0,1,30,150}` in
  `monada-hashes.txt`, gating the *sim* half only (movement, collision,
  deck occupancy, doors). Vision/camera are render-side and invisible to
  the state hash — the same reason chess/RPG goldens survived the flat-lit
  and tint changes.

## 5. Determinism & tests

- Hashed sim: crew pose/deck, door state, any ship-system counters. The
  `ship@` golden gates these. FoW/camera/lights are render-side → excluded
  by construction (NullBridge no-ops them), so tuning vision never
  re-blesses.
- Reuse the RPG demo's headless harnesses: `TerrainBridge` for
  collision/voxel tests, `LoopbackTransport` for a 2-crew lockstep sync +
  replay test (co-op crew is the natural next step and the net stack is
  already map-agnostic).

## 6. Extension path toward SS13 (deferred)

The slice-1 API is chosen so these are additive, not reshapes:

- **Lights + light-gated vision** (slice 2): `add_light`/`light_enable`
  + `vision_config` light-gate flag → dark rooms blind you, a dropped
  flashlight lights a corridor.
- **Doors / airlocks**: `voxel_clear`/`voxel_set` toggling a cell as a
  sim entity; a breached hull cell exposes the void.
- **Hearing → gameplay**: `vision_hear` already carries loudness; wire it
  to `play_sound` sites so noise reveals.
- **Atmospherics / power / damage**: pure sim systems (hashed), rendered
  through the tint/light/HUD seams that already exist.
- **Multi-crew co-op**: `players > 1`, one `vision_observer` per client,
  QUIC lockstep (unchanged from chess/RPG).

## Open questions / risks

- **z is UNSCALED in the grid but SCALED for sprites/camera — the core
  verticality bug (BLOCKING, needs a coordinate decision).** `voxel_fill` /
  `voxel_set` place voxels at grid-z `GROUND_Z - sim_z` (**unscaled** in z;
  only x/y scale by `SCALE`), but `world_of` places sprites + the camera at
  `GROUND_Z - sim_z·SCALE` (**scaled**). The two systems coincide ONLY at
  `sim_z = 0` — which is why chess/RPG (everything on the flat floor) never hit
  it. The ship's UPPER deck does: a crew member seated at `sim_z = 4` renders
  (via `world_of`) at world-z 36, while its deck-floor voxels sit at grid-z 96
  → the sprite floats ~60 units off its own deck. `deck_clip` is now correct in
  the grid's OWN coords (`deck_clip_world_z(z_hi) = GROUND_Z - z_hi`, unscaled,
  unit-tested end-to-end against a real grid + raycast), so the lower-deck
  cutaway works — but the upper-deck render is broken until the coordinate
  systems are reconciled. Options: **(A)** scale voxel z in `voxel_fill` too
  (consistent-scaled; breaks RPG/chess z tuning — pillars 12→192 tall — and
  needs their re-tune + a live pass); **(B)** leave both, and give the ship two
  SEPARATE grids (one per deck) offset in world-z instead of stacking bands in
  one grid; **(C)** UNSCALE `world_of`'s z to match the grid (`GROUND_Z -
  sim_z`) — chess/RPG are all at `sim_z = 0` so unaffected, and the ship's crew
  then sits on its deck; the ship geometry just needs taller sim-z gaps to read
  (walls/decks are only ~1 unit/cell tall unscaled). Recommend **C** (smallest,
  existing-demo-safe), but it's an engine-author call and every option needs a
  real-display pass. Until resolved, the ship's vertical render + the ViewCutout
  `z_bias`/`margin` (world units, currently assuming the SCALEd world) are
  unverified. S-D's FoW deck path (`FowObserver.eye_z`, `DeckBand`) lives in the
  same grid-z and must follow whichever choice lands.
  **RESOLVED (2026-07-19): chose (C).** `world_of`'s z is now UNSCALED
  (`GROUND_Z - sim_z`), matching the grid; chess/RPG (all at sim-z 0) are
  byte-unaffected, and the ship's crew now seats on its deck. Consequence: the
  ship geometry was re-tuned taller (unscaled z means 1 unit/layer, so a storey
  needs a tall span to clear the ~22-unit crew sprite) — `DECK_STRIDE = 28`,
  walls ~24 tall, upper deck floor at sim-z 28 (`deck_floor`/`deck_top`
  helpers; the wall predicate is xy-only so heights never touch collision).
  ViewCutout `z_bias` dropped to ~1 unit (unscaled). Still needs a real-display
  pass for the visual feel (wall heights, cutout radius/z_bias, camera
  pitch/dist), but the coordinate systems now agree.
- **Deck-relative collision (S-B, done) + a real engine gap.** S-A's
  `blocked()` read a fixed wall layer (`voxel_solid(cx, cy, 1)`); S-B threads
  the mover's deck through `blocked`/`clear`/`try_move` and seats on the
  deck's floor. But the deeper finding: monada's collision store
  (`VoxelStore` in monada-script) is a **single heightmap** — a column is
  solid from its floor up to its top voxel — so it **cannot represent air
  below a floor**. Painting the upper deck's plate makes `voxel_solid` true
  across the whole lower deck. So S-B collides against a **script-side wall
  predicate** instead of `voxel_solid`, kept in sync with what `build_hull`
  paints. roxlap already models decks (`DeckBand`, `FowObserver.eye_z`); the
  gap is on monada's side. **Engine TODO (pre-SS13):** a multi-deck / voxel-
  set collision primitive — e.g. per-deck heightmaps, or a sparse solid-voxel
  set — so `voxel_solid(x, y, z)` answers truthfully with stacked floors.
  Doors (`voxel_clear`) and the FoW deck index will want the same model.
- **GPU FoW residency.** `gpu.rs` warns if `FrameParams::fow` names a
  grid not resident on the GPU. Confirm the ship grid registers before the
  first FoW frame (it should — it is the scene's only grid); otherwise
  force CPU (`ROXLAP_GPU=0`) for the demo until confirmed.
- **Cutout tuning for over-the-shoulder.** `ViewCutout::new` is tuned for
  a top-down keyhole (radius 110 px, feather 24, `z_bias` +0.5 cutting
  below the focus). Behind-the-shoulder wants a focus near the chest and a
  positive `z_bias` (~chest-to-feet) so lintels cut but the floor stays —
  expose radius/feather (and maybe z_bias) as `camera_cutout` args to tune
  on a real display (can't verify headless here).
- **Where facing comes from.** The FoW cone needs the crew member's look
  direction. Reuse the RPG's `entity_set_facing` yaw (aim/move facing) as
  the `FowObserver.facing` source — one render-side value feeds both the
  sprite pick and the vision cone.
```
