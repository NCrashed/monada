// monada digger demo — slice D1 (docs/plans/digger-demo.md §3).
//
// A box-on-wheels on the flat apron of a volume-terrain arena: drive,
// steer, brake. The engine knows no vehicle — it gained only the
// generic volume store and the phys_* sim verbs; this script declares
// the materials, paints the floor, builds the vehicle, and maps input
// onto wheels. Feel constants live HERE (plan §1f), not in the engine.
//
// Input packing (the rpg pattern, one command per tick, verb 0):
//   arg.x = drive in −1..1, arg.y = steer in −1..1 (+1 = steer RIGHT,
//   the player's view), arg.z = drill-pitch nudge in −1..1 (+1 tilts
//   the nose UP), target bit 0 = brake, bit 1 = drill.
//
// All gameplay state lives in the World (decision A2): one entity
// carries the body/wheel ids and the latest input intent as fields.

// --- sim layer -------------------------------------------------------

fn init() {
    // Gravity −10, the plan §1f stance.
    phys_gravity(fixed(0), fixed(0), fixed(-10));

    // Material 0 — apron/bedrock ground (what un-materialed voxel_fill
    // paints); material 1 — the vehicle hull; then the mountain's
    // layers (plan §2): soft sandstone, granite veins, and the crystal
    // the chambers hide.
    let ground = phys_material(fixed(1), ratio(4, 5), fixed(0), fixed(40));
    let hull = phys_material(fixed(1), ratio(1, 2), fixed(0), fixed(100));
    // Granite is drillable-but-slow, NOT impassable: its hardness must
    // stay under the drill budget (80), or a vein becomes a softlock
    // instead of a grind.
    let sandstone = phys_material(fixed(1), ratio(4, 5), fixed(0), fixed(10));
    let granite = phys_material(fixed(1), ratio(4, 5), fixed(0), fixed(50));
    let crystal = phys_material(fixed(1), ratio(1, 2), fixed(0), fixed(25));

    // The flat apron: a 160×160 slab, two voxels thick (cells z 0..1,
    // top surface at z = 2) — runway enough for the golden's full
    // drive/arc/brake schedule. Spans many 16³ chunks, so the
    // chunk-hash cache is exercised from tick 0.
    // NB the colour's high byte is roxlap BRIGHTNESS (0xBB_RR_GG_BB) —
    // 0x00 there renders pure black. And render-side inserts NEVER
    // recolour an existing solid voxel (VoxLap span semantics: the
    // first painter owns the colour) — so the start pad goes down
    // BEFORE the slab it sits flush in.
    voxel_fill(19, 114, 1, 31, 126, 1, 0x8050_78a0);
    voxel_fill(0, 0, 0, 159, 159, 1, 0x8080_8080);
    // Corner posts: real terrain (solid, hashed, collidable) marking
    // the apron's extent.
    voxel_fill(0, 0, 0, 1, 1, 4, 0x80a0_6040);
    voxel_fill(158, 0, 0, 159, 1, 4, 0x80a0_6040);
    voxel_fill(0, 158, 0, 1, 159, 4, 0x80a0_6040);
    voxel_fill(158, 158, 0, 159, 159, 4, 0x80a0_6040);

    // The bedrock underlayer (plan §2, D3): eight cells of solid ground
    // under the whole apron — what a pitched-down drill descends
    // through. The BASEMENT vault (x 128..156, y 110..126, z −4..−1 — a
    // long undercroft below the mountain whose ceiling is the apron slab
    // itself, so a ~14° effective bore from the crystal chamber breaks
    // through within the chamber-to-vault run; steeper shafts cut slots
    // too low to enter and shallower ones never get down — the D3 trace
    // lessons; the floor sits a jump-class ~3-cell drop below the
    // breach, so the arrival lands on wheels, D4) hides inside it,
    // built paint-AROUND — five boxes, the cavity itself never painted —
    // so init carves nothing (no puff storm, no wasted rehash).
    voxel_fill(0, 0, -8, 127, 159, -1, 0x8060_5850);
    voxel_fill(157, 0, -8, 159, 159, -1, 0x8060_5850);
    voxel_fill(128, 0, -8, 156, 109, -1, 0x8060_5850);
    voxel_fill(128, 127, -8, 156, 159, -1, 0x8060_5850);
    voxel_fill(128, 110, -8, 156, 126, -5, 0x8060_5850);
    // Crystal pillars on the vault floor (decoration; the OBJECTIVE
    // crystals are entities, below).
    voxel_fill(134, 114, -4, 134, 114, -3, 0x80a0_e8f0, crystal);
    voxel_fill(143, 121, -4, 143, 120, -3, 0x80a0_e8f0, crystal);

    // Perimeter walls (D4): the sandbox has no kill-plane, and driving
    // off the world edge was an endless fall — now it's a wall. The
    // east side is walled only OUTSIDE the mountain's footprint.
    voxel_fill(0, 0, 2, 1, 159, 4, 0x80a0_6040);
    voxel_fill(0, 0, 2, 159, 1, 4, 0x80a0_6040);
    voxel_fill(0, 158, 2, 159, 159, 4, 0x80a0_6040);
    voxel_fill(158, 0, 2, 159, 95, 4, 0x80a0_6040);
    voxel_fill(158, 146, 2, 159, 157, 4, 0x80a0_6040);

    // The ramp field (plan §2, D2): stepped ramps of increasing grade.
    // The JUMP ramp sits on the golden's straight-run path (y 114..126,
    // driving +x from the pad): four 1-cell risers with 4-cell treads
    // climbing to z 5, then a hard drop at x = 71 — the launch lip.
    ramp(55, 114, 126, 4, 4);
    // Three practice ramps of steepening grade, south of the golden
    // path, climbing +x: treads 4 / 3 / 2 at grades the P3 staircase
    // rules make progressively harder run-ups.
    ramp(60, 30, 44, 4, 4);
    ramp(90, 30, 44, 3, 4);
    ramp(120, 30, 44, 2, 4);

    // The mountain (plan §2): a sandstone block against the east side of
    // the golden path, granite veins through it, and a hollow crystal
    // chamber at the core (x 130..141, y 110..121, z 2..8 — sitting
    // right above the basement vault). Paint order: veins → bulk →
    // veins again → crystal pillars; the bulk goes down as five boxes
    // AROUND the chamber, so the cavity is never filled and the
    // pillars paint into clean air.
    let msx = 120;
    let mex = 159;
    let msy = 96;
    let mey = 145;
    let mz = 15;
    // Granite veins: the two truths paint in OPPOSITE orders — the
    // volume store keeps the LAST painter (fills overwrite materials),
    // the render grid keeps the FIRST (VoxLap inserts never recolour).
    // So the veins go down twice: before the bulk (their colour) and
    // after it (their material). Both passes are two thin slabs — cheap.
    vein(msy, mey, granite);
    // Sandstone bulk around the chamber cavity (x 130..141, y 110..121,
    // z 2..8): west slab, east slab, south slab, north slab, roof.
    voxel_fill(msx, msy, 2, 129, mey, mz, 0x80c8_a860, sandstone);
    voxel_fill(142, msy, 2, mex, mey, mz, 0x80c8_a860, sandstone);
    voxel_fill(130, msy, 2, 141, 109, mz, 0x80c8_a860, sandstone);
    voxel_fill(130, 122, 2, 141, mey, mz, 0x80c8_a860, sandstone);
    voxel_fill(130, 110, 9, 141, 121, mz, 0x80c8_a860, sandstone);
    // Second vein pass: store materials (see the order note above).
    vein(msy, mey, granite);
    // Crystal pillars inside the mountain chamber.
    voxel_fill(133, 113, 2, 133, 113, 4, 0x80a0_e8f0, crystal);
    voxel_fill(137, 117, 2, 137, 116, 3, 0x80a0_e8f0, crystal);

    // The vehicle: a 6×4×2 chassis on four wheels — the physics
    // test-suite stance (wheelbase ±3.5, track ±2.5, k = 240, c = 80).
    // CoM at z = 5 puts the suspension exactly at rest on the apron:
    // wheel anchors sit at world z = 4, minus rest 3/2 and radius 1/2
    // lands the contact on the z = 2 surface.
    let body = phys_box(6, 4, 2, hull, fixed(25), fixed(120), fixed(5));
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

    // The drill nose (plan §1c): a box ahead of the chassis front face,
    // SHAPE coords like the wheel anchors. Vertically the box bottom
    // sits EXACTLY on the wheel-roll plane at ride height (CoM −2.5 =
    // the slab top, with the ride's ~0.5 bob as margin) — a level bore
    // must never nick the floor it drives on: a one-cell pothole under
    // the wheels is an unclimbable lip from standstill (the P3 lesson,
    // learned the hard way in the D3 traces).
    phys_drill_tool(
        body,
        ratio(15, 2), fixed(2), fixed(1),
        fixed(3), fixed(3), ratio(5, 2),
    );

    let arch = archetype([
        "body", "w_fl", "w_fr", "w_rl", "w_rr", "drive", "steer", "brake", "drill", "pitch_in",
        "pitch", "score", "cam", "bite", "cyaw", "found",
    ]);
    let v = entity_create(arch);
    entity_set_field(v, "body", fixed(body));
    entity_set_field(v, "w_fl", fixed(w_fl));
    entity_set_field(v, "w_fr", fixed(w_fr));
    entity_set_field(v, "w_rl", fixed(w_rl));
    entity_set_field(v, "w_rr", fixed(w_rr));
    entity_set_position(v, phys_pos(body));

    // The objective (plan §2): three crystals — ramp-top, chamber,
    // vault — as ENTITIES with proximity sensors on phys_pos (the tick
    // below), not terrain. No fail state: a sandbox with a finish line.
    let cr = archetype(["got"]);
    let cm = model_box(10, 10, 14, 0x80a0_e8f0);
    let c1 = entity_create(cr);
    entity_set_position(c1, vec3(fixed(124), fixed(37), fixed(6)));
    entity_set_model(c1, cm);
    let c2 = entity_create(cr);
    entity_set_position(c2, vec3(fixed(137), fixed(119), fixed(3)));
    entity_set_model(c2, cm);
    let c3 = entity_create(cr);
    entity_set_position(c3, vec3(fixed(150), fixed(118), fixed(-4)));
    entity_set_model(c3, cm);

    // The body mirror wears the map's own material colours (D4) —
    // matching the terrain paints, so carved faces and hull agree.
    phys_material_color(ground, 0x8060_5850);
    phys_material_color(hull, 0x8078_8290);
    phys_material_color(sandstone, 0x80c8_a860);
    phys_material_color(granite, 0x8060_6a74);
    phys_material_color(crystal, 0x80a0_e8f0);

    // Render-side dressing (no-ops headless). The engine's automatic body
    // mirror draws the vehicle (plan §1d) — the map never hand-mirrors it.
    // The chase cam takes over on the first tick; this is just the pre-tick
    // frame.
    camera_focus(phys_pos(body));
    camera_angle(pi() - phys_yaw(body), ratio(1, 2));
    camera_dist(fixed(220));
    // The view cutout stays ON for the whole map: "am I under a roof"
    // is not expressible from the sim layer (no z-threshold works —
    // the crystal chamber floor sits at surface height), and a keyhole
    // that dissolves rock between camera and vehicle is harmless in
    // the open. Focus discipline keeps the bored face safe: geometry
    // beyond the focus distance is never cut.
    camera_cutout(fixed(5), fixed(2));
    set_light(vec3(fixed(1), fixed(2), fixed(-3)), ratio(9, 10));
    status("digger: W/S drive, A/D steer, Space brake, E drill, R/F pitch");
}

