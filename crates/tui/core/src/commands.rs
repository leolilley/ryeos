//! Command palette — registry of commands for the command palette overlay.

use serde::{Deserialize, Serialize};

/// A command that can be dispatched from the command palette.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Command {
    pub id: String,
    pub label: String,
    pub category: String,
    pub description: String,
}

/// Built-in command registry.
pub fn builtin_commands() -> Vec<Command> {
    vec![
        Command {
            id: "view.threads".into(),
            label: "Threads".into(),
            category: "View".into(),
            description: "Switch to thread list".into(),
        },
        Command {
            id: "view.remotes".into(),
            label: "Remotes".into(),
            category: "View".into(),
            description: "Switch to remotes view".into(),
        },
        Command {
            id: "view.projects".into(),
            label: "Projects".into(),
            category: "View".into(),
            description: "Switch to projects view".into(),
        },
        Command {
            id: "view.items".into(),
            label: "Items".into(),
            category: "View".into(),
            description: "Browse items in space".into(),
        },
        Command {
            id: "view.trust".into(),
            label: "Trust".into(),
            category: "View".into(),
            description: "View trust status".into(),
        },
        Command {
            id: "view.graph".into(),
            label: "Graph".into(),
            category: "View".into(),
            description: "View graph topology".into(),
        },
        Command {
            id: "view.events".into(),
            label: "Events".into(),
            category: "View".into(),
            description: "Inspect raw events".into(),
        },
        Command {
            id: "layout.split-h".into(),
            label: "Split Horizontal".into(),
            category: "Layout".into(),
            description: "Split focused tile left/right".into(),
        },
        Command {
            id: "layout.split-v".into(),
            label: "Split Vertical".into(),
            category: "Layout".into(),
            description: "Split focused tile top/bottom".into(),
        },
        Command {
            id: "layout.close".into(),
            label: "Close Tile".into(),
            category: "Layout".into(),
            description: "Close the focused tile".into(),
        },
        Command {
            id: "layout.reset".into(),
            label: "Reset Layout".into(),
            category: "Layout".into(),
            description: "Reset to default 3-pane layout".into(),
        },
        Command {
            id: "session.new".into(),
            label: "New Session".into(),
            category: "Session".into(),
            description: "Clear input and start fresh".into(),
        },
        Command {
            id: "app.quit".into(),
            label: "Quit".into(),
            category: "App".into(),
            description: "Exit the TUI".into(),
        },
    ]
}

/// Filter commands by a query string (matches label, category, or description).
pub fn filter_commands<'a>(commands: &'a [Command], query: &str) -> Vec<&'a Command> {
    if query.is_empty() {
        return commands.iter().collect();
    }
    let query_lower = query.to_lowercase();
    commands
        .iter()
        .filter(|c| {
            c.label.to_lowercase().contains(&query_lower)
                || c.category.to_lowercase().contains(&query_lower)
                || c.description.to_lowercase().contains(&query_lower)
                || c.id.to_lowercase().contains(&query_lower)
        })
        .collect()
}
