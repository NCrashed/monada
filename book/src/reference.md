# API reference

Every function a map script can call, grouped by layer. The **layer** tells
you where a function may be used and whether it is hashed:

- **Simulation** functions run in the sim scope (`init`, `tick`, `command`)
  and their effects are part of the world state — every client must compute
  them identically. See [the determinism contract](determinism.md).
- **Local** functions run only in the [local layer](input.md) (`local_tick`,
  `action`, `pointer`, …); they read input and the world but never mutate it.
- **Presentation** functions affect what the player sees or hears and are not
  hashed, so a headless run turns most of them into no-ops. World painting is
  the exception: its colours are presentation, but the *solidity* it creates
  feeds the deterministic collision queries, so a headless run still applies
  it (see the *World painting* section below).
- **Any** functions (numbers and math) are pure and usable anywhere.

Several queries are available in **both** the simulation and local scopes —
reading the world to drive gameplay, or to drive per-client UI. Those rows
carry a `Layer` column; where a whole section shares one layer it is named in
the heading.

Coordinates are sim cells (`x`, `y` across the map, `z` up); numbers are
fixed-point.

<!-- The `api-coverage` test in monada-oracle checks this list against the
functions monada-script actually registers, so a new or removed host
function must be reflected here. Keep every entry as a table row whose first
cell is the function name in backticks. -->

## Numbers and math — *any*

Pure, deterministic value helpers. Fixed-point is the only numeric type in a
script (there are no float literals).

| Function | Result |
|---|---|
| `fixed(i)` | the integer `i` as a fixed-point number |
| `ratio(n, d)` | the fraction `n/d` as fixed-point |
| `vec3(x, y, z)` | a fixed-point 3-vector (components read as `v.x`/`v.y`/`v.z`) |
| `to_int(a)` | floor `a` to an integer (for board coordinates, tags) |
| `floor(a)` | round `a` down to a whole fixed-point value |
| `ceil(a)` | round `a` up |
| `round(a)` | round `a` to nearest |
| `to_debug(a)` | a debug string for a fixed-point value (diagnostics) |
| `sin(a)` | fixed-point sine of `a` radians |
| `cos(a)` | fixed-point cosine of `a` radians |
| `tau()` | the constant τ (2π) |
| `pi()` | the constant π |
| `pi_2()` | the constant π/2 |

## Entities

The world's hashed state: entities, their positions, and their named fields.
Mutators run only in the simulation; the read queries are also available in
the local layer (for selection, tooltips, and the like).

| Function | Layer | Result |
|---|---|---|
| `archetype(fields)` | sim | declare an archetype with the given field names; returns its id |
| `entity_create(archetype)` | sim | spawn an entity of `archetype`; returns its id |
| `entity_despawn(entity)` | sim | remove an entity; returns whether it existed |
| `entity_set_position(entity, pos)` | sim | set an entity's position (`Vec3`, sim cells) |
| `entity_set_field(entity, name, value)` | sim | set a named fixed-point field |
| `entity_position(entity)` | sim + local | an entity's position, or the zero vector |
| `entity_field(entity, name)` | sim + local | read a named field, or zero |
| `entities()` | sim + local | ids of every entity, in a defined order |
| `entities_of(archetype)` | sim + local | ids of one archetype, ascending |

## Randomness — *simulation*

The world's seeded generator — the only source of randomness a map may use.

| Function | Result |
|---|---|
| `rng01()` | a fixed-point value in `[0, 1)` |
| `rng_below(n)` | an integer in `0..n` |

## Collision queries — *simulation + local*

Deterministic queries over the voxels the map has painted (a pure function of
those paint calls, so safe to feed hashed decisions). Available in both
scopes — the simulation gates movement on them, and the local layer can use
them too.

| Function | Result |
|---|---|
| `voxel_solid(x, y, z)` | whether a cell is solid |
| `ground_height(x, y)` | the highest solid `z` in a column, or `0` |

## Command routing

The bridge between the local layer and the simulation.

| Function | Layer | Result |
|---|---|---|
| `submit_command(verb, target, arg)` | local | queue a command for the host to route into the tick stream |
| `local_player()` | any | the local player's id, or `-1` when there is no single one (hotseat) |

`submit_command` belongs to the local layer by convention: it is where input
becomes a command. It is technically reachable from the simulation scope too,
but submitting a command from inside the tick is a bug (it injects
non-deterministic input mid-simulation). The input queries below, by
contrast, are registered *only* in the local layer, so the simulation cannot
reach them at all.

## Local input — *local*

Read declared actions and the cursor. Registered only in the local layer, so
the simulation can never observe raw input. Every result is a sim type, ready
to place in a command payload.

