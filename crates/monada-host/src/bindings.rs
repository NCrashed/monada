//! Physical-input → action binding resolver (steps 1–2 of
//! `docs/plans/input-bindings.md`).
//!
//! The host's former fixed key map becomes a table of named *actions*
//! with default bindings, overridable from the user's `bindings.toml`.
//! Two action families share the table:
//!
//! - the **engine base set** ([`Action`]) — quit, camera, the pointer,
//!   the real-time move axis, replay transport;
//! - **map-declared actions** (`[[action]]` in the manifest,
//!   [`monada_format::ActionDecl`]) — resolved into [`ActionRef::Map`]
//!   entries whose live values ([`MapActionValue`]) the local script
//!   layer will consume (plan step 3).
//!
//! Actions are a host-side concept only — the simulation still sees
//! nothing but `Command`s (DESIGN.md §3.1), so rebinding can never
//! desync a match or change how a replay plays back.
//!
//! Bindings resolve against a stack of [`Context`]s: the host activates
//! `global`, one gameplay context (`turnbased` or `realtime`, from the
//! map's tick model), `map` for the map's own actions, and `replay` on
//! top when watching a recording. The topmost context wins, so e.g.
//! `Space` is the real-time dodge but the replay pause toggle. This is
//! the seed of the map-driven context stack (plan §3); until then the
//! host owns which contexts are active.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use monada_format::{ActionDecl, ActionDefault, ActionKind};
use winit::event::MouseButton;
use winit::keyboard::KeyCode;

/// A physical input a binding can attach to: a key (by physical
/// position, layout-independent) or a mouse button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PhysInput {
    Key(KeyCode),
    Mouse(MouseButton),
}

impl PhysInput {
    /// The mouse buttons a binding can name. `Back`/`Forward`/`Other`
    /// are dropped (no stable cross-platform meaning yet).
    pub fn from_mouse(button: MouseButton) -> Option<PhysInput> {
        match button {
            MouseButton::Left => Some(PhysInput::Mouse(MouseButton::Left)),
            MouseButton::Right => Some(PhysInput::Mouse(MouseButton::Right)),
            MouseButton::Middle => Some(PhysInput::Mouse(MouseButton::Middle)),
            _ => None,
        }
    }

    /// Parse a config/manifest name: `MouseLeft`/`MouseRight`/
    /// `MouseMiddle`, else a winit [`KeyCode`] variant name (`KeyW`,
    /// `Space`, `BracketLeft`, …) via its serde form.
    fn parse(name: &str) -> Option<PhysInput> {
        match name {
            "MouseLeft" => return Some(PhysInput::Mouse(MouseButton::Left)),
            "MouseRight" => return Some(PhysInput::Mouse(MouseButton::Right)),
            "MouseMiddle" => return Some(PhysInput::Mouse(MouseButton::Middle)),
            _ => {}
        }
        toml::Value::String(name.to_owned())
            .try_into::<KeyCode>()
            .ok()
            .map(PhysInput::Key)
    }

    /// A display/config label for this input (`KeyW`, `MouseLeft`).
    #[must_use]
    pub fn label(self) -> String {
        self.name()
    }

    /// The config-file name of this input (inverse of [`Self::parse`]).
    fn name(self) -> String {
        match self {
            PhysInput::Key(code) => format!("{code:?}"),
            PhysInput::Mouse(MouseButton::Left) => "MouseLeft".to_owned(),
            PhysInput::Mouse(MouseButton::Right) => "MouseRight".to_owned(),
            PhysInput::Mouse(MouseButton::Middle) => "MouseMiddle".to_owned(),
            PhysInput::Mouse(_) => "MouseOther".to_owned(),
        }
    }
}

/// Which binding table an entry lives in. Contexts higher in the active
/// stack shadow lower ones for the same physical input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Context {
    /// Always active: quit, debug HUD, camera orbit, primary pointer.
    Global,
    /// Command-driven maps (chess) and the circle scenes: camera zoom.
    TurnBased,
    /// Fixed-rate maps: the move axis, dodge, held attack.
    RealTime,
    /// The running map's `[[action]]` declarations (above the engine's
    /// gameplay context, below replay transport).
    MapGameplay,
    /// Replay viewing: transport controls.
    Replay,
}

impl Context {
    /// The `bindings.toml` section name for this context. Map actions
    /// are overridden via `[map."<name>"]`, not a context section.
    fn section(self) -> &'static str {
        match self {
            Context::Global => "global",
            Context::TurnBased => "turnbased",
            Context::RealTime => "realtime",
            Context::MapGameplay => "map",
            Context::Replay => "replay",
        }
    }

    fn from_section(name: &str) -> Option<Context> {
        match name {
            "global" => Some(Context::Global),
            "turnbased" => Some(Context::TurnBased),
            "realtime" => Some(Context::RealTime),
            "replay" => Some(Context::Replay),
            _ => None,
        }
    }

    /// A heading for this context's group in the rebind panel.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Context::Global => "General",
            Context::TurnBased => "Turn-based",
            Context::RealTime => "Real-time",
            Context::MapGameplay => "This map",
            Context::Replay => "Replay",
        }
    }
}

/// The engine's base action set — every hardcoded host key, promoted to
/// a named, rebindable action.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Quit,
    DebugHud,
    /// Toggle the key-bindings panel.
    OpenBindings,
    OrbitLeft,
    OrbitRight,
    OrbitUp,
    OrbitDown,
    ZoomIn,
    ZoomOut,
    /// The click-to-interact gesture (pick / spawn / chess move) for
    /// command-driven scenes; fires on press.
    PointerPrimary,
    MoveFwd,
    MoveBack,
    MoveLeft,
    MoveRight,
    Dodge,
    /// Held attack for real-time maps (shadows [`Action::PointerPrimary`]).
    Attack,
    ReplayPause,
    ReplaySlower,
    ReplayFaster,
}

