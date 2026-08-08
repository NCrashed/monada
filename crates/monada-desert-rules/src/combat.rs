//! Shooting, and what the ground has to do with it
//! (docs/plans/desert-game.md §7) — the D-6 slice.
//!
//! The rest of this game's combat is the genre's: a damage table by
//! weapon and armour class, a cooldown, a projectile with a flight time.
//! One thing is not, and it is the reason the slice exists.
//!
//! **Direct fire is blocked by terrain.** Not by a scripted cover flag
//! on a tile — by the actual voxels between the muzzle and the target,
//! asked with the same integer ray march the physics solver uses. So a
//! Surfling rampart really is a firing position, a Dweller trench really
//! is cover, and a unit in a pit is safe from everything that cannot
//! shoot over the rim.
//!
//! **Arcing weapons are the answer**, and that is the whole rock-paper
//! of it: a mortar ignores the line of fire and cares only about range,
//! which is why artillery exists and why a berm is a commitment rather
//! than an autowin. It pays for that with a slow shell the target can
//! drive out from under.
//!
//! **A splash edits the ground.** A shell that lands makes a crater, the
//! crater slumps (§4d), and the map is a little different for the rest
//! of the match. The economy notices too, because spice is terrain.

use std::collections::BTreeMap;

use monada_runtime::Host;
use monada_sim::EntityId;

use crate::economy::PlayerNo;
use crate::terraform::Terraform;

/// What a thing is made of, as a shell cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Armour {
    /// Soft, and hard to hit with a big slow shell.
    Foot,
    /// Wheels and light plate.
    Light,
    /// A main tank.
    Heavy,
    /// It does not move, and it is made of packed fill.
    Building,
}

/// What a thing shoots with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Weapon {
    /// Infantry small arms: fast, cheap, useless against plate.
    Gun,
    /// A tank's main gun: flat trajectory, wants a clear line.
    Cannon,
    /// A turret's rocket: slower, hits harder, still direct.
    Rocket,
    /// Artillery: lobbed, so the ground between does not matter.
    Mortar,
}

impl Weapon {
    /// Whether the ground between shooter and target can stop it.
    #[must_use]
    pub fn is_direct(self) -> bool {
        !matches!(self, Weapon::Mortar)
    }

    /// Damage against each armour class, in tenths of a point — the
    /// `(weapon × armour)` table of §7, and the only place balance
    /// between the three lives.
    #[must_use]
    // A balance table, read as a grid. Collapsing the arms that happen
    // to share a number today would make it unreadable and make every
    // future tweak a refactor.
    #[allow(clippy::match_same_arms)]
    pub fn damage(self, against: Armour) -> u32 {
        match (self, against) {
            (Weapon::Gun, Armour::Foot) => 60,
            (Weapon::Gun, Armour::Light) => 25,
            (Weapon::Gun, Armour::Heavy) => 6,
            (Weapon::Gun, Armour::Building) => 10,

            (Weapon::Cannon, Armour::Foot) => 40,
            (Weapon::Cannon, Armour::Light) => 90,
            (Weapon::Cannon, Armour::Heavy) => 70,
            (Weapon::Cannon, Armour::Building) => 60,

            (Weapon::Rocket, Armour::Foot) => 50,
            (Weapon::Rocket, Armour::Light) => 110,
            (Weapon::Rocket, Armour::Heavy) => 120,
            (Weapon::Rocket, Armour::Building) => 90,

            // A mortar is a poor answer to anything that can walk out of
            // the way, and a very good one to anything that cannot.
            (Weapon::Mortar, Armour::Foot) => 30,
            (Weapon::Mortar, Armour::Light) => 60,
            (Weapon::Mortar, Armour::Heavy) => 55,
            (Weapon::Mortar, Armour::Building) => 130,
        }
    }

    /// Range in cells, ticks between shots, and the crater a hit leaves
    /// (radius in cells; zero for weapons that do not move earth).
    #[must_use]
    pub fn profile(self) -> (i64, u32, i64) {
        match self {
            Weapon::Gun => (10, 12, 0),
            Weapon::Cannon => (18, 45, 1),
            Weapon::Rocket => (24, 75, 2),
            Weapon::Mortar => (40, 110, 3),
        }
    }

