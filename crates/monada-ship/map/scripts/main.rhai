//! The spaceship crew-sim demo — S-B two-deck hull, entirely in the map.
//!
//! The engine knows no genre: this script paints a two-deck voxel hull,
//! defines an animated billboard "crew" actor (reusing the RPG hero GIFs), and
//! walks it around each deck on the deterministic fixed-point sim, crossing
//! decks by climbing a fore-starboard stairwell. The host injects one real-time input
//! command per tick (verb 0): `arg = vec3(move_x, move_y, buttons)`, and a
//! client with a pointer adds a second (verb 1): the hull CELL its cursor is
//! on. Render calls (`entity_set_anim`, `entity_set_facing`, `camera_focus`,
//! `gizmo_*`) never touch the hashed world, so they can't desync a peer.
//!
//! Collision is DECK-RELATIVE (see the `decks` section): the crew tracks which
//! deck it stands on and tests only that deck's walls, so the stacked floor
//! slabs don't confuse seating and the upper deck never hits lower walls. NB
//! walls are a script-side predicate, not `voxel_solid` — monada's heightmap
//! collision store can't represent air below a floor (see `blocked`).
//!
//! The hull is a RIGID BODY (docs/plans/ship-physics.md): `init` authors it
//! once, into the voxels the player sees and the cells the dynamics weigh, and
//! `grid_body` makes the body's pose the grid's frame. So the ship's motion is
//! not something this map computes any more — it fires engines and the physics
//! decides. Nothing about walking around inside it changed: the crew's
//! positions are hull-local, and `blocked` / `reachable` / the stairs are the
//! same cells they always were, at any attitude and under any acceleration.
//!
//! The view/visibility seams ride the same frame: wall cutout, deck clip, fog
//! of war (docs/plans/ship-visibility.md).
//!
//! Cargo is CURSOR-DIRECTED (docs/plans/ship-building.md): `pick_cell` resolves
//! this client's pointer against the hull's own geometry and answers in the
//! hull's own cells, so the crew picks up what you point at and puts it down on
//! the cell you point at — snapped, rotatable, refused where it will not fit,
//! and previewed by a ghost the local layer draws with `gizmo_*` and colours
//! with the very predicate the tick decides by (`can_place`). A stowed prop
//! blocks, so where things go is a decision.
//!
//! Archetypes register in order, so ids are fixed: CREW = 0, SHIP = 1,
//! CRATE = 2, ENGINE = 3. Model ids by registration order: crew actor = 0,
//! crate = 1, locker = 2.

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

/// The 8-way sector a direction falls in: 0=E,1=NE,2=N,3=NW,4=W,5=SW,6=S,7=SE
/// (sim space: +x east, +y north, CCW). The renderer bins to 8 sprites, so
/// exact atan2 isn't needed.
fn dir_octant(dx, dy) {
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
    k
}

/// 8-way facing yaw (radians) from a direction vector: sector k → k·tau/8.
fn dir_yaw(dx, dy) { ratio(dir_octant(dx, dy), 8) * tau() }

/// The unit cell step of sector `k` — which cell a crew member is facing.
/// Kept as sim state (`fdx`/`fdy`) because it is what a placement aims at
/// when this peer has no cursor to aim with (see `aim_cell`).
fn octant_dx(k) {
    if k == 0 || k == 1 || k == 7 { 1 } else if k >= 3 && k <= 5 { -1 } else { 0 }
}