/// Every base action, in template / documentation order.
const ALL_ACTIONS: [Action; 19] = [
    Action::Quit,
    Action::DebugHud,
    Action::OpenBindings,
    Action::OrbitLeft,
    Action::OrbitRight,
    Action::OrbitUp,
    Action::OrbitDown,
    Action::PointerPrimary,
    Action::ZoomIn,
    Action::ZoomOut,
    Action::MoveFwd,
    Action::MoveBack,
    Action::MoveLeft,
    Action::MoveRight,
    Action::Dodge,
    Action::Attack,
    Action::ReplayPause,
    Action::ReplaySlower,
    Action::ReplayFaster,
];

impl Action {
    /// The stable id used in `bindings.toml` (and later in the rebind UI
    /// and map manifests).
    fn id(self) -> &'static str {
        match self {
            Action::Quit => "ui.quit",
            Action::DebugHud => "ui.debug_hud",
            Action::OpenBindings => "ui.bindings",
            Action::OrbitLeft => "camera.orbit_left",
            Action::OrbitRight => "camera.orbit_right",
            Action::OrbitUp => "camera.orbit_up",
            Action::OrbitDown => "camera.orbit_down",
            Action::ZoomIn => "camera.zoom_in",
            Action::ZoomOut => "camera.zoom_out",
            Action::PointerPrimary => "pointer.primary",
            Action::MoveFwd => "move.fwd",
            Action::MoveBack => "move.back",
            Action::MoveLeft => "move.left",
            Action::MoveRight => "move.right",
            Action::Dodge => "game.dodge",
            Action::Attack => "game.attack",
            Action::ReplayPause => "replay.pause",
            Action::ReplaySlower => "replay.slower",
            Action::ReplayFaster => "replay.faster",
        }
    }

    fn from_id(id: &str) -> Option<Action> {
        ALL_ACTIONS.into_iter().find(|a| a.id() == id)
    }

    /// A human-readable label for the rebind UI.
    fn label(self) -> &'static str {
        match self {
            Action::Quit => "Quit",
            Action::DebugHud => "Debug overlay",
            Action::OpenBindings => "Key bindings",
            Action::OrbitLeft => "Camera left",
            Action::OrbitRight => "Camera right",
            Action::OrbitUp => "Camera up",
            Action::OrbitDown => "Camera down",
            Action::ZoomIn => "Zoom in",
            Action::ZoomOut => "Zoom out",
            Action::PointerPrimary => "Primary click",
            Action::MoveFwd => "Move forward",
            Action::MoveBack => "Move back",
            Action::MoveLeft => "Move left",
            Action::MoveRight => "Move right",
            Action::Dodge => "Dodge",
            Action::Attack => "Attack",
            Action::ReplayPause => "Replay pause",
            Action::ReplaySlower => "Replay slower",
            Action::ReplayFaster => "Replay faster",
        }
    }

    /// The single context each base action belongs to.
    fn context(self) -> Context {
        match self {
            Action::Quit
            | Action::DebugHud
            | Action::OpenBindings
            | Action::OrbitLeft
            | Action::OrbitRight
            | Action::OrbitUp
            | Action::OrbitDown
            | Action::PointerPrimary => Context::Global,
            Action::ZoomIn | Action::ZoomOut => Context::TurnBased,
            Action::MoveFwd
            | Action::MoveBack
            | Action::MoveLeft
            | Action::MoveRight
            | Action::Dodge
            | Action::Attack => Context::RealTime,
            Action::ReplayPause | Action::ReplaySlower | Action::ReplayFaster => Context::Replay,
        }
    }

    /// The engine default bindings — exactly the keys the host hardcoded
    /// before this module existed (behavior-preserving refactor).
    // One key deliberately serves several actions across contexts (W =
    // zoom / move-forward); merging those arms would hide the table.
    #[allow(clippy::match_same_arms)]
    fn defaults(self) -> &'static [PhysInput] {
        match self {
            Action::Quit => &[PhysInput::Key(KeyCode::Escape)],
            Action::DebugHud => &[PhysInput::Key(KeyCode::F1)],
            Action::OpenBindings => &[PhysInput::Key(KeyCode::F2)],
            Action::OrbitLeft => &[PhysInput::Key(KeyCode::ArrowLeft)],
            Action::OrbitRight => &[PhysInput::Key(KeyCode::ArrowRight)],
            Action::OrbitUp => &[PhysInput::Key(KeyCode::ArrowUp)],
            Action::OrbitDown => &[PhysInput::Key(KeyCode::ArrowDown)],
            Action::PointerPrimary => &[PhysInput::Mouse(MouseButton::Left)],
            Action::ZoomIn => &[PhysInput::Key(KeyCode::KeyW)],
            Action::ZoomOut => &[PhysInput::Key(KeyCode::KeyS)],
            Action::MoveFwd => &[PhysInput::Key(KeyCode::KeyW)],
            Action::MoveBack => &[PhysInput::Key(KeyCode::KeyS)],
            Action::MoveLeft => &[PhysInput::Key(KeyCode::KeyA)],
            Action::MoveRight => &[PhysInput::Key(KeyCode::KeyD)],
            Action::Dodge => &[PhysInput::Key(KeyCode::Space)],
            Action::Attack => &[PhysInput::Mouse(MouseButton::Left)],
            Action::ReplayPause => &[PhysInput::Key(KeyCode::Space)],
            Action::ReplaySlower => &[PhysInput::Key(KeyCode::BracketLeft)],
            Action::ReplayFaster => &[PhysInput::Key(KeyCode::BracketRight)],
        }
    }
}

/// Which slot of a map action a physical input drives: the whole button,
/// or one pole of an axis / axis2 composite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Part {
    Press,
    Pos,
    Neg,
    Up,
    Down,
    Left,
    Right,
}

