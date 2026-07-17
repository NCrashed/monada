# Plan: the Map-Maker's Book

Status: skeleton landed (build-order steps 1–2). Done: mdBook at
`book/` (book.toml + SUMMARY + real chapters 1–3, stubs 4–11); the
first example `book/examples/01-hello-voxels` (a real runnable map);
the example harness `monada_oracle::run_example_map` + the
`book_examples_run_headless` test that packs/loads/runs every
`book/examples/*` under the normal `cargo test` matrix; `book.yml`
CI (mdbook build on PR + master, deploy to Pages on master); mdbook
in the flake devshell. Deferred: mdbook-linkcheck (multi-backend
output-path friction — add once verifiable), the API-reference
fleshing out chapters 4, 6, 8–10. Chapter 11 (API reference) is
written — every registered host function, grouped by layer — and the
`api_reference_matches_registered_functions` test in monada-oracle
diffs it against the `register_fn` names monada-script actually
registers, so adding or removing a host function fails CI until the
reference is updated to match (Phase 1 of §2's coverage strategy; the
Phase-2 generator stays optional). Chapter 7 (input) is written, with a second runnable
example (`book/examples/02-top-down-mover`) demonstrating declared
actions, the local layer (`local_tick` + `action` + `pick_ground`),
and the command handler. Chapter 5 (the determinism contract) is
written — the core rules chapter (fixed-point, seeded RNG, defined
iteration order, the simulation/presentation split, coordinates, and
how the oracle gates it all). The book is deployed to GitHub Pages
(https://ncrashed.github.io/monada/) and linked from the README.

A book for map authors: engine API reference, guides, and runnable
examples — with CI that builds the book and compiles/runs every
example. Companion plan: `docs/plans/input-bindings.md` (its chapters
land here once the feature ships).

## 0. Shape

- **mdBook** at `book/` in this repo. Book tracks `master`; each
  example pins `engine_version` in its manifest like any map.
- Written in English (matches DESIGN.md and code comments); a
  translation is a later, separate decision.
- **Examples are real maps, not snippets.** Each lives at
  `book/examples/NN-name/` with the standard map layout
  (`manifest.toml`, `scripts/`, `assets/`) and is runnable via
  `monada-host --map book/examples/NN-name`. Chapters pull code with
  `{{#include}}` + anchors.
- **Policy: no inline dead code.** Every code block in prose is
  either included from a compiled example or explicitly marked
  illustrative. This is what keeps the book honest: examples that CI
  doesn't execute rot into lies.

## 1. Contents outline

1. **Getting started** — install/build the host, run the bundled
   chess and rpg maps, run your first example map.
2. **Anatomy of a map** — archive format (tar.zst, SHA-256 identity),
   `manifest.toml` fields (`players`, `sim_hz`, `entry`, later
   `local_entry` and `[[action]]`), directory layout.
3. **Hello, voxels** — tutorial: paint a board, `voxel_fill`, tiles,
   lighting/sky, camera.
4. **Entities and models** — archetypes, KV6 sprites, actors/GIF
   billboards, anim/facing/tint.
5. **The determinism contract** — the single most important chapter:
   fixed-point only, no float in sim, `rng01`/`rng_below`, why
   HashMap is banned, what is hashed vs presentation-only,
   coordinate conventions (incl. the world-X mirror).
6. **Commands and lockstep** — `Command {verb, target, arg}`, the
   `command(player, ...)` handler, turn-based vs `sim_hz` real-time
   maps, `tick(dt)`, validation rules ("the client has zero
   authority"), replays and goldens.
7. **Input, actions, bindings** — after the input-bindings plan
   ships: local layer, `local_tick`, action declarations, pick API,
   targeting contexts. Chess click-FSM and rpg movement as worked
   examples.
8. **UI, HUD, audio** — `ui_*`, `status`, sounds/music, dialogue
   subsystem.
9. **Multiplayer** — hosting/joining over LAN, `local_player`,
   observer behavior, desync diagnostics.
10. **Publishing** — packing with monada-format, versioning, hash
    identity.
11. **Reference** — manifest schema; the full Rhai API, split by
    layer (sim / local / presentation), one entry per registered
    function with signature and determinism notes.

Tutorial chapters 3–7 each ship with their own `book/examples/` map;
the chess and rpg maps double as the capstone worked examples.

## 2. API reference strategy

- **Phase 1 (hand-written + enforced coverage).** Reference pages are
  written by hand. A CI check extracts the set of registered function
  names from `monada-script`'s Rhai registration
  (`rhai_backend.rs`) and fails if any registered function is missing
  from the reference (and vice versa — no documenting removed API).
  Cheap to build, catches drift immediately.
- **Phase 2 (optional, single source of truth).** Move registration
  into a declarative table in `monada-script` (name, signature,
  layer, doc string) consumed both by the Rhai registration code and
  by a generator that emits the reference markdown. Do this only if
  Phase 1's checker proves annoying to maintain.

## 3. CI

New workflow jobs (or a `book.yml` workflow):

1. **`book-build`** — `mdbook build` + `mdbook-linkcheck`. Fails on
   broken intra-book links and missing `{{#include}}` targets (a
   renamed anchor in an example breaks the build — by design).
2. **`book-examples`** — an integration test in `monada-oracle`
   (which already runs maps headless) that globs
   `book/examples/*/`, packs each with monada-format, loads it, and
   runs N ticks asserting no script/load errors. Runs under the
   existing `cargo test` matrix, so examples are exercised on
   Linux/macOS/Windows. Examples with interesting sim logic
   additionally get golden hashes in `monada-hashes.txt` — teaching
   determinism by example, gated by the existing determinism job.
3. **`api-coverage`** — the Phase-1 reference/registration diff
   check (a unit test in `monada-script` or a small script job).
4. **`book-deploy`** — on push to `master`, publish `book/book/` to
   GitHub Pages.

## 4. Build order

1. **Skeleton**: `book/` with book.toml, SUMMARY, chapters 1–2
   stubs; `book-build` + `book-deploy` CI. The book is live (thin)
   from day one.
2. **Example harness**: the `monada-oracle` glob test over
   `book/examples/`; first example `01-hello-voxels` wired into
   chapter 3.
3. **Core chapters**: determinism contract (ch. 5), commands &
   lockstep (ch. 6) — these encode knowledge currently living only
   in DESIGN.md and heads.
4. **Reference + coverage check** (ch. 11, CI job 3).
5. **Input chapter** (ch. 7) — lands with/after the input-bindings
   plan, its examples double as that feature's acceptance demos.
6. Remaining chapters (UI/audio, multiplayer, publishing) as the
   features they describe stabilize.
