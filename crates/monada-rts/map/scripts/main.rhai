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
//! R-D adds the economy: a town hall + a gold mine per side (painted
//! voxel blocks — their columns block walking/nav by the SAME height rule
//! as cliffs and trees, no extra bookkeeping), a worker harvest loop
//! (walk to mine → mine → carry to hall → +gold → repeat), and worker
//! training at the hall for gold, capped by supply.
//!
//! Archetypes register in order: UNIT = 0, TREE = 1, BUILDING = 2,
//! MINE = 3, GAME = 4.
//! Model ids by call order: worker actor = 0.
//! Command verbs: 1 = MOVE(unit, dest), 2 = HARVEST(unit, arg.z = mine),
//! 3 = TRAIN (the sender's hall).
//! Worker task field: 0 = idle/move, 1 = to-mine, 2 = mining,
//! 3 = to-hall.

// --- tuning (all fixed-point; no_float Rhai forbids IEEE) ---------------
// NB: Rhai functions are pure — they do NOT see top-level `const`s — so the
// archetype ids, the field bounds (walkable [1, 46] of 48×48), the walk
// tolerance (max_step 2) and heights (plateau 12, rim 18, tree 5) are
// written as literals throughout.

fn unit_speed() { ratio(14, 100) } // sim cells / tick (30 Hz)
fn pan_speed() { ratio(50, 100) } // camera scroll, cells / tick

// Economy tuning.
fn start_gold() { 100 }
fn worker_cost() { 50 }
fn train_ticks() { 90 } // 3 s at 30 Hz
fn mine_ticks() { 45 } // 1.5 s inside the mine per trip
fn gold_per_trip() { 10 }
fn mine_reserve() { 500 } // per mine
fn supply_cap() { 12 } // max units per player

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
    // order (final dest + next nav waypoint + active flag), facing yaw,
    // and the harvest loop (task FSM, mine-visit timer, carried gold flag,
    // the mine entity it works).
    archetype(["owner", "hp", "dest_x", "dest_y", "wp_x", "wp_y", "has_dest", "face", "task", "timer", "carry", "mine"]);
    // Tree: a destructible prop (hp; felling lands in R-E). Its painted
    // voxel column is what blocks walking — the entity is the hittable half.
    archetype(["hp"]);
    // Building: hall (kind 0; trains workers) — barracks (kind 1) lands in
    // R-E. Painted as a voxel block; the entity carries hp + the queue.
    archetype(["owner", "kind", "hp", "queue", "progress"]);
    // Gold mine: neutral, finite.
    archetype(["gold"]);
    // Game singleton: per-player gold.
    archetype(["gold0", "gold1"]);

    set_light(vec3(ratio(-6, 10), ratio(-3, 10), ratio(7, 10)), ratio(8, 10));

    build_terrain();

    // The animated billboard worker actor (id 0 by call order); reuses the
    // RPG hero's idle/run GIFs as a placeholder, team-tinted per owner.
    model_actor("assets/char/worker", ["idle", "run"], ratio(14, 10));
    model_drop(0, ratio(35, 100)); // seat the visible feet on the floor

    // Starting bases — a town hall + a gold mine per plateau, painted as
    // voxel blocks whose columns block walking/nav by the same height rule
    // as everything else. 180°-rotation symmetric (cell → (47−x, 47−y)).
    spawn_hall(0, 5, 5);
    spawn_hall(1, 42, 42);
    spawn_mine(10, 10);
    spawn_mine(37, 37);

    // Starting workers — three per player, beside their halls.
    spawn_worker(0, 8, 4);
    spawn_worker(0, 4, 8);
    spawn_worker(0, 8, 8);
    spawn_worker(1, 39, 43);
    spawn_worker(1, 43, 39);
    spawn_worker(1, 39, 39);

    // The game singleton: both purses.
    let g = entity_create(4);
    entity_set_field(g, "gold0", fixed(start_gold()));
    entity_set_field(g, "gold1", fixed(start_gold()));

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

