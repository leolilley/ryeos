//! Affordance system — operator actions backed by UI verbs or Rye invocations.
//!
//! Surfaces declare commands (palette entries, keybindings, contextual actions).
//! Each affordance invokes either:
//! - A **UI-local verb** for cockpit behavior (focus, split, quit, etc.)
//! - A **Rye-native invocation** for daemon behavior (aliases, verbs, operations)
//!
//! Dispatch is code-owned. Surfaces expose data; Rust owns execution.
//! The TUI does NOT expand aliases or duplicate Rye dispatch rules.

use crate::effects::Effect;
use crate::ids::ThreadId;
use crate::ids::TileId;
use crate::layout::SplitAxis;
use crate::surface::ViewKindSpec;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Invocation — the target of an affordance
// ---------------------------------------------------------------------------

/// What an affordance invokes.
///
/// UI verbs are cockpit-local — they mutate the model through the reducer.
/// Rye invocations go through the daemon unexpanded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InvocationSpec {
    Ui(UiInvocation),
    Rye(RyeInvocation),
}

/// A UI-local verb invocation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UiInvocation {
    pub verb: UiVerb,
    #[serde(default)]
    pub args: serde_json::Value,
}

/// Closed enum of UI verbs — intentionally small, cockpit-only.
///
/// Rye operations (cancel_thread, execute_item, etc.) belong in `InvocationSpec::Rye`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiVerb {
    FocusNext,
    FocusPrevious,
    SwitchView,
    SplitPane,
    ClosePane,
    OpenOverlay,
    ResetLayout,
    NewSession,
    ToggleHelp,
    ToggleCommandPalette,
    Quit,
}

/// A Rye-native invocation sent to the daemon.
///
/// The TUI resolves selectors but does NOT expand aliases or
/// interpret verbs/operations locally. Those travel as-is to Rye.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeInvocation {
    /// UI context selector resolved by the TUI before dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<TargetSelector>,
    /// Rye alias to execute (e.g. "@deploy-prod").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Canonical item ref (e.g. "graph:deploy").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_ref: Option<String>,
    /// Rye verb (e.g. "cancel", "resume").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verb: Option<String>,
    /// Rye operation (e.g. "retry_failed_step").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// Additional arguments passed through.
    #[serde(default)]
    pub args: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Selectors — TUI-owned context resolution
// ---------------------------------------------------------------------------

/// A UI context selector that the TUI resolves to a concrete target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetSelector {
    FocusedThread,
    SelectedThread,
    HoveredNode,
    FocusedPane,
    FocusedPaneItem,
}

/// Result of resolving a target selector against UI context.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedTarget {
    Thread { thread_id: ThreadId },
    Item { item_ref: String },
    Node { node_id: String },
    Pane { tile_id: TileId },
    None,
}

// ---------------------------------------------------------------------------
// Affordance — a user-facing action declaration
// ---------------------------------------------------------------------------

/// An affordance that can be dispatched from the palette, keybinding, or action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Affordance {
    pub id: String,
    pub label: String,
    pub category: String,
    pub description: String,
    pub invoke: InvocationSpec,
    #[serde(default)]
    pub requires_capabilities: Vec<String>,
}

// ---------------------------------------------------------------------------
// Built-in affordances
// ---------------------------------------------------------------------------

/// Built-in affordance registry.
pub fn builtin_affordances() -> Vec<Affordance> {
    vec![
        affordance("focus.next", "Focus Next", "UI", "Move focus to next tile",
            InvocationSpec::Ui(UiInvocation { verb: UiVerb::FocusNext, args: serde_json::Value::Null })),
        affordance("focus.prev", "Focus Previous", "UI", "Move focus to previous tile",
            InvocationSpec::Ui(UiInvocation { verb: UiVerb::FocusPrevious, args: serde_json::Value::Null })),
        affordance("view.thread_list", "Threads", "View", "Switch focused tile to thread list",
            ui_switch(ViewKindSpec::ThreadList)),
        affordance("view.thread", "Thread", "View", "Switch to thread view",
            ui_switch(ViewKindSpec::Thread)),
        affordance("view.remotes", "Remotes", "View", "Switch to remotes view",
            ui_switch(ViewKindSpec::Remotes)),
        affordance("view.projects", "Projects", "View", "Switch to projects view",
            ui_switch(ViewKindSpec::Projects)),
        affordance("view.items", "Items", "View", "Browse items in space",
            ui_switch(ViewKindSpec::SpaceBrowser)),
        affordance("view.trust", "Trust", "View", "View trust status",
            ui_switch(ViewKindSpec::Trust)),
        affordance("view.graph", "Graph", "View", "View graph topology",
            ui_switch(ViewKindSpec::Graph)),
        affordance("view.events", "Events", "View", "Inspect raw events",
            ui_switch(ViewKindSpec::EventInspector)),
        affordance("layout.split_h", "Split Horizontal", "Layout", "Split focused tile left/right",
            InvocationSpec::Ui(UiInvocation {
                verb: UiVerb::SplitPane,
                args: serde_json::json!({ "axis": "horizontal", "view": "thread_list" }),
            })),
        affordance("layout.split_v", "Split Vertical", "Layout", "Split focused tile top/bottom",
            InvocationSpec::Ui(UiInvocation {
                verb: UiVerb::SplitPane,
                args: serde_json::json!({ "axis": "vertical", "view": "event_inspector" }),
            })),
        affordance("layout.close", "Close Tile", "Layout", "Close the focused tile",
            InvocationSpec::Ui(UiInvocation { verb: UiVerb::ClosePane, args: serde_json::Value::Null })),
        affordance("layout.reset", "Reset Layout", "Layout", "Reset to surface layout",
            InvocationSpec::Ui(UiInvocation { verb: UiVerb::ResetLayout, args: serde_json::Value::Null })),
        affordance("session.new", "New Session", "Session", "Clear input and start fresh",
            InvocationSpec::Ui(UiInvocation { verb: UiVerb::NewSession, args: serde_json::Value::Null })),
        affordance("help", "Help", "App", "Show help overlay",
            InvocationSpec::Ui(UiInvocation { verb: UiVerb::ToggleHelp, args: serde_json::Value::Null })),
        affordance("palette", "Command Palette", "App", "Open command palette",
            InvocationSpec::Ui(UiInvocation { verb: UiVerb::ToggleCommandPalette, args: serde_json::Value::Null })),
        affordance("app.quit", "Quit", "App", "Exit the TUI",
            InvocationSpec::Ui(UiInvocation { verb: UiVerb::Quit, args: serde_json::Value::Null })),
    ]
}

