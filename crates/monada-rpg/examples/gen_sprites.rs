//! Generate the RPG demo's placeholder actor art — run once, commit the
//! output. Emits `map/assets/char/<actor>/<state>/<side>.gif`: a 2-frame
//! animated GIF per compass side, for every animation state the script names.
//!
//! ```text
//! cargo run -p monada-rpg --example gen_sprites
//! ```
//!
//! These are deliberately crude billboards (a body + head + a bright
//! **facing nose** on the side the figure looks toward), so the 8-way
//! direction is unmistakable when calibrating `ACTOR_SIDES` in monada-host.
//! Swap in sculpted GIFs later — the layout convention is the contract.

// A throwaway procedural-art tool: small canvas casts and a catch-all facing
// arm are fine here.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::match_same_arms
)]

use std::path::Path;

const W: usize = 16;
const H: usize = 24;
/// The 8 compass sides (must match `monada_host`'s `ACTOR_SIDES` names).
const SIDES: [&str; 8] = [
    "south",
    "south-east",
    "east",
    "north-east",
    "north",
    "north-west",
    "west",
    "south-west",
];
const STATES: [&str; 6] = ["idle", "run", "attack", "dodge", "hurt", "death"];

/// Actor base body colour (RGB).
const ACTORS: [(&str, [u8; 3]); 2] = [
    ("hero", [225, 205, 140]),  // warm gold
    ("enemy", [120, 170, 110]), // sickly green
];

/// Attack-burst effects: a single `burst` state per "actor", coloured so the
/// player's swing (cyan) reads differently from a mob's (red). Bigger canvas
/// so the burst spans roughly the attack's hit area.
const FXW: usize = 40;
const FXH: usize = 40;
const FX: [(&str, [u8; 3]); 2] = [
    ("fx_player", [90, 220, 255]), // cyan
    ("fx_enemy", [255, 90, 70]),   // red
];

type Rgba = Vec<u8>;

/// Screen-space facing of a compass side (x right, y down).
fn facing(side: &str) -> (i32, i32) {
    match side {
        "north" => (0, -1),
        "south" => (0, 1),
        "east" => (1, 0),
        "west" => (-1, 0),
        "north-east" => (1, -1),
        "north-west" => (-1, -1),
        "south-east" => (1, 1),
        "south-west" => (-1, 1),
        _ => (0, 1),
    }
}

fn put(buf: &mut Rgba, x: i32, y: i32, c: [u8; 4]) {
    if x < 0 || y < 0 || x >= W as i32 || y >= H as i32 {
        return;
    }
    let i = (y as usize * W + x as usize) * 4;
    buf[i..i + 4].copy_from_slice(&c);
}

fn disc(buf: &mut Rgba, cx: i32, cy: i32, r: i32, c: [u8; 4]) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                put(buf, cx + dx, cy + dy, c);
            }
        }
    }
}

fn rect(buf: &mut Rgba, x0: i32, y0: i32, x1: i32, y1: i32, c: [u8; 4]) {
    for y in y0..=y1 {
        for x in x0..=x1 {
            put(buf, x, y, c);
        }
    }
}

