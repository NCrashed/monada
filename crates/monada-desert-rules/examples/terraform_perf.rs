//! D-3's perf gate (docs/plans/desert-game.md §11): what does a tick of
//! terraforming actually cost, once the store, the settling automaton and
//! the navigation caches all get their share of it?
//!
//! The gate's number is 3000 cells a tick, under a millisecond in the
//! store. That is the easy part now the digest is incremental (§13a); the
//! interesting costs are the ones an edit *drags behind it* — every
//! `volume_fill` invalidates navigation stands and wakes a column of
//! sand, and those are paid whether the map asked for them or not. So
//! each is measured separately, cold and warm, rather than as one figure
//! that would hide which one bites.
//!
//! ```text
//! cargo run --release -p monada-desert-rules --example terraform_perf
//! ```

use std::sync::{Arc, Mutex};
use std::time::Instant;

use monada_desert_rules::gen::{BEDROCK_Z, SKY_Z};
use monada_desert_rules::terraform::{Terraform, Work, CELLS_PER_TICK};
use monada_desert_rules::{material, DesertRules, MAP_CELLS, VEHICLE};
use monada_runtime::{
    shared_physics, shared_world, Host, NativeBackend, NullBridge, ScriptBackend, SharedBridge,
    VolumeLimits,
};

/// One 30 Hz tick.
const TICK_MS: f64 = 1000.0 / 30.0;

fn limits() -> VolumeLimits {
    VolumeLimits {
        bounds: (0, 0, MAP_CELLS - 1, MAP_CELLS - 1),
        z_range: (BEDROCK_Z, SKY_Z),
        budget: 40_000,
    }
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn line(what: &str, took: f64) {
    println!(
        "{what:<34}{took:>8.2} ms   {:>5.1}% of a tick",
        100.0 * took / TICK_MS
    );
}

fn main() {
    let mut backend = NativeBackend::new(
        shared_world(0x0DE5_E271),
        Box::new(DesertRules::default()),
    );
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    backend.set_volume(&shared_physics(30));

    let t = Instant::now();
    backend.on_init().expect("init");
    println!("mission load (paint {MAP_CELLS}²): {:>8.1} ms\n", ms(t));

    let host = backend.host();

    // The floor: what the store alone charges for the gate's 3000 edits,
    // with nothing derived from the terrain yet to invalidate.
    let t = Instant::now();
    for i in 0..i64::from(CELLS_PER_TICK) {
        let (x, y) = (200 + i % 50, 8 + i / 50);
        host.volume_fill((x, y, 40), (x, y, 40), material::ROCK, 0x8078_6c60);
    }
    line("3000 raw edits, cold caches", ms(t));

    // The same 3000 cells as a real Surfling order, still cold. The
    // difference is the verb's own overhead: a column read per cell and
    // the settle bookkeeping the edit triggers.
    let mut work = Terraform::new();
    work.order((8, 8), (57, 67), Work::Raise { level: 44 });
    let t = Instant::now();
    let spent = work.run(host);
    line(
        &format!("a {}-cell Raise tick, cold", spent.total()),
        ms(t),
    );

    // Warm the navigation graph the way a match does — one army crossing
    // — and then terraform into it. This is the number that matters: an
    // edit's real price is the stands it throws away.
    let ground = |x: i64, y: i64| host.volume_top(x, y).map_or(BEDROCK_Z, |(z, _)| z);
    let (ax, ay) = (24, 24);
    let (bx, by) = (MAP_CELLS - 24, MAP_CELLS - 24);
    let t = Instant::now();
    let path = host.nav_path3(
        (ax, ay, ground(ax, ay)),
        (bx, by, ground(bx, by)),
        VEHICLE,
        &limits(),
    );
    println!(
        "\n(nav warmed by one crossing:      {:>7.1} ms, {} waypoints)\n",
        ms(t),
        path.len()
    );

    let mut work = Terraform::new();
    work.order((120, 8), (169, 67), Work::Raise { level: 44 });
    let t = Instant::now();
    let spent = work.run(host);
    line(
        &format!("a {}-cell Raise tick, warm nav", spent.total()),
        ms(t),
    );

    // Digging is two edits a cell and reads the spoil column each time,
    // so it is the expensive verb — and the one a Dweller runs all match.
    let mut work = Terraform::new();
    work.order((60, 100), (99, 139), Work::Dig { level: 20, spoil: (200, 200) });
    let t = Instant::now();
    let spent = work.run(host);
    line(&format!("a {}-cell Dig tick", spent.total()), ms(t));

    // A shell, and then the tick that pays for it. The blast is not
    // budgeted (an explosion does not queue); the slump that follows is.
    let t = Instant::now();
    let blasted = Terraform::crater(host, (150, 150), 12);
    line(&format!("one crater, {blasted} cells blasted"), ms(t));

    let mut idle = Terraform::new();
    let t = Instant::now();
    let spent = idle.run(host);
    line(
        &format!("the settle tick after it ({} cells)", spent.settled),
        ms(t),
    );

    let mut ticks = 1;
    let t = Instant::now();
    while idle.run(host).settled > 0 {
        ticks += 1;
        assert!(ticks < 10_000, "the crater never came to rest");
    }
    println!(
        "{:<34}{:>8.2} ms   over {ticks} ticks",
        "the crater settling out",
        ms(t)
    );

    // Quiet terrain is the common case by a wide margin: most ticks of
    // most matches have nothing falling anywhere. It has to be free.
    let t = Instant::now();
    for _ in 0..100 {
        idle.run(host);
    }
    line("100 quiet ticks", ms(t));

    println!(
        "\nbudget reference: one 30 Hz tick is {TICK_MS:.1} ms, and terraforming\n\
         is one of the things in it — the sim, the movers and the render\n\
         mirror want the rest. {CELLS_PER_TICK} cells is the §4e knob; it is one\n\
         number, and everything above moves with it."
    );
}
