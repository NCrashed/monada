//! Player commands — the only thing that travels the lockstep wire
//! (DESIGN.md §3.1, M3).
//!
//! A [`Command`] is **engine-opaque gameplay intent**: the simulation
//! core never interprets it. `verb` is a script-defined opcode and the
//! payload (`target` / `arg`) is whatever that opcode means to the map
//! script — the engine ships no genre (DESIGN.md §1, decision A2). The
//! net layer bundles, schedules, and serialises commands; the script's
//! `command` trigger is what actually mutates the [`World`](crate::World).
//!
//! Commands carry only sim-native, deterministically-serialisable types
//! (`u32`, [`EntityId`], [`FixedVec3`]) so a recorded input stream
//! replays bit-exactly on every platform.

use monada_fixed::FixedVec3;

use crate::EntityId;

/// Which player issued a command. Used for attribution and, critically,
/// for the **deterministic apply order** within a tick: a tick's
/// commands are applied sorted by `PlayerId` so two clients fold the
/// same inputs in the same order (DESIGN.md §3.1).
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct PlayerId(pub u32);

/// One unit of player input, scheduled for a specific tick by the
/// lockstep layer and interpreted by the map script.
///
/// The shape is deliberately a small fixed envelope, not a genre-typed
/// enum: `verb` selects a script-defined action, `target` names an
/// entity it acts on (or [`EntityId(0)`](EntityId) for "none"), and
/// `arg` carries a fixed-point vector argument (a clicked point, a
/// velocity, …). That is enough for the M3 demo and for chess (M4)
/// without baking any rule into the engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct Command {
    /// Script-defined opcode. The engine treats it as an opaque tag.
    pub verb: u32,
    /// Entity the command acts on; `EntityId(0)` conventionally means
    /// "no target" (e.g. a spawn command).
    pub target: EntityId,
    /// Fixed-point vector argument — a clicked position, a velocity, a
    /// direction. Meaning is the script's, not the engine's.
    pub arg: FixedVec3,
}

impl Command {
    /// A command with no target and a zero argument — the common case
    /// for verbs that need only the opcode.
    #[must_use]
    pub fn new(verb: u32) -> Command {
        Command {
            verb,
            target: EntityId(0),
            arg: FixedVec3::ZERO,
        }
    }

    /// A command carrying a vector argument (e.g. spawn-at-point).
    #[must_use]
    pub fn at(verb: u32, arg: FixedVec3) -> Command {
        Command {
            verb,
            target: EntityId(0),
            arg,
        }
    }

    /// A command targeting an entity with a vector argument (e.g.
    /// set-velocity).
    #[must_use]
    pub fn on(verb: u32, target: EntityId, arg: FixedVec3) -> Command {
        Command { verb, target, arg }
    }
}
