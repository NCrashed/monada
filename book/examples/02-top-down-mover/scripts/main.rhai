// A top-down mover. It shows the whole input path end to end:
//
//   declared actions (manifest)
//     → the local layer reads them and submits commands
//     → the sim command handler applies them
//     → the tick integrates the result
//
// The local layer runs per client and never touches the world directly;
// the simulation only ever sees the commands it submits, so nothing here
// can desync a networked match.
//
// Small conventions this map picks (the engine assigns no meaning to them):
//   archetype 0 = the mover, archetype 1 = a click marker;
//   command verb 0 = per-tick move input, verb 1 = place a marker.

// ANCHOR: init
fn init() {
    set_light(vec3(fixed(1), fixed(1), fixed(2)), ratio(3, 4));
    // A 16x16 floor.
    voxel_fill(0, 0, 0, 15, 15, 0, 0x384868);

    // Two archetypes in declaration order: the mover (archetype 0), which
    // carries its input as two fields, then the click marker (archetype 1).
    let movers = archetype(["vx", "vy"]);
    archetype([]);

    let cube = model_box(6, 6, 8, 0xE0C040);
    let e = entity_create(movers);
    entity_set_model(e, cube);
    entity_set_position(e, vec3(fixed(8), fixed(8), fixed(1)));
}
// ANCHOR_END: init

// ANCHOR: local
// Once per simulation tick on this client: read the WASD action and forward
// it as one command (verb 0). `action_axis2` returns a Vec3 (x, y, 0) in sim
// units, so it drops straight into the command payload — no float ever
// crosses into the stream.
fn local_tick(dt) {
    let mv = action_axis2("move");
    submit_command(0, 0, mv);
}

// A discrete click. The pick API hands back a sim-space ground point,
// already fixed-point and ready to ride in the command (verb 1).
// `pick_ground` returns `()` when the cursor ray misses the world.
fn action(id, down) {
    if id == "mark" && down {
        let hit = pick_ground();
        if type_of(hit) == "Vec3" {
            submit_command(1, 0, hit);
        }
    }
}
// ANCHOR_END: local

// ANCHOR: command
// The simulation handler — the only place player input enters the world,
// and where a networked map would validate it (ownership, range, legality).
fn command(player, verb, target, arg) {
    if verb == 0 {
        // Store the move axis on the mover for the tick to integrate.
        let e = entities_of(0)[0];
        entity_set_field(e, "vx", arg.x);
        entity_set_field(e, "vy", arg.y);
    } else if verb == 1 {
        // Drop a marker at the picked cell.
        let m = entity_create(1);
        entity_set_position(m, vec3(arg.x, arg.y, fixed(1)));
    }
}
// ANCHOR_END: command

// ANCHOR: tick
// Integrate the stored intent. Speed is in cells per second, scaled by dt
// to stay rate-independent, and clamped to the floor.
fn tick(dt) {
    let e = entities_of(0)[0];
    let p = entity_position(e);
    let speed = fixed(6);
    let nx = clamp_cell(p.x + entity_field(e, "vx") * speed * dt);
    let ny = clamp_cell(p.y + entity_field(e, "vy") * speed * dt);
    entity_set_position(e, vec3(nx, ny, p.z));
}

fn clamp_cell(v) {
    if v < fixed(0) {
        return fixed(0);
    }
    if v > fixed(15) {
        return fixed(15);
    }
    v
}
// ANCHOR_END: tick
