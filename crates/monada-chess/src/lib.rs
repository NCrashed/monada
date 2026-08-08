//! Chess as **compiled rules** — the second implementation of the same
//! map, and the D-0 gate of docs/plans/desert-game.md §11.
//!
//! `map/scripts/main.rhai` stays the shipped map and the living proof
//! that the Rhai path works; this module is its line-for-line twin
//! against [`monada_runtime::Host`], written to answer one question with
//! a test rather than an argument: *does a map behave identically under
//! the native backend?* `tests/native_parity.rs` plays the same moves
//! through both runtimes and compares the world's state hash after every
//! one of them.
//!
//! Faithfulness is the whole point, so the port keeps the script's
//! observable order of operations even where Rust would prefer another
//! shape: `entities_of` scan order (which decides which entity
//! `piece_at` returns), the sequence of field writes, and the fact that
//! reasoning runs on a 64-cell board snapshot rather than on the world.
//!
//! State model (identical to the script's — see its header):
//!
//! - archetype 0 `piece`, fields `kind` / `color`, position = the square
//!   `(x, y, 0)`;
//! - archetype 1 `game`, a parked singleton holding `to_move`, `winner`,
//!   `castle`, `ep_x`, `ep_y` and `halfmove`;
//! - `kind`: 0 pawn, 1 knight, 2 bishop, 3 rook, 4 queen, 5 king;
//! - `color`: 0 white, 1 black; a board code is `kind * 2 + color`, with
//!   `-1` for an empty square.

// The rules crate's determinism wall (docs/plans/desert-game.md §3c):
// compiled rules can express what Rhai's `no_float` made impossible, so
// the lints stand in for the runtime's guarantee.
#![deny(clippy::float_arithmetic)]
#![forbid(unsafe_code)]
// Board maths is terse by nature — `b`, `x`, `y`, `k`, `c` are the
// script's own names and reworking them into prose would obscure the
// line-for-line correspondence this port exists to keep.
#![allow(clippy::many_single_char_names, clippy::similar_names)]
// Board indices are `i32` because the rules reason in signed deltas
// (`tx - fx`, off-board probes at -1), and every index that reaches an
// array has already passed `in_b` or comes from a `0..8` loop — the
// invariant the script carried with no types at all.
#![allow(clippy::cast_sign_loss)]
// The move-offset tables read as data at their point of use.
#![allow(clippy::items_after_statements)]

use monada_fixed::{Fixed, FixedVec3};
use monada_runtime::{Host, MapRules};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

/// The reasoning board: piece codes by `y * 8 + x`, `-1` for empty.
type Board = [i32; 64];

/// The `MOVE` verb the local layer submits (`submit_command(1, …)`).
const MOVE: u32 = 1;

/// Chess's rules, as a [`MapRules`] value.
///
/// The fields are *handles*, not hashed state: archetype ids and model
/// ids are re-derived identically by `init` on every peer, so the default
/// `snapshot` ("no state of my own") is correct — this map keeps
/// everything in the world, exactly as the script must.
pub struct ChessRules {
    piece: ArchetypeId,
    game: ArchetypeId,
    /// `models[kind * 2 + color]`, filled by `init`.
    models: Vec<i64>,
}

impl Default for ChessRules {
    fn default() -> Self {
        Self::new()
    }
}

impl ChessRules {
    #[must_use]
    pub fn new() -> ChessRules {
        ChessRules {
            piece: ArchetypeId(0),
            game: ArchetypeId(1),
            models: Vec::new(),
        }
    }

    /// The singleton `game` entity.
    fn game_entity(&self, host: &dyn Host) -> EntityId {
        host.entities_of(self.game)[0]
    }

    /// An integer field off an entity (the script's `to_int(entity_field(…))`).
    fn field_i32(host: &dyn Host, e: EntityId, name: &str) -> i32 {
        host.entity_field(e, name).floor_to_int()
    }

    fn set_field_i32(host: &dyn Host, e: EntityId, name: &str, v: i32) {
        host.entity_set_field(e, name, Fixed::from_int(v));
    }

    /// Entity id of the piece on `(x, y)`, or `None` — the script's
    /// `piece_at`, including its first-match-wins scan order.
    fn piece_at(&self, host: &dyn Host, x: i32, y: i32) -> Option<EntityId> {
        host.entities_of(self.piece).into_iter().find(|&e| {
            let p = host.entity_position(e);
            p.x.floor_to_int() == x && p.y.floor_to_int() == y
        })
    }

