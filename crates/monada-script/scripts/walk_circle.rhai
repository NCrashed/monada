// walk_circle — the M0 "100 entities walk in a circle" scenario, now as
// a script (DESIGN.md §7, M2). The engine knows nothing about circles:
// the archetype, the orbit maths, and the per-tick rule all live here.
//
// All gameplay state is in the world (decision A2): each mover carries
// `angle` / `omega` / `radius` fields, so the script keeps no state of
// its own. Arithmetic is fixed-point only (`no_float` Rhai); rng comes
// from the engine's seeded, deterministic stream.

fn orbit(angle, radius) {
    vec3(cos(angle) * radius, sin(angle) * radius, fixed(0))
}

fn init() {
    let mover = archetype(["angle", "omega", "radius"]);
    for i in 0..100 {
        let e = entity_create(mover);
        // radius in [4, 12), phase in [0, tau), omega in [1/32, 1/16).
        let radius = fixed(4) + rng01() * fixed(8);
        let angle = rng01() * tau();
        let omega = ratio(1, 32) + rng01() * ratio(1, 32);
        entity_set_field(e, "angle", angle);
        entity_set_field(e, "omega", omega);
        entity_set_field(e, "radius", radius);
        entity_set_position(e, orbit(angle, radius));
    }
}

fn tick() {
    for e in entities() {
        let angle = entity_field(e, "angle") + entity_field(e, "omega");
        entity_set_field(e, "angle", angle);
        entity_set_position(e, orbit(angle, entity_field(e, "radius")));
    }
}
