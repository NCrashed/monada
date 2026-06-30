//! A Hades-style co-op action-RPG demo, entirely in the map.
//!
//! The engine knows no genre: this script paints a voxel arena, defines
//! animated billboard actors (GIFs), and runs all gameplay in `tick` on the
//! deterministic fixed-point sim. The host injects one real-time input
//! command per tick (verb 0): `target` = button bitmask (bit0 attack/LMB,
//! bit1 dodge/Space), `arg = vec3(move_x, move_y, aim)`. Render-side calls
//! (`entity_set_anim`, `entity_set_facing`, `camera_focus`) never touch the
//! hashed world, so they can't desync a peer.
//!
//! Archetypes register in order, so their ids are fixed: PLAYER = 0,
//! ENEMY = 1 (reused in `tick` without threading them through).

// --- tuning (all fixed-point; no_float Rhai forbids IEEE) ---------------
// NB: Rhai functions are pure — they do NOT see top-level `const`s — so the
// archetype ids (PLAYER = 0, ENEMY = 1, registered in that order), the arena
// size (20; walkable [1, 18]), and the button bits (attack = 1, dodge = 2)
// are written as literals throughout rather than named constants.

fn player_speed()  { ratio(18, 100) }   // sim cells / tick (30 Hz)
fn enemy_speed()   { ratio(10, 100) }
fn dodge_speed()   { ratio(55, 100) }
fn dodge_ticks()   { fixed(8) }
fn dodge_cool()    { fixed(25) }
fn attack_ticks()  { fixed(10) }
fn hurt_ticks()    { fixed(8) }
fn death_ticks()   { fixed(12) }
fn enemy_atkcd()   { fixed(30) }

// --- small fixed-point helpers ------------------------------------------
fn absf(x) { if x < fixed(0) { -x } else { x } }

fn nonzero(x) { x > fixed(0) || x < fixed(0) }

/// 8-way facing yaw (radians, sim space: +x east, +y north, CCW) from a
/// direction vector — no `atan2` needed, the renderer bins to 8 sprites
/// anyway. Sector k → yaw = k·tau/8: 0=E,1=NE,2=N,3=NW,4=W,5=SW,6=S,7=SE.
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

/// The camera's orbit yaw — MUST match `camera_angle` in `init`, since the
/// player moves relative to the view.
fn cam_yaw() { ratio(8, 10) }

/// Rotate a `(right, forward)` input — W/D in screen space — into the
/// sim-space move direction, so W goes "up the screen" rather than along
/// world +y. Accounts for the camera yaw and the render-side world-X mirror
/// (`world_of` flips x, so a sim heading `(dx, dy)` is seen as `(-dx, dy)`):
/// up-screen is world `[cos y, sin y]`, screen-right is world `[-sin y,
/// cos y]`, and undoing the mirror gives the sim vector below. If W ends up
/// going the wrong way, negate `cam_yaw`; if A/D are swapped, negate `rx`.
fn cam_relative(rx, fy) {
    let y = cam_yaw();
    let c = cos(y);
    let s = sin(y);
    vec3(rx * s - fy * c, fy * s + rx * c, fixed(0))
}

/// Is sim cell `(cx, cy)` a wall / out of bounds?
fn blocked(cx, cy) {
    if cx < 1 || cy < 1 || cx > 18 || cy > 18 {
        return true;
    }
    voxel_solid(cx, cy, 1)
}

/// Whether a mover's footprint — a small box of radius `r` around `(px, py)`
/// — is clear of walls. Checking the four corners (rather than just the
/// centre cell) stops the body before it overlaps an obstacle, so a ~1-cell-
/// wide character can't clip into or squeeze through a pillar.
fn clear(px, py) {
    let r = ratio(40, 100);
    !(blocked(to_int(px - r), to_int(py - r))
        || blocked(to_int(px + r), to_int(py - r))
        || blocked(to_int(px - r), to_int(py + r))
        || blocked(to_int(px + r), to_int(py + r)))
}

/// Move entity `e` by `(vx, vy)`, axis-separated so it slides along walls,
/// and reseat it on the ground.
fn try_move(e, vx, vy) {
    let pos = entity_position(e);
    let nx = pos.x + vx;
    let fx = if clear(nx, pos.y) { nx } else { pos.x };
    let ny = pos.y + vy;
    let fy = if clear(fx, ny) { ny } else { pos.y };
    let gz = ground_height(to_int(fx), to_int(fy));
    entity_set_position(e, vec3(fx, fy, fixed(gz)));
}

