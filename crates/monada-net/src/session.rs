//! The lockstep session loop (DESIGN.md §3.1, M3).
//!
//! [`LockstepSession`] is the one driver both the headless oracle
//! (loopback) and the host (loopback now, QUIC later) run. It ties
//! together the [`Lockstep`] scheduler, a [`Transport`], and a
//! [`SimDriver`], and adds the two things the scheduler deliberately
//! leaves out: **executing** released ticks against the world, and
//! **desync detection** via periodic checksum exchange.
//!
//! The session never sees rhai or floats — `SimDriver` is the seam the
//! script layer implements (`monada-script`'s `RhaiDriver`), so the
//! whole loop stays sim-only and the script<->sim wall holds.

use std::collections::BTreeMap;
use std::fmt;

use monada_sim::{Command, PlayerId};

use crate::lockstep::Lockstep;
use crate::replay::Replay;
use crate::transport::Transport;
use crate::wire::{Checksum, NetMessage};

/// What a session needs from the simulation: apply a command, advance one
/// tick, and produce a canonical state hash. Infallible by design —
/// scripts are fixed map assets, so a raise is a bug the implementor
/// surfaces (matching the host's `on_tick().expect(...)` stance), not a
/// data condition the protocol handles.
pub trait SimDriver {
    /// Apply one player's command to the world (the script's `command`
    /// trigger). Called for every command of a released tick, in
    /// canonical player order, before [`step`](Self::step).
    fn apply_command(&mut self, player: PlayerId, command: &Command);

    /// Advance the simulation exactly one tick (the script's `tick`
    /// trigger + the world tick bump).
    fn step(&mut self);

    /// Canonical state hash for desync detection (`World::state_hash`).
    fn state_hash(&self) -> u64;
}

/// Session tuning.
#[derive(Clone, Copy, Debug)]
pub struct SessionConfig {
    /// Command delay (lag) in ticks; passed to [`Lockstep`].
    pub command_delay: u64,
    /// Exchange + compare a state-hash checksum every this many ticks
    /// (DESIGN.md §3.1 suggests ~30).
    pub checksum_interval: u64,
}

impl Default for SessionConfig {
    fn default() -> SessionConfig {
        SessionConfig {
            command_delay: 2,
            checksum_interval: 30,
        }
    }
}

/// Replay identity metadata captured at session start.
#[derive(Clone, Debug)]
pub struct MatchInfo {
    pub seed: u64,
    /// SHA-256 map identity (`monada_format::hash`, DESIGN.md §3.4).
    pub map_hash: [u8; 32],
    pub engine_version: String,
}

/// The fatal lockstep condition: a peer's state hash disagrees with ours
/// at a tick. The match must halt and dump for diff (DESIGN.md §3.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Desync {
    pub tick: u64,
    pub peer: PlayerId,
    pub local: u64,
    pub remote: u64,
}

impl fmt::Display for Desync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "desync at tick {}: local hash {:#018x} != player {} hash {:#018x}",
            self.tick, self.local, self.peer.0, self.remote
        )
    }
}

impl std::error::Error for Desync {}

/// A running lockstep match for one local peer.
pub struct LockstepSession<T: Transport, D: SimDriver> {
    lockstep: Lockstep,
    transport: T,
    driver: D,
    checksum_interval: u64,
    /// Our own state hashes at checksum ticks, kept to compare against
    /// peer checksums that may arrive earlier or later. Pruned per tick by
    /// [`reconcile`](Self::reconcile) once that checkpoint is confirmed, so
    /// it stays bounded over a long match rather than leaking one entry per
    /// checksum tick.
    own_hashes: BTreeMap<u64, u64>,
    /// Buffered peer hashes awaiting (or already past) our own; pruned
    /// alongside `own_hashes` when a tick is confirmed.
    peer_hashes: BTreeMap<u64, BTreeMap<PlayerId, u64>>,
    /// Local commands awaiting submission. [`step`](Self::step) appends to
    /// this and drains it into the bundle for the next executed tick, so a
    /// command passed while stalled is held, never dropped.
    outbox: Vec<Command>,
    replay: Replay,
}

impl<T: Transport, D: SimDriver> LockstepSession<T, D> {
    /// Start a session. `players` is the full roster (sorted internally);
    /// `local` must be a member.
    #[must_use]
    pub fn new(
        driver: D,
        transport: T,
        local: PlayerId,
        players: &[PlayerId],
        config: SessionConfig,
        info: MatchInfo,
    ) -> LockstepSession<T, D> {
        let lockstep = Lockstep::new(local, players, config.command_delay);
        let replay = Replay::new(
            info.seed,
            info.map_hash,
            info.engine_version,
            config.command_delay,
        );
        LockstepSession {
            lockstep,
            transport,
            driver,
            checksum_interval: config.checksum_interval,
            own_hashes: BTreeMap::new(),
            peer_hashes: BTreeMap::new(),
            outbox: Vec::new(),
            replay,
        }
    }

    /// The next tick that will execute.
    #[must_use]
    pub fn tick(&self) -> u64 {
        self.lockstep.tick()
    }

    /// Borrow the simulation driver (e.g. to read positions for render).
    pub fn driver(&self) -> &D {
        &self.driver
    }

    /// Mutably borrow the driver for a **non-sim** side effect — e.g. the
    /// host running a map's pointer-gesture handler, which only touches
    /// local UI and queues a command (it must not advance the sim or apply
    /// a command directly; those go through [`step`](Self::step) so they
    /// stay in lockstep).
    pub fn driver_mut(&mut self) -> &mut D {
        &mut self.driver
    }

