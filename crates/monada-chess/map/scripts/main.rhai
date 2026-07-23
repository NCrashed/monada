// chess — the M4 demo map (DESIGN.md §6). **Full FIDE move rules**: piece
// movement, captures, castling, en passant, pawn promotion (auto-queen),
// check-aware legality (you may never leave your own king in check),
// checkmate / stalemate, and the fifty-move draw. (Threefold repetition and
// insufficient-material are intentionally out of scope for this map.)
//
// The engine ships no genre (DESIGN.md §4): the *whole* game — rules, board,
// pieces, presentation, interaction — lives here. The host only renders the
// models we define, paints the voxels we paint, and forwards raw pointer
// events to `pointer(...)`.
//
// State model (decision A2 — gameplay state in the World):
//   archetype 0 `piece`  fields [kind, color]; position = board square
//                        (x, y, 0), x/y in 0..7.
//   archetype 1 `game`   fields [to_move, winner, castle, ep_x, ep_y,
//                        halfmove]; a singleton, parked off-board. The whole
//                        authoritative & hashed game state.
//     winner: -1 in progress · 0 white wins · 1 black wins · 2 draw
//     castle: 4-bit mask — 1 white O-O · 2 white O-O-O · 4 black O-O ·
//             8 black O-O-O
//     ep_x/ep_y: square a pawn may be captured en passant on (-1 = none)
//     halfmove: plies since the last pawn move or capture (fifty-move rule)
// `kind`:  0 pawn  1 knight  2 bishop  3 rook  4 queen  5 king
// `color`: 0 white 1 black
//
// Reasoning runs on a script-local board snapshot — an `int[64]` of piece
// codes (`kind*2 + color`, -1 = empty), index `y*8 + x`. Rhai passes array
// arguments by value, so we test hypothetical moves (for check / legality /
// mate) on cheap clones without ever touching the World; only a proven-legal
// move mutates it. Board maths are native i64 (clean deltas/abs/mask); the
// World stores `Fixed`, crossed with `to_int(...)` on read and `fixed(...)` /
// `vec3(...)` on write.
//
// Render / input host-API (DESIGN.md §3.3):
//   model_kv6(asset_path) -> id      define a sprite model from a .kv6 asset
//   entity_set_model(e, id)          bind an entity to a model
//   voxel_fill(x0,y0,z0,x1,y1,z1,c)  paint the board (sim coords)
//   camera_focus(point)              aim the camera (sim coords)
//   status(text)                     set the HUD line
//   highlight(e)/highlight_clear()/highlighted()   local selection
//   submit_command(verb, target, arg)             queue a move

fn init() {
    let piece = archetype(["kind", "color"]); // id 0
    let game = archetype(["to_move", "winner", "castle", "ep_x", "ep_y", "halfmove"]); // id 1

    // Paint the 8×8 board as voxels: a thin two-tone slab per square.
    for sy in 0..8 {
        for sx in 0..8 {
            let c = if (sx + sy) % 2 == 0 { 0x8060_4028 } else { 0x80B0_8858 };
            voxel_fill(sx, sy, 0, sx, sy, 1, c);
        }
    }

    // One sprite model per (kind, color), loaded from the shipped KV6
    // assets (boxes for now — see `examples/gen_pieces.rs`; swap for
    // sculpted art without touching the engine). `models[kind*2 + color]`.
    let kinds = ["pawn", "knight", "bishop", "rook", "queen", "king"];
    let models = [];
    for k in 0..6 {
        models.push(model_kv6("assets/pieces/" + kinds[k] + "_white.kv6", turns_for(0)));
        models.push(model_kv6("assets/pieces/" + kinds[k] + "_black.kv6", turns_for(1)));
    }

    let g = entity_create(1);
    entity_set_position(g, vec3(fixed(-1), fixed(-1), fixed(0)));
    entity_set_field(g, "to_move", fixed(0)); // white first
    entity_set_field(g, "winner", fixed(-1)); // -1 = in progress
    entity_set_field(g, "castle", fixed(15)); // all four rights
    entity_set_field(g, "ep_x", fixed(-1)); // no en-passant square
    entity_set_field(g, "ep_y", fixed(-1));
    entity_set_field(g, "halfmove", fixed(0));

    // Standard setup. Back-rank kinds, files a..h.
    let back = [3, 1, 2, 4, 5, 2, 1, 3];
    for x in 0..8 {
        place(models, back[x], 0, x, 0); // white back rank
        place(models, 0, 0, x, 1); // white pawns
        place(models, 0, 1, x, 6); // black pawns
        place(models, back[x], 1, x, 7); // black back rank
    }

    camera_focus(vec3(ratio(7, 2), ratio(7, 2), fixed(0))); // board centre
    // Sit on white's side (yaw ≈ π/2): white's rank is the near edge. The
    // host mirrors world X (see MapRender::world_of), so from here the files
    // read a..h left to right — e1 dark, h1 light, white's king on white's
    // right, per notation. pitch ≈ 1.1 keeps the steep top-down look.
    camera_angle(ratio(15708, 10000), ratio(11, 10));

    // A directional sun over the board from the +x/+y side and above: it
    // travels toward -x/-y and downward (z grows down → +z travels down),
    // i.e. the light comes from the high near-right corner. That matches the
    // pieces' built-in side-tint (brightest toward +x/+y), so board and
    // pieces read lit from the same side. `fixed(1)` = full shadow contrast.
    set_light(vec3(ratio(-7, 10), ratio(-5, 10), ratio(6, 10)), fixed(1));
    set_sky("assets/skybox.png");
    status("white to move");
}

