// monada digger demo — slice D1 (docs/plans/digger-demo.md §3).
//
// A box-on-wheels on the flat apron of a volume-terrain arena: drive,
// steer, brake. The engine knows no vehicle — it gained only the
// generic volume store and the phys_* sim verbs; this script declares
// the materials, paints the floor, builds the vehicle, and maps input
// onto wheels. Feel constants live HERE (plan §1f), not in the engine.
//
// Input packing (the rpg pattern, one command per tick, verb 0):
//   arg.x = drive in −1..1, arg.y = steer in −1..1, target bit 0 = brake.
//
// All gameplay state lives in the World (decision A2): one entity
// carries the body/wheel ids and the latest input intent as fields.

// --- sim layer -------------------------------------------------------

fn init() {
    // Gravity −10, the plan §1f stance.
    phys_gravity(fixed(0), fixed(0), fixed(-10));

    // Material 0 — apron ground (what un-materialed voxel_fill paints);
    // material 1 — the vehicle hull. Hardness matters only from D3 on,
    // but the apron already declares the plan §2 bedrock value.
    let ground = phys_material(fixed(1), ratio(4, 5), fixed(0), fixed(40));
    let hull = phys_material(fixed(1), ratio(1, 2), fixed(0), fixed(100));

    // The flat apron: a 160×160 slab, two voxels thick (cells z 0..1,
    // top surface at z = 2) — runway enough for the golden's full
    // drive/arc/brake schedule. Spans many 16³ chunks, so the
    // chunk-hash cache is exercised from tick 0.
    voxel_fill(0, 0, 0, 159, 159, 1, 0x808080);

    // The vehicle: a 6×4×2 chassis on four wheels — the physics
    // test-suite stance (wheelbase ±3.5, track ±2.5, k = 240, c = 80).
    // CoM at z = 5 puts the suspension exactly at rest on the apron:
    // wheel anchors sit at world z = 4, minus rest 3/2 and radius 1/2
    // lands the contact on the z = 2 surface.
    let body = phys_box(6, 4, 2, hull, fixed(40), fixed(120), fixed(5));
    let rest = ratio(3, 2);
    let radius = ratio(1, 2);
    let k = fixed(240);
    let c = fixed(80);
    let mu = ratio(4, 5);
    // Wheel anchors in SHAPE coordinates (the engine rebases via the
    // derived CoM); +x is the vehicle's nose.
    let w_fl = phys_wheel(body, ratio(13, 2), ratio(9, 2), fixed(0), rest, radius, k, c, mu);
    let w_fr = phys_wheel(body, ratio(13, 2), ratio(-1, 2), fixed(0), rest, radius, k, c, mu);
    let w_rl = phys_wheel(body, ratio(-1, 2), ratio(9, 2), fixed(0), rest, radius, k, c, mu);
    let w_rr = phys_wheel(body, ratio(-1, 2), ratio(-1, 2), fixed(0), rest, radius, k, c, mu);

    let arch = archetype(["body", "w_fl", "w_fr", "w_rl", "w_rr", "drive", "steer", "brake"]);
    let v = entity_create(arch);
    entity_set_field(v, "body", fixed(body));
    entity_set_field(v, "w_fl", fixed(w_fl));
    entity_set_field(v, "w_fr", fixed(w_fr));
    entity_set_field(v, "w_rl", fixed(w_rl));
    entity_set_field(v, "w_rr", fixed(w_rr));
    entity_set_position(v, phys_pos(body));

    // Render-side dressing (no-ops headless).
    camera_focus(vec3(fixed(80), fixed(80), fixed(2)));
    camera_dist(fixed(60));
    set_light(vec3(fixed(1), fixed(2), fixed(-3)), ratio(9, 10));
    status("digger: W/S drive, A/D steer, Space brake");
}

// The single vehicle entity (the map spawns exactly one).
fn vehicle() {
    entities()[0]
}

fn command(player, verb, target, arg) {
    if verb == 0 {
        let v = vehicle();
        entity_set_field(v, "drive", arg.x);
        entity_set_field(v, "steer", arg.y);
        entity_set_field(v, "brake", fixed(target & 1));
    }
}

fn tick(dt) {
    let v = vehicle();
    let body = to_int(entity_field(v, "body"));
    // Feel constants: full drive = torque 25, full brake = 100, full
    // steer = 1/5 rad (3/10 drifts the stance into a bottomed-out slide).
    let drive = entity_field(v, "drive") * fixed(25);
    let steer = entity_field(v, "steer") * ratio(1, 5);
    let brake = entity_field(v, "brake") * fixed(100);
    // Front wheels steer; all four drive and brake.
    phys_wheel_input(body, to_int(entity_field(v, "w_fl")), steer, drive, brake);
    phys_wheel_input(body, to_int(entity_field(v, "w_fr")), steer, drive, brake);
    phys_wheel_input(body, to_int(entity_field(v, "w_rl")), fixed(0), drive, brake);
    phys_wheel_input(body, to_int(entity_field(v, "w_rr")), fixed(0), drive, brake);
    // Mirror the pose onto the entity: sensors, camera and the D2
    // render mirror all read the World, never the physics handle.
    entity_set_position(v, phys_pos(body));
}

// --- local layer (unsynchronized, docs/plans/input-bindings.md) ------
// Assembles the one packed input command per tick from the manifest's
// declared actions. Pure function of current input — no local state.

fn local_tick(dt) {
    let btn = 0;
    if action_down("brake") {
        btn = 1;
    }
    submit_command(
        0,
        btn,
        vec3(fixed(action_axis("drive")), fixed(action_axis("steer")), fixed(0)),
    );
}
