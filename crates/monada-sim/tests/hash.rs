//! `StateHasher` primitive behaviour — the canonical-hash building
//! block desync detection and archive hashing rely on.

use monada_sim::StateHasher;

#[test]
fn write_bytes_equals_byte_by_byte() {
    let blob = b"chess.monada\x00\x01\x02manifest";
    let mut a = StateHasher::new();
    a.write_bytes(blob);

    let mut b = StateHasher::new();
    for &byte in blob {
        b.write_u8(byte);
    }
    assert_eq!(a.finish(), b.finish());
}

#[test]
fn hashing_is_order_sensitive() {
    // Field order is part of the canonical form: swapping inputs must
    // change the digest.
    let mut a = StateHasher::new();
    a.write_u64(1);
    a.write_u64(2);

    let mut b = StateHasher::new();
    b.write_u64(2);
    b.write_u64(1);

    assert_ne!(a.finish(), b.finish());
    // Empty vs. non-empty also differ.
    assert_ne!(StateHasher::new().finish(), a.finish());
}
