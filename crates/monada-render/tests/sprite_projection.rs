//! Regression guard for the M1 sprite camera bug: the orbit camera's
//! basis must be **right-handed** (`right × down = forward`), or roxlap's
//! sprite frustum cull rejects every sprite (the grid opticast tolerates
//! a left-handed basis, so the bug shows only as "no sprites"). Drives
//! roxlap's CPU `draw_sprite` headlessly and checks pixels land, centred.
#![allow(clippy::similar_names)] // minx/maxx/miny/maxy bbox bounds

use glam::DVec3;
use monada_render::OrbitCamera;
use roxlap_core::camera_math;
use roxlap_core::opticast::OpticastSettings;
use roxlap_core::sprite::{draw_sprite, DrawTarget, SpriteLighting};
use roxlap_formats::kv6::Kv6;
use roxlap_formats::sprite::Sprite;

/// Draw one cube sprite at world `pos` and return `(pixels_written,
/// bbox_center)`.
fn draw_at(pos: [f32; 3]) -> (u32, (u32, u32)) {
    let (w, h) = (960u32, 720u32);
    let settings = OpticastSettings::for_oracle_framebuffer(w, h);
    let cam = OrbitCamera::framing(DVec3::new(0.0, 0.0, 100.0)).to_roxlap();
    let cam_state = camera_math::derive(&cam, w, h, settings.hx, settings.hy, settings.hz);

    let sprite = Sprite::axis_aligned(Kv6::solid_cube(10, 0x80FF_6B35), pos);
    let lighting = SpriteLighting::default_oracle();

    let mut fb = vec![0u32; (w * h) as usize];
    let mut zb = vec![f32::INFINITY; (w * h) as usize];
    let written = {
        let mut target = DrawTarget::new(&mut fb, &mut zb, w as usize, w, h);
        draw_sprite(&mut target, &cam_state, &settings, &lighting, &sprite)
    };

    let (mut minx, mut miny, mut maxx, mut maxy) = (w, h, 0u32, 0u32);
    for y in 0..h {
        for x in 0..w {
            if fb[(y * w + x) as usize] != 0 {
                minx = minx.min(x);
                maxx = maxx.max(x);
                miny = miny.min(y);
                maxy = maxy.max(y);
            }
        }
    }
    let center = if written == 0 {
        (0, 0)
    } else {
        ((minx + maxx) / 2, (miny + maxy) / 2)
    };
    (written, center)
}

#[test]
fn sprite_in_front_of_camera_draws() {
    // A cube lifted above the board centre must render (handedness OK)
    // and land near the middle of the 960x720 frame.
    let (written, (cx, cy)) = draw_at([0.0, 0.0, 76.0]);
    assert!(
        written > 0,
        "centred sprite drew nothing — camera handedness regressed"
    );
    assert!(
        (380..=580).contains(&cx) && (260..=460).contains(&cy),
        "sprite landed at ({cx},{cy}); expected near screen centre (480,360)"
    );
}
