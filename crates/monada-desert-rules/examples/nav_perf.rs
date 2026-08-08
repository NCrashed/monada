//! D-2's perf gate (docs/plans/desert-game.md §11): does three-dimensional
//! navigation hold up at the scale the game actually reaches?
//!
//! The plan's number is 200 movers. What that costs depends on three
//! things this measures separately, because they have different fixes:
//! extracting stands over a cold cache (one-time, per mission), planning
//! a route (per order), and re-planning after terraform (per edit). A
//! single "nav is fast" figure would hide which one bites.
//!
//! ```text
//! cargo run --release -p monada-desert-rules --example nav_perf
//! ```

use std::sync::{Arc, Mutex};
use std::time::Instant;

use monada_desert_rules::gen::{BEDROCK_Z, SKY_Z};
use monada_desert_rules::{material, DesertRules, INFANTRY, MAP_CELLS, VEHICLE};
use monada_runtime::{
    shared_physics, shared_world, Host, NativeBackend, NullBridge, ScriptBackend, SharedBridge,
    VolumeLimits,
};

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

fn main() {
    let world = shared_world(0x0DE5_E271);
    let mut backend = NativeBackend::new(world, Box::new(DesertRules::default()));
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    backend.set_volume(&shared_physics(30));

    let t = Instant::now();
    backend.on_init().expect("init");
    println!("mission load (paint {MAP_CELLS}²): {:>8.1} ms", ms(t));

    let host = backend.host();
    let ground = |x: i64, y: i64| {
        let mut z = SKY_Z;
        while z > BEDROCK_Z && !host.volume_solid(x, y, z) {
            z -= 1;
        }
        z
    };

    // Corner to corner: the longest route the map can ask for, and the
    // one an attack-move across the ridge produces.
    let (ax, ay) = (24, 24);
    let (bx, by) = (MAP_CELLS - 24, MAP_CELLS - 24);
    let (from, to) = ((ax, ay, ground(ax, ay)), (bx, by, ground(bx, by)));

    let t = Instant::now();
    let path = host.nav_path3(from, to, VEHICLE, &limits());
    let cold = ms(t);
    println!(
        "first crossing (cold cache):     {cold:>8.1} ms  ({} waypoints)",
        path.len()
    );

    let t = Instant::now();
    let path = host.nav_path3(from, to, VEHICLE, &limits());
    println!(
        "same crossing (warm cache):      {:>8.2} ms  ({} waypoints)",
        ms(t),
        path.len()
    );

    // How much of that is the search exploring the wrong side of the
    // ridge? The octile heuristic points straight at the goal, and the
    // ridge is a long barrier, so A* expands nearly everything before
    // rounding its end. A budget sweep shows the shape.
    println!("\nbudget    time      waypoints  reached");
    for budget in [1_000usize, 4_000, 10_000, 40_000] {
        let lim = VolumeLimits { budget, ..limits() };
        let t = Instant::now();
        let p = host.nav_path3(from, to, VEHICLE, &lim);
        println!(
            "{budget:>6}  {:>7.1} ms  {:>9}  {}",
            ms(t),
            p.len(),
            if p.last() == Some(&to) { "yes" } else { "no" }
        );
    }

    // 200 movers is the plan's figure. They do not all plan on the same
    // tick in a real match — an order arrives, a path is retained, the
    // next re-plan is cells away — so this is the pathological case: an
    // entire army ordered at once.
    let t = Instant::now();
    let mut total = 0usize;
    for i in 0..200i64 {
        let (sx, sy) = (24 + (i % 20) * 4, 24 + (i / 20) * 4);
        let p = host.nav_path3(
            (sx, sy, ground(sx, sy)),
            (bx, by, ground(bx, by)),
            VEHICLE,
            &limits(),
        );
        total += p.len();
    }
    let burst = ms(t);
    println!(
        "200 movers ordered at once:      {burst:>8.1} ms  ({:.2} ms each, {total} waypoints)",
        burst / 200.0
    );

    // Infantry sees a different graph (it tunnels and climbs further), so
    // its cache is cold even after the vehicle's is warm — worth knowing
    // before assuming one warm-up covers the army.
    let t = Instant::now();
    let foot = host.nav_path3(from, to, INFANTRY, &limits());
    println!(
        "first crossing, infantry:        {:>8.1} ms  ({} waypoints)",
        ms(t),
        foot.len()
    );

    // Terraform, then re-plan: the loop the three factions run all match.
    let t = Instant::now();
    for y in 100..140 {
        host.volume_fill(
            (120, y, BEDROCK_Z),
            (120, y, ground(120, y) + 9),
            material::ROCK,
            0x8078_6c60,
        );
    }
    let edit = ms(t);
    let t = Instant::now();
    let after = host.nav_path3(from, to, VEHICLE, &limits());
    println!(
        "raise a 40-cell wall:            {edit:>8.2} ms\n\
         re-plan through the change:      {:>8.1} ms  ({} waypoints)",
        ms(t),
        after.len()
    );

    println!(
        "\nbudget reference: one 30 Hz tick is 33.3 ms. A cold crossing is a\n\
         once-per-order cost, not a per-tick one — the path is retained."
    );
}
