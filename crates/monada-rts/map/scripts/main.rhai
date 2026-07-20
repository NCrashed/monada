//! A Warcraft-III-class RTS demo — R-B terrain levels + shared pathfinding.
//!
//! The engine knows no genre: this script paints a heightfield (a lowland,
//! two cliff-walled start plateaus, walkable ramps, tree columns), spawns
//! worker units per player, and routes them with the engine's deterministic
//! `nav_path` (monada-nav A*) on the fixed-point sim. Orders are EVENT-
//! driven: the local layer's `action` handler turns click edges into
//! explicit unit-addressed commands (`MOVE unit → dest`) — the shared sim
//! never sees the mouse; selection is per-client (`highlight`), never
//! hashed.
//!
//! THE WALK RULE (one source of truth): a step between neighbouring cells
//! passes when |Δ ground_height| ≤ 2. Movement collision (`clear`) and
//! `nav_path` both apply it against the same painted heightmap, so what a
//! unit can walk is exactly what the pathfinder routes. Cliffs (Δ12) wall
//! off, ramps (Δ2 per cell) route, trees (Δ5 columns) block — no separate
//! obstacle bookkeeping at all.
//!
//! Waypoint follow is REPLAN-PER-CELL: a unit stores only its final dest +
//! the next waypoint (hashed fields — script functions are pure and can
//! hold no path arrays), and re-asks `nav_path` when the waypoint is
//! reached. A* on this map is microseconds in Rust, and a felled tree /
//! new footprint re-routes automatically.
//!
//! Archetypes register in order: UNIT = 0, TREE = 1.
//! Model ids by call order: worker actor = 0.
//! Command verbs: 1 = MOVE(unit, dest).

// --- tuning (all fixed-point; no_float Rhai forbids IEEE) ---------------
// NB: Rhai functions are pure — they do NOT see top-level `const`s — so the
// archetype ids, the field bounds (walkable [1, 46] of 48×48), the walk
// tolerance (max_step 2) and heights (plateau 12, rim 18, tree 5) are
// written as literals throughout.

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
/// +y (accounts for the camera yaw + the render-side world-X mirror; see
/// the RPG demo's note).
fn cam_relative(rx, fy) {
    let y = cam_yaw();
    let c = cos(y);
    let s = sin(y);
    vec3(rx * s - fy * c, fy * s + rx * c, fixed(0))
}

// --- the walk rule -------------------------------------------------------

/// Sim position → the cell index it falls in (positions are cell CENTRES —
/// see the RPG demo's note on why flooring would sit half a cell off).
fn cell(v) { to_int(v + ratio(1, 2)) }

/// Can a mover standing at height `hz` step onto cell `(cx, cy)`? The SAME
/// rule `nav_path(…, 2)` routes by: in the field, climb ≤ 2. Cliffs, the
/// rim band and tree columns all block purely by height delta.
fn passable(hz, cx, cy) {
    if cx < 1 || cy < 1 || cx > 46 || cy > 46 {
        return false;
    }
    let d = ground_height(cx, cy) - hz;
    d <= 2 && d >= -2
}

/// Whether a mover's footprint — a small box of radius `r` around
/// `(px, py)` — can stand there coming from height `hz` (four corners, so
/// a ~1-cell body can't clip a cliff edge or a tree).
fn clear(px, py, hz) {
    let r = ratio(40, 100);
    passable(hz, cell(px - r), cell(py - r))
        && passable(hz, cell(px + r), cell(py - r))
        && passable(hz, cell(px - r), cell(py + r))
        && passable(hz, cell(px + r), cell(py + r))
}

/// Move entity `e` by `(vx, vy)` under the walk rule, axis-separated so it
/// slides along cliff edges, and seat it on the ground it ends on.
fn try_move(e, vx, vy) {
    let pos = entity_position(e);
    let hz = ground_height(cell(pos.x), cell(pos.y));
    let nx = pos.x + vx;
    let fx = if clear(nx, pos.y, hz) { nx } else { pos.x };
    let ny = pos.y + vy;
    let fy = if clear(fx, ny, hz) { ny } else { pos.y };
    let gz = ground_height(cell(fx), cell(fy));
    entity_set_position(e, vec3(fx, fy, fixed(gz)));
}

