//! Base building in three dimensions (docs/plans/desert-game.md §7) —
//! the D-5 slice.
//!
//! Dune II asked two questions of a build site: is the tile free, and
//! does it touch something of yours. A volumetric map asks three more,
//! and they are the interesting ones.
//!
//! **Is it level enough, and can it be made level?** A dune sea has
//! relief everywhere, so almost nothing is flat. A yard grades its own
//! pad — up to [`MAX_GRADE`] cells of spread — and anything steeper has
//! to be terraformed first, which is exactly the Surfling doctrine of
//! platform-first construction (§6a) expressed as a placement rule
//! rather than as flavour text.
//!
//! **Will the ground bear it?** Rock, packed fill and glass hold. Raw
//! sand does not: a structure on sand still goes up, and then slowly
//! comes apart. That is Dune II's concrete rule made literal — the
//! descendant of "build on concrete or your building degrades", except
//! the concrete is a material in the world that a faction manufactures.
//!
//! **How deep is it?** A pad well below the ground around it is *buried*
//! and a pad well above is *elevated*. Nothing in this slice reads that
//! yet; D-6 does, when direct fire starts caring whether a berm is in
//! the way and a factory in a pit can only be hit from above.

use std::collections::BTreeMap;

use monada_runtime::{Host, MaterialId, WorldRead};
use monada_sim::EntityId;

use crate::economy::{Economy, PlayerNo, Structure};
use crate::material;

/// The most a pad may be out of level and still be graded flat by the
/// builders. Two cells: a vehicle's climb, and about the relief of an
/// ordinary dune.
pub const MAX_GRADE: i64 = 2;

/// Clear air a structure needs above its pad.
pub const BUILD_CLEARANCE: i64 = 4;

/// How close a new structure must be to one of yours. Dune II's
/// adjacency rule, in cells rather than tiles.
pub const ADJACENCY: i64 = 6;

/// Damage a structure on poor ground takes per tick, in tenths of a
/// health point — sand gives way under a refinery slowly enough to be a
/// decision and fast enough to be a problem.
pub const SAND_DECAY: u32 = 1;

/// What a structure costs and how long it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Blueprint {
    pub kind: Structure,
    /// Footprint in CELLS, square. Buildings are described in tiles in
    /// the design (§7) and in cells here, because cells are what the
    /// ground is made of.
    pub span: i64,
    pub cost: u32,
    pub ticks: u32,
    /// Whether this one has to touch an existing base. The yard does
    /// not — it is what a base starts as.
    pub adjacent: bool,
}

/// Everything buildable in this slice. Ordered, because the order is the
/// sidebar's order and the sidebar's order must be the same on every
/// peer that draws one.
pub const CATALOGUE: [Blueprint; 4] = [
    Blueprint {
        kind: Structure::Yard,
        span: 8,
        cost: 0,
        ticks: 0,
        adjacent: false,
    },
    Blueprint {
        kind: Structure::WindTrap,
        span: 4,
        cost: 300,
        ticks: 150,
        adjacent: true,
    },
    Blueprint {
        kind: Structure::Refinery,
        span: 8,
        cost: 400,
        ticks: 300,
        adjacent: true,
    },
    Blueprint {
        kind: Structure::Silo,
        span: 4,
        cost: 150,
        ticks: 90,
        adjacent: true,
    },
];

/// The blueprint for a kind, or `None` if it is not buildable.
#[must_use]
pub fn blueprint(kind: Structure) -> Option<Blueprint> {
    CATALOGUE.iter().copied().find(|b| b.kind == kind)
}

/// How a structure sits relative to the ground around it (§7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Exposure {
    /// In a pit: hittable only from above, visible by its vents.
    Buried,
    /// On the surface, like everything in a flat-map RTS.
    Level,
    /// On a berm: it sees further and is seen from further.
    Elevated,
}

/// Why a site was refused. Worth naming rather than returning a bool:
/// the HUD says it, the AI reasons about it, and "you cannot build
/// there" with no reason is the worst message in the genre.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Off the edge of the world, or over a hole.
    NoGround,
    /// More than [`MAX_GRADE`] cells of relief across the pad.
    TooSteep,
    /// Something solid in the way above the pad.
    Obstructed,
    /// Nothing of yours within [`ADJACENCY`].
    Unconnected,
    /// Another structure already stands here.
    Occupied,
}

/// A validated site: where the pad will be, and what it will be like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Site {
    /// Lower corner of the footprint, in cells.
    pub at: (i64, i64),
    pub span: i64,
    /// The level the pad will be graded to.
    pub pad_z: i64,
    pub exposure: Exposure,
    /// Whether the ground under it will hold (§6a).
    pub firm: bool,
}

