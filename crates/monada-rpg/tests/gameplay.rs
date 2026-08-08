//! The action-RPG demo map's gameplay as a headless canary: load the rules from the
//! packed `map/` archive under a [`NullBridge`] (so `voxel_fill` paints
//! collidable terrain and `voxel_solid`/`ground_height` answer, while render
//! calls — actors, anim, camera — are no-ops), then drive the real-time
//! input + tick path and assert world state. The seed of the M-E oracle
//! golden; mirrors monada-chess's `rules.rs`.

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_script::{
    shared_world, RhaiBackend, ScriptBackend, SharedBridge, SharedWorld, NullBridge,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const PLAYER: ArchetypeId = ArchetypeId(0);
const ENEMY: ArchetypeId = ArchetypeId(1);
const GAME: ArchetypeId = ArchetypeId(2);
const SUMMON: ArchetypeId = ArchetypeId(4); // pending-spawn telegraph
const VERB_INPUT: u32 = 0;
const P0: PlayerId = PlayerId(0);
const BTN_ATTACK: u64 = 1;
const BTN_DODGE: u64 = 2;

/// The action-RPG demo rules, through the real archive path (pack `map/`, read back,
/// take the entry script).
fn script() -> String {
    let map_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack rpg map");
    let map = monada_format::Map::read(&bytes).expect("read rpg map");
    map.entry_script()
        .expect("rpg map has an entry script")
        .to_string()
}

fn fresh() -> (SharedWorld, RhaiBackend) {
    let world = shared_world(SEED);
    let mut backend = RhaiBackend::new(world.clone());
    // NullBridge: `init`'s voxel paints fill a real collision store; the
    // actor / anim / camera calls are no-ops headlessly.
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    backend.load(&script()).expect("compile main.rhai");
    backend.on_init().expect("init runs");
    (world, backend)
}

/// One real-time input command: move axis + button bitmask.
fn input(mx: i32, my: i32, buttons: u64) -> Command {
    Command::on(
        VERB_INPUT,
        EntityId(buttons),
        FixedVec3::new(Fixed::from_int(mx), Fixed::from_int(my), Fixed::ZERO),
    )
}

fn step(b: &mut RhaiBackend, cmd: &Command) {
    b.on_command(P0, cmd).expect("input command");
    b.on_tick().expect("tick");
}

/// Dismiss the intro cutscene: two attack presses jump the typewriter to the
/// end and launch wave 1 (see `advance_dialogue`). Also spawns P0's hero.
fn skip_intro(b: &mut RhaiBackend) {
    step(b, &input(0, 0, 1)); // attack bit skips the typewriter
    step(b, &input(0, 0, 1)); // ... then dismisses the full line → wave 1
}

fn count(world: &SharedWorld, arch: ArchetypeId) -> usize {
    world.lock().unwrap().count(arch)
}

fn field(world: &SharedWorld, arch: ArchetypeId, idx: usize, name: &str) -> f64 {
    let w = world.lock().unwrap();
    let e = w.entities(arch)[idx];
    w.field(e, name).unwrap().to_f64()
}

fn player_pos(world: &SharedWorld) -> FixedVec3 {
    let w = world.lock().unwrap();
    let p = w.entities(PLAYER)[0];
    w.position(p).unwrap()
}

#[test]
fn dead_enemy_lingers_before_despawn() {
    let (world, mut b) = fresh();
    skip_intro(&mut b); // past the intro → wave 1 telegraphed, hero spawned
                        // Wait out the summon telegraph so wave-1 enemies materialise.
    for _ in 0..50 {
        step(&mut b, &input(0, 0, 0));
    }
    let n0 = count(&world, ENEMY);
    assert!(n0 >= 2, "wave enemies present");

    // Kill one enemy directly (bypasses aiming/positioning).
    let victim = {
        let mut w = world.lock().unwrap();
        let e = w.entities(ENEMY)[0];
        w.set_field(e, "hp", Fixed::ZERO);
        e
    };

    // One tick later it must still exist, playing its death anim — not gone.
    step(&mut b, &input(0, 0, 0));
    assert!(
        world.lock().unwrap().position(victim).is_some(),
        "dead enemy lingers for its death animation, not removed at once"
    );

    // After the death window (~death_ticks) it despawns.
    for _ in 0..60 {
        step(&mut b, &input(0, 0, 0));
    }
    assert!(
        world.lock().unwrap().position(victim).is_none(),
        "dead enemy despawns after the death window"
    );
}

#[test]
fn obstacles_block_the_hero() {
    let (world, mut b) = fresh();
    skip_intro(&mut b); // past the intro so the hero can move
                        // Movement is camera-relative (yaw 0.8): the `(-1,-1)` input rotates to a
                        // near-straight sim south (−y), driving the hero down its spawn column
                        // (x≈7) into the pillar at cells x∈{6,7}, y∈{6,7}. It must stop above the
                        // pillar (y ≈ 8), never passing through to the far side (y < 6).
    for _ in 0..120 {
        step(&mut b, &input(-1, -1, 0));
    }
    let y = player_pos(&world).y.to_f64();
    assert!(y > 7.0, "hero blocked by the pillar (stopped at y={y})");
}

#[test]
fn setup_spawns_arena_and_first_wave() {
    let (world, mut b) = fresh();
    // Fresh = the intro cutscene: no hero, no wave, gameplay frozen.
    assert_eq!(count(&world, PLAYER), 0, "no hero until a player acts");
    assert_eq!(count(&world, SUMMON), 0, "no wave during the intro");
    assert!(
        field(&world, GAME, 0, "started") < 0.5,
        "still in the intro"
    );

    // Dismiss the intro → wave 1 TELEGRAPHED as summons; enemies materialise
    // after the ~1.5s window (so nothing spawns in the player's face).
    skip_intro(&mut b);
    assert!(field(&world, GAME, 0, "started") > 0.5, "intro dismissed");
    assert_eq!(
        count(&world, SUMMON),
        4,
        "wave 1 = round(1.2 * (2 + wave)) = 4 summons"
    );
    assert_eq!(count(&world, ENEMY), 0, "enemies haven't materialised yet");
    assert!(
        field(&world, GAME, 0, "wave") > 0.5,
        "wave director at wave 1"
    );
    assert_eq!(count(&world, PLAYER), 1, "hero spawned from the skip input");
    assert!(
        field(&world, PLAYER, 0, "hp") > 99.0,
        "hero starts at full hp"
    );

    // After the telegraph the summons become enemies.
    for _ in 0..50 {
        step(&mut b, &input(0, 0, 0));
    }
    assert_eq!(
        count(&world, ENEMY),
        4,
        "the 4 summons materialised into enemies"
    );
    assert_eq!(count(&world, SUMMON), 0, "telegraphs cleared");
}

#[test]
fn co_op_spawns_a_hero_per_player() {
    let (world, mut b) = fresh();
    // Two peers act this tick — each gets its own hero (no PvP, shared waves).
    b.on_command(P0, &input(0, 0, 0)).expect("p0 input");
    b.on_command(PlayerId(1), &input(0, 0, 0))
        .expect("p1 input");
    b.on_tick().expect("tick");
    assert_eq!(count(&world, PLAYER), 2, "one hero per player");
}

#[test]
fn input_moves_the_hero() {
    let (world, mut b) = fresh();
    skip_intro(&mut b); // past the intro; hero spawned
    let start = player_pos(&world).x.to_f64();
    // Walk east for a while.
    for _ in 0..20 {
        step(&mut b, &input(1, 0, 0));
    }
    let end = player_pos(&world).x.to_f64();
    assert!(end > start + 1.0, "hero moved east ({start} → {end})");
    assert!(end < 19.0, "hero stayed inside the arena wall");
}

#[test]
fn enemies_chase_and_wound_the_hero() {
    let (world, mut b) = fresh();
    skip_intro(&mut b); // past the intro → wave 1
                        // Stand still; the four enemies converge and start hitting.
    for _ in 0..240 {
        step(&mut b, &input(0, 0, 0));
    }
    assert!(
        field(&world, PLAYER, 0, "hp") < 100.0,
        "stationary hero took damage from chasing enemies"
    );
}

#[test]
fn two_runs_agree_on_an_input_stream() {
    // A varied scripted input stream (orbit + periodic attack / dodge) must
    // fold to identical state on two independent runs — the determinism
    // contract for continuous real-time input.
    let inputs: Vec<Command> = (0..300u64)
        .map(|t| {
            let (mx, my) = match t % 4 {
                0 => (1, 0),
                1 => (0, 1),
                2 => (-1, 0),
                _ => (0, -1),
            };
            let mut btn = 0;
            if t % 7 == 0 {
                btn |= BTN_ATTACK;
            }
            if t % 23 == 0 {
                btn |= BTN_DODGE;
            }
            input(mx, my, btn)
        })
        .collect();

    let (wa, mut a) = fresh();
    let (wb, mut b) = fresh();
    for (t, cmd) in inputs.iter().enumerate() {
        step(&mut a, cmd);
        step(&mut b, cmd);
        let ha = wa.lock().unwrap().state_hash();
        let hb = wb.lock().unwrap().state_hash();
        assert_eq!(ha, hb, "runs diverged at tick {t}");
    }
}
