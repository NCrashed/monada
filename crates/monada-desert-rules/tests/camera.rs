//! Which way the view moves when the player presses a key.
//!
//! This is the one piece of the camera worth testing, and it is worth
//! testing because it is invisible to every other kind of check: the
//! numbers are all finite, nothing panics, no state desyncs, and the
//! only symptom is that A and D feel swapped. It cost a play session to
//! notice, so it gets an assertion.
//!
//! The map is mirrored in x on the way to the screen (`world_of`), so a
//! screen-relative pan cannot use the camera's world-space basis
//! unchanged — and getting that wrong negates the x term of both axes at
//! once.

use monada_desert_rules::pan_in_view;
use monada_fixed::Fixed;

/// Assert a pan direction to a hundredth of a cell.
#[track_caller]
fn assert_pan(yaw: Fixed, keys: (i64, i64), want: (f64, f64), what: &str) {
    let (dx, dy) = pan_in_view(yaw, keys.0, keys.1);
    let (gx, gy) = (dx.to_f64(), dy.to_f64());
    assert!(
        (gx - want.0).abs() < 0.01 && (gy - want.1).abs() < 0.01,
        "{what}: got ({gx:.3}, {gy:.3}), wanted ({:.3}, {:.3})",
        want.0,
        want.1
    );
}

#[test]
fn at_yaw_zero_the_keys_line_up_with_the_view() {
    // Looking along world +x, which the mirror makes sim −x: "away from
    // the camera" is therefore sim −x, and screen-right is sim +y.
    let zero = Fixed::ZERO;
    assert_pan(zero, (1, 0), (0.0, 1.0), "D");
    assert_pan(zero, (-1, 0), (0.0, -1.0), "A");
    assert_pan(zero, (0, 1), (-1.0, 0.0), "W");
    assert_pan(zero, (0, -1), (1.0, 0.0), "S");
}

#[test]
fn a_quarter_turn_takes_the_keys_with_it() {
    let quarter = Fixed::from_f64(std::f64::consts::FRAC_PI_2);
    assert_pan(quarter, (1, 0), (1.0, 0.0), "D");
    assert_pan(quarter, (0, 1), (0.0, 1.0), "W");
}

#[test]
fn a_and_d_are_opposites_and_perpendicular_to_w() {
    // The property that actually broke: a mirror applied to one axis and
    // not the other leaves the pair no longer a rotation of the keypad.
    for eighth in 0..8 {
        let yaw = Fixed::from_f64(f64::from(eighth) * std::f64::consts::FRAC_PI_4);
        let (rx, ry) = pan_in_view(yaw, 1, 0);
        let (lx, ly) = pan_in_view(yaw, -1, 0);
        assert!(
            (rx + lx).to_f64().abs() < 0.01 && (ry + ly).to_f64().abs() < 0.01,
            "at yaw {eighth}/8 π, A is not the opposite of D"
        );

        let (ux, uy) = pan_in_view(yaw, 0, 1);
        let dot = (rx * ux + ry * uy).to_f64();
        assert!(
            dot.abs() < 0.01,
            "at yaw {eighth}/8 π, D and W are not perpendicular (dot {dot:.3})"
        );
        // And each is a unit step, so panning is the same speed whichever
        // way the camera happens to be pointing.
        let len = (rx.to_f64().powi(2) + ry.to_f64().powi(2)).sqrt();
        assert!((len - 1.0).abs() < 0.01, "D is {len:.3} cells, not one");
    }
}

#[test]
fn turning_the_camera_turns_the_keys_the_same_way() {
    // Panning right, then turning the camera a quarter and panning right
    // again, must trace a quarter turn on the ground — not a reflection,
    // which is what a mirrored axis produces and what "the directions
    // are not relative to the camera" feels like.
    let (ax, ay) = pan_in_view(Fixed::ZERO, 1, 0);
    let (bx, by) = pan_in_view(Fixed::from_f64(std::f64::consts::FRAC_PI_2), 1, 0);
    // Cross product of the two unit steps: +1 for a quarter turn one
    // way, −1 for the other. A reflection would give the wrong sign,
    // consistently, for every starting yaw.
    let cross = (ax * by - ay * bx).to_f64();
    assert!(
        (cross + 1.0).abs() < 0.01,
        "a quarter turn of the camera moved the keys by {cross:.3}, not a quarter turn"
    );
}
