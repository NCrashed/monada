//! The spaceship crew-sim demo — S-B two-deck hull, entirely in the map.
//!
//! The engine knows no genre: this script paints a two-deck voxel hull,
//! defines an animated billboard "crew" actor (reusing the RPG hero GIFs), and
//! walks it around each deck on the deterministic fixed-point sim, crossing
//! decks by climbing a fore-starboard stairwell. The host injects one real-time input
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
        // Lower deck: full divider along y=10, doorway gap at x 9..10.
        cy == 10 && cx != 9 && cx != 10
    } else {
        // Upper deck: divider along x=10, doorway gap at y 9..10.
        cx == 10 && cy != 9 && cy != 10
    }
}

fn absi(n) { if n < 0 { -n } else { n } }

/// Which deck a crew member at grid height `cz` stands on (upper half → 1).
fn deck_of(cz) { if cz >= 14 { 1 } else { 0 } }

/// The deck **staircase**: solid step blocks at cx 14..18 × cy 1..2, tucked in
/// the fore-starboard corner (against the south + starboard hull), climbing EAST
/// — the west (low) end is flush with the lower floor, the east (high) end
/// reaches the upper floor. You walk up to it on the deck and climb: there is NO
/// fence around it — the step *risers* (the solid blocks `build_hull` paints,
/// z 0..step-top) block you from stepping onto a step more than one step-height
/// above your feet, exactly like real stairs. `build_hull` carves a matching
/// hole in the upper plate. Keep on_stairs / stair_z / paint in sync.
fn on_stairs(cx, cy) { cx >= 14 && cx <= 18 && cy >= 1 && cy <= 2 }

/// The step-top sim-z at staircase column `cx` (cx 14 → 0 … cx 18 → 28, step 7).
fn stair_z(cx) { (cx - 14) * 7 }

/// The walkable surface height at cell `(cx, cy)` for a crew currently at height
/// `cz`: a staircase step if on the stairs, else the deck floor nearest `cz`.
fn floor_at(cx, cy, cz) {
    if on_stairs(cx, cy) {
        stair_z(cx)
    } else if cz >= 14 {
        28
    } else {
        0
    }
}

/// Sim position → the cell index it falls in. Integer sim coords are cell
/// CENTRES (`world_of` seats a sprite at `-(x+0.5)·SCALE`), so a position
/// rounds to its nearest cell to match what's drawn (see the RPG demo's note
/// on why flooring puts the collision boundary half a cell off).
fn cell(v) { to_int(v + ratio(1, 2)) }

/// Can a crew member at height `cz` stand centred at `(px, py)`? Its footprint
/// must be clear of walls (on the deck `cz` implies), AND the surface it would
/// stand on must be at most one step (`STEP_MAX = 8`) from `cz` — so a tall step
/// riser blocks like a wall, but you climb a normal stair freely.
fn reachable(px, py, cz) {
    let deck = deck_of(cz);
    let r = ratio(40, 100);
    if blocked(cell(px - r), cell(py - r), deck)
        || blocked(cell(px + r), cell(py - r), deck)
        || blocked(cell(px - r), cell(py + r), deck)
        || blocked(cell(px + r), cell(py + r), deck)
    {
        return false;
    }
    absi(floor_at(cell(px), cell(py), cz) - cz) <= 8
}

/// The collision-checked new `(x, y)` for moving entity `e` by `(vx, vy)`,
/// axis-separated so it slides along walls / step risers. Returns the xy only
/// (as a `vec3`, z unused) — `step_crew` sets the z from the surface it lands on.
fn try_move(e, vx, vy, cz) {
    let pos = entity_position(e);
    let nx = pos.x + vx;
    let fx = if reachable(nx, pos.y, cz) { nx } else { pos.x };
    let ny = pos.y + vy;
    let fy = if reachable(fx, ny, cz) { ny } else { pos.y };
    vec3(fx, fy, fixed(0))
}

// --- setup --------------------------------------------------------------
// Archetype id by registration order: CREW = 0. Model id by `model_actor`
// order: crew = 0.
fn init() {
    // Crew: movement intent (ix/iy), last facing yaw, current deck (0 lower /
    // 1 upper — follows z on the stairwell ramp).
    archetype(["ix", "iy", "face", "deck"]);

    set_light(vec3(ratio(-4, 10), ratio(-3, 10), ratio(8, 10)), ratio(8, 10));
    // Space outside the hull: a starfield panorama behind the scene. Unpainted
    // cells (outside the hull) show it. Render-side (headless no-op).
    set_sky("assets/starfield.png");

    // Spawn the ship's own voxel grid (separate from the world grid) so the
    // hull can later be moved/rotated as a unit. grid_spawn returns a
    // render-side handle; (0, 0, 0) places it at the world voxel origin.
    let ship_grid = grid_spawn(0, 0, 0);
    build_hull(ship_grid);

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

    // Fog of war: a ~110° facing cone reaching 9 cells + a 3-cell 360°
    // peripheral. `follow_camera` names the LOCAL crew as the observer each tick
    // (per-client — each peer sees its own line of sight). Render-side.
    vision_config(110, 9, 3);

    status("ship demo — WASD to walk; the fore-starboard stairwell climbs between decks");
}

