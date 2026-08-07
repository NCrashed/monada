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
//! Archetypes register in order, so ids are fixed: CREW = 0, SHIP = 1.
//! Model ids by `model_actor` order: crew = 0.

// --- tuning (all fixed-point; no_float Rhai forbids IEEE) ---------------
// NB: Rhai functions are pure — they do NOT see top-level `const`s — so the
// archetype id (CREW = 0), the hull bounds (walkable [1, 18]) and speeds are
// written as literals throughout rather than named constants.

fn crew_speed() { ratio(16, 100) } // sim cells / tick (30 Hz)

// The hull's grid_spawn handle. It is the first (and only) grid the map spawns,
// so its handle is 0 — written as a literal here the same way CREW = 0 is, since
// pure functions can't see top-level consts. Fog + deck_clip ride this grid.
fn hull_grid() { 0 }

// The ship entity: the only member of archetype SHIP (1), created by `init`.
// Looked up rather than written as a literal, unlike `hull_grid()`: which id
// `entity_create` hands out is an ordering detail of `init`, and id 0 is also
// the engine's conventional "no target" for `Command::target` — pinning the
// singleton there invites a collision the day a verb starts reading `target`.
// One small Array per tick is a fair price for an invariant the engine keeps
// instead of a comment.
fn ship_entity() { entities_of(1)[0] }

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
// The hull stacks two decks as sim-z bands (height-up: a cell painted at
// height z tops out where a crew member standing at z has its feet). Because
// the two floor plates are BOTH solid, a naive `ground_height` (highest solid
// z) would always snap a crew member to the UPPER floor — so the crew tracks
// its deck as sim state and collision / seating is deck-relative. This is the
// off-by-a-band trap from the plan's Open Questions, resolved by threading the
// deck through the whole path rather than reading a fixed z.
//
// The hull is a CUBIC grid (`grid_spawn_cubic`, host_api 15): one sim cell is
// a cube of 16³ world voxels, so z is scaled exactly like x/y and the hull's
// 3D rotation is the sim-space rotation the script asks for — which is what
// lets a map convert between hull-local and world coordinates at all
// (docs/plans/grid-entities.md §6). The price is that VERTICAL GEOMETRY IS
// CELL-QUANTISED: a wall is a whole number of cells tall, and a stair step is
// one cell. Decks are `DECK_STRIDE = 3` cells apart:
//   deck 0 (lower): floor plate cell 0, walls cells 1..2, crew feet at z=0
//   deck 1 (upper): floor plate cell 3, walls cells 4..5, crew feet at z=3
// (numbers are for the eye + the deck cutaway; the wall PREDICATE is xy-only,
// so height changes never touch collision. All tunable on a real display.)

/// The floor sim-z a crew member on `deck` stands on (decks 3 cells apart).
fn deck_floor(deck) { if deck == 0 { 0 } else { 3 } }

/// The top of `deck`'s keep-band for the cutaway: everything above it (the
/// deck above) is cut. The last cell below the next deck's floor plate.
fn deck_top(deck) { deck_floor(deck) + 2 }

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
    // The airlock: two rim cells on the lower deck's starboard side. Open, they
    // are the one walkable cell OUTSIDE the walkable box — you stand IN the
    // doorway with the void at your back. `paint_door` keeps the voxels in sync
    // with this predicate (the same hand-sync contract the walls have).
    if deck == 0 && cx == 19 && (cy == 9 || cy == 10) {
        return !door_open();
    }
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
fn deck_of(cz) { if cz >= 2 { 1 } else { 0 } }

/// The deck **staircase**: solid step blocks at cx 15..18 × cy 1..2, tucked in
/// the fore-starboard corner (against the south + starboard hull), climbing EAST
/// — the west (low) end is flush with the lower floor, the east (high) end
/// reaches the upper floor. You walk up to it on the deck and climb: there is NO
/// fence around it — the step *risers* (the solid blocks `build_hull` paints,
/// cells 0..step-top) block you from stepping onto a step more than one
/// step-height above your feet, exactly like real stairs. `build_hull` carves a
/// matching hole in the upper plate. Keep on_stairs / stair_z / paint in sync.
fn on_stairs(cx, cy) { cx >= 15 && cx <= 18 && cy >= 1 && cy <= 2 }

/// The step-top sim-z at staircase column `cx` (cx 15 → 0 … cx 18 → 3): one
/// CELL per step, the finest a cubic grid can carve.
fn stair_z(cx) { cx - 15 }

