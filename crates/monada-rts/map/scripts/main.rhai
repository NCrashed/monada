//! A Warcraft-III-class RTS demo — R-A click-move skeleton, entirely in the map.
//!
//! The engine knows no genre: this script paints a flat grass field, spawns a
//! handful of worker units per player, and moves them on the deterministic
//! fixed-point sim. Unlike the RPG there is no per-tick input snapshot —
//! orders are EVENT-driven: the local layer's `action` handler turns click
//! edges into explicit unit-addressed commands (`MOVE unit → dest`), any
//! number per tick (the lockstep schedule carries a Vec per player). The
//! shared sim never sees the mouse, only commands — selection is per-client
//! (host-side `highlight`, never hashed), exactly like the camera.
//!
//! R-A movement is a straight line + rim collision; real terrain levels and
//! `nav_path` land in R-B (docs/plans/rts-demo.md).
//!
//! Archetypes register in order, so ids are fixed: UNIT = 0.
//! Model ids by `model_actor` order: worker = 0.
//! Command verbs: 1 = MOVE(unit, dest).

// --- tuning (all fixed-point; no_float Rhai forbids IEEE) ---------------
// NB: Rhai functions are pure — they do NOT see top-level `const`s — so the
// archetype id (UNIT = 0), the field bounds (walkable [1, 46] of a 48×48
// map) and verb numbers are written as literals throughout.

fn unit_speed() { ratio(14, 100) } // sim cells / tick (30 Hz)
fn pan_speed() { ratio(50, 100) } // camera scroll, cells / tick

/// Team colours: a render-side tint multiply (0x00RRGGBB) over the shared
/// worker sprite — player 0 cool blue, player 1 warm red.
fn team_tint(owner) { if owner == 0 { 0x8fa8ff } else { 0xff9a86 } }

fn absf(x) { if x < fixed(0) { -x } else { x } }

fn nonzero(x) { x > fixed(0) || x < fixed(0) }

/// 8-way facing yaw (radians, sim space: +x east, +y north, CCW) from a
/// direction vector — the renderer bins to 8 sprites, so exact atan2 isn't
/// needed. Sector k → yaw = k·tau/8: 0=E,1=NE,2=N,3=NW,4=W,5=SW,6=S,7=SE.
fn dir_yaw(dx, dy) {
    let ax = absf(dx);
    let ay = absf(dy);
    let t = ratio(414, 1000); // tan 22.5°
    let k = 0;
    if ax > ay {
        if ay < t * ax {
            k = if dx > fixed(0) { 0 } else { 4 };
        } else if dx > fixed(0) {
            k = if dy > fixed(0) { 1 } else { 7 };
        } else {
            k = if dy > fixed(0) { 3 } else { 5 };
        }
    } else if ax < t * ay {
        k = if dy > fixed(0) { 2 } else { 6 };
    } else if dx > fixed(0) {
        k = if dy > fixed(0) { 1 } else { 7 };
    } else {
        k = if dy > fixed(0) { 3 } else { 5 };
    }
    ratio(k, 8) * tau()
}

/// A speed-`sp` velocity toward `(dx, dy)`, octagonally normalized (so
/// diagonals aren't faster), as a `vec3`.
fn nvel(dx, dy, sp) {
    let ax = absf(dx);
    let ay = absf(dy);
    let amax = if ax > ay { ax } else { ay };
    let amin = if ax > ay { ay } else { ax };
    let adist = amax + amin * ratio(1, 2);
    if adist > fixed(0) {
        vec3(dx * sp / adist, dy * sp / adist, fixed(0))
    } else {
        vec3(fixed(0), fixed(0), fixed(0))
    }
}

/// Cheap `sqrt`-free distance (octagonal `max + min/2`) — enough for the
/// arrival test (no_float Rhai has no `sqrt`).
fn approx_dist(dx, dy) {
    let ax = absf(dx);
    let ay = absf(dy);
    if ax > ay { ax + ay * ratio(1, 2) } else { ay + ax * ratio(1, 2) }
}

/// The camera's orbit yaw — MUST match `camera_angle` in `init`, since the
/// pan axis is view-relative.
fn cam_yaw() { ratio(8, 10) }