    /// Cells a shell of this weapon covers per tick.
    #[must_use]
    pub fn speed(self) -> i64 {
        match self {
            Weapon::Gun => 12,
            Weapon::Cannon => 8,
            Weapon::Rocket => 5,
            // Slow and lobbed: the shell is the warning.
            Weapon::Mortar => 3,
        }
    }
}

/// How far above its own cell a thing's guns and hull sit.
///
/// Both matter to line of fire and neither is a detail: a shot is taken
/// from the muzzle, not from the ground, and a target is hit in the
/// body, not at its feet. Getting this wrong makes every unit shoot
/// itself in the terrain it is standing on.
pub const MUZZLE: i64 = 1;

/// Health per armour class, in tenths.
#[must_use]
pub fn hull(armour: Armour) -> u32 {
    match armour {
        Armour::Foot => 400,
        Armour::Light => 900,
        Armour::Heavy => 2_000,
        Armour::Building => 4_000,
    }
}

/// One thing that can shoot, be shot, and die.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Fighter {
    pub owner: PlayerNo,
    pub armour: Armour,
    pub weapon: Weapon,
    pub health: u32,
    pub max_health: u32,
    /// Ticks until it may fire again.
    pub cooldown: u32,
}

/// A shell in the air.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Shell {
    pub weapon: Weapon,
    pub owner: PlayerNo,
    /// Where it will land. A shell is committed the moment it is fired —
    /// it flies at a *place*, not at a unit, which is what makes driving
    /// out from under a mortar work.
    pub to: (i64, i64, i64),
    /// Ticks left in the air.
    pub eta: u32,
}

/// What one tick of fighting did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub shots: u32,
    pub hits: u32,
    pub kills: u32,
    /// Cells of ground the splashes moved.
    pub cratered: u32,
}

/// Everyone who can shoot, and everything in the air.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Battle {
    fighters: BTreeMap<EntityId, Fighter>,
    shells: BTreeMap<EntityId, Shell>,
    /// The next shell's id, so a shell is an entity like anything else
    /// and the render side needs no special case.
    next: u64,
}

impl Battle {
    #[must_use]
    pub fn new() -> Battle {
        Battle::default()
    }

    /// Put a fighter into the line.
    pub fn enlist(&mut self, unit: EntityId, owner: PlayerNo, armour: Armour, weapon: Weapon) {
        let max_health = hull(armour);
        self.fighters.insert(
            unit,
            Fighter {
                owner,
                armour,
                weapon,
                health: max_health,
                max_health,
                cooldown: 0,
            },
        );
    }

    pub fn discharge(&mut self, unit: EntityId) {
        self.fighters.remove(&unit);
    }

    #[must_use]
    pub fn get(&self, unit: EntityId) -> Option<&Fighter> {
        self.fighters.get(&unit)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.fighters.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fighters.is_empty()
    }

    #[must_use]
    pub fn shells(&self) -> usize {
        self.shells.len()
    }

    /// One tick: shells land, then everybody who can shoot does.
    ///
    /// Landing first, deliberately. A shell fired last tick belongs to
    /// last tick's world, and resolving it before this tick's shots means
    /// a unit killed by an incoming round does not also get to fire —
    /// the alternative gives every trade to whoever the map iterated
    /// first.
    pub fn run(&mut self, host: &dyn Host, yards: &mut crate::Yards) -> Report {
        let mut report = Report::default();
        self.land(host, yards, &mut report);
        self.shoot(host, &mut report);
        for f in self.fighters.values_mut() {
            f.cooldown = f.cooldown.saturating_sub(1);
        }
        report
    }