    fn is_piece(&self, host: &dyn Host, target: EntityId) -> bool {
        host.entities_of(self.piece).contains(&target)
    }

    /// Snapshot the live board into piece codes. All rule reasoning runs
    /// on this, never on the world.
    fn snapshot(&self, host: &dyn Host) -> Board {
        let mut b = [-1; 64];
        for e in host.entities_of(self.piece) {
            let p = host.entity_position(e);
            let x = p.x.floor_to_int();
            let y = p.y.floor_to_int();
            let k = Self::field_i32(host, e, "kind");
            let c = Self::field_i32(host, e, "color");
            b[(y * 8 + x) as usize] = k * 2 + c;
        }
        b
    }

    fn place(&self, host: &dyn Host, kind: i32, color: i32, x: i32, y: i32) {
        let e = host.entity_create(self.piece);
        host.entity_set_position(e, square(x, y));
        Self::set_field_i32(host, e, "kind", kind);
        Self::set_field_i32(host, e, "color", color);
        host.entity_set_model(e, self.models[(kind * 2 + color) as usize]);
    }
}

impl MapRules for ChessRules {
    fn init(&mut self, host: &dyn Host) {
        self.piece = host.archetype(&["kind", "color"]);
        self.game = host.archetype(&["to_move", "winner", "castle", "ep_x", "ep_y", "halfmove"]);

        // The 8×8 board as voxels: a thin two-tone slab per square.
        for sy in 0..8 {
            for sx in 0..8 {
                let c = if (sx + sy) % 2 == 0 {
                    0x8060_4028
                } else {
                    0x80B0_8858
                };
                host.voxel_fill((sx, sy, 0), (sx, sy, 1), c);
            }
        }

        // One model per (kind, color), from the shipped KV6 assets.
        let kinds = ["pawn", "knight", "bishop", "rook", "queen", "king"];
        self.models = Vec::with_capacity(12);
        for k in kinds {
            self.models
                .push(host.model_kv6(&format!("assets/pieces/{k}_white.kv6"), turns_for(0)));
            self.models
                .push(host.model_kv6(&format!("assets/pieces/{k}_black.kv6"), turns_for(1)));
        }

        let g = host.entity_create(self.game);
        host.entity_set_position(g, square(-1, -1));
        Self::set_field_i32(host, g, "to_move", 0); // white first
        Self::set_field_i32(host, g, "winner", -1); // -1 = in progress
        Self::set_field_i32(host, g, "castle", 15); // all four rights
        Self::set_field_i32(host, g, "ep_x", -1); // no en-passant square
        Self::set_field_i32(host, g, "ep_y", -1);
        Self::set_field_i32(host, g, "halfmove", 0);

        let back = [3, 1, 2, 4, 5, 2, 1, 3];
        for x in 0..8 {
            self.place(host, back[x as usize], 0, x, 0); // white back rank
            self.place(host, 0, 0, x, 1); // white pawns
            self.place(host, 0, 1, x, 6); // black pawns
            self.place(host, back[x as usize], 1, x, 7); // black back rank
        }

        host.camera_focus(FixedVec3::new(
            Fixed::from_ratio(7, 2),
            Fixed::from_ratio(7, 2),
            Fixed::ZERO,
        ));
        // White's side (yaw ≈ π/2), steep pitch — see the script's note on
        // why this reads a..h left to right through the host's X mirror.
        host.camera_angle(Fixed::from_ratio(15708, 10000), Fixed::from_ratio(11, 10));
        host.set_light(
            FixedVec3::new(
                Fixed::from_ratio(-7, 10),
                Fixed::from_ratio(-5, 10),
                Fixed::from_ratio(6, 10),
            ),
            Fixed::from_int(1),
        );
        host.set_sky("assets/skybox.png");
        host.status("white to move");
    }