fn place(models, kind, color, x, y) {
    let e = entity_create(0);
    entity_set_position(e, vec3(fixed(x), fixed(y), fixed(0)));
    entity_set_field(e, "kind", fixed(kind));
    entity_set_field(e, "color", fixed(color));
    entity_set_model(e, models[kind * 2 + color]);
}

// The host forwards a raw click; we run the select → move gesture here.
fn pointer(button, point, entity) {
    if button != 0 {
        return; // left button only
    }
    let g = entities_of(1)[0];
    if to_int(entity_field(g, "winner")) != -1 {
        return; // game decided
    }
    let to_move = to_int(entity_field(g, "to_move"));
    // Networked: this client may only act on its own side. -1 = hotseat
    // (one window drives both sides), so no gating.
    let lp = local_player();
    if lp != -1 && lp != to_move {
        return;
    }
    let sx = to_int(point.x);
    let sy = to_int(point.y);
    if sx < 0 || sx > 7 || sy < 0 || sy > 7 {
        return; // off the board
    }

    let sel = highlighted();
    if sel == -1 {
        // First click: select a piece of the side to move.
        let p = piece_at(sx, sy);
        if p != -1 && to_int(entity_field(p, "color")) == to_move {
            highlight(p);
            status("piece selected — pick a destination");
        }
    } else {
        // Second click: commit the move (the host routes the command).
        highlight_clear();
        submit_command(1, sel, vec3(fixed(sx), fixed(sy), fixed(0)));
    }
}