fn octant_dy(k) {
    if k >= 1 && k <= 3 { 1 } else if k >= 5 && k <= 7 { -1 } else { 0 }
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
    // Cargo is as solid as a bulkhead. A placement system whose output you
    // walk straight through decides nothing, so a prop somebody put down
    // blocks — which is what makes a crate in a doorway a barricade and
    // where you leave a locker a choice (docs/plans/ship-building.md).
    // Only STOWED props: the one in your hands is inside your own cell.
    if occupied(cx, cy, deck) {
        return true;
    }
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
    // Crew: movement intent (ix/iy), last facing yaw and its cell step
    // (fdx/fdy — where "in front of me" is), current deck (0 lower / 1 upper
    // — follows z on the stairwell ramp), the carried prop (its id + 1,
    // 0 = hands free), last tick's button mask (so `use` / `door` / `rotate`
    // act on the press, not on every tick the key is held), and the
    // PLACEMENT AIM: the hull cell this player's cursor named this tick
    // (aimx/aimy/aimz, valid while `aim` is 1) and the rotation they have
    // turned what they hold to (`rot`, 0..3). The aim is hashed like any
    // other command payload — where a cursor IS stays per-client, but what
    // it asked for is a decision every peer has to agree about.
    archetype([
        "ix", "iy", "face", "fdx", "fdy", "deck", "carry", "btn", "prev",
        "aimx", "aimy", "aimz", "aim", "rot",
    ]);

    // Ship: the airlock state, and the physics body the hull IS. The hull's
    // pose is no longer state this map keeps — it is an outcome of the
    // dynamics, carried into the grid's frame by `grid_body` after every step
    // (docs/plans/ship-physics.md). What stays hashed here is what the map
    // decides: whether the airlock is open, and which body is the ship.
    // Archetype id 1 (after CREW = 0).
    archetype(["door", "body"]);
    let ship = entity_create(1);

    // Cargo: "is someone holding me", what KIND of thing it is (0 = a
    // one-cell crate, 1 = a two-cell floor locker) and which way it lies
    // (`rot`, 0..3). What makes a prop interesting is still WHICH FRAME it
    // is in — riding the hull, or released into the world (see `step_use`)
    // — but now also WHERE on the deck, because a placed prop blocks.
    // Archetype id 2.
    archetype(["held", "kind", "rot"]);

    // Engines: a thruster is an ENTITY on the hull, at a hull cell, pointing
    // along a hull direction, with a throttle. Nothing about it is engine-side
    // — `phys_thrust` takes a place, a direction and a force, and everything
    // that makes this one a main drive rather than a tractor beam is the four
    // fields below. Archetype id 3.
    archetype(["dx", "dy", "dz", "throttle"]);

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

    // Space: nothing falls. The map owns feel constants, so gravity is a map
    // decision like any other — a station under spin gravity would say
    // something else here and change nothing about the code below.
    phys_gravity(fixed(0), fixed(0), fixed(0));
    // Hull steel. FIRST material registered, so it is id 0 — the id every
    // un-materialed paint writes (the material-0 contract). This map paints no
    // world terrain at all, so id 0 is simply what the ship is made of.
    let steel = phys_material(fixed(1), ratio(6, 10), ratio(1, 10), fixed(4));

    // The hull, authored ONCE into two places: the voxels the player sees and
    // the cells the dynamics weigh. `build_hull` writes both through
    // `hull_box`, so the ship's mass, centre of mass and inertia are the real
    // geometry's — a shell's, not the solid block that bounds it. The two can
    // no longer drift apart, which is the same contract `blocked()` still owes
    // the paint by hand.
    let shape = phys_shape(20, 20, 6);
    build_hull(ship_grid, shape, steel);
    let body = phys_body(shape, vec3(fixed(0), fixed(0), fixed(0)));
    entity_set_field(ship, "body", fixed(body));
    // From here the map does not pose the hull at all: the body's pose becomes
    // the grid's frame after every physics step, and the grid's pivot becomes
    // the body's centre of mass — so the ship turns about the point it really
    // turns about, and no hand-authored pivot can drift from it. Crew, crates,
    // the fog cone, the deck cutaway and the camera all ride the frame exactly
    // as they did when a hashed `spin` field drove it.
    grid_body(ship_grid, body);

    // The drive: two main engines low and aft, thrusting along the hull's +x,
    // mounted symmetrically about the centreline so full throttle pushes
    // straight rather than yawing. Manoeuvring is `phys_torque` in `step_ship`
    // — a reaction wheel rather than nozzles, because a hull this size wants to
    // turn on the spot, not fly a burn to rotate.
    mount_engine(0, 7, 1, 1, 0, 0);
    mount_engine(0, 12, 1, 1, 0, 0);

    // The animated billboard crew actor (id 0 by call order). The 3rd arg is
    // the rendered height in cells; the host scales the art to it. Reuses the
    // RPG hero's idle/run GIFs under assets/char/crew.
    model_actor("assets/char/crew", ["idle", "run"], ratio(14, 10));
    // The crate: a procedural box, model id 1 (the call after the actor).
    model_box(9, 9, 9, 0x80a8_7a3c);
    // The floor locker, model id 2: two cells long, so which way it lies is
    // a fact about the deck rather than a decoration — which is what makes
    // rotation worth having a key for.
    model_box(26, 11, 13, 0x8074_8c96);

    // Cargo stowed on the lower deck, authored in HULL coordinates — so
    // `entity_set_grid` (the raw bind) is right here: nothing should move, the
    // numbers already mean "this cell of the ship". `entity_attach` is for
    // later, when a crate that is somewhere in the WORLD has to come aboard
    // without jumping.
    //
    // NB none of them sits in a walking line the crew uses: a stowed prop
    // blocks now (see `blocked`), so where the demo parks its cargo is a
    // level-design decision and not just dressing.
    stow_prop(0, 4, 3, 0); // a crate beside the crew's spawn, within arm's reach
    stow_prop(1, 2, 3, 0); // a locker beside it, lying east-west (two cells)
    stow_prop(0, 12, 14, 0); // another crate aft, across the divider's doorway
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

    status("ship demo — WASD walk, point + F/click to move cargo, R turn it, G airlock, SPACE burn, Q/E turn ship; the hull is a rigid body and you are standing in it");
}