/// A structure standing on the map, as the rules know it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Standing {
    pub owner: PlayerNo,
    pub kind: Structure,
    pub at: (i64, i64),
    pub span: i64,
    pub pad_z: i64,
    pub exposure: Exposure,
    pub firm: bool,
    /// Health in TENTHS, so sand decay can be slower than a point a tick
    /// without needing a fraction anywhere in hashed state.
    pub health: u32,
    pub max_health: u32,
}

impl Standing {
    /// Whether the footprint covers a cell.
    #[must_use]
    pub fn covers(&self, x: i64, y: i64) -> bool {
        x >= self.at.0 && x < self.at.0 + self.span && y >= self.at.1 && y < self.at.1 + self.span
    }

    /// The cell a unit should drive to when it wants this building —
    /// the middle of the footprint.
    #[must_use]
    pub fn centre(&self) -> (i64, i64) {
        (self.at.0 + self.span / 2, self.at.1 + self.span / 2)
    }
}

/// Health a structure has per cell of footprint, in tenths.
pub const HEALTH_PER_CELL: u32 = 40;

/// One player's build queue: Dune II's one-item-at-a-time yard.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Queue {
    /// What is being built and how far along, in ticks.
    building: Option<(Structure, u32)>,
    /// What is finished and waiting for the player to say where.
    ready: Option<Structure>,
}

impl Queue {
    #[must_use]
    pub fn building(&self) -> Option<(Structure, u32)> {
        self.building
    }

    #[must_use]
    pub fn ready(&self) -> Option<Structure> {
        self.ready
    }

    /// Start building, charging for it up front. Refuses if something is
    /// already on the line, if one is already waiting to be placed, or
    /// if it cannot be afforded.
    pub fn order(&mut self, economy: &mut Economy, who: PlayerNo, kind: Structure) -> bool {
        if self.building.is_some() || self.ready.is_some() {
            return false;
        }
        let Some(bp) = blueprint(kind) else {
            return false;
        };
        if !economy.player(who).charge(bp.cost) {
            return false;
        }
        self.building = Some((kind, 0));
        true
    }

    /// Advance by one tick's worth of work.
    ///
    /// **Power is the build speed** (§7). A yard at full power puts in a
    /// tick per tick; at half power, half. The rounding is deliberate:
    /// integer division would stall a badly browned-out base completely,
    /// so the floor is one — slow, never stopped.
    pub fn tick(&mut self, economy: &mut Economy, who: PlayerNo) {
        let Some((kind, done)) = self.building else {
            return;
        };
        let Some(bp) = blueprint(kind) else {
            self.building = None;
            return;
        };
        let rate = (economy.player(who).satisfaction() / 10).max(1);
        let done = done + rate;
        if done >= bp.ticks {
            self.building = None;
            self.ready = Some(kind);
        } else {
            self.building = Some((kind, done));
        }
    }

    /// Take what is waiting, because it is about to be placed.
    pub fn take(&mut self) -> Option<Structure> {
        self.ready.take()
    }

    /// Give it back — the site was refused.
    pub fn put_back(&mut self, kind: Structure) {
        self.ready = Some(kind);
    }
}

/// Every structure standing, and every player's queue.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Yards {
    standing: BTreeMap<EntityId, Standing>,
    queues: BTreeMap<PlayerNo, Queue>,
}

impl Yards {
    #[must_use]
    pub fn new() -> Yards {
        Yards::default()
    }

    pub fn queue(&mut self, who: PlayerNo) -> &mut Queue {
        self.queues.entry(who).or_default()
    }

    #[must_use]
    pub fn queue_of(&self, who: PlayerNo) -> Option<&Queue> {
        self.queues.get(&who)
    }

    #[must_use]
    pub fn get(&self, entity: EntityId) -> Option<&Standing> {
        self.standing.get(&entity)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.standing.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.standing.is_empty()
    }

    /// Every structure, in entity order.
    pub fn iter(&self) -> impl Iterator<Item = (&EntityId, &Standing)> {
        self.standing.iter()
    }

