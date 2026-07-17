# The determinism contract

Everything else in monada rests on one rule: the simulation must produce
bit-identical results on every machine and every run, given the same inputs.
This is what lets two players re-derive the same game state from the same
command stream instead of shipping the state itself (lockstep networking),
and what lets a recorded match replay exactly. Much of the scripting API is
shaped by this rule, and a map that breaks it desyncs — silently at first,
then loudly when a checkpoint hash diverges.

The rules below are the whole of the contract. Follow them and your map is
deterministic by construction.

## Fixed-point numbers, not floats

Simulation logic uses fixed-point numbers, never floating point. A float
result can differ in its last bit between CPUs, compilers, and optimization
levels — harmless for rendering, fatal for a hash that must match exactly.

You build fixed-point values with `fixed` and `ratio`, and the only
transcendental functions are the provided fixed-point ones:

```rust,ignore
let one = fixed(1);        // the integer 1
let half = ratio(1, 2);    // one half
let n = to_int(some_fixed); // floor back to an integer, for board coords
let s = sin(angle);        // fixed-point trig; also cos, tau, pi
```

There are no floating-point literals in a map script — the scripting
language rejects them outright. When you need an integer (a board square, an
archetype tag), work in integers; when you need a fraction, use `ratio`.

## Randomness comes from the world

Any randomness must come from the world's seeded generator, never from
wall-clock time or the host:

```rust,ignore
let r = rng01();        // a fixed-point value in [0, 1)
let i = rng_below(6);   // an integer in 0..6
```

Every client seeds the same generator identically and draws from it in the
same order, so the sequence is the same everywhere. Reading the clock, or
anything outside the world, would diverge instantly.

## Iteration order is defined

The engine's entity queries return ids in a defined, stable order — that is
what makes a loop over `entities()` or `entities_of(archetype)` safe to feed
into hashed decisions. (Internally the engine forbids the hash-map types
whose iteration order is unspecified, for exactly this reason.) Don't build
logic that depends on any order the API doesn't promise.

## Simulation versus presentation

Not everything a script does is hashed. The host API splits cleanly in two:

- **Simulation** — entities, their positions and fields, the RNG. This is
  the world state, and it *is* the hash. Every client must compute it
  identically.
- **Presentation** — models, sprites, the camera, lighting, sound, the HUD,
  and the local selection. None of it touches the hash.

That split is what makes presentation safe to diverge. A headless server or
the determinism harness runs a map with the render and audio calls turned
into no-ops and still computes the same state as a player who sees and hears
everything. It is also why the [local script layer](input.md) — hover,
tooltips, per-client UI — can never desync a match: it is presentation by
construction.

A useful habit: if a value affects what happens next in the world, it must
come from the simulation side. If it only affects what the player sees or
hears this frame, it belongs to presentation.

## Coordinates

Scripts work in **sim cells**: `x` and `y` across the map, `z` up, all
fixed-point. The renderer applies its own scale and camera on top, and — as
a rendering detail — mirrors one world axis, but a script never sees that:
you read and write positions in the same sim space you paint voxels in, and
the pick API hands cursor results back in that same space. Stay in sim
coordinates and the mirror is invisible.

## How it is enforced

You don't have to take determinism on faith. The `monada-oracle` harness
runs the engine's scenarios — and the maps that ship with it — on Linux,
macOS, and Windows in CI, hashing the world state at fixed tick checkpoints
and failing the build if any platform diverges by a single bit. The engine's
own crates additionally gate at compile time: floating-point arithmetic is
denied in the simulation core, and the unordered hash-map types are denied
across the workspace.

The examples in this book run under that same harness (headless, a few ticks
each), so every one is exercised for load-and-run correctness on every
platform. A map that follows the rules here inherits the same guarantee.
