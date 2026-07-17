# Hello, voxels

The smallest complete map is a floor and one moving cube. This chapter
walks through its entire source, which lives at
[`book/examples/01-hello-voxels/`](https://github.com/NCrashed/monada/tree/master/book/examples/01-hello-voxels).
Run it with:

```console
$ cargo run --release -p monada-host -- --map book/examples/01-hello-voxels
```

The manifest declares a single-player, real-time map (from the
[previous chapter](anatomy.md)); all the interesting parts are in
`scripts/main.rhai`.

## `init`: build the world once

The host calls `init` exactly once, before the first tick. This is where the
map paints its world and spawns its entities.

```rust,ignore
{{#include ../examples/01-hello-voxels/scripts/main.rhai:init}}
```

A few things worth naming:

- **Coordinates are sim cells.** `x` and `y` run across the floor, `z` is
  up. The engine scales cells to world voxels for rendering; the script
  never deals in pixels.
- **Numbers are fixed-point.** `fixed(1)` is the integer 1; `ratio(3, 4)` is
  three-quarters. There are no floating-point literals — the
  [determinism chapter](determinism.md) explains why.
- **State lives in the World, not the script.** `entity_create` returns an
  id; `entity_set_position` and `entity_set_model` act on it. The script
  holds no entity state of its own between calls.
- **`set_light` and `model_box` are presentation.** They shape what you see
  but never affect the simulation, so a headless run (the tests, a
  dedicated server) simply ignores them.

## `tick`: advance the world each step

Because the map declared `sim_hz = "30hz"`, the host calls `tick(dt)` thirty
times a second. `dt` is the step duration as a fixed-point number.

```rust,ignore
{{#include ../examples/01-hello-voxels/scripts/main.rhai:tick}}
```

Advancing from `dt` rather than adding a fixed amount per tick keeps the
motion tied to real time: at a different `sim_hz` the cube still crosses the
board at one cell per second. `entities()` returns every entity in the
world; with only one spawned, `entities()[0]` is our cube.

That is a whole map. The next chapters add real art, rules, input, and
networking — but every one of them is this same shape: `init` to set up,
`tick` or `command` to advance, the World holding all the state.
