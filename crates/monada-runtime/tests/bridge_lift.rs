//! What a NATIVE map can reach (`host_api` 27).
//!
//! The tileset, actor and audio verbs have existed on [`HostBridge`] since
//! the Rhai days, but were never lifted into `WorldRead` / `Host` when
//! `monada-runtime` was split out of `monada-script`. A compiled map could
//! therefore paint a tileset or animate a billboard only by locking
//! `bridge()` and reaching round the typed surface the split exists to
//! provide — while a Rhai-scripted map did both freely. "The runtime is
//! swappable" was untrue in exactly the direction that mattered.
//!
//! These tests are about that promise rather than about the verbs: a map
//! written against the traits alone reaches the host, and the one verb
//! that also feeds collision keeps the eye and the pathfinder agreeing.

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_runtime::{
    shared_terrain, shared_world, Host, HostBridge, RuntimeHost, SharedBridge, WorldRead,
};

/// The log, held outside the bridge so a test can read it without
/// downcasting a `dyn HostBridge`.
type Log = Arc<Mutex<Vec<String>>>;

/// A bridge that writes down what it was told, and nothing else.
struct Recorder(Log);

impl Recorder {
    fn note(&mut self, what: String) {
        self.0.lock().expect("log mutex").push(what);
    }
}

impl HostBridge for Recorder {
    // The handful `HostBridge` leaves without a default.
    fn model_box(&mut self, _w: i64, _h: i64, _d: i64, _color: i64) -> i64 {
        0
    }
    #[allow(clippy::too_many_arguments)]
    fn model_box_sides(
        &mut self,
        _w: i64,
        _h: i64,
        _d: i64,
        _x: i64,
        _neg_x: i64,
        _y: i64,
        _neg_y: i64,
        _z: i64,
        _neg_z: i64,
    ) -> i64 {
        0
    }
    fn model_kv6(&mut self, _asset_path: &str, _turns: i64) -> i64 {
        0
    }
    fn entity_set_model(&mut self, _entity: i64, _model: i64) {}
    #[allow(clippy::too_many_arguments)]
    fn voxel_fill(&mut self, _: i64, _: i64, _: i64, _: i64, _: i64, _: i64, _: i64) {}
    fn voxel_set(&mut self, _x: i64, _y: i64, _z: i64, _color: i64) {}
    fn highlight(&mut self, _entity: i64) {}
    fn highlight_clear(&mut self) {}
    fn highlighted(&self) -> i64 {
        -1
    }
    fn status(&mut self, _text: &str) {}
    fn camera_focus(&mut self, _point: FixedVec3) {}
    fn camera_angle(&mut self, _yaw: Fixed, _pitch: Fixed) {}
    fn submit_command(&mut self, _verb: i64, _target: i64, _arg: FixedVec3) {}
    fn local_player(&self) -> Option<i64> {
        Some(0)
    }
    fn set_light(&mut self, _dir: FixedVec3, _intensity: Fixed) {}
    fn set_sky(&mut self, _asset_path: &str) {}

