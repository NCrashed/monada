# Anatomy of a map

A map is a single archive: a `tar` stream compressed with zstd, by
convention named `something.monada`. During development you can also point
the host at an unpacked directory with the same layout, and it packs it for
you.

## Layout

```text
my-map/
├── manifest.toml      # required — the map's identity and runtime needs
├── scripts/
│   └── main.rhai      # the entry script (init, tick, command, ...)
└── assets/            # optional — sprites, tiles, sky, audio
    ├── pieces/king.kv6
    └── skybox.png
```

Everything under `scripts/` is Rhai source; everything under `assets/` is
opaque data the script refers to by its archive-relative path.

## The manifest

`manifest.toml` declares what the engine needs to load and run the map:

```toml
name = "Hello Voxels"
engine_version = "0.0.1"
host_api = 1
players = 1
sim_hz = "30hz"
script_runtime = "rhai"
entry = "scripts/main.rhai"
```

| Field | Meaning |
|---|---|
| `name` | Human-readable title, shown in the host and used as the key for saved key bindings. |
| `engine_version` | The engine version the map was authored against. |
| `host_api` | The script-API version the map requires — the version of the function set your scripts call (`entity_create`, `voxel_set`, …). Declare the oldest version that carries every verb you call; a host runs only a version inside its supported range (`1..=14` today) and refuses the map up front instead of failing mid-game. Growth is additive, so the range's top moves with each new verb while the bottom stays put — an old map keeps working. Only a *breaking* change moves the bottom, turning an outdated declaration into a loud refusal rather than a quiet re-interpretation. Omitted = `1`. |
| `players` | Player count (a two-player map is `2`). |
| `sim_hz` | The tick model — see below. |
| `script_runtime` | The scripting backend; `"rhai"` in v0. |
| `entry` | Archive-relative path to the entry script. |

Two optional fields appear in later chapters: `[[action]]` tables declare
[rebindable inputs](input.md), and `local_entry` names a separate script for
the map's [local, unsynchronized layer](input.md).

## The tick model

`sim_hz` decides *when* the simulation advances, and it is the single most
important choice in the manifest:

- **`"on_command"`** — turn-based. The world only advances when a command
  arrives. Chess uses this: nothing moves until someone makes a move.
- **`"30hz"`** (any rate) — real-time. The world advances on a fixed clock,
  and the map's `tick(dt)` runs every step. The action-RPG uses `30hz`.

The rate is fixed, not the frame rate: the host renders as fast as it can
and interpolates between the last two simulation steps, but the simulation
itself always steps at exactly the declared rate, so every machine computes
the same sequence of states.

## Identity

A map's identity is the SHA-256 of its canonical (uncompressed) `tar`. That
hash rides in every replay and networked match, so opening a replay against
the wrong version of a map fails loudly instead of desyncing silently.
Packing is deterministic — sorted entries, zeroed timestamps — so the same
source always produces the same hash.

## Named constants, and enums

Rhai has no `enum`, and a map script cannot reach a top-level `const` at all:
**functions are pure**, so a `const NORTH = 2;` at the top of the file is
invisible inside every `fn` (`Variable not found: NORTH`). The same goes for a
top-level `let`.

So a named constant in a map is a **zero-argument function**, and an enum is a
family of them over small integers:

```rhai
// BlockSide — which face of a block something is on.
fn SIDE_NORTH() { 0 }
fn SIDE_EAST()  { 1 }
fn SIDE_SOUTH() { 2 }
fn SIDE_WEST()  { 3 }
fn SIDE_COUNT() { 4 }
```

Integers, not strings, because that is what a value has to be to live anywhere
that matters: an entity field holds a `Fixed`, so a side is stored as
`entity_set_field(e, "side", fixed(SIDE_EAST()))` and read back with
`to_int(entity_field(e, "side"))`. A command carries the same shapes — an `i64`
verb and target, and three `Fixed` in its `arg` — so an integer enum crosses the
network wire and enters the desync hash without a conversion. A string could do
neither.

Give the family the operations you actually want beside it, so the encoding
stays in one place:

```rhai
/// The unit step of a side, as a sim-space direction.
fn side_step(s) {
    if s == SIDE_NORTH() { vec3(fixed(0), fixed(1), fixed(0)) }
    else if s == SIDE_EAST() { vec3(fixed(1), fixed(0), fixed(0)) }
    else if s == SIDE_SOUTH() { vec3(fixed(0), fixed(-1), fixed(0)) }
    else { vec3(fixed(-1), fixed(0), fixed(0)) }
}

/// The opposite side — arithmetic the encoding earns you.
fn side_opposite(s) { (s + 2) % SIDE_COUNT() }

/// For the HUD only. Indexing an array by the value keeps the order in one
/// place instead of spread over four branches.
fn side_name(s) { ["north", "east", "south", "west"][s] }
```

`switch` works and reads better than a chain of `if`s — but **its labels must be
literals**, so it cannot name your constants (`switch s { SIDE_NORTH() => … }`
is a compile error, "Expecting a literal expression"). Use it where the numbers
are obvious at the call site, and `if`/`else if` where the names carry the
meaning:

```rhai
fn side_axis(s) { switch s { 0 => 1, 2 => 1, _ => 0 } } // 1 = the y axis
```

Two traps worth knowing before you hit them:

- **A value out of range must be caught by you.** `side_name(7)` is an
  out-of-bounds index and raises; `side_step(7)` silently answers "west",
  because the last branch is an `else`. Decide which you want per function —
  a total function with a defined default, or one that raises on a value that
  should never exist — and say so in its doc comment.
- **Never compare across number types.** `action_axis` answers with an `INT`
  while `action_axis2` and every field answer with `Fixed`, and Rhai has no
  `>` registered between them: it evaluates to `false` rather than raising, so
  the branch silently never fires. Wrap at the boundary — `fixed(action_axis(…))`
  — or compare integers with integers.