    #[allow(clippy::too_many_lines)] // the script's `command`, one for one
    fn command(&mut self, host: &dyn Host, _player: PlayerId, command: &Command) {
        let g = self.game_entity(host);
        if Self::field_i32(host, g, "winner") != -1 {
            return; // game over
        }
        if command.verb != MOVE {
            return;
        }
        // Turn is enforced by the moving piece's colour, not the command's
        // player id (hotseat plays both sides).
        let to_move = Self::field_i32(host, g, "to_move");
        let target = command.target;
        if !self.is_piece(host, target) {
            host.status("illegal — no piece");
            return;
        }
        if Self::field_i32(host, target, "color") != to_move {
            host.status("illegal — not your turn");
            return;
        }

        let from = host.entity_position(target);
        let fx = from.x.floor_to_int();
        let fy = from.y.floor_to_int();
        let tx = command.arg.x.floor_to_int();
        let ty = command.arg.y.floor_to_int();

        let b = self.snapshot(host);
        let ep_x = Self::field_i32(host, g, "ep_x");
        let ep_y = Self::field_i32(host, g, "ep_y");
        let castle = Self::field_i32(host, g, "castle");

        if !move_is_legal(&b, to_move, fx, fy, tx, ty, ep_x, ep_y, castle) {
            host.status("illegal move");
            return;
        }

        // Legal — classify it for application.
        let kind = Self::field_i32(host, target, "kind");
        let occ = b[(ty * 8 + tx) as usize];
        let is_castle = kind == 5 && (tx - fx).abs() == 2 && ty == fy;
        let is_ep = kind == 0 && tx == ep_x && ty == ep_y && occ == -1;
        let was_capture = occ != -1 || is_ep;

        // Capture (en passant takes the pawn beside the mover).
        if is_ep {
            if let Some(victim) = self.piece_at(host, tx, fy) {
                host.entity_despawn(victim);
            }
        } else if occ != -1 {
            if let Some(victim) = self.piece_at(host, tx, ty) {
                host.entity_despawn(victim);
            }
        }

        host.entity_set_position(target, square(tx, ty));

        // Castling also slides the rook to the king's far side.
        if is_castle {
            if tx == 6 {
                if let Some(rook) = self.piece_at(host, 7, fy) {
                    host.entity_set_position(rook, square(5, fy));
                }
            } else if let Some(rook) = self.piece_at(host, 0, fy) {
                host.entity_set_position(rook, square(3, fy));
            }
        }

        // Promotion — auto-queen on the last rank.
        let mut promoted = false;
        if kind == 0 && (ty == 7 || ty == 0) {
            Self::set_field_i32(host, target, "kind", 4);
            let model = host.model_kv6(
                &format!("assets/pieces/queen_{}.kv6", side(to_move)),
                turns_for(to_move),
            );
            host.entity_set_model(target, model);
            promoted = true;
        }

        let new_castle = update_castle(castle, kind, to_move, fx, fy, tx, ty);
        Self::set_field_i32(host, g, "castle", new_castle);

        // En-passant target: only a two-square pawn push offers one.
        let (nep_x, nep_y) = if kind == 0 && (ty - fy).abs() == 2 {
            (fx, (fy + ty) / 2)
        } else {
            (-1, -1)
        };
        Self::set_field_i32(host, g, "ep_x", nep_x);
        Self::set_field_i32(host, g, "ep_y", nep_y);

        // Fifty-move clock: reset on a pawn move or capture, else tick.
        let hm = if kind == 0 || was_capture {
            0
        } else {
            Self::field_i32(host, g, "halfmove") + 1
        };
        Self::set_field_i32(host, g, "halfmove", hm);

        let next = 1 - to_move;
        Self::set_field_i32(host, g, "to_move", next);

        // Terminal conditions for the side now to move.
        let nb = self.snapshot(host);
        let in_check = king_in_check(&nb, next);
        if !has_any_legal_move(&nb, next, nep_x, nep_y, new_castle) {
            if in_check {
                Self::set_field_i32(host, g, "winner", to_move);
                host.status(&format!("{} wins by checkmate", side(to_move)));
            } else {
                Self::set_field_i32(host, g, "winner", 2);
                host.status("draw — stalemate");
            }
            return;
        }
        if hm >= 100 {
            Self::set_field_i32(host, g, "winner", 2);
            host.status("draw — fifty-move rule");
            return;
        }

        let prefix = if was_capture {
            "capture! "
        } else if promoted {
            "promotion! "
        } else {
            ""
        };
        let suffix = if in_check { " — check" } else { "" };
        host.status(&format!("{prefix}{} to move{suffix}", side(next)));
    }
}

// --- helpers -----------------------------------------------------------

fn square(x: i32, y: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO)
}

fn side(c: i32) -> &'static str {
    if c == 0 {
        "white"
    } else {
        "black"
    }
}

/// Quarter-turns (clockwise about vertical) so the two armies face each
/// other: black one step CW, white the other way (CCW = 3 CW).
fn turns_for(color: i32) -> i64 {
    if color == 0 {
        3
    } else {
        1
    }
}

fn sign_i(n: i32) -> i32 {
    match n.cmp(&0) {
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
    }
}

