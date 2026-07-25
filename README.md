# monada

A Rust game engine for deterministic, lockstep-networked games in the
spirit of late-90s / early-00s strategy classics, with CPU voxel
rendering via [`roxlap`](https://crates.io/crates/roxlap-core).

Three design pillars: **determinism first**, **scripting is the
gameplay layer**, **voxels all the way down**.

## Documentation

- [**The Map-Maker's Book**](https://ncrashed.github.io/monada/) — the guide
  to writing maps: the archive format, the scripting API, determinism,
  input and bindings, and networking, each with a runnable example. Its
  sources live in [`book/`](book/).
- [`DESIGN.md`](DESIGN.md) — the full architectural reference.

## Status

Pre-v0. The core is proven end to end: Q32.32 fixed-point math
(`monada-fixed`), the deterministic sim (`monada-sim`), Rhai scripting
behind the host-API wall (`monada-script`), QUIC lockstep + replays
(`monada-net`), the map archive format (`monada-format`), deterministic
voxel rigid-body physics with wheels, destruction and drilling
(`monada-physics`), grid pathfinding (`monada-nav`), and the roxlap
host (`monada-host`) — all gated by the cross-platform state-hash
harness (`monada-oracle`, `monada-hashes.txt`). The workspace layout
(one focused crate per subsystem) mirrors roxlap's convention and is
documented in `DESIGN.md` §4.

## Demos

Each demo is a thin `cargo run` launcher around a pure script + assets
map, and each pins its own oracle golden:

| Demo | `cargo run -p …` | What it proves |
|---|---|---|
| Chess 2.0 | `monada-chess` | turn-based rules entirely in the map script; LAN lockstep + replays |
| Action RPG | `monada-rpg` | real-time tick, per-tick input, animated GIF billboard actors, co-op |
| Ship | `monada-ship` | multi-deck interiors: deck cutaway, fog of war, per-client visibility |
| RTS | `monada-rts` | WC3-style orders over deterministic A* (`monada-nav`), economy, multi-select |
| Digger | `monada-digger` | the physics payoff — see below |

**The digger** is the `monada-physics` showcase: a drill-nosed vehicle
on true 3D *volume* terrain (`terrain = "volume"` — the column
heightmap can't hold a tunnel). Drive it, jump it off stepped ramps,
and bore into the mountain: the drill carves the hashed voxel store
in-sim while the engine mirrors bodies, wheels and debris to the
screen automatically. Pitch the drill down to descend through the
apron into a hidden basement vault, and back up to climb out; three
crystals — one up a ramp, one inside the mountain, one underground —
are the finish line. Every carve, contact and wheel impulse folds into
the same lockstep state hash as the entity world (`digger@` in the
goldens), so the whole sandbox would replay bit-identically on any
platform.

## Building

Native builds and tests run on stable Rust. The `rust-toolchain.toml`
nightly pin only matters for the wasm-threads path inherited from
roxlap.

```sh
cargo test --workspace
cargo run -p monada-oracle      # determinism harness
```

### Dev shells (Nix)

The flake provides two devshells:

```sh
nix develop          # default: toolchain + render/wasm deps
nix develop .#fuzz   # cargo-fuzz + clang/LLVM for the monada-fixed fuzz targets
```

Fuzzing the fixed-point core (arithmetic invariants — see
[`crates/monada-fixed/fuzz`](crates/monada-fixed/fuzz/README.md)):

```sh
nix develop .#fuzz
cd crates/monada-fixed
cargo fuzz run roundtrip        # also: sqrt, mul_assoc
```