// A stepped ramp climbing +x from `x0`, spanning `y0..=y1`: `steps`
// 1-cell risers, each `tread` cells long, on top of the z 0..1 slab.
// Step k (1-based) is solid at z 2..=(1 + k) — a staircase the
// suspension can climb with a run-up (the P3 lesson).
fn ramp(x0, y0, y1, tread, steps) {
    let k = 1;
    while k <= steps {
        let sx = x0 + (k - 1) * tread;
        voxel_fill(sx, y0, 2, sx + tread - 1, y1, 1 + k, 0x8070_5838);
        k += 1;
    }
}

// Two thin vertical granite slabs flanking the mountain's chamber
// column (x 126..127 west of it, 146..147 east — never inside the
// cavity), spanning the mountain's y and most of its height.
fn vein(y0, y1, mat) {
    voxel_fill(126, y0, 2, 127, y1, 12, 0x8060_6a74, mat);
    voxel_fill(146, y0, 2, 147, y1, 12, 0x8060_6a74, mat);
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
        entity_set_field(v, "pitch_in", arg.z);
        entity_set_field(v, "brake", fixed(target & 1));
        entity_set_field(v, "drill", fixed((target >> 1) & 1));
    }
}

fn tick(dt) {
    let v = vehicle();
    let body = to_int(entity_field(v, "body"));
    // Feel constants: full drive = torque 25, full brake = 100, full
    // steer = 1/5 rad (3/10 drifts the stance into a bottomed-out slide).
    // Steer is NEGATED: input +1 means "screen right", and with the
    // chase cam behind the body a positive physics yaw (+z, toward sim
    // +y) reads as a LEFT turn on screen.
    let drive = entity_field(v, "drive") * fixed(25);
    let steer = entity_field(v, "steer") * ratio(-1, 5);
    let brake = entity_field(v, "brake") * fixed(100);
    // Front wheels steer; all four drive and brake.
    phys_wheel_input(body, to_int(entity_field(v, "w_fl")), steer, drive, brake);
    phys_wheel_input(body, to_int(entity_field(v, "w_fr")), steer, drive, brake);
    phys_wheel_input(body, to_int(entity_field(v, "w_rl")), fixed(0), drive, brake);
    phys_wheel_input(body, to_int(entity_field(v, "w_rr")), fixed(0), drive, brake);
    // Drill pitch (plan §1e): the nudge axis integrates into a held
    // pitch, clamped to ±45° (pi/4). +1 nudges the nose UP at 45°/s.
    let pitch = entity_field(v, "pitch") + entity_field(v, "pitch_in") * pi() * dt / fixed(4);
    let lim = pi() / fixed(4);
    if pitch > lim {
        pitch = lim;
    }
    if pitch < -lim {
        pitch = -lim;
    }
    entity_set_field(v, "pitch", pitch);
    // The drill (plan §1c): one engine sweep per held tick. Budget 80:
    // the reaction of a full cut (80) stays under the drive train's
    // push (4×25 torque ≈ 100-force), so boring never out-brakes the
    // wheels — 8 sandstone / 2 bedrock / 1 granite cells per tick.
    // The bore is GRAVITY-STABLE: subtracting the chassis attitude
    // keeps the cut on the commanded grade even when ramming the face
    // noses the body up — otherwise the tunnel ratchets upward and the
    // vehicle beaches on its own steps (the D3 trace lesson #2).
    // `bite` = this tick's cut count, the plan §3 D4 feedback: the HUD
    // reads it as a crude "how hard is this rock" indicator.
    let bite = 0;
    if entity_field(v, "drill") > fixed(0) {
        bite = phys_drill(body, pitch - phys_pitch(body), fixed(80));
        entity_set_field(v, "score", entity_field(v, "score") + fixed(bite));
    }
    entity_set_field(v, "bite", fixed(bite));
    // Mirror the pose onto the entity (sensors and game logic read the
    // World, never the physics handle).
    let p0 = phys_pos(body);
    entity_set_position(v, p0);
    // Crystal sensors (plan §2): touch = proximity of the body's CoM,
    // radius 4. A touched crystal despawns (its sprite with it) and the
    // persistent counter lives on the vehicle entity ("found") — all
    // state in the World, decision A2.
    let found = entity_field(v, "found");
    for c in entities_of(1) {
        let cp = entity_position(c);
        let dx = cp.x - p0.x;
        let dy = cp.y - p0.y;
        let dz = cp.z - p0.z;
        if dx * dx + dy * dy + dz * dz < fixed(16) {
            entity_despawn(c);
            // A bright pickup chime (synthesised — no asset).
            play_blip(3, 900, 150, ratio(1, 2));
            found += fixed(1);
        }
    }
    entity_set_field(v, "found", found);
    // HUD line: objective counter + the drill bite (render-side).
    status("crystals " + to_int(found) + "/3 | bite " + bite + " | W/S drive, A/D steer, Space brake, E drill, R/F pitch");
    // Chase cam (plan §1e): yaw tracks the heading (world yaw = pi −
    // sim yaw under the render mirror), pitch fixed. On the surface the
    // focus aims AHEAD so the road fills the view; underground / while
    // drilling the cam pulls in, flattens, and focuses ON the body —
    // the view cutout then dissolves only what stands between camera
    // and vehicle, never the face ahead (geometry beyond the focus
    // distance is untouched). camera_dist is set ONLY when the mode
    // flips: the host's mouse-wheel zoom adjusts the same distance, and
    // hammering it every tick would eat the player's zoom.
    // Smooth the chase yaw (D4): wrap the difference through atan2 so
    // ±pi never snaps, then close a sixth of the gap per tick. The
    // smoothed value is World state ("cyaw") — deterministic, like the
    // pose mirror; the camera it drives stays render-side.
    let yaw = phys_yaw(body);
    let cyaw = entity_field(v, "cyaw");
    cyaw += atan2(sin(yaw - cyaw), cos(yaw - cyaw)) / fixed(6);
    entity_set_field(v, "cyaw", cyaw);
    let p = phys_pos(body);
    let underground = entity_field(v, "drill") > fixed(0) || p.z < fixed(3);
    let mode = if underground { fixed(1) } else { fixed(0) };
    if mode != entity_field(v, "cam") {
        entity_set_field(v, "cam", mode);
        if underground {
            camera_dist(fixed(140));
        } else {
            camera_dist(fixed(220));
        }
    }
    if underground {
        camera_focus(p);
        camera_angle(pi() - cyaw, ratio(1, 5));
    } else {
        camera_focus(vec3(
            p.x + cos(cyaw) * fixed(10),
            p.y + sin(cyaw) * fixed(10),
            p.z,
        ));
        camera_angle(pi() - cyaw, ratio(1, 2));
    }
}

// --- local layer (unsynchronized, docs/plans/input-bindings.md) ------
// Assembles the one packed input command per tick from the manifest's
// declared actions. Pure function of current input — no local state.

fn local_tick(dt) {
    let btn = 0;
    if action_down("brake") {
        btn = 1;
    }
    if action_down("drill") {
        btn += 2;
    }
    submit_command(
        0,
        btn,
        vec3(
            fixed(action_axis("drive")),
            fixed(action_axis("steer")),
            fixed(action_axis("drill_pitch")),
        ),
    );
}
