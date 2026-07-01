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
fn dodge_ticks()   { fixed(5) }   // shorter roll (was 8) — less distance + i-frames
fn dodge_cool()    { fixed(25) }
// Attack commitment window: while it counts down the "attack" state is held
// (highest anim priority below dodge), so the one-shot swing GIF plays to its
// last frame instead of being cut back to idle/run. Set this ≥ your attack
// animation's length in ticks (30 Hz); if the clip is shorter it just holds its
// final frame. It also gates the next swing (no re-attack until it hits 0).
fn attack_ticks()  { fixed(18) }
fn hurt_ticks()    { fixed(8) }
fn death_ticks()   { fixed(45) }   // ~1.5s: the death pose (one-shot) stays visible
fn enemy_atkcd()   { fixed(30) }
// Boid separation: enemies within `sep_radius` push apart so a chasing pack
// spreads into a crowd instead of stacking on one point. `sep_gain` scales the
// push, `sep_max` caps it (< enemy_speed, so the chase still dominates).
fn sep_radius()    { ratio(12, 10) }    // ~1.2 cells (mobs are ~1.3 wide)
fn sep_gain()      { ratio(6, 100) }
fn sep_max()       { ratio(8, 100) }

// --- intro dialogue ------------------------------------------------------
// The goddess's synth "voice": engine `play_blip(wave, freq_hz, dur_ms, gain)`
// fired once every `blip_every` typed glyphs (wave 2 = triangle, soft). Tune
// the timbre/pitch here.
fn goddess_wave()  { 2 }              // 0 square 1 saw 2 triangle 3 sine 4 noise
fn goddess_freq()  { 380 }            // base Hz (a small per-glyph warble is added)
fn goddess_dur()   { 45 }             // ms per blip
fn goddess_gain()  { ratio(22, 100) } // 0.22
fn blip_every()    { 2 }              // one blip per N glyphs
fn reveal_hold()   { 90 }             // ticks to hold the full line before play starts

/// The intro line, revealed by the typewriter (`.len()` glyph count +
/// `.sub_string`).
fn intro_text() {
    "Hero, your task is to hold out as long as you can against the forces of evil. Move with WASD, Space to dodge, LMB to swing your sword."
}

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

/// Cheap `sqrt`-free distance (octagonal `max + min/2`) — enough for the
/// separation weighting (no_float Rhai has no `sqrt`).
fn approx_dist(dx, dy) {
    let ax = absf(dx);
    let ay = absf(dy);
    if ax > ay { ax + ay * ratio(1, 2) } else { ay + ax * ratio(1, 2) }
}

