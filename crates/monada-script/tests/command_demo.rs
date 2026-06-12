//! The command path end to end (DESIGN.md §3.1, M3): the Rhai
//! `command_demo` map driven through a lockstep session over the
//! in-process loopback transport. Proves two clients fold an identical
//! command stream to identical state, and that the recorded replay
//! reproduces it bit-exactly.

use monada_fixed::{Fixed, FixedVec3};
use monada_net::{LockstepSession, LoopbackTransport, MatchInfo, SessionConfig, SimDriver};
use monada_script::{shared_world, RhaiBackend, RhaiDriver, ScriptBackend, COMMAND_DEMO_SCRIPT};
use monada_sim::{ArchetypeId, Command, EntityId, PlayerId};

const SEED: u64 = 0x4D4F_4E41_4441_5F30;
const UNIT: ArchetypeId = ArchetypeId(0);
const P0: PlayerId = PlayerId(0);
const P1: PlayerId = PlayerId(1);

fn vec(x: i32, y: i32) -> FixedVec3 {
    FixedVec3::new(Fixed::from_int(x), Fixed::from_int(y), Fixed::ZERO)
}

fn info() -> MatchInfo {
    MatchInfo {
        seed: SEED,
        map_hash: monada_net::map_hash(COMMAND_DEMO_SCRIPT),
        engine_version: "test".to_string(),
    }
}

fn session(
    local: PlayerId,
    transport: LoopbackTransport,
) -> LockstepSession<LoopbackTransport, RhaiDriver> {
    let driver = RhaiDriver::new(shared_world(SEED), COMMAND_DEMO_SCRIPT).expect("compile demo");
    LockstepSession::new(
        driver,
        transport,
        local,
        &[P0, P1],
        SessionConfig::default(),
        info(),
    )
}

/// P0's command for a given step: spawn two units early, then steer the
/// first one. P1 issues nothing. Deterministic, so both clients agree.
fn p0_commands(step: u64) -> Vec<Command> {
    match step {
        0 => vec![Command::at(1, vec(4, 0))], // spawn unit -> EntityId(0)
        1 => vec![Command::at(1, vec(-3, 2))], // spawn unit -> EntityId(1)
        5 => vec![Command::on(2, EntityId(0), vec(1, 1))], // steer unit 0
        9 => vec![Command::on(2, EntityId(1), vec(0, -1))], // steer unit 1
        _ => vec![],
    }
}

#[test]
fn two_clients_agree_and_replay_reproduces() {
    let (ta, tb) = LoopbackTransport::pair();
    let mut a = session(P0, ta);
    let mut b = session(P1, tb);

    for step in 0..120u64 {
        assert!(a.step(p0_commands(step)).expect("no desync"));
        assert!(b.step(vec![]).expect("no desync"));
        assert_eq!(
            a.driver().state_hash(),
            b.driver().state_hash(),
            "clients diverged at step {step}"
        );
    }

    // Both spawn commands took effect.
    assert_eq!(
        a.driver().world().lock().unwrap().count(UNIT),
        2,
        "both units spawned"
    );

    // The replay of A reproduces A's final state bit-exactly.
    let final_hash = a.driver().state_hash();
    let mut fresh = RhaiDriver::new(shared_world(SEED), COMMAND_DEMO_SCRIPT).unwrap();
    assert_eq!(a.replay().playback(&mut fresh), final_hash);
}

#[test]
fn map_with_no_command_handler_ignores_input() {
    // walk_circle defines no `command` fn; applying a command must be a
    // harmless no-op (not a script raise).
    use monada_script::WALK_CIRCLE_SCRIPT;

    let mut driver = RhaiDriver::new(shared_world(SEED), WALK_CIRCLE_SCRIPT).unwrap();
    let before = driver.state_hash();
    driver.apply_command(P0, &Command::at(1, vec(1, 1)));
    assert_eq!(driver.state_hash(), before, "command must not mutate state");
}

#[test]
fn typo_inside_command_handler_surfaces() {
    // A map *with* a `command` handler whose body calls a misspelled host
    // function must raise, not be silently swallowed as "no handler" — a
    // silent no-op could desync only the peer whose path hit the typo.
    const TYPO: &str = r#"
        fn init() { let _unit = archetype(["v"]); }
        fn command(player, verb, target, arg) {
            // `entity_set_postion` is a typo for `entity_set_position`.
            entity_set_postion(0, arg);
        }
    "#;
    let mut backend = RhaiBackend::new(shared_world(SEED));
    backend.load(TYPO).expect("compiles");
    backend.on_init().expect("init runs");
    let result = backend.on_command(P0, &Command::at(1, vec(1, 1)));
    assert!(
        result.is_err(),
        "a typo'd host call inside the handler must propagate, not no-op"
    );
}
