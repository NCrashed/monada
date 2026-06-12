// chess — the M4 demo map (DESIGN.md §6). A *legal-move subset* of
// standard chess: every piece's movement, capture (= despawn), and
// win-by-king-capture. Castling, en passant, promotion and check / mate
// detection are a later slice.
//
// The engine ships no genre (DESIGN.md §4): all of chess lives here. The
// host forwards each player command's opaque fields to `command(...)`;
// this script alone knows they mean "move the piece `target` to `arg`".
//
// State model (decision A2 — all gameplay state in the World):
//   archetype 0 `piece`  fields [kind, color]; position = board square
//                        (x, y, 0), x/y in 0..7.
//   archetype 1 `game`   fields [to_move, winner]; a singleton, parked
//                        off-board at (-1, -1). Authoritative & hashed.
// `kind`:  0 pawn  1 knight  2 bishop  3 rook  4 queen  5 king
// `color`: 0 white 1 black
//
// Board maths run in native i64 (clean for L-moves / deltas / abs); the
// World stores `Fixed`, so we cross the boundary with `to_int(...)` on
// read and `fixed(...)` / `vec3(...)` on write. That conversion seam is
// the §6 "is Rhai ergonomic for chess?" canary — noted, not fought.
//
// Command shape (monada_sim::Command): (player, verb, target, arg)
//   verb 1  move — move piece `target` to square (arg.x, arg.y)
//
// UI event codes (ui_emit_event(code, a, b, c)) — host/HUD only:
//   1 turn_changed(to_move)         a = side now to move (0/1)
//   2 illegal(reason)               a = 0 not-your-turn 1 not-your-piece
//                                       2 own-piece-on-dest 3 illegal-move
//                                       4 game-over 5 bad-verb
//   3 capture(x, y)                 a,b = square the captured piece stood on
//   4 game_over(winner)             a = winning side (0/1)

fn init() {
    let piece = archetype(["kind", "color"]); // id 0
    let game = archetype(["to_move", "winner"]); // id 1

    let g = entity_create(1);
    entity_set_position(g, vec3(fixed(-1), fixed(-1), fixed(0)));
    entity_set_field(g, "to_move", fixed(0)); // white moves first
    entity_set_field(g, "winner", fixed(-1)); // -1 = game in progress

    // Back-rank piece kinds, files a..h.
    let back = [3, 1, 2, 4, 5, 2, 1, 3];
    for x in 0..8 {
        spawn_piece(back[x], 0, x, 0); // white back rank
        spawn_piece(0, 0, x, 1); // white pawns
        spawn_piece(0, 1, x, 6); // black pawns
        spawn_piece(back[x], 1, x, 7); // black back rank
    }
}

fn spawn_piece(kind, color, x, y) {
    let e = entity_create(0);
    entity_set_position(e, vec3(fixed(x), fixed(y), fixed(0)));
    entity_set_field(e, "kind", fixed(kind));
    entity_set_field(e, "color", fixed(color));
}