/// Boid separation for enemy `e` at `ep`: sum a push away from every other
/// living enemy within `sep_radius` (harder the closer it is), scaled by
/// `sep_gain` and clamped to `sep_max`. Deterministic (fixed-point, ascending-
/// id iteration). Returns a velocity `vec3` to add to the chase.
fn separation(e, ep) {
    let r = sep_radius();
    let sx = fixed(0);
    let sy = fixed(0);
    for o in entities_of(1) {
        if o == e || entity_field(o, "dead") > fixed(0) {
            continue;
        }
        let op = entity_position(o);
        let dx = ep.x - op.x; // pointing away from the neighbour
        let dy = ep.y - op.y;
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

/// Sim position → the cell index it falls in. Integer sim coords are cell
/// CENTRES: `world_of` seats a sprite at `-(x+0.5)·SCALE` and the tile/voxels
/// for cell C are centred on sim `x = C`, so a position rounds to its nearest
/// cell to match what's drawn. Flooring (plain `to_int`) would put the collision
/// boundary half a cell off the visible tile edge — you'd sink into an obstacle
/// on one side and hit an invisible wall in empty space on the other.
fn cell(v) { to_int(v + ratio(1, 2)) }

/// Whether a mover's footprint — a small box of radius `r` around `(px, py)`
/// — is clear of walls. Checking the four corners (rather than just the
/// centre cell) stops the body before it overlaps an obstacle, so a ~1-cell-
/// wide character can't clip into or squeeze through a pillar.
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
// Archetype ids by registration order: PLAYER = 0, ENEMY = 1, GAME = 2.
// Model ids by `model_actor` order: hero = 0, enemy = 1.
fn init() {
    archetype(["hp", "ix", "iy", "btn", "face", "atk", "dodge", "ddx", "ddy", "cool", "hurt", "dead", "aim", "swing"]);
    archetype(["hp", "face", "atkcd", "hurt", "dead"]);
    archetype(["wave", "timer", "score", "dlg", "started"]); // dlg = typewriter index, started = intro over
    archetype(["life"]); // 3 = attack-burst effect (purely cosmetic, short-lived)
    archetype(["life", "sx", "sy"]); // 4 = summon telegraph: a pending spawn at (sx, sy)

    set_light(vec3(ratio(-6, 10), ratio(-3, 10), ratio(7, 10)), ratio(8, 10));
    set_sky("assets/skybox.png");

    // Autotiled flat floor: grass (type 1) over dirt (type 0), blended by the
    // marching-squares transition sheet. Terrain is render-only (the floor is
    // walkable), so collision/determinism are unchanged.
    transition(0, 1, "assets/tiles/grass-dirt.png");
    terrain_fill(0, 0, 19, 19, 1); // grass everywhere…
    terrain_fill(4, 4, 7, 8, 0); // …with a couple of dirt patches
    terrain_fill(13, 11, 16, 15, 0);
    terrain_fill(9, 2, 11, 3, 0);
    terrain_blit(1); // base outside the arena = grass

    // Rim walls + a couple of pillars, in stone (raised solid blocks; ~12 cells
    // of height for the pillars reads as a 3D obstacle). These feed collision.
    let stone = tile("assets/tiles/stone.png");
    tile_fill(0, 0, 0, 19, 0, 6, stone);
    tile_fill(0, 19, 0, 19, 19, 6, stone);
    tile_fill(0, 0, 0, 0, 19, 6, stone);
    tile_fill(19, 0, 0, 19, 19, 6, stone);
    tile_fill(6, 6, 0, 7, 7, 12, stone);
    tile_fill(12, 12, 0, 13, 13, 12, stone);

    // Register the animated billboard actors (ids by call order: 0 = hero,
    // 1 = enemy, 2 = player attack-burst, 3 = enemy attack-burst, 4 = summon
    // telegraph). The 3rd arg is the rendered height in cells; the host scales
    // the art to it regardless of pixel resolution. FX effects can be a single
    // non-directional GIF (the host loads it for every facing) — the bursts and
    // the summon circle don't depend on view angle.
    model_actor("assets/char/hero", ["idle", "run", "attack", "dodge", "hurt", "death"], ratio(14, 10)); // the knight, a bit bigger
    model_actor("assets/char/enemy", ["idle", "run", "attack", "dodge", "hurt", "death"], ratio(132, 100));
    model_actor("assets/char/fx_player", ["burst"], ratio(16, 10));
    model_actor("assets/char/fx_enemy", ["burst"], ratio(16, 10));
    model_actor("assets/char/fx_enemy", ["summon"], ratio(2, 1)); // 4 = ground summon circle

    // Lower the characters so their visible feet sit in the collision box: the
    // pivot grounds on the trimmed opaque bottom, but the art leaves them a
    // touch high. Hero + enemies drop by the same amount (tune to the art).
    model_drop(0, ratio(35, 100)); // hero
    model_drop(1, ratio(35, 100)); // enemy — same drop as the hero

    // HUD textures — ids follow load order (0..7), used as literals in
    // `build_ui` (like the model ids above). Render-side: the headless oracle
    // returns -1 and every ui_* draw is a no-op there, so this can't desync.
    ui_texture("assets/ui/health-bar-back.png");        // 0
    ui_texture("assets/ui/health-bar-middle.png");      // 1 (red fill)
    ui_texture("assets/ui/health-bar-front.png");       // 2 (frame + heart)
    ui_texture("assets/ui/score-panel.png");            // 3
    ui_texture("assets/ui/wave-panel.png");             // 4
    ui_texture("assets/ui/restart-button.png");         // 5
    ui_texture("assets/ui/restart-button-hover.png");   // 6
    ui_texture("assets/ui/restart-button-pressed.png"); // 7
    ui_texture("assets/ui/dialog-box.png");             // 8 = dialogue panel
    ui_gif("assets/ui/goddes-talking.gif");             // gif 0 = talking portrait

    // Wave director singleton. The first wave is deferred until the intro
    // dialogue finishes (see `advance_dialogue`); heroes spawn lazily as each
    // player's input first arrives (co-op join), no player count needed up front.
    entity_create(2);

    // Hades-style fixed camera, pulled in close on the hero; follows the
    // local player each tick.
    camera_angle(ratio(8, 10), ratio(10, 10));
    camera_dist(fixed(80));
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
    let n = ((2 + w) * 12 + 5) / 10; // ~20% more enemies (×1.2, rounded)
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
        spawn_summon(x, y);
        i += 1;
    }
}

/// Telegraph duration before an enemy materialises (ticks @30 Hz): the summon
/// circle shows for this long so the player can read the spot and step away —
/// no more mobs appearing in your face.
fn summon_ticks() { fixed(45) } // ~1.5s

/// Place a summon telegraph at rim cell `(x, y)`: a ground circle that counts
/// down, then spawns the enemy there (see the summon loop in `tick`).
fn spawn_summon(x, y) {
    let s = entity_create(4);
    entity_set_position(s, vec3(fixed(x), fixed(y), fixed(0)));
    entity_set_field(s, "life", summon_ticks());
    entity_set_field(s, "sx", fixed(x));
    entity_set_field(s, "sy", fixed(y));
    entity_set_model(s, 4); // summon-circle model
    entity_set_anim(s, "summon");
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
        // HUD restart button (bit 16, chosen in `build_ui`): reset the run.
        // It arrives as a lockstep command, so every peer restarts in sync.
        if (target & 16) != 0 {
            do_restart();
            return;
        }
        let p = entities_of(0)[player];
        entity_set_field(p, "ix", arg.x);
        entity_set_field(p, "iy", arg.y);
        entity_set_field(p, "btn", fixed(target));
        entity_set_field(p, "aim", arg.z); // mouse-aim yaw (sim radians)
    }
}