fn command(player, verb, target, arg) {
    let g = entities_of(1)[0];
    if to_int(entity_field(g, "winner")) != -1 {
        return; // game over
    }
    if verb != 1 {
        return;
    }
    // Turn is enforced by the moving piece's colour, not the command's player
    // id (hotseat plays both sides; LAN colour↔player binding is the host's).
    let to_move = to_int(entity_field(g, "to_move"));
    if !is_piece(target) {
        status("illegal — no piece");
        return;
    }
    if to_int(entity_field(target, "color")) != to_move {
        status("illegal — not your turn");
        return;
    }

    let from = entity_position(target);
    let fx = to_int(from.x);
    let fy = to_int(from.y);
    let tx = to_int(arg.x);
    let ty = to_int(arg.y);

    let b = snapshot();
    let ep_x = to_int(entity_field(g, "ep_x"));
    let ep_y = to_int(entity_field(g, "ep_y"));
    let castle = to_int(entity_field(g, "castle"));

    if !move_is_legal(b, to_move, fx, fy, tx, ty, ep_x, ep_y, castle) {
        status("illegal move");
        return;
    }

    // The move is legal — classify it for application.
    let kind = to_int(entity_field(target, "kind"));
    let occ = b[ty * 8 + tx];
    let is_castle = kind == 5 && abs_i(tx - fx) == 2 && ty == fy;
    let is_ep = kind == 0 && tx == ep_x && ty == ep_y && occ == -1;
    let was_capture = occ != -1 || is_ep;

    // Capture (en passant takes the pawn beside the mover, not on the target).
    if is_ep {
        entity_despawn(piece_at(tx, fy));
    } else if occ != -1 {
        entity_despawn(piece_at(tx, ty));
    }

    // Move the piece.
    entity_set_position(target, vec3(fixed(tx), fixed(ty), fixed(0)));

    // Castling also slides the rook to the king's far side.
    if is_castle {
        if tx == 6 {
            entity_set_position(piece_at(7, fy), vec3(fixed(5), fixed(fy), fixed(0)));
        } else {
            entity_set_position(piece_at(0, fy), vec3(fixed(3), fixed(fy), fixed(0)));
        }
    }

    // Promotion — auto-queen on reaching the last rank.
    let promoted = false;
    if kind == 0 && (ty == 7 || ty == 0) {
        entity_set_field(target, "kind", fixed(4));
        entity_set_model(target, model_kv6("assets/pieces/queen_" + side(to_move) + ".kv6", turns_for(to_move)));
        promoted = true;
    }

    // Castling rights: a king move, a rook leaving home, or a rook captured
    // on its home corner each retire the relevant right(s).
    let new_castle = update_castle(castle, kind, to_move, fx, fy, tx, ty);
    entity_set_field(g, "castle", fixed(new_castle));

    // En-passant target: only a two-square pawn push offers one (for the very
    // next move); anything else clears it.
    let nep_x = -1;
    let nep_y = -1;
    if kind == 0 && abs_i(ty - fy) == 2 {
        nep_x = fx;
        nep_y = (fy + ty) / 2;
    }
    entity_set_field(g, "ep_x", fixed(nep_x));
    entity_set_field(g, "ep_y", fixed(nep_y));

    // Fifty-move clock: reset on a pawn move or capture, else tick.
    let hm = to_int(entity_field(g, "halfmove"));
    if kind == 0 || was_capture { hm = 0; } else { hm += 1; }
    entity_set_field(g, "halfmove", fixed(hm));

    // Hand the move over.
    let next = 1 - to_move;
    entity_set_field(g, "to_move", fixed(next));

    // Terminal conditions for the side now to move.
    let nb = snapshot();
    let in_check = king_in_check(nb, next);
    if !has_any_legal_move(nb, next, nep_x, nep_y, new_castle) {
        if in_check {
            entity_set_field(g, "winner", fixed(to_move));
            status(side(to_move) + " wins by checkmate");
        } else {
            entity_set_field(g, "winner", fixed(2));
            status("draw — stalemate");
        }
        return;
    }
    if hm >= 100 {
        entity_set_field(g, "winner", fixed(2));
        status("draw — fifty-move rule");
        return;
    }

    let prefix = if was_capture { "capture! " } else if promoted { "promotion! " } else { "" };
    let suffix = if in_check { " — check" } else { "" };
    status(prefix + side(next) + " to move" + suffix);
}

// --- World helpers (cross the Fixed boundary) ------------------------

fn side(c) {
    if c == 0 { "white" } else { "black" }
}

// Quarter-turns (clockwise about vertical) to face a colour's art so the two
// armies face each other: black one step CW, white the other way (CCW = 3
// CW). Tweak here if a colour faces the wrong way.
fn turns_for(color) {
    if color == 0 { 3 } else { 1 }
}

// Entity id of the piece on (x, y), or -1. Used only when *applying* a move.
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

// Snapshot the live board into an `int[64]` of piece codes (`kind*2 + color`,
// -1 = empty). All rule reasoning runs on this, never on the World.
fn snapshot() {
    let b = [];
    b.pad(64, -1);
    for e in entities_of(0) {
        let p = entity_position(e);
        let x = to_int(p.x);
        let y = to_int(p.y);
        let k = to_int(entity_field(e, "kind"));
        let c = to_int(entity_field(e, "color"));
        b[y * 8 + x] = k * 2 + c;
    }
    b
}

// --- board reasoning (pure, on a snapshot) ---------------------------

fn abs_i(n) {
    if n < 0 { -n } else { n }
}

fn sign_i(n) {
    if n > 0 { 1 } else if n < 0 { -1 } else { 0 }
}

fn in_b(x, y) {
    x >= 0 && x <= 7 && y >= 0 && y <= 7
}

// Code at (x, y), or -1 for empty / off-board.
fn cell(b, x, y) {
    if x < 0 || x > 7 || y < 0 || y > 7 { return -1; }
    b[y * 8 + x]
}

