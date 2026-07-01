//! Preview a marching-squares transition sheet by autotiling a small test
//! terrain (a blob of `high` over `low`, with a hole and a satellite) and
//! saving the blended floor as a PNG. Use it to sanity-check a new sheet's
//! corner convention *before* wiring it into a map.
//!
//! ```text
//! cargo run -p monada-rpg --example terrain_preview            # grass-dirt.png
//! cargo run -p monada-rpg --example terrain_preview stone-grass # any tiles/<name>.png
//! ```
//!
//! If the boundaries come out scrambled, the sheet uses a different 16-tile
//! layout than `monada_host::autotile::CONFIG_TO_TILE` — re-derive that table.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_lossless
)]

use monada_host::autotile::{Terrain, Transition};
use std::path::Path;

const CELL: usize = 16;
const W: i64 = 16;
const H: i64 = 16;

fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "grass-dirt".into());
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("map/assets/tiles");
    let sheet = image::open(root.join(format!("{name}.png")))
        .unwrap_or_else(|e| panic!("open {name}.png: {e}"))
        .to_rgba8();
    let (sw, sh) = sheet.dimensions();

    let mut terr = Terrain::new(CELL);
    // low = 0 (background), high = 1 (foreground / "on top").
    terr.add_transition(0, 1, Transition::from_sheet(&sheet, sw, sh, CELL));

    // A high-type disc with a low hole, plus a satellite — exercises every
    // edge, outer corner, inner corner, and diagonal.
    for cy in 0..H {
        for cx in 0..W {
            let (dx, dy) = (cx - 7, cy - 7);
            let disc = dx * dx + dy * dy <= 25;
            let hole = (cx - 7).abs() <= 1 && (cy - 7).abs() <= 1;
            let blob = (11..=13).contains(&cx) && (2..=4).contains(&cy);
            if (disc && !hole) || blob {
                terr.cells.insert((cx, cy), 1);
            }
        }
    }

    let mut img = image::RgbaImage::new((W as u32) * CELL as u32, (H as u32) * CELL as u32);
    terr.paint(0, 0, W - 1, H - 1, 0, |fx, fy, color| {
        if fx < 0 || fy < 0 || fx >= img.width() as i64 || fy >= img.height() as i64 {
            return;
        }
        let rgb = [
            ((color >> 16) & 0xff) as u8,
            ((color >> 8) & 0xff) as u8,
            (color & 0xff) as u8,
            255,
        ];
        img.put_pixel(fx as u32, fy as u32, image::Rgba(rgb));
    });

    let dst = std::env::temp_dir().join(format!("terrain-preview-{name}.png"));
    img.save(&dst).expect("save");
    println!("wrote {}", dst.display());
}