    /// Bring in everything whose flight time has run out.
    fn land(&mut self, host: &dyn Host, yards: &mut crate::Yards, report: &mut Report) {
        let mut arrived = Vec::new();
        for (&id, shell) in &mut self.shells {
            shell.eta = shell.eta.saturating_sub(1);
            if shell.eta == 0 {
                arrived.push((id, *shell));
            }
        }
        for (id, shell) in arrived {
            self.shells.remove(&id);
            host.entity_despawn(id);

            let (_, _, splash) = shell.weapon.profile();
            // Everything of another player's within the splash takes it,
            // in entity order.
            let hurt: Vec<EntityId> = self
                .fighters
                .iter()
                .filter(|(_, f)| f.owner != shell.owner)
                .map(|(&e, _)| e)
                .filter(|&e| within(host, e, shell.to, splash.max(1)))
                .collect();
            for unit in hurt {
                let Some(f) = self.fighters.get_mut(&unit) else {
                    continue;
                };
                let hit = shell.weapon.damage(f.armour);
                f.health = f.health.saturating_sub(hit);
                report.hits += 1;
                if f.health == 0 {
                    self.fighters.remove(&unit);
                    host.entity_despawn(unit);
                    report.kills += 1;
                }
            }
            // Structures under the splash take it on the SAME health
            // pool sand decay eats and repair refills — see
            // `Yards::hurt`.
            for dy in -splash..=splash {
                for dx in -splash..=splash {
                    let Some(hit) = yards.at(shell.to.0 + dx, shell.to.1 + dy) else {
                        continue;
                    };
                    if yards.hurt(hit, shell.weapon.damage(Armour::Building)) {
                        yards.demolish(host, hit);
                        host.entity_despawn(hit);
                        report.kills += 1;
                    }
                    report.hits += 1;
                    break;
                }
            }
            if splash > 0 {
                report.cratered += Terraform::crater(host, (shell.to.0, shell.to.1), splash);
            }
        }
    }

    /// Everybody who has a target, a clear shot and a cold gun.
    fn shoot(&mut self, host: &dyn Host, report: &mut Report) {
        let shooters: Vec<(EntityId, Fighter)> = self
            .fighters
            .iter()
            .filter(|(_, f)| f.cooldown == 0)
            .map(|(&e, &f)| (e, f))
            .collect();
        for (shooter, f) in shooters {
            let Some(target) = self.pick_target(host, shooter, f) else {
                continue;
            };
            let to = cell_of(host, target);
            let shell = EntityId(SHELL_BASE + self.next);
            self.next += 1;
            let range = distance(cell_of(host, shooter), to);
            let eta = u32::try_from((range / f.weapon.speed()).max(1)).unwrap_or(1);
            self.shells.insert(
                shell,
                Shell {
                    weapon: f.weapon,
                    owner: f.owner,
                    to: (to.0, to.1, to.2),
                    eta,
                },
            );
            if let Some(g) = self.fighters.get_mut(&shooter) {
                g.cooldown = f.weapon.profile().1;
            }
            report.shots += 1;
        }
    }

    /// The nearest enemy this one can actually shoot.
    fn pick_target(&self, host: &dyn Host, shooter: EntityId, f: Fighter) -> Option<EntityId> {
        let from = cell_of(host, shooter);
        let muzzle = (from.0, from.1, from.2 + MUZZLE);
        let (range, _, _) = f.weapon.profile();
        let mut best: Option<(EntityId, i64)> = None;
        for (&other, g) in &self.fighters {
            if g.owner == f.owner {
                continue;
            }
            let at = cell_of(host, other);
            let d = distance(from, at);
            if d > range {
                continue;
            }
            // The line of fire, and the whole point of the slice: a
            // direct weapon needs the ground out of the way, an arcing
            // one does not.
            if f.weapon.is_direct() {
                let body = (at.0, at.1, at.2 + MUZZLE);
                if let Some(blocked) = host.volume_ray(muzzle, body) {
                    // A hit on the target's own cell is not a block —
                    // that is the ray arriving.
                    if (blocked.0, blocked.1) != (at.0, at.1) {
                        continue;
                    }
                }
            }
            if best.map_or(true, |(_, near)| d < near) {
                best = Some((other, d));
            }
        }
        best.map(|(e, _)| e)
    }
}

/// Shell entity ids start well above anything a map spawns, so a shell
/// and a unit can never collide in the table.
const SHELL_BASE: u64 = 1 << 40;

/// An entity's cell.
fn cell_of(host: &dyn Host, e: EntityId) -> (i64, i64, i64) {
    let p = host.entity_position(e);
    (
        i64::from(p.x.floor_to_int()),
        i64::from(p.y.floor_to_int()),
        i64::from(p.z.floor_to_int()),
    )
}

/// Chebyshev distance in cells — the same metric the nav graph walks by,
/// so "in range" and "one step away" agree.
fn distance(a: (i64, i64, i64), b: (i64, i64, i64)) -> i64 {
    (a.0 - b.0).abs().max((a.1 - b.1).abs())
}

/// Whether an entity is within `radius` cells of a point.
fn within(host: &dyn Host, e: EntityId, of: (i64, i64, i64), radius: i64) -> bool {
    distance(cell_of(host, e), of) <= radius
}
