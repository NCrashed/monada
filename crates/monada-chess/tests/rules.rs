//! The chess map's rules as a unit-level canary (DESIGN.md §6). Loads the
//! rules **from the packed `map/` archive** (exercising the format round-
//! trip) under a [`NullBridge`] (so `init`'s render calls are no-ops), then
//! drives the `command` handler directly and asserts the resulting **world
//! state**: opening setup, legal movement, turn alternation, illegal-move
//! rejection with the sim hash untouched, capture = despawn, and win-on-
//! king-capture. The seed of the M4 oracle golden (slice 4).

use std::path::Path;
use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_script::{
    shared_world, NullBridge, RhaiBackend, ScriptBackend, SharedBridge, SharedWorld,
};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const PIECE: ArchetypeId = ArchetypeId(0);
const GAME: ArchetypeId = ArchetypeId(1);
const MOVE: u32 = 1;
const ANY: PlayerId = PlayerId(0); // turn is enforced by piece colour, not id

/// The chess rules, loaded through the real archive path: pack `map/`,
/// read it back, take the manifest's entry script.
fn chess_script() -> String {
    let map_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("map");
    let bytes = monada_format::pack_dir(&map_dir).expect("pack chess map");
    let map = monada_format::Map::read(&bytes).expect("read chess map");
    map.entry_script()
        .expect("chess map has an entry script")
        .to_string()
}

