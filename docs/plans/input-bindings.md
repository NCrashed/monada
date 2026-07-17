# Plan: actions, key bindings, and the local script layer

Status: steps 1–5 landed, plus most of step 4. Done: binding resolver +
`bindings.toml` (host key hardcode removed); manifest `[[action]]` +
per-map config; the sim/local backend split (`LocalBackend`:
`local_init` / `local_frame(dt)` / `local_tick(dt)` / `action` /
`pointer`; sim backend lost `on_pointer`/`on_key`); pick API
(`pick_ground` / `pick_entity` / `aim_yaw` / `ui_clicks`); rpg map
migrated to declared actions + `local_tick` packing; chess unchanged;
all goldens pass un-re-blessed. **Step 5 (rebind UI):** an egui panel
(base action `ui.bindings`, default F2) lists every slot — base actions
plus each map action's parts — grouped by context; click a row then
press a key to rebind (a key means one thing per context, so a conflict
displaces the other slot), per-slot and global reset-to-default, and
writes `bindings.toml` on every change. `Bindings` gained
`slots`/`rebind`/`reset`/`reset_all`/`is_modified`/`to_toml`/`save`
(unit-tested, incl. an axis2 round-trip). The panel dims rows that are
inert for the running map — a context not in the active stack, or a key
a higher context wins — so a no-effect rebind isn't offered; Esc closes
it; a displaced key is announced. Remaining: `pick_voxel` + `cursor()`
(deferred until a consumer map), step 6 script-driven contexts +
`cursor_mode`, gamepad, and **optional-part axes in `monada-format`** —
today a rebind that strands one pole of a map axis can't be saved (the
`ActionDefault` shape needs every pole), so it reloads at the manifest
default and the saved state diverges from the session (see `rebind`'s
doc note). Companion to DESIGN.md §3.3 (script triggers) and §3.1
(lockstep). Terminology and constraints here are
grounded in the current code; file references are as of this writing.

## 0. Problem and core idea

Today the host hardcodes physical keys (WASD/Space/arrows in
`monada-host/src/main.rs`), packs the per-tick `VERB_INPUT` command
itself, and owns picking (`MapRender::pick`, ground-plane only). Maps
cannot declare their own inputs, users cannot rebind, and the host
still "knows" about concrete maps — a hole in the M4-S3 agnostic-host
seam.

We separate three concepts that are currently fused:

- **Physical input** — scancodes, mouse buttons, cursor position.
  Lives only in the host (winit). Never reaches scripts as-is.
- **Action** — a named input with a kind (`button` / `axis` /
  `axis2`). This is what key bindings map to. Lives in the host and
  in the map's *local* script layer. Never enters the simulation.
- **Sim command** — the existing `Command { verb: u32, target:
  EntityId, arg: FixedVec3 }` in the lockstep stream
  (`monada-sim/src/command.rs`). The *only* input the simulation ever
  sees. **The wire format does not change** — replays and oracle
  goldens stay valid.

Pipeline:

```
scancode → [binding resolver] → action → [map's local script]
         → submit_command → lockstep → command(player, verb, target, arg)
```

The WC3 lesson: WC3 maps had no access to the mouse and no channel
from local computation into synced orders, which forced hacks like a
sphere of invisible units around the camera to detect hover. Our
answer: the mouse is neither smuggled into the sim nor forbidden —
the local layer gets a first-class pick API (cursor → ray →
voxel/entity) whose results are *already quantized to sim types*
(`FixedVec3`, entity id, cell coords) and ready to become command
payloads. Hover/tooltips stay purely local.

## 1. Local script layer

Today everything runs in one Rhai scope: the chess `pointer()`
handler drives a local selection FSM in the same globals where sim
logic lives. Nothing mechanically prevents a sim handler from reading
local state → a desync that only surfaces on goldens.

Two backend instances over the same map:

- **Sim backend** — as today: `init()`, `tick(dt)`,
  `command(player, verb, target, arg)`. API: deterministic functions
  (world, rng, `voxel_solid`, `ground_height`) plus presentation
  calls (sound/anim — fire-and-forget, unhashed, identical on every
  client because command handlers run everywhere).
- **Local backend** — its own global scope, its own entry points:
  - `local_init()` — camera, UI, cursor mode;
  - `local_tick(dt)` — once per sim tick on the local client:
    action polling, hover, per-tick input assembly;
  - `action(id, phase)` — edge events (`pressed` / `released` /
    `repeat`);
  - `pointer(button, point, entity)` — kept for compatibility (host
    performs the default ground pick, as today);
  - `text(str)` — chat/text entry while a text context is active.

  Local API surface: all presentation calls, **exclusively** input
  (`action_down`, `action_axis2`, `cursor*`, `pick_*`, context
  stack), `submit_command`, `local_player`, and *read-only* world
  access (entity positions/fields at the last executed tick — for
  selection rendering and tooltips).

Manifest gains optional `local_entry = "scripts/local.rhai"`; when
absent, the local backend loads the same file as the sim (shared
helper functions, but **separate globals** — document loudly). The
guarantee is mechanical: sim script physically cannot call
`cursor()`, local script cannot mutate the world. The "UI state
leaked into the sim" bug class dies at the API level, not in code
review.

`on_pointer` / `on_key` are removed from the sim backend (`on_key`
is dead already — the host never calls it).

## 2. Action registry: engine base set + map declarations

**Engine base set** (available to every map out of the box) — exactly
what `main.rs` hardcodes today, but as bindable actions:
`pointer.primary/secondary`, `camera.orbit_*`, `camera.zoom_*`,
`ui.menu` (Esc), `ui.debug_hud` (F1), `replay.pause` /
`replay.speed_*`, `ui.chat`.

