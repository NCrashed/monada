# monada — design doc

> **Status:** Living design doc, v0.1. Locked decisions are summarized
> in §10.1; open questions in §10.2. Sections marked *post-v0* describe
> committed architecture direction whose implementation comes after
> the M4 chess demo ships.

## Audience and scope

This document is the architectural reference for monada — the entry
point for new contributors and the source of truth for cross-crate
contracts. It states *what* the engine is and *why* its choices are
what they are; per-crate `README.md`s and dedicated subsystem docs
(e.g. `VOXVIDEO.md`) carry the *how*.

## 1. Vision

**monada** is a Rust game engine for deterministic, lockstep-networked
games in the spirit of late-90s / early-00s strategy classics — Age of
Empires II, Warcraft III, StarCraft. Voxel rendering via
[`roxlap`](https://crates.io/crates/roxlap-core) gives it a CPU-rendered, retro look without GPU
shader pipelines. Scripted "maps" — not hardcoded gameplay — are the
unit of distribution: monada is a *runtime* the way WC3's engine was a
runtime for `.w3x` scenarios and JASS triggers.

Three non-negotiable design pillars:

1. **Determinism first.** Every simulation tick is bit-identical on
   every machine, given identical inputs. This is the foundation for
   lockstep networking, replays, and reproducible bug reports — all
   built once, at the core.
2. **Scripting is the gameplay layer.** Engine code = primitives,
   physics, rendering, networking, persistence. *Game rules* —
   chess movement, unit costs, victory conditions, abilities — live
   in script. The engine ships no built-in genre.
3. **Voxels all the way down.** Geometry, art, animation, and
   physics all operate on voxel grids — never on triangle meshes.
   roxlap's KFA (bone-rigged KV6) is the *starting point*, not the
   ceiling: monada extends voxel animation toward a frame-stream
   ("voxel video") format — think *MP4 / GIF for voxels* — so a
   capture event, an explosion, or a unit walk-cycle can be either
   a rigged KV6+KFA or a baked sequence of KV6 deltas. Physics
   likewise is voxel-aware: rigid bodies, momentum, destruction
   all resolve against voxel grids, not BVHs over triangles.
   No PBR, no global illumination, no shaders. Look + feel target
   = "Slab6 / Ace of Spades / Cube Voxel art with a 2002
   strategy-game HUD."

Non-goals (v0):

- *Triangle-mesh* physics or rendering. Anything PhysX / Bullet /
  Jolt-style operating on tri-meshes is out — monada's physics is
  voxel-native (see §3.6). Voxel rigid bodies / destruction are
  in scope, post-v0.
- Photoreal rendering, dynamic GI, raytraced anything beyond
  roxlap's opticast.
- Open-world streaming at planet scale (roxlap-scene S6/S7 may get
  there; monada doesn't need it yet).
- Real-time twitch netcode (rollback / GGPO style). Lockstep only.
  Discussed in §3.1.

## 2. Influences & references

| Title | Borrowed | Rejected |
|-------|----------|----------|
| **Age of Empires II** | Tick-rate (~5–10 Hz sim, render decoupled), pathfinding-as-script, mod-friendly DAT files, replays = inputs + seed | Isometric 2D sprite renderer |
| **Warcraft III** | `.w3x` map archive idea (script + assets bundled), trigger system on top of scripting language (JASS/Lua), custom-game culture | Hero-action RPG mechanics baked into engine |
| **StarCraft (BW)** | The original lockstep-RTS netcode reference; "turn" = command-delay window | 256-unit limit, glide-pathing quirks |
| **Voxlap-era games** (Voxelstein 3D, Ace of Spades) | Visual identity — destructible voxel terrain as core texture | Per-pixel shooting / FPS gunfeel |
| **Defold / LÖVE / Factorio** | Lua-scripted runtimes with serious shipping titles built on them | Defold's proprietary IDE; LÖVE's no-multiplayer story |
| **Bevy / hecs** | ECS data layout patterns (with caveats — see §3.1) | Bevy's renderer + async scheduler (nondeterministic by default) |

## 3. Core architectural pillars

### 3.1 Deterministic lockstep simulation

**The model** (classical AoE/WC3/SC1):

```
Each client                          Each tick
+--------------------+               +------------------------+
| local input        | ---bcast--->  | gather inputs for tick |
| simulation state   |               | run sim deterministic  |
| render (interp.)   | <---all-recv  | hash state, broadcast  |
+--------------------+               +------------------------+
```

Only inputs ("player A clicked here at tick T") travel on the wire.
State never does. Every client re-runs the simulation from the same
seed and arrives at identical state. Bandwidth scales with *input
volume*, not world size — a 16-player chess match is the same wire
cost as a 16-player RTS.

**What that demands from the engine:**

| Requirement | Implementation |
|---|---|
| Deterministic arithmetic | **Q32.32 fixed-point** for all sim coords, vectors, RNG outputs. No `f32`/`f64` in `monada-sim`. Q32.32 is overkill for a chess board but the right floor for an RTS with sub-tile precision over a multi-kilometre map; the 64-bit primitive cost is negligible compared to the cost of debugging platform divergence. Float math is permitted in `monada-render` because render state is throwaway. |
| Deterministic RNG | Single seeded PCG / xoshiro per game, advanced only inside sim. No `thread_rng()` anywhere in sim. Each scripted query gets a sub-RNG split off via SplitMix-style seeding so script execution order matters less. |
| Deterministic iteration | No `HashMap` iteration in sim. Use `BTreeMap` or `IndexMap` (insertion-order). ECS entity iteration must be sorted by stable entity id, not arena index. |
| Fixed timestep | Sim runs at fixed N Hz. **Default 25 Hz (matches WC3's update rate)**, but the rate is **declared per-map at load time** in `manifest.toml` and locked for the duration of the match. Chess sets it to "advance only on player command" (turn-based); an RTS map keeps the 25 Hz default; a turn-based 4X could pick 1 Hz. Render runs at display rate and interpolates between sim states. |
| Input delay (command latency) | Each command is scheduled for tick `T = current + lag`. `lag` adapts to round-trip; AoE2 used 2–6 ticks. Lets every client receive all commands for tick T before executing it. |
| Desync detection | At the end of every Nth tick (e.g. every 30), each client hashes the sim state (FNV-1a / xxhash over canonical-ordered fields) and broadcasts the hash. Mismatch = halt + dump for diff. roxlap-oracle is the exact template — reuse the hash-and-diff harness style. |
| Replays | `(seed, ordered input stream, engine version, map hash)`. That's the whole replay file. Re-running gives bit-exact playback. Adding spectator support = same file, played live. |

**Why lockstep, not rollback?** Rollback (GGPO / RetroArch netcode) is
the modern answer for ≤4-player twitch fighting/platformers. It
requires *the ability to roll the sim back* — cheap when sim state is
small, expensive when it is a 1000-unit RTS. AoE2 ran 8 players over
56k modems on lockstep precisely because state can be arbitrarily
large. Lockstep is the right fit for monada's target genres. A
rollback variant remains possible later (e.g. for a fighting-game
demo) but would be a separate core loop.

**Why fixed-point, not "careful floats"?** IEEE-754 *is* deterministic
across architectures in theory, but:
- `x87` 80-bit intermediates on 32-bit x86 differ from SSE2 64-bit.
- `fma` instructions fuse on some chips and not others.
- Compilers reorder commutative ops at `-O2`.
- `sin` / `cos` / `sqrt` are not in the IEEE spec and *do* vary by libm.

Lockstep RTSes have been burned by every one of these. Fixed-point
sidesteps the entire class. roxlap's renderer uses floats freely
(non-deterministic across SIMD widths and that's fine for it — see
the per-arch goldens in `roxlap-oracle`), but those values never
feed back into sim state. Strict wall between sim and render.

### 3.2 Rendering — voxels via roxlap

monada renders by feeding `roxlap_scene::Scene` to roxlap's render
path and overlaying an `egui`-driven HUD on top of the same
framebuffer (winit + softbuffer host, matching `roxlap-cave-demo`).

**Capabilities inherited from roxlap:**

- Multi-grid scene with `f64` world positions and per-grid rotation
  (`roxlap-scene` S2 + S5). A chess board is one grid; each piece is
  its own grid; pieces can rotate when captured.
- KV6 sprites with KFA bone rigs. roxlap supplies the *rig + render*
  (the `roxlap-host` demo poses a rigged sprite from joint angles);
  what it does **not** ship is the animation-*clip player* (keyframe
  interpolation of baked `.kfa` curves) or scene-graph integration of
  animated sprites — those are `monada-render` work, not inherited.
  **For v0, units are 2D-style billboards: flat, camera-facing
  single-layer KV6 (PNG → one-voxel-deep slab).** This stays
  voxel-native (no triangle quads, pillar 3 intact), sidesteps the rig
  + clip-player work, and fits a limited-art budget; KFA rigs and the
  voxel-video format (§3.2) are the richer path layered in after the
  game loop proves out.
- Real-time voxel edits via `roxlap_formats::edit::*`. Carving
  terrain for buildings, craters, deformation on damage — all
  validated byte-equal against voxlap C's `setspans`.
- Serde snapshots (`roxlap-scene` S2.3) — the renderable scene
  round-trips through bytes. Replay = snapshot at tick 0 + inputs.
- Wasm32 build. Browser demos already work; monada inherits.

**Engine additions on top of roxlap:**

- A *render-from-sim* layer that, each frame, walks the deterministic
  sim state and computes the `Scene` (grid positions, sprite poses,
  HUD state) for that frame, interpolating between the last two sim
  ticks. Sim state never holds an `f64` pose; the render layer is
  where the fixed-point → float conversion happens.
- **Voxel-video format** — an extension to roxlap's animation story.
  KFA covers rigged sprite animation (joints + keyframes acting on a
  KV6 hull). monada adds a *frame-stream* format: an ordered sequence
  of KV6 deltas (set-spans / del-spans against a base keyframe)
  played back at a declared FPS. The intent is "MP4 / GIF for
  voxels" — a single binary asset usable for effects rigging cannot
  express (an explosion's volumetric expansion, a destruction
  cascade, simulation-baked cloth). Compression is run-length over
  the per-column delta stream; the codec lives in the
  `monada-voxvideo` crate so it is reusable outside the engine.
  Format spec lives in `VOXVIDEO.md` (owed before M6 implementation).
- An `egui`-driven HUD layer composited on top of the roxlap
  framebuffer. egui supplies widgets, layout, and accessibility for
  the cost of one heavy dep; the retro aesthetic is a *theme*, not
  a rendering technique.
- Camera modes: top-down strategy view (chess, RTS), free-fly
  (inherited from roxlap demos), follow-unit (later). The strategy
  view is a **high-angle perspective camera tuned for frame rate, not
  a true orthographic projection** — roxlap's opticast is perspective
  by construction (a single eye point with diverging per-column rays),
  so a steep, high camera is what delivers the AoE2/WC3 "top-down"
  read without reworking the raycaster. The requirement is "a top-down
  camera that renders fast enough to be enjoyable," and a tilted
  perspective meets it; true ortho is explicitly *not* a goal.
- Picking — screen-space click → grid/voxel/entity resolution. roxlap
  has no built-in picker, but the pieces now exist and are validated:
  - **The ray** for pixel `(px, py)` comes from the public camera
    basis. Each backend has its own projection, so the unproject must
    match the active one — the CPU opticast uses
    `(px − hx)·right + (py − hy)·down + hz·forward` (voxlap
    `setcamera`); the GPU marcher uses a vertical-FOV pinhole. Using
    the wrong one drifts the hit off-pointer proportionally to distance
    from screen centre. *(monada-render should expose one canonical
    unproject that the renderer guarantees both backends honour, rather
    than reconstructing per backend.)*
  - **Tile/board selection:** intersect that ray with the ground
    plane. No depth readback; sufficient for chess and grid-based RTS
    placement.
  - **Surface-exact hits:** roxlap's render facade now exposes
    `SceneRenderer::pick_depth(x, y) → Option<f32>` — the per-pixel
    world-t to the nearest grid surface. The world hit is
    `cam.pos + t · normalize(ray)`; floor it (after the grid transform)
    for the voxel. CPU reads its in-memory z-buffer for free; GPU
    stages the depth buffer on demand (a click-time device poll, not
    per frame). The depth is the *scene* pass's output, so overlay
    sprites (a cursor) don't occlude the pick.
  - Both routes are validated by a cursor/click prototype in
    `roxlap-scene-demo` (top-down camera, `C` to enter) on both the CPU
    and GPU backends.

### 3.3 Scripting layer

The scripting runtime is **Rhai for v0** and **WebAssembly post-v0**;
§5 documents the rationale and migration plan.

The engine exposes a fixed C-ABI-style API to scripts:

```
fn entity_create(archetype_id) -> EntityId
fn entity_set_position(e, FixedVec3)
fn entity_query(filter) -> EntityIter
fn rng_next(stream_id) -> u64
fn voxel_set(grid_id, pos, color)
fn ui_emit_event(event)
fn schedule_tick(delay_ticks, callback_id)
```

Scripts cannot:
- Read system time, network state, or the filesystem.
- Allocate memory in nondeterministic ways the host cannot seed.
- Call native code outside the allowlist above.
- Use floating-point arithmetic affecting sim state (enforcement
  varies by runtime; see §5).

Scripts can:
- Define entity archetypes (chess pieces, RTS units, abilities).
- Implement movement / attack / build rules as state machines.
- Author triggers (`on tick 0`, `on entity X destroyed`, `on UI
  button pressed`).
- Drive the HUD via a declarative widget tree.

§5 covers the language decision (Rhai → WASM); §3.5 covers the
authoring surface on top of those runtimes (in-editor code view,
blueprint-style visual graph for non-programmer designers).

### 3.4 Map / mod format

A "map" is a single **`tar.zst`** archive (chosen over Zip because
voxel data — particularly the new voxel-video streams — compresses
substantially better with zstd than with deflate, and `tar` gives us
streaming-friendly entries without Zip's central-directory seek
dance):

```
mymap.monada/                    # tar.zst-packed
  manifest.toml                  # engine version, name, players, sim_hz, deps
  scripts/
    main.rhai                    # v0 — Rhai
    rules/...
    # post-v0: precompiled .wasm modules for shipping maps
  assets/
    pieces/king.kv6              # roxlap sprite voxels
    board.vxl                    # roxlap world voxels
    fx/explosion.vvid            # voxel-video stream (§3.2)
    portraits/*.png
  audio/
    *.ogg
  locale/
    en.toml
```

**Map hashing.** SHA-256 of the canonical-serialized archive. The hash
is part of the replay file, so opening a replay with the wrong map
version fails loudly instead of desyncing silently.

**Mod stacking** (deferred). Multiple maps loaded at once with a
priority chain — useful for "base ruleset + balance mod + cosmetic
mod" later, not needed for the chess demo.

### 3.5 Authoring tools — the editor

monada ships a **WC3-World-Editor-class application** as a
first-class engine component, not a separate ecosystem afterthought.
Crate: `monada-editor`. For non-trivial maps the editor is the only
practical authoring surface — voxel terrain, KV6 sprite assets,
scripts, triggers, and the manifest are too many moving parts to
keep coherent from a text editor alone.

Subsystems, modeled on the WC3 World Editor:

| Subsystem | Description |
|---|---|
| Terrain editor | Voxel-grid editing using `roxlap_formats::edit::*` — brushes (set/del span, cube, sphere), per-voxel colour, "stamp a KV6 here" |
| Object / archetype editor | Tabular UI for the script-declared archetypes (pieces, units, abilities). Spreadsheet-style; backs the same data the scripts read |
| Trigger / script editor | Two surfaces sharing one IR (see below): a text code editor and a blueprint-style node graph |
| Asset import | KV6 / VXL / KFA preview + import; PNG → palette + KV6 conversion; ogg validation |
| Test-play | Spin up two in-process `monada-host` instances + a loopback `monada-net` for desync-aware solo testing |

**Code editor — text surface.** Embedded code editor based on
`egui_code_editor` (or `egui_addon::code_editor`) — syntax
highlighting, completion, error annotations. Two target languages
at v1:

- **Rust** (default). The same toolchain modders use for Rust mods
  after v0. The editor invokes `rustc --target
  wasm32-unknown-unknown` (or `cargo-component`) on save and
  reports diagnostics inline.
- **Haskell** (experimental). GHC's wasm backend (GHC 9.6+) makes
  Haskell-to-wasm viable. Niche but a natural fit for the "typed,
  pure, deterministic by construction" surface monada exposes.
  Experimental status means first-party support but second-class
  documentation and no compatibility guarantee before adoption
  signals it has earned one.

Both languages compile to the **same wasm module shape** consumed
by `monada-script`'s post-v0 runtime. During the v0/v1 transition,
the editor compiles either to wasm (if the WASM backend is
available) or to a Rhai AST via a restricted Rust subset; the
specific transition path is tracked in §10.2. Hand-written Rhai
remains supported and is the intended path for short trigger logic.

**Blueprint-style visual scripting — node-graph surface.** The
graph compiles to the same IR as the text editor and produces
identical bytecode. This is the authoring path for designers
working without writing code, matching WC3's "GUI Triggers" model
(compiled to JASS) and Unreal's Blueprints (compiled to BP VM
bytecode).

The Rust ecosystem offers **no off-the-shelf "Blueprints in a box."**
The available libraries supply node-graph *rendering* only; the
language semantics — node type registry, type-checker on
connections, graph-to-IR compiler — must be implemented for the
specific domain. Surveyed options:

| Library | Provides | Engine team implements |
|---|---|---|
| [`egui-snarl`](https://github.com/zakarumych/egui-snarl) (selected) | Modern node-graph widget for egui — pan/zoom, multi-port nodes, custom in/out type rendering. Actively maintained, authored by the `edict` ECS maintainer | Node type registry, type checker, graph → Rhai/Wasm IR compiler |
| [`egui_node_graph`](https://github.com/setzer22/egui_node_graph) | Older, more opinionated framework for egui. Used in the `blackjack` 3D modeler. Smaller surface, battle-tested in a shipping editor | Same as above |
| [`nodui`](https://github.com/SimonOldfield/nodui) | Newer egui node editor; minimal | Same as above |
| Blockly (Google) | Drop-in visual-programming UI compiling to JS / Python / Lua; ships as a web component | Native integration via web-view (heavy), or commit to a web-only editor |
| Rete.js / LiteGraph.js | JS node-graph engines with execution semantics included | Same web-embed compromise |

**Choice: `egui-snarl` plus a custom graph-to-IR compiler.** The
node UI is days of integration work; the compiler is the value.
Critically, *the target IR is the engine's script IR* — text-mode
Rhai/Rust, graph-mode visual nodes, and any future surface
(block-based for classroom use, voice-coded for accessibility)
converge on one backend. This mirrors the WC3 (GUI Triggers → JASS)
and Unreal (Blueprints → BP VM) architectures.

**Scope:** The editor is M5/M6 work, not v0. The M4 chess demo
ships with hand-written Rhai; the editor is the first major
deliverable after the engine itself proves out.

### 3.6 Voxel physics (post-v0)

Off the v0 critical path, but the architecture direction is
committed now so M0's sim types and tick contracts do not
foreclose it:

- **Rigid bodies** are voxel grids (their own `roxlap_scene::Grid`)
  with a fixed-point centre-of-mass, mass tensor (precomputed from
  the voxel volume at load), linear velocity, and angular velocity.
  Translation and rotation update at sim tick rate.
- **Collision** is voxel-vs-voxel grid intersection. roxlap-scene's
  chunked storage supplies broadphase via chunk occupancy bitmaps;
  narrowphase marches one grid's voxels through the other's chunks.
  All arithmetic is fixed-point.
- **Destruction** carves voxels from the impacted grid at the
  contact patch via `roxlap_formats::edit::*`. Connected-component
  analysis spawns new grids for separated chunks.
- Fluids, soft bodies, and cloth are not in scope at any milestone
  before M7. A voxel-cellular-automaton approach is the planned
  vehicle if and when those land (same fixed-point grid + tick
  loop, no new arithmetic regime).

Implementation begins no earlier than M7; the architecture appears
here so the v0 type contracts do not accidentally exclude it.

## 4. Workspace layout

Cargo workspace, mirroring roxlap's convention of small focused
crates:

| Crate | Purpose |
|-------|---------|
| `monada-fixed` | Q32.32 fixed-point types, vectors, trig LUTs. Pure Rust, `#![no_std]`-compatible, fuzz-tested for arithmetic invariants. |
| `monada-sim` | The deterministic core. Hand-rolled SoA-per-archetype world state (see §4.1), fixed-step ticker, entity ids, archetype registry, deterministic RNG, state hashing. Depends only on `monada-fixed` + `serde`. |
| `monada-script` | The scripting runtime — Rhai in v0, WASM (wasmtime native / wasmi wasm-in-wasm) post-v0 — plus the engine-side API surface scripts call into. Strict wall: this crate is the *only* place script-language types touch sim types. |
| `monada-net` | Lockstep transport. Per-tick input bundling, command-delay scheduling, desync hash exchange, reconnect, spectators. `tokio` + `quinn` (QUIC) on native, WebTransport (or WebSocket fallback) on wasm. |
| `monada-render` | Sim → roxlap-scene translator. Per-frame interpolation, picking, egui HUD compositor. Depends on `roxlap-core`, `roxlap-scene`, `roxlap-formats`. |
| `monada-voxvideo` | Voxel-video format — codec + decoder for the KV6-delta frame-stream described in §3.2. Standalone, reusable outside the engine (e.g. for an offline FX renderer). |
| `monada-physics` | Voxel-rigid-body solver (§3.6). Post-v0; lives in the workspace from M0 as an empty crate so the API contracts are visible to `monada-sim`'s archetype design. |
| `monada-host` | Native binary. winit + softbuffer + monada-render + monada-net glue. Mirrors `roxlap-host` / `roxlap-cave-demo`. |
| `monada-web` | wasm32 binary. Same as `monada-host` but with web transport. Mirrors `roxlap-web`. |
| `monada-format` | Map archive read/write (tar.zst), manifest schema, asset bundling, integrity hashing. |
| `monada-editor` | The WC3-style World Editor (§3.5). Native-only; uses egui + `egui-snarl` + an embedded code editor + a Rust/Haskell-to-wasm build pipeline. Post-v0. |
| `monada-oracle` | Determinism harness — runs a fixed scenario for N ticks on every supported platform, hashes the resulting sim state, diffs against golden. Direct lift of `roxlap-oracle`'s style. CI gates on this. |
| `monada-chess` | The first demo map, as both a "shipped map" and a development driver. Contains scripts + assets only — no engine code. |

`monada-fixed` and `monada-sim` are the load-bearing crates and
should be locked down first (with tests + fuzz). Everything else
plugs in around them.

### 4.1 ECS choice — hand-rolled SoA per archetype

**Decision: no ECS library. Hand-rolled struct-of-arrays storage,
one `Vec<Component>` per (archetype, component) pair.**

Existing Rust ECS libraries either trade determinism for ergonomics
or carry maintenance risk. The closest candidate to "drop in and
constrain" is `legion`, but the constraints required to make it
deterministic erase its value proposition:

| Legion determinism hazard | Source |
|---|---|
| Parallel scheduler | `Schedule` runs systems in parallel by default. Forcing serial execution is one flag, but it removes the library's main selling point. |
| Internal hashmaps | `EntityStore` uses `HashMap` for entity-to-archetype lookups; iteration order can leak through entity creation order in subtle ways. |
| Archetype storage order | Stable in current builds, but not part of the public API contract. A future version could reorder for cache wins and silently break replays. |
| Maintenance posture | Legion was the Amethyst engine's ECS; Amethyst is archived. Legion itself is in maintenance mode. |

`hecs` is the better library candidate: single-threaded by default,
archetype-based, minimal API surface, actively maintained, with no
hidden HashMap iteration order in practice. Its remaining cost is
that it abstracts over `Vec<T>` — which, at monada's scale, is
abstraction without payoff.

Alternative architectures considered:

| Design | When it wins | Fit for monada |
|---|---|---|
| Sparse-set ECS (`specs`, `bevy_ecs` storage variant) | High component churn — entities gain/lose components every tick | Poor fit: RTS units do not gain/lose components mid-match; archetypes are stable for the entity's lifetime |
| Generational-arena per type (`slotmap`, `thunderdome`) | Minimal "typed handles" world | Equivalent to SoA-per-archetype here, with slightly worse iteration locality |
| Database-style row store (the Factorio approach) | Genuinely tabular state with many joins | Overkill: chess and the eventual RTS both have a fixed small set of entity kinds |
| Pure functional / persistent state (Paradox approach) | Trivially-correct rollback semantics | Wrong tool: monada is lockstep, not rollback; the allocation cost would be real |

For monada's workload — a handful of stable archetypes (pieces,
units, projectiles, buildings, terrain props), each with hundreds
to a few thousand instances — the SoA-per-archetype layout is the
simplest design that maps directly to the determinism requirements:

```rust
// Sketch in monada-sim
pub struct World {
    pub tick: u64,
    pub rng: DeterministicRng,
    pub pieces: ArchetypeStorage<PieceArchetype>,    // SoA inside
    pub units:  ArchetypeStorage<UnitArchetype>,
    pub projectiles: ArchetypeStorage<ProjArchetype>,
    pub free_ids: BTreeSet<EntityId>,
}
```

Iteration is a straight loop in entity-id order; world hashing for
desync detection is a fixed walk through the archetype storages in
a fixed order; serde-derive on the storage types yields replay
snapshots without additional code. No query language, no scheduler,
no parallel seam to fence.

If library ergonomics later prove worth a dependency, `hecs` is the
documented fallback.

## 5. Scripting language

**v0 runtime: Rhai. v1+ runtime: WebAssembly** (wasmtime on native,
wasmi for in-browser hosting). Lua and Luau are explicitly ruled
out.

### 5.1 Why not Lua / Luau

Despite Lua's dominance of the embedded-scripting space, two
concrete problems disqualify it for monada:

1. **FFI cost.** Every script-to-Rust call crosses the C ABI
   through Lua's `lua_State`, marshalling Lua values to Rust
   values on each call. An RTS with thousands of per-tick script
   callbacks (movement validation, ability triggers, AI decisions)
   pays this overhead per call and cannot amortize it. The
   Rust-Rhai border, by contrast, is plain Rust function calls —
   Rhai is itself a Rust crate.
2. **No typed-language option survives the determinism filter.**
   Luau provides typing but inherits Lua's double-by-default
   arithmetic. Mitigating that requires a patched Lua build
   (`LUA_FLOAT_TYPE=LUA_FLOAT_INT64`) and forbidding raw numeric
   operations in scripts; the typed-language ergonomics that
   motivated picking Luau in the first place do not survive that
   constraint.

### 5.2 Why Rhai for v0

- **Pure Rust.** No FFI seam, no patched C build, compiles cleanly
  to wasm32. Matches roxlap's idiomatic-Rust philosophy.
- **Determinism is a single feature flag.** `rhai = { version =
  "1", default-features = false, features = ["no_float", "sync"]
  }` removes float support at the crate level — scripts cannot
  perform IEEE arithmetic at all. Combined with exposing only
  `monada-fixed` types through the host API, determinism is
  enforced statically rather than by convention.
- **Adequate for v0 scope.** The M4 chess demo and other early
  trigger-density gameplay fit comfortably within Rhai's
  tree-walking-interpreter performance envelope.

### 5.3 Why WASM post-v0

WASM is the planned runtime for v1+ because it addresses three
problems Rhai cannot:

1. **Modder IP protection.** Rhai ships scripts as source text;
   any user can copy a clever mod's mechanics. A compiled wasm
   module is a stripped binary — not impossible to reverse, but
   the friction is sufficient that authors can ship balance
   calculations or AI logic without exposing them. WC3
   custom-map authors have wanted this since the engine launched
   and never got it.
2. **Choice of source language.** Modders write Rust (default),
   Haskell (experimental — GHC's wasm backend is shipping),
   AssemblyScript, Zig, C, or any other language with a wasm
   target. The editor's language picker is real rather than
   aspirational; no author is forced into an engine-specific
   dialect.
3. **Performance for hot paths.** Wasm code optimized by
   wasmtime's Cranelift backend runs within ~1.5× of native Rust
   on typical workloads. A 1000-unit RTS with per-tick ability
   evaluation fits in that envelope; a tree-walking interpreter
   at the same scale does not.

**Why not WASM in v0?** Build-pipeline friction. The chess demo's
"hello world" path should be `cargo run -p monada-chess`, not
"install the wasm32 toolchain, install cargo-component, compile
the script crate to wasm, place the result." Rhai's text-file
scripts keep iteration fast while the engine itself is still
moving. WASM lands alongside the editor (§3.5), which hides the
wasm toolchain behind a save → compile → test workflow — the way
Unreal hides MSVC from a Blueprint user.

### 5.4 Candidates considered and rejected

| Candidate | Reason rejected |
|---|---|
| Lua 5.4 (`mlua`) | FFI cost; double-by-default arithmetic; no typed surface |
| Luau (`mlua` luau feature) | Same FFI + double problems; smaller modder familiarity than vanilla Lua |
| `piccolo` (pure-Rust Lua) | Promising but alpha-stage with bus factor 1; revisit in 2–3 years |
| Custom DSL | Engineering cost not justified when Rhai exists |

### 5.5 Migration plan: Rhai → WASM

Every script call routes through `monada-script`'s host-side API
surface (the table in §3.3), so the runtime is swappable. The
migration:

1. **v0:** `RhaiBackend` ships as the only `ScriptBackend`
   implementation.
2. **M6:** `WasmBackend` lands alongside, gated behind a feature
   flag. Both runtimes implement the same `ScriptBackend` trait;
   each map's `manifest.toml` selects via `script_runtime = "rhai"
   | "wasm"`.
3. **M6+:** New ambitious maps adopt the wasm runtime from
   day one. The chess demo can remain on Rhai indefinitely or
   migrate; both are supported.
4. **Editor (M5/M6):** The Rust/Haskell code editor targets only
   the wasm backend. Rhai's role becomes "embedded glue scripts";
   wasm becomes "real mods".

No engine code changes during migration. Per-map cost is rewriting
that map's scripts; cumulative cost is bounded by deliberately not
writing many before M6.

## 6. First demo: Chess 2.0

Purpose: prove every architectural seam end-to-end on the simplest
ruleset that still admits interesting mods.

**Definition of "Chess 2.0":**
- Standard 8×8 board with standard pieces as the default ruleset.
- Map scripts can replace any rule: piece movement, board geometry
  (10×10, hex grids), victory condition, or "races" (factions
  where, for example, the King becomes a Wizard with a one-time
  teleport ability).
- Two-player only for v0. Lockstep at N=2 is still lockstep.
- Turn-based tick model: each player submits exactly one command
  per turn and the sim advances when both commands have arrived.
  This is degenerate lockstep — command delay equals one full turn
  — and exercises the same code path an RTS uses at 25 Hz (§3.1).
  The map declares `sim_hz = "on_command"` in `manifest.toml`.

**What this stresses, by pillar:**

| Pillar | Exercise |
|---|---|
| `monada-fixed` | Trivial — chess uses i8 board coords. Validates that the type system propagates fixed-point properly even when the actual math is integer. |
| `monada-sim` | ECS with ~32 entities, archetype-driven rule lookup, state hashing. Tiny, but covers every code path. |
| `monada-script` | Movement rules as scripted predicates. A "custom race" is a swap of the script bundle. First load-bearing validation of the language choice: if expressing a knight's L-move in Rhai is awkward, the issue surfaces immediately. |
| `monada-net` | Two clients, command-per-turn lockstep over QUIC. Desync hashes after every turn. Reconnect-and-resync flow. |
| `monada-render` | Voxel chess pieces (KV6 sprites), animated capture (KFA), camera orbit, click-to-pick, HUD with turn timer + move history. |
| `monada-format` | The chess map ships as `chess.monada` — manifest + scripts + 16 piece KV6s + board.vxl. |
| `monada-oracle` | Fixed-scenario test: scripted 30-move game from seed S must reach state hash H on every platform / build. |

**Voxel art bootstrap:** initial pieces are simple cube-stacks
generated procedurally (`roxlap-cavegen`-style). Authored KV6 art
is a polish pass after the engine works end-to-end.

**Stretch mods that validate the "engine ships no genre" claim:**

- Chess on a hex grid.
- A "wizard" piece that costs three turns of inaction to cast a
  teleport.
- "Auto-chess" — pieces move on their own toward enemies; the
  player only places them.

If any of these require engine changes, the abstraction boundary
is in the wrong place.

## 7. Milestones / roadmap

**M0 — Determinism kernel (no game yet).**
- `monada-fixed` lands with arithmetic + trig LUTs + fuzz tests.
- `monada-sim` lands with a trivial scenario ("100 entities walk in
  a circle") that hashes to a known value on every platform.
- `monada-oracle` gates CI.

**M1 — Render bridge.**
- `monada-render` translates `monada-sim` state → `roxlap_scene::Scene`.
- `monada-host` shows the M0 walk-in-a-circle scenario rendered as
  100 KV6 sprites on a voxel plane.
- Camera, picking, basic HUD.

**M2 — Scripting wall.**
- `monada-script` lands with `RhaiBackend` and the host API.
- Walk-in-a-circle rewritten as a Rhai script. Engine code knows
  nothing about circles.

**M3 — Lockstep.**
- `monada-net` lands (quinn / QUIC). Two `monada-host` instances run
  the scripted scenario in sync. Desync hashes match. Replay file
  works.

**M4 — Chess 2.0 ships.**
- `chess.monada` map (tar.zst), two-player over LAN, standard rules
  + one "race" variant. Replays viewable. Wasm build runs in browser.

**M5 — Editor v0.**
- `monada-editor` lands with terrain editor, archetype editor, and
  the Rhai text-mode code editor. Visual scripting (egui-snarl) and
  WASM backend deferred to M6.

**M6 — Voxel video + WASM backend.**
- `monada-voxvideo` codec + decoder; chess demo gains a baked
  capture animation as proof of concept.
- `WasmBackend` lands behind a feature flag; editor gains Rust /
  Haskell language pickers and the egui-snarl visual graph.

**M7+ — open.** RTS demo, voxel physics demo (§3.6), modder-UX
polish. Scope decided after M6 lands.

## 8. Risks

| Risk | Mitigation |
|---|---|
| Nondeterminism creeps into `monada-sim` (a `HashMap` iteration, a `sin` call, a `thread_rng`) | `monada-oracle` runs on every PR across a Linux/macOS/Windows matrix. Clippy gates: workspace `clippy.toml` `disallowed-types` flags `HashMap`/`HashSet`; `monada-sim` additionally `#![deny(clippy::float_arithmetic, clippy::disallowed_types)]`. (No lint bans the float *type* — the hazard is float *arithmetic*, which `float_arithmetic` catches.) |
| roxlap-scene's render path is not deterministic (and is not required to be) — render-side drift could desync from sim invisibly | Wall enforced at the module boundary: `monada-render` reads sim state only. Enforced at compile time via trait bounds. |
| Rhai's interpreter becomes too slow as gameplay scales beyond chess | WASM backend is the planned escape hatch (M6). `monada-script`'s `ScriptBackend` trait abstraction exists from M2 so the swap does not cascade. |
| KV6 authoring is a niche skill (voxel art bottleneck) | Procedural pieces (in-code) for v0. Authored art is M4 polish, not a blocker. The editor (M5) widens the authoring funnel. |
| Voxel-video format is new design work, not a port | The format spec is treated as a real subproject — `VOXVIDEO.md` design doc lands before M6 code. Bit-exact codec round-trip is validated via a `monada-voxvideo-oracle` patterned on `roxlap-oracle`. |
| Editor scope creep (WC3's World Editor was a multi-year project) | Editor work begins at M5. The M4 chess demo must be authorable by hand-editing tar contents and a text editor; if it is not, the engine API has the wrong shape. |
| Lockstep latency feels bad over modem-grade RTT | Adaptive command delay (AoE2 used 2–6 ticks). The chess demo masks the problem because it is turn-based; the eventual RTS demo will require tuning. |

## 9. Foundations

The combination of language, dependency stack, and prior work makes
the architecture above realistic on a small team:

- roxlap's scene-graph, serde snapshots, and real-time edit APIs
  cover the visual side of the sim-to-world mapping with no
  additional engine work required.
- roxlap's wasm pipeline gives monada browser play without
  additional toolchain investment.
- Rust's type system enforces the sim/render wall — and a single
  `#![deny(clippy::float_arithmetic)]` in each sim crate forbids float
  *operations* there (no lint can ban the `f32`/`f64` type itself, but
  the arithmetic is the actual divergence hazard).
- The dependency set is pure Rust end-to-end (Rhai, glam-based
  fixed-point, quinn for QUIC, winit + softbuffer for the host
  loop). One workspace, one toolchain, no FFI seams. The same
  philosophy as roxlap.

## 10. Decisions and open questions

### 10.1 Locked decisions

| Topic | Decision |
|---|---|
| Genre lineage | AoE2-style lockstep RTS / RPG family |
| Sim arithmetic | Q32.32 fixed-point (single precision tier; no Q16.16 variant) |
| Sim tick rate | Configurable per-map at load. Default 25 Hz (WC3 parity); turn-based maps use `sim_hz = "on_command"` |
| Network transport | QUIC via `quinn` on native; WebTransport on wasm (WebSocket fallback) |
| ECS | Hand-rolled SoA per archetype — no library. `hecs` is the documented fallback |
| HUD | egui composited over the roxlap framebuffer |
| Map archive | tar.zst |
| Scripting (v0) | Rhai with the `no_float` feature |
| Scripting (v1+) | WebAssembly — wasmtime on native, wasmi for wasm-in-wasm. Authoring languages: Rust (default), Haskell (experimental). Editor compiles to wasm |
| Visual scripting | egui-snarl + a custom graph-to-IR compiler (no off-the-shelf "Blueprints in a box" exists for Rust) |
| Animation | KFA (inherited from roxlap) + new voxel-video frame-stream format (`.vvid`) |
| Physics | Voxel-native (rigid bodies as grids, voxel-vs-voxel collision, edit-API destruction). Implementation post-v0 |

### 10.2 Open questions

None block the M0–M4 critical path. Listed so they are tracked
when they become relevant:

1. **Voxel-video format specification.** A `VOXVIDEO.md` design
   doc is owed before M6 implementation. Sub-questions:
   chunked-vs-whole-grid keyframes; inter-frame delta granularity
   (per-voxel, per-span, or per-column); whether colour-palette
   deltas piggyback on the geometry stream or live in a sidecar.
2. **Editor's Rust/Haskell-to-Rhai bridge during M5.** If the
   editor lands at M5 before the WASM backend at M6, two options
   apply: (a) the M5 editor supports only Rhai text-mode and
   Rust/Haskell appear at M6; (b) the M5 editor compiles a
   restricted Rust subset to a Rhai AST. (a) is the simpler
   plan; (b) delivers a complete editor UX at M5 at the cost of a
   throwaway compiler. Current preference: (a).
3. **NAT traversal for QUIC.** QUIC over UDP requires
   punchthrough or a relay. STUN plus a community relay is the
   modern baseline; matchmaker design is owed at M3.
4. **Anti-cheat under lockstep.** Lockstep guarantees every
   client sees the same state; cheating manifests as desync.
   Desync detection is built in; identifying *which* client
   diverged is not, and is owed before ranked multiplayer.
5. **Audio.** Not addressed above. The chess demo likely needs
   piece-move SFX. A `monada-audio` crate is the probable home;
   defer until M4 polish.
6. **WASM mod sandbox policy.** Once the wasm backend is live,
   the review surface for mods that could request arbitrary
   imports needs a policy. The expected baseline is wasmtime's
   deny-by-default import table plus an explicit per-map
   allow-list, but the policy schema itself is undefined.