impl Part {
    /// The parts a map action of `kind` binds — one for a button, a pole
    /// pair for an axis, a quad for an axis2.
    fn of(kind: ActionKind) -> &'static [Part] {
        match kind {
            ActionKind::Button => &[Part::Press],
            ActionKind::Axis => &[Part::Pos, Part::Neg],
            ActionKind::Axis2 => &[Part::Up, Part::Down, Part::Left, Part::Right],
        }
    }

    /// A label suffix distinguishing an axis part in the rebind UI.
    fn suffix(self) -> &'static str {
        match self {
            Part::Press => "",
            Part::Pos => " +",
            Part::Neg => " −",
            Part::Up => " ↑",
            Part::Down => " ↓",
            Part::Left => " ←",
            Part::Right => " →",
        }
    }

    /// The `bindings.toml` key for an axis part (`pos`/`neg`/`up`/…).
    fn config_key(self) -> &'static str {
        match self {
            Part::Press => "",
            Part::Pos => "pos",
            Part::Neg => "neg",
            Part::Up => "up",
            Part::Down => "down",
            Part::Left => "left",
            Part::Right => "right",
        }
    }
}

/// One rebindable row in the key-bindings panel: a base action, or one
/// part of a map action (an axis pole, an axis2 direction).
pub struct BindSlot {
    /// Display label (`"Move ↑"`, `"Zoom in"`).
    pub label: String,
    /// The context the slot binds in (for the conflict rule + the panel's
    /// grouping).
    pub context: Context,
    /// The action/part this slot rebinds.
    pub target: ActionRef,
    /// The inputs currently bound to it (usually one).
    pub inputs: Vec<PhysInput>,
}

/// A resolved binding target: an engine base action, or one part of the
/// running map's declared action `index`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActionRef {
    Base(Action),
    Map { index: usize, part: Part },
}

/// A map-declared action as the binding table knows it: id + kind + a
/// display label for the rebind UI (the manifest's `label.en`, else the
/// id).
pub struct MapAction {
    pub id: String,
    pub kind: ActionKind,
    pub label: String,
}

/// The live value of one map action, driven by input dispatch and (from
/// plan step 3 on) read by the map's local script layer. Also shown on
/// the F1 debug HUD so a map author can watch bindings land.
#[derive(Clone, Copy, Debug)]
pub enum MapActionValue {
    Button(bool),
    Axis {
        pos: bool,
        neg: bool,
    },
    Axis2 {
        up: bool,
        down: bool,
        left: bool,
        right: bool,
    },
}

impl MapActionValue {
    fn new(kind: ActionKind) -> MapActionValue {
        match kind {
            ActionKind::Button => MapActionValue::Button(false),
            ActionKind::Axis => MapActionValue::Axis {
                pos: false,
                neg: false,
            },
            ActionKind::Axis2 => MapActionValue::Axis2 {
                up: false,
                down: false,
                left: false,
                right: false,
            },
        }
    }

    /// Apply one input edge to the slot `part` names. Mismatched parts
    /// cannot happen (entries are built from the same kind) and are
    /// ignored defensively.
    fn set(&mut self, part: Part, is_down: bool) {
        match (self, part) {
            (MapActionValue::Button(b), Part::Press) => *b = is_down,
            (MapActionValue::Axis { pos, .. }, Part::Pos) => *pos = is_down,
            (MapActionValue::Axis { neg, .. }, Part::Neg) => *neg = is_down,
            (MapActionValue::Axis2 { up, .. }, Part::Up) => *up = is_down,
            (MapActionValue::Axis2 { down, .. }, Part::Down) => *down = is_down,
            (MapActionValue::Axis2 { left, .. }, Part::Left) => *left = is_down,
            (MapActionValue::Axis2 { right, .. }, Part::Right) => *right = is_down,
            _ => {}
        }
    }

    /// The debug-HUD rendering of this value.
    pub fn describe(self) -> String {
        match self {
            MapActionValue::Button(b) => if b { "down" } else { "up" }.to_owned(),
            MapActionValue::Axis { pos, neg } => {
                format!("{:+}", i8::from(pos) - i8::from(neg))
            }
            MapActionValue::Axis2 {
                up,
                down,
                left,
                right,
            } => format!(
                "({:+}, {:+})",
                i8::from(right) - i8::from(left),
                i8::from(up) - i8::from(down)
            ),
        }
    }
}

/// The live values of all map actions, index-parallel to the manifest's
/// declaration order (= [`Bindings::map_actions`] = `ActionRef::Map`
/// indexes). Owned by the render bridge (`MapRender`), written by input
/// dispatch, read by the local script layer and the debug HUD.
pub struct MapActionStates(Vec<MapActionValue>);

impl MapActionStates {
    /// Fresh (all-released) values for the map's declarations.
    #[must_use]
    pub fn new(actions: &[ActionDecl]) -> MapActionStates {
        MapActionStates(
            actions
                .iter()
                .map(|a| MapActionValue::new(a.kind))
                .collect(),
        )
    }

    pub fn set(&mut self, index: usize, part: Part, is_down: bool) {
        if let Some(value) = self.0.get_mut(index) {
            value.set(part, is_down);
        }
    }

    pub fn get(&self, index: usize) -> Option<MapActionValue> {
        self.0.get(index).copied()
    }
}

/// One binding-table row.
#[derive(Clone)]
struct Entry {
    input: PhysInput,
    context: Context,
    action: ActionRef,
}

/// The resolved binding table: engine defaults + the running map's
/// declared actions, with the user's `bindings.toml` overrides applied.
pub struct Bindings {
    /// ~20 base + a handful of map entries — linear scan beats a map.
    entries: Vec<Entry>,
    /// The engine + map default table, snapshot before any user override,
    /// so the rebind UI can reset one action (or all) to its default.
    defaults: Vec<Entry>,
    /// The map's declared actions ([`ActionRef::Map`] indexes here).
    map_actions: Vec<MapAction>,
    /// The map's name, for the `[map."<name>"]` config section.
    map_name: String,
}

