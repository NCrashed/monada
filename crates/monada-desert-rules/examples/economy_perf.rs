//! What the economy costs per tick, on the real map
//! (docs/plans/desert-game.md §13a) — the D-4 measurement.
//!
//! The economy is the first loop that runs *every tick for the whole
//! match* rather than in bursts: harvesters search, re-target, re-plan
//! and cut continuously from minute one. So the question is not whether
//! one harvest is fast, it is what the steady state costs and how it
//! scales — D-9 wants a late game with dozens of them.
//!
//! ```text
//! cargo run --release -p monada-desert-rules --example economy_perf
//! ```

use std::sync::{Arc, Mutex};
use std::time::Instant;

use monada_desert_rules::economy::{Economy, Structure, CREDITS_PER_CELL};
use monada_desert_rules::harvest::Fleet;
use monada_desert_rules::mover::Router;
use monada_desert_rules::{material, Building, DesertParams, DesertRules, VEHICLE};
use monada_fixed::{Fixed, FixedVec3};
use monada_runtime::{
    shared_physics, shared_world, Host, NativeBackend, NullBridge, ScriptBackend, SharedBridge,
};

const TICK_MS: f64 = 1000.0 / 30.0;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Paint the real desert, and hand back a host to work it with.
fn desert() -> (NativeBackend, Vec<(i64, i64, i64)>) {
    let params = DesertParams {
        proving_ground: false,
        ..DesertParams::default()
    };
    let rules = DesertRules::new(params);
    let sites = rules.desert().spice_sites();
    let mut backend = NativeBackend::new(shared_world(0x0DE5_E271), Box::new(rules));
    let bridge: SharedBridge = Arc::new(Mutex::new(NullBridge));
    backend.set_bridge(&bridge);
    backend.set_volume(&shared_physics(30));
    let t = Instant::now();
    backend.on_init().expect("init");
    println!("mission load (paint + veins):    {:>8.1} ms", ms(t));
    (backend, sites)
}

fn seat(host: &dyn Host, x: i64, y: i64) -> FixedVec3 {
    let z = host.volume_top(x, y).map_or(0, |(z, _)| z) + 1;
    FixedVec3::new(
        Fixed::from_int(i32::try_from(x).unwrap_or(0)),
        Fixed::from_int(i32::try_from(y).unwrap_or(0)),
        Fixed::from_int(i32::try_from(z).unwrap_or(0)),
    )
}

fn main() {
    let (backend, sites) = desert();
    let host = backend.host();

    // Work the field nearest the first player's corner, which is what a
    // real opening does.
    // The nearest site with spice actually EXPOSED: the closest one to
    // the corner turns out to be a deep vein, which is worth nothing
    // until somebody digs it up (§7) and is therefore the wrong thing to
    // measure a harvest against.
    let (bx, by) = (38, 38);
    let mut open: Vec<(i64, i64, i64)> = sites
        .iter()
        .copied()
        .filter(|&s| disc_spice(host, s) > 0)
        .collect();
    open.sort_by_key(|(x, y, _)| (x - bx).abs() + (y - by).abs());
    let field = open.first().copied().expect("the map has surface spice");
    println!(
        "nearest worked field:            ({}, {}) r{}, {} cells exposed\n",
        field.0,
        field.1,
        field.2,
        disc_spice(host, field)
    );
    // A refinery a short haul away, so the loop includes the drive.
    let (sx, sy) = (field.0 + field.2 + 6, field.1);

    // The runs share one map, on purpose: each works a thinner field
    // than the last, which is the late game and the case where the
    // searches have the most ground to cover for the least ore.
    println!("harvesters   ticks    mean/tick    worst  mined   % of a tick");
    for crew in [1_usize, 8, 24] {
        let mut economy = Economy::new();
        economy.found(0, 0);
        let mut fleet = Fleet::new();
        let mut router = Router::new();

        let kind = host.archetype(&["owner"]);
        for i in 0..crew {
            let unit = host.entity_create(kind);
            let n = i64::try_from(i).unwrap_or(0);
            host.entity_set_position(unit, seat(host, sx + n % 6, sy + n / 6));
            fleet.enlist(unit, 0, (sx, sy));
        }

        let refinery = [Building {
            owner: 0,
            kind: Structure::Refinery,
        }];
        let ticks = 3_000;
        let before = disc_spice(host, field);
        let mut worst = 0.0_f64;
        let t = Instant::now();
        for _ in 0..ticks {
            let one = Instant::now();
            economy.begin_tick();
            economy.count(refinery.iter());
            fleet.run(host, &mut economy, &mut router, &sites, VEHICLE);
            economy.end_tick();
            worst = worst.max(ms(one));
        }
        let mean = ms(t) / f64::from(ticks);
        let mined = before - disc_spice(host, field);
        println!(
            "{crew:>10}  {ticks:>6}  {mean:>9.3} ms  {worst:>6.1} ms  {mined:>5}  {:>7.1}%",
            100.0 * mean / TICK_MS
        );
    }

    // What the field looks like afterwards: the economy is terrain, so
    // "how much did we mine" and "what does the map look like now" are
    // the same question.
    let left = disc_spice(host, field);
    println!(
        "\nspice left in that field:        {left} cells ({} credits)",
        left * CREDITS_PER_CELL
    );
    println!(
        "budget reference: one 30 Hz tick is {TICK_MS:.1} ms. The worst tick is the\n\
         one that matters — a spike past the budget is a stutter, and the first\n\
         plan of a match is always the expensive one (§4c)."
    );
}

fn disc_spice(host: &dyn Host, (cx, cy, r): (i64, i64, i64)) -> u32 {
    let mut n = 0;
    for y in (cy - r)..=(cy + r) {
        for x in (cx - r)..=(cx + r) {
            if (x - cx) * (x - cx) + (y - cy) * (y - cy) <= r * r
                && host.volume_top(x, y).is_some_and(|(_, m)| m == material::SPICE)
            {
                n += 1;
            }
        }
    }
    n
}
