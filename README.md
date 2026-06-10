# monada

A Rust game engine for deterministic, lockstep-networked games in the
spirit of late-90s / early-00s strategy classics, with CPU voxel
rendering via [`roxlap`](https://crates.io/crates/roxlap-core).

Three design pillars: **determinism first**, **scripting is the
gameplay layer**, **voxels all the way down**. See [`DESIGN.md`](DESIGN.md)
for the full architectural reference.

## Status

Pre-v0, implementing **M0 — the determinism kernel**:

- `monada-fixed` — Q32.32 fixed-point scalars, vectors, and trig.
- `monada-sim` — the deterministic simulation core.
- `monada-oracle` — cross-platform state-hash regression harness.

The workspace layout (one focused crate per subsystem) mirrors roxlap's
convention and is documented in `DESIGN.md` §4.

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