// --- setup --------------------------------------------------------------
// Archetype ids by registration order: PLAYER = 0, ENEMY = 1, GAME = 2.
// Model ids by `model_actor` order: hero = 0, enemy = 1.
fn init() {
    archetype(["hp", "ix", "iy", "btn", "face", "atk", "dodge", "ddx", "ddy", "cool", "hurt", "dead"]);
    archetype(["hp", "face", "atkcd", "hurt", "dead"]);
    archetype(["wave", "timer"]);
    archetype(["life"]); // 3 = attack-burst effect (purely cosmetic, short-lived)

    set_light(vec3(ratio(-6, 10), ratio(-3, 10), ratio(7, 10)), ratio(8, 10));
    set_sky("assets/skybox.png");

    // Arena: floor (saturated grass green) + rim walls (clean stone grey,
    // raised) + a couple of pillars (warm orange, tall enough to read as 3D
    // obstacles against the grass). `z` is height above the floor (one cell =
    // SCALE voxels), so a pillar of height ~12 stands roughly chest-high.
    // Painting also feeds collision + ground_height.
    voxel_fill(0, 0, 0, 19, 19, 0, 0x804C_A64C);
    voxel_fill(0, 0, 0, 19, 0, 6, 0x807A_7E86);
    voxel_fill(0, 19, 0, 19, 19, 6, 0x807A_7E86);
    voxel_fill(0, 0, 0, 0, 19, 6, 0x807A_7E86);
    voxel_fill(19, 0, 0, 19, 19, 6, 0x807A_7E86);
    voxel_fill(6, 6, 0, 7, 7, 12, 0x80C8_7832);
    voxel_fill(12, 12, 0, 13, 13, 12, 0x80C8_7832);

    // Register the animated billboard actors (ids by call order: 0 = hero,
    // 1 = enemy, 2 = player attack-burst, 3 = enemy attack-burst).
    model_actor("assets/char/hero", ["idle", "run", "attack", "dodge", "hurt", "death"]);
    model_actor("assets/char/enemy", ["idle", "run", "attack", "dodge", "hurt", "death"]);
    model_actor("assets/char/fx_player", ["burst"]);
    model_actor("assets/char/fx_enemy", ["burst"]);

    // Wave director singleton, then the first wave. Heroes spawn lazily as
    // each player's input first arrives (co-op join), so no player count is
    // needed up front.
    let g = entity_create(2);
    next_wave(g);

    // Hades-style fixed camera, pulled in close on the hero; follows the
    // local player each tick.
    camera_angle(ratio(8, 10), ratio(10, 10));
    camera_dist(fixed(150));
    camera_focus(vec3(fixed(10), fixed(10), fixed(0)));
    status("action-RPG demo — WASD move, Space dodge, LMB attack");
}

/// Advance to the next wave: spawn `2 + wave` enemies at random rim cells
/// (deterministic — the shared sim RNG, so every peer agrees), and reset the
/// inter-wave timer.
fn next_wave(g) {
    let w = to_int(entity_field(g, "wave")) + 1;
    entity_set_field(g, "wave", fixed(w));
    entity_set_field(g, "timer", fixed(60));
    let n = 2 + w;
    let i = 0;
    while i < n {
        let side = rng_below(4);
        // Spawn one cell in from the rim (cells 2..17): a mover's collision
        // footprint (radius ~0.4) would otherwise overlap the bound wall at
        // cell 0/19 and wedge it in place.
        let a = 2 + rng_below(16);
        let x = 10;
        let y = 10;
        if side == 0 {
            x = 2;
            y = a;
        } else if side == 1 {
            x = 17;
            y = a;
        } else if side == 2 {
            x = a;
            y = 2;
        } else {
            x = a;
            y = 17;
        }
        spawn_enemy_at(x, y);
        i += 1;
    }
}

fn spawn_enemy_at(x, y) {
    let e = entity_create(1);
    entity_set_position(e, vec3(fixed(x), fixed(y), fixed(0)));
    entity_set_field(e, "hp", fixed(50));
    entity_set_model(e, 1); // enemy actor model
    entity_set_anim(e, "idle");
}

/// Spawn a short-lived attack-burst effect at sim point `(px, py)` (both
/// `Fixed`), telegraphing the hit area. `model` = 2 (player) or 3 (enemy).
fn spawn_fx(px, py, model) {
    let e = entity_create(3);
    entity_set_position(e, vec3(px, py, fixed(0)));
    entity_set_field(e, "life", fixed(8));
    entity_set_model(e, model);
    entity_set_anim(e, "burst");
}