    // The lifted ones, recorded.
    fn model_actor(&mut self, dir_path: &str, states: &[String], _height: Fixed) -> i64 {
        self.note(format!("model_actor {dir_path} [{}]", states.join(",")));
        7
    }
    fn model_character(&mut self, asset_path: &str, _height: Fixed) -> i64 {
        self.note(format!("model_character {asset_path}"));
        8
    }
    fn model_drop(&mut self, model: i64, _cells: Fixed) {
        self.note(format!("model_drop {model}"));
    }
    fn entity_set_anim(&mut self, entity: i64, state: &str) {
        self.note(format!("entity_set_anim {entity} {state}"));
    }
    fn entity_set_tint(&mut self, entity: i64, tint: i64) {
        self.note(format!("entity_set_tint {entity} {tint:#x}"));
    }
    fn tile(&mut self, asset_path: &str) -> i64 {
        self.note(format!("tile {asset_path}"));
        3
    }
    #[allow(clippy::too_many_arguments)]
    fn tile_fill(&mut self, x0: i64, y0: i64, z0: i64, x1: i64, y1: i64, z1: i64, tile: i64) {
        self.note(format!("tile_fill {x0},{y0},{z0}..{x1},{y1},{z1} #{tile}"));
    }
    fn transition(&mut self, low: i64, high: i64, asset_path: &str) {
        self.note(format!("transition {low}->{high} {asset_path}"));
    }
    fn terrain_fill(&mut self, x0: i64, y0: i64, x1: i64, y1: i64, type_id: i64) {
        self.note(format!("terrain_fill {x0},{y0}..{x1},{y1} ={type_id}"));
    }
    fn terrain_blit(&mut self, base_type: i64) {
        self.note(format!("terrain_blit {base_type}"));
    }
    fn play_sound(&mut self, asset_path: &str) {
        self.note(format!("play_sound {asset_path}"));
    }
    fn play_loop(&mut self, asset_path: &str) {
        self.note(format!("play_loop {asset_path}"));
    }
    fn play_music(&mut self, asset_path: &str) {
        self.note(format!("play_music {asset_path}"));
    }
    fn stop_music(&mut self) {
        self.note("stop_music".to_string());
    }
    fn ui_texture(&mut self, asset_path: &str) -> i64 {
        self.note(format!("ui_texture {asset_path}"));
        4
    }
    fn ui_image(&mut self, tex: i64, x: i64, y: i64) {
        self.note(format!("ui_image {tex} @{x},{y}"));
    }
    fn ui_pin(&mut self, at: FixedVec3) {
        self.note(format!("ui_pin {},{},{}", at.x, at.y, at.z));
    }
    fn ui_text_wrap(&mut self, x: i64, y: i64, text: &str, _s: i64, _w: i64, _c: i64) {
        self.note(format!("ui_text_wrap {x},{y} {text}"));
    }
}

fn wired() -> (RuntimeHost, Log) {
    let log: Log = Arc::default();
    let bridge: SharedBridge = Arc::new(Mutex::new(Recorder(log.clone())));
    let mut host = RuntimeHost::new(shared_world(0));
    host.set_bridge(&bridge);
    // The host holds its own clone of the bridge, so dropping ours is fine.
    (host, log)
}

/// A native map reaches every lifted verb through the traits alone — no
/// `bridge()`, no `Mutex`, no `HostBridge` in sight. That this function
/// compiles is most of the point; that the calls arrive is the rest.
#[test]
fn a_native_map_paints_a_tileset_and_animates_an_actor() {
    let (host, log) = wired();

    // The tileset half of a Warcraft-III-shaped map.
    let grass = host.tile("assets/tiles/grass.png");
    assert_eq!(grass, 3, "tile ids come back through the typed surface");
    host.transition(0, 1, "assets/tiles/grass-dirt.png");
    host.terrain_fill((0, 0), (16, 16), 1);
    host.terrain_blit(1);
    host.tile_fill((0, 0, 0), (16, 16, 0), grass);

    // The billboard half.
    let hero = host.model_actor(
        "assets/char/hero",
        &["idle", "run"],
        Fixed::from_ratio(14, 10),
    );
    assert_eq!(hero, 7);
    host.model_drop(hero, Fixed::from_ratio(1, 10));
    assert_eq!(
        host.model_character("assets/char/boss.rkc", Fixed::from_int(2)),
        8
    );

    let archetype = host.archetype(&["hp"]);
    let e = host.entity_create(archetype);
    host.entity_set_anim(e, "run");
    host.entity_set_tint(e, 0x00FF_4040);

    // Sound and HUD.
    host.play_loop("assets/sounds/run.mp3");
    host.play_sound("assets/sounds/hit.mp3");
    host.play_music("assets/music/field.ogg");
    host.stop_music();
    let panel = host.ui_texture("assets/ui/panel.png");
    host.ui_pin(FixedVec3::new(Fixed::from(3), Fixed::from(4), Fixed::from(5)));
    host.ui_image(panel, 8, 8);
    host.ui_text_wrap(8, 40, "the dead are those you talk to", 12, 200, 0x00FF_FFFF);

    assert_eq!(
        log.lock().expect("log mutex").as_slice(),
        [
            "tile assets/tiles/grass.png",
            "transition 0->1 assets/tiles/grass-dirt.png",
            "terrain_fill 0,0..16,16 =1",
            "terrain_blit 1",
            "tile_fill 0,0,0..16,16,0 #3",
            "model_actor assets/char/hero [idle,run]",
            "model_drop 7",
            "model_character assets/char/boss.rkc",
            &format!("entity_set_anim {} run", e.0),
            &format!("entity_set_tint {} 0xff4040", e.0),
            "play_loop assets/sounds/run.mp3",
            "play_sound assets/sounds/hit.mp3",
            "play_music assets/music/field.ogg",
            "stop_music",
            "ui_texture assets/ui/panel.png",
            "ui_pin 3,4,5",
            "ui_image 4 @8,8",
            "ui_text_wrap 8,40 the dead are those you talk to",
        ]
        .map(String::from),
    );
}