/// A town hall: a 3×3 painted block (center `(x, y)`, 8 tall above its
/// plateau — the column height walls it off for movement AND nav, exactly
/// like a cliff) + the building entity (owner, kind 0, train queue).
fn spawn_hall(owner, x, y) {
    let base = ground_height(x, y);
    let wall = if owner == 0 { 0x808a_5a33 } else { 0x8070_4a63 };
    let roof = 0x80b8_3c2a;
    voxel_fill(x - 1, y - 1, base, x + 1, y + 1, base + 7, wall);
    voxel_fill(x - 1, y - 1, base + 8, x + 1, y + 1, base + 8, roof);
    let b = entity_create(2);
    entity_set_position(b, vec3(fixed(x), fixed(y), fixed(base)));
    entity_set_field(b, "owner", fixed(owner));
    entity_set_field(b, "kind", fixed(0));
    entity_set_field(b, "hp", fixed(300));
}

/// A neutral gold mine: a 3×3 painted rock block with a gold-flecked top
/// + the mine entity holding its reserve.
fn spawn_mine(x, y) {
    let base = ground_height(x, y);
    voxel_fill(x - 1, y - 1, base, x + 1, y + 1, base + 5, 0x806b_6b70);
    voxel_fill(x - 1, y - 1, base + 6, x + 1, y + 1, base + 6, 0x80c9_a227);
    let m = entity_create(3);
    entity_set_position(m, vec3(fixed(x), fixed(y), fixed(base)));
    entity_set_field(m, "gold", fixed(mine_reserve()));
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

/// Click edges from the map-declared actions. `select` (LMB) is a drag
/// gesture — press anchors a ground rectangle on the host (`drag_begin`,
/// drawn there: the gesture state can't live in this stateless layer),
/// release takes it (`drag_end`) and selects. `order` (RMB) sends EVERY
/// selected unit to the picked point — one explicit command per unit
/// (lockstep carries a Vec per player per tick).
fn action(id, down) {
    if id == "select" {
        if down {
            drag_begin();
        } else {
            let r = drag_end();
            if r.len() == 2 {
                box_select(r[0], r[1]);
            }
        }
    } else if id == "order" && down {
        let g = pick_ground();
        if g != () {
            // Context order: a click on a gold mine sends HARVEST (the
            // mine entity id rides arg.z), open ground sends MOVE — one
            // command per selected unit either way.
            let m = mine_at(g);
            for e in highlighted_all() {
                if mine(e) {
                    if m >= 0 {
                        submit_command(2, e, vec3(g.x, g.y, fixed(m)));
                    } else {
                        submit_command(1, e, vec3(g.x, g.y, fixed(0)));
                    }
                }
            }
        }
    } else if id == "train1" && down {
        // Queue a worker at this player's hall (the sim locates it).
        submit_command(3, -1, vec3(fixed(0), fixed(0), fixed(0)));
    }
}

/// The gold mine whose 3×3 block contains ground point `g`, or `-1`.
/// (Buildings are painted voxels, not model-bound sprites, so
/// `pick_entity` can't see them — the cell lookup is the context test.)
fn mine_at(g) {
    let cx = cell(g.x);
    let cy = cell(g.y);
    for m in entities_of(3) {
        let p = entity_position(m);
        let mx = cell(p.x);
        let my = cell(p.y);
        if cx >= mx - 1 && cx <= mx + 1 && cy >= my - 1 && cy <= my + 1 {
            return m;
        }
    }
    -1
}

fn minf(a, b) { if a < b { a } else { b } }
fn maxf(a, b) { if a < b { b } else { a } }

/// Resolve a finished select gesture. A tiny rectangle is a CLICK —
/// single-select whatever unit is under the cursor (or clear); a real
/// drag selects every own unit inside the ground rectangle.
fn box_select(a, b) {
    if approx_dist(b.x - a.x, b.y - a.y) < ratio(35, 100) {
        let e = pick_entity();
        if e >= 0 && mine(e) {
            highlight(e);
        } else {
            highlight_clear();
        }
        return;
    }
    let x0 = minf(a.x, b.x);
    let x1 = maxf(a.x, b.x);
    let y0 = minf(a.y, b.y);
    let y1 = maxf(a.y, b.y);
    highlight_clear();
    for u in entities_of(0) {
        if !mine(u) {
            continue;
        }
        let p = entity_position(u);
        if p.x >= x0 && p.x <= x1 && p.y >= y0 && p.y <= y1 {
            highlight_add(u);
        }
    }
}

// --- commands: the only thing that crosses the wire ----------------------
// The sim validates ownership on every verb — a peer cannot order units
// (or spend gold) it doesn't own, whatever its local layer says.
// verb 1 = MOVE(unit, dest): explicit move, interrupts any harvest task.
// verb 2 = HARVEST(unit, arg.z = mine): start the harvest loop.
// verb 3 = TRAIN: queue a worker at the sender's hall (gold + supply
// checked here, so the deduction is deterministic on every peer).
fn command(player, verb, target, arg) {
    if verb == 1 {
        if !is_unit(target) || to_int(entity_field(target, "owner")) != player {
            return;
        }
        entity_set_field(target, "task", fixed(0));
        walk_to(target, arg.x, arg.y);
    } else if verb == 2 {
        if !is_unit(target) || to_int(entity_field(target, "owner")) != player {
            return;
        }
        let m = to_int(arg.z);
        if !is_mine(m) {
            return;
        }
        entity_set_field(target, "task", fixed(1));
        entity_set_field(target, "mine", fixed(m));
        let p = entity_position(m);
        walk_to(target, p.x, p.y);
    } else if verb == 3 {
        let h = hall_of(player);
        if h < 0 {
            return;
        }
        let g = entities_of(4)[0];
        let purse = "gold" + player;
        let queued = to_int(entity_field(h, "queue"));
        if to_int(entity_field(g, purse)) < worker_cost() {
            return;
        }
        if count_units(player) + queued >= supply_cap() {
            return;
        }
        entity_set_field(g, purse, entity_field(g, purse) - fixed(worker_cost()));
        entity_set_field(h, "queue", fixed(queued + 1));
    }
}

/// Point unit `u` at `(x, y)`: clamp into the field, arm the order, plan
/// the first nav waypoint.
fn walk_to(u, x, y) {
    entity_set_field(u, "dest_x", clampf(x, fixed(1), fixed(46)));
    entity_set_field(u, "dest_y", clampf(y, fixed(1), fixed(46)));
    entity_set_field(u, "has_dest", fixed(1));
    replan(u);
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

fn is_mine(e) {
    for m in entities_of(3) {
        if m == e {
            return true;
        }
    }
    false
}

/// `player`'s town hall (kind 0), or `-1` if it no longer stands.
fn hall_of(player) {
    for b in entities_of(2) {
        if to_int(entity_field(b, "kind")) == 0 && to_int(entity_field(b, "owner")) == player {
            return b;
        }
    }
    -1
}

fn count_units(player) {
    let n = 0;
    for u in entities_of(0) {
        if to_int(entity_field(u, "owner")) == player {
            n += 1;
        }
    }
    n
}

/// Is unit `u` within Chebyshev `r` cells of entity `e`'s cell? (`r` = 2
/// reaches the ring around a 3×3 building block.)
fn near(u, e, r) {
    let a = entity_position(u);
    let b = entity_position(e);
    let dx = cell(a.x) - cell(b.x);
    let dy = cell(a.y) - cell(b.y);
    dx <= r && dx >= -r && dy <= r && dy >= -r
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
    for b in entities_of(2) {
        step_building(b);
    }
    build_ui();
}

/// Advance a hall's train queue: progress ticks while anything is queued;
/// a finished worker pops out onto a free ring cell (if the whole ring is
/// jammed, delivery retries next tick — the gold is already spent, the
/// queue must not be lost).
fn step_building(b) {
    let q = to_int(entity_field(b, "queue"));
    if q == 0 {
        return;
    }
    let p = to_int(entity_field(b, "progress")) + 1;
    if p < train_ticks() {
        entity_set_field(b, "progress", fixed(p));
        return;
    }
    let pos = entity_position(b);
    let owner = to_int(entity_field(b, "owner"));
    let out = free_ring_cell(cell(pos.x), cell(pos.y), to_int(pos.z));
    if out.x < fixed(0) {
        return; // ring jammed: hold at full progress, retry next tick
    }
    spawn_worker(owner, to_int(out.x), to_int(out.y));
    entity_set_field(b, "queue", fixed(q - 1));
    entity_set_field(b, "progress", fixed(0));
}

/// The first walkable cell on the Chebyshev-2 ring around `(cx, cy)` (the
/// ring hugging a 3×3 block), from height `hz` — or `(-1, -1)` if jammed.
/// Fixed scan order keeps delivery deterministic.
fn free_ring_cell(cx, cy, hz) {
    for dx in -2..=2 {
        for dy in -2..=2 {
            let onring = dx == -2 || dx == 2 || dy == -2 || dy == 2;
            if onring && passable(hz, cx + dx, cy + dy) {
                return vec3(fixed(cx + dx), fixed(cy + dy), fixed(0));
            }
        }
    }
    vec3(fixed(-1), fixed(-1), fixed(0))
}

/// The always-on HUD: the LOCAL player's purse + supply and the train
/// hotkey. Render-side (`ui_*` no-op headlessly, `local_player` is a
/// per-client read that never touches the hashed world).
fn build_ui() {
    ui_clear();
    let w = ui_width();
    if w <= 0 {
        return; // no viewport yet (pre-first-frame, or headless)
    }
    let lp = local_player();
    let p = if lp < 0 { 0 } else { lp };
    let g = entities_of(4)[0];
    ui_scale(fixed(2));
    ui_text(8, 8, "Gold " + to_int(entity_field(g, "gold" + p)) + "    Supply " + count_units(p) + "/" + supply_cap(), 12);
    ui_text(8, 38, "LMB select · drag box · RMB move/harvest · Q train worker", 8);
}

/// Advance one unit: run the harvest FSM at its stations (mining inside
/// the mine, deciding at arrivals), then walk it along its nav waypoints
/// under the walk rule.
fn step_unit(u) {
    let owner = to_int(entity_field(u, "owner"));
    entity_set_tint(u, team_tint(owner));

    // Station: inside the mine (task 2) — count the visit down, then
    // shoulder the gold and head home.
    if to_int(entity_field(u, "task")) == 2 {
        let t = to_int(entity_field(u, "timer")) - 1;
        if t > 0 {
            entity_set_field(u, "timer", fixed(t));
            entity_set_anim(u, "idle");
            return;
        }
        let m = to_int(entity_field(u, "mine"));
        let left = if is_mine(m) { to_int(entity_field(m, "gold")) } else { 0 };
        let take = if left < gold_per_trip() { left } else { gold_per_trip() };
        if take == 0 {
            entity_set_field(u, "task", fixed(0)); // mine ran dry under us
            entity_set_anim(u, "idle");
            return;
        }
        entity_set_field(m, "gold", fixed(left - take));
        entity_set_field(u, "carry", fixed(take));
        let h = hall_of(owner);
        if h < 0 {
            entity_set_field(u, "task", fixed(0));
            entity_set_anim(u, "idle");
            return;
        }
        entity_set_field(u, "task", fixed(3));
        let hp = entity_position(h);
        walk_to(u, hp.x, hp.y);
    }

    if to_int(entity_field(u, "has_dest")) == 0 {
        // Arrived (or parked as close as reachable): harvest stations act.
        let task = to_int(entity_field(u, "task"));
        if task == 1 {
            arrive_at_mine(u);
        } else if task == 3 {
            arrive_at_hall(u, owner);
        }
        if to_int(entity_field(u, "has_dest")) == 0 {
            entity_set_anim(u, "idle");
            return;
        }
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

/// A task-1 worker has stopped walking: beside its mine (the nav closest-
/// approach parks on the ring — the block itself is unreachable), with ore
/// left → start the visit; otherwise give up.
fn arrive_at_mine(u) {
    let m = to_int(entity_field(u, "mine"));
    if is_mine(m) && to_int(entity_field(m, "gold")) > 0 && near(u, m, 2) {
        entity_set_field(u, "task", fixed(2));
        entity_set_field(u, "timer", fixed(mine_ticks()));
    } else {
        entity_set_field(u, "task", fixed(0));
    }
}

/// A task-3 worker has stopped walking: beside the hall → bank the load
/// and turn back for more (while the mine holds out).
fn arrive_at_hall(u, owner) {
    let h = hall_of(owner);
    if h >= 0 && near(u, h, 2) {
        let g = entities_of(4)[0];
        let purse = "gold" + owner;
        entity_set_field(g, purse, entity_field(g, purse) + entity_field(u, "carry"));
        entity_set_field(u, "carry", fixed(0));
        let m = to_int(entity_field(u, "mine"));
        if is_mine(m) && to_int(entity_field(m, "gold")) > 0 {
            entity_set_field(u, "task", fixed(1));
            let mp = entity_position(m);
            walk_to(u, mp.x, mp.y);
            return;
        }
    }
    entity_set_field(u, "task", fixed(0));
}
