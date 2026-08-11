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
| `atan2(y, x)` | fixed-point full-quadrant arctangent, radians (angle work: headings, wrapping an angle difference) |
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
| `nav_block(x, y, on)` | mark / clear a cell as impassable for navigation (building footprints, props) |
| `nav_path(x0, y0, x1, y1, max_step)` | a deterministic A\* path as an array of waypoint `vec3`s (`z` = ground height); steps climb at most `max_step`, an unreachable goal yields the closest approach |

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
| `pick_grid()` | the grid whose voxels the cursor ray meets first, or `-1` (requires `host_api` 24) |
| `pick_cell(grid)` | the sim cell of that hit in `grid`'s own cells (`-1` = the world grid), or `()` on a miss |
| `pick_face(grid)` | the hit's outward face normal in `grid`'s sim axes, so `pick_cell + pick_face` is the empty cell in front of it |
| `aim_yaw()` | the sim-space angle from the local player toward the cursor |
| `ui_clicks()` | the HUD button bits clicked since the last call (take-and-clear) |

## Overlay gizmos — *local*

World-space outlines drawn over the frame, in a grid's own frame and cells:
a placement ghost, a snap lattice, a range ring. Alpha-blended, which
nothing else a map draws can be — `ui_*` is flat HUD pixels, and a voxel is
opaque and a whole cell across. Immediate mode with the HUD's contract: the
map calls `gizmo_clear` and redraws; what it drew last stays on screen
through the frames between its ticks. Local layer only, and never hashed
(requires `host_api` 24).

| Function | Result |
|---|---|
| `gizmo_clear()` | start a fresh set; resets the style |
| `gizmo_style(width_px, on_top)` | line width, and whether segments ignore the depth buffer |
| `gizmo_box(grid, x0, y0, z0, x1, y1, z1, color)` | outline an inclusive cell box in `grid`'s frame (`-1` = the world frame) |
| `gizmo_line(grid, a, b, color)` | one segment between two sim points of `grid`'s frame |

`color` is `0xAA_RR_GG_BB` — here the high byte really is **alpha**, unlike
a voxel colour's (which is brightness).

## Models and sprites — *presentation*

| Function | Result |
|---|---|
| `model_box(w, h, d, color)` | define a procedural box sprite; returns a model id |
| `model_box_sides(w, h, d, x, neg_x, y, neg_y, z, neg_z)` | define a procedural box sprite with each of its 6 local faces a distinct colour; returns a model id (requires `host_api` 26) |
| `model_kv6(path, turns)` | define a sprite from a KV6 asset; returns a model id |
| `model_actor(path, states, height)` | define an animated 8-direction billboard; returns a model id |
| `model_character(path, height)` | define a rigged `.rkc` voxel character (skeleton + named clips); returns a model id, or `-1` if the asset is missing or unparsable (requires `host_api` 14) |
| `model_drop(model, cells)` | nudge an actor's or character's sprites down/up by `cells` |
| `entity_set_model(entity, model)` | bind an entity to a render model |
| `entity_set_grid(entity, grid)` | bind an entity to a `grid_spawn` grid so it rides that grid's transform (its position is read as grid-local); `-1` unbinds; unbound entities render in the global frame. Binding the fog observer also moves fog/`deck_clip` onto that grid |
| `entity_attach(entity, grid)` | bind *and* rewrite the position into that grid's frame, so the entity does not move in the world — stepping onto a hull. An entity already riding another grid hops straight across. Returns whether it happened (requires `host_api` 16) |
| `entity_detach(entity)` | the inverse: rewrite into world coordinates and unbind — stepping off. Returns whether it was riding anything |
| `entity_grid(entity)` | the grid an entity rides, or `-1` |
| `entity_set_anim(entity, state)` | set an entity's animation: an actor state, or a character's `.rkc` clip name (an unknown name keeps the current one) |
| `entity_set_facing(entity, yaw)` | set an entity's facing yaw (radians): an actor picks its directional sprite, a character turns its geometry |
| `entity_set_side(entity, dir, roll)` | set an entity's axis-aligned side and roll — a discrete alternative to `entity_set_facing` that can point a KV6/box model along any of the 6 grid faces (`dir`) with any of 4 quarter-turns around it (`roll`), not just a horizontal turn. `dir`/`roll` are `Direction`/`Roll` discriminants; it WINS over `entity_set_facing` when a map sets both, and a billboard actor ignores it (requires `host_api` 25) |
| `entity_set_tint(entity, tint)` | multiply an actor's sprite by a `0xRRGGBB` tint (billboard actors only) |