// --- one simulation tick ------------------------------------------------
fn tick() {
    let g = entities_of(2)[0];
    // Intro cutscene: the goddess speaks. Gameplay is frozen (no player step,
    // no enemies) until the dialogue is dismissed and the first wave launches.
    if to_int(entity_field(g, "started")) == 0 {
        advance_dialogue(g);
        follow_camera();
        build_ui();
        return;
    }

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
    // Summon telegraphs: count down, then materialise the enemy at the marked
    // cell and clear the circle.
    for s in entities_of(4) {
        let life = entity_field(s, "life") - fixed(1);
        if life > fixed(0) {
            entity_set_field(s, "life", life);
        } else {
            spawn_enemy_at(to_int(entity_field(s, "sx")), to_int(entity_field(s, "sy")));
            entity_despawn(s);
        }
    }
    wave_director();
    follow_camera();
    update_status();
    build_ui();
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
    // The arena is "clear" only when no enemies AND no pending summons remain
    // (a summon is a mob that hasn't materialised yet).
    if living_enemies() == 0 && entities_of(4).len() == 0 {
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
        play_sound("assets/sounds/hero_roll.mp3");
    }

    // Velocity.
    if dodge > fixed(0) {
        try_move(p, entity_field(p, "ddx") * dodge_speed(), entity_field(p, "ddy") * dodge_speed());
    } else {
        let u = nvel(mx, my, player_speed());
        try_move(p, u.x, u.y);
        // Movement turns you — but NOT mid-swing: while attacking, keep facing
        // the swing direction locked at the click (set below), so you can walk
        // and still swing where you aimed.
        if moving && !(atk > fixed(0)) {
            entity_set_field(p, "face", dir_yaw(mx, my));
        }
    }

    // Footstep loop while running (the roll sound covers a dodge). Requested
    // every tick we're moving; the host keeps the loop alive and stops it
    // shortly after we halt — render-side, so no field / no desync.
    if moving && !(dodge > fixed(0)) {
        play_loop("assets/sounds/hero_run.mp3");
    }

    // Attack on the button edge: a melee swing toward the mouse aim.
    if (btn & 1) != 0 && !(atk > fixed(0)) && !(dodge > fixed(0)) {
        entity_set_field(p, "atk", attack_ticks());
        atk = attack_ticks();
        let aim = entity_field(p, "aim");
        entity_set_field(p, "face", aim); // turn to face the swing
        let pos = entity_position(p);
        let hx = pos.x + cos(aim) * ratio(15, 10);
        let hy = pos.y + sin(aim) * ratio(15, 10);
        spawn_fx(hx, hy, 2); // telegraph the swing's hit area
        // Alternate swing1/swing2 each attack for variety (deterministic toggle).
        let sw = to_int(entity_field(p, "swing"));
        entity_set_field(p, "swing", fixed(1 - sw));
        if sw == 0 {
            play_sound("assets/sounds/hero_swing1.mp3");
        } else {
            play_sound("assets/sounds/hero_swing2.mp3");
        }
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
                // One swing can hit several enemies; the host de-dups this to a
                // single hurt sound (not one roar per enemy).
                play_sound("assets/sounds/enemy_hurt.mp3");
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
    flash(p); // red while the hurt timer runs
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
        let g = entities_of(2)[0];
        entity_set_field(g, "score", entity_field(g, "score") + fixed(10));
        entity_set_field(e, "dead", death_ticks());
        entity_set_anim(e, "death");
        entity_set_tint(e, 0xffffff); // clear the hit-flash for the corpse
        play_sound("assets/sounds/enemy_death.mp3"); // de-duped if a wave dies at once
        return;
    }

    let ep = entity_position(e);
    let target = nearest_player(ep);
    if target < 0 {
        entity_set_anim(e, "idle");
        flash(e);
        return;
    }
    let pp = entity_position(target);
    let dx = pp.x - ep.x;
    let dy = pp.y - ep.y;
    entity_set_field(e, "face", dir_yaw(dx, dy));

    // Push away from crowding neighbours (added to the chase, or applied alone
    // while attacking so the pack surrounds the player instead of stacking).
    let sep = separation(e, ep);

    let atkcd = entity_field(e, "atkcd");
    if dx * dx + dy * dy < ratio(16, 10) {
        if !(atkcd > fixed(0)) {
            // Dodging players have i-frames.
            if !(entity_field(target, "dodge") > fixed(0)) {
                let nhp = entity_field(target, "hp") - fixed(8);
                entity_set_field(target, "hp", nhp);
                entity_set_field(target, "hurt", hurt_ticks());
                if !(nhp > fixed(0)) {
                    play_sound("assets/sounds/hero_pain.mp3"); // the fatal blow
                } else if rng_below(3) == 0 {
                    // 33% of non-fatal hits get the pained grunt (deterministic
                    // shared RNG, so every peer agrees), the rest the plain hurt.
                    play_sound("assets/sounds/hero_pain.mp3");
                } else {
                    play_sound("assets/sounds/hero_hurt.mp3");
                }
            }
            entity_set_field(e, "atkcd", enemy_atkcd());
            // Burst one cell in front of the mob, telegraphing its swing.
            let nd = nvel(dx, dy, fixed(1));
            spawn_fx(ep.x + nd.x, ep.y + nd.y, 3);
        }
        // Jostle apart while attacking (in place otherwise).
        try_move(e, sep.x, sep.y);
        entity_set_anim(e, "attack");
    } else {
        let v = nvel(dx, dy, enemy_speed());
        try_move(e, v.x + sep.x, v.y + sep.y);
        entity_set_anim(e, "run");
    }
    dec(e, "atkcd");

    if entity_field(e, "hurt") > fixed(0) {
        dec(e, "hurt");
        entity_set_anim(e, "hurt");
    }
    entity_set_facing(e, entity_field(e, "face"));
    flash(e); // red while the hurt timer runs
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

/// Flash an actor red while its `hurt` timer runs, clearing to white
/// otherwise. Render-side tint (`0x00RR_GGBB` colour multiply) — a damage
/// flash that never touches the hashed sim.
fn flash(e) {
    if entity_field(e, "hurt") > fixed(0) {
        entity_set_tint(e, 0xff4040); // red multiply
    } else {
        entity_set_tint(e, 0xffffff); // white = no tint
    }
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
        // Game-over track (idempotent: starts once, no-op while it loops).
        play_music("assets/sounds/dead_music.mp3");
    } else {
        stop_music(); // playing again (or restarted) → silence the death track
        if alive == 0 {
            status("Wave " + w + " cleared — next incoming");
        } else {
            status("Wave " + w + " — enemies: " + alive);
        }
    }
}

