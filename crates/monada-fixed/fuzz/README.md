# monada-fixed fuzz targets

cargo-fuzz harnesses backing the "fuzz-tested for arithmetic
invariants" claim in `DESIGN.md` §4. Detached from the engine workspace
(its own `[workspace]`) because `libfuzzer-sys` needs nightly +
`-Zsanitizer` flags that the normal `cargo build --workspace` must not
apply.

## Running

The toolchain (cargo-fuzz + clang + LLVM + the pinned nightly) lives in
a dedicated flake devshell, kept separate from the default shell so the
heavy LLVM stack isn't pulled in for ordinary builds:

```sh
nix develop .#fuzz                # from the repo root
cd crates/monada-fixed            # cargo-fuzz looks for the ./fuzz dir here
cargo fuzz list                   # roundtrip / sqrt / mul_assoc
cargo fuzz run roundtrip          # runs until the first crash
cargo fuzz run sqrt -- -max_total_time=60
cargo fuzz run mul_assoc
```

The shell's nightly is the default toolchain, so no `+nightly` is
needed. Without Nix: `cargo install cargo-fuzz` and have `clang` on
`PATH`, then the same `cargo fuzz …` commands.

Targets:

| Target | Invariant |
|--------|-----------|
| `roundtrip` | `from_bits`/`to_bits` identity; additive identity/inverse; `+`/`*` commutativity; `*` identity & annihilator — all exact for every bit pattern |
| `sqrt` | non-negativity, monotonicity, and the floor bound `r² ≤ x` |
| `mul_assoc` | `(a·b)·c` ≈ `a·(b·c)` within a rounding envelope (catches wrap/shift regressions) |

These complement the deterministic LCG property tests in
`../tests/arithmetic.rs` — same invariants, but coverage-guided input
search instead of a fixed sample.