// --- setup --------------------------------------------------------------
// Archetype ids by registration order: UNIT = 0, TREE = 1. Model id by
// `model_actor` order: worker = 0.
fn init() {
    // Unit: owner (player 0/1), hit points (combat lands in R-E), the move
    // order (final dest + next nav waypoint + active flag), facing yaw.
    archetype(["owner", "hp", "dest_x", "dest_y", "wp_x", "wp_y", "has_dest", "face"]);
    // Tree: a destructible prop (hp; felling lands in R-E). Its painted
    // voxel column is what blocks walking — the entity is the hittable half.
    archetype(["hp"]);

    set_light(vec3(ratio(-6, 10), ratio(-3, 10), ratio(7, 10)), ratio(8, 10));

    build_terrain();

    // The animated billboard worker actor (id 0 by call order); reuses the
    // RPG hero's idle/run GIFs as a placeholder, team-tinted per owner.
    model_actor("assets/char/worker", ["idle", "run"], ratio(14, 10));
    model_drop(0, ratio(35, 100)); // seat the visible feet on the floor

    // Starting workers — three per player, up on their plateaus. The map is
    // 180°-rotation symmetric (cell → (47−x, 47−y)).
    spawn_worker(0, 5, 5);
    spawn_worker(0, 8, 4);
    spawn_worker(0, 4, 8);
    spawn_worker(1, 42, 42);
    spawn_worker(1, 39, 43);
    spawn_worker(1, 43, 39);

    // WC3-style fixed-angle camera: a steep-ish tilt, free pan (local
    // layer) + host mouse-wheel zoom. `local_init` aims it at the base.
    camera_angle(ratio(8, 10), ratio(11, 10));
    camera_dist(fixed(140));

    status("RTS demo — LMB select a worker, RMB order it, WASD pan, wheel zoom");
}

/// Paint the heightfield. Heights ARE the level design: lowland 0, start
/// plateaus 12 (cliff-walled — Δ12 ≫ the walk tolerance 2), ramps dropping
/// 2 per cell over 6 cells (the only ways up, and so the chokepoints), a
/// rim band at 18 (unclimbable even from a plateau), tree columns at 5.
/// Everything is `tile_fill`/`voxel_fill`, so collision + nav read the
/// exact same shape the eye sees.
fn build_terrain() {
    // Autotiled flat grass over the lowland (render-only floor at z 0).
    transition(0, 1, "assets/tiles/grass-dirt.png");
    terrain_fill(0, 0, 47, 47, 1);
    terrain_blit(1);

    let grass = tile("assets/tiles/grass1.png");
    let dirt = tile("assets/tiles/dirt.png");
    let stone = tile("assets/tiles/stone.png");

    // Rim band (height 18): the field's edge, unclimbable from any level.
    tile_fill(0, 0, 0, 47, 0, 18, stone);
    tile_fill(0, 47, 0, 47, 47, 18, stone);
    tile_fill(0, 0, 0, 0, 47, 18, stone);
    tile_fill(47, 0, 0, 47, 47, 18, stone);

    // Start plateaus (height 12): SW for player 0, NE for player 1 — dirt
    // flanks below a grass cap, painted as one slab + a cap layer.
    tile_fill(1, 1, 0, 14, 14, 11, dirt);
    tile_fill(1, 1, 12, 14, 14, 12, grass);
    tile_fill(33, 33, 0, 46, 46, 11, dirt);
    tile_fill(33, 33, 12, 46, 46, 12, grass);

    // Ramps (2 cells wide, dropping 12 → 0 by 2 per cell): east out of the
    // SW plateau at rows y 7..8, west out of the NE plateau at rows
    // y 39..40 — each plateau's single chokepoint, 180°-mirrored.
    let sx = 15;
    while sx <= 20 {
        let h = 12 - (sx - 14) * 2; // 10, 8, 6, 4, 2, 0
        if h > 0 {
            tile_fill(sx, 7, 0, sx, 8, h, dirt);
        }
        let mx = 47 - sx; // 32 … 27, the mirrored column
        if h > 0 {
            tile_fill(mx, 39, 0, mx, 40, h, dirt);
        }
        sx += 1;
    }

    // Trees: destructible props down the contested middle, with gaps —
    // decoration that shapes lanes, not a wall. A trunk + canopy column
    // (height 5) blocks by the walk rule alone; the entity carries the hp.
    plant_tree(24, 10);
    plant_tree(24, 14);
    plant_tree(24, 18);
    plant_tree(24, 22);
    plant_tree(23, 37);
    plant_tree(23, 33);
    plant_tree(23, 29);
    plant_tree(23, 25);
    plant_tree(18, 30);
    plant_tree(29, 17);
}

/// A tree at cell `(x, y)`: a painted trunk/canopy column (the blocker) +
/// a sim entity (the destructible half, R-E).
fn plant_tree(x, y) {
    voxel_fill(x, y, 0, x, y, 2, 0x805b_4226); // trunk (brown)
    voxel_fill(x, y, 3, x, y, 5, 0x8032_7a2e); // canopy (green)
    let t = entity_create(1);
    entity_set_position(t, vec3(fixed(x), fixed(y), fixed(0)));
    entity_set_field(t, "hp", fixed(30));
}

