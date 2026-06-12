//! The chess map's rules as a unit-level canary (DESIGN.md §6). Drives
//! the `command` handler directly (no host, no net) and asserts: opening
//! setup, legal piece movement, turn alternation, illegal-move rejection
//! with the sim hash untouched, capture = despawn, and win-on-king-
//! capture — plus the `ui_emit_event` stream the host/HUD consumes. This
//! is the seed of the M4 oracle golden (slice 3).

use monada_fixed::{Fixed, FixedVec3};
use monada_script::{shared_world, RhaiBackend, ScriptBackend, SharedWorld, UiEvent, CHESS_SCRIPT};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const PIECE: ArchetypeId = ArchetypeId(0);
const MOVE: u32 = 1;
const WHITE: PlayerId = PlayerId(0);
const BLACK: PlayerId = PlayerId(1);

/// Event codes, mirroring the header of `scripts/chess.rhai`.
const EV_TURN: u32 = 1;
const EV_ILLEGAL: u32 = 2;
const EV_CAPTURE: u32 = 3;
const EV_GAME_OVER: u32 = 4;

fn fresh() -> (SharedWorld, RhaiBackend) {
    let world = shared_world(SEED);
    let mut backend = RhaiBackend::new(world.clone());
    backend.load(CHESS_SCRIPT).expect("compile chess.rhai");
    backend.on_init().expect("init runs");
    backend.drain_ui_events(); // discard anything emitted during setup
    (world, backend)
}

fn square(x: i32, y: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO)
}

/// The piece entity standing on `(x, y)`, if any.
fn piece_at(world: &SharedWorld, x: i32, y: i32) -> Option<EntityId> {
    let w = world.lock().unwrap();
    w.entities(PIECE)
        .iter()
        .copied()
        .find(|&e| w.position(e) == Some(square(x, y)))
}

fn occupied(world: &SharedWorld, x: i32, y: i32) -> bool {
    piece_at(world, x, y).is_some()
}

fn piece_count(world: &SharedWorld) -> usize {
    world.lock().unwrap().count(PIECE)
}

/// Issue `move (fx,fy) -> (tx,ty)` as `player`, targeting whatever piece
/// stands on the source square, and return the UI events it produced.
fn mv(
    backend: &mut RhaiBackend,
    world: &SharedWorld,
    player: PlayerId,
    fx: i32,
    fy: i32,
    tx: i32,
    ty: i32,
) -> Vec<UiEvent> {
    let e = piece_at(world, fx, fy).expect("a piece on the source square");
    backend
        .on_command(player, &Command::on(MOVE, e, square(tx, ty)))
        .expect("handler runs");
    backend.drain_ui_events()
}

#[test]
fn opening_position_is_standard() {
    let (world, _backend) = fresh();
    assert_eq!(piece_count(&world), 32, "16 pieces a side");
    // A couple of spot checks: the white e-pawn and black queen's rook.
    assert!(occupied(&world, 4, 1), "white e-pawn on e2");
    assert!(occupied(&world, 0, 7), "black rook on a8");
    assert!(!occupied(&world, 4, 3), "e4 empty at the start");
}

#[test]
fn legal_moves_alternate_turns() {
    let (world, mut b) = fresh();

    // 1. e2-e4 (white double step).
    let ev = mv(&mut b, &world, WHITE, 4, 1, 4, 3);
    assert_eq!(ev, vec![turn(1)], "white moved, black to move");
    assert!(occupied(&world, 4, 3) && !occupied(&world, 4, 1));

    // 1... Ng8-f6 (black knight, the L-move canary).
    let ev = mv(&mut b, &world, BLACK, 6, 7, 5, 5);
    assert_eq!(ev, vec![turn(0)], "black moved, white to move");
    assert!(occupied(&world, 5, 5) && !occupied(&world, 6, 7));

    // 2. Bf1-c4 (white bishop slides over the now-empty e2/d3 diagonal).
    let ev = mv(&mut b, &world, WHITE, 5, 0, 2, 3);
    assert_eq!(ev, vec![turn(1)]);
    assert!(occupied(&world, 2, 3) && !occupied(&world, 5, 0));
}