    /// The recorded replay so far.
    pub fn replay(&self) -> &Replay {
        &self.replay
    }

    /// Whether the underlying transport still considers the peer connected
    /// (see [`Transport::connected`]). A `false` here means the session can
    /// no longer make progress — distinct from a transient stall waiting on
    /// a slow but connected peer. No reconnect in M3, so it is terminal.
    pub fn connected(&self) -> bool {
        self.transport.connected()
    }

    /// Pump the network, then report whether the current tick can execute
    /// (every player's input is present) — e.g. for a UI that wants to show
    /// a "waiting for peer" state without advancing. Submitting commands no
    /// longer requires this: [`step`](Self::step) buffers what it is given.
    ///
    /// # Errors
    /// Returns [`Desync`] if a peer checksum disagrees with one we have
    /// already computed.
    pub fn poll_ready(&mut self) -> Result<bool, Desync> {
        self.pump()?;
        Ok(self.lockstep.ready())
    }

    /// Drain the transport into the scheduler and the checksum bookkeeping.
    ///
    /// # Errors
    /// Returns [`Desync`] if a peer checksum disagrees with one we have
    /// already computed.
    pub fn pump(&mut self) -> Result<(), Desync> {
        for msg in self.transport.poll() {
            match msg {
                NetMessage::Input(bundle) => self.lockstep.record(bundle),
                NetMessage::Checksum(c) => self.note_peer_checksum(c)?,
            }
        }
        Ok(())
    }

    /// Attempt to execute the current tick. Pumps the network first, then
    /// — if every player's input for the tick is present — schedules and
    /// broadcasts the local input for `tick + command_delay`, applies the
    /// released tick's commands in canonical order, steps the sim, and
    /// (on checksum ticks) exchanges and verifies a state hash.
    ///
    /// `local_commands` are the player's actions to submit. They are
    /// **buffered, never dropped**: if the session is stalled waiting on a
    /// peer (`Ok(false)`), they are held and go out on the next executed
    /// tick, scheduled `command_delay` ticks after it. So the natural
    /// `session.step(my_cmds)` loop is safe — a stalled frame loses no
    /// input. Returns `Ok(true)` if a tick executed, `Ok(false)` if stalled.
    ///
    /// # Errors
    /// Returns [`Desync`] on a checksum mismatch.
    pub fn step(&mut self, local_commands: Vec<Command>) -> Result<bool, Desync> {
        // Buffer first, so anything passed survives a stall.
        self.outbox.extend(local_commands);
        self.pump()?;
        if !self.lockstep.ready() {
            return Ok(false);
        }

        // Broadcast our buffered input for the future tick so peers can
        // advance. Draining only on a ready tick means the command's
        // execution tick is well-defined even if it waited out a stall.
        let bundle = self
            .lockstep
            .schedule_local(std::mem::take(&mut self.outbox));
        self.transport.send(NetMessage::Input(bundle));

        // Release and execute the current tick. `ready()` was true above,
        // so this is `Some`; the `else` is unreachable but keeps the
        // method panic-free.
        let Some((executed, commands)) = self.lockstep.take_step() else {
            return Ok(false);
        };
        for (player, list) in &commands {
            for command in list {
                self.driver.apply_command(*player, command);
            }
        }
        // Record the executed tick for the replay — non-empty bundles only,
        // plus the tick count so idle ticks are re-run on playback, not
        // stored.
        self.replay.record(executed, &commands);
        self.driver.step();

        // Periodic desync probe. Solo sessions have nobody to compare
        // against, so skip the bookkeeping entirely (and never accumulate).
        if self.lockstep.players().len() > 1 && executed % self.checksum_interval == 0 {
            let hash = self.driver.state_hash();
            self.own_hashes.insert(executed, hash);
            self.transport.send(NetMessage::Checksum(Checksum {
                tick: executed,
                player: self.lockstep.local(),
                hash,
            }));
            self.reconcile(executed)?;
        }

        Ok(true)
    }

    /// How many checksum ticks are still awaiting confirmation from a peer
    /// — the live size of the desync-detection bookkeeping. A diagnostic:
    /// it should stay small and bounded (pruned as checkpoints confirm),
    /// not grow with match length.
    #[must_use]
    pub fn outstanding_checksums(&self) -> usize {
        self.own_hashes.len() + self.peer_hashes.len()
    }

    /// Record a peer checksum, then reconcile its tick.
    fn note_peer_checksum(&mut self, c: Checksum) -> Result<(), Desync> {
        self.peer_hashes
            .entry(c.tick)
            .or_default()
            .insert(c.player, c.hash);
        self.reconcile(c.tick)
    }

    /// Compare our hash for `tick` against every peer hash we have for it.
    /// Errors on a mismatch; once *every* other player has reported and all
    /// agree, the checkpoint is confirmed and both maps drop it so they do
    /// not grow unbounded over a long match.
    fn reconcile(&mut self, tick: u64) -> Result<(), Desync> {
        let Some(&own) = self.own_hashes.get(&tick) else {
            return Ok(()); // can't verify until we've computed our own hash
        };
        let Some(peers) = self.peer_hashes.get(&tick) else {
            return Ok(()); // no peer reports yet
        };
        for (&peer, &remote) in peers {
            if remote != own {
                return Err(Desync {
                    tick,
                    peer,
                    local: own,
                    remote,
                });
            }
        }
        // Every *other* player must report for full confirmation.
        let others = self.lockstep.players().len() - 1;
        if peers.len() >= others {
            self.own_hashes.remove(&tick);
            self.peer_hashes.remove(&tick);
        }
        Ok(())
    }
}
