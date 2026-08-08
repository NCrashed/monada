//! Spice, silos and power (docs/plans/desert-game.md §7) — the D-4 slice.
//!
//! Three facts hold this together, and each of them is a design decision
//! rather than an accounting one.
//!
//! **Spice is terrain.** It is not a number on a tile: it is cells of
//! [`SPICE`](crate::material::SPICE) in the volume store, and harvesting
//! is `volume_clear`. A field visibly shrinks as it is worked, a deep
//! vein is worth exactly as much as your ability to remove what lies on
//! top of it, and a shell that craters a field scatters real material.
//! Nothing here needed a new engine verb — D-3 already built them all.
//!
//! **Spice does not regrow.** A mission is a finite resource race, so
//! every cell spent is spent, and the map is a clock.
//!
//! **Power pays for terraforming.** §4e's one knob is not a constant any
//! more: it is scaled by how much of a player's demand their generators
//! actually meet. A brownout does not stop the war, it makes the
//! engineers slow — which is what makes power the tightest resource in
//! the game rather than a box you tick once.

use std::collections::BTreeMap;

use crate::terraform::CELLS_PER_TICK;

/// A player, as the simulation knows one. Small and copyable: factions,
/// colours and names are D-9's business.
pub type PlayerNo = u8;

/// What one cell of spice is worth once it reaches a refinery.
pub const CREDITS_PER_CELL: u32 = 5;

/// How many cells a harvester holds — 700 credits, Dune II's number,
/// expressed in the unit this game actually moves.
pub const HARVESTER_CAPACITY: u32 = 140;

/// Cells cut per tick while harvesting, and cells unloaded per tick at a
/// refinery. At 30 Hz that is a bit under five seconds to fill and a bit
/// over one to empty: the loop is visible without being tedious.
pub const HARVEST_RATE: u32 = 1;
pub const UNLOAD_RATE: u32 = 4;

/// Credits a refinery and a silo each hold. Overflow is lost, which is
/// the whole reason to build the second silo.
pub const REFINERY_CAPACITY: u32 = 1_000;
pub const SILO_CAPACITY: u32 = 1_000;

/// Power a wind trap makes, and what the buildings that have one draw.
/// Faction generators differ in *placement*, not in output (§7).
pub const WINDTRAP_POWER: u32 = 100;
pub const REFINERY_DRAW: u32 = 30;
pub const SILO_DRAW: u32 = 5;

/// What a structure is, as the economy sees it. Everything else about a
/// building — its footprint, its models, what it can produce — is D-5's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Structure {
    Refinery,
    Silo,
    WindTrap,
}

impl Structure {
    /// Credits this one holds.
    #[must_use]
    pub fn capacity(self) -> u32 {
        match self {
            Structure::Refinery => REFINERY_CAPACITY,
            Structure::Silo => SILO_CAPACITY,
            Structure::WindTrap => 0,
        }
    }

    /// Power made and power drawn. A generator draws nothing; everything
    /// else draws and makes nothing, which is the whole of the scalar.
    #[must_use]
    pub fn power(self) -> (u32, u32) {
        match self {
            Structure::Refinery => (0, REFINERY_DRAW),
            Structure::Silo => (0, SILO_DRAW),
            Structure::WindTrap => (WINDTRAP_POWER, 0),
        }
    }
}

/// A structure standing on the map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Building {
    pub owner: PlayerNo,
    pub kind: Structure,
}

/// One player's economy.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Player {
    /// Banked credits, never above [`capacity`](Player::capacity).
    pub credits: u32,
    /// Storage, recomputed from the buildings that stand each tick.
    pub capacity: u32,
    /// Generated and demanded power, likewise recomputed.
    pub made: u32,
    pub used: u32,
    /// Credits that arrived with nowhere to go. Not a statistic — it is
    /// the feedback that tells a player to build a silo, so the HUD wants
    /// it and the AI reads it.
    pub spilled: u32,
}

impl Player {
    /// Bank `amount`, returning what would not fit and was lost.
    pub fn deposit(&mut self, amount: u32) -> u32 {
        let room = self.capacity.saturating_sub(self.credits);
        let taken = amount.min(room);
        self.credits += taken;
        let lost = amount - taken;
        self.spilled += lost;
        lost
    }

    /// Spend, if it can be afforded. All-or-nothing: a half-paid building
    /// is not a thing.
    pub fn charge(&mut self, amount: u32) -> bool {
        if self.credits < amount {
            return false;
        }
        self.credits -= amount;
        true
    }

    /// How much of this player's power demand is actually met, as a
    /// percentage capped at 100. A player drawing nothing is fully
    /// powered by definition — an empty base is not in a brownout.
    #[must_use]
    pub fn satisfaction(&self) -> u32 {
        if self.used == 0 {
            return 100;
        }
        (self.made * 100 / self.used).min(100)
    }

    /// This player's terraform allowance for the tick (§4e): the engine's
    /// ceiling, scaled by the power they are actually getting.
    ///
    /// Brownouts slow the engineers rather than stopping them — a floor
    /// of one cell, because a rule that can reach exactly zero is a rule
    /// that can deadlock a mission whose objective is to dig.
    #[must_use]
    pub fn allowance(&self) -> u32 {
        (CELLS_PER_TICK * self.satisfaction() / 100).max(1)
    }
}

/// Every player's economy. Hashed, snapshotted state.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Economy {
    players: BTreeMap<PlayerNo, Player>,
}

impl Economy {
    #[must_use]
    pub fn new() -> Economy {
        Economy::default()
    }

    /// A player's economy, created empty on first mention.
    pub fn player(&mut self, who: PlayerNo) -> &mut Player {
        self.players.entry(who).or_default()
    }

    /// A player's economy for reading, without conjuring one.
    #[must_use]
    pub fn get(&self, who: PlayerNo) -> Option<&Player> {
        self.players.get(&who)
    }

    /// Start a player with a treasury and nothing else.
    pub fn found(&mut self, who: PlayerNo, credits: u32) {
        let p = self.player(who);
        p.capacity = REFINERY_CAPACITY;
        p.credits = credits.min(p.capacity);
    }

    /// Clear the derived totals before a tick recounts the buildings.
    ///
    /// Recounted rather than adjusted on build and death: an economy that
    /// accumulates deltas drifts the first time a structure dies in a way
    /// nobody wrote a handler for, and a full recount of a base is a few
    /// dozen entities.
    pub fn begin_tick(&mut self) {
        for p in self.players.values_mut() {
            p.capacity = 0;
            p.made = 0;
            p.used = 0;
        }
    }

    /// Recount storage and power from the buildings that are standing.
    pub fn count<'a>(&mut self, buildings: impl Iterator<Item = &'a Building>) {
        for b in buildings {
            let (made, used) = b.kind.power();
            let p = self.player(b.owner);
            p.capacity += b.kind.capacity();
            p.made += made;
            p.used += used;
        }
    }

    /// Trim every player's bank to the storage that survived the tick.
    /// A silo lost to a shell spills what it was holding.
    pub fn end_tick(&mut self) {
        for p in self.players.values_mut() {
            if p.credits > p.capacity {
                p.spilled += p.credits - p.capacity;
                p.credits = p.capacity;
            }
        }
    }
}