// Is (tx, ty) attacked by any piece of colour `by`? (Piece codes:
// pawn=by, knight=2+by, bishop=4+by, rook=6+by, queen=8+by, king=10+by.)
fn is_attacked(b, tx, ty, by) {
    // Pawns sit one rank "behind" the square, toward their own home.
    let pr = if by == 0 { ty - 1 } else { ty + 1 };
    if cell(b, tx - 1, pr) == by { return true; }
    if cell(b, tx + 1, pr) == by { return true; }

    let kn = 2 + by;
    let ko = [[1, 2], [2, 1], [-1, 2], [-2, 1], [1, -2], [2, -1], [-1, -2], [-2, -1]];
    for o in ko {
        if cell(b, tx + o[0], ty + o[1]) == kn { return true; }
    }

    let kg = 10 + by;
    for dy in -1..=1 {
        for dx in -1..=1 {
            if (dx != 0 || dy != 0) && cell(b, tx + dx, ty + dy) == kg { return true; }
        }
    }

    let rk = 6 + by;
    let bs = 4 + by;
    let qn = 8 + by;
    let orth = [[1, 0], [-1, 0], [0, 1], [0, -1]];
    for d in orth {
        let cx = tx + d[0];
        let cy = ty + d[1];
        while in_b(cx, cy) {
            let c = b[cy * 8 + cx];
            if c != -1 {
                if c == rk || c == qn { return true; }
                break;
            }
            cx += d[0];
            cy += d[1];
        }
    }
    let diag = [[1, 1], [1, -1], [-1, 1], [-1, -1]];
    for d in diag {
        let cx = tx + d[0];
        let cy = ty + d[1];
        while in_b(cx, cy) {
            let c = b[cy * 8 + cx];
            if c != -1 {
                if c == bs || c == qn { return true; }
                break;
            }
            cx += d[0];
            cy += d[1];
        }
    }
    false
}

fn king_in_check(b, color) {
    let kc = 10 + color;
    let kx = -1;
    let ky = -1;
    for i in 0..64 {
        if b[i] == kc { kx = i % 8; ky = i / 8; break; }
    }
    if kx == -1 { return false; } // no king on board (can't happen in FIDE)
    is_attacked(b, kx, ky, 1 - color)
}

// Squares strictly between (fx,fy) and (tx,ty) all empty (rook/bishop/queen).
fn path_clear_b(b, fx, fy, tx, ty) {
    let sx = sign_i(tx - fx);
    let sy = sign_i(ty - fy);
    let cx = fx + sx;
    let cy = fy + sy;
    while cx != tx || cy != ty {
        if b[cy * 8 + cx] != -1 { return false; }
        cx += sx;
        cy += sy;
    }
    true
}

// Movement-pattern legality (no check test, no castling). `capturing` =
// the destination is an enemy piece OR an en-passant capture.
fn pseudo_legal(b, kind, color, fx, fy, tx, ty, capturing) {
    let dx = tx - fx;
    let dy = ty - fy;
    let adx = abs_i(dx);
    let ady = abs_i(dy);

    if kind == 1 { return (adx == 1 && ady == 2) || (adx == 2 && ady == 1); } // knight
    if kind == 5 { return adx <= 1 && ady <= 1; } // king (one step; castling elsewhere)
    if kind == 3 { if dx != 0 && dy != 0 { return false; } return path_clear_b(b, fx, fy, tx, ty); }
    if kind == 2 { if adx != ady { return false; } return path_clear_b(b, fx, fy, tx, ty); }
    if kind == 4 {
        if dx != 0 && dy != 0 && adx != ady { return false; }
        return path_clear_b(b, fx, fy, tx, ty);
    }
    if kind == 0 { // pawn
        let dir = if color == 0 { 1 } else { -1 };
        let start = if color == 0 { 1 } else { 6 };
        if capturing { return adx == 1 && dy == dir; }
        if dx == 0 && dy == dir { return true; }
        if dx == 0 && dy == 2 * dir && fy == start { return b[(fy + dir) * 8 + fx] == -1; }
        return false;
    }
    false
}

// Apply (fx,fy)->(tx,ty) on a *clone* of `b` (args are by value in Rhai) and
// report whether `color`'s king is safe afterwards.
fn king_safe_after(b, color, fx, fy, tx, ty, is_ep) {
    let mc = b[fy * 8 + fx];
    if is_ep { b[fy * 8 + tx] = -1; } // the en-passant-captured pawn
    b[ty * 8 + tx] = mc;
    b[fy * 8 + fx] = -1;
    !king_in_check(b, color)
}