/// Paint the two-deck hull into `grid`, for the EYE. Collision is the `blocked`
/// predicate above (the heightmap store can't do stacked decks); keep this
/// layout and that predicate in sync. A deck is a floor slab + a tall rim wall
/// band; an interior divider (with a doorway gap) splits each deck into two
/// compartments joined by the gap — the "corridor". The two decks get
/// perpendicular dividers so they read distinct, and sit `DECK_STRIDE = 28`
/// apart in (unscaled) z.
fn build_hull(grid) {
    let plate = 0x8055_5f6b; // steel floor plating
    let wall = 0x8079_8592; // hull wall
    let step = 0x80b8_9850; // brass stair steps

    // --- deck 0 (lower): floor z=0, walls z 1..24 ---
    voxel_fill_in(grid, 0, 0, 0, 19, 19, 0, plate);
    rim_walls(grid, 1, 24, wall);
    // Full divider along y=10 splitting fore (y<10) / aft (y>10), doorway gap
    // x 9..10 (the stairwell is off this row, so no other opening is needed).
    voxel_fill_in(grid, 1, 10, 1, 8, 10, 24, wall);
    voxel_fill_in(grid, 11, 10, 1, 18, 10, 24, wall);

    // --- deck 1 (upper): floor slab z=28 with a HOLE over the staircase
    // (cx 14..18 × cy 1..2, so the crew climbs up through it), walls z 29..52 ---
    voxel_fill_in(grid, 0, 0, 28, 13, 19, 28, plate); // west of the stair hole
    voxel_fill_in(grid, 19, 0, 28, 19, 19, 28, plate); // east sliver (under the rim)
    voxel_fill_in(grid, 14, 3, 28, 18, 19, 28, plate); // north of the hole (cy 3..19)
    rim_walls(grid, 29, 52, wall);
    // Divider along x=10 splitting port (x<10) / starboard (x>10), gap y 9..10.
    voxel_fill_in(grid, 10, 1, 29, 10, 8, 52, wall);
    voxel_fill_in(grid, 10, 11, 29, 10, 18, 52, wall);

    // The staircase (fore-starboard corner, cy 1..2) climbs EAST: solid step
    // blocks at cx 14..18, each a column from z 0 up to `stair_z(cx)` — 0 at cx
    // 14 (flush with the lower floor) to 28 (the upper floor) at cx 18. No
    // surrounding wall: the blocks' own risers do the containment (see
    // `reachable`). Keep in sync with `on_stairs` / `stair_z`.
    voxel_fill_in(grid, 14, 1, 0, 14, 2, 0, step); // cx 14 → z 0 (lower floor)
    voxel_fill_in(grid, 15, 1, 0, 15, 2, 7, step);
    voxel_fill_in(grid, 16, 1, 0, 16, 2, 14, step);
    voxel_fill_in(grid, 17, 1, 0, 17, 2, 21, step);
    voxel_fill_in(grid, 18, 1, 0, 18, 2, 28, step); // cx 18 → z 28 (upper floor)
}

/// A rim wall band (z `lo`..`hi`) around the 20×20 hull edge, painted into `grid`.
fn rim_walls(grid, lo, hi, col) {
    voxel_fill_in(grid, 0, 0, lo, 19, 0, hi, col); // south
    voxel_fill_in(grid, 0, 19, lo, 19, 19, hi, col); // north
    voxel_fill_in(grid, 0, 0, lo, 0, 19, hi, col); // west
    voxel_fill_in(grid, 19, 0, lo, 19, 19, hi, col); // east
}

/// Spawn crew members lazily as each player's input first arrives (co-op join),
/// so no player count is needed up front. Ids follow `entities_of(0)` order.
fn ensure_crew(player) {
    while entities_of(0).len() < player + 1 {
        let idx = entities_of(0).len();
        let c = entity_create(0);
        // Fore-port on the lower deck, in the staircase's row (cy 2) so walking
        // east leads straight onto the steps; spread so co-op crew don't stack.
        entity_set_position(c, vec3(fixed(5 + idx * 2), fixed(2), fixed(0)));
        entity_set_field(c, "deck", fixed(0));
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

/// Always-on HUD readout of the LOCAL crew's current deck, so climbing the
/// stairwell is unmistakable (the map HUD paints every frame, unlike the F1
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
    // `ui_scale(2)` doubles sizes + text height; the second line clears the
    // 12pt first line (≈24px scaled) with room to spare.
    ui_text(8, 8, label, 12);
    ui_text(8, 42, "fore-starboard stairwell links the decks", 8);
}

/// Advance one crew member: turn its stored input into a view-relative,
/// height-checked move, seat it on the surface it lands on (deck floor or a
/// staircase step), and pick the walk/idle animation + facing.
fn step_crew(c) {
    let pos = entity_position(c);
    let cz = to_int(pos.z); // height BEFORE this move drives the step check
    let nx = pos.x;
    let ny = pos.y;
    let ix = entity_field(c, "ix");
    let iy = entity_field(c, "iy");
    if nonzero(ix) || nonzero(iy) {
        let dir = cam_relative(ix, iy);
        let v = nvel(dir.x, dir.y, crew_speed());
        let m = try_move(c, v.x, v.y, cz);
        nx = m.x;
        ny = m.y;
        entity_set_field(c, "face", dir_yaw(dir.x, dir.y));
        entity_set_facing(c, entity_field(c, "face"));
        entity_set_anim(c, "run");
    } else {
        entity_set_anim(c, "idle");
    }

    // Seat on the surface under the destination — a staircase step or a deck
    // floor — and let the DECK follow z (upper half → deck 1). Climbing east up
    // the stairs raises z step by step; the deck flips at the midpoint. No
    // teleport, no fence: the visible step risers do the containment.
    let z = floor_at(cell(nx), cell(ny), cz);
    entity_set_field(c, "deck", fixed(deck_of(z)));
    entity_set_position(c, vec3(nx, ny, fixed(z)));
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
        // See the world through this crew member's eyes (fog of war).
        vision_observer(c);
        // Show the crew's deck band (floor..deck_top): deck 0 cuts the upper
        // deck away; deck 1 is the top band and cuts nothing.
        let d = to_int(entity_field(c, "deck"));
        deck_clip(deck_floor(d), deck_top(d));
    }
}