#[test]
fn illegal_moves_are_rejected_without_touching_state() {
    let (world, mut b) = fresh();
    let hash0 = world.lock().unwrap().state_hash();

    // Black has no move yet: white is to move.
    let e = piece_at(&world, 4, 6).unwrap();
    b.on_command(BLACK, &Command::on(MOVE, e, square(4, 4)))
        .unwrap();
    assert_eq!(b.drain_ui_events(), vec![illegal(0)], "out of turn");

    // White tries to move a black piece.
    let e = piece_at(&world, 0, 6).unwrap();
    b.on_command(WHITE, &Command::on(MOVE, e, square(0, 5)))
        .unwrap();
    assert_eq!(b.drain_ui_events(), vec![illegal(1)], "not your piece");

    // White knight to a non-L (empty) square.
    let e = piece_at(&world, 1, 0).unwrap();
    b.on_command(WHITE, &Command::on(MOVE, e, square(1, 2)))
        .unwrap();
    assert_eq!(b.drain_ui_events(), vec![illegal(3)], "not a knight move");

    // A blocked rook (own pawn in front) cannot move.
    let e = piece_at(&world, 0, 0).unwrap();
    b.on_command(WHITE, &Command::on(MOVE, e, square(0, 3)))
        .unwrap();
    assert_eq!(b.drain_ui_events(), vec![illegal(3)], "rook path blocked");

    assert_eq!(
        world.lock().unwrap().state_hash(),
        hash0,
        "no illegal attempt may perturb the deterministic state"
    );
    assert_eq!(piece_count(&world), 32);
}

#[test]
fn capture_removes_the_taken_piece() {
    let (world, mut b) = fresh();

    mv(&mut b, &world, WHITE, 4, 1, 4, 3); // 1. e4
    mv(&mut b, &world, BLACK, 3, 6, 3, 4); // 1... d5
    let ev = mv(&mut b, &world, WHITE, 4, 3, 3, 4); // 2. exd5

    assert_eq!(ev, vec![capture(3, 4), turn(1)], "pawn takes, then black");
    assert_eq!(piece_count(&world), 31, "one black pawn gone");
    let taken = piece_at(&world, 3, 4).expect("white pawn now stands on d5");
    assert_eq!(
        world.lock().unwrap().field(taken, "color"),
        Some(Fixed::from_int(0)),
        "the survivor on d5 is the white pawn"
    );
}

#[test]
fn capturing_the_king_wins() {
    let (world, mut b) = fresh();

    // A fool's-mate shape, but played to actually *take* the king (the
    // subset has no check rule — king capture is the win condition).
    mv(&mut b, &world, WHITE, 5, 1, 5, 2); // 1. f3
    mv(&mut b, &world, BLACK, 4, 6, 4, 4); // 1... e5
    mv(&mut b, &world, WHITE, 6, 1, 6, 3); // 2. g4
    mv(&mut b, &world, BLACK, 3, 7, 7, 3); // 2... Qh4 (slides the open diagonal)
    mv(&mut b, &world, WHITE, 0, 1, 0, 2); // 3. a3 (a waiting move)

    let ev = mv(&mut b, &world, BLACK, 7, 3, 4, 0); // 3... Qxe1 — takes the king

    assert_eq!(
        ev,
        vec![capture(4, 0), game_over(1)],
        "king captured -> black wins, no turn handoff"
    );
    // The king is gone; the capturing black queen now stands on e1.
    assert_eq!(piece_count(&world), 31, "white king removed");
    let on_e1 = piece_at(&world, 4, 0).expect("the black queen occupies e1");
    let w = world.lock().unwrap();
    assert_eq!(w.field(on_e1, "color"), Some(Fixed::from_int(1)), "it is black");
    assert_eq!(w.field(on_e1, "kind"), Some(Fixed::from_int(4)), "it is the queen");
    drop(w);

    // The game is decided: further commands are rejected as game-over.
    let e = piece_at(&world, 0, 2).unwrap();
    b.on_command(WHITE, &Command::on(MOVE, e, square(0, 3)))
        .unwrap();
    assert_eq!(b.drain_ui_events(), vec![illegal(4)], "game already over");
}

// --- event constructors ----------------------------------------------

fn turn(side: i64) -> UiEvent {
    UiEvent {
        code: EV_TURN,
        a: side,
        b: 0,
        c: 0,
    }
}

fn illegal(reason: i64) -> UiEvent {
    UiEvent {
        code: EV_ILLEGAL,
        a: reason,
        b: 0,
        c: 0,
    }
}

fn capture(x: i64, y: i64) -> UiEvent {
    UiEvent {
        code: EV_CAPTURE,
        a: x,
        b: y,
        c: 0,
    }
}

fn game_over(winner: i64) -> UiEvent {
    UiEvent {
        code: EV_GAME_OVER,
        a: winner,
        b: 0,
        c: 0,
    }
}