    /// The economy's view of what is standing.
    pub fn economy_view(&self) -> impl Iterator<Item = crate::Building> + '_ {
        self.standing.values().map(|s| crate::Building {
            owner: s.owner,
            kind: s.kind,
        })
    }

    /// Can `who` put a `kind` with its lower corner at `at`?
    ///
    /// The terrain half of this is [`grade`], which the placement ghost
    /// calls too — a preview that judged a site by its own rules would
    /// eventually disagree with the answer, and disagreeing with your own
    /// preview is worse than having none.
    ///
    /// # Errors
    /// A [`Refusal`] naming what is wrong with the site.
    pub fn survey(
        &self,
        host: &dyn Host,
        who: PlayerNo,
        kind: Structure,
        at: (i64, i64),
    ) -> Result<Site, Refusal> {
        let bp = blueprint(kind).ok_or(Refusal::NoGround)?;
        let span = bp.span;

        for y in at.1..(at.1 + span) {
            for x in at.0..(at.0 + span) {
                if self.occupied(x, y) {
                    return Err(Refusal::Occupied);
                }
            }
        }
        let (pad_z, firm) = grade(host, at, span)?;

        if bp.adjacent && !self.connected(who, at, span) {
            return Err(Refusal::Unconnected);
        }

        Ok(Site {
            at,
            span,
            pad_z,
            exposure: exposure(host, at, span, pad_z),
            firm,
        })
    }

    /// The nearest site to `near` that would take a `kind`, searched in
    /// rings outward.
    ///
    /// What an MCV driver does by eye and what the AI will do by rule
    /// (§7, L8) — and what makes a starting base possible on a dune sea
    /// at all, where the exact cell you happen to stand on is very
    /// unlikely to be level enough.
    pub fn find_site(
        &self,
        host: &dyn Host,
        who: PlayerNo,
        kind: Structure,
        near: (i64, i64),
        radius: i64,
    ) -> Option<Site> {
        for r in 0..=radius {
            for dy in -r..=r {
                for dx in -r..=r {
                    // Only the ring at this radius: the inner ones were
                    // tried already, and a spiral that revisits them
                    // turns a linear search into a quadratic one.
                    if dx.abs() != r && dy.abs() != r {
                        continue;
                    }
                    if let Ok(site) = self.survey(host, who, kind, (near.0 + dx, near.1 + dy)) {
                        return Some(site);
                    }
                }
            }
        }
        None
    }

    /// Grade the pad and put the structure on it.
    ///
    /// The terraforming is not decoration: the pad is filled to level
    /// with bearing material, so a building really does change the ground
    /// it stands on, the navigation graph really does hear about it (the
    /// paint verbs invalidate — §4d), and a structure demolished later
    /// leaves a platform behind.
    pub fn raise(
        &mut self,
        host: &dyn Host,
        who: PlayerNo,
        kind: Structure,
        site: Site,
        entity: EntityId,
    ) {
        let fill = if site.firm {
            material::PACKED_FILL
        } else {
            material::SAND
        };
        for y in site.at.1..(site.at.1 + site.span) {
            for x in site.at.0..(site.at.0 + site.span) {
                let top = host.volume_top(x, y).map_or(site.pad_z, |(z, _)| z);
                if top < site.pad_z {
                    host.volume_fill(
                        (x, y, top + 1),
                        (x, y, site.pad_z),
                        fill,
                        material::color(fill),
                    );
                }
                host.nav_block(x, y, true);
            }
        }
        let max_health = HEALTH_PER_CELL * u32::try_from(site.span * site.span).unwrap_or(1);
        self.standing.insert(
            entity,
            Standing {
                owner: who,
                kind,
                at: site.at,
                span: site.span,
                pad_z: site.pad_z,
                exposure: site.exposure,
                firm: site.firm,
                health: max_health,
                max_health,
            },
        );
    }

    /// Take a structure off the map. Returns what it was.
    pub fn demolish(&mut self, host: &dyn Host, entity: EntityId) -> Option<Standing> {
        let gone = self.standing.remove(&entity)?;
        for y in gone.at.1..(gone.at.1 + gone.span) {
            for x in gone.at.0..(gone.at.0 + gone.span) {
                host.nav_block(x, y, false);
            }
        }
        Some(gone)
    }

    /// Wear and repair, once a tick.
    ///
    /// Returns the entities that fell down, for the caller to despawn —
    /// the rules own the entity table, not this.
    pub fn weather(&mut self, economy: &mut Economy) -> Vec<EntityId> {
        let mut lost = Vec::new();
        for (&entity, s) in &mut self.standing {
            if s.firm {
                continue;
            }
            // Poor ground: the descendant of Dune II's concrete rule
            // (§6a). A structure on raw sand does not fall over at once,
            // it settles — and a Surfling pad is what stops it.
            s.health = s.health.saturating_sub(SAND_DECAY);
            if s.health == 0 {
                lost.push(entity);
            }
        }
        for entity in &lost {
            self.standing.remove(entity);
        }
        let _ = economy;
        lost
    }

    /// Spend credits mending one structure. Returns what it cost.
    pub fn repair(&mut self, economy: &mut Economy, entity: EntityId, rate: u32) -> u32 {
        let Some(s) = self.standing.get_mut(&entity) else {
            return 0;
        };
        let missing = s.max_health - s.health;
        let mend = rate.min(missing);
        if mend == 0 {
            return 0;
        }
        // A tenth of a health point costs a credit: cheap enough to be
        // the right answer, dear enough that letting a base rot on sand
        // is a real bill rather than an inconvenience.
        let price = mend;
        if !economy.player(s.owner).charge(price) {
            return 0;
        }
        s.health += mend;
        price
    }

    /// Whether a cell is inside somebody's footprint.
    fn occupied(&self, x: i64, y: i64) -> bool {
        self.standing.values().any(|s| s.covers(x, y))
    }

    /// Whether the site touches this player's base.
    fn connected(&self, who: PlayerNo, at: (i64, i64), span: i64) -> bool {
        self.standing.values().any(|s| {
            s.owner == who
                && at.0 - (s.at.0 + s.span) <= ADJACENCY
                && s.at.0 - (at.0 + span) <= ADJACENCY
                && at.1 - (s.at.1 + s.span) <= ADJACENCY
                && s.at.1 - (at.1 + span) <= ADJACENCY
        })
    }
}

