//! The spaceship crew-sim demo — S-B two-deck hull, entirely in the map.
//!
//! The engine knows no genre: this script paints a two-deck voxel hull,
//! defines an animated billboard "crew" actor (reusing the RPG hero GIFs), and
//! walks it around each deck on the deterministic fixed-point sim, crossing
//! decks at a fore-starboard hatch. The host injects one real-time input
//! command per tick (verb 0): `arg = vec3(move_x, move_y, 0)`. Render calls
//! (`entity_set_anim`, `entity_set_facing`, `camera_focus`) never touch the
//! hashed world, so they can't desync a peer.
//!
//! Collision is DECK-RELATIVE (see the `decks` section): the crew tracks which
//! deck it stands on and tests only that deck's walls, so the stacked floor
//! slabs don't confuse seating and the upper deck never hits lower walls. NB
//! walls are a script-side predicate, not `voxel_solid` — monada's heightmap
//! collision store can't represent air below a floor (see `blocked`).
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

// --- decks --------------------------------------------------------------
// The hull stacks two decks as sim-z bands (height-up: `voxel_fill` maps
// height z → world g − z). Because the two floor slabs are BOTH solid, a
// naive `ground_height` (highest solid z) would always snap a crew member to
// the UPPER floor — so the crew tracks its deck as sim state and collision /
// seating is deck-relative. This is the off-by-a-band trap from the plan's
// Open Questions, resolved by threading the deck through the whole path
// rather than reading a fixed z.
//
// z is UNSCALED (1 world unit per sim-z; only x/y scale by SCALE=16 — see
// `world_of`), so a deck needs a tall sim-z span to read as a real storey and
// clear the ~1.4-cell (~22-unit) crew sprite. Decks are `DECK_STRIDE = 28`
// apart:
//   deck 0 (lower): floor z=0,  walls z 1..24
//   deck 1 (upper): floor z=28, walls z 29..52
// (numbers are for the eye + the deck cutaway; the wall PREDICATE is xy-only,
// so height changes never touch collision. All tunable on a real display.)

/// The floor sim-z a crew member on `deck` stands on (decks 28 apart).
fn deck_floor(deck) { if deck == 0 { 0 } else { 28 } }

/// The top of `deck`'s keep-band for the cutaway: everything above it (the
/// deck above) is cut. One below the next deck's floor.
fn deck_top(deck) { deck_floor(deck) + 27 }

/// Is sim cell `(cx, cy)` a hull wall / out of bounds *on `deck`*?
///
/// NB collision here is a script-side predicate, NOT a `voxel_solid` query.
/// monada's collision store (`VoxelStore`) is a single HEIGHTMAP — a column is
/// solid from the floor up to its top voxel — so it cannot represent air below
/// a floor: painting the upper deck's plate above makes `voxel_solid(x, y, 1)`
/// true across the WHOLE lower deck (everything "below the top"). Stacked
/// walkable decks need a voxel-set / per-deck collision primitive the engine
/// doesn't have yet (see the plan's Open Questions — a real S-C+ engine gap;
/// roxlap already models decks via `DeckBand`, monada's collision does not).
/// Until then the map owns its wall layout as this predicate, kept in sync
/// with what `build_hull` paints for the eye.
fn blocked(cx, cy, deck) {
    if cx < 1 || cy < 1 || cx > 18 || cy > 18 {
        return true; // rim / out of bounds, both decks
    }
    if deck == 0 {
        // Lower deck: full divider along y=10, doorway gap at x 9..10. (The
        // hatch is fore-starboard, off this row, so no opening is needed here.)
        cy == 10 && cx != 9 && cx != 10
    } else {
        // Upper deck: divider along x=10, doorway gap at y 9..10.
        cx == 10 && cy != 9 && cy != 10
    }
}

/// The deck **hatch**: a discrete fore-starboard corner cell (`cx ≥ 16`,
/// `cy ≤ 4`) that links the two decks — stepping onto it flips the crew's deck
/// (see `step_crew`). Painted as a bright marker in `build_hull` so it reads as
/// a ladder/hatch, not just floor. (S-B used the whole starboard strip; a
/// discrete hatch reads far better in the top-down view.) It sits clear of both
/// dividers (lower y=10 / upper x=10), so a flip always lands on open floor.
fn is_stair(cx, cy) { cx >= 16 && cy <= 4 }

/// Sim position → the cell index it falls in. Integer sim coords are cell
/// CENTRES (`world_of` seats a sprite at `-(x+0.5)·SCALE`), so a position
/// rounds to its nearest cell to match what's drawn (see the RPG demo's note
/// on why flooring puts the collision boundary half a cell off).
fn cell(v) { to_int(v + ratio(1, 2)) }

/// Whether a mover's footprint — a small box of radius `r` around `(px, py)`
/// — is clear of walls on `deck`. Checking the four corners stops the body
/// before it overlaps a wall, so a ~1-cell-wide crew member can't clip through.
fn clear(px, py, deck) {
    let r = ratio(40, 100);
    !(blocked(cell(px - r), cell(py - r), deck)
        || blocked(cell(px + r), cell(py - r), deck)
        || blocked(cell(px - r), cell(py + r), deck)
        || blocked(cell(px + r), cell(py + r), deck))
}

