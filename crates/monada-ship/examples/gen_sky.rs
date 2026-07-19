//! Generate the ship demo's starfield sky panorama — run once, commit the
//! output. Emits `map/assets/starfield.png`: a dark equirectangular panorama
//! speckled with deterministic stars (a plain LCG, no `rand`), sampled by view
//! direction behind the scene (`set_sky`). Swap in real art later — the host
//! loads any-resolution PNG.
//!
//! ```text
//! cargo run -p monada-ship --example gen_sky
//! ```

// A throwaway procedural-art tool: small casts + terse locals are fine here.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::cast_precision_loss,
    clippy::many_single_char_names
)]

use std::path::Path;

use image::{ImageBuffer, Rgba};

const W: u32 = 1024;
const H: u32 = 512;

/// One step of a cheap LCG (Numerical Recipes constants) — deterministic star
/// placement without a dependency.
fn lcg(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state
}

fn main() {
    let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(W, H);

    // Deep-space background: a faint vertical gradient (a touch bluer up top,
    // near-black at the horizon) so the panorama isn't a flat block.
    for y in 0..H {
        let t = y as f32 / H as f32; // 0 top .. 1 bottom
        let b = (10.0 * (1.0 - t) + 2.0) as u8;
        for x in 0..W {
            img.put_pixel(x, y, Rgba([b / 2, b / 2, b + 4, 255]));
        }
    }

    // Stars: ~1400 points, each a bright pixel with an occasional dimmer halo.
    let mut rng = 0x5EED_0000_C0FF_EE01u64;
    for _ in 0..1400 {
        let x = (lcg(&mut rng) % W as u64) as u32;
        let y = (lcg(&mut rng) % H as u64) as u32;
        let mag = 140 + (lcg(&mut rng) % 116) as u16; // 140..=255 brightness
        let tint = lcg(&mut rng) % 3; // 0 white, 1 warm, 2 cool
        let (r, g, b) = match tint {
            1 => (mag, (mag * 15 / 16), (mag * 13 / 16)),
            2 => ((mag * 13 / 16), (mag * 15 / 16), mag),
            _ => (mag, mag, mag),
        };
        let px = Rgba([r as u8, g as u8, b as u8, 255]);
        img.put_pixel(x, y, px);
        // A dim cross halo for the brighter stars, so they read at a distance.
        if mag > 220 {
            let halo = Rgba([(r / 3) as u8, (g / 3) as u8, (b / 3) as u8, 255]);
            if x > 0 {
                img.put_pixel(x - 1, y, halo);
            }
            if x + 1 < W {
                img.put_pixel(x + 1, y, halo);
            }
            if y > 0 {
                img.put_pixel(x, y - 1, halo);
            }
            if y + 1 < H {
                img.put_pixel(x, y + 1, halo);
            }
        }
    }

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("map/assets");
    std::fs::create_dir_all(&dir).expect("create assets dir");
    img.save(dir.join("starfield.png"))
        .expect("write starfield.png");
    eprintln!("wrote {}", dir.join("starfield.png").display());
}
