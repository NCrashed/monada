//! The three factions' verbs, and the one knob that pays for them
//! (docs/plans/desert-game.md §6, §4e).
//!
//! Each faction reshapes the desert a different way — Surflings add
//! material, Dwellers move it, Binders change what it is — but from the
//! engine's side all three are the same thing: a bounded stream of cell
//! edits against the volume store. So they are one type with three
//! shapes of work rather than three subsystems, and they share the one
//! resource that makes terraforming safe to expose at all.
//!
//! **The budget is the whole design** (§4e). A terrain edit is never
//! free: it dirties a render chunk, invalidates navigation stands, and
//! wakes the settling automaton. Capping *edits per tick* caps all four
//! costs at once, which is why the pacing knob lives here, in the rules,
//! and not in the engine — the engine cannot know what a fair rate is,
//! and the game cannot afford not to have one.
//!
//! **Order of a tick.** The desert answers before the engineers act: up
//! to half the allowance goes to settling whatever is still falling, and
//! the jobs get the rest plus whatever settling did not want. A player
//! cannot out-dig an avalanche, and an avalanche cannot starve the
//! player forever — sand converges, so the allowance flows back.

use std::collections::BTreeMap;

use monada_runtime::{Host, MaterialId};

use crate::gen::BEDROCK_Z;
use crate::material;

/// One tick's terraform allowance, in host edits — the §4e knob.
///
/// 3000 is the number the D-3 gate is written against: the store absorbs
/// that many edits in well under a millisecond now that its digest is
/// incremental (§13a), and it is enough that a bore crew visibly digs
/// while being far short of what would stall a re-upload.
pub const CELLS_PER_TICK: u32 = 3_000;

/// The most of a tick the settling pass may take, leaving the remainder
/// to the jobs. Half: enough that a collapse always progresses, bounded
/// enough that engineering never fully stops for one.
pub const SETTLE_SHARE: u32 = CELLS_PER_TICK / 2;

/// What a terraform job does to the ground it covers.
///
/// The three variants are the three factions (§6a–c), and their
/// asymmetry is deliberate: only [`Dig`](Work::Dig) conserves mass, only
/// [`Raise`](Work::Raise) creates it, and only
/// [`Vitrify`](Work::Vitrify) leaves the shape alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Work {
    /// **Surfling.** Build every column in the footprint up to `level` in
    /// packed fill — a berm, a ramp, a platform for a refinery to stand
    /// on. The fill is manufactured, so this adds net material to the
    /// map, and packed fill is not granular, so what it builds stands
    /// with sheer sides.
    Raise { level: i64 },
    /// **Dweller.** Cut every column down to `level` and pile what comes
    /// out on the `spoil` column. Mass is conserved exactly — a trench
    /// here is a heap there — and the heap is loose, so it slumps into a
    /// cone that anyone can see from across the map. That is the
    /// faction's weakness rendered as terrain rather than as a rule.
    Dig { level: i64, spoil: (i64, i64) },
    /// **Binder.** Turn the top `depth` cells of every column to glass,
    /// whatever they were: sand, spice, an enemy's packed fill. The shape
    /// of the ground does not change at all — only what it is made of,
    /// and therefore what walks on it, what a worm will enter, and
    /// whether it holds.
    Vitrify { depth: i64 },
}

/// A queued piece of terraforming: what to do, over which columns, and
/// how far along it is.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Job {
    work: Work,
    lo: (i64, i64),
    hi: (i64, i64),
    /// The next column to work on, y-major like the paint pass; `None`
    /// once the footprint is exhausted.
    at: Option<(i64, i64)>,
}

impl Job {
    /// Do one unit of work — one cell — or report the job finished.
    ///
    /// Columns that need nothing are skipped without spending, which is
    /// what lets a job be re-ordered over ground that is already the
    /// right shape and cost nothing but the scan.
    fn step(&mut self, host: &dyn Host) -> Option<u32> {
        loop {
            let (x, y) = self.at?;
            if let Some(spent) = self.work.step_column(host, x, y) {
                return Some(spent);
            }
            self.advance();
        }
    }