impl Bindings {
    /// The engine default table plus the map's declared defaults (no
    /// user overrides). Bad input names in a declaration are collected
    /// as warnings, not errors — a map must load even if a default key
    /// name is junk (the action is then bindable via config only).
    pub fn defaults(map_name: &str, actions: &[ActionDecl]) -> (Bindings, Vec<String>) {
        let mut warnings = Vec::new();
        let mut entries: Vec<Entry> = ALL_ACTIONS
            .into_iter()
            .flat_map(|action| {
                action.defaults().iter().map(move |&input| Entry {
                    input,
                    context: action.context(),
                    action: ActionRef::Base(action),
                })
            })
            .collect();
        let mut map_actions = Vec::with_capacity(actions.len());
        for (index, decl) in actions.iter().enumerate() {
            if let Some(default) = &decl.default {
                match parse_map_binding(decl.kind, default) {
                    Ok(parts) => entries.extend(parts.into_iter().map(|(input, part)| Entry {
                        input,
                        context: Context::MapGameplay,
                        action: ActionRef::Map { index, part },
                    })),
                    Err(err) => warnings.push(format!("action `{}`: {err}", decl.id)),
                }
            }
            map_actions.push(MapAction {
                id: decl.id.clone(),
                kind: decl.kind,
                label: decl
                    .label
                    .get("en")
                    .cloned()
                    .unwrap_or_else(|| decl.id.clone()),
            });
        }
        (
            Bindings {
                defaults: entries.clone(),
                entries,
                map_actions,
                map_name: map_name.to_owned(),
            },
            warnings,
        )
    }

    /// Defaults + the user's config, if one exists. A missing config file
    /// is seeded with a fully commented-out template (so `bindings.toml`
    /// documents itself); any malformed entry is reported to stderr and
    /// skipped — a bad config must never brick the host.
    pub fn load(map_name: &str, actions: &[ActionDecl]) -> Bindings {
        let (mut bindings, warnings) = Bindings::defaults(map_name, actions);
        for warning in &warnings {
            eprintln!("monada-host: manifest: {warning}");
        }
        if let Some(path) = config_path() {
            match fs::read_to_string(&path) {
                Ok(text) => {
                    for warning in bindings.apply_overrides(&text) {
                        eprintln!("monada-host: {}: {warning}", path.display());
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    bindings.write_template(&path);
                }
                Err(err) => eprintln!("monada-host: {}: {err}", path.display()),
            }
        }
        bindings
    }

    /// The running map's declared actions (index-aligned with
    /// [`ActionRef::Map`] and [`MapActionStates`]).
    pub fn map_actions(&self) -> &[MapAction] {
        &self.map_actions
    }

    /// Every rebindable slot, in panel display order: the engine base
    /// actions, then each map action expanded into its parts.
    #[must_use]
    pub fn slots(&self) -> Vec<BindSlot> {
        let mut out = Vec::new();
        for action in ALL_ACTIONS {
            let target = ActionRef::Base(action);
            out.push(BindSlot {
                label: action.label().to_owned(),
                context: action.context(),
                target,
                inputs: self.inputs_for(target),
            });
        }
        for (index, ma) in self.map_actions.iter().enumerate() {
            for &part in Part::of(ma.kind) {
                let target = ActionRef::Map { index, part };
                out.push(BindSlot {
                    label: format!("{}{}", ma.label, part.suffix()),
                    context: Context::MapGameplay,
                    target,
                    inputs: self.inputs_for(target),
                });
            }
        }
        out
    }

    /// The inputs currently bound to one target.
    fn inputs_for(&self, target: ActionRef) -> Vec<PhysInput> {
        self.entries
            .iter()
            .filter(|e| e.action == target)
            .map(|e| e.input)
            .collect()
    }

    /// The context a target binds in.
    fn target_context(target: ActionRef) -> Context {
        match target {
            ActionRef::Base(a) => a.context(),
            ActionRef::Map { .. } => Context::MapGameplay,
        }
    }

    /// The display label of a target (for the "unbound X" notice).
    fn target_label(&self, target: ActionRef) -> String {
        match target {
            ActionRef::Base(a) => a.label().to_owned(),
            ActionRef::Map { index, part } => {
                let name = self
                    .map_actions
                    .get(index)
                    .map_or("?", |m| m.label.as_str());
                format!("{name}{}", part.suffix())
            }
        }
    }

    /// Bind `input` to `target`, replacing whatever it was bound to and
    /// unbinding `input` from any other slot in the same context (a key
    /// means one thing per context). Returns the displaced slot's label,
    /// if the key was taken.
    ///
    /// Known limitation: if the displaced slot is one part of a map axis /
    /// axis2, that action is left with fewer parts than its kind needs.
    /// [`to_toml`](Self::to_toml) can't represent a partial axis (the
    /// manifest `ActionDefault` requires every pole), so on the next load
    /// that action falls back to its manifest default — the saved state
    /// diverges from the in-session one. Reaching it needs a deliberate
    /// cross-binding of a map-axis key onto another map action; the real
    /// fix is optional-part axes in `monada-format` (plan follow-up).
    pub fn rebind(&mut self, target: ActionRef, input: PhysInput) -> Option<String> {
        let ctx = Self::target_context(target);
        let displaced = self
            .entries
            .iter()
            .find(|e| e.input == input && e.context == ctx && e.action != target)
            .map(|e| self.target_label(e.action));
        // Drop the key from this context, and clear the target's slot,
        // then bind the two together.
        self.entries
            .retain(|e| !(e.input == input && e.context == ctx) && e.action != target);
        self.entries.push(Entry {
            input,
            context: ctx,
            action: target,
        });
        displaced
    }

    /// Restore one target's default binding.
    pub fn reset(&mut self, target: ActionRef) {
        self.entries.retain(|e| e.action != target);
        let restored: Vec<Entry> = self
            .defaults
            .iter()
            .filter(|e| e.action == target)
            .cloned()
            .collect();
        self.entries.extend(restored);
    }

    /// Restore every binding to its engine/map default.
    pub fn reset_all(&mut self) {
        self.entries = self.defaults.clone();
    }

    /// Whether the current table differs from the defaults (drives the
    /// panel's "modified" hint / whether a save is worthwhile).
    #[must_use]
    pub fn is_modified(&self) -> bool {
        // Compare as sets — rebind/reset reorder the entry list. A single
        // full-tuple debug string is a cheap total order over the ~20
        // rows (no Ord on the input/context/action types).
        let sorted = |entries: &[Entry]| {
            let mut keys: Vec<String> = entries
                .iter()
                .map(|e| format!("{:?}", (e.input, e.context, e.action)))
                .collect();
            keys.sort();
            keys
        };
        sorted(&self.entries) != sorted(&self.defaults)
    }

    /// Serialise the current table to `bindings.toml` form: base actions
    /// grouped by context section, map actions reassembled under
    /// `[map."<name>"]` in their declared shapes.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let mut text =
            String::from("# monada key bindings — written by the in-app rebind panel.\n");
        for context in [
            Context::Global,
            Context::TurnBased,
            Context::RealTime,
            Context::Replay,
        ] {
            let _ = write!(text, "\n[{}]\n", context.section());
            for action in ALL_ACTIONS {
                if action.context() != context {
                    continue;
                }
                let names: Vec<String> = self
                    .inputs_for(ActionRef::Base(action))
                    .iter()
                    .map(|i| format!("\"{}\"", i.name()))
                    .collect();
                let _ = writeln!(text, "\"{}\" = [{}]", action.id(), names.join(", "));
            }
        }
        if !self.map_actions.is_empty() {
            let _ = write!(text, "\n[map.\"{}\"]\n", self.map_name);
            for (index, ma) in self.map_actions.iter().enumerate() {
                if let Some(value) = self.map_action_toml(index, ma) {
                    let _ = writeln!(text, "\"{}\" = {value}", ma.id);
                }
            }
        }
        text
    }