fn in_b(x: i32, y: i32) -> bool {
    (0..=7).contains(&x) && (0..=7).contains(&y)
}

/// Code at `(x, y)`, or `-1` for empty / off-board.
fn cell(b: &Board, x: i32, y: i32) -> i32 {
    if in_b(x, y) {
        b[(y * 8 + x) as usize]
    } else {
        -1
    }
}

/// Is `(tx, ty)` attacked by any piece of colour `by`?
fn is_attacked(b: &Board, tx: i32, ty: i32, by: i32) -> bool {
    // Pawns sit one rank "behind" the square, toward their own home.
    let pr = if by == 0 { ty - 1 } else { ty + 1 };
    if cell(b, tx - 1, pr) == by || cell(b, tx + 1, pr) == by {
        return true;
    }

    let kn = 2 + by;
    const KNIGHT: [(i32, i32); 8] = [
        (1, 2),
        (2, 1),
        (-1, 2),
        (-2, 1),
        (1, -2),
        (2, -1),
        (-1, -2),
        (-2, -1),
    ];
    if KNIGHT
        .iter()
        .any(|&(dx, dy)| cell(b, tx + dx, ty + dy) == kn)
    {
        return true;
    }

    let kg = 10 + by;
    for dy in -1..=1 {
        for dx in -1..=1 {
            if (dx != 0 || dy != 0) && cell(b, tx + dx, ty + dy) == kg {
                return true;
            }
        }
    }

    let (rk, bs, qn) = (6 + by, 4 + by, 8 + by);
    const ORTH: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];
    const DIAG: [(i32, i32); 4] = [(1, 1), (1, -1), (-1, 1), (-1, -1)];
    for (dirs, near, far) in [(ORTH, rk, qn), (DIAG, bs, qn)] {
        for (dx, dy) in dirs {
            let (mut cx, mut cy) = (tx + dx, ty + dy);
            while in_b(cx, cy) {
                let c = b[(cy * 8 + cx) as usize];
                if c != -1 {
                    if c == near || c == far {
                        return true;
                    }
                    break;
                }
                cx += dx;
                cy += dy;
            }
        }
    }
    false
}

fn king_in_check(b: &Board, color: i32) -> bool {
    let kc = 10 + color;
    let Some(i) = b.iter().position(|&c| c == kc) else {
        return false; // no king on board (can't happen in FIDE)
    };
    let i = i32::try_from(i).expect("a 64-cell board index fits an i32");
    is_attacked(b, i % 8, i / 8, 1 - color)
}

/// Squares strictly between the two ends all empty.
fn path_clear_b(b: &Board, fx: i32, fy: i32, tx: i32, ty: i32) -> bool {
    let sx = sign_i(tx - fx);
    let sy = sign_i(ty - fy);
    let (mut cx, mut cy) = (fx + sx, fy + sy);
    while cx != tx || cy != ty {
        if b[(cy * 8 + cx) as usize] != -1 {
            return false;
        }
        cx += sx;
        cy += sy;
    }
    true
}

/// Movement-pattern legality (no check test, no castling).
#[allow(clippy::too_many_arguments)] // the script.s signature, unchanged
fn pseudo_legal(
    b: &Board,
    kind: i32,
    color: i32,
    fx: i32,
    fy: i32,
    tx: i32,
    ty: i32,
    capturing: bool,
) -> bool {
    let (dx, dy) = (tx - fx, ty - fy);
    let (adx, ady) = (dx.abs(), dy.abs());
    match kind {
        1 => (adx == 1 && ady == 2) || (adx == 2 && ady == 1), // knight
        5 => adx <= 1 && ady <= 1, // king (one step; castling elsewhere)
        3 => (dx == 0 || dy == 0) && path_clear_b(b, fx, fy, tx, ty),
        2 => adx == ady && path_clear_b(b, fx, fy, tx, ty),
        4 => (dx == 0 || dy == 0 || adx == ady) && path_clear_b(b, fx, fy, tx, ty),
        0 => {
            let dir = if color == 0 { 1 } else { -1 };
            let start = if color == 0 { 1 } else { 6 };
            if capturing {
                return adx == 1 && dy == dir;
            }
            if dx == 0 && dy == dir {
                return true;
            }
            dx == 0 && dy == 2 * dir && fy == start && b[((fy + dir) * 8 + fx) as usize] == -1
        }
        _ => false,
    }
}