fn fresh() -> (SharedWorld, RhaiBackend) {
    let world = shared_world(SEED);
    let mut backend = RhaiBackend::new(world.clone());
    // `init` defines models / paints the board — needs a bridge (no-op).
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    backend.load(&chess_script()).expect("compile main.rhai");
    backend.on_init().expect("init runs");
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

/// Read the singleton `game` entity's integer field (`to_move`/`winner`).
fn game_field(world: &SharedWorld, field: &str) -> i64 {
    let w = world.lock().unwrap();
    let g = w.entities(GAME)[0];
    i64::from(w.field(g, field).unwrap().floor_to_int())
}

/// Move whatever stands on `(fx,fy)` to `(tx,ty)` (as the side to move).
fn mv(b: &mut RhaiBackend, world: &SharedWorld, fx: i32, fy: i32, tx: i32, ty: i32) {
    let e = piece_at(world, fx, fy).expect("a piece on the source square");
    b.on_command(ANY, &Command::on(MOVE, e, square(tx, ty)))
        .expect("handler runs");
}

#[test]
fn opening_position_is_standard() {
    let (world, _b) = fresh();
    assert_eq!(piece_count(&world), 32, "16 pieces a side");
    assert!(occupied(&world, 4, 1), "white e-pawn on e2");
    assert!(occupied(&world, 0, 7), "black rook on a8");
    assert!(!occupied(&world, 4, 3), "e4 empty at the start");
    assert_eq!(game_field(&world, "to_move"), 0, "white to move");
    assert_eq!(game_field(&world, "winner"), -1, "game in progress");
    assert_eq!(game_field(&world, "castle"), 15, "all four castling rights");
    assert_eq!(game_field(&world, "ep_x"), -1, "no en-passant square");
    assert_eq!(game_field(&world, "halfmove"), 0, "fifty-move clock at zero");
}

#[test]
fn legal_moves_alternate_turns() {
    let (world, mut b) = fresh();

    mv(&mut b, &world, 4, 1, 4, 3); // 1. e4 (white double step)
    assert!(occupied(&world, 4, 3) && !occupied(&world, 4, 1));
    assert_eq!(game_field(&world, "to_move"), 1, "black to move");

    mv(&mut b, &world, 6, 7, 5, 5); // 1... Nf6 (the L-move canary)
    assert!(occupied(&world, 5, 5) && !occupied(&world, 6, 7));
    assert_eq!(game_field(&world, "to_move"), 0, "white to move");

    mv(&mut b, &world, 5, 0, 2, 3); // 2. Bc4 (bishop slides the diagonal)
    assert!(occupied(&world, 2, 3) && !occupied(&world, 5, 0));
    assert_eq!(game_field(&world, "to_move"), 1);
}

#[test]
fn illegal_moves_are_rejected_without_touching_state() {
    let (world, mut b) = fresh();
    let hash0 = world.lock().unwrap().state_hash();

    // Black to move out of turn (white's turn): rejected by colour.
    let e = piece_at(&world, 4, 6).unwrap();
    b.on_command(ANY, &Command::on(MOVE, e, square(4, 4)))
        .unwrap();

    // White knight to a non-L (empty) square.
    let e = piece_at(&world, 1, 0).unwrap();
    b.on_command(ANY, &Command::on(MOVE, e, square(1, 2)))
        .unwrap();

    // A blocked rook (own pawn in front) cannot move.
    let e = piece_at(&world, 0, 0).unwrap();
    b.on_command(ANY, &Command::on(MOVE, e, square(0, 3)))
        .unwrap();

    assert_eq!(
        world.lock().unwrap().state_hash(),
        hash0,
        "no illegal attempt may perturb the deterministic state"
    );
    assert_eq!(piece_count(&world), 32);
    assert_eq!(game_field(&world, "to_move"), 0, "still white to move");
}

#[test]
fn capture_removes_the_taken_piece() {
    let (world, mut b) = fresh();

    mv(&mut b, &world, 4, 1, 4, 3); // 1. e4
    mv(&mut b, &world, 3, 6, 3, 4); // 1... d5
    mv(&mut b, &world, 4, 3, 3, 4); // 2. exd5

    assert_eq!(piece_count(&world), 31, "one black pawn gone");
    let taken = piece_at(&world, 3, 4).expect("white pawn now stands on d5");
    assert_eq!(
        world.lock().unwrap().field(taken, "color"),
        Some(Fixed::from_int(0)),
        "the survivor on d5 is the white pawn"
    );
    assert_eq!(
        game_field(&world, "to_move"),
        1,
        "black to move after capture"
    );
}

/// The kind/colour of the piece on a square (for the FIDE canaries).
fn piece_kind_color(world: &SharedWorld, x: i32, y: i32) -> (i64, i64) {
    let e = piece_at(world, x, y).expect("a piece on the square");
    let w = world.lock().unwrap();
    (
        i64::from(w.field(e, "kind").unwrap().floor_to_int()),
        i64::from(w.field(e, "color").unwrap().floor_to_int()),
    )
}

#[test]
fn fools_mate_is_checkmate() {
    let (world, mut b) = fresh();

    // Fool's mate — the fastest checkmate. Black's queen mates on h4; the
    // white king is boxed in by its own unmoved pieces (so it is mate, not
    // a king capture: kings are never taken under the full rules).
    mv(&mut b, &world, 5, 1, 5, 2); // 1. f3
    mv(&mut b, &world, 4, 6, 4, 4); // 1... e5
    mv(&mut b, &world, 6, 1, 6, 3); // 2. g4
    mv(&mut b, &world, 3, 7, 7, 3); // 2... Qh4#

    assert_eq!(game_field(&world, "winner"), 1, "black wins by checkmate");
    assert_eq!(piece_count(&world), 32, "checkmate, nothing captured");
    assert!(occupied(&world, 4, 0), "the white king still stands on e1");

    // The game is decided: further commands are no-ops.
    let before = world.lock().unwrap().state_hash();
    let e = piece_at(&world, 0, 1).unwrap(); // white a-pawn, still home
    b.on_command(ANY, &Command::on(MOVE, e, square(0, 2))).unwrap();
    assert_eq!(
        world.lock().unwrap().state_hash(),
        before,
        "game over: no moves"
    );
}

#[test]
fn castling_kingside_moves_king_and_rook() {
    let (world, mut b) = fresh();

    // Clear f1/g1 and keep the h1 rook & e1 king home, then O-O.
    mv(&mut b, &world, 6, 0, 5, 2); // 1. Nf3   (vacates g1)
    mv(&mut b, &world, 0, 6, 0, 5); // 1... a6
    mv(&mut b, &world, 4, 1, 4, 2); // 2. e3    (opens the f1 bishop)
    mv(&mut b, &world, 1, 6, 1, 5); // 2... b6
    mv(&mut b, &world, 5, 0, 4, 1); // 3. Be2   (vacates f1)
    mv(&mut b, &world, 2, 6, 2, 5); // 3... c6
    mv(&mut b, &world, 4, 0, 6, 0); // 4. O-O   (king e1 -> g1, two squares)

    assert!(!occupied(&world, 4, 0), "king left e1");
    assert!(!occupied(&world, 7, 0), "rook left h1");
    assert_eq!(piece_kind_color(&world, 6, 0), (5, 0), "white king on g1");
    assert_eq!(piece_kind_color(&world, 5, 0), (3, 0), "white rook on f1");
    assert_eq!(game_field(&world, "to_move"), 1, "black to move");
    // White forfeited both castling rights (bits 1|2); black keeps 4|8.
    assert_eq!(game_field(&world, "castle"), 12, "white rights spent");
}

#[test]
fn castling_queenside_moves_king_and_rook() {
    let (world, mut b) = fresh();

    // Clear b1/c1/d1 for white, keep the a1 rook & e1 king home, then O-O-O.
    mv(&mut b, &world, 3, 1, 3, 3); // 1. d4   (frees d1/c1 diagonals)
    mv(&mut b, &world, 0, 6, 0, 5); // 1... a6
    mv(&mut b, &world, 1, 0, 2, 2); // 2. Nc3  (vacates b1)
    mv(&mut b, &world, 1, 6, 1, 5); // 2... b6
    mv(&mut b, &world, 2, 0, 5, 3); // 3. Bf4  (vacates c1)
    mv(&mut b, &world, 2, 6, 2, 5); // 3... c6
    mv(&mut b, &world, 3, 0, 3, 1); // 4. Qd2  (vacates d1)
    mv(&mut b, &world, 5, 6, 5, 5); // 4... f6
    mv(&mut b, &world, 4, 0, 2, 0); // 5. O-O-O (king e1 -> c1, two squares)

    assert!(!occupied(&world, 4, 0), "king left e1");
    assert!(!occupied(&world, 0, 0), "rook left a1");
    assert_eq!(piece_kind_color(&world, 2, 0), (5, 0), "white king on c1");
    assert_eq!(piece_kind_color(&world, 3, 0), (3, 0), "white rook on d1");
    assert_eq!(game_field(&world, "to_move"), 1, "black to move");
}

#[test]
fn castling_kingside_black_moves_king_and_rook() {
    let (world, mut b) = fresh();

    // White makes waiting moves; black clears f8/g8 then castles O-O.
    mv(&mut b, &world, 0, 1, 0, 2); // 1. a3
    mv(&mut b, &world, 6, 7, 5, 5); // 1... Nf6 (vacates g8)
    mv(&mut b, &world, 0, 2, 0, 3); // 2. a4
    mv(&mut b, &world, 4, 6, 4, 5); // 2... e6  (opens the f8 bishop)
    mv(&mut b, &world, 7, 1, 7, 2); // 3. h3
    mv(&mut b, &world, 5, 7, 4, 6); // 3... Be7 (vacates f8)
    mv(&mut b, &world, 7, 2, 7, 3); // 4. h4
    mv(&mut b, &world, 4, 7, 6, 7); // 4... O-O (king e8 -> g8, two squares)

    assert!(!occupied(&world, 4, 7), "king left e8");
    assert!(!occupied(&world, 7, 7), "rook left h8");
    assert_eq!(piece_kind_color(&world, 6, 7), (5, 1), "black king on g8");
    assert_eq!(piece_kind_color(&world, 5, 7), (3, 1), "black rook on f8");
    assert_eq!(game_field(&world, "to_move"), 0, "white to move");
}

#[test]
fn en_passant_captures_the_passed_pawn() {
    let (world, mut b) = fresh();

    mv(&mut b, &world, 4, 1, 4, 3); // 1. e4
    mv(&mut b, &world, 0, 6, 0, 5); // 1... a6 (waiting)
    mv(&mut b, &world, 4, 3, 4, 4); // 2. e5
    mv(&mut b, &world, 3, 6, 3, 4); // 2... d5  (double step past the e5 pawn)
    assert_eq!(game_field(&world, "ep_x"), 3, "en-passant file is d");
    assert_eq!(game_field(&world, "ep_y"), 5, "en-passant square is d6");

    mv(&mut b, &world, 4, 4, 3, 5); // 3. exd6 e.p.

    assert!(occupied(&world, 3, 5), "white pawn now on d6");
    assert!(!occupied(&world, 3, 4), "the d5 pawn was taken off-square");
    assert_eq!(piece_count(&world), 31, "one black pawn gone");
    assert_eq!(game_field(&world, "ep_x"), -1, "en-passant window closed");
    assert_eq!(game_field(&world, "to_move"), 1, "black to move");
}

#[test]
fn pawn_promotes_to_queen_on_the_last_rank() {
    let (world, mut b) = fresh();

    // March a white pawn to the eighth rank via two captures.
    mv(&mut b, &world, 4, 1, 4, 3); // 1. e4
    mv(&mut b, &world, 3, 6, 3, 4); // 1... d5
    mv(&mut b, &world, 4, 3, 3, 4); // 2. exd5  (white pawn reaches d5)
    mv(&mut b, &world, 6, 7, 5, 5); // 2... Nf6 (waiting)
    mv(&mut b, &world, 3, 4, 3, 5); // 3. d6
    mv(&mut b, &world, 0, 6, 0, 5); // 3... a6 (waiting)
    mv(&mut b, &world, 3, 5, 2, 6); // 4. dxc7  (takes the c-pawn)
    mv(&mut b, &world, 0, 5, 0, 4); // 4... a5 (waiting)
    mv(&mut b, &world, 2, 6, 1, 7); // 5. cxb8=Q (takes the knight, promotes)

    assert_eq!(
        piece_kind_color(&world, 1, 7),
        (4, 0),
        "a white queen now stands on b8"
    );
    assert_eq!(game_field(&world, "to_move"), 1, "black to move");
}
