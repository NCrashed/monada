//! What crosses the wire (DESIGN.md §3.1, M3).
//!
//! Only two things ever travel between peers: **input bundles** (a
//! player's commands for a tick) and **checksums** (a player's state
//! hash at a tick, for desync detection). State never moves — every
//! client re-derives it from the shared seed + the input stream.

use monada_sim::{Command, PlayerId};
use serde::{Deserialize, Serialize};

/// One player's commands for one simulation tick. An *empty* bundle is
/// still meaningful and still sent: it tells peers "player P has nothing
/// for tick T", which is exactly what the tick barrier needs to release
/// (DESIGN.md §3.1, command-delay).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputBundle {
    pub tick: u64,
    pub player: PlayerId,
    pub commands: Vec<Command>,
}

/// A desync probe: one player's canonical state hash at a tick. Peers
/// compare these every `checksum_interval` ticks; a mismatch halts the
/// match (DESIGN.md §3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checksum {
    pub tick: u64,
    pub player: PlayerId,
    pub hash: u64,
}

/// Everything monada speaks on the wire. Inputs and checksums only —
/// never simulation state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetMessage {
    Input(InputBundle),
    Checksum(Checksum),
}

/// Encode a message for a real transport (QUIC, M3 Phase B) or a replay
/// file. The in-process loopback transport never calls this — it moves
/// values directly.
///
/// # Errors
/// Propagates a [`postcard`] serialisation failure (not expected for
/// well-formed messages).
pub fn encode(msg: &NetMessage) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(msg)
}

/// Decode a message produced by [`encode`].
///
/// # Errors
/// Returns a [`postcard`] error on malformed or truncated input.
pub fn decode(bytes: &[u8]) -> Result<NetMessage, postcard::Error> {
    postcard::from_bytes(bytes)
}