fn spawn_worker(owner, x, y) {
    let u = entity_create(0);
    entity_set_position(u, vec3(fixed(x), fixed(y), fixed(ground_height(x, y))));
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

/// Aim the opening view at this client's plateau.
fn local_init() {
    let base = if me() == 0 { vec3(fixed(7), fixed(7), fixed(12)) } else { vec3(fixed(40), fixed(40), fixed(12)) };
    camera_focus(base);
}

/// Per-tick local input: the held pan axis scrolls the camera (view-
/// relative, accumulated host-side — the local layer itself is stateless).
fn local_tick(dt) {
    let p = action_axis2("pan");
    if nonzero(p.x) || nonzero(p.y) {
        let d = cam_relative(p.x, p.y);
        camera_pan(d.x * pan_speed(), d.y * pan_speed());
    }
}

/// Click edges from the map-declared actions. `select` (LMB) picks an own
/// unit under the cursor (or clears); `order` (RMB) sends the selected
/// unit to the picked ground point as a MOVE command.
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
// verb 1 = MOVE(unit = target, dest = arg.xy). The sim validates ownership
// — a peer cannot order units it doesn't own, whatever its local layer
// says — then plans the first nav waypoint.
fn command(player, verb, target, arg) {
    if verb == 1 {
        if !is_unit(target) || to_int(entity_field(target, "owner")) != player {
            return;
        }
        entity_set_field(target, "dest_x", clampf(arg.x, fixed(1), fixed(46)));
        entity_set_field(target, "dest_y", clampf(arg.y, fixed(1), fixed(46)));
        entity_set_field(target, "has_dest", fixed(1));
        replan(target);
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

/// (Re)plan unit `u`'s next waypoint toward its stored dest: ask
/// `nav_path` from its current cell and keep only the FIRST step (fields
/// can hold no arrays; A* here is cheap, so each reached waypoint replans
/// — which also re-routes around whatever changed since the order).
/// Within the goal cell the dest point itself is the waypoint. An empty
/// path (boxed in / already as close as reachable) ends the order.
fn replan(u) {
    let pos = entity_position(u);
    let cx = cell(pos.x);
    let cy = cell(pos.y);
    let dx = cell(entity_field(u, "dest_x"));
    let dy = cell(entity_field(u, "dest_y"));
    if cx == dx && cy == dy {
        entity_set_field(u, "wp_x", entity_field(u, "dest_x"));
        entity_set_field(u, "wp_y", entity_field(u, "dest_y"));
        return;
    }
    let path = nav_path(cx, cy, dx, dy, 2);
    if path.len() == 0 {
        entity_set_field(u, "has_dest", fixed(0)); // nowhere closer to walk
    } else {
        let wp = path[0];
        entity_set_field(u, "wp_x", wp.x);
        entity_set_field(u, "wp_y", wp.y);
    }
}

// --- one simulation tick ------------------------------------------------
fn tick() {
    for u in entities_of(0) {
        step_unit(u);
    }
}

/// Advance one unit along its nav waypoints: walk toward the stored
/// waypoint under the walk rule; on reaching it, either finish (it was the
/// dest) or replan the next one.
fn step_unit(u) {
    let owner = to_int(entity_field(u, "owner"));
    entity_set_tint(u, team_tint(owner));
    if to_int(entity_field(u, "has_dest")) == 0 {
        entity_set_anim(u, "idle");
        return;
    }
    let pos = entity_position(u);
    let tx = entity_field(u, "wp_x");
    let ty = entity_field(u, "wp_y");
    let dx = tx - pos.x;
    let dy = ty - pos.y;
    if approx_dist(dx, dy) < ratio(20, 100) {
        // Waypoint reached. Done if it was the dest point; else replan.
        let fx = entity_field(u, "dest_x") - pos.x;
        let fy = entity_field(u, "dest_y") - pos.y;
        if approx_dist(fx, fy) < ratio(30, 100) {
            entity_set_field(u, "has_dest", fixed(0));
            entity_set_anim(u, "idle");
            return;
        }
        replan(u);
        if to_int(entity_field(u, "has_dest")) == 0 {
            entity_set_anim(u, "idle");
            return;
        }
        dx = entity_field(u, "wp_x") - pos.x;
        dy = entity_field(u, "wp_y") - pos.y;
    }
    let v = nvel(dx, dy, unit_speed());
    try_move(u, v.x, v.y);
    entity_set_field(u, "face", dir_yaw(dx, dy));
    entity_set_facing(u, entity_field(u, "face"));
    entity_set_anim(u, "run");
}
