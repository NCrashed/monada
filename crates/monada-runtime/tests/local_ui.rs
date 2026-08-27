//! What a compiled map may draw (`host_api` 37).
//!
//! The `ui_*` verbs are a HUD: labels, images and buttons a game puts over
//! its world. An authoring tool wants docked panels, a tree of regions, a
//! tile palette — and building those out of positioned rectangles is how
//! you end up writing a widget toolkit. So a compiled map's local layer
//! may draw straight into the host's egui pass.
//!
//! Legal because the local layer is one client's presentation and is
//! already outside the state hash: an `egui::Context` reaches no further
//! than a `status` line does. These tests hold that line — the UI runs,
//! and it changes nothing the simulation can see.

#![cfg(feature = "ui")]

use std::sync::{Arc, Mutex};

use monada_runtime::{
    egui, shared_terrain, shared_world, LocalHost, LocalLayer, LocalRules, NativeLocalBackend,
    NullBridge, SharedBridge,
};

/// A map that draws one window and counts its frames.
#[derive(Default)]
struct Panel {
    frames: usize,
    clicked: bool,
}

impl LocalRules for Panel {
    fn local_ui(&mut self, ctx: &egui::Context, host: &dyn LocalHost) {
        self.frames += 1;
        // The host is handed over too, so a panel can show what the world
        // says rather than a copy the layer keeps of it.
        let _ = host.entities();
        egui::Window::new("brushes").show(ctx, |ui| {
            if ui.button("raise").clicked() {
                self.clicked = true;
            }
            ui.label("radius");
        });
    }
}

/// A layer over a world nobody else is watching, with a bridge that
/// draws nothing -- the UI pass needs neither.
fn backend(rules: Box<dyn LocalRules>) -> NativeLocalBackend {
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    NativeLocalBackend::new(&shared_world(0), &bridge, &shared_terrain(), rules)
}

/// The pass reaches the map, and what the map draws comes out the other
/// side as geometry to paint.
#[test]
fn a_compiled_map_draws_into_the_hosts_egui_pass() {
    let mut layer = backend(Box::new(Panel::default()));
    let ctx = egui::Context::default();

    ctx.begin_pass(egui::RawInput::default());
    layer.on_local_ui(&ctx).expect("local_ui");
    let out = ctx.end_pass();

    assert!(
        !out.shapes.is_empty(),
        "the map's window produced nothing to paint",
    );
}

/// A scripted map has no `Context` to hold, so the default draws nothing
/// rather than failing — the `ui_*` verbs stay its whole surface.
#[test]
fn a_layer_that_draws_nothing_is_not_an_error() {
    struct Silent;
    impl LocalRules for Silent {}

    let mut layer = backend(Box::new(Silent));
    let ctx = egui::Context::default();
    ctx.begin_pass(egui::RawInput::default());
    assert!(layer.on_local_ui(&ctx).is_ok());
    let _ = ctx.end_pass();
}

/// The UI pass is a frame's worth of drawing, not a tick: it runs at
/// whatever rate the window redraws, and nothing it does may depend on
/// having run a fixed number of times. Pinned because a panel that
/// accumulated per pass would desync the moment two peers rendered at
/// different rates -- if it could reach the simulation at all, which is
/// the other half of why this is safe.
#[test]
fn the_ui_pass_is_per_frame_and_touches_no_simulation() {
    let world = shared_world(0);
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    let before = world.lock().expect("world").state_hash();

    let mut layer = NativeLocalBackend::new(
        &world,
        &bridge,
        &shared_terrain(),
        Box::new(Panel::default()),
    );
    let ctx = egui::Context::default();
    for _ in 0..5 {
        ctx.begin_pass(egui::RawInput::default());
        layer.on_local_ui(&ctx).expect("local_ui");
        let _ = ctx.end_pass();
    }

    assert_eq!(
        world.lock().expect("world").state_hash(),
        before,
        "drawing a panel moved the simulation",
    );
}