**Maps declare extra actions in the manifest**, declaratively — so
the host can build the rebind UI and validate config without running
scripts:

```toml
[[action]]
id      = "move"
kind    = "axis2"                     # button | axis | axis2
default = { up = "KeyW", down = "KeyS", left = "KeyA", right = "KeyD" }
label   = { en = "Move", ru = "Движение" }

[[action]]
id      = "dodge"
kind    = "button"
default = ["Space"]

[[action]]
id      = "cast_fireball"
kind    = "button"
default = ["KeyQ"]
context = "gameplay"
```

The manifest is inside the archive SHA-256, so changing actions
changes `map_hash` — correct, actions are part of the map's contract.

**Binding resolution**: user config > map defaults > engine defaults.
Key conflicts are detected within a single context.

## 3. Input contexts

A context stack (`gameplay` → `menu` → `targeting`); bindings resolve
top-down with fall-through. The local script drives it:
`input_push_context("targeting")` / `input_pop_context()`.

This yields WC3-style spell targeting without hacks: `cast_fireball`
pressed → local script pushes `targeting`, swaps the cursor; the next
`pointer.primary` is consumed by that context → `pick_ground()` →
`submit_command(VERB_CAST, spell, point)` → pop. The sim sees only
the final command with a quantized point.

Cursor modes belong here too: `cursor_mode("free" | "locked")`; in
`locked` the relative mouse feeds an `axis2` action (groundwork for
FPS-like maps).

## 4. Pick API — cursor handling done right

Picking moves from the hardcoded `MapRender::pick` (ray-plane at
`GROUND_Z` + nearest entity) into an API the local script calls:

```
cursor()       -> (px, py)              // screen space, for UI
pick_ground()  -> FixedVec3 | ()        // cursor ray vs ground plane (current behavior)
pick_entity()  -> entity_id | -1        // nearest entity on the ray (today: PICK_RADIUS)
pick_voxel()   -> (x, y, z, face) | ()  // DDA through the voxel grid — terrain/building maps
```

Key invariant: **all pick functions return sim types** (fixed-point,
integer cells, entity ids) — results are valid `Command.arg`/`target`
by construction; a float from the local layer cannot leak into the
sim.

Hover (highlight, tooltips) is a purely local loop: `local_tick` →
`pick_entity()` → `highlight()` / `ui_text()`. It never touches the
sim. The hardcoded `scene.hover()` in the host's `redraw()` goes
away.

If a map wants to show allied cursors (co-op), that is opt-in via
ordinary commands with a rate limit (every N ticks); the engine does
nothing special.

## 5. Real-time input: per-tick assembly moves into the map

Today the host itself knows about WASD/dodge/attack and packs
`VERB_INPUT` (`main.rs`). In the new scheme the rpg map's local
script does the packing:

```rust
fn local_tick(dt) {
    let mv = action_axis2("move");                    // already fixed-point
    let btns = action_down("attack") | action_down("dodge") << 1;
    submit_command(VERB_INPUT, btns, vec3(mv.x, mv.y, aim_yaw()));
}
```

The map defines its own bitmask encoding — it owns both ends
(`local_tick` and the `command` handler). The host loses its last
map-specific knowledge. Two emission modes coexist: edge events
(`action` handler — discrete orders, chess) and polling in
`local_tick` (continuous input, rpg); latency is absorbed by the
existing lockstep `command_delay`.

## 6. Trust and validation

Command payloads are client data; clients have zero authority. Engine
guarantee: lockstep tags each command with its sender, so `player` in
`command(player, ...)` cannot be spoofed. Everything else is the sim
layer's job (chess already lives this way: full move-legality
validation in `command`). The map-making docs get an explicit rule —
"validate target ownership and range in the command handler" — and
the engine should provide an `entity_owner(id)` helper.

Pleasant consequences: replays store only commands → rebinds /
sensitivity / resolution do not affect playback; an observer client
gets the full local layer (hover, camera, inspection) while its
`submit_command` is dropped for lack of a player slot.

## 7. Host config and rebind UI

There is no persisted host config today. Introduce
`~/.config/monada/bindings.toml`:

```toml
[global]                # overrides for engine base actions
"camera.zoom_in" = ["KeyE"]

[map."Chess"]           # keyed by map name, not hash: rebinds survive map updates
"cast_fireball" = ["KeyR"]
```

The rebind screen is a generic egui panel in the host: lists
contexts/actions from the registry (labels from the manifest),
press-a-key capture, conflict check, reset-to-default, save. Maps
write nothing for this.

## 8. Build order

1. **Binding resolver in the host** for base actions + read/write of
   `bindings.toml`. The hardcoded keys in `main.rs` die. No visible
   behavior change.
2. **`[[action]]` in the manifest** (monada-format) + map-default
   resolution (rebinding via config edits only, no UI yet).
3. **Backend split** (sim/local scopes, `local_init` / `local_tick` /
   `action`): port the chess pointer FSM and the rpg `VERB_INPUT`
   packing into map scripts. The command stream is identical →
   chess@/rpg@ goldens must pass *without re-blessing* — that is the
   acceptance test for this step.
4. **Pick API** (`pick_ground/entity/voxel`, `cursor`), remove the
   host-side `scene.hover`.
5. **Rebind UI** in the egui HUD.
6. **Contexts + cursor_mode** — when a consumer map appears (spell
   targeting / FPS camera).

Open question — gamepad: action kinds (`axis`/`axis2`) are designed
for it, but wiring gilrs is deferred until a map needs it.
