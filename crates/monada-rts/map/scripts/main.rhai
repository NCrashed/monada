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
//! R-E adds combat and the win: a barracks per side trains soldiers,
//! ATTACK orders (and soldier auto-acquire) resolve melee against units,
//! buildings and trees — a felled tree `voxel_clear`s its column, so the
//! cell opens for walking AND nav in the same breath — and destroying
//! the enemy town hall wins the game.
//!
//! Archetypes register in order: UNIT = 0, TREE = 1, BUILDING = 2,
//! MINE = 3, GAME = 4.
//! Model ids by call order: worker actor = 0, soldier actor = 1.
//! Command verbs: 1 = MOVE(unit, dest), 2 = HARVEST(unit, arg.z = mine),
//! 3 = TRAIN worker (hall), 4 = TRAIN soldier (barracks),
//! 5 = ATTACK(unit, arg.z = victim entity).
//! Unit task field: 0 = idle/move, 1 = to-mine, 2 = mining, 3 = to-hall,
//! 4 = attacking. Unit kind: 0 = worker, 1 = soldier.
//! Building kind: 0 = hall, 1 = barracks. game.winner: 0 none, 1/2 =
//! player + 1.

// --- tuning (all fixed-point; no_float Rhai forbids IEEE) ---------------
// NB: Rhai functions are pure — they do NOT see top-level `const`s — so the
// archetype ids, the field bounds (walkable [1, 46] of 48×48), the walk
// tolerance (max_step 2) and heights (plateau 12, rim 18, tree 5) are
// written as literals throughout.

fn unit_speed() { ratio(14, 100) } // sim cells / tick (30 Hz)
fn pan_speed() { ratio(50, 100) } // camera scroll, cells / tick

// Unit separation (the RPG's boid push): bodies within `sep_radius` push
// apart so a marching group arrives as a crowd, not a single stacked
// sprite. `sep_gain` scales the push, `sep_max` caps it below unit_speed
// so steering still dominates.
fn sep_radius() { ratio(9, 10) }
fn sep_gain() { ratio(8, 100) }
fn sep_max() { ratio(9, 100) }

// Economy tuning.
fn start_gold() { 100 }
fn worker_cost() { 50 }
fn soldier_cost() { 75 }
fn train_ticks() { 90 } // 3 s at 30 Hz
fn mine_ticks() { 45 } // 1.5 s inside the mine per trip
fn gold_per_trip() { 10 }
fn mine_reserve() { 500 } // per mine
fn supply_cap() { 12 } // max units per player

// Combat tuning.
fn worker_hp() { 40 }
fn soldier_hp() { 60 }
fn worker_dmg() { 3 } // workers chop / defend weakly
fn soldier_dmg() { 10 }
fn attack_cd() { 30 } // ticks between swings (1 s)
fn aggro_range() { fixed(5) } // soldier auto-acquire (approx cells)

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