/// Rotate a `(right, forward)` input — W/D in screen space — into the
/// sim-space direction, so W pans "up the screen" rather than along world
/// +y. Accounts for the camera yaw and the render-side world-X mirror (see
/// the RPG demo's note). If W goes the wrong way, negate `cam_yaw`; if A/D
/// are swapped, negate `rx`.
fn cam_relative(rx, fy) {
    let y = cam_yaw();
    let c = cos(y);
    let s = sin(y);
    vec3(rx * s - fy * c, fy * s + rx * c, fixed(0))
}

/// Is sim cell `(cx, cy)` a wall / out of bounds? R-A has only the rim; R-B
/// replaces this with the heightfield walk rule.
fn blocked(cx, cy) {
    if cx < 1 || cy < 1 || cx > 46 || cy > 46 {
        return true;
    }
    voxel_solid(cx, cy, 1)
}

/// Sim position → the cell index it falls in (positions are cell CENTRES —
/// see the RPG demo's note on why flooring would sit half a cell off).
fn cell(v) { to_int(v + ratio(1, 2)) }

/// Whether a mover's footprint — a small box of radius `r` around `(px, py)`
/// — is clear of walls (four corners, so a ~1-cell body can't clip through).
fn clear(px, py) {
    let r = ratio(40, 100);
    !(blocked(cell(px - r), cell(py - r))
        || blocked(cell(px + r), cell(py - r))
        || blocked(cell(px - r), cell(py + r))
        || blocked(cell(px + r), cell(py + r)))
}

/// Move entity `e` by `(vx, vy)`, axis-separated so it slides along walls,
/// and reseat it on the ground.
fn try_move(e, vx, vy) {
    let pos = entity_position(e);
    let nx = pos.x + vx;
    let fx = if clear(nx, pos.y) { nx } else { pos.x };
    let ny = pos.y + vy;
    let fy = if clear(fx, ny) { ny } else { pos.y };
    let gz = ground_height(cell(fx), cell(fy));
    entity_set_position(e, vec3(fx, fy, fixed(gz)));
}

// --- setup --------------------------------------------------------------
// Archetype id by registration order: UNIT = 0. Model id by `model_actor`
// order: worker = 0.
fn init() {
    // Unit: owner (player 0/1), hit points (unused until R-E), current move
    // order (dest + has_dest flag), last facing yaw.
    archetype(["owner", "hp", "dest_x", "dest_y", "has_dest", "face"]);

    set_light(vec3(ratio(-6, 10), ratio(-3, 10), ratio(7, 10)), ratio(8, 10));

    // Autotiled flat grass field (render-only floor, walkable everywhere
    // inside the rim). Real height levels + cliffs land in R-B.
    transition(0, 1, "assets/tiles/grass-dirt.png");
    terrain_fill(0, 0, 47, 47, 1);
    terrain_blit(1);

    // A stone rim wall containing the field. Feeds collision (`voxel_solid`).
    let stone = tile("assets/tiles/stone.png");
    tile_fill(0, 0, 0, 47, 0, 6, stone);
    tile_fill(0, 47, 0, 47, 47, 6, stone);
    tile_fill(0, 0, 0, 0, 47, 6, stone);
    tile_fill(47, 0, 0, 47, 47, 6, stone);

    // The animated billboard worker actor (id 0 by call order); reuses the
    // RPG hero's idle/run GIFs as a placeholder, team-tinted per owner.
    model_actor("assets/char/worker", ["idle", "run"], ratio(14, 10));
    model_drop(0, ratio(35, 100)); // seat the visible feet on the floor

    // Starting workers — three per player, in opposite corners (the map is
    // 180°-rotation symmetric from here on). Spawned at init, not lazily:
    // an RTS needs something to click before any input exists.
    spawn_worker(0, 6, 6);
    spawn_worker(0, 9, 5);
    spawn_worker(0, 5, 9);
    spawn_worker(1, 41, 41);
    spawn_worker(1, 38, 42);
    spawn_worker(1, 42, 38);

    // WC3-style fixed-angle camera: a steep-ish tilt, free pan (local layer)
    // + host mouse-wheel zoom. `local_init` aims it at the local base.
    camera_angle(ratio(8, 10), ratio(11, 10));
    camera_dist(fixed(140));

    status("RTS demo — LMB select a worker, RMB order it, WASD pan, wheel zoom");
}

