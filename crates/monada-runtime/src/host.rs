//! [`Host`] — the surface a map's rules call, independent of the language
//! they are written in (docs/plans/desert-game.md §3a, decision L1/L7).
//!
//! Today the same surface exists twice in spirit: once as the closures
//! `monada-script` registers into a Rhai engine, and once as the
//! [`HostBridge`] the native host implements. This trait is the single
//! definition both runtimes call, so a Rust map's rules and a Rhai map's
//! script reach identical engine behaviour by construction rather than by
//! two implementations agreeing.
//!
//! **Why `&self` and not `&mut self`.** A host is a *handle* to the
//! runtime, not the runtime itself: every implementation holds shared
//! state (`Arc<Mutex<World>>`, the bridge) exactly as the Rhai
//! registration already does, and several handles may be live at once
//! (the sim layer and the local layer both hold one). Mutation belongs to
//! the runtime behind the handle, so the methods that change hashed state
//! take `&self` and lock. This also keeps the surface directly
//! expressible as a wasm import table, where a handle is an index and
//! `&mut` has no meaning (§3f).

use monada_fixed::{Fixed, FixedVec3};
use monada_sim::{ArchetypeId, EntityId};

use crate::{SharedBridge, SharedWorld};

/// The engine surface a map's rules call.
///
/// Grows verb by verb as the native backend's maps need them; the Rhai
/// registration in `monada-script` remains the exhaustive list until the
/// migration finishes (docs/plans/desert-game.md D-0).
pub trait Host {
    /// Declare an archetype with the given field names; returns its id.
    fn archetype(&self, fields: &[&str]) -> ArchetypeId;
    /// Spawn an entity of `archetype`.
    fn entity_create(&self, archetype: ArchetypeId) -> EntityId;
    /// Remove an entity; returns whether it existed.
    fn entity_despawn(&self, entity: EntityId) -> bool;
    /// Set an entity's position (sim cells).
    fn entity_set_position(&self, entity: EntityId, pos: FixedVec3);
    /// Set a named fixed-point field.
    fn entity_set_field(&self, entity: EntityId, name: &str, value: Fixed);
    /// An entity's position, or the zero vector.
    fn entity_position(&self, entity: EntityId) -> FixedVec3;
    /// Read a named field, or zero.
    fn entity_field(&self, entity: EntityId, name: &str) -> Fixed;
    /// Every entity, in a defined order.
    fn entities(&self) -> Vec<EntityId>;
    /// The entities of one archetype, ascending.
    fn entities_of(&self, archetype: ArchetypeId) -> Vec<EntityId>;
    /// A fixed-point value in `[0, 1)` from the world's seeded generator.
    fn rng01(&self) -> Fixed;
    /// An integer in `0..n` from the world's seeded generator. Unsigned
    /// where the script surface says `i64`: a Rhai map has one numeric
    /// type, compiled rules do not, and the conversion belongs at the
    /// language boundary rather than in the world.
    fn rng_below(&self, n: u64) -> u64;

    /// The render / input seam, when one is attached. `None` on a
    /// headless peer (the oracle, a dedicated server), where presentation
    /// verbs are no-ops by design — rules must therefore treat drawing as
    /// optional and never let a bridge's absence change hashed state.
    fn bridge(&self) -> Option<&SharedBridge>;
}

/// The [`Host`] implementation over monada's own runtime state: the
/// shared [`World`](monada_sim::World) plus an optional render bridge.
///
/// Cheap to clone (it is a bundle of handles), which is what lets the
/// Rhai registration capture one per closure and the native backend hold
/// one for the whole match.
#[derive(Clone)]
pub struct RuntimeHost {
    world: SharedWorld,
    bridge: Option<SharedBridge>,
}

impl RuntimeHost {
    /// A host over `world`, with no render bridge (headless).
    #[must_use]
    pub fn new(world: SharedWorld) -> RuntimeHost {
        RuntimeHost {
            world,
            bridge: None,
        }
    }

    /// Attach the render / input bridge. Call before `init`, matching
    /// `RhaiBackend::set_bridge`'s contract.
    pub fn set_bridge(&mut self, bridge: &SharedBridge) {
        self.bridge = Some(bridge.clone());
    }

    /// The shared world this host mutates.
    #[must_use]
    pub fn world(&self) -> &SharedWorld {
        &self.world
    }
}

impl Host for RuntimeHost {
    fn archetype(&self, fields: &[&str]) -> ArchetypeId {
        self.world
            .lock()
            .expect("world mutex")
            .register_archetype(fields)
    }

    fn entity_create(&self, archetype: ArchetypeId) -> EntityId {
        self.world.lock().expect("world mutex").spawn(archetype)
    }

    fn entity_despawn(&self, entity: EntityId) -> bool {
        self.world.lock().expect("world mutex").despawn(entity)
    }

    fn entity_set_position(&self, entity: EntityId, pos: FixedVec3) {
        self.world
            .lock()
            .expect("world mutex")
            .set_position(entity, pos);
    }

    fn entity_set_field(&self, entity: EntityId, name: &str, value: Fixed) {
        self.world
            .lock()
            .expect("world mutex")
            .set_field(entity, name, value);
    }

    fn entity_position(&self, entity: EntityId) -> FixedVec3 {
        self.world
            .lock()
            .expect("world mutex")
            .position(entity)
            .unwrap_or(FixedVec3::ZERO)
    }

    fn entity_field(&self, entity: EntityId, name: &str) -> Fixed {
        self.world
            .lock()
            .expect("world mutex")
            .field(entity, name)
            .unwrap_or(Fixed::ZERO)
    }

    fn entities(&self) -> Vec<EntityId> {
        self.world.lock().expect("world mutex").all_entities()
    }

    fn entities_of(&self, archetype: ArchetypeId) -> Vec<EntityId> {
        self.world
            .lock()
            .expect("world mutex")
            .entities(archetype)
            .to_vec()
    }

    fn rng01(&self) -> Fixed {
        self.world.lock().expect("world mutex").rng.next_fixed_01()
    }

    fn rng_below(&self, n: u64) -> u64 {
        self.world.lock().expect("world mutex").rng.gen_below(n)
    }

    fn bridge(&self) -> Option<&SharedBridge> {
        self.bridge.as_ref()
    }
}