A **billboard actor** is pre-drawn art: eight GIF facings the renderer picks
from, on a card that stays upright, so a steep camera foreshortens it. A
**character** is a real voxel rig — [demiurg]'s `.rkc` container, holding the
meshes, the skeleton and every clip in one file — so it turns in world space
and holds its proportions at any camera angle. `height` is the rendered height
in sim cells, measured over the character's *first* clip (its idle) so a
sprawling death pose can't shrink it; pass `0` to keep the artist's scale, one
model voxel per world voxel. Both kinds animate and face through the same two
verbs above.

Whether a character's clip loops or holds its last frame is baked into the
clip itself (its keyframe sequence), not chosen by the map — so a death
animation that replays forever is fixed in the editor, not in the script.
Switching clips restarts the new one from its first frame.

[demiurg]: https://github.com/NCrashed/demiurg

## World painting — *presentation*

Paint the voxel world. `voxel_fill`/`voxel_set`/`tile_fill` also feed the
collision queries above.

| Function | Result |
|---|---|
| `voxel_fill(x0, y0, z0, x1, y1, z1, color)` | fill a solid box of voxels |
| `voxel_set(x, y, z, color)` | set one voxel |
| `voxel_clear(x, y, z)` | cut a column down: everything at and above `z` becomes air (render + collision + nav) |
| `tile(path)` | load a per-cell tile texture; returns a tile id, or `-1` |
| `tile_fill(x0, y0, z0, x1, y1, z1, tile)` | paint a cell region with a tile |
| `transition(low, high, path)` | register an autotile transition sheet |
| `terrain_fill(x0, y0, x1, y1, type)` | set the floor terrain type of a region |
| `terrain_blit(base_type)` | autotile-paint the floor from the terrain types set |

## Volume maps and physics — *simulation*

A map that declares `terrain = "volume"` in its manifest (requires a fixed
`sim_hz` and `host_api` 8) swaps the per-column heightmap for a chunked 3D
voxel store — tunnels and overhangs are first-class — and embeds a
deterministic rigid-body physics sim beside the entity world. The script
vocabulary stays the same, with deeper semantics:

- `voxel_fill`/`voxel_set` write the **hashed** volume store (and still
  paint the render grid); both accept an optional trailing *material id*
  argument (default `0`). `voxel_clear` hole-punches ONE cell — the tunnel
  primitive — instead of truncating a column.
- **The material-0 contract:** paints without a material id write material
  `0`, so the map's FIRST `phys_material` call is its ground material and
  must precede any tick that can bring a body into terrain contact. The
  same discipline covers every explicit material id: painting terrain
  with an id `phys_material` never returned panics at the first contact
  or drill sweep that touches the cell — a map-author bug surfaces
  loudly, not as a desync.
- Rendering is *isotropic*: one render voxel per sim cell on all three
  axes (the column convention's unscaled z cannot rotate with a rigid
  body). Physics bodies and their wheels are mirrored to the screen
  automatically — never hand-mirror a body with sprites or grids.

