//! Local-layer parity — the other half of the D-0 gate.
//!
//! `native_parity.rs` proves the two runtimes *compute* the same game.
//! This proves they are *driven* the same way: the click gesture that
//! turns two taps into a `MOVE` command runs once as the script's
//! `pointer` handler and once as [`ChessLocal`], over a bridge that
//! records everything the client emits — submitted commands, selection
//! changes, status lines. A local layer cannot desync a match (it never
//! touches hashed state), so the comparison is behavioural rather than
//! hash-based: the same clicks must produce the same commands, or the
//! game is not the same game.
//!
//! The recording bridge doubles as the harness every future local-layer
//! test wants, which is why it answers `highlighted()` for real instead
//! of stubbing it: chess's whole gesture is a selection state machine
//! read back out of the host.

use std::sync::{Arc, Mutex};

use monada_chess::{ChessLocal, ChessRules};
use monada_fixed::{Fixed, FixedVec3};
use monada_runtime::{HostBridge, NativeBackend, NativeLocalBackend, ScriptBackend};
use monada_script::{shared_world, LocalBackend, SharedBridge, SharedWorld};
use monada_sim::EntityId;

const SEED: u64 = 0x4D4F_4E41_4441_5F30;

/// What a client did, in order — the thing the two local layers must
/// agree on.
#[derive(Debug, Default, PartialEq, Eq)]
struct Emitted {
    commands: Vec<(i64, i64, (i32, i32))>,
    selection: Vec<i64>,
    status: Vec<String>,
}

/// A [`HostBridge`] that records the local layer's output and answers the
/// selection query for real.
#[derive(Default)]
struct Recorder {
    out: Emitted,
    selected: Option<i64>,
    /// The client this bridge belongs to; `None` = hotseat.
    player: Option<i64>,
}

impl HostBridge for Recorder {
    fn model_box(&mut self, _w: i64, _h: i64, _d: i64, _color: i64) -> i64 {
        -1
    }
    fn model_kv6(&mut self, _asset_path: &str, _turns: i64) -> i64 {
        -1
    }
    fn entity_set_model(&mut self, _entity: i64, _model: i64) {}
    fn voxel_fill(&mut self, _x0: i64, _y0: i64, _z0: i64, _x1: i64, _y1: i64, _z1: i64, _c: i64) {}
    fn voxel_set(&mut self, _x: i64, _y: i64, _z: i64, _color: i64) {}
    fn highlight(&mut self, entity: i64) {
        self.selected = Some(entity);
        self.out.selection.push(entity);
    }
    fn highlight_clear(&mut self) {
        self.selected = None;
        self.out.selection.push(-1);
    }
    fn highlighted(&self) -> i64 {
        self.selected.unwrap_or(-1)
    }
    fn status(&mut self, text: &str) {
        self.out.status.push(text.to_string());
    }
    fn camera_focus(&mut self, _point: FixedVec3) {}
    fn camera_angle(&mut self, _yaw: Fixed, _pitch: Fixed) {}
    fn submit_command(&mut self, verb: i64, target: i64, arg: FixedVec3) {
        self.out
            .commands
            .push((verb, target, (arg.x.floor_to_int(), arg.y.floor_to_int())));
    }
    fn local_player(&self) -> Option<i64> {
        self.player
    }
    fn set_light(&mut self, _dir: FixedVec3, _intensity: Fixed) {}
    fn set_sky(&mut self, _asset_path: &str) {}
}

/// A shared recorder plus the bridge handle the layers talk through.
fn recorder(player: Option<i64>) -> (Arc<Mutex<Recorder>>, SharedBridge) {
    let rec = Arc::new(Mutex::new(Recorder {
        player,
        ..Recorder::default()
    }));
    let bridge: SharedBridge = rec.clone();
    (rec, bridge)
}

fn chess_script() -> String {
    let map_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack chess map");
    let map = monada_format::Map::read(&bytes).expect("read chess map");
    map.entry_script()
        .expect("chess map has an entry script")
        .to_string()
}

fn square(x: i32, y: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO)
}

/// A world with chess set up, via the native rules (proven identical to
/// the script's in `native_parity.rs`).
fn started_world() -> SharedWorld {
    let world = shared_world(SEED);
    let bridge: SharedBridge = Arc::new(Mutex::new(monada_runtime::NullBridge));
    let mut sim = NativeBackend::new(world.clone(), Box::new(ChessRules::new()));
    sim.set_bridge(&bridge);
    sim.on_init().expect("init");
    world
}

/// Clicks as board squares. The `entity` the host would have picked is
/// irrelevant to chess's gesture (it reads the square), so both layers
/// get the same `-1`.
type Click = (i32, i32);

fn run_rhai_local(clicks: &[Click], player: Option<i64>) -> Emitted {
    let world = started_world();
    let (rec, bridge) = recorder(player);
    let mut local = LocalBackend::new(&world, &bridge);
    local.load(&chess_script()).expect("compile");
    local.on_local_init().expect("local_init");
    for &(x, y) in clicks {
        local.on_pointer(0, square(x, y), -1).expect("pointer");
    }
    let out = std::mem::take(&mut rec.lock().expect("recorder").out);
    out
}