/// The local player's current HP (0..100), or 0 if it has none / is dead —
/// drives the health-bar fill.
fn local_hp() {
    let players = entities_of(0);
    let lp = local_player();
    let idx = if lp < 0 { 0 } else { lp };
    if idx < players.len() {
        let hp = to_int(entity_field(players[idx], "hp"));
        if hp < 0 { 0 } else { hp }
    } else {
        0
    }
}

/// Whether the local hero exists *and* has died — the game-over state that
/// shows the restart button. False before the hero has spawned (so an empty
/// slot at start isn't mistaken for death).
fn local_dead() {
    let players = entities_of(0);
    let lp = local_player();
    let idx = if lp < 0 { 0 } else { lp };
    idx < players.len() && !(entity_field(players[idx], "hp") > fixed(0))
}

/// Rebuild the screen-space HUD this tick. Immediate-mode: the host holds the
/// widget list between ticks and repaints it every frame, mapping button
/// clicks back into the next input command. Texture ids are the `init` load
/// order (0..7). Every ui_* call is render-side — never hashed.
/// Any button the local player is pressing during the intro (LMB / Space), or
/// `0` before a hero exists — used to skip the typewriter / dismiss the intro.
fn intro_input() {
    let players = entities_of(0);
    if players.len() > 0 { to_int(entity_field(players[0], "btn")) } else { 0 }
}

