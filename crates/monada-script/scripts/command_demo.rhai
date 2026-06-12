// command_demo — the M3 command-driven scenario (DESIGN.md §3.1, §7).
//
// Unlike walk_circle (fully autonomous), this map reacts to player
// *commands* delivered over the lockstep wire. The engine ships no
// genre: it forwards each command's opaque fields to `command(...)`
// below, and this script decides what they mean.
//
// Command shape (monada_sim::Command): (player, verb, target, arg)
//   verb 1  spawn  — create a unit at `arg`, at rest
//   verb 2  steer  — set unit `target`'s velocity to (arg.x, arg.y)
// `arg` is a Vec3 (fixed-point). All state lives in the world (A2):
// each unit carries `vx`/`vy` velocity fields; position is integrated
// every tick. Arithmetic is fixed-point only (`no_float` Rhai).

fn init() {
    // One archetype: a unit with a 2D velocity. It is the first (and
    // only) archetype registered, so its id is 0 — what `command` spawns
    // into below. Bind the id so `init` returns unit, not the i64.
    let unit = archetype(["vx", "vy"]);
}

fn command(player, verb, target, arg) {
    if verb == 1 {
        // spawn: a new unit at the commanded point, initially at rest.
        let e = entity_create(0);
        entity_set_position(e, arg);
        entity_set_field(e, "vx", fixed(0));
        entity_set_field(e, "vy", fixed(0));
    } else if verb == 2 {
        // steer: set the target unit's velocity from the command vector.
        entity_set_field(target, "vx", arg.x);
        entity_set_field(target, "vy", arg.y);
    }
}

fn tick() {
    // Integrate every unit's position by its velocity (Euler step).
    for e in entities() {
        let p = entity_position(e);
        let vx = entity_field(e, "vx");
        let vy = entity_field(e, "vy");
        entity_set_position(e, vec3(p.x + vx, p.y + vy, p.z));
    }
}