/// Spawn this peer's hero the first time we hear from `player` (co-op join);
/// heroes start spread along the arena centre line. Deterministic: it runs
/// inside `command`, which every peer applies in lockstep.
fn ensure_hero(player) {
    while entities_of(0).len() < player + 1 {
        let idx = entities_of(0).len();
        let p = entity_create(0);
        entity_set_position(p, vec3(fixed(7 + idx * 2), fixed(10), fixed(0)));
        entity_set_field(p, "hp", fixed(100));
        entity_set_model(p, 0); // hero actor model
        entity_set_anim(p, "idle");
    }
}

// --- input: store this peer's intent on its player entity ---------------
fn command(player, verb, target, arg) {
    if verb == 0 {
        ensure_hero(player);
        let p = entities_of(0)[player];
        entity_set_field(p, "ix", arg.x);
        entity_set_field(p, "iy", arg.y);
        entity_set_field(p, "btn", fixed(target));
    }
}

// --- one simulation tick ------------------------------------------------
fn tick() {
    for p in entities_of(0) {
        step_player(p);
    }
    for e in entities_of(1) {
        step_enemy(e);
    }
    for fx in entities_of(3) {
        let life = entity_field(fx, "life") - fixed(1);
        if life > fixed(0) {
            entity_set_field(fx, "life", life);
        } else {
            entity_despawn(fx);
        }
    }
    wave_director();
    follow_camera();
    update_status();
}

/// Count living (not dying) enemies.
fn living_enemies() {
    let n = 0;
    for e in entities_of(1) {
        if entity_field(e, "dead") > fixed(0) {
            continue;
        }
        if entity_field(e, "hp") > fixed(0) {
            n += 1;
        }
    }
    n
}

/// When the arena is clear, count down the inter-wave timer, then send the
/// next (larger) wave.
fn wave_director() {
    let g = entities_of(2)[0];
    if living_enemies() == 0 {
        let t = entity_field(g, "timer");
        if t > fixed(0) {
            entity_set_field(g, "timer", t - fixed(1));
        } else {
            next_wave(g);
        }
    }
}

fn step_player(p) {
    let hp = entity_field(p, "hp");
    if !(hp > fixed(0)) {
        entity_set_anim(p, "death");
        return;
    }

    let ix = entity_field(p, "ix");
    let iy = entity_field(p, "iy");
    let btn = to_int(entity_field(p, "btn"));
    let atk = entity_field(p, "atk");
    let dodge = entity_field(p, "dodge");
    let cool = entity_field(p, "cool");
    let moving = nonzero(ix) || nonzero(iy);
    // Camera-relative intent: W is "up the screen", not world +y.
    let m = cam_relative(ix, iy);
    let mx = m.x;
    let my = m.y;

    // Begin a dodge (i-frames) on the button edge while moving and off-cooldown.
    if !(dodge > fixed(0)) && (btn & 2) != 0 && moving && !(cool > fixed(0)) {
        let u = nvel(mx, my, fixed(1));
        entity_set_field(p, "ddx", u.x);
        entity_set_field(p, "ddy", u.y);
        entity_set_field(p, "dodge", dodge_ticks());
        entity_set_field(p, "cool", dodge_cool());
        entity_set_field(p, "face", dir_yaw(mx, my));
        dodge = dodge_ticks();
    }

    // Velocity.
    if dodge > fixed(0) {
        try_move(p, entity_field(p, "ddx") * dodge_speed(), entity_field(p, "ddy") * dodge_speed());
    } else {
        let u = nvel(mx, my, player_speed());
        try_move(p, u.x, u.y);
        if moving {
            entity_set_field(p, "face", dir_yaw(mx, my));
        }
    }

    // Attack on the button edge: a melee swing in the facing direction.
    if (btn & 1) != 0 && !(atk > fixed(0)) && !(dodge > fixed(0)) {
        entity_set_field(p, "atk", attack_ticks());
        atk = attack_ticks();
        let face = entity_field(p, "face");
        let pos = entity_position(p);
        let hx = pos.x + cos(face) * ratio(15, 10);
        let hy = pos.y + sin(face) * ratio(15, 10);
        spawn_fx(hx, hy, 2); // telegraph the swing's hit area
        for en in entities_of(1) {
            if entity_field(en, "dead") > fixed(0) {
                continue;
            }
            let ep = entity_position(en);
            let dx = ep.x - hx;
            let dy = ep.y - hy;
            if dx * dx + dy * dy < ratio(20, 10) {
                entity_set_field(en, "hp", entity_field(en, "hp") - fixed(30));
                entity_set_field(en, "hurt", fixed(6));
            }
        }
    }

    // Timers.
    dec(p, "atk");
    dec(p, "dodge");
    dec(p, "cool");
    dec(p, "hurt");

    // Render-side animation + facing (never hashed).
    let anim = "idle";
    if entity_field(p, "dodge") > fixed(0) {
        anim = "dodge";
    } else if entity_field(p, "atk") > fixed(0) {
        anim = "attack";
    } else if entity_field(p, "hurt") > fixed(0) {
        anim = "hurt";
    } else if moving {
        anim = "run";
    }
    entity_set_anim(p, anim);
    entity_set_facing(p, entity_field(p, "face"));
}