/// Apply the move on a copy and report whether `color`'s king is safe.
fn king_safe_after(b: &Board, color: i32, fx: i32, fy: i32, tx: i32, ty: i32, is_ep: bool) -> bool {
    let mut b = *b;
    let mc = b[(fy * 8 + fx) as usize];
    if is_ep {
        b[(fy * 8 + tx) as usize] = -1; // the en-passant-captured pawn
    }
    b[(ty * 8 + tx) as usize] = mc;
    b[(fy * 8 + fx) as usize] = -1;
    !king_in_check(&b, color)
}

/// King two squares toward a rook: all the castling conditions.
fn castle_legal(b: &Board, color: i32, fx: i32, fy: i32, tx: i32, castle: i32) -> bool {
    let rank = if color == 0 { 0 } else { 7 };
    if fy != rank || fx != 4 {
        return false;
    }
    if king_in_check(b, color) {
        return false; // may not castle out of check
    }
    let opp = 1 - color;
    let rk = 6 + color;
    let at = |x: i32| b[(rank * 8 + x) as usize];
    if tx == 6 {
        // king-side
        let bit = if color == 0 { 1 } else { 4 };
        castle & bit != 0
            && at(5) == -1
            && at(6) == -1
            && at(7) == rk
            // King may not pass through or land on an attacked square.
            && !is_attacked(b, 5, rank, opp)
            && !is_attacked(b, 6, rank, opp)
    } else if tx == 2 {
        // queen-side
        let bit = if color == 0 { 2 } else { 8 };
        castle & bit != 0
            && at(1) == -1
            && at(2) == -1
            && at(3) == -1
            && at(0) == rk
            && !is_attacked(b, 3, rank, opp)
            && !is_attacked(b, 2, rank, opp)
    } else {
        false
    }
}

/// Movement pattern + castling + en passant + the king-safety filter.
#[allow(clippy::too_many_arguments)] // the script's signature, unchanged
fn move_is_legal(
    b: &Board,
    color: i32,
    fx: i32,
    fy: i32,
    tx: i32,
    ty: i32,
    ep_x: i32,
    ep_y: i32,
    castle: i32,
) -> bool {
    if !in_b(fx, fy) || !in_b(tx, ty) || (fx == tx && fy == ty) {
        return false;
    }
    let mc = b[(fy * 8 + fx) as usize];
    if mc == -1 || mc % 2 != color {
        return false;
    }
    let occ = b[(ty * 8 + tx) as usize];
    if occ != -1 && occ % 2 == color {
        return false; // own piece on target
    }
    let kind = mc / 2;
    if kind == 5 && (tx - fx).abs() == 2 && ty == fy {
        return castle_legal(b, color, fx, fy, tx, castle);
    }
    let is_ep = kind == 0 && tx == ep_x && ty == ep_y && occ == -1;
    let capturing = occ != -1 || is_ep;
    if !pseudo_legal(b, kind, color, fx, fy, tx, ty, capturing) {
        return false;
    }
    king_safe_after(b, color, fx, fy, tx, ty, is_ep)
}

/// Does `color` have any legal move? (Drives checkmate vs stalemate.)
fn has_any_legal_move(b: &Board, color: i32, ep_x: i32, ep_y: i32, castle: i32) -> bool {
    for i in 0..64 {
        let c = b[i as usize];
        if c == -1 || c % 2 != color {
            continue;
        }
        for j in 0..64 {
            if move_is_legal(b, color, i % 8, i / 8, j % 8, j / 8, ep_x, ep_y, castle) {
                return true;
            }
        }
    }
    false
}

/// Retire castling rights touched by a move. Mask: 1 white O-O, 2 white
/// O-O-O, 4 black O-O, 8 black O-O-O.
fn update_castle(
    castle: i32,
    kind: i32,
    color: i32,
    fx: i32,
    fy: i32,
    tx: i32,
    ty: i32,
) -> i32 {
    let mut castle = castle;
    if kind == 5 {
        // king moved → both of its colour's rights
        castle &= 15 ^ if color == 0 { 3 } else { 12 };
    }
    if kind == 3 {
        // rook left a home corner
        castle &= 15 ^ corner_bit(fx, fy);
    }
    // A rook captured on its home corner (whatever took it).
    castle & (15 ^ corner_bit(tx, ty))
}

/// The castling-right bit a home corner carries, or 0 elsewhere.
fn corner_bit(x: i32, y: i32) -> i32 {
    match (x, y) {
        (0, 0) => 2,
        (7, 0) => 1,
        (0, 7) => 8,
        (7, 7) => 4,
        _ => 0,
    }
}