/// Paint the two-deck hull into `grid`, for the EYE. Collision is the `blocked`
/// predicate above (the heightmap store can't do stacked decks); keep this
/// layout and that predicate in sync. A deck is a floor plate + a two-cell rim
/// wall band; an interior divider (with a doorway gap) splits each deck into two
/// compartments joined by the gap — the "corridor". The two decks get
/// perpendicular dividers so they read distinct, and sit `DECK_STRIDE = 3`
/// cells apart. Every z here is a CELL of the cubic grid (16 world voxels), so
/// a "one cell" wall is as tall as the crew is wide.
fn build_hull(grid, shape, mat) {
    let plate = 0x8055_5f6b; // steel floor plating
    let wall = 0x8079_8592; // hull wall
    let step = 0x80b8_9850; // brass stair steps

    // --- deck 0 (lower): floor plate cell 0, walls cells 1..2 ---
    hull_box(grid, shape, 0, 0, 0, 19, 19, 0, plate, mat);
    rim_walls(grid, shape, 1, 2, wall, mat);
    // Full divider along y=10 splitting fore (y<10) / aft (y>10), doorway gap
    // x 9..10 (the stairwell is off this row, so no other opening is needed).
    hull_box(grid, shape, 1, 10, 1, 8, 10, 2, wall, mat);
    hull_box(grid, shape, 11, 10, 1, 18, 10, 2, wall, mat);

    // --- deck 1 (upper): floor plate cell 3 with a HOLE over the staircase
    // (cx 15..18 × cy 1..2, so the crew climbs up through it), walls cells 4..5 ---
    hull_box(grid, shape, 0, 0, 3, 14, 19, 3, plate, mat); // west of the stair hole
    hull_box(grid, shape, 19, 0, 3, 19, 19, 3, plate, mat); // east sliver (under the rim)
    hull_box(grid, shape, 15, 3, 3, 18, 19, 3, plate, mat); // north of the hole (cy 3..19)
    rim_walls(grid, shape, 4, 5, wall, mat);
    // Divider along x=10 splitting port (x<10) / starboard (x>10), gap y 9..10.
    hull_box(grid, shape, 10, 1, 4, 10, 8, 5, wall, mat);
    hull_box(grid, shape, 10, 11, 4, 10, 18, 5, wall, mat);

    // The staircase (fore-starboard corner, cy 1..2) climbs EAST: solid step
    // blocks at cx 15..18, each a column from cell 0 up to `stair_z(cx)` — 0 at
    // cx 15 (flush with the lower floor) to 3 (the upper floor) at cx 18, one
    // cell per step. No surrounding wall: the blocks' own risers do the
    // containment (see `reachable`). Keep in sync with `on_stairs` / `stair_z`.
    hull_box(grid, shape, 15, 1, 0, 15, 2, 0, step, mat); // cx 15 → z 0 (lower floor)
    hull_box(grid, shape, 16, 1, 0, 16, 2, 1, step, mat);
    hull_box(grid, shape, 17, 1, 0, 17, 2, 2, step, mat);
    hull_box(grid, shape, 18, 1, 0, 18, 2, 3, step, mat); // cx 18 → z 3 (upper floor)
}

/// One authored cell box of hull: painted for the eye AND filled into the body
/// shape for the dynamics, in one call so the two cannot drift apart. The
/// ship's mass, centre of mass and inertia are whatever this paints — which is
/// why the hull flies like a shell rather than like the brick that bounds it.
fn hull_box(grid, shape, x0, y0, z0, x1, y1, z1, col, mat) {
    voxel_fill_in(grid, x0, y0, z0, x1, y1, z1, col);
    phys_shape_fill(shape, x0, y0, z0, x1, y1, z1, mat);
}

/// A rim wall band (z `lo`..`hi`) around the 20×20 hull edge.
fn rim_walls(grid, shape, lo, hi, col, mat) {
    hull_box(grid, shape, 0, 0, lo, 19, 0, hi, col, mat); // south
    hull_box(grid, shape, 0, 19, lo, 19, 19, hi, col, mat); // north
    hull_box(grid, shape, 0, 0, lo, 0, 19, hi, col, mat); // west
    hull_box(grid, shape, 19, 0, lo, 19, 19, hi, col, mat); // east
}

/// Bolt a thruster to hull cell `(cx, cy, cz)` pointing along hull direction
/// `(dx, dy, dz)`. It rides the hull grid like the crew do, so its anchor is
/// authored in the same cells the hull is painted in — and `phys_thrust` reads
/// exactly that, in the shape coordinates `build_hull` filled.
fn mount_engine(cx, cy, cz, dx, dy, dz) {
    let e = entity_create(3);
    entity_set_position(e, vec3(fixed(cx), fixed(cy), fixed(cz)));
    entity_set_grid(e, hull_grid());
    entity_set_field(e, "dx", fixed(dx));
    entity_set_field(e, "dy", fixed(dy));
    entity_set_field(e, "dz", fixed(dz));
    entity_set_field(e, "throttle", fixed(0));
}

// --- cargo + airlock -----------------------------------------------------
// The slice that exercises grid membership (docs/plans/grid-entities.md): a
// crate is an entity whose FRAME is the gameplay. Stowed, it rides the hull and
// tumbles with it; dropped through the open airlock it is released into the
// world frame and stays exactly where it was let go, while the ship turns away
// from it. `entity_attach` / `entity_detach` are what make those two the same
// crate rather than a teleport.
//
// Where on the deck a prop stands is the other half (docs/plans/ship-building.md):
// it snaps to a cell, it blocks, and it goes where the CURSOR points rather
// than where the crew happens to stand.

/// The kinds of prop the ship stows, as a footprint LENGTH in cells: a crate
/// is one cell, a floor locker two. Rhai has no enum, so a map writes one as
/// a function over an int (see the book's "an enum in a map").
fn kind_cells(kind) { if kind == 1 { 2 } else { 1 } }

