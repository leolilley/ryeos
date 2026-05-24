//! CommandRegistry — renderer-agnostic command dispatch model.
//!
//! Merges built-in affordances with surface-declared affordances,
//! filters by granted capabilities, and provides palette/key/ID
//! resolution. Both terminal and web renderers use the same registry.

use crate::commands::InvocationSpec;
use crate::surface::SurfaceCommandSpec;

/// A resolved, dispatchable command.
#[derive(Debug, Clone)]
pub struct ResolvedCommand {
    pub id: String,
    pub label: String,
    pub category: String,
    pub description: String,
    pub invoke: InvocationSpec,
}

/// Footer hint for keybinding display.
#[derive(Debug, Clone)]
pub struct FooterHint {
    pub command_id: String,
    pub key: String,
    pub label: String,
}

/// Granted capability set for filtering affordances.
#[derive(Debug, Clone, Default)]
pub struct CapabilitySet {
    pub granted: Vec<String>,
}

impl CapabilitySet {
    pub fn has(&self, cap: &str) -> bool {
        self.granted.iter().any(|c| c == cap)
    }
}

/// Contextual predicates the registry can inspect.
#[derive(Debug, Clone, Default)]
pub struct CommandContext {
    pub has_focused_thread: bool,
    pub has_selection: bool,
    pub is_read_only: bool,
}

/// The renderer-agnostic command registry.
pub struct CommandRegistry {
    affordances: Vec<ResolvedCommand>,
    /// Keybindings: maps key chord strings to command IDs.
    /// For now, built-in defaults.
    bindings: Vec<(String, String)>,
}

impl CommandRegistry {
    /// Build the registry by merging built-in affordances with
    /// surface-declared affordances, filtered by capabilities.
    pub fn build(
        _builtin: &[crate::commands::Affordance],
        surface_commands: &[SurfaceCommandSpec],
        granted_caps: &CapabilitySet,
    ) -> Self {
        let (merged, _warnings) = crate::commands::merge_affordances(surface_commands);

        let mut resolved: Vec<ResolvedCommand> = Vec::new();

        for aff in &merged {
            // Check capabilities.
            let caps_met = aff.requires_capabilities.iter().all(|cap| granted_caps.has(cap));
            if !caps_met {
                continue;
            }

            resolved.push(ResolvedCommand {
                id: aff.id.clone(),
                label: aff.label.clone(),
                category: aff.category.clone(),
                description: aff.description.clone(),
                invoke: aff.invoke.clone(),
            });
        }

        // Built-in keybindings.
        let bindings = vec![
            ("q".into(), "app.quit".into()),
            ("?".into(), "help".into()),
            (":".into(), "palette".into()),
            ("Tab".into(), "focus.next".into()),
            ("Backtab".into(), "focus.prev".into()),
        ];

        Self {
            affordances: resolved,
            bindings,
        }
    }

    /// All commands for the command palette, filtered by context.
    pub fn for_palette(&self, _ctx: &CommandContext) -> Vec<&ResolvedCommand> {
        self.affordances.iter().collect()
    }

    /// Resolve a key binding to an invocation spec.
    pub fn resolve_binding(&self, key: &str, ctx: &CommandContext) -> Option<InvocationSpec> {
        let id = self.bindings.iter().find(|(k, _)| k == key)?;
        self.resolve_by_id(&id.1, ctx)
    }

    /// Resolve a command by its ID.
    pub fn resolve_by_id(&self, id: &str, _ctx: &CommandContext) -> Option<InvocationSpec> {
        self.affordances
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.invoke.clone())
    }

    /// Footer hints for keybinding display.
    pub fn footer_hints(&self, _ctx: &CommandContext) -> Vec<FooterHint> {
        self.bindings
            .iter()
            .filter_map(|(key, id)| {
                let cmd = self.affordances.iter().find(|c| c.id == *id)?;
                Some(FooterHint {
                    command_id: id.clone(),
                    key: key.clone(),
                    label: cmd.label.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{InvocationSpec, UiInvocation, UiVerb, builtin_affordances};

    #[test]
    fn merges_builtin_and_surface_affordances() {
        let surface = vec![SurfaceCommandSpec {
            id: "custom.action".into(),
            label: "Custom".into(),
            category: "Test".into(),
            description: "A custom action".into(),
            invoke: Some(InvocationSpec::Ui(UiInvocation {
                verb: UiVerb::ToggleHelp,
                args: serde_json::Value::Null,
            })),
            requires_capabilities: vec![],
        }];
        let caps = CapabilitySet::default();
        let registry = CommandRegistry::build(&builtin_affordances(), &surface, &caps);
        let palette = registry.for_palette(&CommandContext::default());
        assert!(palette.iter().any(|c| c.id == "custom.action"));
        assert!(palette.iter().any(|c| c.id == "app.quit"));
    }

    #[test]
    fn denied_affordance_not_resolved() {
        let surface = vec![SurfaceCommandSpec {
            id: "admin.nuke".into(),
            label: "Nuke".into(),
            category: "Admin".into(),
            description: "Nuke everything".into(),
            invoke: Some(InvocationSpec::Ui(UiInvocation {
                verb: UiVerb::Quit,
                args: serde_json::Value::Null,
            })),
            requires_capabilities: vec!["admin.nuke".into()],
        }];
        // Empty caps — nuke should be filtered out.
        let caps = CapabilitySet::default();
        let registry = CommandRegistry::build(&builtin_affordances(), &surface, &caps);
        let result = registry.resolve_by_id("admin.nuke", &CommandContext::default());
        assert!(result.is_none());
    }

    #[test]
    fn key_dispatch_returns_invocation_spec() {
        let caps = CapabilitySet::default();
        let registry = CommandRegistry::build(&builtin_affordances(), &[], &caps);
        let result = registry.resolve_binding("q", &CommandContext::default());
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), InvocationSpec::Ui(ui) if ui.verb == UiVerb::Quit));
    }

    #[test]
    fn predicate_disables_affordance() {
        // For now, predicates are not wired. This test verifies
        // the seam exists and context is passed through.
        let caps = CapabilitySet::default();
        let registry = CommandRegistry::build(&builtin_affordances(), &[], &caps);
        let ctx = CommandContext {
            is_read_only: true,
            ..Default::default()
        };
        // Read-only doesn't filter anything yet — verify the path.
        let palette = registry.for_palette(&ctx);
        assert!(!palette.is_empty());
    }
}
