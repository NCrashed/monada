// Hello, voxels — the smallest complete monada map.
//
// The host calls `init` once, then `tick(dt)` every simulation step. All
// gameplay state lives in the shared World (entities and their fields);
// the script only reads and writes it through the host API. Coordinates
// are sim cells: x/y across the floor, z up.

// ANCHOR: init
fn init() {
    // A directional "sun" so the floor and the cube read with some depth.
    // `dir` is the direction the light travels; the second arg is its
    // strength. Render-side only — it never touches the simulation.
    set_light(vec3(fixed(1), fixed(1), fixed(2)), ratio(3, 4));

    // Paint an 8x8 floor, one voxel thick, at ground level (z = 0).
    // Two corners plus a packed 0xRRGGBB colour.
    voxel_fill(0, 0, 0, 7, 7, 0, 0x4030A0);

    // Declare an archetype (no fields here), build a box sprite model,
    // and bind it to one fresh entity sitting one cell above the floor.
    let walkers = archetype([]);
    let cube = model_box(6, 6, 6, 0xE0C020);
    let e = entity_create(walkers);
    entity_set_model(e, cube);
    entity_set_position(e, vec3(fixed(0), fixed(0), fixed(1)));
}
// ANCHOR_END: init

// ANCHOR: tick
// Walk the cube along +x at one cell per second, wrapping at the far edge.
// `dt` is the tick duration (1/30 s) as a fixed-point number — advancing
// from it, rather than a per-tick constant, keeps motion rate-independent.
fn tick(dt) {
    let e = entities()[0];
    let p = entity_position(e);
    let nx = p.x + dt;
    if nx > fixed(7) {
        nx = fixed(0);
    }
    entity_set_position(e, vec3(nx, p.y, p.z));
}
// ANCHOR_END: tick