    /// One map action's value in `bindings.toml` form, or `None` if a
    /// required axis part is unbound (can't form a valid shape — the
    /// loader then falls back to the manifest default).
    fn map_action_toml(&self, index: usize, ma: &MapAction) -> Option<String> {
        let first = |part: Part| {
            self.entries
                .iter()
                .find(|e| e.action == ActionRef::Map { index, part })
                .map(|e| e.input.name())
        };
        match ma.kind {
            ActionKind::Button => {
                let names: Vec<String> = self
                    .inputs_for(ActionRef::Map {
                        index,
                        part: Part::Press,
                    })
                    .iter()
                    .map(|i| format!("\"{}\"", i.name()))
                    .collect();
                Some(format!("[{}]", names.join(", ")))
            }
            ActionKind::Axis | ActionKind::Axis2 => {
                let parts = Part::of(ma.kind);
                let mut fields = Vec::with_capacity(parts.len());
                for &part in parts {
                    // A partly-unbound axis can't be written (the shape needs
                    // every pole); `None` here drops the whole action, so it
                    // reloads at the manifest default — see `rebind`'s note.
                    let name = first(part)?;
                    fields.push(format!("{} = \"{name}\"", part.config_key()));
                }
                Some(format!("{{ {} }}", fields.join(", ")))
            }
        }
    }

    /// Write the current table to the user's `bindings.toml`. Best-effort
    /// — a read-only config dir just means the change stays in-session.
    pub fn save(&self) {
        let Some(path) = config_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            if fs::create_dir_all(dir).is_err() {
                return;
            }
        }
        if let Err(err) = fs::write(&path, self.to_toml()) {
            eprintln!("monada-host: {}: {err}", path.display());
        }
    }

    /// Apply `bindings.toml` overrides. Each `[context]` section maps a
    /// base action id to the list of inputs bound to it; a
    /// `[map."<name>"]` section rebinds that map's declared actions (in
    /// the manifest's default-binding shapes). Listing an action
    /// *replaces* its defaults (an empty list unbinds a button). Returns
    /// human-readable warnings for entries that could not be applied.
    fn apply_overrides(&mut self, text: &str) -> Vec<String> {
        let mut warnings = Vec::new();
        let doc: toml::Table = match text.parse() {
            Ok(doc) => doc,
            Err(err) => {
                warnings.push(format!("parse error: {err}"));
                return warnings;
            }
        };
        for (section, value) in &doc {
            if section == "map" {
                self.apply_map_overrides(value, &mut warnings);
                continue;
            }
            let Some(context) = Context::from_section(section) else {
                warnings.push(format!("unknown context [{section}]"));
                continue;
            };
            let Some(table) = value.as_table() else {
                warnings.push(format!("[{section}] is not a table"));
                continue;
            };
            for (id, inputs) in table {
                match Action::from_id(id) {
                    Some(action) if action.context() == context => {
                        self.rebind_base(action, inputs, &mut warnings);
                    }
                    Some(action) => warnings.push(format!(
                        "action `{id}` belongs in [{}], not [{section}]",
                        action.context().section()
                    )),
                    None => warnings.push(format!("unknown action `{id}` in [{section}]")),
                }
            }
        }
        warnings
    }

    /// Apply the `[map."<name>"]` sections; only the loaded map's
    /// section is consulted, other maps' rebinds stay dormant.
    fn apply_map_overrides(&mut self, value: &toml::Value, warnings: &mut Vec<String>) {
        let Some(by_map) = value.as_table() else {
            warnings.push("[map] is not a table".to_owned());
            return;
        };
        let Some(section) = by_map.get(&self.map_name).and_then(|v| v.as_table()) else {
            return;
        };
        for (id, binding) in section.clone() {
            let Some(index) = self.map_actions.iter().position(|a| a.id == id) else {
                warnings.push(format!("map {:?} declares no action `{id}`", self.map_name));
                continue;
            };
            let kind = self.map_actions[index].kind;
            match toml_map_binding(kind, &binding) {
                Ok(parts) => {
                    self.entries.retain(
                        |e| !matches!(e.action, ActionRef::Map { index: i, .. } if i == index),
                    );
                    self.entries
                        .extend(parts.into_iter().map(|(input, part)| Entry {
                            input,
                            context: Context::MapGameplay,
                            action: ActionRef::Map { index, part },
                        }));
                }
                Err(err) => warnings.push(format!("`{id}`: {err}")),
            }
        }
    }

    /// Replace a base `action`'s bindings with the config value (an
    /// array of input names). On any bad name the whole entry is
    /// rejected, so a typo degrades to the default binding rather than
    /// to nothing.
    fn rebind_base(&mut self, action: Action, inputs: &toml::Value, warnings: &mut Vec<String>) {
        let Some(list) = inputs.as_array() else {
            warnings.push(format!("`{}` must be an array of input names", action.id()));
            return;
        };
        let mut parsed = Vec::with_capacity(list.len());
        for value in list {
            let Some(input) = value.as_str().and_then(PhysInput::parse) else {
                warnings.push(format!("`{}`: unknown input {value}", action.id()));
                return;
            };
            parsed.push(input);
        }
        self.entries.retain(|e| e.action != ActionRef::Base(action));
        self.entries.extend(parsed.into_iter().map(|input| Entry {
            input,
            context: action.context(),
            action: ActionRef::Base(action),
        }));
    }

    /// Resolve a physical input against the active context stack
    /// (topmost context wins).
    pub fn resolve(&self, stack: &[Context], input: PhysInput) -> Option<ActionRef> {
        stack.iter().rev().find_map(|&context| {
            self.entries
                .iter()
                .find(|e| e.input == input && e.context == context)
                .map(|e| e.action)
        })
    }

    /// Best-effort: seed a commented-out template at `path` so the file
    /// documents the action ids and their defaults. Failure is silent —
    /// a read-only config dir just means no template. Map actions are
    /// not templated (their defaults live in each map's manifest); the
    /// header documents the `[map."<name>"]` form instead.
    fn write_template(&self, path: &PathBuf) {
        let mut text = String::from(
            "# monada key bindings. Uncomment a line to override the default.\n\
             # An action's value is a list of inputs: winit KeyCode names\n\
             # (\"KeyW\", \"Space\", \"BracketLeft\", ...) or \"MouseLeft\" /\n\
             # \"MouseRight\" / \"MouseMiddle\". An empty list unbinds.\n\
             #\n\
             # A map's own actions rebind under its name, in the shapes its\n\
             # manifest uses, e.g.:\n\
             #   [map.\"Chess\"]\n\
             #   \"cast_fireball\" = [\"KeyR\"]\n\
             #   \"move\" = { up = \"KeyI\", down = \"KeyK\", left = \"KeyJ\", right = \"KeyL\" }\n",
        );
        for context in [
            Context::Global,
            Context::TurnBased,
            Context::RealTime,
            Context::Replay,
        ] {
            let _ = write!(text, "\n[{}]\n", context.section());
            for action in ALL_ACTIONS {
                if action.context() != context {
                    continue;
                }
                let names: Vec<String> = self
                    .entries
                    .iter()
                    .filter(|e| e.action == ActionRef::Base(action))
                    .map(|e| format!("\"{}\"", e.input.name()))
                    .collect();
                let _ = writeln!(text, "# \"{}\" = [{}]", action.id(), names.join(", "));
            }
        }
        if let Some(dir) = path.parent() {
            if fs::create_dir_all(dir).is_ok() {
                let _ = fs::write(path, text);
            }
        }
    }
}