fn run_native_local(clicks: &[Click], player: Option<i64>) -> Emitted {
    let world = started_world();
    let (rec, bridge) = recorder(player);
    let mut local = NativeLocalBackend::new(&world, &bridge, Box::new(ChessLocal));
    local.on_local_init();
    for &(x, y) in clicks {
        local.on_pointer(0, square(x, y), None);
    }
    let out = std::mem::take(&mut rec.lock().expect("recorder").out);
    out
}

fn assert_local_parity(name: &str, clicks: &[Click], player: Option<i64>) -> Emitted {
    let rhai = run_rhai_local(clicks, player);
    let native = run_native_local(clicks, player);
    assert_eq!(rhai, native, "{name}: the local layers emitted differently");
    native
}

#[test]
fn a_select_then_move_gesture_is_identical() {
    let out = assert_local_parity("e2-e4", &[(4, 1), (4, 3)], None);
    assert_eq!(out.commands.len(), 1, "two clicks should submit one move");
    let (verb, _target, to) = out.commands[0];
    assert_eq!((verb, to), (1, (4, 3)), "MOVE to e4");
    assert_eq!(
        out.selection,
        vec![out.selection[0], -1],
        "select then clear"
    );
}

#[test]
fn clicking_an_empty_square_first_selects_nothing() {
    let out = assert_local_parity("empty first", &[(4, 4), (4, 3)], None);
    assert!(
        out.commands.is_empty(),
        "no selection means no command: {out:?}"
    );
}

#[test]
fn the_other_sides_piece_cannot_be_selected() {
    // Black's pawn on e7 while white is to move.
    let out = assert_local_parity("wrong colour", &[(4, 6), (4, 4)], None);
    assert!(out.commands.is_empty(), "black may not move first: {out:?}");
}

#[test]
fn clicks_off_the_board_are_ignored() {
    let out = assert_local_parity("off board", &[(-1, -1), (9, 9)], None);
    assert_eq!(out, Emitted::default(), "nothing should happen");
}

#[test]
fn a_client_may_only_act_on_its_own_side() {
    // Player 1 (black) clicking white's pawn on white's turn: the gate is
    // the local layer's, and both implementations must apply it.
    let out = assert_local_parity("wrong player", &[(4, 1), (4, 3)], Some(1));
    assert!(
        out.commands.is_empty(),
        "black's client must not move white: {out:?}"
    );
    // …while player 0 on the same position goes through.
    let out = assert_local_parity("right player", &[(4, 1), (4, 3)], Some(0));
    assert_eq!(out.commands.len(), 1, "white's own client may move");
}

#[test]
fn the_command_names_the_selected_piece() {
    // The gesture must submit the *entity* it highlighted, not the square
    // — the one place a local layer can silently address the wrong thing.
    let world = started_world();
    let (rec, bridge) = recorder(None);
    let mut local = NativeLocalBackend::new(&world, &bridge, Box::new(ChessLocal));
    for &(x, y) in &[(4, 1), (4, 3)] {
        local.on_pointer(0, square(x, y), None);
    }
    let out = &rec.lock().expect("recorder").out;
    let pawn = {
        let w = world.lock().expect("world mutex");
        w.entities(monada_sim::ArchetypeId(0))
            .iter()
            .copied()
            .find(|&e| w.position(e) == Some(square(4, 1)))
            .expect("a pawn on e2")
    };
    assert_eq!(
        out.commands[0].1,
        i64::try_from(pawn.0).expect("entity id fits"),
        "the submitted target should be the highlighted pawn"
    );
    assert_eq!(out.selection.first(), Some(&out.commands[0].1));
}

/// A local layer that reads the world it was given, not a snapshot: after
/// the sim applies a move, the next gesture must see the new position.
#[test]
fn the_gesture_follows_the_world() {
    let world = started_world();
    let (rec, bridge) = recorder(None);
    let mut sim = NativeBackend::new(world.clone(), Box::new(ChessRules::new()));
    sim.set_bridge(&bridge);
    let mut local = NativeLocalBackend::new(&world, &bridge, Box::new(ChessLocal));

    // Click e2 then e4, then route the command the way the host would.
    local.on_pointer(0, square(4, 1), None);
    local.on_pointer(0, square(4, 3), None);
    let (verb, target, (tx, ty)) = rec.lock().expect("recorder").out.commands[0];
    sim.on_command(
        monada_sim::PlayerId(0),
        &monada_sim::Command {
            verb: u32::try_from(verb).expect("verb fits"),
            target: EntityId(u64::try_from(target).expect("target fits")),
            arg: square(tx, ty),
        },
    )
    .expect("command");

    // e2 is now empty, so a click there selects nothing.
    rec.lock().expect("recorder").out = Emitted::default();
    local.on_pointer(0, square(4, 1), None);
    assert!(
        rec.lock().expect("recorder").out.selection.is_empty(),
        "the pawn has left e2; nothing to select there"
    );
    // …and the pawn is selectable on e4, which is black's turn now, so it
    // must NOT be selectable — the gesture reads `to_move` from the world.
    local.on_pointer(0, square(4, 3), None);
    assert!(
        rec.lock().expect("recorder").out.selection.is_empty(),
        "white just moved; white pieces are not selectable"
    );
}
