//! What the shroud promises (docs/plans/desert-game.md §4f) — the D-7
//! gate: frame-rate-independent, and a no-op headless.
//!
//! Both halves fall out of where it lives rather than out of care taken
//! while writing it, and the tests are shaped to say so. Revealing is
//! idempotent and cumulative, so the number of frames a unit stood
//! somewhere cannot change the answer. And the whole thing is local:
//! painted through bridge verbs that do nothing without a bridge, and
//! unreachable from the simulation, so a headless peer both draws
//! nothing and hashes nothing extra.

use std::sync::{Arc, Mutex};

use monada_fixed::{Fixed, FixedVec3};
use monada_runtime::{
    shared_physics, shared_terrain, shared_world, Host, HostBridge, LocalHost, LocalRules,
    MapRules, NativeBackend, NativeLocalBackend, NullBridge, ScriptBackend, SharedBridge,
    WorldRead,
};

use monada_desert_rules::shroud::{Shroud, BASE_SIGHT, UNIT_SIGHT};
use monada_desert_rules::{material, SAND_REPOSE};

const GROUND: i64 = 20;

/// A plain to explore.
struct Plain;

impl MapRules for Plain {
    fn init(&mut self, host: &dyn Host) {
        host.volume_fill(
            (0, 0, 0),
            (
                monada_desert_rules::MAP_CELLS - 1,
                monada_desert_rules::MAP_CELLS - 1,
                GROUND,
            ),
            material::SAND,
            material::color(material::SAND),
        );
        host.granular_register(material::SAND, SAND_REPOSE);
    }
}

/// A bridge that counts overlay paints, so a test can see whether the
/// lid was drawn without a window.
#[derive(Default)]
struct Counter {
    grids: i64,
    fills: usize,
    clears: usize,
}

impl HostBridge for Counter {
    // The overlay verbs are the ones under test.
    fn grid_spawn_cubic(&mut self, _: i64, _: i64, _: i64) -> i64 {
        let id = self.grids;
        self.grids += 1;
        id
    }
    #[allow(clippy::too_many_arguments)]
    fn voxel_fill_in(&mut self, _: i64, _: i64, _: i64, _: i64, _: i64, _: i64, _: i64, _: i64) {
        self.fills += 1;
    }
    fn voxel_clear_in(&mut self, _: i64, _: i64, _: i64, _: i64) {
        self.clears += 1;
    }

    // The rest of the seam, ignored. A bridge has no default body for
    // the verbs a map cannot do without, which is what keeps a real one
    // honest and what makes a test one verbose.
    fn voxel_fill(&mut self, _: i64, _: i64, _: i64, _: i64, _: i64, _: i64, _: i64) {}
    fn voxel_set(&mut self, _: i64, _: i64, _: i64, _: i64) {}
    fn model_box(&mut self, _: i64, _: i64, _: i64, _: i64) -> i64 {
        0
    }
    #[allow(clippy::too_many_arguments)]
    fn model_box_sides(
        &mut self,
        _: i64,
        _: i64,
        _: i64,
        _: i64,
        _: i64,
        _: i64,
        _: i64,
        _: i64,
        _: i64,
    ) -> i64 {
        0
    }
    fn model_kv6(&mut self, _: &str, _: i64) -> i64 {
        0
    }
    fn entity_set_model(&mut self, _: i64, _: i64) {}
    fn highlight(&mut self, _: i64) {}
    fn highlight_clear(&mut self) {}
    fn highlighted(&self) -> i64 {
        -1
    }
    fn status(&mut self, _: &str) {}
    fn camera_focus(&mut self, _: FixedVec3) {}
    fn camera_angle(&mut self, _: Fixed, _: Fixed) {}
    fn submit_command(&mut self, _: i64, _: i64, _: FixedVec3) {}
    fn local_player(&self) -> Option<i64> {
        None
    }
    fn set_light(&mut self, _: FixedVec3, _: Fixed) {}
    fn set_sky(&mut self, _: &str) {}
}

/// The pieces a local layer needs, with a chosen bridge.
struct Client {
    sim: NativeBackend,
    local: NativeLocalBackend,
    bridge: Arc<Mutex<dyn HostBridge>>,
}

/// A do-nothing local layer: these tests drive [`Shroud`] directly, so
/// the rules' own frame logic is not what is under test.
struct Idle;
impl LocalRules for Idle {
    fn local_init(&mut self, _: &dyn LocalHost) {}
    fn local_frame(&mut self, _: &dyn LocalHost, _: Fixed) {}
}

fn client(bridge: Arc<Mutex<dyn HostBridge>>) -> Client {
    let world = shared_world(9);
    let terrain = shared_terrain();
    let phys = shared_physics(30);
    let shared: SharedBridge = bridge.clone();

    let mut sim = NativeBackend::new(world.clone(), Box::new(Plain));
    sim.set_bridge(&shared);
    sim.set_volume(&phys);
    sim.on_init().expect("init");

    let mut local = NativeLocalBackend::new(&world, &shared, &terrain, Box::new(Idle));
    local.set_volume(&phys);
    Client { sim, local, bridge }
}