fn step_enemy(e) {
    let dead = entity_field(e, "dead");
    if dead > fixed(0) {
        entity_set_anim(e, "death");
        entity_set_field(e, "dead", dead - fixed(1));
        if !(dead - fixed(1) > fixed(0)) {
            entity_despawn(e);
        }
        return;
    }
    if !(entity_field(e, "hp") > fixed(0)) {
        entity_set_field(e, "dead", death_ticks());
        entity_set_anim(e, "death");
        return;
    }

    let ep = entity_position(e);
    let target = nearest_player(ep);
    if target < 0 {
        entity_set_anim(e, "idle");
        return;
    }
    let pp = entity_position(target);
    let dx = pp.x - ep.x;
    let dy = pp.y - ep.y;
    entity_set_field(e, "face", dir_yaw(dx, dy));

    let atkcd = entity_field(e, "atkcd");
    if dx * dx + dy * dy < ratio(16, 10) {
        if !(atkcd > fixed(0)) {
            // Dodging players have i-frames.
            if !(entity_field(target, "dodge") > fixed(0)) {
                entity_set_field(target, "hp", entity_field(target, "hp") - fixed(8));
                entity_set_field(target, "hurt", hurt_ticks());
            }
            entity_set_field(e, "atkcd", enemy_atkcd());
            // Burst one cell in front of the mob, telegraphing its swing.
            let nd = nvel(dx, dy, fixed(1));
            spawn_fx(ep.x + nd.x, ep.y + nd.y, 3);
        }
        entity_set_anim(e, "attack");
    } else {
        let v = nvel(dx, dy, enemy_speed());
        try_move(e, v.x, v.y);
        entity_set_anim(e, "run");
    }
    dec(e, "atkcd");

    if entity_field(e, "hurt") > fixed(0) {
        dec(e, "hurt");
        entity_set_anim(e, "hurt");
    }
    entity_set_facing(e, entity_field(e, "face"));
}

/// Nearest living player to `from`, or `-1`.
fn nearest_player(from) {
    let best = -1;
    let bestd = fixed(0);
    let first = true;
    for tp in entities_of(0) {
        if !(entity_field(tp, "hp") > fixed(0)) {
            continue;
        }
        let pp = entity_position(tp);
        let dx = pp.x - from.x;
        let dy = pp.y - from.y;
        let d2 = dx * dx + dy * dy;
        if first || d2 < bestd {
            best = tp;
            bestd = d2;
            first = false;
        }
    }
    best
}

/// Decrement a tick timer field, flooring at 0.
fn dec(e, name) {
    let t = entity_field(e, name);
    if t > fixed(0) {
        entity_set_field(e, name, t - fixed(1));
    }
}

/// Keep the camera on this peer's player (hotseat → player 0).
fn follow_camera() {
    let players = entities_of(0);
    let lp = local_player();
    let idx = if lp < 0 { 0 } else { lp };
    if idx < players.len() {
        camera_focus(entity_position(players[idx]));
    }
}

fn update_status() {
    let alive = living_enemies();
    let any_player = false;
    for p in entities_of(0) {
        if entity_field(p, "hp") > fixed(0) {
            any_player = true;
        }
    }
    let w = to_int(entity_field(entities_of(2)[0], "wave"));
    if !any_player {
        status("You died on wave " + w);
    } else if alive == 0 {
        status("Wave " + w + " cleared — next incoming");
    } else {
        status("Wave " + w + " — enemies: " + alive);
    }
}