fn spawn_worker(owner, x, y) {
    let u = entity_create(0);
    entity_set_position(u, vec3(fixed(x), fixed(y), fixed(0)));
    entity_set_field(u, "owner", fixed(owner));
    entity_set_field(u, "hp", fixed(40));
    entity_set_model(u, 0); // worker actor model
    entity_set_anim(u, "idle");
    entity_set_tint(u, team_tint(owner));
}

// --- local layer (unsynced, this client only) ---------------------------
// Selection + camera live here and on the host (`highlight`, the camera
// focus) — never in the hashed world. Orders leave as explicit commands.

/// The player this client acts as: `local_player()`, or player 0 in a
/// hotseat session (no single local player).
fn me() {
    let lp = local_player();
    if lp < 0 { 0 } else { lp }
}

/// Does this client own entity `e`?
fn mine(e) { to_int(entity_field(e, "owner")) == me() }

/// Aim the opening view at this client's base corner.
fn local_init() {
    let base = if me() == 0 { vec3(fixed(7), fixed(7), fixed(0)) } else { vec3(fixed(40), fixed(40), fixed(0)) };
    camera_focus(base);
}

/// Per-tick local input: the held pan axis scrolls the camera (view-relative,
/// accumulated host-side — the local layer itself is stateless).
fn local_tick(dt) {
    let p = action_axis2("pan");
    if nonzero(p.x) || nonzero(p.y) {
        let d = cam_relative(p.x, p.y);
        camera_pan(d.x * pan_speed(), d.y * pan_speed());
    }
}

/// Click edges from the map-declared actions. `select` (LMB) picks an own
/// unit under the cursor (or clears); `order` (RMB) sends the selected unit
/// to the picked ground point as a MOVE command.
fn action(id, down) {
    if !down {
        return;
    }
    if id == "select" {
        let e = pick_entity();
        if e >= 0 && mine(e) {
            highlight(e);
        } else {
            highlight_clear();
        }
    } else if id == "order" {
        let sel = highlighted();
        if sel >= 0 && mine(sel) {
            let g = pick_ground();
            if g != () {
                submit_command(1, sel, vec3(g.x, g.y, fixed(0)));
            }
        }
    }
}

// --- commands: the only thing that crosses the wire ----------------------
// verb 1 = MOVE(unit = target, dest = arg.xy). The sim validates ownership —
// a peer cannot order units it doesn't own, whatever its local layer says.
fn command(player, verb, target, arg) {
    if verb == 1 {
        if !is_unit(target) || to_int(entity_field(target, "owner")) != player {
            return;
        }
        // Clamp the destination into the walkable field so an order into the
        // rim parks at the edge instead of grinding against it forever.
        entity_set_field(target, "dest_x", clampf(arg.x, fixed(1), fixed(46)));
        entity_set_field(target, "dest_y", clampf(arg.y, fixed(1), fixed(46)));
        entity_set_field(target, "has_dest", fixed(1));
    }
}

fn clampf(v, lo, hi) { if v < lo { lo } else if v > hi { hi } else { v } }

/// Is `e` a live unit entity? (Guards `entity_field` on a stale/foreign id.)
fn is_unit(e) {
    for u in entities_of(0) {
        if u == e {
            return true;
        }
    }
    false
}

// --- one simulation tick ------------------------------------------------
fn tick() {
    for u in entities_of(0) {
        step_unit(u);
    }
}

/// Advance one unit along its straight-line move order (R-B replaces the
/// line with `nav_path` waypoints), with rim collision + arrival.
fn step_unit(u) {
    let owner = to_int(entity_field(u, "owner"));
    entity_set_tint(u, team_tint(owner));
    if to_int(entity_field(u, "has_dest")) == 0 {
        entity_set_anim(u, "idle");
        return;
    }
    let pos = entity_position(u);
    let dx = entity_field(u, "dest_x") - pos.x;
    let dy = entity_field(u, "dest_y") - pos.y;
    if approx_dist(dx, dy) < ratio(20, 100) {
        entity_set_field(u, "has_dest", fixed(0));
        entity_set_anim(u, "idle");
        return;
    }
    let v = nvel(dx, dy, unit_speed());
    try_move(u, v.x, v.y);
    entity_set_field(u, "face", dir_yaw(dx, dy));
    entity_set_facing(u, entity_field(u, "face"));
    entity_set_anim(u, "run");
}
