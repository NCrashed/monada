// chess — the M4 demo map (DESIGN.md §6). A *legal-move subset* of
// standard chess: piece movement, capture (= despawn), win-by-king-
// capture. Castling, en passant, promotion and check / mate detection are
// a later slice.
//
// The engine ships no genre (DESIGN.md §4): the *whole* game — rules,
// board, pieces, presentation, interaction — lives here. The host only
// renders the models we define, paints the voxels we paint, and forwards
// raw pointer events to `pointer(...)`.
//
// State model (decision A2 — gameplay state in the World):
//   archetype 0 `piece`  fields [kind, color]; position = board square
//                        (x, y, 0), x/y in 0..7.
//   archetype 1 `game`   fields [to_move, winner]; a singleton, parked
//                        off-board. Authoritative & hashed.
// `kind`:  0 pawn  1 knight  2 bishop  3 rook  4 queen  5 king
// `color`: 0 white 1 black
//
// Board maths run in native i64 (clean for L-moves / deltas / abs); the
// World stores `Fixed`, so we cross the boundary with `to_int(...)` on
// read and `fixed(...)` / `vec3(...)` on write.
//
// Render / input host-API (DESIGN.md §3.3):
//   model_box(w,h,d,color) -> id     define a procedural sprite model
//   entity_set_model(e, id)          bind an entity to a model
//   voxel_fill(x0,y0,z0,x1,y1,z1,c)  paint the board (sim coords)
//   camera_focus(point)              aim the camera (sim coords)
//   status(text)                     set the HUD line
//   highlight(e)/highlight_clear()/highlighted()   local selection
//   submit_command(verb, target, arg)             queue a move

fn init() {
    let piece = archetype(["kind", "color"]); // id 0
    let game = archetype(["to_move", "winner"]); // id 1

    // Paint the 8×8 board as voxels: a thin two-tone slab per square.
    for sy in 0..8 {
        for sx in 0..8 {
            let c = if (sx + sy) % 2 == 0 { 0x8060_4028 } else { 0x80B0_8858 };
            voxel_fill(sx, sy, 0, sx, sy, 1, c);
        }
    }

    // One sprite model per (kind, color): a box whose height reads the
    // piece's rank. `models[kind*2 + color]`.
    let heights = [10, 14, 16, 14, 22, 26];
    let white = 0x80F0_EAD8;
    let black = 0x8028_343C;
    let models = [];
    for k in 0..6 {
        models.push(model_box(10, 10, heights[k], white));
        models.push(model_box(10, 10, heights[k], black));
    }

    let g = entity_create(1);
    entity_set_position(g, vec3(fixed(-1), fixed(-1), fixed(0)));
    entity_set_field(g, "to_move", fixed(0)); // white first
    entity_set_field(g, "winner", fixed(-1)); // -1 = in progress

    // Standard setup. Back-rank kinds, files a..h.
    let back = [3, 1, 2, 4, 5, 2, 1, 3];
    for x in 0..8 {
        place(models, back[x], 0, x, 0); // white back rank
        place(models, 0, 0, x, 1); // white pawns
        place(models, 0, 1, x, 6); // black pawns
        place(models, back[x], 1, x, 7); // black back rank
    }

    camera_focus(vec3(ratio(7, 2), ratio(7, 2), fixed(0))); // board centre
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
        return;
    }
    if verb != 1 {
        return;
    }
    // Turn is enforced by the moving piece's colour, not the command's
    // player id (hotseat: one window plays both sides; the player↔colour
    // mapping for networked play lands in slice 4).
    let to_move = to_int(entity_field(g, "to_move"));
    if !is_piece(target) {
        status("illegal — no piece");
        return;
    }
    let mover_color = to_int(entity_field(target, "color"));
    if mover_color != to_move {
        status("illegal — not your turn");
        return;
    }

    let from = entity_position(target);
    let fx = to_int(from.x);
    let fy = to_int(from.y);
    let tx = to_int(arg.x);
    let ty = to_int(arg.y);

    if tx < 0 || tx > 7 || ty < 0 || ty > 7 || (tx == fx && ty == fy) {
        status("illegal move");
        return;
    }

    let occ = piece_at(tx, ty);
    if occ != -1 && to_int(entity_field(occ, "color")) == to_move {
        status("illegal — your own piece is there");
        return;
    }

    let kind = to_int(entity_field(target, "kind"));
    let capturing = occ != -1;
    if !legal_move(kind, to_move, fx, fy, tx, ty, capturing) {
        status("illegal move");
        return;
    }

    if capturing {
        let captured_king = to_int(entity_field(occ, "kind")) == 5;
        entity_despawn(occ);
        if captured_king {
            entity_set_position(target, vec3(fixed(tx), fixed(ty), fixed(0)));
            entity_set_field(g, "winner", fixed(to_move));
            status(side(to_move) + " wins by king capture");
            return;
        }
    }

    entity_set_position(target, vec3(fixed(tx), fixed(ty), fixed(0)));
    let next = 1 - to_move;
    entity_set_field(g, "to_move", fixed(next));
    let msg = if capturing { "capture! " } else { "" };
    status(msg + side(next) + " to move");
}

// --- helpers ---------------------------------------------------------

fn side(c) {
    if c == 0 { "white" } else { "black" }
}

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
    if kind == 3 { // rook
        if dx != 0 && dy != 0 { return false; }
        return path_clear(fx, fy, tx, ty);
    }
    if kind == 2 { // bishop
        if adx != ady { return false; }
        return path_clear(fx, fy, tx, ty);
    }
    if kind == 4 { // queen
        if dx != 0 && dy != 0 && adx != ady { return false; }
        return path_clear(fx, fy, tx, ty);
    }
    if kind == 0 { // pawn
        let dir = if color == 0 { 1 } else { -1 };
        let start = if color == 0 { 1 } else { 6 };
        if capturing {
            return adx == 1 && dy == dir;
        }
        if dx == 0 && dy == dir {
            return true;
        }
        if dx == 0 && dy == 2 * dir && fy == start {
            return piece_at(fx, fy + dir) == -1;
        }
        return false;
    }
    return false;
}
