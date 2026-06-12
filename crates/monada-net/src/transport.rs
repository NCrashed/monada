//! The transport seam (DESIGN.md §3.1, §4).
//!
//! [`LockstepSession`](crate::LockstepSession) is generic over this
//! trait so the deterministic core is identical whether peers are
//! connected by an in-process queue (tests, the oracle, solo desync
//! testing — DESIGN.md line ~300) or by QUIC (M3 Phase B). The trait is
//! intentionally **sync and non-blocking**: `poll` drains whatever has
//! arrived and returns immediately, keeping the session loop free of
//! async.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::wire::NetMessage;

/// A non-blocking, message-oriented link to the other peer(s).
///
/// SCOPE (M3): this is a **single-peer, broadcast** seam — `send` has no
/// addressing, so it means "to the one peer", and [`QuicTransport`] is
/// one connection / one bi-stream. The [`Lockstep`](crate::Lockstep)
/// scheduler above it is already N-player (sorted roster, per-player
/// barrier), so supporting 3+ players is a *transport* change: this trait
/// will need peer-addressed or fan-out `send` (and `poll` to attribute
/// messages by peer) before then. Two-player is the deliberate v0 target
/// (DESIGN.md §6).
pub trait Transport {
    /// Queue a message for delivery to the peer (broadcast to the single
    /// peer in M3). Best-effort; ordering within a single `send`-stream is
    /// preserved by real transports.
    fn send(&mut self, msg: NetMessage);

    /// Drain and return every message that has arrived since the last
    /// poll. Never blocks; returns empty when nothing is pending.
    fn poll(&mut self) -> Vec<NetMessage>;

    /// Whether the link is still believed up. Lets a caller distinguish
    /// "waiting on a slow peer" (still connected, just no input yet) from
    /// "peer is gone". Defaults to `true` for transports that cannot
    /// disconnect (e.g. [`LoopbackTransport`]); a real transport (QUIC)
    /// flips it to `false` when its connection ends.
    ///
    /// KNOWN OMISSION (M3, dev/LAN): there is **no reconnect** and no
    /// graceful "peer left" protocol message — a `false` here is terminal
    /// for the session. Reconnect/spectators land in a later milestone
    /// (DESIGN.md §3.4); `send` failures are likewise swallowed, so this
    /// query is the only disconnect signal in the stack.
    fn connected(&self) -> bool {
        true
    }
}

/// Shared, single-thread message queue underpinning [`LoopbackTransport`].
type Queue = Rc<RefCell<VecDeque<NetMessage>>>;

/// An in-process, zero-latency transport: two endpoints with crossed
/// queues. Sending on one endpoint makes the message pollable on the
/// other. Single-threaded by construction (`Rc`/`RefCell`) — it backs
/// the oracle's two-session determinism gate and the protocol tests, not
/// real play.
pub struct LoopbackTransport {
    /// I push here; the peer polls it.
    outbox: Queue,
    /// I poll here; the peer pushes to it.
    inbox: Queue,
}

impl LoopbackTransport {
    /// Build a connected pair. Whatever endpoint A sends, endpoint B
    /// polls, and vice versa.
    #[must_use]
    pub fn pair() -> (LoopbackTransport, LoopbackTransport) {
        let a: Queue = Rc::new(RefCell::new(VecDeque::new()));
        let b: Queue = Rc::new(RefCell::new(VecDeque::new()));
        let end_a = LoopbackTransport {
            outbox: Rc::clone(&a),
            inbox: Rc::clone(&b),
        };
        let end_b = LoopbackTransport {
            outbox: b,
            inbox: a,
        };
        (end_a, end_b)
    }
}

impl Transport for LoopbackTransport {
    fn send(&mut self, msg: NetMessage) {
        self.outbox.borrow_mut().push_back(msg);
    }

    fn poll(&mut self) -> Vec<NetMessage> {
        self.inbox.borrow_mut().drain(..).collect()
    }
}