/// Lower a manifest [`ActionDefault`] into `(input, part)` rows. The
/// shape/kind match is already validated by `monada-format`; a name that
/// fails to parse as a physical input is an error here (the format crate
/// stays window-system agnostic).
fn parse_map_binding(
    kind: ActionKind,
    default: &ActionDefault,
) -> Result<Vec<(PhysInput, Part)>, String> {
    let one = |name: &str, part: Part| {
        PhysInput::parse(name)
            .map(|input| (input, part))
            .ok_or_else(|| format!("unknown input {name:?}"))
    };
    match (kind, default) {
        (ActionKind::Button, ActionDefault::Inputs(names)) => {
            names.iter().map(|name| one(name, Part::Press)).collect()
        }
        (ActionKind::Axis, ActionDefault::AxisPair { pos, neg }) => {
            Ok(vec![one(pos, Part::Pos)?, one(neg, Part::Neg)?])
        }
        (
            ActionKind::Axis2,
            ActionDefault::Axis2Quad {
                up,
                down,
                left,
                right,
            },
        ) => Ok(vec![
            one(up, Part::Up)?,
            one(down, Part::Down)?,
            one(left, Part::Left)?,
            one(right, Part::Right)?,
        ]),
        _ => Err("default shape does not match kind".to_owned()),
    }
}

/// Parse a `[map."<name>"]` override value into `(input, part)` rows,
/// accepting the same shapes as the manifest's `default`.
fn toml_map_binding(
    kind: ActionKind,
    value: &toml::Value,
) -> Result<Vec<(PhysInput, Part)>, String> {
    let default: ActionDefault = value
        .clone()
        .try_into()
        .map_err(|e| format!("bad binding shape: {e}"))?;
    parse_map_binding(kind, &default)
}