| Function | Result |
|---|---|
| `phys_gravity(gx, gy, gz)` | set gravity (the crate default is ZERO — set it in `init`) |
| `phys_material(density, friction, restitution, hardness)` | register a material; returns its id (first call = the ground material) |
| `phys_box(sx, sy, sz, mat, x, y, z)` | spawn a solid box body; CoM placed at `(x, y, z)`; returns a body id |
| `phys_shape(sx, sy, sz)` | open an empty body shape of `sx × sy × sz` cells; returns a shape handle (requires `host_api` 21) |
| `phys_shape_fill(shape, x0, y0, z0, x1, y1, z1, mat)` | fill an inclusive cell box of an open shape with a material (either corner first) |
| `phys_shape_clear(shape, x0, y0, z0, x1, y1, z1)` | empty a cell box — `phys_shape_fill`'s inverse, and how a shell is written: fill the block, clear the inside |
| `phys_body(shape, point)` | spawn a body from an open shape, its derived CoM placed at sim `point`; returns a body id. CONSUMES the shape — the handle is spent, and writing through it afterwards is a map bug |
| `phys_mass(body)` | the body's derived mass — the sum of its cells' densities, so a shell weighs its walls (ZERO for an unknown id) |
| `grid_body(grid, body)` | drive a `grid_spawn_cubic` grid's frame from a body: the map stops posing that grid, the grid's pivot becomes the body's centre of mass, and the body stops being auto-mirrored (the painted grid is its picture). `-1` releases it. Riders, props, fog, the deck cutaway and the camera follow the frame with no change (requires `host_api` 22) |
| `phys_wheel(body, ax, ay, az, rest, radius, k, c, mu)` | attach a wheel at SHAPE coords `(ax, ay, az)`; returns a wheel id |
| `phys_wheel_input(body, wheel, steer, drive, brake)` | set a wheel's retained steering/drive/brake |
| `phys_impulse(body, jx, jy, jz)` | apply an impulse at the CoM |
| `phys_thrust(body, anchor, dir, force)` | fire `force` for one tick at SHAPE-coordinate `anchor` along BODY-frame `dir` (auto-normalised; a zero-length one fires nothing) — the thruster primitive. Off the centreline it turns the ship as well as pushing it, and it follows the hull round as the hull turns (requires `host_api` 23) |
| `phys_torque(body, torque)` | apply `torque` for one tick as a pure world-frame couple — a gyro, a reaction wheel, an RCS quad: it turns without pushing, which an off-centre impulse cannot do |
| `phys_angvel(body)` | the body's angular velocity (rad/s, world frame; ZERO for an unknown id) — what a map's own stabiliser reads to write `phys_torque(body, -k*w)` |
| `phys_solid(x, y, z)` | whether the VOLUME store holds a solid cell there — the deterministic terrain read on volume maps (the column `voxel_solid` reads an empty world here by design) |
| `phys_pos(body)` | the body's CoM position (ZERO for an unknown id) |
| `phys_vel(body)` | the body's linear velocity (ZERO for an unknown id) |
| `phys_yaw(body)` | the body's heading about +z, radians (the chase-cam read) |
| `phys_pitch(body)` | the body's attitude pitch, radians, positive = nose up (subtract from a commanded drill pitch for a gravity-stable bore) |
| `phys_drill_tool(body, ax, ay, az, hx, hy, hz)` | register the body's drill box: anchor at SHAPE coords `(ax, ay, az)`, half-extents `(hx, hy, hz)`; call once at spawn |
| `phys_drill(body, pitch, budget)` | one drill sweep: overlapped terrain cells cut front-to-back while their summed hardness fits `budget`, carved from the store (with the physics wake/reaction and the render mirror), `pitch` tilts the nose (radians, positive = up); returns voxels cut |
| `phys_material_color(mat, color)` | bind a render colour (`0xBB_RR_GG_BB`) to a material id — the automatic body mirror paints that material's voxels with it (render-side; the engine palette is the fallback) |
| `body_deco_box(body, x0, y0, z0, x1, y1, z1, color)` | render-only trim on the body mirror, in FINE voxels (16 per cell), shape-local — skirts, fenders, a cockpit; rides the physics pose, never enters the hashed shape |
| `drill_indicator(body, pitch, spinning)` | drive the body's drill-cone telltale: tilted by `pitch` like the bore, spinning while `spinning`; call every tick like the camera verbs |

