//! The M0 determinism scenario: **100 entities walk in a circle**
//! (DESIGN.md §7). It is deliberately tiny but exercises the whole
//! kernel — RNG-seeded spawn, SoA archetype storage, fixed-point trig
//! per tick, and canonical state hashing — so a single golden digest
//! proves the kernel is bit-stable across platforms and builds.
//!
//! Each mover orbits the origin on its own circle: random radius and
//! starting phase (drawn from the seeded RNG), constant angular speed.
//! Per tick the angle advances and the position is recomputed with
//! [`monada_fixed::trig`] — the only transcendental on the sim's hot
//! path, and the one most likely to diverge across libms if it were
//! not our own integer LUT.

use monada_fixed::{trig, Fixed, FixedVec3};

use crate::entity::EntityAllocator;
use crate::hash::{StateHash, StateHasher};
use crate::rng::DeterministicRng;
use crate::sim::Simulation;
use crate::storage::{ArchetypeStorage, Columns};

/// The default population for the canonical scenario.
pub const DEFAULT_COUNT: u32 = 100;
/// The default seed for the canonical scenario — ASCII `"MONADA_0"`.
pub const DEFAULT_SEED: u64 = 0x4D4F_4E41_4441_5F30;

/// Golden [`CircleSim::state_hash`] of the canonical scenario after
/// 600 ticks. This is the determinism gate: every supported platform
/// and build must reproduce it exactly (DESIGN.md §3.1, §7). Bump it
/// only alongside a deliberate change to the scenario itself.
pub const CANONICAL_HASH_AT_600: u64 = 2_920_854_233_001_871_778;

/// One mover's spawn-time components. A named row (not a positional
/// tuple) so the three same-typed `Fixed`s can't be silently misordered
/// — the contract [`Columns::Row`] is meant to model for codegen.
#[derive(Clone, Copy, Debug)]
pub struct MoverRow {
    pub pos: FixedVec3,
    pub angle: Fixed,
    pub omega: Fixed,
    pub radius: Fixed,
}

/// SoA columns for the orbiting-mover archetype.
///
/// Fields are private on purpose (see [`Columns`]): length only changes
/// via [`ArchetypeStorage::spawn`]/`despawn`, and values are touched
/// through the slice accessors below — never by pushing into a column.
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MoverColumns {
    pos: Vec<FixedVec3>,
    angle: Vec<Fixed>,
    omega: Vec<Fixed>,
    radius: Vec<Fixed>,
}

impl MoverColumns {
    /// World positions (recomputed each tick). For renderers/tests.
    #[must_use]
    pub fn pos(&self) -> &[FixedVec3] {
        &self.pos
    }

    /// Orbital angles (radians).
    #[must_use]
    pub fn angle(&self) -> &[Fixed] {
        &self.angle
    }

    /// Orbit radii.
    #[must_use]
    pub fn radius(&self) -> &[Fixed] {
        &self.radius
    }

    /// All columns as disjoint mutable slices, for the per-tick update.
    /// Returning slices means values can change but lengths cannot, so
    /// the storage's id↔column parallelism is structurally safe.
    fn split_mut(&mut self) -> (&mut [FixedVec3], &mut [Fixed], &mut [Fixed], &mut [Fixed]) {
        (
            &mut self.pos,
            &mut self.angle,
            &mut self.omega,
            &mut self.radius,
        )
    }
}

impl Columns for MoverColumns {
    type Row = MoverRow;

    fn push_row(&mut self, row: Self::Row) {
        self.pos.push(row.pos);
        self.angle.push(row.angle);
        self.omega.push(row.omega);
        self.radius.push(row.radius);
    }

    fn remove_at(&mut self, index: usize) {
        self.pos.remove(index);
        self.angle.remove(index);
        self.omega.remove(index);
        self.radius.remove(index);
    }

    fn len(&self) -> usize {
        self.pos.len()
    }
}

impl StateHash for MoverColumns {
    fn hash(&self, h: &mut StateHasher) {
        self.pos.hash(h);
        self.angle.hash(h);
        self.omega.hash(h);
        self.radius.hash(h);
    }
}

/// The walk-in-a-circle world.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct CircleSim {
    tick: u64,
    rng: DeterministicRng,
    alloc: EntityAllocator,
    movers: ArchetypeStorage<MoverColumns>,
}

impl CircleSim {
    /// Build the scenario with `count` movers seeded from `seed`.
    #[must_use]
    pub fn new(seed: u64, count: u32) -> CircleSim {
        let mut rng = DeterministicRng::seed_from_u64(seed);
        let mut alloc = EntityAllocator::new();
        let mut movers = ArchetypeStorage::new();

        for _ in 0..count {
            let id = alloc.alloc();
            // radius in [4, 12), phase in [0, τ), omega in [1/32, 1/16).
            let radius = Fixed::from_int(4) + rng.next_fixed_01() * Fixed::from_int(8);
            let angle = rng.next_fixed_01() * trig::TAU;
            let omega = Fixed::from_ratio(1, 32) + rng.next_fixed_01() * Fixed::from_ratio(1, 32);
            let pos = orbit(angle, radius);
            movers.spawn(
                id,
                MoverRow {
                    pos,
                    angle,
                    omega,
                    radius,
                },
            );
        }

        CircleSim {
            tick: 0,
            rng,
            alloc,
            movers,
        }
    }

    /// The canonical scenario: [`DEFAULT_COUNT`] movers, [`DEFAULT_SEED`].
    #[must_use]
    pub fn canonical() -> CircleSim {
        CircleSim::new(DEFAULT_SEED, DEFAULT_COUNT)
    }

    /// Read-only view of the mover storage (for renderers / tests).
    #[must_use]
    pub fn movers(&self) -> &ArchetypeStorage<MoverColumns> {
        &self.movers
    }
}

/// Position on a circle of the given `radius` at `angle`, in the z=0
/// plane.
fn orbit(angle: Fixed, radius: Fixed) -> FixedVec3 {
    FixedVec3::new(
        trig::cos(angle) * radius,
        trig::sin(angle) * radius,
        Fixed::ZERO,
    )
}

impl Simulation for CircleSim {
    fn step(&mut self) {
        self.tick += 1;
        // Slice accessors only: values change, lengths can't — the
        // id↔column parallelism stays structurally intact.
        let (pos, angle, omega, radius) = self.movers.columns_mut().split_mut();
        for i in 0..pos.len() {
            angle[i] += omega[i];
            pos[i] = orbit(angle[i], radius[i]);
        }
    }

    fn tick(&self) -> u64 {
        self.tick
    }

    fn state_hash(&self) -> u64 {
        let mut h = StateHasher::new();
        h.write_u64(self.tick);
        self.rng.hash(&mut h);
        self.alloc.hash(&mut h);
        self.movers.hash(&mut h);
        h.finish()
    }
}