    /// Move the cursor to the next column, or off the end.
    fn advance(&mut self) {
        let Some((x, y)) = self.at else {
            return;
        };
        self.at = if x < self.hi.0 {
            Some((x + 1, y))
        } else if y < self.hi.1 {
            Some((self.lo.0, y + 1))
        } else {
            None
        };
    }
}

impl Work {
    /// One cell of this verb on one column: the number of host edits it
    /// cost, or `None` when the column already satisfies the order.
    ///
    /// Why some verbs cost two. Replacing a material is a clear followed
    /// by a fill, because a paint over an existing solid does not
    /// recolour it — the first painter owns the colour — so a conversion
    /// that skipped the clear would change the ground under the player's
    /// feet without changing what they see. Charging both edits to the
    /// budget is not bookkeeping pedantry: two edits really do cost two
    /// chunk re-uploads.
    fn step_column(self, host: &dyn Host, x: i64, y: i64) -> Option<u32> {
        match self {
            Work::Raise { level } => {
                let top = host.volume_top(x, y).map_or(BEDROCK_Z - 1, |(z, _)| z);
                if top >= level {
                    return None;
                }
                place(host, x, y, top + 1, material::PACKED_FILL);
                Some(1)
            }
            Work::Dig { level, spoil } => {
                let (top, mat) = host.volume_top(x, y)?;
                if top <= level || spoil == (x, y) {
                    // Digging into your own spoil pile is not work, it is
                    // a loop: the cell comes out and goes straight back,
                    // and the column never gets any lower. Refusing it
                    // here is what keeps a job with a badly placed heap
                    // finite instead of hanging the tick.
                    return None;
                }
                host.volume_clear(x, y, top);
                let spoil_z = host
                    .volume_top(spoil.0, spoil.1)
                    .map_or(BEDROCK_Z, |(z, _)| z + 1);
                place(host, spoil.0, spoil.1, spoil_z, material::spoil_of(mat));
                Some(2)
            }
            Work::Vitrify { depth } => {
                let (top, _) = host.volume_top(x, y)?;
                let floor = (top - depth + 1).max(BEDROCK_Z);
                let mut z = top;
                while z >= floor {
                    match host.volume_material(x, y, z) {
                        Some(mat) if mat != material::GLASS => {
                            host.volume_clear(x, y, z);
                            place(host, x, y, z, material::GLASS);
                            return Some(2);
                        }
                        // Already glass, or a void inside the window (a
                        // bore runs under here): neither is work.
                        _ => z -= 1,
                    }
                }
                None
            }
        }
    }
}

/// Integer square root, rounded down — Newton's method on i64, six or
/// seven iterations for anything a crater is made of.
///
/// Hand-rolled because `i64::isqrt` landed after this workspace's MSRV,
/// and because a float `sqrt` here would be the one place a platform
/// could disagree about the shape of a hole in the ground.
fn isqrt(v: i64) -> i64 {
    if v <= 0 {
        return 0;
    }
    let mut x = v;
    let mut next = (x + 1) / 2;
    while next < x {
        x = next;
        next = (x + v / x) / 2;
    }
    x
}

/// Put one cell of `material` down, colour and all.
fn place(host: &dyn Host, x: i64, y: i64, z: i64, mat: MaterialId) {
    host.volume_fill((x, y, z), (x, y, z), mat, material::color(mat));
}

/// What one tick of terraforming actually did — the numbers a HUD shows
/// and a test asserts on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spent {
    /// Cells the settling pass moved.
    pub settled: u32,
    /// Cells the jobs edited.
    pub edited: u32,
}

impl Spent {
    /// The whole tick's charge against [`CELLS_PER_TICK`].
    #[must_use]
    pub fn total(self) -> u32 {
        self.settled + self.edited
    }
}

