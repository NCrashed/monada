//! The spaceship crew-sim demo — S-A skeleton, entirely in the map.
//!
//! The engine knows no genre: this script paints a voxel hull, defines an
//! animated billboard "crew" actor (reusing the RPG hero GIFs), and walks it
//! around on the deterministic fixed-point sim. The host injects one real-time
//! input command per tick (verb 0): `arg = vec3(move_x, move_y, 0)`. Render
//! calls (`entity_set_anim`, `entity_set_facing`, `camera_focus`) never touch
//! the hashed world, so they can't desync a peer.
//!
//! Later slices bolt the roxlap-0.30 view/visibility seams onto this frame:
//! wall cutout, deck clip, fog of war (docs/plans/ship-visibility.md).
//!
//! Archetypes register in order, so ids are fixed: CREW = 0.
//! Model ids by `model_actor` order: crew = 0.

// --- tuning (all fixed-point; no_float Rhai forbids IEEE) ---------------
// NB: Rhai functions are pure — they do NOT see top-level `const`s — so the
// archetype id (CREW = 0), the hull bounds (walkable [1, 18]) and speeds are
// written as literals throughout rather than named constants.

fn crew_speed() { ratio(16, 100) } // sim cells / tick (30 Hz)

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

/// The camera's orbit yaw — MUST match `camera_angle` in `init`, since the
/// crew member moves relative to the view.
fn cam_yaw() { ratio(8, 10) }

/// Rotate a `(right, forward)` input — W/D in screen space — into the
/// sim-space move direction, so W goes "up the screen" rather than along
/// world +y. Accounts for the camera yaw and the render-side world-X mirror
/// (see the RPG demo's note). If W goes the wrong way, negate `cam_yaw`; if
/// A/D are swapped, negate `rx`.
fn cam_relative(rx, fy) {
    let y = cam_yaw();
    let c = cos(y);
    let s = sin(y);
    vec3(rx * s - fy * c, fy * s + rx * c, fixed(0))
}

/// Is sim cell `(cx, cy)` a hull wall / out of bounds?
///
/// S-A is a single deck, so this reads the one wall layer at z = 1. **S-B
/// TODO:** with two decks the query must use the *mover's* deck z-band (pass
/// it in), else a crew member on the upper deck would collide against the
/// lower deck's walls — the off-by-a-band risk flagged in the plan's Open
/// Questions (docs/plans/ship-visibility.md).
fn blocked(cx, cy) {
    if cx < 1 || cy < 1 || cx > 18 || cy > 18 {
        return true;
    }
    voxel_solid(cx, cy, 1)
}

/// Sim position → the cell index it falls in. Integer sim coords are cell
/// CENTRES (`world_of` seats a sprite at `-(x+0.5)·SCALE`), so a position
/// rounds to its nearest cell to match what's drawn (see the RPG demo's note
/// on why flooring puts the collision boundary half a cell off).
fn cell(v) { to_int(v + ratio(1, 2)) }

/// Whether a mover's footprint — a small box of radius `r` around `(px, py)`
/// — is clear of walls. Checking the four corners stops the body before it
/// overlaps a wall, so a ~1-cell-wide crew member can't clip through it.
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
// Archetype id by registration order: CREW = 0. Model id by `model_actor`
// order: crew = 0.
fn init() {
    // Crew: movement intent (ix/iy), last facing yaw. Grows in later slices.
    archetype(["ix", "iy", "face"]);

    set_light(vec3(ratio(-4, 10), ratio(-3, 10), ratio(8, 10)), ratio(8, 10));

    // The hull: a 20×20 metal floor slab at z=0, ringed by rim walls (z 1..3)
    // so collision has something to bite. `voxel_fill` paints both the visible
    // voxels AND the collision store (`voxel_solid`/`ground_height`). A single
    // deck for S-A; the second deck + compartments land in S-B.
    voxel_fill(0, 0, 0, 19, 19, 0, 0x8055_5f6b); // floor plating (steel grey)
    voxel_fill(0, 0, 1, 19, 0, 3, 0x8079_8592); // south wall
    voxel_fill(0, 19, 1, 19, 19, 3, 0x8079_8592); // north wall
    voxel_fill(0, 0, 1, 0, 19, 3, 0x8079_8592); // west wall
    voxel_fill(19, 0, 1, 19, 19, 3, 0x8079_8592); // east wall

    // The animated billboard crew actor (id 0 by call order). The 3rd arg is
    // the rendered height in cells; the host scales the art to it. Reuses the
    // RPG hero's idle/run GIFs under assets/char/crew.
    model_actor("assets/char/crew", ["idle", "run"], ratio(14, 10));
    // Lower the sprite so its visible feet sit in the collision box (the pivot
    // grounds on the trimmed opaque bottom, but the art leaves it a touch high).
    model_drop(0, ratio(35, 100));

    // A follow camera pulled in close on the crew member; `follow_camera`
    // re-aims it at the local player each tick.
    camera_angle(ratio(8, 10), ratio(10, 10));
    camera_dist(fixed(80));
    camera_focus(vec3(fixed(10), fixed(10), fixed(0)));
    status("ship demo (S-A) — WASD to walk the deck");
}

/// Spawn crew members lazily as each player's input first arrives (co-op join),
/// so no player count is needed up front. Ids follow `entities_of(0)` order.
fn ensure_crew(player) {
    while entities_of(0).len() < player + 1 {
        let idx = entities_of(0).len();
        let c = entity_create(0);
        entity_set_position(c, vec3(fixed(8 + idx * 2), fixed(10), fixed(0)));
        entity_set_model(c, 0); // crew actor model
        entity_set_anim(c, "idle");
    }
}

// --- local layer (unsynced, this client only) ---------------------------
// Assemble the per-tick input command the sim's `command` handler decodes:
// the move axis in `arg.xy`. The action ids + default keys live in
// manifest.toml `[[action]]`; the host resolves rebinds, this only reads the
// named action.
fn local_tick(dt) {
    let mv = action_axis2("move");
    submit_command(0, 0, vec3(mv.x, mv.y, fixed(0)));
}

// --- input: store this peer's intent on its crew entity ------------------
fn command(player, verb, target, arg) {
    if verb == 0 {
        ensure_crew(player);
        let c = entities_of(0)[player];
        entity_set_field(c, "ix", arg.x);
        entity_set_field(c, "iy", arg.y);
    }
}

// --- one simulation tick ------------------------------------------------
fn tick() {
    for c in entities_of(0) {
        step_crew(c);
    }
    follow_camera();
}

/// Advance one crew member: turn its stored input into a view-relative,
/// collision-checked move, and pick the walk/idle animation + facing.
fn step_crew(c) {
    let ix = entity_field(c, "ix");
    let iy = entity_field(c, "iy");
    if nonzero(ix) || nonzero(iy) {
        let dir = cam_relative(ix, iy);
        let v = nvel(dir.x, dir.y, crew_speed());
        try_move(c, v.x, v.y);
        entity_set_field(c, "face", dir_yaw(dir.x, dir.y));
        entity_set_facing(c, entity_field(c, "face"));
        entity_set_anim(c, "run");
    } else {
        entity_set_anim(c, "idle");
    }
}

/// Re-aim the camera at the local player's crew member (or crew 0 in a hotseat
/// session with no single local player).
fn follow_camera() {
    let crew = entities_of(0);
    let lp = local_player();
    let idx = if lp < 0 { 0 } else { lp };
    if idx < crew.len() {
        camera_focus(entity_position(crew[idx]));
    }
}