/// `<config dir>/monada/bindings.toml`: `%APPDATA%` on Windows,
/// `$XDG_CONFIG_HOME` else `~/.config` elsewhere (XDG is a unix
/// convention — a stray `XDG_CONFIG_HOME` on Windows is ignored).
fn config_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
    }?;
    Some(base.join("monada").join("bindings.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: &[Context] = &[Context::Global, Context::TurnBased, Context::MapGameplay];
    const REAL: &[Context] = &[Context::Global, Context::RealTime, Context::MapGameplay];
    const REPLAY: &[Context] = &[
        Context::Global,
        Context::TurnBased,
        Context::MapGameplay,
        Context::Replay,
    ];

    fn base() -> Bindings {
        Bindings::defaults("Test", &[]).0
    }

    fn key(c: KeyCode) -> PhysInput {
        PhysInput::Key(c)
    }

    #[test]
    fn defaults_match_the_old_hardcoded_keys() {
        let b = base();
        let base_of = |r: Option<ActionRef>| match r {
            Some(ActionRef::Base(a)) => Some(a),
            _ => None,
        };
        assert_eq!(
            base_of(b.resolve(TURN, key(KeyCode::Escape))),
            Some(Action::Quit)
        );
        assert_eq!(
            base_of(b.resolve(TURN, key(KeyCode::KeyW))),
            Some(Action::ZoomIn)
        );
        assert_eq!(
            base_of(b.resolve(REAL, key(KeyCode::KeyW))),
            Some(Action::MoveFwd)
        );
        assert_eq!(
            base_of(b.resolve(REAL, key(KeyCode::Space))),
            Some(Action::Dodge)
        );
        assert_eq!(
            base_of(b.resolve(REPLAY, key(KeyCode::Space))),
            Some(Action::ReplayPause)
        );
        assert_eq!(
            base_of(b.resolve(REPLAY, key(KeyCode::KeyW))),
            Some(Action::ZoomIn),
            "replay keeps turn-based zoom via fall-through"
        );
        // The same physical button lands on different actions per stack.
        let click = PhysInput::Mouse(MouseButton::Left);
        assert_eq!(
            base_of(b.resolve(TURN, click)),
            Some(Action::PointerPrimary)
        );
        assert_eq!(base_of(b.resolve(REAL, click)), Some(Action::Attack));
        // Unbound keys resolve to nothing (A/D in turn-based).
        assert_eq!(b.resolve(TURN, key(KeyCode::KeyA)), None);
    }

    #[test]
    fn override_replaces_the_default() {
        let mut b = base();
        let warnings = b.apply_overrides("[turnbased]\n\"camera.zoom_in\" = [\"KeyE\"]\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            b.resolve(TURN, key(KeyCode::KeyE)),
            Some(ActionRef::Base(Action::ZoomIn))
        );
        assert_eq!(b.resolve(TURN, key(KeyCode::KeyW)), None);
        // Other contexts untouched: W still moves in real-time.
        assert_eq!(
            b.resolve(REAL, key(KeyCode::KeyW)),
            Some(ActionRef::Base(Action::MoveFwd))
        );
    }

    #[test]
    fn empty_list_unbinds_and_mouse_names_parse() {
        let mut b = base();
        let warnings = b.apply_overrides(
            "[global]\n\"ui.debug_hud\" = []\n[realtime]\n\"game.attack\" = [\"MouseRight\"]\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(b.resolve(TURN, key(KeyCode::F1)), None);
        assert_eq!(
            b.resolve(REAL, PhysInput::Mouse(MouseButton::Right)),
            Some(ActionRef::Base(Action::Attack))
        );
        // Left click now falls through to the global pointer action.
        assert_eq!(
            b.resolve(REAL, PhysInput::Mouse(MouseButton::Left)),
            Some(ActionRef::Base(Action::PointerPrimary))
        );
    }

    #[test]
    fn bad_entries_warn_and_keep_defaults() {
        let mut b = base();
        let warnings = b.apply_overrides(
            "[global]\n\"ui.quit\" = [\"NotAKey\"]\n\"no.such\" = [\"KeyQ\"]\n\
             \"camera.zoom_in\" = [\"KeyZ\"]\n[weird]\nx = 1\n",
        );
        // Bad key name, unknown action, wrong section, unknown context.
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        // The rejected rebind keeps its default rather than unbinding.
        assert_eq!(
            b.resolve(TURN, key(KeyCode::Escape)),
            Some(ActionRef::Base(Action::Quit))
        );
    }

    fn decls() -> Vec<ActionDecl> {
        let manifest: monada_format::Manifest = toml::from_str(
            r#"
            name = "Test"
            engine_version = "0.1.0"
            players = 1
            sim_hz = "25"
            script_runtime = "rhai"
            entry = "scripts/main.rhai"

            [[action]]
            id = "cast"
            kind = "button"
            default = ["KeyQ"]

            [[action]]
            id = "strafe"
            kind = "axis2"
            default = { up = "KeyI", down = "KeyK", left = "KeyJ", right = "KeyL" }
        "#,
        )
        .unwrap();
        manifest.actions
    }

    #[test]
    fn map_actions_bind_and_shadow() {
        let (b, warnings) = Bindings::defaults("Test", &decls());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            b.resolve(REAL, key(KeyCode::KeyQ)),
            Some(ActionRef::Map {
                index: 0,
                part: Part::Press
            })
        );
        assert_eq!(
            b.resolve(REAL, key(KeyCode::KeyJ)),
            Some(ActionRef::Map {
                index: 1,
                part: Part::Left
            })
        );
        // Map context sits above the engine gameplay context…
        let (b2, _) = Bindings::defaults(
            "Test",
            &[ActionDecl {
                id: "grab".into(),
                kind: ActionKind::Button,
                default: Some(ActionDefault::Inputs(vec!["KeyW".into()])),
                context: None,
                label: std::collections::BTreeMap::new(),
            }],
        );
        assert!(matches!(
            b2.resolve(REAL, key(KeyCode::KeyW)),
            Some(ActionRef::Map { index: 0, .. })
        ));
        // …but replay transport stays on top.
        let (b3, _) = Bindings::defaults(
            "Test",
            &[ActionDecl {
                id: "jump".into(),
                kind: ActionKind::Button,
                default: Some(ActionDefault::Inputs(vec!["Space".into()])),
                context: None,
                label: std::collections::BTreeMap::new(),
            }],
        );
        assert_eq!(
            b3.resolve(REPLAY, key(KeyCode::Space)),
            Some(ActionRef::Base(Action::ReplayPause))
        );
    }

    #[test]
    fn map_config_section_rebinds_by_name() {
        let (mut b, _) = Bindings::defaults("Test", &decls());
        let warnings = b.apply_overrides(
            "[map.\"Test\"]\n\"cast\" = [\"KeyR\"]\n\
             \"strafe\" = { up = \"KeyT\", down = \"KeyG\", left = \"KeyF\", right = \"KeyH\" }\n\
             [map.\"Other\"]\n\"cast\" = [\"KeyZ\"]\n",
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            b.resolve(REAL, key(KeyCode::KeyR)),
            Some(ActionRef::Map {
                index: 0,
                part: Part::Press
            })
        );
        assert_eq!(
            b.resolve(REAL, key(KeyCode::KeyQ)),
            None,
            "default replaced"
        );
        assert_eq!(
            b.resolve(REAL, key(KeyCode::KeyT)),
            Some(ActionRef::Map {
                index: 1,
                part: Part::Up
            })
        );
        // Another map's section is dormant.
        assert_eq!(b.resolve(REAL, key(KeyCode::KeyZ)), None);
        // Unknown id in our section warns.
        let warnings = b.apply_overrides("[map.\"Test\"]\n\"no_such\" = [\"KeyZ\"]\n");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    // `name()` uses winit's Debug formatting while `parse()` goes through
    // its serde form — today the two agree, but nothing couples them.
    // This pins the round trip so a winit upgrade can't silently break
    // template generation.
    #[test]
    fn input_names_round_trip() {
        for action in ALL_ACTIONS {
            for &input in action.defaults() {
                assert_eq!(PhysInput::parse(&input.name()), Some(input), "{input:?}");
            }
        }
        for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
            let input = PhysInput::Mouse(button);
            assert_eq!(PhysInput::parse(&input.name()), Some(input));
        }
    }

    #[test]
    fn map_action_values_compose() {
        let mut v = MapActionValue::new(ActionKind::Axis2);
        v.set(Part::Up, true);
        v.set(Part::Right, true);
        assert_eq!(v.describe(), "(+1, +1)");
        v.set(Part::Up, false);
        v.set(Part::Down, true);
        assert_eq!(v.describe(), "(+1, -1)");
        let mut b = MapActionValue::new(ActionKind::Button);
        b.set(Part::Press, true);
        assert_eq!(b.describe(), "down");
    }

    #[test]
    fn slots_enumerate_base_then_map_parts() {
        let (b, _) = Bindings::defaults("Test", &decls());
        let slots = b.slots();
        // Every base action gets one slot.
        assert_eq!(
            slots
                .iter()
                .filter(|s| matches!(s.target, ActionRef::Base(_)))
                .count(),
            ALL_ACTIONS.len()
        );
        // `cast` (button) = 1 slot, `strafe` (axis2) = 4 slots.
        let map_slots: Vec<_> = slots
            .iter()
            .filter(|s| matches!(s.target, ActionRef::Map { .. }))
            .collect();
        assert_eq!(map_slots.len(), 5);
        let up = map_slots
            .iter()
            .find(|s| s.label == "strafe ↑")
            .expect("axis2 up slot");
        assert_eq!(up.inputs, vec![key(KeyCode::KeyI)]);
    }

    #[test]
    fn rebind_moves_a_key_and_reports_the_conflict() {
        let mut b = base();
        // Zoom-in defaults to W (turn-based). Rebind it to E: W frees up.
        let displaced = b.rebind(ActionRef::Base(Action::ZoomIn), key(KeyCode::KeyE));
        assert!(displaced.is_none());
        assert_eq!(
            b.resolve(TURN, key(KeyCode::KeyE)),
            Some(ActionRef::Base(Action::ZoomIn))
        );
        assert_eq!(b.resolve(TURN, key(KeyCode::KeyW)), None);
        // Now rebind zoom-out (currently S) to E too: it steals E from
        // zoom-in and the call reports the displaced action.
        let displaced = b.rebind(ActionRef::Base(Action::ZoomOut), key(KeyCode::KeyE));
        assert_eq!(displaced.as_deref(), Some("Zoom in"));
        assert_eq!(
            b.resolve(TURN, key(KeyCode::KeyE)),
            Some(ActionRef::Base(Action::ZoomOut))
        );
        // A different context is untouched: W still moves in real-time.
        assert_eq!(
            b.resolve(REAL, key(KeyCode::KeyW)),
            Some(ActionRef::Base(Action::MoveFwd))
        );
    }

    #[test]
    fn reset_restores_one_slot_and_all() {
        let mut b = base();
        b.rebind(ActionRef::Base(Action::ZoomIn), key(KeyCode::KeyE));
        assert!(b.is_modified());
        b.reset(ActionRef::Base(Action::ZoomIn));
        assert_eq!(
            b.resolve(TURN, key(KeyCode::KeyW)),
            Some(ActionRef::Base(Action::ZoomIn))
        );
        assert!(!b.is_modified());
        // reset_all after several edits.
        b.rebind(ActionRef::Base(Action::Quit), key(KeyCode::KeyP));
        b.rebind(ActionRef::Base(Action::ZoomOut), key(KeyCode::KeyO));
        b.reset_all();
        assert!(!b.is_modified());
        assert_eq!(
            b.resolve(TURN, key(KeyCode::Escape)),
            Some(ActionRef::Base(Action::Quit))
        );
    }

    #[test]
    fn to_toml_round_trips_including_map_axis2() {
        let (mut a, _) = Bindings::defaults("Test", &decls());
        // Rebind a base action and one axis2 part.
        a.rebind(ActionRef::Base(Action::ZoomIn), key(KeyCode::KeyE));
        a.rebind(
            ActionRef::Map {
                index: 1,
                part: Part::Up,
            },
            key(KeyCode::KeyT),
        );
        let toml = a.to_toml();
        // Reload onto a fresh default table and compare resolved bindings.
        let (mut b, _) = Bindings::defaults("Test", &decls());
        let warnings = b.apply_overrides(&toml);
        assert!(warnings.is_empty(), "{warnings:?}");
        for stack in [TURN, REAL] {
            for code in [KeyCode::KeyE, KeyCode::KeyT, KeyCode::KeyI, KeyCode::Escape] {
                assert_eq!(
                    a.resolve(stack, key(code)),
                    b.resolve(stack, key(code)),
                    "{stack:?} {code:?}"
                );
            }
        }
        // The rebinds actually took.
        assert_eq!(
            b.resolve(REAL, key(KeyCode::KeyT)),
            Some(ActionRef::Map {
                index: 1,
                part: Part::Up
            })
        );
    }
}