fn command(player, verb, target, arg) {
    let g = entities_of(1)[0];

    if to_int(entity_field(g, "winner")) != -1 {
        ui_emit_event(2, 4, 0, 0); // the game is already decided
        return;
    }
    if verb != 1 { // the only verb this map understands is `move`
        ui_emit_event(2, 5, 0, 0);
        return;
    }
    if player != to_int(entity_field(g, "to_move")) {
        ui_emit_event(2, 0, 0, 0); // not your turn
        return;
    }
    if !is_piece(target) || to_int(entity_field(target, "color")) != player {
        ui_emit_event(2, 1, 0, 0); // not your piece (or not a piece)
        return;
    }

    let from = entity_position(target);
    let fx = to_int(from.x);
    let fy = to_int(from.y);
    let tx = to_int(arg.x);
    let ty = to_int(arg.y);

    if tx < 0 || tx > 7 || ty < 0 || ty > 7 || (tx == fx && ty == fy) {
        ui_emit_event(2, 3, 0, 0); // off-board or a non-move
        return;
    }

    // Destination occupancy. A friendly piece blocks; an enemy is a
    // capture (validated as legal for this piece kind below).
    let occ = piece_at(tx, ty);
    if occ != -1 && to_int(entity_field(occ, "color")) == player {
        ui_emit_event(2, 2, 0, 0); // own piece sits on the destination
        return;
    }

    let kind = to_int(entity_field(target, "kind"));
    let capturing = occ != -1;
    if !legal_move(kind, player, fx, fy, tx, ty, capturing) {
        ui_emit_event(2, 3, 0, 0); // not a legal move for this piece
        return;
    }

    if capturing {
        let captured_king = to_int(entity_field(occ, "kind")) == 5;
        entity_despawn(occ);
        ui_emit_event(3, tx, ty, 0);
        if captured_king {
            entity_set_position(target, vec3(fixed(tx), fixed(ty), fixed(0)));
            entity_set_field(g, "winner", fixed(player));
            ui_emit_event(4, player, 0, 0);
            return;
        }
    }

    entity_set_position(target, vec3(fixed(tx), fixed(ty), fixed(0)));
    let next = 1 - player;
    entity_set_field(g, "to_move", fixed(next));
    ui_emit_event(1, next, 0, 0);
}

// --- helpers ---------------------------------------------------------

// The piece entity standing on (x, y), or -1 if the square is empty.
fn piece_at(x, y) {
    for e in entities_of(0) {
        let p = entity_position(e);
        if to_int(p.x) == x && to_int(p.y) == y {
            return e;
        }
    }
    return -1;
}

fn is_piece(target) {
    for e in entities_of(0) {
        if e == target {
            return true;
        }
    }
    return false;
}

fn abs_i(n) {
    if n < 0 { -n } else { n }
}

fn sign_i(n) {
    if n > 0 { 1 } else if n < 0 { -1 } else { 0 }
}

// True when every square strictly between (fx,fy) and (tx,ty) along a
// rank / file / diagonal is empty. Caller guarantees the line is one of
// those; the destination square itself is not checked here.
fn path_clear(fx, fy, tx, ty) {
    let sx = sign_i(tx - fx);
    let sy = sign_i(ty - fy);
    let cx = fx + sx;
    let cy = fy + sy;
    while cx != tx || cy != ty {
        if piece_at(cx, cy) != -1 {
            return false;
        }
        cx += sx;
        cy += sy;
    }
    return true;
}

fn legal_move(kind, color, fx, fy, tx, ty, capturing) {
    let dx = tx - fx;
    let dy = ty - fy;
    let adx = abs_i(dx);
    let ady = abs_i(dy);

    if kind == 1 { // knight: the L
        return (adx == 1 && ady == 2) || (adx == 2 && ady == 1);
    }
    if kind == 5 { // king: one square any direction
        return adx <= 1 && ady <= 1;
    }
    if kind == 3 { // rook: rank or file, path clear
        if dx != 0 && dy != 0 { return false; }
        return path_clear(fx, fy, tx, ty);
    }
    if kind == 2 { // bishop: diagonal, path clear
        if adx != ady { return false; }
        return path_clear(fx, fy, tx, ty);
    }
    if kind == 4 { // queen: rook or bishop
        if dx != 0 && dy != 0 && adx != ady { return false; }
        return path_clear(fx, fy, tx, ty);
    }
    if kind == 0 { // pawn
        let dir = if color == 0 { 1 } else { -1 };
        let start = if color == 0 { 1 } else { 6 };
        if capturing {
            return adx == 1 && dy == dir; // diagonal one, forward only
        }
        if dx == 0 && dy == dir {
            return true; // single step forward onto an empty square
        }
        if dx == 0 && dy == 2 * dir && fy == start {
            return piece_at(fx, fy + dir) == -1; // double step, path clear
        }
        return false;
    }
    return false;
}
