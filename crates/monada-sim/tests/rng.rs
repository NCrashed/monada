//! Properties the lockstep/scripting layers rely on (DESIGN.md §3.1):
//! the RNG is reproducible, and forked sub-streams are pure functions
//! of `(seed, stream_id)` — independent of how far the parent advanced.

use monada_sim::DeterministicRng;

/// First `n` outputs of a stream (by value, leaving the original).
fn take(rng: &DeterministicRng, n: usize) -> Vec<u64> {
    let mut r = rng.clone();
    (0..n).map(|_| r.next_u64()).collect()
}

#[test]
fn seeding_is_reproducible() {
    let a = DeterministicRng::seed_from_u64(12345);
    let b = DeterministicRng::seed_from_u64(12345);
    assert_eq!(take(&a, 64), take(&b, 64));
    // Different seeds diverge immediately.
    let c = DeterministicRng::seed_from_u64(12346);
    assert_ne!(take(&a, 4), take(&c, 4));
}

#[test]
fn fork_is_pure_in_seed_and_stream_id() {
    let parent = DeterministicRng::seed_from_u64(0xDEAD_BEEF);
    assert_eq!(take(&parent.fork(7), 64), take(&parent.fork(7), 64));
    // A different stream id yields a different stream.
    assert_ne!(take(&parent.fork(7), 8), take(&parent.fork(8), 8));
}

#[test]
fn fork_is_independent_of_parent_advancement() {
    // The order-independence guarantee: forking stream 42 before vs.
    // after pulling from the parent must give the same sub-stream.
    let parent = DeterministicRng::seed_from_u64(99);
    let early = parent.fork(42);

    let mut advanced = parent.clone();
    for _ in 0..1000 {
        advanced.next_u64();
    }
    let late = advanced.fork(42);

    assert_eq!(take(&early, 64), take(&late, 64));
}

#[test]
fn adjacent_streams_are_not_correlated() {
    // Neighbouring (seed, stream_id) pairs must not produce streams
    // that march together. Check that consecutive stream ids differ in
    // their first output, and that consecutive seeds do too.
    let parent = DeterministicRng::seed_from_u64(0x0102_0304_0506_0708);
    for id in 0..256u64 {
        let a = parent.fork(id).next_u64();
        let b = parent.fork(id + 1).next_u64();
        assert_ne!(a, b, "streams {id} and {} collided on first draw", id + 1);
    }
    for seed in 0..256u64 {
        let a = DeterministicRng::seed_from_u64(seed).fork(0).next_u64();
        let b = DeterministicRng::seed_from_u64(seed + 1).fork(0).next_u64();
        assert_ne!(a, b, "seeds {seed} and {} collided on first draw", seed + 1);
    }
}