// King two squares toward a rook: all the castling conditions.
fn castle_legal(b, color, fx, fy, tx, castle) {
    let rank = if color == 0 { 0 } else { 7 };
    if fy != rank || fx != 4 { return false; }
    if king_in_check(b, color) { return false; } // may not castle out of check
    let opp = 1 - color;
    let rk = 6 + color;
    if tx == 6 { // king-side
        let bit = if color == 0 { 1 } else { 4 };
        if (castle & bit) == 0 { return false; }
        if b[rank * 8 + 5] != -1 || b[rank * 8 + 6] != -1 { return false; }
        if b[rank * 8 + 7] != rk { return false; }
        // King may not pass through or land on an attacked square.
        if is_attacked(b, 5, rank, opp) || is_attacked(b, 6, rank, opp) { return false; }
        return true;
    }
    if tx == 2 { // queen-side
        let bit = if color == 0 { 2 } else { 8 };
        if (castle & bit) == 0 { return false; }
        if b[rank * 8 + 1] != -1 || b[rank * 8 + 2] != -1 || b[rank * 8 + 3] != -1 { return false; }
        if b[rank * 8 + 0] != rk { return false; }
        if is_attacked(b, 3, rank, opp) || is_attacked(b, 2, rank, opp) { return false; }
        return true;
    }
    false
}

// The full legality test: movement pattern + castling + en passant + the
// king-safety filter. Shared by command validation and move generation.
fn move_is_legal(b, color, fx, fy, tx, ty, ep_x, ep_y, castle) {
    if !in_b(fx, fy) || !in_b(tx, ty) || (fx == tx && fy == ty) { return false; }
    let mc = b[fy * 8 + fx];
    if mc == -1 || mc % 2 != color { return false; }
    let occ = b[ty * 8 + tx];
    if occ != -1 && occ % 2 == color { return false; } // own piece on target
    let kind = mc / 2;
    if kind == 5 && abs_i(tx - fx) == 2 && ty == fy {
        return castle_legal(b, color, fx, fy, tx, castle);
    }
    let is_ep = kind == 0 && tx == ep_x && ty == ep_y && occ == -1;
    let capturing = occ != -1 || is_ep;
    if !pseudo_legal(b, kind, color, fx, fy, tx, ty, capturing) { return false; }
    king_safe_after(b, color, fx, fy, tx, ty, is_ep)
}

// Does `color` have any legal move? (Drives checkmate vs stalemate.) Scans
// every piece × every target, early-exiting on the first legal move — so the
// common "not mated" case is cheap.
fn has_any_legal_move(b, color, ep_x, ep_y, castle) {
    for i in 0..64 {
        let c = b[i];
        if c == -1 || c % 2 != color { continue; }
        let fx = i % 8;
        let fy = i / 8;
        for j in 0..64 {
            if move_is_legal(b, color, fx, fy, j % 8, j / 8, ep_x, ep_y, castle) {
                return true;
            }
        }
    }
    false
}

// Retire castling rights touched by a move. Rights mask: 1 white O-O,
// 2 white O-O-O, 4 black O-O, 8 black O-O-O. (`x & (15 ^ m)` clears bits m;
// Rhai has no bitwise-NOT, and the mask only ever spans the low 4 bits.)
fn update_castle(castle, kind, color, fx, fy, tx, ty) {
    if kind == 5 { // king moved → both of its colour's rights
        let m = if color == 0 { 3 } else { 12 };
        castle = castle & (15 ^ m);
    }
    if kind == 3 { // rook left a home corner
        if fx == 0 && fy == 0 { castle = castle & (15 ^ 2); }
        if fx == 7 && fy == 0 { castle = castle & (15 ^ 1); }
        if fx == 0 && fy == 7 { castle = castle & (15 ^ 8); }
        if fx == 7 && fy == 7 { castle = castle & (15 ^ 4); }
    }
    // A rook captured on its home corner (whatever took it).
    if tx == 0 && ty == 0 { castle = castle & (15 ^ 2); }
    if tx == 7 && ty == 0 { castle = castle & (15 ^ 1); }
    if tx == 0 && ty == 7 { castle = castle & (15 ^ 8); }
    if tx == 7 && ty == 7 { castle = castle & (15 ^ 4); }
    castle
}