All `phys_*` state — bodies, wheels, materials, the volume terrain — folds
into the desync hash alongside the entity world; treat it exactly like
entity state (deterministic inputs only).

## Dynamic grids — *presentation*

Spawn and paint additional voxel grids independent of the world grid (e.g. ships, moving platforms). Render-only: dynamic grids do not feed collision. A grid handle is render-side — never store it in `World` state or hashed `tick()` logic. A `< 0` handle means the host lacks multi-grid support (requires `host_api` 7).

**Cell shape.** A `grid_spawn` grid keeps the world grid's *column* cell: `SCALE×SCALE×1` voxels, so sim z is unscaled and a cell is a thin slab. A `grid_spawn_cubic` grid makes the cell a cube (`SCALE³` voxels), so z scales like x/y. The shape belongs to the grid: `voxel_fill_in`, `grid_pivot`, the seat of an entity bound with `entity_set_grid`, and `deck_clip` all follow the grid they are given.

A map whose entities ride a moving grid usually wants `camera_grid` too (see the camera table): those entities' positions are grid-local, so a world-fixed camera has view-relative input steering in the grid's frame while the player watches the world's — "forward" points somewhere new every time the grid turns.

Prefer the cubic grid whenever the map turns a grid about anything but the vertical, or wants to convert a point between the grid and the world: sim→world on a column cell is anisotropic, and only a rotation about z survives it — a tilted turn renders honestly but is not the rotation the script asked for. The cubic cell makes that map a similarity transform, so every orientation is exact. The trade is that vertical geometry is cell-quantised there: a wall is a whole number of cells tall, and the finest stair step is one cell.

| Function | Result |
|---|---|
| `grid_spawn(wx, wy, wz)` | spawn a grid offset by sim cell `(wx, wy, wz)` from the world origin (`(0,0,0)` = world origin); returns a grid handle (i64), or `< 0` if unsupported |
| `grid_spawn_cubic(wx, wy, wz)` | the same, but the grid's cells are CUBES — sim z scales like x/y inside it, so any 3D orientation is exact and hull-local and world points can be converted (requires `host_api` 15) |
| `voxel_fill_in(grid, x0, y0, z0, x1, y1, z1, color)` | fill a solid box of voxels in the given dynamic grid (same coords as `voxel_fill`) |
| `grid_orient(grid, axis, angle)` | turn a grid to a 3D orientation: `angle` radians about the (auto-normalised) `axis` in SIM coordinates (+z up), replacing its rotation — entities riding it and its fog/`deck_clip` follow; a zero-length axis is ignored |
| `grid_pivot(grid, point)` | the grid-local sim-cell `point` `grid_orient` turns the grid about — a hull spanning cells `0..=19` turns in place about `9.5`, not about the corner its local origin sits on. Sticky; call once at spawn |
| `grid_move(grid, point)` | move the grid to sim-space `point`, replacing `grid_spawn`'s offset — a hull under way. Fixed-point, so a hull can drift a fraction of a cell per tick; riders and fog follow (requires `host_api` 16) |
| `grid_despawn(grid)` | retire the grid: its voxels leave the scene and the handle dies for good (handles are never reused, so a stale one is inert). Riders are **detached alive**, keeping their world pose — killing them is the map's call, not the renderer's |
| `voxel_set_in(grid, x, y, z, color)` | paint one cell of a dynamic grid |
| `voxel_clear_in(grid, x, y, z)` | erase one cell — `voxel_fill_in`'s inverse, the door / hull-breach primitive. Render-only: a dynamic grid still feeds no collision, so the map must open its own passability rule too |
| `vision_observer(entity, grid)` | fog/`deck_clip` overload that rides the given dynamic grid instead of the world grid (movable hull). Names the fog's grid only — it does not bind the entity, and an observer bound via `entity_set_grid` fogs the grid it rides instead |

## Grid frames — *simulation* (reads: any layer)

A grid's *voxels* are presentation, but its **frame** — where it sits, what it
is turned to, who rides it — is kept a second time in fixed-point, so a map may
convert points between a grid and the world and act on the answer. The frame is
a pure function of the map's own deterministic calls, so every peer computes the
same one: results may steer `tick()`, exactly like `voxel_solid` or `nav_path`.