/// Advance the intro cutscene one tick: reveal the next glyph (blipping the
/// goddess's synth voice every `blip_every` glyphs), let a button skip the
/// typing, hold the full line briefly, then start the game (`started` = 1 +
/// first wave). The reveal index `dlg` is hashed sim state, so every peer's
/// intro is in lockstep; the blip is render-side.
fn advance_dialogue(g) {
    let len = intro_text().len();
    let dlg = to_int(entity_field(g, "dlg"));
    let btn = intro_input();
    if dlg < len {
        if btn != 0 {
            entity_set_field(g, "dlg", fixed(len)); // skip straight to the full line
        } else {
            let nd = dlg + 1;
            entity_set_field(g, "dlg", fixed(nd));
            if nd % blip_every() == 0 {
                // A small per-glyph warble gives the voice some life.
                play_blip(goddess_wave(), goddess_freq() + (nd % 4) * 20, goddess_dur(), goddess_gain());
            }
        }
    } else if dlg < len + reveal_hold() && btn == 0 {
        entity_set_field(g, "dlg", fixed(dlg + 1)); // hold, showing the full line
    } else {
        entity_set_field(g, "started", fixed(1));
        next_wave(g);
    }
}

/// The intro dialogue box: panel + talking portrait + the revealed text.
fn build_dialogue(g, w, h) {
    let dlg = to_int(entity_field(g, "dlg"));
    let len = intro_text().len();
    let shown = if dlg > len { len } else { dlg };

    // Panel (dialog-box 230×54) at 3× along the bottom.
    ui_scale(fixed(3));
    let bw = 690;
    let bh = 162;
    let bx = (w - bw) / 2;
    let by = h - bh - 30;
    ui_image(8, bx, by);

    // Talking portrait (256×256) at ~0.8×, pinned to the panel's top-left
    // corner (rising above it, overlapping the edge a touch).
    ui_scale(ratio(80, 100));
    let pw = 205;
    ui_anim(0, bx + 15, by - pw + 10);

    // The revealed text, word-wrapped across the panel — 30 px tighter on each
    // side than the panel, a touch smaller, and warm yellow.
    ui_scale(fixed(2));
    ui_text_wrap(bx + 100, by + 41, intro_text().sub_string(0, shown), 11, 270, 0xffe066);
}

