# The determinism contract

*This chapter is not yet written — but it is the most important one, so here
is the short version.*

The simulation must be bit-identical on every machine and every run. To hold
that line, map scripts follow a few rules:

- **No floating point in simulation logic.** Use fixed-point numbers
  (`fixed`, `ratio`) and the provided `sin` / `cos`. Floating-point results
  vary across CPUs and compilers.
- **Randomness comes from the world's seeded RNG** (`rng01`, `rng_below`),
  never from wall-clock time or the host.
- **No iteration-order-dependent logic** over unordered collections. The
  engine's entity queries return ids in a defined order for exactly this
  reason.
- **Presentation is not simulation.** Rendering, sound, camera, and UI calls
  never affect the world state or its hash, so a headless peer that skips
  them still computes the same result.

The full chapter will spell out which host functions are hashed versus
presentation-only, the coordinate conventions (including the render-side
world-X mirror), and how the determinism oracle gates all of this in CI.