/// Every terraform order in flight, and the budget they share.
///
/// Hashed, snapshotted state: a peer that disagrees about how far a
/// trench has been dug disagrees about the ground within a second.
#[derive(Default, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Terraform {
    jobs: BTreeMap<u64, Job>,
    /// The next id to hand out. Monotonic, so the map order is also the
    /// order the orders were given — first come, first served, and the
    /// same on every peer.
    next: u64,
}

/// A handle on a queued order, for cancelling it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId(pub u64);

impl Terraform {
    #[must_use]
    pub fn new() -> Terraform {
        Terraform::default()
    }

    /// Queue `work` over the rectangle of columns `lo..=hi`.
    pub fn order(&mut self, lo: (i64, i64), hi: (i64, i64), work: Work) -> JobId {
        let (lo, hi) = (
            (lo.0.min(hi.0), lo.1.min(hi.1)),
            (lo.0.max(hi.0), lo.1.max(hi.1)),
        );
        let id = self.next;
        self.next += 1;
        self.jobs.insert(
            id,
            Job {
                work,
                lo,
                hi,
                at: Some(lo),
            },
        );
        JobId(id)
    }

    /// Abandon an order — the engineers were killed, the player changed
    /// their mind. Whatever was already dug stays dug.
    pub fn cancel(&mut self, job: JobId) {
        self.jobs.remove(&job.0);
    }

    /// How many orders are still in flight.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.jobs.len()
    }

    /// One tick: let the ground settle, then spend what is left on the
    /// orders, oldest first.
    pub fn run(&mut self, host: &dyn Host) -> Spent {
        let settled = host.settle(SETTLE_SHARE);
        let allowance = CELLS_PER_TICK - settled.min(CELLS_PER_TICK);
        let mut edited = 0;

        // Snapshot the ids: a job finishing removes itself, and iterating
        // a map one is being removed from is neither borrowable nor
        // canonical.
        let ids: Vec<u64> = self.jobs.keys().copied().collect();
        for id in ids {
            while edited < allowance {
                let Some(job) = self.jobs.get_mut(&id) else {
                    break;
                };
                if let Some(spent) = job.step(host) {
                    edited += spent;
                } else {
                    self.jobs.remove(&id);
                    break;
                }
            }
            if edited >= allowance {
                break;
            }
        }
        Spent { settled, edited }
    }

    /// Blow a bowl out of the ground: an instant edit, not an order.
    ///
    /// An explosion does not queue and does not conserve — the material
    /// is thrown away, which is exactly what distinguishes a crater from
    /// a Dweller's excavation (§6b). What makes it read as a crater
    /// rather than as a cylinder is the settling that follows: the rim is
    /// left over-steep on purpose and slumps inward over the next ticks.
    ///
    /// Returns the edits it cost, so a caller that cares can charge them
    /// against the same allowance the orders draw on.
    pub fn crater(host: &dyn Host, at: (i64, i64), radius: i64) -> u32 {
        if radius <= 0 {
            return 0;
        }
        let mut edits = 0;
        for y in (at.1 - radius)..=(at.1 + radius) {
            for x in (at.0 - radius)..=(at.0 + radius) {
                let (dx, dy) = (x - at.0, y - at.1);
                let d2 = dx * dx + dy * dy;
                if d2 > radius * radius {
                    continue;
                }
                // A hemisphere: `depth² + d² ≤ r²`, which is deep in the
                // middle and — crucially — still two or three cells deep
                // right up against the lip. A gentler profile would come
                // out of the ground already at the angle of repose and
                // never slump, which is not what a shell does to sand.
                let depth = isqrt(radius * radius - d2);
                let Some((top, _)) = host.volume_top(x, y) else {
                    continue;
                };
                let floor = (top - depth + 1).max(BEDROCK_Z);
                for z in floor..=top {
                    host.volume_clear(x, y, z);
                    edits += 1;
                }
            }
        }
        edits
    }
}