/// Boid separation for unit `u` at `up`: a push away from every OTHER
/// unit within `sep_radius` (harder the closer), scaled and capped.
/// Bodies are bodies — both sides push. Deterministic: fixed-point,
/// ascending-id iteration.
fn separation(u, up) {
    let r = sep_radius();
    let sx = fixed(0);
    let sy = fixed(0);
    for o in entities_of(0) {
        if o == u {
            continue;
        }
        let op = entity_position(o);
        let dx = up.x - op.x; // pointing away from the neighbour
        let dy = up.y - op.y;
        let d = approx_dist(dx, dy);
        if d > fixed(0) && d < r {
            let w = (r - d) / d; // normalise + weight: stronger when closer
            sx += dx * w;
            sy += dy * w;
        }
    }
    let px = sx * sep_gain();
    let py = sy * sep_gain();
    let pm = approx_dist(px, py);
    if pm > sep_max() {
        px = px * sep_max() / pm;
        py = py * sep_max() / pm;
    }
    vec3(px, py, fixed(0))
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
    grid_spawn(0, 0, 0); // world grid for terrain painting

    // Unit: owner (player 0/1), kind (worker/soldier), hit points, the
    // move order (final dest + next nav waypoint + active flag), facing
    // yaw, the task FSM (harvest stations / attacking), its mine, its
    // attack victim + swing cooldown, carried gold.
    archetype(["owner", "kind", "hp", "dest_x", "dest_y", "wp_x", "wp_y", "has_dest", "face", "task", "timer", "carry", "mine", "victim", "atkcd", "stall"]);
    // Tree: a destructible prop. Its painted voxel column is what blocks
    // walking — the entity is the hittable half; felling clears the column.
    archetype(["hp"]);
    // Building: hall (kind 0, trains workers) / barracks (kind 1, trains
    // soldiers). Painted as a voxel block; the entity carries hp + queue.
    archetype(["owner", "kind", "hp", "queue", "progress"]);
    // Gold mine: neutral, finite.
    archetype(["gold"]);
    // Game singleton: per-player gold + the winner (0 none, player + 1).
    archetype(["gold0", "gold1", "winner"]);

    set_light(vec3(ratio(-6, 10), ratio(-3, 10), ratio(7, 10)), ratio(8, 10));

    build_terrain();

    // The animated billboard actors (ids 0/1 by call order); placeholders
    // reusing RPG art (hero = worker, enemy = soldier — the soldier gets a
    // real attack animation), team-tinted per owner.
    model_actor("assets/char/worker", ["idle", "run", "attack"], ratio(14, 10));
    model_actor("assets/char/soldier", ["idle", "run", "attack"], ratio(14, 10));
    // NO model_drop: the actor pivot already grounds on the art's trimmed
    // opaque bottom, and unlike the RPG (render-only floor, where a 0.35-cell
    // sink ≈ 5.6 world units is invisible) this map's terrain is REAL voxel
    // columns — the RPG's drop buried units ankle-deep and hid the selection
    // ring under the surface (the ship demo hit the same trap). Nudge with
    // e.g. `model_drop(0, ratio(5, 100))` only if the art's feet float.

    // Starting bases — a town hall + a gold mine per plateau, painted as
    // voxel blocks whose columns block walking/nav by the same height rule
    // as everything else. 180°-rotation symmetric (cell → (47−x, 47−y)).
    spawn_hall(0, 5, 5);
    spawn_hall(1, 42, 42);
    spawn_barracks(0, 5, 12);
    spawn_barracks(1, 42, 35);
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

/// A barracks: a 3×3 painted block (kind 1) that trains soldiers.
fn spawn_barracks(owner, x, y) {
    let base = ground_height(x, y);
    let wall = if owner == 0 { 0x805a_6a8a } else { 0x808a_5a5a };
    voxel_fill(x - 1, y - 1, base, x + 1, y + 1, base + 6, wall);
    voxel_fill(x - 1, y - 1, base + 7, x + 1, y + 1, base + 7, 0x8044_4a55);
    let b = entity_create(2);
    entity_set_position(b, vec3(fixed(x), fixed(y), fixed(base)));
    entity_set_field(b, "owner", fixed(owner));
    entity_set_field(b, "kind", fixed(1));
    entity_set_field(b, "hp", fixed(200));
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

fn spawn_worker(owner, x, y) { spawn_unit(owner, 0, x, y) }

fn spawn_unit(owner, kind, x, y) {
    let u = entity_create(0);
    entity_set_position(u, vec3(fixed(x), fixed(y), fixed(ground_height(x, y))));
    entity_set_field(u, "owner", fixed(owner));
    entity_set_field(u, "kind", fixed(kind));
    entity_set_field(u, "hp", fixed(if kind == 0 { worker_hp() } else { soldier_hp() }));
    entity_set_model(u, kind); // model ids mirror unit kinds (0/1)
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
            if r.len() == 4 {
                box_select(r);
            }
        }
    } else if id == "order" && down {
        let g = pick_ground();
        if g != () {
            // Context order, WC3 style, one command per selected unit: an
            // enemy unit under the cursor → ATTACK it; an enemy building
            // or a tree under the ground point → ATTACK (painted blocks
            // are invisible to pick_entity, so those two are cell tests);
            // a gold mine → HARVEST; open ground → MOVE.
            let victim = pick_entity();
            if !(victim >= 0 && is_unit_local(victim) && !mine(victim)) {
                victim = hostile_at(g);
            }
            let m = mine_at(g);
            for e in highlighted_all() {
                if mine(e) {
                    if victim >= 0 {
                        submit_command(5, e, vec3(g.x, g.y, fixed(victim)));
                    } else if m >= 0 {
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
    } else if id == "train2" && down {
        // Queue a soldier at this player's barracks.
        submit_command(4, -1, vec3(fixed(0), fixed(0), fixed(0)));
    }
}

fn is_unit_local(e) {
    for u in entities_of(0) {
        if u == e {
            return true;
        }
    }
    false
}

/// The attackable thing whose block/cell contains ground point `g`: an
/// enemy building (its 3×3 block) or a tree (its cell); `-1` otherwise.
fn hostile_at(g) {
    let cx = cell(g.x);
    let cy = cell(g.y);
    for b in entities_of(2) {
        if to_int(entity_field(b, "owner")) != me() {
            let p = entity_position(b);
            if cx >= cell(p.x) - 1 && cx <= cell(p.x) + 1 && cy >= cell(p.y) - 1 && cy <= cell(p.y) + 1 {
                return b;
            }
        }
    }
    for t in entities_of(1) {
        let p = entity_position(t);
        if cx == cell(p.x) && cy == cell(p.y) {
            return t;
        }
    }
    -1
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

/// Resolve a finished select gesture. `q` is the host's screen-aligned drag
/// quad — four sim-ground corners wound around the rect (`drag_end`). A tiny
/// quad is a CLICK — single-select whatever unit is under the cursor (or
/// clear); a real drag selects every own unit inside the (rotated) box. We
/// test the oriented quad directly, so a unit is picked iff it sits under the
/// on-screen rectangle, whatever the camera yaw.
fn box_select(q) {
    // Opposite corners q[0]/q[2] are the press/release diagonal.
    if approx_dist(q[2].x - q[0].x, q[2].y - q[0].y) < ratio(35, 100) {
        let e = pick_entity();
        if e >= 0 && mine(e) {
            highlight(e);
        } else {
            highlight_clear();
        }
        return;
    }
    highlight_clear();
    for u in entities_of(0) {
        if !mine(u) {
            continue;
        }
        let p = entity_position(u);
        if in_quad(q, p.x, p.y) {
            highlight_add(u);
        }
    }
}

/// Is sim point `(px, py)` inside the convex quad `q` (4 corners wound around
/// it)? By the sign of each edge's cross product with the point: inside iff the
/// point is on the same side of all four edges. No division, and winding-
/// agnostic (works whichever way the host wound the corners). A point exactly
/// on an edge (cross 0) counts as inside.
fn in_quad(q, px, py) {
    let pos = false;
    let neg = false;
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        let cr = (b.x - a.x) * (py - a.y) - (b.y - a.y) * (px - a.x);
        if cr > fixed(0) { pos = true; }
        if cr < fixed(0) { neg = true; }
    }
    !(pos && neg)
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
        train_at(player, 0, worker_cost());
    } else if verb == 4 {
        train_at(player, 1, soldier_cost());
    } else if verb == 5 {
        if !is_unit(target) || to_int(entity_field(target, "owner")) != player {
            return;
        }
        let v = to_int(arg.z);
        if !attackable(v, player) {
            return;
        }
        entity_set_field(target, "task", fixed(4));
        entity_set_field(target, "victim", fixed(v));
        let p = entity_position(v);
        walk_to(target, p.x, p.y);
    }
}

/// Queue one unit at `player`'s building of `bkind` (0 hall → worker,
/// 1 barracks → soldier), gold + supply gated — the deduction runs inside
/// the command fold, so every peer's purse agrees.
fn train_at(player, bkind, cost) {
    let h = building_of(player, bkind);
    if h < 0 {
        return;
    }
    let g = entities_of(4)[0];
    let purse = "gold" + player;
    let queued = to_int(entity_field(h, "queue"));
    if to_int(entity_field(g, purse)) < cost {
        return;
    }
    if count_units(player) + total_queued(player) >= supply_cap() {
        return;
    }
    entity_set_field(g, purse, entity_field(g, purse) - fixed(cost));
    entity_set_field(h, "queue", fixed(queued + 1));
}

fn total_queued(player) {
    let n = 0;
    for b in entities_of(2) {
        if to_int(entity_field(b, "owner")) == player {
            n += to_int(entity_field(b, "queue"));
        }
    }
    n
}

/// May `player`'s unit attack entity `v`? Enemy units, enemy buildings,
/// and any tree.
fn attackable(v, player) {
    if is_unit(v) {
        return to_int(entity_field(v, "owner")) != player;
    }
    for b in entities_of(2) {
        if b == v {
            return to_int(entity_field(v, "owner")) != player;
        }
    }
    for t in entities_of(1) {
        if t == v {
            return true;
        }
    }
    false
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
fn hall_of(player) { building_of(player, 0) }

/// `player`'s building of `kind`, or `-1`.
fn building_of(player, kind) {
    for b in entities_of(2) {
        if to_int(entity_field(b, "kind")) == kind && to_int(entity_field(b, "owner")) == player {
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
    let game = entities_of(4)[0];
    if to_int(entity_field(game, "winner")) != 0 {
        build_ui(); // game over: the world freezes, the banner stays
        return;
    }
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
    // A hall delivers workers, a barracks soldiers (unit kind = building kind).
    spawn_unit(owner, to_int(entity_field(b, "kind")), to_int(out.x), to_int(out.y));
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
    ui_text(8, 38, "LMB select · drag box · RMB move/harvest/attack · Q worker · E soldier", 8);
    let winner = to_int(entity_field(g, "winner"));
    if winner != 0 {
        let msg = if winner == p + 1 { "VICTORY — the enemy hall has fallen" } else { "DEFEAT — your hall has fallen" };
        ui_text(w / 4, ui_height() / 2 - 20, msg, 20);
    }
}

/// Advance one unit: run the combat / harvest FSM at its stations, then
/// walk it along its nav waypoints under the walk rule.
fn step_unit(u) {
    let owner = to_int(entity_field(u, "owner"));
    entity_set_tint(u, team_tint(owner));

    // Cooldown always runs down, even mid-walk.
    let cd = to_int(entity_field(u, "atkcd"));
    if cd > 0 {
        entity_set_field(u, "atkcd", fixed(cd - 1));
    }

    // Idle soldiers pick their own fights (WC3 auto-acquire): the nearest
    // enemy UNIT inside aggro range (deterministic ascending-id scan).
    if to_int(entity_field(u, "task")) == 0 && to_int(entity_field(u, "kind")) == 1 {
        let v = nearest_enemy_unit(u, owner);
        if v >= 0 {
            entity_set_field(u, "task", fixed(4));
            entity_set_field(u, "victim", fixed(v));
            let p = entity_position(v);
            walk_to(u, p.x, p.y);
        }
    }

    // Station: fighting (task 4) — swing in reach, chase otherwise.
    if to_int(entity_field(u, "task")) == 4 {
        step_attack(u, owner);
        if to_int(entity_field(u, "has_dest")) == 0 {
            return; // in reach (or the fight ended) — no walking this tick
        }
    }

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
            // Even at rest, bodies push apart — a crowd that has arrived
            // relaxes into a loose cluster instead of one stacked sprite.
            let pos = entity_position(u);
            let sep = separation(u, pos);
            if nonzero(sep.x) || nonzero(sep.y) {
                try_move(u, sep.x, sep.y);
            }
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
    let pos2 = entity_position(u);
    let v = nvel(dx, dy, unit_speed());
    let sep = separation(u, pos2);
    try_move(u, v.x + sep.x, v.y + sep.y);
    entity_set_field(u, "face", dir_yaw(dx, dy));
    entity_set_facing(u, entity_field(u, "face"));
    entity_set_anim(u, "run");

    // Stall detection: a crowd converging on one point can never ALL land
    // within the strict arrival radius (separation holds them apart), and
    // a jammed corridor can wedge a walker outright. If real progress
    // stops: near the dest → call it arrived (the WC3 "close enough");
    // far from it → replan around whatever now blocks the way.
    let np = entity_position(u);
    let moved = approx_dist(np.x - pos2.x, np.y - pos2.y);
    if moved < unit_speed() * ratio(1, 3) {
        let stall = to_int(entity_field(u, "stall")) + 1;
        let fx = entity_field(u, "dest_x") - np.x;
        let fy = entity_field(u, "dest_y") - np.y;
        if stall > 8 && approx_dist(fx, fy) < fixed(2) {
            entity_set_field(u, "stall", fixed(0));
            entity_set_field(u, "has_dest", fixed(0));
            entity_set_anim(u, "idle");
        } else if stall > 30 {
            entity_set_field(u, "stall", fixed(0));
            replan(u);
        } else {
            entity_set_field(u, "stall", fixed(stall));
        }
    } else {
        entity_set_field(u, "stall", fixed(0));
    }
}

/// The nearest living enemy unit within aggro range of `u`, or `-1`.
/// Ascending-id iteration + strict `<` keep the pick deterministic.
fn nearest_enemy_unit(u, owner) {
    let up = entity_position(u);
    let best = -1;
    let bestd = fixed(0);
    for e in entities_of(0) {
        if to_int(entity_field(e, "owner")) == owner {
            continue;
        }
        let p = entity_position(e);
        let d = approx_dist(p.x - up.x, p.y - up.y);
        if d < aggro_range() && (best < 0 || d < bestd) {
            best = e;
            bestd = d;
        }
    }
    best
}

/// One combat tick for a task-4 unit: victim gone → stand down; in reach
/// (Chebyshev 2 cells — the ring of a building block, generous for a
/// unit) → stop and swing on cooldown; out of reach → keep the chase
/// pointed at where the victim IS now.
fn step_attack(u, owner) {
    let v = to_int(entity_field(u, "victim"));
    if !attackable(v, owner) {
        entity_set_field(u, "task", fixed(0));
        entity_set_field(u, "has_dest", fixed(0));
        return;
    }
    let vp = entity_position(v);
    if near(u, v, 2) {
        entity_set_field(u, "has_dest", fixed(0)); // planted for the swing
        let pos = entity_position(u);
        entity_set_field(u, "face", dir_yaw(vp.x - pos.x, vp.y - pos.y));
        entity_set_facing(u, entity_field(u, "face"));
        if to_int(entity_field(u, "atkcd")) == 0 {
            entity_set_field(u, "atkcd", fixed(attack_cd()));
            let dmg = if to_int(entity_field(u, "kind")) == 1 { soldier_dmg() } else { worker_dmg() };
            deal_damage(v, dmg);
        }
        // Play "attack" only for a window after each swing, dropping back
        // to idle in between: the host restarts a clip on the state
        // TRANSITION (`set_actor_state` fires on change only), so this
        // idle gap is what makes every swing replay the animation from
        // its first frame instead of freezing on the last. Workers swing
        // too (chopping trees / desperate defence) — both actors now
        // carry an "attack" state.
        let swinging = to_int(entity_field(u, "atkcd")) > attack_cd() - 18;
        entity_set_anim(u, if swinging { "attack" } else { "idle" });
        return;
    }
    // Chase: re-aim the walk whenever the victim drifted off our dest.
    let dx = entity_field(u, "dest_x") - vp.x;
    let dy = entity_field(u, "dest_y") - vp.y;
    if to_int(entity_field(u, "has_dest")) == 0 || approx_dist(dx, dy) > fixed(2) {
        walk_to(u, vp.x, vp.y);
    }
}

/// Apply `dmg` to entity `v` (unit, building or tree) and resolve its
/// death: units despawn; a felled tree despawns, `voxel_clear`s its
/// column (walking + nav open up in the same breath — chasers replan on
/// their next waypoint) and leaves a dirt stump voxel; a destroyed hall
/// ends the game.
fn deal_damage(v, dmg) {
    let hp = to_int(entity_field(v, "hp")) - dmg;
    if hp > 0 {
        entity_set_field(v, "hp", fixed(hp));
        return;
    }
    if is_unit(v) {
        entity_despawn(v);
        return;
    }
    for t in entities_of(1) {
        if t == v {
            let p = entity_position(v);
            let x = cell(p.x);
            let y = cell(p.y);
            // Clear from z 1 up: the trunk's BASE voxel survives as a
            // stump and the floor tile underneath is never touched (a
            // z-0 clear would erase it — a hole in the world). The store
            // keeps the column at top 0 → walkable (Δ0 from the lowland).
            voxel_clear(x, y, 1);
            entity_despawn(v);
            return;
        }
    }
    // A building fell: raze its 3×3 block down to one voxel above its
    // base — the surviving foundation layer reads as a ruin scar AND keeps
    // the plateau surface intact (walkable, Δ1), so the site opens for
    // movement + nav. A hall decides the game.
    let kind = to_int(entity_field(v, "kind"));
    let owner = to_int(entity_field(v, "owner"));
    let p = entity_position(v);
    let bx = cell(p.x);
    let by = cell(p.y);
    let base = to_int(p.z);
    for dx in -1..=1 {
        for dy in -1..=1 {
            voxel_clear(bx + dx, by + dy, base + 1);
        }
    }
    entity_despawn(v);
    if kind == 0 {
        let g = entities_of(4)[0];
        entity_set_field(g, "winner", fixed(2 - owner)); // the OTHER player + 1
    }
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