/// One animation frame as RGBA (transparent background).
fn frame(base: [u8; 3], state: &str, side: &str, f: usize) -> Rgba {
    let mut buf = vec![0u8; W * H * 4]; // alpha 0 = transparent everywhere
    let [r, g, b] = base;
    let body = [r, g, b, 255];
    let dark = [r / 2, g / 2, b / 2, 255];

    // A per-state tint overlaid on the body so state changes read at a glance.
    let tint = match state {
        "attack" => Some([255, 80, 60, 255]),
        "dodge" => Some([90, 200, 255, 255]),
        "hurt" => Some([255, 255, 255, 255]),
        _ => None,
    };
    let body = tint.unwrap_or(body);

    if state == "death" {
        // A flat heap on the ground.
        rect(&mut buf, 3, H as i32 - 5, W as i32 - 4, H as i32 - 2, dark);
        return buf;
    }

    // Run / dodge bob the legs by a pixel on the off-frame.
    let bob = i32::from((state == "run" || state == "dodge") && f == 1);
    let cx = (W / 2) as i32;
    let top = 4 + bob;

    // Legs.
    rect(&mut buf, cx - 3, H as i32 - 6, cx - 1, H as i32 - 2 - bob, dark);
    rect(&mut buf, cx + 1, H as i32 - 6, cx + 3, H as i32 - 2 + bob, dark);
    // Torso.
    rect(&mut buf, cx - 4, top + 5, cx + 4, H as i32 - 6, body);
    // Head.
    disc(&mut buf, cx, top + 2, 3, body);

    // Facing "nose": a bright marker pushed toward the looking direction, so
    // the 8 sides are visually distinct.
    let (fx, fy) = facing(side);
    let nose = [255, 245, 200, 255];
    disc(&mut buf, cx + fx * 5, top + 2 + fy * 4, 1, nose);

    // Attack throws the marker further out (a lunge read).
    if state == "attack" {
        disc(&mut buf, cx + fx * 7, (top + 8) + fy * 5, 1, [255, 60, 40, 255]);
    }
    buf
}

fn fx_put(buf: &mut Rgba, x: i32, y: i32, c: [u8; 4]) {
    if x < 0 || y < 0 || x >= FXW as i32 || y >= FXH as i32 {
        return;
    }
    let i = (y as usize * FXW + x as usize) * 4;
    buf[i..i + 4].copy_from_slice(&c);
}

/// An expanding ring + cross-spikes burst on the FX canvas; `f` grows the
/// radius so the hit area reads as a quick flash.
fn fx_frame(color: [u8; 3], f: usize) -> Rgba {
    let mut buf = vec![0u8; FXW * FXH * 4];
    let (cx, cy) = ((FXW / 2) as i32, (FXH / 2) as i32);
    let r = 5 + f as i32 * 6; // 5, 11, 17
    let inner = (r - 3).max(0);
    let [cr, cg, cb] = color;
    let col = [cr, cg, cb, 255];
    for dy in -r..=r {
        for dx in -r..=r {
            let d2 = dx * dx + dy * dy;
            if d2 <= r * r && d2 >= inner * inner {
                fx_put(&mut buf, cx + dx, cy + dy, col); // annulus
            }
        }
    }
    for k in -r..=r {
        fx_put(&mut buf, cx + k, cy, col); // spikes
        fx_put(&mut buf, cx, cy + k, col);
    }
    buf
}

fn write_gif(path: &Path, mut frames: Vec<Rgba>, w: usize, h: usize, delay: u16) {
    let mut out = Vec::new();
    {
        let mut enc =
            gif::Encoder::new(&mut out, w as u16, h as u16, &[]).expect("gif encoder");
        enc.set_repeat(gif::Repeat::Infinite).expect("set repeat");
        for rgba in &mut frames {
            let mut frame = gif::Frame::from_rgba(w as u16, h as u16, rgba);
            frame.delay = delay; // centiseconds
            frame.dispose = gif::DisposalMethod::Background;
            enc.write_frame(&frame).expect("write frame");
        }
    }
    std::fs::write(path, out).expect("write gif");
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("map/assets/char");
    let mut count = 0;
    for (actor, base) in ACTORS {
        for state in STATES {
            let dir = root.join(actor).join(state);
            std::fs::create_dir_all(&dir).expect("create state dir");
            for side in SIDES {
                let frames = vec![frame(base, state, side, 0), frame(base, state, side, 1)];
                write_gif(&dir.join(format!("{side}.gif")), frames, W, H, 12);
                count += 1;
            }
        }
    }
    // Attack-burst effects: one `burst` state, 8 identical sides (radial).
    for (name, color) in FX {
        let dir = root.join(name).join("burst");
        std::fs::create_dir_all(&dir).expect("create fx dir");
        for side in SIDES {
            let frames = vec![fx_frame(color, 0), fx_frame(color, 1), fx_frame(color, 2)];
            write_gif(&dir.join(format!("{side}.gif")), frames, FXW, FXH, 4);
            count += 1;
        }
    }
    println!("wrote {count} actor GIFs under {}", root.display());
}
