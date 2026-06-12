//! The lockstep protocol state (DESIGN.md §3.1, M3).
//!
//! Pure input scheduling — no transport, no world, no hashing. It tracks
//! which commands are scheduled for which tick by which player, and
//! decides when a tick is safe to execute: a tick releases only once
//! *every* player's bundle for it has arrived (the **tick barrier**).
//!
//! **Command delay** is the buffer that keeps the barrier from
//! deadlocking. A command issued while executing tick `T` is scheduled
//! for tick `T + command_delay`; the delay is the number of ticks of
//! input always in flight, so each peer's bundle for a tick arrives
//! before that tick needs to execute (AoE2 used 2–6 — DESIGN.md §3.1).
//! Ticks `0..command_delay` are *warmup*: no command can target them, so
//! every client seeds them empty and identically, and they execute
//! immediately.

use std::collections::BTreeMap;

use monada_sim::{Command, PlayerId};

use crate::wire::InputBundle;

/// Per-tick, per-player scheduled commands. `BTreeMap` (never a hash
/// map) so the apply order is the canonical sorted-by-`PlayerId` walk on
/// every machine.
type Schedule = BTreeMap<u64, BTreeMap<PlayerId, Vec<Command>>>;

/// A released tick's commands in canonical player order — what
/// [`Lockstep::take_step`] hands back for execution.
pub type StepCommands = Vec<(PlayerId, Vec<Command>)>;

/// Lockstep input scheduler for one local peer.
pub struct Lockstep {
    /// Next tick to execute.
    tick: u64,
    /// Command delay (lag) in ticks. Must be `>= 1` or the barrier
    /// deadlocks at tick 0 (no warmup to cover it).
    command_delay: u64,
    /// This peer's id.
    local: PlayerId,
    /// All participating players, sorted — the deterministic apply order.
    players: Vec<PlayerId>,
    /// `tick -> player -> commands`. A player key present for a tick
    /// means that player's bundle has arrived (possibly empty).
    schedule: Schedule,
}

impl Lockstep {
    /// Create a scheduler. `players` need not be sorted; it is sorted and
    /// deduplicated here. Warmup ticks `0..command_delay` are pre-seeded
    /// empty for every player.
    ///
    /// # Panics
    /// Panics if `command_delay == 0` (would deadlock at tick 0) or if
    /// `local` is not in `players`.
    #[must_use]
    pub fn new(local: PlayerId, players: &[PlayerId], command_delay: u64) -> Lockstep {
        assert!(command_delay >= 1, "command_delay must be >= 1");
        let mut players = players.to_vec();
        players.sort_unstable();
        players.dedup();
        assert!(players.contains(&local), "local player not in roster");

        let mut schedule: Schedule = BTreeMap::new();
        for warmup in 0..command_delay {
            let slot = schedule.entry(warmup).or_default();
            for &p in &players {
                slot.insert(p, Vec::new());
            }
        }

        Lockstep {
            tick: 0,
            command_delay,
            local,
            players,
            schedule,
        }
    }

    /// The next tick that will execute.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// The command delay this session was configured with.
    #[must_use]
    pub fn command_delay(&self) -> u64 {
        self.command_delay
    }

    /// This peer's player id.
    #[must_use]
    pub fn local(&self) -> PlayerId {
        self.local
    }

    /// The sorted player roster.
    #[must_use]
    pub fn players(&self) -> &[PlayerId] {
        &self.players
    }

    /// Schedule the local player's `commands` for `tick + command_delay`,
    /// record them locally, and return the bundle to broadcast to peers.
    /// Call exactly once per executed tick (with an empty vec when the
    /// player did nothing) so every future tick gets exactly one local
    /// bundle.
    pub fn schedule_local(&mut self, commands: Vec<Command>) -> InputBundle {
        let target = self.tick + self.command_delay;
        let bundle = InputBundle {
            tick: target,
            player: self.local,
            commands,
        };
        self.record(bundle.clone());
        bundle
    }

    /// Record a bundle (local or received from a peer) into the schedule.
    /// A peer that re-sends a bundle for an already-known tick is ignored
    /// (first write wins) so duplicate delivery is harmless.
    pub fn record(&mut self, bundle: InputBundle) {
        // Past ticks are immutable once executed; drop late/duplicate
        // arrivals.
        if bundle.tick < self.tick {
            return;
        }
        self.schedule
            .entry(bundle.tick)
            .or_default()
            .entry(bundle.player)
            .or_insert(bundle.commands);
    }

    /// Whether the current tick can execute: every player's bundle for it
    /// is present.
    #[must_use]
    pub fn ready(&self) -> bool {
        match self.schedule.get(&self.tick) {
            Some(slot) => self.players.iter().all(|p| slot.contains_key(p)),
            None => false,
        }
    }

    /// Take the current tick's commands in deterministic order
    /// (`(PlayerId, commands)` sorted by player) and advance to the next
    /// tick. Returns `None` if the tick is not [`ready`](Self::ready).
    ///
    /// The returned tick number is the one just released (the value
    /// [`tick`](Self::tick) had *before* this call).
    pub fn take_step(&mut self) -> Option<(u64, StepCommands)> {
        let executed = self.tick;
        let slot = self.schedule.remove(&executed)?;
        // The slot exists but might be missing a player's bundle — only
        // release when every player is present.
        if !self.players.iter().all(|p| slot.contains_key(p)) {
            // Not ready: put it back untouched.
            self.schedule.insert(executed, slot);
            return None;
        }
        // BTreeMap iterates sorted by key (PlayerId) — the canonical order.
        let commands: StepCommands = slot.into_iter().collect();
        self.tick += 1;
        Some((executed, commands))
    }
}
