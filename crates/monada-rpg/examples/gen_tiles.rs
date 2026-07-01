//! Generate the RPG demo's placeholder per-cell tile textures — run once,
//! commit the output. Emits `map/assets/tiles/<name>.png`: 16×16 noise tiles
//! (the host resamples to the cell resolution) for grass variants, dirt, and
//! stone. Swap in real art later — the host loads any-resolution PNGs.
//!
//! ```text
//! cargo run -p monada-rpg --example gen_tiles
//! ```

// A throwaway procedural-art tool: small integer casts are fine here.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::path::Path;

use image::{ImageBuffer, Rgba};

const TILE: u32 = 16;

/// A cheap, deterministic per-pixel hash (no RNG) for brightness speckle.
fn hash(x: u32, y: u32, seed: u32) -> u32 {
    let mut h = x
        .wrapping_mul(73_856_093)
        ^ y.wrapping_mul(19_349_663)
        ^ seed.wrapping_mul(83_492_791);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^ (h >> 15)
}

/// One tile: `base` colour with a `var`-amplitude brightness speckle.
fn tile(base: [u8; 3], var: i32, seed: u32) -> ImageBuffer<Rgba<u8>, Vec<u8>> {
    let mut img = ImageBuffer::new(TILE, TILE);
    for y in 0..TILE {
        for x in 0..TILE {
            let n = (hash(x, y, seed) % (2 * var as u32 + 1)) as i32 - var;
            let c = |b: u8| (i32::from(b) + n).clamp(0, 255) as u8;
            img.put_pixel(x, y, Rgba([c(base[0]), c(base[1]), c(base[2]), 255]));
        }
    }
    img
}

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("map/assets/tiles");
    std::fs::create_dir_all(&dir).expect("create tiles dir");
    // (name, base RGB, speckle amplitude)
    let tiles = [
        ("grass1", [70, 150, 70], 22),
        ("grass2", [92, 168, 82], 20),
        ("grass3", [58, 132, 62], 26),
        ("dirt", [122, 92, 60], 24),
        ("stone", [128, 130, 138], 30),
    ];
    for (i, (name, base, var)) in tiles.iter().enumerate() {
        tile(*base, *var, i as u32 + 1)
            .save(dir.join(format!("{name}.png")))
            .expect("write tile png");
    }
    println!("wrote {} tiles under {}", tiles.len(), dir.display());
}