// --- the gate -------------------------------------------------------------

#[test]
fn how_many_frames_pass_cannot_change_what_is_explored() {
    // The frame-rate half. A client at 240 Hz revealed the same march
    // eight times as often as one at 30 Hz; if that changed the map they
    // see, two players on the same stream would disagree about the world
    // because one of them has a better graphics card.
    let walk = |per_cell: usize| {
        let c = client(Arc::new(Mutex::new(NullBridge)));
        let host = c.local.host();
        let mut shroud = Shroud::new();
        shroud.lay(host);
        for step in 0..40 {
            for _ in 0..per_cell {
                shroud.reveal(host, (30 + step, 30), UNIT_SIGHT);
            }
        }
        shroud.explored()
    };
    assert_eq!(walk(1), walk(8), "the shroud depends on the frame rate");
    assert!(walk(1) > 0, "nothing was revealed at all");
}

#[test]
fn a_headless_peer_paints_nothing_and_hashes_nothing() {
    // The no-op half. `NullBridge` is what a dedicated server and the
    // oracle run with; the shroud must be inert there — and it must not
    // have touched the simulation on the way, which is a structural
    // fact (`Shroud` only ever sees a `LocalHost`) that this pins down
    // by digest.
    let c = client(Arc::new(Mutex::new(NullBridge)));
    let before = c.sim.host().volume_top(40, 40);

    let mut shroud = Shroud::new();
    shroud.lay(c.local.host());
    shroud.reveal(c.local.host(), (40, 40), BASE_SIGHT);

    assert!(shroud.explored() > 0, "the shroud did not track anything");
    assert_eq!(
        c.sim.host().volume_top(40, 40),
        before,
        "the shroud moved the ground"
    );
}

#[test]
fn with_a_bridge_the_lid_is_painted_and_peeled() {
    // The other side of the same coin: given somewhere to draw, it draws
    // — once for the lid, and one rub-out per newly explored cell.
    let counter = Arc::new(Mutex::new(Counter::default()));
    let c = client(counter.clone());
    let host = c.local.host();

    let mut shroud = Shroud::new();
    shroud.lay(host);
    let laid = counter.lock().expect("counter").fills;
    assert!(laid > 0, "the lid was never painted");
    // Runs of equal height share a call, so a flat plain is one fill a
    // row rather than one a column.
    assert!(
        laid <= usize::try_from(monada_desert_rules::MAP_CELLS).unwrap(),
        "a flat plain took {laid} fills, one per column"
    );

    shroud.reveal(host, (40, 40), 3);
    let peeled = counter.lock().expect("counter").clears;
    assert_eq!(
        peeled,
        usize::try_from(shroud.explored()).unwrap(),
        "one rub-out per explored cell"
    );

    // Revealing the same ground again costs nothing.
    shroud.reveal(host, (40, 40), 3);
    assert_eq!(counter.lock().expect("counter").clears, peeled);
    let _ = c.bridge;
}

// --- what a shroud is ------------------------------------------------------

#[test]
fn exploring_is_permanent_and_local() {
    // Dune II has no re-fog (§4f): ground you have seen stays seen when
    // the unit that saw it leaves. That is not a feature to implement —
    // it is what "no code un-reveals" means, and the test says so.
    let c = client(Arc::new(Mutex::new(NullBridge)));
    let host = c.local.host();
    let mut shroud = Shroud::new();
    shroud.lay(host);

    shroud.reveal(host, (30, 30), UNIT_SIGHT);
    assert!(shroud.seen(30, 30));
    let after_first = shroud.explored();

    // The unit drives away and reveals elsewhere; the old ground stays.
    shroud.reveal(host, (90, 90), UNIT_SIGHT);
    assert!(shroud.seen(30, 30), "explored ground was re-fogged");
    assert!(shroud.explored() > after_first);

    // And a second client is a second shroud: what one has explored says
    // nothing about the other, which is why none of this may be hashed.
    let other = Shroud::new();
    assert_eq!(other.explored(), 0);
    assert!(!other.seen(30, 30));
}

#[test]
fn the_shroud_is_a_disc_and_stops_at_the_edge_of_the_map() {
    let c = client(Arc::new(Mutex::new(NullBridge)));
    let host = c.local.host();
    let mut shroud = Shroud::new();
    shroud.lay(host);
    shroud.reveal(host, (50, 50), 5);

    assert!(shroud.seen(50, 50), "the centre is not explored");
    assert!(shroud.seen(55, 50), "the rim is not explored");
    assert!(!shroud.seen(56, 50), "the reveal reaches past its radius");
    assert!(!shroud.seen(54, 54), "the reveal is a square, not a disc");

    // Off the edge is not a crash and not an explored cell.
    shroud.reveal(host, (1, 1), 8);
    assert!(shroud.seen(0, 0));
    assert!(!shroud.seen(-1, 0));
}