/// The render model each kind binds (`init` registers them in this order).
fn kind_model(kind) { if kind == 1 { 2 } else { 1 } }

fn kind_name(kind) { if kind == 1 { "locker" } else { "crate" } }

/// The unit cell step of rotation `r` — 0 = +x, 1 = +y, 2 = -x, 3 = -y —
/// i.e. which way a multi-cell prop lies from its first cell.
fn rot_dx(r) { if r == 0 { 1 } else if r == 2 { -1 } else { 0 } }

fn rot_dy(r) { if r == 1 { 1 } else if r == 3 { -1 } else { 0 } }

/// The facing yaw that draws a prop lying along rotation `r`.
fn rot_yaw(r) { ratio(r, 4) * tau() }

/// Where a prop of `n` cells STANDS when its first cell is `(cx, cy)` on
/// `deck`: the centre of its footprint, because the model is drawn centred on
/// the entity. A one-cell prop lands on the cell centre exactly as crates
/// always have; a two-cell one sits on the half-cell between its two.
fn prop_seat(cx, cy, deck, r, n) {
    let half = ratio(n - 1, 2);
    vec3(
        fixed(cx) + fixed(rot_dx(r)) * half,
        fixed(cy) + fixed(rot_dy(r)) * half,
        fixed(deck_floor(deck)),
    )
}

/// A prop's FIRST footprint cell — `prop_seat` read backwards.
fn prop_base_x(k) {
    let n = kind_cells(to_int(entity_field(k, "kind")));
    let r = to_int(entity_field(k, "rot"));
    cell(entity_position(k).x - fixed(rot_dx(r)) * ratio(n - 1, 2))
}

fn prop_base_y(k) {
    let n = kind_cells(to_int(entity_field(k, "kind")));
    let r = to_int(entity_field(k, "rot"));
    cell(entity_position(k).y - fixed(rot_dy(r)) * ratio(n - 1, 2))
}

/// Does STOWED prop `k` stand on hull cell `(cx, cy)` of `deck`? A carried
/// prop covers nothing — it is inside its carrier's own cell, and a crew
/// member is not an obstacle to themselves.
fn prop_covers(k, cx, cy, deck) {
    if to_int(entity_field(k, "held")) == 1 {
        return false;
    }
    let p = entity_position(k);
    if deck_of(to_int(p.z)) != deck {
        return false;
    }
    let r = to_int(entity_field(k, "rot"));
    let n = kind_cells(to_int(entity_field(k, "kind")));
    let bx = prop_base_x(k);
    let by = prop_base_y(k);
    let i = 0;
    while i < n {
        if bx + rot_dx(r) * i == cx && by + rot_dy(r) * i == cy {
            return true;
        }
        i += 1;
    }
    false
}

/// Is a prop standing on this cell? The cargo half of `blocked`.
fn occupied(cx, cy, deck) {
    for k in entities_of(2) {
        if prop_covers(k, cx, cy, deck) {
            return true;
        }
    }
    false
}

/// Is a crew member standing on this cell? Placement's one rule that is about
/// people rather than architecture: a prop blocks, so dropping one on somebody
/// would wall them in.
///
/// The CELL somebody is in, not the cells their footprint grazes. A crew
/// member is 0.8 of a cell wide, so it almost always overlaps a neighbour by
/// a hair, and refusing those cells would refuse the cell right in front of
/// you half the time. The graze is what ordinary collision is for: a prop put
/// down against you blocks the step INTO it and nothing else, so you can
/// always back off — which is exactly what walking into a crate should feel
/// like.
fn crew_at(cx, cy, deck) {
    for c in entities_of(0) {
        if to_int(entity_field(c, "deck")) == deck {
            let p = entity_position(c);
            if cell(p.x) == cx && cell(p.y) == cy {
                return true;
            }
        }
    }
    false
}

/// Is cell `(cx, cy)` within arm's reach of a crew member at `(px, py)`?
/// The same 1.5 cells `use` has always had — you stow cargo, you do not
/// build across the room.
fn in_reach(cx, cy, px, py) {
    let dx = fixed(cx) - px;
    let dy = fixed(cy) - py;
    dx * dx + dy * dy <= ratio(9, 4)
}

/// Can a prop `n` cells long go down with its first cell at `(cx, cy)`,
/// lying along `r`, for a crew member at `(px, py)` on `deck`?
///
/// **THE predicate.** The ghost colours itself with it and `step_use`
/// accepts or refuses with it — one function, called from both sides of the
/// sync wall, because the local layer compiles this same script. So a green
/// ghost cannot lie about what the tick will do, and the tick still does not
/// trust the ghost: it runs the rule again on the command.
fn can_place(cx, cy, deck, r, n, px, py) {
    if !in_reach(cx, cy, px, py) {
        return false;
    }
    let i = 0;
    while i < n {
        let x = cx + rot_dx(r) * i;
        let y = cy + rot_dy(r) * i;
        // `blocked` already covers walls, the rim, a shut airlock — and
        // other props, since cargo blocks.
        if blocked(x, y, deck) || on_stairs(x, y) || crew_at(x, y, deck) {
            return false;
        }
        i += 1;
    }
    true
}