/// Move entity `e` by `(vx, vy)` on `deck`, axis-separated so it slides along
/// walls, and reseat it on that deck's floor.
fn try_move(e, vx, vy, deck) {
    let pos = entity_position(e);
    let nx = pos.x + vx;
    let fx = if clear(nx, pos.y, deck) { nx } else { pos.x };
    let ny = pos.y + vy;
    let fy = if clear(fx, ny, deck) { ny } else { pos.y };
    entity_set_position(e, vec3(fx, fy, fixed(deck_floor(deck))));
}

// --- setup --------------------------------------------------------------
// Archetype id by registration order: CREW = 0. Model id by `model_actor`
// order: crew = 0.
fn init() {
    // Crew: movement intent (ix/iy), last facing yaw, current deck (0 lower /
    // 1 upper), and `on_stair` (rising-edge latch for the deck transition).
    archetype(["ix", "iy", "face", "deck", "on_stair"]);

    set_light(vec3(ratio(-4, 10), ratio(-3, 10), ratio(8, 10)), ratio(8, 10));
    // Space outside the hull: a starfield panorama behind the scene. Unpainted
    // cells (outside the hull) show it. Render-side (headless no-op).
    set_sky("assets/starfield.png");

    build_hull();

    // The animated billboard crew actor (id 0 by call order). The 3rd arg is
    // the rendered height in cells; the host scales the art to it. Reuses the
    // RPG hero's idle/run GIFs under assets/char/crew.
    model_actor("assets/char/crew", ["idle", "run"], ratio(14, 10));
    // No model_drop: the crew pivot already grounds on the deck's solid voxel
    // floor (world_of seats sim-z N at grid-z GROUND_Z-N, the same z the floor
    // voxel sits at). The RPG's `model_drop(0.35)` is `0.35·SCALE ≈ 5.6` WORLD
    // units of sink — invisible over its render-only floor, but here it buried
    // the crew knee-deep in the real floor slab. Raise a hair (e.g.
    // `model_drop(0, ratio(5, 100))`) only if the art's feet float.

    // SS13-style steep top-down follow camera: look DOWN into the deck so the
    // room's walls read as its outline and you see the interior over them
    // (`pitch` is the downward tilt — larger = more top-down, max ~1.45 ≈
    // straight down; `cam_yaw` must match this yaw, movement is view-relative).
    // `follow_camera` re-aims it (and sets the deck cutaway) at the local player
    // each tick. NO wall cutout in this mode: seeing INTO a deck is the deck
    // cutaway's job (`deck_clip` hides the deck above), and from overhead the
    // walls don't block the interior — so they stay put, unlike the
    // over-the-shoulder keyhole (which erased them).
    camera_angle(ratio(8, 10), ratio(12, 10));
    camera_dist(fixed(60));
    camera_focus(vec3(fixed(6), fixed(4), fixed(0)));

    status("ship demo — WASD to walk; the amber fore-starboard hatch changes deck");
}

/// Paint the two-deck hull, for the EYE. Collision is the `blocked` predicate
/// above (the heightmap store can't do stacked decks); keep this layout and
/// that predicate in sync. A deck is a floor slab + a tall rim wall band; an
/// interior divider (with a doorway gap) splits each deck into two compartments
/// joined by the gap — the "corridor". The two decks get perpendicular dividers
/// so they read distinct, and sit `DECK_STRIDE = 28` apart in (unscaled) z.
fn build_hull() {
    let plate = 0x8055_5f6b; // steel floor plating
    let wall = 0x8079_8592; // hull wall
    let hatch = 0x80e0_a828; // amber deck-hatch marker (reads as "caution/ladder")

    // --- deck 0 (lower): floor z=0, walls z 1..24 ---
    voxel_fill(0, 0, 0, 19, 19, 0, plate);
    rim_walls(1, 24, wall);
    // Full divider along y=10 splitting fore (y<10) / aft (y>10), doorway gap
    // x 9..10 (the hatch is off this row, so no other opening is needed).
    voxel_fill(1, 10, 1, 8, 10, 24, wall);
    voxel_fill(11, 10, 1, 18, 10, 24, wall);

    // --- deck 1 (upper): floor slab z=28, walls z 29..52 ---
    voxel_fill(0, 0, 28, 19, 19, 28, plate);
    rim_walls(29, 52, wall);
    // Divider along x=10 splitting port (x<10) / starboard (x>10), gap y 9..10.
    voxel_fill(10, 1, 29, 10, 8, 52, wall);
    voxel_fill(10, 11, 29, 10, 18, 52, wall);

    // The deck hatch (fore-starboard corner, cx 16..18 × cy 1..4 — matches
    // `is_stair`): an amber marker on BOTH deck floors so it reads as the ladder
    // that links them. Cosmetic — `is_stair`/`step_crew` drive the transition.
    voxel_fill(16, 1, 0, 18, 4, 0, hatch);
    voxel_fill(16, 1, 28, 18, 4, 28, hatch);
}