The reads below are registered in both layers (the local layer uses them to turn
a cursor hit into a hull cell); the verbs that *move* a hull or re-seat an entity
are simulation-only.

| Function | Result |
|---|---|
| `grid_world(grid, point)` | a grid-local sim point in world coordinates; an unknown or despawned handle converts as the identity |
| `grid_local(grid, point)` | the inverse — a world point in the grid's frame |
| `grid_riders(grid)` | every entity riding the grid, ascending |

Conversion is exact to fixed-point rounding, not bit-exact. Convert at the
moments that mean something — a crew member steps off the hull, an item is
dropped — rather than round-tripping every tick, which would integrate that
rounding into a drift.

On a column-cell grid the frame is only *drawn* faithfully for a rotation about
the vertical (see the cell-shape note above), so a map that converts coordinates
through a tilted hull wants `grid_spawn_cubic`.

**A pose is written once a tick and drawn over that tick.** On a real-time map
(`sim_hz = "30hz"` and friends) `grid_move` / `grid_orient` / `grid_pivot` set a
*target*: the renderer eases the grid onto it across the tick that follows, so a
hull turning 0.02 rad per tick reads as motion rather than as a 30 Hz staircase,
and everything composed against that frame — riders, props, actor facings, the
fog cone, the deck cutaway, the camera — moves with it as one piece. What the
map *reads* is unaffected: `grid_world` / `grid_local` and every hashed decision
see the tick-exact frame, never the eased one. Poses authored during `init` land
whole, as does any single step big enough to be a re-authoring rather than a
tick of motion (more than two cells of travel, or more than a quarter-turn) —
so a dock snap or a jump drive still snaps.

## Camera, lighting, sky — *presentation*

| Function | Result |
|---|---|
| `camera_focus(point)` | aim the camera at a sim-space point |
| `camera_focus_entity(entity, point)` | aim the camera at an entity's `point`, composed through the grid the entity rides (tracks a crew member on a moving/rotating hull) |
| `camera_angle(yaw, pitch)` | set the camera's orbit angles (radians) |
| `camera_dist(dist)` | set the camera's distance from its focus |
| `camera_pan(dx, dy)` | shift the camera focus by a sim-space delta — an RTS-style scroll, accumulated host-side |
| `camera_cutout(radius, feather)` | dissolve geometry between the camera and its focus inside a keyhole (sim cells; `radius <= 0` off) |
| `camera_grid(grid)` | make the camera RIDE a dynamic grid: its orbit frame turns with that grid, so the grid holds still on screen and the world sweeps past; `-1` returns it to the world frame (requires `host_api` 17) |
| `deck_clip(z_lo, z_hi)` | show only the sim-z band `z_lo..=z_hi`, cutting the ceiling above it away |
| `vision_observer(entity)` | declare the local fog-of-war viewpoint (per-client); `-1` clears it |
| `vision_config(cone_deg, range, peripheral)` | tune the observer's vision cone / reach / peripheral radius (cells) |
| `vision_hear(x, y, z, loudness)` | briefly reveal a cell from a heard sound (`0..1`) |
| `set_light(dir, intensity)` | declare the directional "sun" |
| `set_sky(path)` | load a sky panorama from an asset |

## Selection and status — *presentation*

Per-client UI state — never networked or hashed.

| Function | Result |
|---|---|
| `highlight(entity)` | mark an entity as locally selected (replaces the selection) |
| `highlight_add(entity)` | add an entity to the selection (multi-select) |
| `highlight_clear()` | clear the local selection |
| `highlighted()` | the (first) selected entity, or `-1` |
| `highlighted_all()` | every selected entity, ascending |
| `drag_begin()` | anchor a ground-space drag rectangle at the cursor; the host draws it until `drag_end` |
| `drag_end()` | finish the drag: `[anchor, end]` sim points, or `[]` if none was active |
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