/// Put a fresh prop of `kind` on the lower deck, first cell `(cx, cy)`,
/// lying along `r`.
fn stow_prop(kind, cx, cy, r) {
    let k = entity_create(2);
    entity_set_field(k, "kind", fixed(kind));
    entity_set_field(k, "rot", fixed(r));
    entity_set_position(k, prop_seat(cx, cy, 0, r, kind_cells(kind)));
    entity_set_model(k, kind_model(kind));
    entity_set_facing(k, rot_yaw(r));
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

// --- aiming ---------------------------------------------------------------
// Where a player is pointing is a fact about their CLIENT — the sim is never
// offered a cursor (`monada-runtime`'s `Host` has no input at all). So the
// local layer resolves the cursor against the hull's own geometry
// (`pick_cell`) and submits the CELL it named as an ordinary command, and the
// simulation acts on that cell like any other player intent. What each peer
// sees stays local; what it asked for is agreed by everybody.

/// This crew member's placement target: the hull cell their client's cursor
/// named this tick, or — for a peer with no cursor at all (the oracle, a
/// dedicated server, a keyboard-only session with the pointer off the ship)
/// — the cell they are facing. Always a cell; `aim_on_deck` says whether it
/// is one this crew member may act on.
fn aim_cell(c) {
    if to_int(entity_field(c, "aim")) == 1 {
        return vec3(
            entity_field(c, "aimx"),
            entity_field(c, "aimy"),
            entity_field(c, "aimz"),
        );
    }
    let p = entity_position(c);
    let deck = to_int(entity_field(c, "deck"));
    vec3(
        fixed(cell(p.x) + to_int(entity_field(c, "fdx"))),
        fixed(cell(p.y) + to_int(entity_field(c, "fdy"))),
        fixed(deck_floor(deck)),
    )
}

/// Is the aim on the crew member's OWN deck? The cursor can perfectly well
/// land on the deck below through the stairwell hole, or on the far side of
/// the hull once the fog lifts — you can see it, you cannot reach it.
fn aim_on_deck(c, a) {
    if to_int(entity_field(c, "aim")) != 1 {
        return true; // the facing fallback is always this deck
    }
    deck_of(to_int(a.z)) == to_int(entity_field(c, "deck"))
}

/// The prop standing on cell `(cx, cy)` within reach of `(px, py)`, or -1 —
/// "the one under the cursor", which is what the players asked for.
fn prop_at(cx, cy, deck, px, py) {
    if !in_reach(cx, cy, px, py) {
        return -1;
    }
    for k in entities_of(2) {
        if prop_covers(k, cx, cy, deck) {
            return k;
        }
    }
    -1
}

/// What this crew member's `use` picks up: the prop under their own cursor,
/// or the nearest one within arm's reach when this peer has no cursor to
/// point with. The cursor answer is the one the demo is played with; the
/// fallback is what a headless peer and a keyboard-only session run on, and
/// it is the behaviour the demo shipped with.
fn prop_to_take(c) {
    let p = entity_position(c);
    let deck = to_int(entity_field(c, "deck"));
    if to_int(entity_field(c, "aim")) == 1 {
        let a = aim_cell(c);
        if !aim_on_deck(c, a) {
            return -1;
        }
        return prop_at(to_int(a.x), to_int(a.y), deck, p.x, p.y);
    }
    nearest_prop(c)
}

/// The nearest un-held prop within arm's reach of crew `c`, or -1. Distances
/// are compared in WORLD coordinates — `grid_world` composes each entity
/// through whatever frame it currently rides — so a crate floating in space
/// next to the hull is as reachable as one on the deck, without the map caring
/// which of them is which.
fn nearest_prop(c) {
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

/// One crew member's `use` / `door` / `rotate` presses (rising edges only, so
/// holding the key does not machine-gun the verb).
fn step_use(c) {
    let btn = to_int(entity_field(c, "btn"));
    let prev = to_int(entity_field(c, "prev"));
    entity_set_field(c, "prev", fixed(btn));
    // Bits that went 0 -> 1 this tick, over the EDGE bits only (1 = use,
    // 2 = airlock, 32 = rotate). Subtracting from the mask complements it over
    // exactly those bits. The flight controls between them (4 = burn,
    // 8/16 = turn) are held states, read as they are — an engine fires while
    // the key is down.
    let pressed = btn & (35 - (prev & 35));

    if (pressed & 32) == 32 {
        // Which way the next thing you set down will lie. Hashed on the crew
        // member, not kept by the local layer, because the ghost and the
        // placement have to mean the same rotation — and the local layer is
        // stateless by design (it cannot even debounce a key).
        entity_set_field(c, "rot", fixed((to_int(entity_field(c, "rot")) + 1) % 4));
    }
    if (pressed & 2) == 2 {
        toggle_door();
    }
    if (pressed & 1) == 1 {
        let carried = to_int(entity_field(c, "carry"));
        if carried == 0 {
            let k = prop_to_take(c);
            if k >= 0 {
                // Aboard, keeping its world spot: a crate lying in space is
                // picked up without jumping, and one already on the deck does
                // not move at all (the conversion is the identity there).
                entity_attach(k, entity_grid(c));
                entity_set_field(k, "held", fixed(1));
                entity_set_field(c, "carry", fixed(k + 1));
            }
        } else {
            set_down(c, carried - 1);
        }
    }
}

/// Set prop `k` down — through the airlock into space, or onto the deck cell
/// the crew member is aiming at.
///
/// A refused placement does nothing at all: you keep holding the thing, and
/// the ghost has already been red for as long as you have been pointing
/// there. That is the whole contract between the preview and the rule.
fn set_down(c, k) {
    if in_airlock(c) {
        // Released through the airlock: it leaves the ship's frame and stays
        // where it was let go while the hull turns away from it.
        entity_set_field(k, "held", fixed(0));
        entity_set_field(c, "carry", fixed(0));
        entity_detach(k);
        return;
    }
    let a = aim_cell(c);
    if !aim_on_deck(c, a) {
        return;
    }
    let p = entity_position(c);
    let deck = to_int(entity_field(c, "deck"));
    let r = to_int(entity_field(c, "rot"));
    let n = kind_cells(to_int(entity_field(k, "kind")));
    let cx = to_int(a.x);
    let cy = to_int(a.y);
    if !can_place(cx, cy, deck, r, n, p.x, p.y) {
        return;
    }
    entity_set_field(k, "held", fixed(0));
    entity_set_field(k, "rot", fixed(r));
    entity_set_field(c, "carry", fixed(0));
    entity_set_position(k, prop_seat(cx, cy, deck, r, n));
    entity_set_facing(k, rot_yaw(r));
}

/// A carried prop sits on its crew member's cell, in whatever frame that crew
/// member is in — one assignment, because both are in the same frame by
/// construction (the pick-up attached it there) — and turns as they rotate it,
/// so `R` is visible in your hands before it is visible on the deck.
fn carry_crate(c) {
    let carried = to_int(entity_field(c, "carry"));
    if carried != 0 {
        let p = entity_position(c);
        entity_set_position(carried - 1, vec3(p.x, p.y, p.z));
        entity_set_facing(carried - 1, rot_yaw(to_int(entity_field(c, "rot"))));
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
        // Facing east on spawn: `fdx`/`fdy` are where "in front of me" is, and
        // a crew member who has never taken a step still has a front.
        entity_set_field(c, "fdx", fixed(1));
        entity_set_field(c, "fdy", fixed(0));
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
    // Buttons ride the command's spare z as a bit mask: one command per tick
    // either way. The low two are VERBS the sim takes on the rising edge (the
    // local layer is stateless and cannot debounce anything itself); the flight
    // controls above them are HELD states — an engine burns while the key is
    // down, so there is nothing to debounce.
    let btn = 0;
    if action_down("use") { btn = btn + 1; }
    if action_down("door") { btn = btn + 2; }
    if action_down("burn") { btn = btn + 4; }
    // NB `action_axis` answers with an INT (-1, 0, +1), unlike `action_axis2`'s
    // vec3 of `Fixed`. Comparing it against `fixed(0)` is an operator Rhai has
    // no registration for, and Rhai answers such a comparison with `false`
    // instead of raising — so the turn keys did nothing at all and said nothing
    // about it. `every_control_reaches_the_command` now watches every bit.
    let turn = action_axis("turn");
    if turn > 0 { btn = btn + 8; }
    if turn < 0 { btn = btn + 16; }
    if action_down("rotate") { btn = btn + 32; }
    submit_command(0, 0, vec3(mv.x, mv.y, fixed(btn)));

    // Verb 1 — where this client's cursor is, as a HULL CELL. `pick_cell`
    // marches the cursor ray through the scene's geometry and answers in the
    // grid's own cells (host_api 24), so the ship gets the same numbers
    // `build_hull` painted with, at any attitude, and the deck cutaway means
    // the hit lands on the deck being looked into rather than the roof over
    // it. Sent only while the cursor is on the ship: the sim clears the aim
    // every tick, so silence IS "no cursor" and a peer without a window
    // simply never sends one.
    let cell = pick_cell(hull_grid());
    if type_of(cell) == "Vec3" {
        submit_command(1, 0, cell);
    }
}

/// This client's placement preview, drawn every FRAME rather than every tick:
/// a cursor moves between ticks, and a ghost that lags the pointer by 33 ms
/// feels broken in a way a walking crew member does not.
///
/// Nothing here is hashed and nothing here is trusted. The gizmos are overlay
/// lines this client queues and its own frame draws; the verdict they are
/// coloured by comes from `can_place` — the very function `step_use` refuses
/// with, because the local layer compiles this same script.
fn local_frame(dt) {
    gizmo_clear();
    let crew = entities_of(0);
    let lp = local_player();
    let idx = if lp < 0 { 0 } else { lp };
    if idx >= crew.len() {
        return;
    }
    let c = crew[idx];
    let deck = to_int(entity_field(c, "deck"));
    let z = deck_floor(deck);
    let p = entity_position(c);

    // The snap lattice: every cell within arm's reach, outlined on the deck
    // plate itself. Depth-tested, so the buried half of each cell cube is
    // occluded by the floor and what is left reads as a square painted on it.
    gizmo_style(1, false);
    let dy = -2;
    while dy <= 2 {
        let dx = -2;
        while dx <= 2 {
            let x = cell(p.x) + dx;
            let y = cell(p.y) + dy;
            if in_reach(x, y, p.x, p.y) && !blocked(x, y, deck) {
                gizmo_box(hull_grid(), x, y, z, x, y, z, 0x3078_b4ff);
            }
            dx += 1;
        }
        dy += 1;
    }

    // What the cursor is on. The ghost sits in the cell ABOVE the deck plate
    // — where the thing itself would stand — and shows through the hull, so
    // it is never hidden by the crate you are standing behind.
    let a = aim_cell(c);
    let ax = to_int(a.x);
    let ay = to_int(a.y);
    let carried = to_int(entity_field(c, "carry"));
    gizmo_style(3, true);
    if carried == 0 {
        // Hands free: outline what `use` would pick up, so "the one under the
        // cursor" is something you can see before you press.
        if aim_on_deck(c, a) {
            let k = prop_at(ax, ay, deck, p.x, p.y);
            if k >= 0 {
                let r = to_int(entity_field(k, "rot"));
                let n = kind_cells(to_int(entity_field(k, "kind")));
                let bx = prop_base_x(k);
                let by = prop_base_y(k);
                gizmo_box(
                    hull_grid(), bx, by, z + 1,
                    bx + rot_dx(r) * (n - 1), by + rot_dy(r) * (n - 1), z + 1,
                    0xb0ff_e878,
                );
            }
        }
    } else {
        let r = to_int(entity_field(c, "rot"));
        let n = kind_cells(to_int(entity_field(carried - 1, "kind")));
        let ok = aim_on_deck(c, a) && can_place(ax, ay, deck, r, n, p.x, p.y);
        let col = if ok { 0xb040_ff70 } else { 0xb0ff_5050 };
        gizmo_box(
            hull_grid(), ax, ay, z + 1,
            ax + rot_dx(r) * (n - 1), ay + rot_dy(r) * (n - 1), z + 1,
            col,
        );
    }
}

// --- input: store this peer's intent on its crew entity ------------------
fn command(player, verb, target, arg) {
    if verb == 0 {
        ensure_crew(player);
        let c = entities_of(0)[player];
        entity_set_field(c, "ix", arg.x);
        entity_set_field(c, "iy", arg.y);
        entity_set_field(c, "btn", arg.z);
    } else if verb == 1 {
        // The aim: a hull cell this player's cursor named. `tick` clears the
        // flag after the verbs that read it, so an aim is only ever one tick
        // old and a client that stops pointing stops aiming.
        ensure_crew(player);
        let c = entities_of(0)[player];
        entity_set_field(c, "aimx", arg.x);
        entity_set_field(c, "aimy", arg.y);
        entity_set_field(c, "aimz", arg.z);
        entity_set_field(c, "aim", fixed(1));
    }
}

// --- one simulation tick ------------------------------------------------
fn tick() {
    step_ship();
    for c in entities_of(0) {
        step_use(c);
        step_crew(c);
        carry_crate(c);
        // The aim is a per-tick fact, cleared AFTER the verbs that read it:
        // a client says where it is pointing every tick it has a cursor, so
        // no stale aim is ever acted on and a peer that stops pointing falls
        // back to what it faces.
        entity_set_field(c, "aim", fixed(0));
    }
    follow_camera();
    build_deck_hud();
}

/// Fly the ship. Nothing here poses the hull: the map fires engines, the
/// dynamics decide where the ship ends up, and `grid_body` carries the answer
/// into the frame the crew stand in. That inversion is the whole slice — the
/// hull's motion used to be a hashed angle this function advanced, and is now
/// an outcome nobody stores.
///
/// Controls (any crew member; the ship has no console yet): SPACE burns the
/// main drive, Q/E turn. Fixed-point throughout, so every peer flies the same
/// ship.
fn step_ship() {
    let body = ship_body();
    let cmd = ship_controls();

    // Main drive: one throttle across both engines, so the pair pushes along
    // the centreline. Each engine applies its own force at its own anchor —
    // lose one to a hull breach some day and the ship yaws, for free.
    let throttle = if (cmd & 4) == 4 { fixed(1) } else { fixed(0) };
    for e in entities_of(3) {
        entity_set_field(e, "throttle", throttle);
        if throttle > fixed(0) {
            phys_thrust(
                body,
                entity_position(e),
                vec3(entity_field(e, "dx"), entity_field(e, "dy"), entity_field(e, "dz")),
                engine_force(),
            );
        }
    }

    // Attitude: a reaction wheel about the hull's own up axis, so Q/E turn the
    // ship on the spot rather than flying an arc. `grid_world` maps the hull's
    // +z into the world, which is what a torque wants — the hull may be at any
    // attitude by the time the player presses the key.
    let up = hull_axis(fixed(0), fixed(0), fixed(1));
    if (cmd & 8) == 8 {
        phys_torque(body, scaled(up, rcs_force()));
    }
    if (cmd & 16) == 16 {
        phys_torque(body, scaled(up, -rcs_force()));
    }

    // …and the stabiliser that makes it flyable: bleed off whatever tumble the
    // player is not asking for (`τ = −k·ω`). Three lines and no engine knob —
    // how hard a ship fights its own inertia is a property of the SHIP, so it
    // belongs here (DESIGN.md §3.2).
    let w = phys_angvel(body);
    phys_torque(body, vec3(-w.x * damp(), -w.y * damp(), -w.z * damp()));
}

/// A hull-local direction in world coordinates: the difference between two
/// points of the hull's frame, so the hull's translation cancels and only its
/// rotation remains. What a world-frame torque needs from a ship that may be
/// at any attitude.
fn hull_axis(dx, dy, dz) {
    let o = grid_world(hull_grid(), vec3(fixed(0), fixed(0), fixed(0)));
    let p = grid_world(hull_grid(), vec3(dx, dy, dz));
    vec3(p.x - o.x, p.y - o.y, p.z - o.z)
}

/// A vector times a scalar (no `Vec3 * Fixed` operator in the host API).
fn scaled(v, s) { vec3(v.x * s, v.y * s, v.z * s) }

/// The body the hull IS, stored on the ship entity by `init` rather than
/// written as a literal: which id `phys_body` hands out is an ordering detail
/// of setup, exactly like the entity id `ship_entity` looks up.
fn ship_body() { to_int(entity_field(ship_entity(), "body")) }

/// Main-drive force, in mass·cells/s². The hull weighs ~1500 (its cells at
/// density 1), so this is about 1 cell/s² per engine — a freighter, not a
/// fighter.
fn engine_force() { fixed(750) }

/// Reaction-wheel torque, and the stabiliser gain that bleeds off any tumble
/// the player did not ask for. Both are torques, so they are sized against the
/// hull's INERTIA (~90 000 for a 20-cell shell of this mass), not its mass: the
/// gain gives roughly a tenth of the remaining spin back per tick, so a turn
/// settles in about a third of a second, and the wheel's torque against that
/// gain sets the top turn rate at `rcs_force / damp` ≈ 0.6 rad/s.
fn rcs_force() { fixed(170000) }
fn damp() { fixed(280000) }

/// The ship-control bits of the local crew's input mask (4 = burn, 8/16 =
/// turn). Read from crew 0 rather than summed: a two-player hull has one
/// helm, and the alternative — every crew member's keys adding up — would make
/// the ship fly twice as fast in co-op.
fn ship_controls() {
    let crew = entities_of(0);
    if crew.len() == 0 {
        return 0;
    }
    to_int(entity_field(crew[0], "btn"))
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
    // Cargo is cursor-directed now, so the prompt says what the CURSOR is on
    // rather than what the verb is called — the two differ, and which one
    // differs is the whole feature.
    let carried = to_int(entity_field(c, "carry"));
    let a = aim_cell(c);
    let deck = d;
    let p = entity_position(c);
    let hands = if carried != 0 {
        if in_airlock(c) {
            "F / click: release into space"
        } else {
            let r = to_int(entity_field(c, "rot"));
            let n = kind_cells(to_int(entity_field(carried - 1, "kind")));
            if aim_on_deck(c, a)
                && can_place(to_int(a.x), to_int(a.y), deck, r, n, p.x, p.y)
            {
                "F / click: put it there   R: turn it"
            } else {
                "no room there   R: turn it"
            }
        }
    } else if aim_on_deck(c, a)
        && prop_at(to_int(a.x), to_int(a.y), deck, p.x, p.y) >= 0
    {
        "F / click: pick up the " + kind_name(
            to_int(entity_field(prop_at(to_int(a.x), to_int(a.y), deck, p.x, p.y), "kind")),
        )
    } else {
        "point at a crate to pick it up"
    };
    ui_text(8, 42, hands, 8);
    let lock = if door_open() { "G: airlock OPEN (starboard, deck 1)" } else { "G: airlock closed" };
    ui_text(8, 60, lock, 8);

    // The flight readout. Speed and spin are the two numbers that tell a
    // player whether the ship is doing what they asked, and mass is what makes
    // the drive feel the size it does — all three read straight off the body,
    // so a hull that loses cells some day reports its new self without this
    // line changing.
    let body = ship_body();
    let v = phys_vel(body);
    let w = phys_angvel(body);
    ui_text(8, 78, "SPACE burn  Q/E turn", 8);
    ui_text(8, 96, "speed " + to_int(mag(v) * fixed(10)) + " dm/s", 8);
    ui_text(8, 114, "spin " + to_int(mag(w) * fixed(100)) + " crad/s", 8);
    ui_text(8, 132, "hull mass " + to_int(phys_mass(body)), 8);
}

/// Length of a vector, octagonally approximated — the HUD wants a number to
/// read, not a square root the fixed-point library does not have.
fn mag(v) {
    let ax = absf(v.x);
    let ay = absf(v.y);
    let az = absf(v.z);
    let hi = if ax > ay { ax } else { ay };
    let lo = if ax > ay { ay } else { ax };
    let top = if hi > az { hi } else { az };
    let rest = (hi + lo + az - top) * ratio(1, 2);
    top + rest
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
        let k = dir_octant(dir.x, dir.y);
        entity_set_field(c, "face", ratio(k, 8) * tau());
        entity_set_facing(c, entity_field(c, "face"));
        // The same octant as a cell step: what this crew member is facing,
        // which is what a placement aims at when there is no cursor.
        entity_set_field(c, "fdx", fixed(octant_dx(k)));
        entity_set_field(c, "fdy", fixed(octant_dy(k)));
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
