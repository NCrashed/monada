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