fn build_ui() {
    ui_clear();
    let w = ui_width();
    let h = ui_height();
    if w <= 0 {
        return; // no viewport yet (pre-first-frame, or headless oracle)
    }
    let g = entities_of(2)[0];
    if to_int(entity_field(g, "started")) == 0 {
        build_dialogue(g, w, h);
        return;
    }

    ui_scale(fixed(2)); // draw every HUD texture + number at 2× native size

    // Texture sizes are doubled by ui_scale, so lay out at 2× dims. Base sizes:
    // health bar 158×22, panels 86×22, restart button 86×26.
    let hb_w = 316;
    let panel_w = 172;
    let panel_h = 44;

    // Health bar (top-left): dark back, red fill clipped to the HP fraction,
    // then the framed front (heart). Draw order = layering.
    ui_image(0, 16, 16);
    ui_image_clip(1, 16, 16, ratio(local_hp(), 100));
    ui_image(2, 16, 16);

    // Score + wave panels (top-right), numbers overlaid on their baked labels.
    let px = w - panel_w - 16;
    ui_image(3, px, 16);
    ui_text(px + 120, 24, "" + to_int(entity_field(g, "score")), 13);
    let wy = 16 + panel_h + 8;
    ui_image(4, px, wy);
    // Wave panel's baked label is wider than the score's, so its number sits
    // one digit-width (~15 px at this scale) further right.
    ui_text(px + 135, wy + 8, "" + to_int(entity_field(g, "wave")), 13);

    // Restart button — only on death, centred as a game-over prompt. A click
    // OR-s bit 16 into the next input command, handled in `command` as
    // `do_restart`.
    if local_dead() {
        ui_button(5, 6, 7, w / 2 - 86, h / 2 - 26, 16);
    }
}

/// Reset the run (restart button): clear all enemies + effects, revive and
/// reseat every player, zero the wave/score, and launch wave 1. Runs inside
/// `command`, so every peer restarts deterministically in lockstep.
fn do_restart() {
    for e in entities_of(1) {
        entity_despawn(e);
    }
    for fx in entities_of(3) {
        entity_despawn(fx);
    }
    for s in entities_of(4) {
        entity_despawn(s);
    }
    let players = entities_of(0);
    let i = 0;
    while i < players.len() {
        let p = players[i];
        entity_set_position(p, vec3(fixed(7 + i * 2), fixed(10), fixed(0)));
        entity_set_field(p, "hp", fixed(100));
        entity_set_field(p, "atk", fixed(0));
        entity_set_field(p, "dodge", fixed(0));
        entity_set_field(p, "cool", fixed(0));
        entity_set_field(p, "hurt", fixed(0));
        i += 1;
    }
    let g = entities_of(2)[0];
    entity_set_field(g, "wave", fixed(0));
    entity_set_field(g, "score", fixed(0));
    entity_set_field(g, "timer", fixed(0));
    next_wave(g);
}