fn affordance(id: &str, label: &str, category: &str, desc: &str, invoke: InvocationSpec) -> Affordance {
    Affordance {
        id: id.into(),
        label: label.into(),
        category: category.into(),
        description: desc.into(),
        invoke,
        requires_capabilities: Vec::new(),
    }
}

fn ui_switch(view: ViewKindSpec) -> InvocationSpec {
    InvocationSpec::Ui(UiInvocation {
        verb: UiVerb::SwitchView,
        args: serde_json::to_value(&view).unwrap_or(serde_json::Value::Null),
    })
}

// ---------------------------------------------------------------------------
// Merge — built-in affordances + surface commands
// ---------------------------------------------------------------------------

/// Merge built-in affordances with surface-declared commands.
/// Surface commands override built-in affordances with the same id.
/// Surface commands with unknown invoke kinds are filtered out with warnings.
pub fn merge_affordances(
    surface_commands: &[crate::surface::SurfaceCommandSpec],
) -> (Vec<Affordance>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut registry: Vec<Affordance> = builtin_affordances();

    for spec in surface_commands {
        let invoke = match spec_to_invocation(spec) {
            Some(inv) => inv,
            None => {
                warnings.push(format!(
                    "surface command '{}' has no invocable target, ignored",
                    spec.id
                ));
                continue;
            }
        };

        if let Some(pos) = registry.iter().position(|a| a.id == spec.id) {
            // Override built-in by id
            registry[pos].label.clone_from(&spec.label);
            registry[pos].description.clone_from(&spec.description);
            if !spec.category.is_empty() {
                registry[pos].category.clone_from(&spec.category);
            }
            registry[pos].invoke = invoke;
            if !spec.requires_capabilities.is_empty() {
                registry[pos].requires_capabilities.clone_from(&spec.requires_capabilities);
            }
        } else {
            registry.push(Affordance {
                id: spec.id.clone(),
                label: spec.label.clone(),
                category: spec.category.clone(),
                description: spec.description.clone(),
                invoke,
                requires_capabilities: spec.requires_capabilities.clone(),
            });
        }
    }

    (registry, warnings)
}