/// A rim wall band (z `lo`..`hi`) around the 20×20 hull edge.
fn rim_walls(lo, hi, col) {
    voxel_fill(0, 0, lo, 19, 0, hi, col); // south
    voxel_fill(0, 19, lo, 19, 19, hi, col); // north
    voxel_fill(0, 0, lo, 0, 19, hi, col); // west
    voxel_fill(19, 0, lo, 19, 19, hi, col); // east
}

/// Spawn crew members lazily as each player's input first arrives (co-op join),
/// so no player count is needed up front. Ids follow `entities_of(0)` order.
fn ensure_crew(player) {
    while entities_of(0).len() < player + 1 {
        let idx = entities_of(0).len();
        let c = entity_create(0);
        // Fore-port on the lower deck (clear of the y=10 divider and the
        // hatch, cx≥16 & cy≤4), spread so co-op crew don't stack.
        entity_set_position(c, vec3(fixed(5 + idx * 2), fixed(4), fixed(0)));
        entity_set_field(c, "deck", fixed(0));
        entity_set_field(c, "on_stair", fixed(0));
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
    build_deck_hud();
}

/// Always-on HUD readout of the LOCAL crew's current deck, so a deck change at
/// the hatch is unmistakable (the map HUD paints every frame, unlike the F1
/// status line). Render-side: headless/oracle no-op every `ui_*` call.
fn build_deck_hud() {
    let crew = entities_of(0);
    let lp = local_player();
    let idx = if lp < 0 { 0 } else { lp };
    if idx >= crew.len() {
        return;
    }
    let d = to_int(entity_field(crew[idx], "deck"));
    ui_clear();
    ui_scale(fixed(2));
    let label = if d == 0 { "DECK 1 / 2  (lower)" } else { "DECK 2 / 2  (upper)" };
    ui_text(8, 8, label, 12);
    ui_text(8, 26, "amber hatch (fore-starboard) changes deck", 8);
}

/// Advance one crew member: turn its stored input into a view-relative,
/// collision-checked move on its current deck, handle the deck transition when
/// it steps onto the hatch, and pick the walk/idle animation + facing.
fn step_crew(c) {
    let deck = to_int(entity_field(c, "deck"));
    let ix = entity_field(c, "ix");
    let iy = entity_field(c, "iy");
    if nonzero(ix) || nonzero(iy) {
        let dir = cam_relative(ix, iy);
        let v = nvel(dir.x, dir.y, crew_speed());
        try_move(c, v.x, v.y, deck);
        entity_set_field(c, "face", dir_yaw(dir.x, dir.y));
        entity_set_facing(c, entity_field(c, "face"));
        entity_set_anim(c, "run");
    } else {
        entity_set_anim(c, "idle");
    }

    // Deck transition. Stepping onto the hatch flips the deck and reseats onto
    // the new floor. Two safeguards keep it soft-lock-proof (belt-and-braces —
    // the hatch is clear of both dividers, so the guard never actually fires):
    //  * LANDING GUARD — flip only if the crew's footprint is CLEAR on the
    //    target deck, so a transition can never seat it inside a wall.
    //  * RETRY — the `on_stair` latch (which stops it toggling every tick while
    //    standing on the hatch) is set ONLY once a flip actually succeeds, so a
    //    refused attempt retries next tick instead of stranding the crew.
    let pos = entity_position(c);
    let was_on_stair = to_int(entity_field(c, "on_stair")) == 1;
    let in_stair = is_stair(cell(pos.x), cell(pos.y));
    let flipped = false;
    if in_stair && !was_on_stair {
        let nd = 1 - deck;
        if clear(pos.x, pos.y, nd) {
            deck = nd;
            entity_set_field(c, "deck", fixed(deck));
            entity_set_position(c, vec3(pos.x, pos.y, fixed(deck_floor(deck))));
            flipped = true;
        }
    }
    let latched = in_stair && (was_on_stair || flipped);
    entity_set_field(c, "on_stair", fixed(if latched { 1 } else { 0 }));
}

/// Re-aim the camera at the local player's crew member (or crew 0 in a hotseat
/// session with no single local player), and cut the deck away above whichever
/// deck that crew member stands on — so we see into their deck, not the hull
/// roof. Both are render-side (per-client): each peer views its own crew.
fn follow_camera() {
    let crew = entities_of(0);
    let lp = local_player();
    let idx = if lp < 0 { 0 } else { lp };
    if idx < crew.len() {
        let c = crew[idx];
        camera_focus(entity_position(c));
        // Show the crew's deck band (floor..deck_top): deck 0 cuts the upper
        // deck away; deck 1 is the top band and cuts nothing.
        let d = to_int(entity_field(c, "deck"));
        deck_clip(deck_floor(d), deck_top(d));
    }
}