| Function | Result |
|---|---|
| `action_down(id)` | whether a `button` action is held |
| `action_axis(id)` | an `axis` action's value: `-1`, `0`, or `+1` |
| `action_axis2(id)` | an `axis2` action's value as a `Vec3` (`x`, `y`, `0`) |
| `pick_ground()` | the cursor's ground point (`Vec3`), or `()` on a miss |
| `pick_entity()` | the entity under the cursor, or `-1` |
| `aim_yaw()` | the sim-space angle from the local player toward the cursor |
| `ui_clicks()` | the HUD button bits clicked since the last call (take-and-clear) |

## Models and sprites — *presentation*

| Function | Result |
|---|---|
| `model_box(w, h, d, color)` | define a procedural box sprite; returns a model id |
| `model_kv6(path, turns)` | define a sprite from a KV6 asset; returns a model id |
| `model_actor(path, states, height)` | define an animated 8-direction billboard; returns a model id |
| `model_drop(model, cells)` | nudge an actor model's sprites down/up by `cells` |
| `entity_set_model(entity, model)` | bind an entity to a render model |
| `entity_set_anim(entity, state)` | set an actor entity's animation state |
| `entity_set_facing(entity, yaw)` | set an actor entity's facing yaw (radians) |
| `entity_set_tint(entity, tint)` | multiply an actor's sprite by a `0xRRGGBB` tint |

## World painting — *presentation*

Paint the voxel world. `voxel_fill`/`voxel_set`/`tile_fill` also feed the
collision queries above.

| Function | Result |
|---|---|
| `voxel_fill(x0, y0, z0, x1, y1, z1, color)` | fill a solid box of voxels |
| `voxel_set(x, y, z, color)` | set one voxel |
| `tile(path)` | load a per-cell tile texture; returns a tile id, or `-1` |
| `tile_fill(x0, y0, z0, x1, y1, z1, tile)` | paint a cell region with a tile |
| `transition(low, high, path)` | register an autotile transition sheet |
| `terrain_fill(x0, y0, x1, y1, type)` | set the floor terrain type of a region |
| `terrain_blit(base_type)` | autotile-paint the floor from the terrain types set |

## Camera, lighting, sky — *presentation*

| Function | Result |
|---|---|
| `camera_focus(point)` | aim the camera at a sim-space point |
| `camera_angle(yaw, pitch)` | set the camera's orbit angles (radians) |
| `camera_dist(dist)` | set the camera's distance from its focus |
| `camera_pan(dx, dy)` | shift the camera focus by a sim-space delta — an RTS-style scroll, accumulated host-side |
| `camera_cutout(radius, feather)` | dissolve geometry between the camera and its focus inside a keyhole (sim cells; `radius <= 0` off) |
| `deck_clip(z_lo, z_hi)` | show only the sim-z band `z_lo..=z_hi`, cutting the ceiling above it away |
| `set_light(dir, intensity)` | declare the directional "sun" |
| `set_sky(path)` | load a sky panorama from an asset |

## Selection and status — *presentation*

Per-client UI state — never networked or hashed.

| Function | Result |
|---|---|
| `highlight(entity)` | mark an entity as locally selected |
| `highlight_clear()` | clear the local selection |
| `highlighted()` | the selected entity, or `-1` |
| `status(text)` | set the HUD status line |

## Audio — *presentation*

Identical one-shots fired the same frame are de-duplicated by the host.

| Function | Result |
|---|---|
| `play_sound(path)` | play a one-shot sound |
| `play_sound_gain(path, gain)` | play a one-shot at an explicit gain |
| `play_blip(wave, freq, dur_ms, gain)` | synthesise a short "voice" blip |
| `play_loop(path)` | keep a looping sound audible this tick (footsteps) |
| `play_music(path)` | start or replace the background track |
| `stop_music()` | stop the background track |

## HUD — *presentation*

An immediate-mode overlay: clear it and re-issue the widgets each tick.
Positions are screen points from the top-left.

| Function | Result |
|---|---|
| `ui_texture(path)` | register a HUD texture; returns an id, or `-1` |
| `ui_gif(path)` | register an animated HUD image; returns an id, or `-1` |
| `ui_anim(gif, x, y)` | draw an animated image's current frame |
| `ui_image(tex, x, y)` | draw a texture |
| `ui_image_clip(tex, x, y, frac)` | draw a texture clipped to its left `frac` (health bars) |
| `ui_text(x, y, text, size)` | draw a line of text |
| `ui_text_wrap(x, y, text, size, width, color)` | draw word-wrapped text |
| `ui_button(tex, hover, pressed, x, y, bit)` | draw an image button; a click OR-s `bit` into the input |
| `ui_width()` | the viewport width in points, or `0` |
| `ui_height()` | the viewport height in points, or `0` |
| `ui_scale(factor)` | uniform scale for HUD draws this frame |
| `ui_clear()` | begin a fresh HUD frame |
| `ui_emit_event(code, a, b, c)` | push a render-side UI event for the host |