/// Convert a surface `SurfaceCommandSpec` to an `InvocationSpec`.
/// Returns None if the command has no invocable target.
fn spec_to_invocation(spec: &crate::surface::SurfaceCommandSpec) -> Option<InvocationSpec> {
    spec.invoke.clone()
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch an affordance invocation against the model.
/// Returns (handled, effects).
pub fn dispatch_affordance(
    invocation: &InvocationSpec,
    model: &mut crate::model::AppModel,
) -> (bool, Vec<Effect>) {
    match invocation {
        InvocationSpec::Ui(ui) => dispatch_ui_verb(&ui.verb, &ui.args, model),
        InvocationSpec::Rye(rye) => dispatch_rye_invocation(rye, model),
    }
}

/// Dispatch a UI verb against the model.
fn dispatch_ui_verb(
    verb: &UiVerb,
    args: &serde_json::Value,
    model: &mut crate::model::AppModel,
) -> (bool, Vec<Effect>) {
    match verb {
        UiVerb::FocusNext => {
            model.workspace.focus_next();
            model.mark_dirty();
            (true, Vec::new())
        }
        UiVerb::FocusPrevious => {
            model.workspace.focus_prev();
            model.mark_dirty();
            (true, Vec::new())
        }
        UiVerb::SwitchView => {
            let view: ViewKindSpec = match serde_json::from_value(args.clone()) {
                Ok(v) => v,
                Err(_) => return (false, Vec::new()),
            };
            let tile_id = model.workspace.focused_tile;
            if let Some(tile) = model.workspace.tiles.get_mut(&tile_id) {
                tile.view = view.to_view_spec();
                tile.local = view.initial_local_state();
            }
            model.mark_dirty();
            (true, Vec::new())
        }
        UiVerb::SplitPane => {
            let axis = args.get("axis")
                .and_then(|v| v.as_str())
                .and_then(|s| match s {
                    "horizontal" => Some(SplitAxis::Horizontal),
                    "vertical" => Some(SplitAxis::Vertical),
                    _ => None,
                })
                .unwrap_or(SplitAxis::Horizontal);
            let view_kind = args.get("view")
                .and_then(|v| v.as_str())
                .and_then(|s| parse_view_kind(s))
                .unwrap_or(ViewKindSpec::ThreadList);
            model.workspace.split_focused(axis, view_kind.to_view_spec());
            model.mark_dirty();
            (true, vec![Effect::PersistSession])
        }
        UiVerb::ClosePane => {
            model.workspace.close_focused();
            model.mark_dirty();
            (true, vec![Effect::PersistSession])
        }
        UiVerb::ResetLayout => {
            model.workspace = model.surface.rebuild_workspace();
            model.mark_dirty();
            (true, vec![Effect::PersistSession])
        }
        UiVerb::NewSession => {
            model.workspace.input_bar.clear();
            model.mark_dirty();
            (true, Vec::new())
        }
        UiVerb::ToggleHelp => {
            model.overlay = match model.overlay {
                Some(crate::model::OverlayState::Help) => None,
                _ => Some(crate::model::OverlayState::Help),
            };
            model.mark_dirty();
            (true, Vec::new())
        }
        UiVerb::ToggleCommandPalette => {
            model.overlay = match model.overlay {
                Some(crate::model::OverlayState::CommandPalette { .. }) => None,
                _ => Some(crate::model::OverlayState::CommandPalette {
                    query: String::new(),
                    selected: 0,
                }),
            };
            model.mark_dirty();
            (true, Vec::new())
        }
        UiVerb::OpenOverlay => {
            // Overlays are opened via ToggleHelp/ToggleCommandPalette or Confirm.
            // If args specify a type, open that overlay; otherwise no-op.
            let overlay_type = args.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match overlay_type {
                "help" => {
                    model.overlay = Some(crate::model::OverlayState::Help);
                }
                "palette" => {
                    model.overlay = Some(crate::model::OverlayState::CommandPalette {
                        query: String::new(),
                        selected: 0,
                    });
                }
                "confirm" => {
                    let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("Confirm?");
                    let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
                    model.overlay = Some(crate::model::OverlayState::Confirm {
                        message: message.to_string(),
                        action: action.to_string(),
                    });
                }
                _ => return (false, Vec::new()),
            }
            model.mark_dirty();
            (true, Vec::new())
        }
        UiVerb::Quit => (true, vec![Effect::Quit]),
    }
}

/// Parse a view kind string from affordance args.
fn parse_view_kind(s: &str) -> Option<ViewKindSpec> {
    match s {
        "thread_list" => Some(ViewKindSpec::ThreadList),
        "thread" => Some(ViewKindSpec::Thread),
        "remotes" => Some(ViewKindSpec::Remotes),
        "projects" => Some(ViewKindSpec::Projects),
        "space_browser" => Some(ViewKindSpec::SpaceBrowser),
        "trust" => Some(ViewKindSpec::Trust),
        "graph" => Some(ViewKindSpec::Graph),
        "event_inspector" => Some(ViewKindSpec::EventInspector),
        _ => None,
    }
}

/// Dispatch a Rye invocation.
///
/// The TUI resolves selectors and constructs daemon requests.
/// It does NOT expand aliases or interpret verbs/operations locally.
fn dispatch_rye_invocation(
    invocation: &RyeInvocation,
    model: &mut crate::model::AppModel,
) -> (bool, Vec<Effect>) {
    // If the invocation has an alias, send it to the daemon for execution.
    if let Some(alias) = &invocation.alias {
        // Build an execute request with the alias as the item_ref.
        // The daemon resolves the alias according to its own rules.
        let project_path = std::path::PathBuf::from("/tmp/ryeos");
        return (true, vec![Effect::Execute {
            project_path,
            item_ref: alias.clone(),
            parameters: invocation.args.clone(),
        }]);
    }

    // If the invocation has an item_ref + operation/verb, send to daemon.
    if let Some(item_ref) = &invocation.item_ref {
        // Resolve target selector if present
        let _resolved = invocation.target.as_ref().map(|sel| resolve_selector(sel, model));

        // For now, construct an execute effect. When daemon supports
        // targeted verb/operation APIs, this will use the appropriate
        // request type instead of generic execute.
        let project_path = std::path::PathBuf::from("/tmp/ryeos");
        return (true, vec![Effect::Execute {
            project_path,
            item_ref: item_ref.clone(),
            parameters: invocation.args.clone(),
        }]);
    }

    // If the invocation has a target + verb (e.g. cancel focused thread),
    // resolve the selector and dispatch.
    if let (Some(target), Some(verb)) = (&invocation.target, &invocation.verb) {
        let resolved = resolve_selector(target, model);
        match resolved {
            ResolvedTarget::Thread { thread_id } => {
                return (true, vec![Effect::SendThreadCommand {
                    thread_id,
                    command: verb_to_thread_command(verb),
                }]);
            }
            _ => return (false, Vec::new()),
        }
    }

    (false, Vec::new())
}

/// Resolve a target selector against the current UI context.
pub fn resolve_selector(
    selector: &TargetSelector,
    model: &crate::model::AppModel,
) -> ResolvedTarget {
    match selector {
        TargetSelector::FocusedThread => {
            // Find the thread tile and check if it has a thread_id
            let focused = model.workspace.focused_tile;
            if let Some(tile) = model.workspace.tiles.get(&focused) {
                if let crate::workspace::ViewSpec::Thread { thread_id: Some(tid) } = tile.view {
                    return ResolvedTarget::Thread { thread_id: tid };
                }
            }
            ResolvedTarget::None
        }
        TargetSelector::SelectedThread => {
            // Find the thread list tile and check cursor
            for (_tile_id, tile) in &model.workspace.tiles {
                if let crate::workspace::ViewSpec::ThreadList = tile.view {
                    if let crate::workspace::ViewLocalState::ThreadList { cursor, .. } = &tile.local {
                        let threads: Vec<_> = model.store.recent_threads();
                        if let Some(selected) = threads.into_iter().nth(*cursor) {
                            return ResolvedTarget::Thread { thread_id: selected.id };
                        }
                    }
                    break;
                }
            }
            ResolvedTarget::None
        }
        TargetSelector::FocusedPane => {
            ResolvedTarget::Pane { tile_id: model.workspace.focused_tile }
        }
        TargetSelector::FocusedPaneItem => ResolvedTarget::None,
        TargetSelector::HoveredNode => ResolvedTarget::None,
    }
}

/// Map a Rye verb string to a thread command.
fn verb_to_thread_command(verb: &str) -> crate::effects::ThreadCommand {
    match verb {
        "cancel" => crate::effects::ThreadCommand::Cancel,
        "kill" => crate::effects::ThreadCommand::Kill,
        "interrupt" => crate::effects::ThreadCommand::Interrupt,
        _ => crate::effects::ThreadCommand::Interrupt,
    }
}

// ---------------------------------------------------------------------------
// Filtering
// ---------------------------------------------------------------------------

/// Filter affordances by a query string (matches label, category, description, or id).
pub fn filter_affordances<'a>(affordances: &'a [Affordance], query: &str) -> Vec<&'a Affordance> {
    if query.is_empty() {
        return affordances.iter().collect();
    }
    let query_lower = query.to_lowercase();
    affordances
        .iter()
        .filter(|a| {
            a.label.to_lowercase().contains(&query_lower)
                || a.category.to_lowercase().contains(&query_lower)
                || a.description.to_lowercase().contains(&query_lower)
                || a.id.to_lowercase().contains(&query_lower)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Keybind parsing and keymap
// ---------------------------------------------------------------------------

use crate::input::Key;

/// A single key combo parsed from a string like `"ctrl+p"` or `"page_up"`.
///
/// Format: `[mod+mod+...]key` where mods are `ctrl`, `alt`, `shift`, `meta`
/// and key is a character or named key like `escape`, `enter`, `tab`, `page_up`, etc.
/// Multiple combos can be separated by commas: `"ctrl+p,mod+shift+p"`.
/// The special value `"none"` disables the binding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KeybindCombo {
    pub key: String,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

/// Parse a keybind config string into one or more combos.
/// Returns empty for `"none"` or empty string.
pub fn parse_keybind(config: &str) -> Vec<KeybindCombo> {
    if config.is_empty() || config == "none" {
        return Vec::new();
    }

    config.split(',').map(|part| {
        let mut combo = KeybindCombo {
            key: String::new(),
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
        };
        for token in part.trim().to_lowercase().split('+') {
            match token {
                "ctrl" | "control" => combo.ctrl = true,
                "alt" | "option" => combo.alt = true,
                "shift" => combo.shift = true,
                "meta" | "cmd" | "command" => combo.meta = true,
                _ => combo.key = token.to_string(),
            }
        }
        combo
    }).collect()
}

/// Try to match a `Key` event against a keybind combo.
pub fn match_keybind(combo: &KeybindCombo, key: &Key) -> bool {
    match key {
        Key::Ctrl(c) => {
            combo.ctrl
                && !combo.alt
                && !combo.shift
                && !combo.meta
                && combo.key == c.to_lowercase().to_string()
        }
        Key::Alt(c) => {
            combo.alt
                && !combo.ctrl
                && !combo.shift
                && !combo.meta
                && combo.key == c.to_lowercase().to_string()
        }
        Key::Char(c) => {
            !combo.ctrl
                && !combo.alt
                && !combo.meta
                && combo.shift == c.is_uppercase()
                && combo.key == c.to_lowercase().to_string()
        }
        Key::ArrowUp => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "arrowup",
        Key::ArrowDown => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "arrowdown",
        Key::ArrowLeft => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "arrowleft",
        Key::ArrowRight => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "arrowright",
        Key::Home => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "home",
        Key::End => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "end",
        Key::PageUp => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "pageup",
        Key::PageDown => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "pagedown",
        Key::Backspace => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "backspace",
        Key::Delete => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "delete",
        Key::Tab => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "tab",
        Key::ShiftTab => !combo.ctrl && !combo.alt && combo.shift && !combo.meta && combo.key == "tab",
        Key::Enter => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "enter",
        Key::Escape => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == "escape",
        Key::F(n) => !combo.ctrl && !combo.alt && !combo.shift && !combo.meta && combo.key == format!("f{}", n),
        _ => false,
    }
}

/// A keymap that maps affordance IDs to keybind config strings.
///
/// Built from defaults, with optional overrides from user config.
/// Lookup iterates all bindings and returns the affordance ID whose
/// keybind matches the pressed key.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Keymap {
    /// Maps affordance ID → keybind config string (e.g. `"ctrl+p"`).
    pub bindings: std::collections::HashMap<String, String>,
}

impl Keymap {
    /// Create a keymap with default bindings.
    pub fn defaults() -> Self {
        let mut bindings = std::collections::HashMap::new();
        // Global
        bindings.insert("app.quit".into(), "escape".into());
        bindings.insert("session.new".into(), "ctrl+n".into());
        bindings.insert("palette".into(), "ctrl+p".into());
        bindings.insert("help".into(), "shift+slash".into()); // ?
        // Focus
        bindings.insert("focus.next".into(), "tab".into());
        bindings.insert("focus.prev".into(), "shift+tab".into());
        // Layout
        bindings.insert("layout.split_h".into(), "ctrl+s".into());
        bindings.insert("layout.split_v".into(), "ctrl+v".into());
        bindings.insert("layout.close".into(), "ctrl+x".into());
        bindings.insert("layout.reset".into(), "ctrl+r".into());
        // Navigation
        bindings.insert("nav.up".into(), "arrowup".into());
        bindings.insert("nav.down".into(), "arrowdown".into());
        bindings.insert("nav.page_up".into(), "pageup".into());
        bindings.insert("nav.page_down".into(), "pagedown".into());
        bindings.insert("nav.toggle_expand".into(), "space".into());
        bindings.insert("nav.open".into(), "enter".into());
        Self { bindings }
    }

    /// Apply user overrides from config (affordance ID → keybind string).
    /// Overrides replace existing bindings. Value `"none"` disables a binding.
    pub fn apply_overrides(&mut self, overrides: &std::collections::HashMap<String, String>) {
        for (id, keybind) in overrides {
            if keybind == "none" {
                self.bindings.remove(id);
            } else {
                self.bindings.insert(id.clone(), keybind.clone());
            }
        }
    }

    /// Look up the affordance ID for a pressed key.
    /// Returns the first matching affordance ID, or None.
    pub fn lookup(&self, key: &Key) -> Option<&str> {
        for (affordance_id, config) in &self.bindings {
            let combos = parse_keybind(config);
            for combo in &combos {
                if match_keybind(combo, key) {
                    return Some(affordance_id.as_str());
                }
            }
        }
        None
    }

    /// Get the keybind config string for an affordance ID.
    /// Used by help overlay and settings UI.
    pub fn get_binding(&self, affordance_id: &str) -> Option<&str> {
        self.bindings.get(affordance_id).map(|s| s.as_str())
    }

    /// Get all bindings as an iterator (for help display).
    pub fn all_bindings(&self) -> impl Iterator<Item = (&str, &str)> {
        self.bindings.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

// ---------------------------------------------------------------------------
// Display formatting
// ---------------------------------------------------------------------------

/// Format a keybind config string for display in the help overlay.
///
/// Converts e.g. `"ctrl+p"` → `"Ctrl+P"`, `"shift+slash"` → `"?"`,
/// `"arrowup"` → `"↑"`, `"escape"` → `"Esc"`, `"space"` → `"Space"`.
pub fn format_keybind_display(config: &str) -> String {
    // Handle special named keys
    match config {
        "escape" => return "Esc".into(),
        "space" => return "Space".into(),
        "arrowup" => return "↑".into(),
        "arrowdown" => return "↓".into(),
        "arrowleft" => return "←".into(),
        "arrowright" => return "→".into(),
        "pageup" => return "PgUp".into(),
        "pagedown" => return "PgDn".into(),
        "shift+slash" => return "?".into(),
        "shift+tab" => return "Shift+Tab".into(),
        "tab" => return "Tab".into(),
        "enter" => return "Enter".into(),
        _ => {}
    }
    // General case: capitalize each part
    config
        .split('+')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_affordances_use_invocation_spec() {
        let affs = builtin_affordances();
        assert!(affs.len() >= 16, "should have at least 16 affordances");
        let ui_count = affs.iter().filter(|a| matches!(a.invoke, InvocationSpec::Ui(_))).count();
        assert!(ui_count >= 15, "most builtins should be UI verbs");
    }

    #[test]
    fn filter_finds_by_label() {
        let affs = builtin_affordances();
        let results = filter_affordances(&affs, "thread");
        assert!(!results.is_empty());
        assert!(results.iter().any(|a| a.label.contains("Thread")));
    }

    #[test]
    fn filter_finds_by_category() {
        let affs = builtin_affordances();
        let results = filter_affordances(&affs, "layout");
        assert!(results.iter().any(|a| a.category == "Layout"));
    }

    #[test]
    fn dispatch_ui_switch_view() {
        let mut model = crate::model::AppModel::new_default("/tmp/test");
        let inv = InvocationSpec::Ui(UiInvocation {
            verb: UiVerb::SwitchView,
            args: serde_json::to_value(ViewKindSpec::Trust).unwrap(),
        });
        let (handled, effects) = dispatch_affordance(&inv, &mut model);
        assert!(handled);
        assert!(effects.is_empty());
        let focused_view = model.workspace.focused_view();
        assert!(matches!(focused_view, Some(crate::workspace::ViewSpec::Trust)));
    }

    #[test]
    fn dispatch_ui_split_pane() {
        let mut model = crate::model::AppModel::new_default("/tmp/test");
        let initial_tiles = model.workspace.tiles.len();
        let inv = InvocationSpec::Ui(UiInvocation {
            verb: UiVerb::SplitPane,
            args: serde_json::json!({ "axis": "vertical", "view": "event_inspector" }),
        });
        let (handled, effects) = dispatch_affordance(&inv, &mut model);
        assert!(handled);
        assert_eq!(model.workspace.tiles.len(), initial_tiles + 1);
        assert!(effects.iter().any(|e| matches!(e, Effect::PersistSession)));
    }

    #[test]
    fn dispatch_ui_close_pane() {
        let mut model = crate::model::AppModel::new_default("/tmp/test");
        model.workspace.split_focused(SplitAxis::Vertical, crate::workspace::ViewSpec::EventInspector);
        let tiles_after_split = model.workspace.tiles.len();
        assert!(tiles_after_split > 3);

        let inv = InvocationSpec::Ui(UiInvocation {
            verb: UiVerb::ClosePane,
            args: serde_json::Value::Null,
        });
        let (handled, _) = dispatch_affordance(&inv, &mut model);
        assert!(handled);
        assert!(model.workspace.tiles.len() < tiles_after_split);
    }

    #[test]
    fn dispatch_ui_quit() {
        let mut model = crate::model::AppModel::new_default("/tmp/test");
        let inv = InvocationSpec::Ui(UiInvocation {
            verb: UiVerb::Quit,
            args: serde_json::Value::Null,
        });
        let (handled, effects) = dispatch_affordance(&inv, &mut model);
        assert!(handled);
        assert!(effects.iter().any(|e| matches!(e, Effect::Quit)));
    }

    #[test]
    fn dispatch_ui_toggle_help() {
        let mut model = crate::model::AppModel::new_default("/tmp/test");
        let inv = InvocationSpec::Ui(UiInvocation {
            verb: UiVerb::ToggleHelp,
            args: serde_json::Value::Null,
        });
        let (handled, _) = dispatch_affordance(&inv, &mut model);
        assert!(handled);
        assert!(matches!(model.overlay, Some(crate::model::OverlayState::Help)));
        // Toggle off
        let (handled, _) = dispatch_affordance(&inv, &mut model);
        assert!(handled);
        assert!(model.overlay.is_none());
    }

    #[test]
    fn dispatch_ui_reset_layout_rebuilds_from_surface() {
        let mut model = crate::model::AppModel::new_default("/tmp/test");
        let initial_tiles = model.workspace.tiles.len();
        model.workspace.split_focused(SplitAxis::Horizontal, crate::workspace::ViewSpec::Remotes);
        assert!(model.workspace.tiles.len() > initial_tiles);

        let inv = InvocationSpec::Ui(UiInvocation {
            verb: UiVerb::ResetLayout,
            args: serde_json::Value::Null,
        });
        let (handled, _) = dispatch_affordance(&inv, &mut model);
        assert!(handled);
        assert_eq!(model.workspace.tiles.len(), initial_tiles);
    }

    #[test]
    fn dispatch_rye_alias_produces_execute_effect() {
        let mut model = crate::model::AppModel::new_default("/tmp/test");
        let inv = InvocationSpec::Rye(RyeInvocation {
            target: None,
            alias: Some("@deploy-prod".into()),
            item_ref: None,
            verb: None,
            operation: None,
            args: serde_json::Value::Null,
        });
        let (handled, effects) = dispatch_affordance(&inv, &mut model);
        assert!(handled);
        assert_eq!(effects.len(), 1);
        assert!(matches!(effects[0], Effect::Execute { ref item_ref, .. } if item_ref == "@deploy-prod"));
    }

    #[test]
    fn dispatch_rye_target_verb_cancel_focused_thread() {
        let mut model = crate::model::AppModel::new_default("/tmp/test");
        let thread_id = crate::ids::ThreadId::new(42);
        // Find the thread tile, set its thread_id, and focus it
        for (tid, tile) in model.workspace.tiles.iter_mut() {
            if matches!(tile.view, crate::workspace::ViewSpec::Thread { .. }) {
                tile.view = crate::workspace::ViewSpec::Thread { thread_id: Some(thread_id) };
                model.workspace.focused_tile = *tid;
                break;
            }
        }

        let inv = InvocationSpec::Rye(RyeInvocation {
            target: Some(TargetSelector::FocusedThread),
            alias: None,
            item_ref: None,
            verb: Some("cancel".into()),
            operation: None,
            args: serde_json::Value::Null,
        });
        let (handled, effects) = dispatch_affordance(&inv, &mut model);
        assert!(handled);
        assert!(matches!(effects[0], Effect::SendThreadCommand { command: crate::effects::ThreadCommand::Cancel, .. }));
    }

    #[test]
    fn selector_resolve_focused_thread_returns_none_when_no_thread() {
        let model = crate::model::AppModel::new_default("/tmp/test");
        let resolved = resolve_selector(&TargetSelector::FocusedThread, &model);
        // If focused tile is a thread_list (not Thread view), returns None
        assert!(matches!(resolved, ResolvedTarget::None));
    }

    #[test]
    fn selector_resolve_focused_thread_returns_thread() {
        let mut model = crate::model::AppModel::new_default("/tmp/test");
        let thread_id = crate::ids::ThreadId::new(99);
        // Set the focused tile to Thread view with a thread_id
        for (tid, tile) in model.workspace.tiles.iter_mut() {
            if matches!(tile.view, crate::workspace::ViewSpec::Thread { .. }) {
                tile.view = crate::workspace::ViewSpec::Thread { thread_id: Some(thread_id) };
                model.workspace.focused_tile = *tid;
                break;
            }
        }
        let resolved = resolve_selector(&TargetSelector::FocusedThread, &model);
        assert_eq!(resolved, ResolvedTarget::Thread { thread_id });
    }

    #[test]
    fn merge_affordances_surface_command_overrides_builtin() {
        let surface_commands = vec![crate::surface::SurfaceCommandSpec {
            id: "layout.reset".into(),
            label: "Revert".into(),
            category: "Ops".into(),
            description: "Revert layout".into(),
            invoke: Some(InvocationSpec::Ui(UiInvocation {
                verb: UiVerb::ResetLayout,
                args: serde_json::Value::Null,
            })),
            requires_capabilities: vec!["ui.layout.reset".into()],
        }];
        let (merged, _) = merge_affordances(&surface_commands);
        let reset = merged.iter().find(|a| a.id == "layout.reset").unwrap();
        assert_eq!(reset.label, "Revert");
        assert_eq!(reset.category, "Ops");
        assert_eq!(reset.requires_capabilities, vec!["ui.layout.reset"]);
    }

    #[test]
    fn merge_affordances_new_surface_command() {
        let surface_commands = vec![crate::surface::SurfaceCommandSpec {
            id: "thread.cancel".into(),
            label: "Cancel".into(),
            category: "Thread".into(),
            description: "Cancel focused thread".into(),
            invoke: Some(InvocationSpec::Rye(RyeInvocation {
                target: Some(TargetSelector::FocusedThread),
                alias: None,
                item_ref: None,
                verb: Some("cancel".into()),
                operation: None,
                args: serde_json::Value::Null,
            })),
            requires_capabilities: vec!["thread.cancel".into()],
        }];
        let (merged, warnings) = merge_affordances(&surface_commands);
        assert!(warnings.is_empty());
        let cancel = merged.iter().find(|a| a.id == "thread.cancel").unwrap();
        assert_eq!(cancel.label, "Cancel");
        assert!(matches!(cancel.invoke, InvocationSpec::Rye(_)));
    }

    #[test]
    fn merge_affordances_warns_on_surface_command_without_invoke() {
        let surface_commands = vec![crate::surface::SurfaceCommandSpec {
            id: "ghost.action".into(),
            label: "Ghost".into(),
            category: "".into(),
            description: "".into(),
            invoke: None,
            requires_capabilities: Vec::new(),
        }];
        let (_, warnings) = merge_affordances(&surface_commands);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("ghost.action"));
    }

    #[test]
    fn rye_invocation_alias_serializes() {
        let inv = RyeInvocation {
            target: None,
            alias: Some("@deploy".into()),
            item_ref: None,
            verb: None,
            operation: None,
            args: serde_json::json!({ "env": "prod" }),
        };
        let json = serde_json::to_string(&inv).unwrap();
        assert!(json.contains("\"alias\":\"@deploy\""));

        let parsed: RyeInvocation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, inv);
    }

    #[test]
    fn rye_invocation_target_verb_serializes() {
        let inv = RyeInvocation {
            target: Some(TargetSelector::FocusedThread),
            alias: None,
            item_ref: None,
            verb: Some("cancel".into()),
            operation: None,
            args: serde_json::Value::Null,
        };
        let yaml = serde_yaml::to_string(&inv).unwrap();
        assert!(yaml.contains("focused_thread"));
        assert!(yaml.contains("cancel"));

        let parsed: RyeInvocation = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, inv);
    }

    #[test]
    fn ui_verb_serializes() {
        let inv = InvocationSpec::Ui(UiInvocation {
            verb: UiVerb::SwitchView,
            args: serde_json::json!("trust"),
        });
        let yaml = serde_yaml::to_string(&inv).unwrap();
        assert!(yaml.contains("ui"));
        assert!(yaml.contains("switch_view"));

        let parsed: InvocationSpec = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed, inv);
    }

    #[test]
    fn surface_command_spec_yaml_roundtrip() {
        let yaml = r#"
id: thread.cancel
label: Cancel Thread
category: Thread
description: Cancel the focused thread
invoke:
  kind: rye
  target: focused_thread
  verb: cancel
requires_capabilities:
  - thread.cancel
"#;
        let spec: crate::surface::SurfaceCommandSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(spec.id, "thread.cancel");
        assert_eq!(spec.label, "Cancel Thread");
        assert!(spec.invoke.is_some());
        assert_eq!(spec.requires_capabilities, vec!["thread.cancel"]);

        let inv = spec.invoke.unwrap();
        assert!(matches!(inv, InvocationSpec::Rye(_)));
    }

    // --- Keybind tests ---

    #[test]
    fn parse_keybind_single() {
        let combos = parse_keybind("ctrl+p");
        assert_eq!(combos.len(), 1);
        assert_eq!(combos[0].key, "p");
        assert!(combos[0].ctrl);
        assert!(!combos[0].alt);
    }

    #[test]
    fn parse_keybind_multi_combo() {
        let combos = parse_keybind("ctrl+p,alt+x");
        assert_eq!(combos.len(), 2);
        assert!(combos[0].ctrl);
        assert!(combos[1].alt);
    }

    #[test]
    fn parse_keybind_none() {
        assert!(parse_keybind("none").is_empty());
        assert!(parse_keybind("").is_empty());
    }

    #[test]
    fn match_ctrl_key() {
        let combo = &parse_keybind("ctrl+p")[0];
        assert!(match_keybind(combo, &Key::Ctrl('p')));
        assert!(match_keybind(combo, &Key::Ctrl('P')));
        assert!(!match_keybind(combo, &Key::Ctrl('x')));
        assert!(!match_keybind(combo, &Key::Char('p')));
    }

    #[test]
    fn match_named_key() {
        let combo = &parse_keybind("escape")[0];
        assert!(match_keybind(combo, &Key::Escape));
        assert!(!match_keybind(combo, &Key::Enter));
    }

    #[test]
    fn match_arrow_key() {
        let combo = &parse_keybind("arrowup")[0];
        assert!(match_keybind(combo, &Key::ArrowUp));
        assert!(!match_keybind(combo, &Key::ArrowDown));
    }

    #[test]
    fn keymap_defaults_lookup() {
        let km = Keymap::defaults();
        assert_eq!(km.lookup(&Key::Escape), Some("app.quit"));
        assert_eq!(km.lookup(&Key::Ctrl('p')), Some("palette"));
        assert_eq!(km.lookup(&Key::ArrowUp), Some("nav.up"));
        assert_eq!(km.lookup(&Key::ArrowDown), Some("nav.down"));
    }

    #[test]
    fn keymap_override_replaces() {
        let mut km = Keymap::defaults();
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("palette".into(), "ctrl+k".into());
        km.apply_overrides(&overrides);
        assert_eq!(km.lookup(&Key::Ctrl('k')), Some("palette"));
        assert!(km.lookup(&Key::Ctrl('p')).is_none());
    }

    #[test]
    fn keymap_override_none_disables() {
        let mut km = Keymap::defaults();
        let mut overrides = std::collections::HashMap::new();
        overrides.insert("help".into(), "none".into());
        km.apply_overrides(&overrides);
        assert!(km.lookup(&Key::Char('?')).is_none());
    }

    #[test]
    fn keymap_unmatched_returns_none() {
        let km = Keymap::defaults();
        assert!(km.lookup(&Key::Char('z')).is_none());
    }
}