/// Every lifted verb is a no-op without a bridge, which is what lets a
/// headless peer and the oracle run the identical rules and draw nothing.
/// Without this, a map that paints its terrain in `init` would panic the
/// oracle rather than render nowhere.
#[test]
fn the_same_map_runs_headless() {
    let host = RuntimeHost::new(shared_world(0));
    assert_eq!(host.tile("assets/tiles/grass.png"), -1);
    assert_eq!(host.model_actor("assets/char/hero", &["idle"], Fixed::ONE), -1);
    assert_eq!(host.ui_texture("assets/ui/panel.png"), -1);
    host.terrain_blit(1);
    host.play_sound("assets/sounds/hit.mp3");
    host.stop_music();
}

/// What a map's ground reaches DOWN to.
///
/// Everything that assumed the datum was the floor mistreated a map that
/// digs: the fog's deck band stopped at sim z 0, so a hollow fell off
/// every deck and the classifier painted it opaque black — ground you
/// could walk into and not see.
#[test]
fn the_store_knows_how_deep_its_ground_goes() {
    let terrain = shared_terrain();
    let host = RuntimeHost::with_terrain(shared_world(0), &terrain);

    host.voxel_fill((0, 0, 0), (8, 8, 0), 0);
    assert_eq!(terrain.lock().expect("terrain").lowest(), 0, "flat is flat");

    // A hollow, on ground not yet painted: the column's top is BELOW the
    // datum, and that is what the fog's band has to reach.
    host.tile_relief(20, 20, -9, -9, &[], -1);
    assert_eq!(terrain.lock().expect("terrain").lowest(), -9);

    // …and a plateau does not move the floor.
    host.voxel_fill((6, 6, 0), (7, 7, 24), 0);
    assert_eq!(terrain.lock().expect("terrain").lowest(), -9);
}

/// The store RAISES; it never lowers.
///
/// `fill` keeps the higher of what is there and what is asked for, so
/// painting a hollow over ground already painted higher does nothing. A
/// map paints each cell once and never notices; an editor re-painting a
/// lowered cell would, and `voxel_clear` is what it wants instead. Pinned
/// because the failure is silent — the ground simply does not move.
#[test]
fn filling_a_column_raises_it_and_never_lowers_it() {
    let terrain = shared_terrain();
    let host = RuntimeHost::with_terrain(shared_world(0), &terrain);

    host.voxel_fill((3, 3, 0), (3, 3, 12), 0);
    assert_eq!(host.ground_height(3, 3), 12);

    host.voxel_fill((3, 3, 0), (3, 3, 4), 0);
    assert_eq!(host.ground_height(3, 3), 12, "a lower fill did not lower it");

    host.voxel_clear(3, 3, 5);
    host.voxel_fill((3, 3, 0), (3, 3, 4), 0);
    assert_eq!(host.ground_height(3, 3), 4, "clearing first is what lowers");
}

/// The one lifted verb that is not render-only.
///
/// `tile_fill` paints a wall the pathfinder must also see. If it only
/// reached the bridge, the eye and `nav_path` would disagree about solid
/// ground — the exact drift `voxel_fill`'s store-then-bridge ordering
/// exists to prevent, and the reason this verb sits on `Host` rather than
/// beside `tile` on `WorldRead`.
#[test]
fn a_tiled_wall_blocks_as_well_as_a_painted_one() {
    let terrain = shared_terrain();
    let log: Log = Arc::default();
    let bridge: SharedBridge = Arc::new(Mutex::new(Recorder(log)));
    let mut host = RuntimeHost::with_terrain(shared_world(0), &terrain);
    host.set_bridge(&bridge);

    host.tile_fill((0, 0, 0), (8, 8, 0), 3); // a floor
    host.tile_fill((4, 0, 1), (4, 8, 5), 3); // a wall across it

    assert_eq!(host.ground_height(2, 2), 0, "floor is floor");
    assert_eq!(
        host.ground_height(4, 4),
        5,
        "the tiled wall stands in the store, not only on screen",
    );
    assert!(
        host.voxel_solid(4, 4, 3),
        "a textured wall is solid to collision, not just to the eye",
    );
}