/// The terrain's verdict on a footprint: the level its pad would be
/// graded to, and whether the ground under it will bear a building.
///
/// Split out of [`Yards::survey`] because the placement ghost has to ask
/// the same question from the local layer, where the structure table is
/// not visible. A preview judging a site by its own copy of these rules
/// would drift from the answer eventually — and disagreeing with your
/// own preview is worse than not having one.
///
/// Takes a [`WorldRead`], not a [`Host`]: this reads the ground, it does
/// not touch it.
///
/// # Errors
/// [`Refusal::NoGround`] over a hole, [`Refusal::TooSteep`] past
/// [`MAX_GRADE`] of relief, [`Refusal::Obstructed`] if anything is
/// standing in the clearance above the pad.
pub fn grade(host: &dyn WorldRead, at: (i64, i64), span: i64) -> Result<(i64, bool), Refusal> {
    let mut lowest = i64::MAX;
    let mut highest = i64::MIN;
    let mut firm = true;
    for y in at.1..(at.1 + span) {
        for x in at.0..(at.0 + span) {
            let (top, mat) = host.volume_top(x, y).ok_or(Refusal::NoGround)?;
            lowest = lowest.min(top);
            highest = highest.max(top);
            firm &= bears(mat);
        }
    }
    if highest - lowest > MAX_GRADE {
        return Err(Refusal::TooSteep);
    }
    // Clearance is checked against the GRADED pad, not the ground as it
    // is: the builders are about to fill the low corners up to `highest`,
    // so what matters is what stands above that.
    let pad_z = highest;
    for y in at.1..(at.1 + span) {
        for x in at.0..(at.0 + span) {
            for z in (pad_z + 1)..=(pad_z + BUILD_CLEARANCE) {
                if host.volume_material(x, y, z).is_some() {
                    return Err(Refusal::Obstructed);
                }
            }
        }
    }
    Ok((pad_z, firm))
}

/// Whether a material will hold a building up (§6a, §6c).
#[must_use]
pub fn bears(mat: MaterialId) -> bool {
    mat == material::ROCK || mat == material::PACKED_FILL || mat == material::GLASS
}

/// How far out the ring that decides buried-or-elevated is sampled.
///
/// Not the cell next door: a structure in the middle of a wide plateau
/// has plateau all around it and would read as level, when what makes it
/// elevated is that the *ground a shooter would stand on* is lower. Eight
/// cells is about where the ground beyond a base's own pad begins.
pub const EXPOSURE_STANDOFF: i64 = 8;

/// Where the pad sits relative to the ground at standoff distance (§7).
fn exposure(host: &dyn WorldRead, at: (i64, i64), span: i64, pad_z: i64) -> Exposure {
    let (lo, hi) = (
        (at.0 - EXPOSURE_STANDOFF, at.1 - EXPOSURE_STANDOFF),
        (
            at.0 + span - 1 + EXPOSURE_STANDOFF,
            at.1 + span - 1 + EXPOSURE_STANDOFF,
        ),
    );
    let mut sum = 0;
    let mut n = 0;
    for y in lo.1..=hi.1 {
        for x in lo.0..=hi.0 {
            // The ring itself, not the square: the inside is the base's
            // own graded ground and says nothing about how it sits.
            if x != lo.0 && x != hi.0 && y != lo.1 && y != hi.1 {
                continue;
            }
            if let Some((z, _)) = host.volume_top(x, y) {
                sum += z;
                n += 1;
            }
        }
    }
    if n == 0 {
        return Exposure::Level;
    }
    let ring = sum / n;
    if pad_z <= ring - MAX_GRADE {
        Exposure::Buried
    } else if pad_z >= ring + MAX_GRADE {
        Exposure::Elevated
    } else {
        Exposure::Level
    }
}
