# Input, actions, bindings

Player input in monada takes a deliberate detour. Physical keys and mouse
buttons never reach the simulation directly; instead they resolve to named
*actions*, a per-client *local script layer* turns those actions into
[commands](commands.md), and only the commands enter the synchronized world.
That detour is what lets players rebind keys freely, lets a replay ignore
which keys were pressed, and keeps raw input from ever desyncing a match.

This chapter's example is
[`book/examples/02-top-down-mover/`](https://github.com/NCrashed/monada/tree/master/book/examples/02-top-down-mover):
WASD walks a cube around a floor, and a click drops a marker under the
cursor.

```console
$ cargo run --release -p monada-host -- --map book/examples/02-top-down-mover
```

## Declared actions

A map names the inputs it wants in its manifest, as `[[action]]` tables:

```toml
[[action]]
id = "move"
kind = "axis2"
default = { up = "KeyW", down = "KeyS", left = "KeyA", right = "KeyD" }
label = { en = "Move" }

[[action]]
id = "mark"
kind = "button"
default = ["MouseLeft"]
label = { en = "Place marker" }
```

An action has a `kind` — `button` (pressed or not), `axis` (a `+`/`−`
pair), or `axis2` (a four-way pad) — and a `default` binding in the matching
shape. The host resolves the actual keys through its binding table, so a
player can rebind any of them (press `F2` in the host for the rebind panel,
or edit `bindings.toml`); the map only ever refers to actions by `id`.

## The local layer

The map's script has two scopes. The simulation scope you have already met —
`init`, `tick`, `command`. The *local* scope runs alongside it, once per
client, and handles everything that must not be synchronized: input, camera,
selection, UI. It has its own entry points:

| Entry point | When it runs |
|---|---|
| `local_init()` | once, after the sim's `init` |
| `local_frame(dt)` | every rendered frame — hover, tooltips, camera |
| `local_tick(dt)` | every simulation tick — assemble per-tick input |
| `action(id, down)` | on a press/release of a declared action |
| `pointer(button, point, entity)` | on a click (the classic select-then-act gesture) |

The wall between the two scopes is enforced by what each can call: the local
layer can read the world and query input, but it cannot mutate the world or
advance the shared RNG. Its only channel into the simulation is
`submit_command`, which queues a command the host routes into the lockstep
stream like any other input.

Here is the example's local layer:

```rust,ignore
{{#include ../examples/02-top-down-mover/scripts/main.rhai:local}}
```

Two patterns show up here:

- **Continuous input** is polled in `local_tick` and sent as one command per
  tick. `action_axis2("move")` returns a `Vec3` already in simulation units,
  so it drops straight into the command payload.
- **Discrete input** is handled as it happens. On a `mark` press, the map
  asks the pick API where the cursor is pointing. `pick_ground` returns `()`
  when the ray misses the world; the example tells a hit from a miss with
  `type_of(hit) == "Vec3"`, which is the reliable way to match a value type
  like `Vec3` rather than comparing it against `()`.

## The pick API

Working out what the cursor is over is a first-class operation, not something
a map has to reconstruct. Every pick function returns a *simulation* value —
fixed-point coordinates, a cell, or an entity id — so its result can become a
command payload without any float leaking across the wall:

| Function | Returns |
|---|---|
| `pick_ground()` | the ground point under the cursor (`Vec3`), or `()` on a miss |
| `pick_entity()` | the entity under the cursor, or `-1` |
| `pick_grid()` | the grid the cursor ray meets first, or `-1` |
| `pick_cell(grid)` | the sim cell of that hit, in `grid`'s own cells, or `()` |
| `pick_face(grid)` | that hit's outward face normal, in `grid`'s sim axes |
| `aim_yaw()` | the sim-space angle from the local player toward the cursor |

Hover highlighting and tooltips build on `pick_entity` in the local layer;
`aim_yaw` gives a twin-stick attack direction (feed it as a command's
`arg.z`, as the action-RPG does) — all without ever touching the simulation.

### Ground, or geometry

`pick_ground` answers about a *plane*: the ground under the cursor, in world
coordinates, at `z = 0`. That is the right question for a board or an arena,
and the wrong one as soon as a map's world has a shape — a two-deck ship
has two floors at different heights, and it is a rigid body, so there is no
world plane its cells can be named in at all.

`pick_cell` asks the scene instead: which voxel of which grid does the ray
actually hit, and which cell of *that grid's own* convention is it. The
answer comes back in the same numbers the map painted with, so a hull
authored in hull cells is addressed in hull cells at any attitude:

```rhai
let cell = pick_cell(hull_grid());
if cell != () {
    // `cell` is a hull cell — the same coordinates `voxel_fill_in` took.
    // `cell + pick_face(hull_grid())` is the empty cell in front of the
    // surface, which is where a crate goes rather than into the wall.
}
```

Two properties worth knowing:

- It is **clip-aware**. Voxels the deck cutaway (`deck_clip`) hides read as
  air, so the cursor lands on the deck the player is looking into rather
  than on the roof that was cut away to let them look.
- It resolves against the pose on screen this frame, while the simulation
  acts on the tick-exact one — up to a tick of cursor offset on a hard-
  accelerating hull, the same asymmetry the camera already lives with.

To *show* where that lands, see the overlay gizmos in the
[reference](reference.md): outlines in a grid's frame, with real alpha.

## Into the simulation

The command handler is unchanged from any other map — it is the one place
input becomes world state, and where a networked map validates it:

```rust,ignore
{{#include ../examples/02-top-down-mover/scripts/main.rhai:command}}
```

And the tick integrates the stored intent, scaling by `dt` so movement stays
rate-independent, with a small `clamp_cell` helper to keep the cube on the
floor:

```rust,ignore
{{#include ../examples/02-top-down-mover/scripts/main.rhai:tick}}
```

Notice the round trip: a key press became an *action*, the local layer turned
it into a *command*, and only that command — never the key — reached the
world. Change the binding and nothing downstream notices; that is the whole
point.