/// The walkable surface height at cell `(cx, cy)` for a crew currently at height
/// `cz`: a staircase step if on the stairs, else the deck floor nearest `cz`.
fn floor_at(cx, cy, cz) {
    if on_stairs(cx, cy) {
        stair_z(cx)
    } else if cz >= 2 {
        3
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
/// stand on must be at most one step (`STEP_MAX = 1` cell) from `cz` — so a
/// two-cell drop or rise blocks like a wall, but you climb the stair freely.
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
    absi(floor_at(cell(px), cell(py), cz) - cz) <= 1
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
// Archetype id by registration order: CREW = 0, SHIP = 1. Model id by
// `model_actor` order: crew = 0.
fn init() {
    // Crew: movement intent (ix/iy), last facing yaw, current deck (0 lower /
    // 1 upper — follows z on the stairwell ramp), the carried crate (its id + 1,
    // 0 = hands free) and last tick's button mask (so `use` / `door` act on the
    // press, not on every tick the key is held).
    archetype(["ix", "iy", "face", "deck", "carry", "btn", "prev"]);

    // Ship: the hull's 3D orientation as a spin angle (radians) about a fixed
    // tumble axis, plus the airlock state. HASHED sim state, so every peer turns
    // the hull identically — the crew ride it via grid-local coords, so the
    // hull's motion is real shared gameplay, not decoration. Archetype id 1
    // (after CREW = 0); one ship entity, created here and advanced each tick by
    // `step_ship`.
    archetype(["spin", "door"]);
    let ship = entity_create(1);
    entity_set_field(ship, "spin", fixed(0));

    // Cargo crates: no fields of their own beyond "is someone holding me". What
    // makes them interesting is WHICH FRAME they are in — riding the hull, or
    // released into the world (see `step_use`). Archetype id 2.
    archetype(["held"]);

    set_light(vec3(ratio(-4, 10), ratio(-3, 10), ratio(8, 10)), ratio(8, 10));
    // Space outside the hull: a starfield panorama behind the scene. Unpainted
    // cells (outside the hull) show it. Render-side (headless no-op).
    set_sky("assets/starfield.png");

    // Spawn the ship's own voxel grid (separate from any world grid) so the
    // hull can be moved/rotated as a unit, and so the fog/deck cutaway ride the
    // crew's own grid. CUBIC cells (host_api 15): a cell is a cube, so the
    // hull's tilted tumble below is the sim-space rotation it asks for and a
    // hull-local point can be converted to a world point exactly — the column
    // cell's unscaled z can only do that for yaw. `(0,0,0)` places it at the
    // world voxel origin. As the first (only) grid it spawns its handle is
    // `hull_grid()` (== 0); a `< 0` return means a host without cubic grids —
    // the manifest requires 15, so bail loud rather than paint into nothing.
    let ship_grid = grid_spawn_cubic(0, 0, 0);
    if ship_grid < 0 {
        status("this host lacks cubic grids (host_api 15+ required)");
        return;
    }
    build_hull(ship_grid);
    // Turn the hull about its own middle, not the corner its local origin sits
    // on. `build_hull` spans sim cells 0..=19 in x/y and 0..=5 in z, so the
    // centre is (9.5, 9.5, 2) — without this the whole ship swings through an
    // arc wider than itself every time `step_ship` re-orients it.
    grid_pivot(ship_grid, vec3(ratio(19, 2), ratio(19, 2), fixed(2)));

    // The animated billboard crew actor (id 0 by call order). The 3rd arg is
    // the rendered height in cells; the host scales the art to it. Reuses the
    // RPG hero's idle/run GIFs under assets/char/crew.
    model_actor("assets/char/crew", ["idle", "run"], ratio(14, 10));
    // The crate: a procedural box, model id 1 (the call after the actor).
    model_box(9, 9, 9, 0x80a8_7a3c);

    // Two crates stowed on the lower deck, authored in HULL coordinates — so
    // `entity_set_grid` (the raw bind) is right here: nothing should move, the
    // numbers already mean "this cell of the ship". `entity_attach` is for
    // later, when a crate that is somewhere in the WORLD has to come aboard
    // without jumping.
    stow_crate(6, 2); // beside the crew's spawn, within arm's reach
    stow_crate(12, 14); // aft, across the divider's doorway
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
    // The camera RIDES the hull: its orbit frame turns with the ship, so the
    // deck holds still on screen and the starfield sweeps past instead. This is
    // not just a look — the crew's position is hull-LOCAL, so a world-fixed
    // camera would have `cam_relative` steering in the ship's frame while the
    // player watches the world's, and "forward" would point somewhere new every
    // tick as the hull turns. One call, and the two frames agree again.
    camera_grid(ship_grid);

    // Fog of war: a ~110° facing cone reaching 9 cells + a 3-cell 360°
    // peripheral. `follow_camera` names the LOCAL crew as the observer each tick
    // (per-client — each peer sees its own line of sight). Render-side.
    vision_config(110, 9, 3);

    status("ship demo — WASD walk, E crate, F airlock; the fore-starboard stairwell climbs between decks");
}

/// Paint the two-deck hull into `grid`, for the EYE. Collision is the `blocked`
/// predicate above (the heightmap store can't do stacked decks); keep this
/// layout and that predicate in sync. A deck is a floor plate + a two-cell rim
/// wall band; an interior divider (with a doorway gap) splits each deck into two
/// compartments joined by the gap — the "corridor". The two decks get
/// perpendicular dividers so they read distinct, and sit `DECK_STRIDE = 3`
/// cells apart. Every z here is a CELL of the cubic grid (16 world voxels), so
/// a "one cell" wall is as tall as the crew is wide.
fn build_hull(grid) {
    let plate = 0x8055_5f6b; // steel floor plating
    let wall = 0x8079_8592; // hull wall
    let step = 0x80b8_9850; // brass stair steps

    // --- deck 0 (lower): floor plate cell 0, walls cells 1..2 ---
    voxel_fill_in(grid, 0, 0, 0, 19, 19, 0, plate);
    rim_walls(grid, 1, 2, wall);
    // Full divider along y=10 splitting fore (y<10) / aft (y>10), doorway gap
    // x 9..10 (the stairwell is off this row, so no other opening is needed).
    voxel_fill_in(grid, 1, 10, 1, 8, 10, 2, wall);
    voxel_fill_in(grid, 11, 10, 1, 18, 10, 2, wall);

    // --- deck 1 (upper): floor plate cell 3 with a HOLE over the staircase
    // (cx 15..18 × cy 1..2, so the crew climbs up through it), walls cells 4..5 ---
    voxel_fill_in(grid, 0, 0, 3, 14, 19, 3, plate); // west of the stair hole
    voxel_fill_in(grid, 19, 0, 3, 19, 19, 3, plate); // east sliver (under the rim)
    voxel_fill_in(grid, 15, 3, 3, 18, 19, 3, plate); // north of the hole (cy 3..19)
    rim_walls(grid, 4, 5, wall);
    // Divider along x=10 splitting port (x<10) / starboard (x>10), gap y 9..10.
    voxel_fill_in(grid, 10, 1, 4, 10, 8, 5, wall);
    voxel_fill_in(grid, 10, 11, 4, 10, 18, 5, wall);

    // The staircase (fore-starboard corner, cy 1..2) climbs EAST: solid step
    // blocks at cx 15..18, each a column from cell 0 up to `stair_z(cx)` — 0 at
    // cx 15 (flush with the lower floor) to 3 (the upper floor) at cx 18, one
    // cell per step. No surrounding wall: the blocks' own risers do the
    // containment (see `reachable`). Keep in sync with `on_stairs` / `stair_z`.
    voxel_fill_in(grid, 15, 1, 0, 15, 2, 0, step); // cx 15 → z 0 (lower floor)
    voxel_fill_in(grid, 16, 1, 0, 16, 2, 1, step);
    voxel_fill_in(grid, 17, 1, 0, 17, 2, 2, step);
    voxel_fill_in(grid, 18, 1, 0, 18, 2, 3, step); // cx 18 → z 3 (upper floor)
}

/// A rim wall band (z `lo`..`hi`) around the 20×20 hull edge, painted into `grid`.
fn rim_walls(grid, lo, hi, col) {
    voxel_fill_in(grid, 0, 0, lo, 19, 0, hi, col); // south
    voxel_fill_in(grid, 0, 19, lo, 19, 19, hi, col); // north
    voxel_fill_in(grid, 0, 0, lo, 0, 19, hi, col); // west
    voxel_fill_in(grid, 19, 0, lo, 19, 19, hi, col); // east
}

// --- cargo + airlock -----------------------------------------------------
// The slice that exercises grid membership (docs/plans/grid-entities.md): a
// crate is an entity whose FRAME is the gameplay. Stowed, it rides the hull and
// tumbles with it; dropped through the open airlock it is released into the
// world frame and stays exactly where it was let go, while the ship turns away
// from it. `entity_attach` / `entity_detach` are what make those two the same
// crate rather than a teleport.

/// Put a fresh crate on the lower deck at hull cell `(cx, cy)`.
fn stow_crate(cx, cy) {
    let k = entity_create(2);
    entity_set_position(k, vec3(fixed(cx), fixed(cy), fixed(0)));
    entity_set_model(k, 1);
    entity_set_grid(k, hull_grid());
}

/// Is the airlock open? HASHED ship state, so every peer's collision agrees.
fn door_open() { to_int(entity_field(ship_entity(), "door")) == 1 }

/// Toggle the airlock: the hashed flag, the wall voxels and — through
/// `blocked` — passability, all in one place so they cannot drift apart.
fn toggle_door() {
    let open = if door_open() { 0 } else { 1 };
    entity_set_field(ship_entity(), "door", fixed(open));
    paint_door(open);
}

/// Cut the doorway out of the rim wall, or weld it shut again. Deck 0's walls
/// are cells 1..2, so the doorway is those two cells of the two door columns.
fn paint_door(open) {
    if open == 1 {
        voxel_clear_in(hull_grid(), 19, 9, 1);
        voxel_clear_in(hull_grid(), 19, 9, 2);
        voxel_clear_in(hull_grid(), 19, 10, 1);
        voxel_clear_in(hull_grid(), 19, 10, 2);
    } else {
        voxel_fill_in(hull_grid(), 19, 9, 1, 19, 10, 2, 0x8079_8592);
    }
}

/// Is this crew member standing IN the open doorway — the one cell where a
/// released crate leaves the ship instead of resting on its deck?
fn in_airlock(c) {
    let p = entity_position(c);
    let cy = cell(p.y);
    door_open() && cell(p.x) == 19 && (cy == 9 || cy == 10) && to_int(entity_field(c, "deck")) == 0
}

/// The nearest un-held crate within arm's reach of crew `c`, or -1. Distances
/// are compared in WORLD coordinates — `grid_world` composes each entity
/// through whatever frame it currently rides — so a crate floating in space
/// next to the hull is as reachable as one on the deck, without the map caring
/// which of them is which.
fn nearest_crate(c) {
    let cw = grid_world(entity_grid(c), entity_position(c));
    let best = -1;
    let bestd = fixed(0);
    for k in entities_of(2) {
        if to_int(entity_field(k, "held")) != 1 {
            let kw = grid_world(entity_grid(k), entity_position(k));
            let dx = kw.x - cw.x;
            let dy = kw.y - cw.y;
            let dz = kw.z - cw.z;
            let d = dx * dx + dy * dy + dz * dz;
            if best < 0 || d < bestd {
                best = k;
                bestd = d;
            }
        }
    }
    if best >= 0 && bestd <= ratio(9, 4) { best } else { -1 } // 1.5 cells
}

/// One crew member's `use` / `door` presses (rising edges only, so holding the
/// key does not machine-gun the verb).
fn step_use(c) {
    let btn = to_int(entity_field(c, "btn"));
    let prev = to_int(entity_field(c, "prev"));
    entity_set_field(c, "prev", fixed(btn));
    let pressed = btn & (15 - prev); // bits that went 0 -> 1 this tick

    if (pressed & 2) == 2 {
        toggle_door();
    }
    if (pressed & 1) == 1 {
        let carried = to_int(entity_field(c, "carry"));
        if carried == 0 {
            let k = nearest_crate(c);
            if k >= 0 {
                // Aboard, keeping its world spot: a crate lying in space is
                // picked up without jumping, and one already on the deck does
                // not move at all (the conversion is the identity there).
                entity_attach(k, entity_grid(c));
                entity_set_field(k, "held", fixed(1));
                entity_set_field(c, "carry", fixed(k + 1));
            }
        } else {
            let k = carried - 1;
            entity_set_field(k, "held", fixed(0));
            entity_set_field(c, "carry", fixed(0));
            if in_airlock(c) {
                // Released through the airlock: it leaves the ship's frame and
                // stays where it was let go while the hull turns away from it.
                entity_detach(k);
            }
        }
    }
}

/// A carried crate sits on its crew member's cell, in whatever frame that crew
/// member is in — one assignment, because both are in the same frame by
/// construction (the pick-up attached it there).
fn carry_crate(c) {
    let carried = to_int(entity_field(c, "carry"));
    if carried != 0 {
        let p = entity_position(c);
        entity_set_position(carried - 1, vec3(p.x, p.y, p.z));
    }
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
        // Ride the hull grid: the crew's sim position is grid-local, composed
        // through the hull's transform when drawn, so they stay seated as the
        // hull turns (`step_ship` spins it in 3D; host_api 12). This binding is
        // also what puts the FOG on the hull — `vision_observer` names a grid but
        // never binds one, so the cone and the crew always ride the same hull.
        entity_set_grid(c, hull_grid());
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
    // Buttons ride the command's spare z as a bit mask (1 = use, 2 = door): one
    // command per tick either way, and the sim takes the rising edge — the
    // local layer is stateless and cannot debounce anything itself.
    let btn = 0;
    if action_down("use") { btn = btn + 1; }
    if action_down("door") { btn = btn + 2; }
    submit_command(0, 0, vec3(mv.x, mv.y, fixed(btn)));
}

// --- input: store this peer's intent on its crew entity ------------------
fn command(player, verb, target, arg) {
    if verb == 0 {
        ensure_crew(player);
        let c = entities_of(0)[player];
        entity_set_field(c, "ix", arg.x);
        entity_set_field(c, "iy", arg.y);
        entity_set_field(c, "btn", arg.z);
    }
}

// --- one simulation tick ------------------------------------------------
fn tick() {
    step_ship();
    for c in entities_of(0) {
        step_use(c);
        step_crew(c);
        carry_crate(c);
    }
    follow_camera();
    build_deck_hud();
}

/// Advance and apply the hull's 3D rotation. The spin angle is HASHED sim state
/// (an entity field), so every peer turns the hull identically; the crew ride it
/// via grid-local coords, so this is real shared motion, not decoration. The
/// hull tumbles slowly about a TILTED axis (not pure yaw) to exercise full 3D
/// orientation — pitch/roll, not merely a flat spin. It turns in place about
/// the hull's centre, the pivot `init` names via `grid_pivot`.
fn step_ship() {
    let ship = ship_entity();
    let spin = entity_field(ship, "spin") + ratio(2, 100); // rad / tick (30 Hz)
    // Wrap so the fixed-point angle never grows without bound.
    if spin > tau() {
        spin = spin - tau();
    }
    entity_set_field(ship, "spin", spin);
    // Tilted tumble axis (host normalises it): mostly +z (yaw) with a +x lean,
    // so the hull visibly rolls in 3D rather than spinning flat. The whole pose
    // is replaced each tick from the hashed angle — no render-side accumulation.
    grid_orient(hull_grid(), vec3(ratio(3, 10), fixed(0), fixed(1)), spin);
    // …and the hull TRAVELS while it turns: a slow sway along the ship's x, so
    // the frame under the crew translates as well as rotates. Same discipline —
    // derived from the hashed angle, so it is shared motion, not decoration.
    grid_move(hull_grid(), vec3(sin(spin) * fixed(4), fixed(0), fixed(0)));
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
    let c = crew[idx];
    let d = to_int(entity_field(c, "deck"));
    ui_clear();
    ui_scale(fixed(2));
    let label = if d == 0 { "DECK 1 / 2  (lower)" } else { "DECK 2 / 2  (upper)" };
    // `ui_scale(2)` doubles sizes + text height; the second line clears the
    // 12pt first line (≈24px scaled) with room to spare.
    ui_text(8, 8, label, 12);
    let hands = if to_int(entity_field(c, "carry")) == 0 {
        "E: pick up a crate"
    } else if in_airlock(c) {
        "E: release into space"
    } else {
        "E: set the crate down"
    };
    ui_text(8, 42, hands, 8);
    let lock = if door_open() { "F: airlock OPEN (starboard, deck 1)" } else { "F: airlock closed" };
    ui_text(8, 60, lock, 8);
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
        // Focus BY entity so the host composes the crew's grid-local seat through
        // the hull's transform — a bare `camera_focus(entity_position(c))` aims at
        // the un-rotated cell and slides off a turning hull.
        camera_focus_entity(c, entity_position(c));
        // See the world through this crew member's eyes (fog of war), against
        // the hull's own grid so the mask stays grid-local (movable hull).
        vision_observer(c, hull_grid());
        // Show the crew's deck band (floor..deck_top): deck 0 cuts the upper
        // deck away; deck 1 is the top band and cuts nothing.
        let d = to_int(entity_field(c, "deck"));
        deck_clip(deck_floor(d), deck_top(d));
    }
}
