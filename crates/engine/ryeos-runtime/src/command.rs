//! Command-surface model and registry.
//!
//! Commands are the user-facing CLI surface: token spellings, argument binding,
//! project behavior, and dispatch intent. They replace the legacy
//! alias/verb command model. A command descriptor does not grant authority;
//! execution authorization remains based on the final item ref.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDef {
    pub category: String,
    pub section: String,
    pub name: String,
    pub tokens: Vec<String>,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<CommandAliasDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<CommandHelpDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<CommandArgumentDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forms: Vec<CommandArgumentForm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_binding: Option<CommandParameterBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<CommandProjectPolicy>,
    pub dispatch: CommandDispatch,
    #[serde(skip)]
    pub source_file: PathBuf,
    #[serde(skip)]
    pub source: CommandSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CommandSource {
    #[default]
    Installed,
    EmbeddedCore,
    SourceLocal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandAliasDef {
    pub tokens: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_tokens: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_in: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandHelpDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandArgumentDef {
    pub name: String,
    #[serde(default)]
    pub kind: CommandArgumentKind,
    pub positional: usize,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub arity: CommandArgumentArity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandArgumentForm {
    #[serde(default)]
    pub slots: Vec<CommandArgumentSlot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandArgumentSlot {
    pub field: String,
    #[serde(default)]
    pub matcher: CommandArgumentKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandArgumentKind {
    #[default]
    String,
    CanonicalRef,
    Path,
    Json,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandArgumentArity {
    #[default]
    One,
    Optional,
    Variadic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandParameterBinding {
    pub mode: CommandParameterBindingMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_flag: Option<String>,
    #[serde(default)]
    pub single_json_object_arg: bool,
    #[serde(default)]
    pub flag_key_normalization: FlagKeyNormalization,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandParameterBindingMode {
    #[default]
    None,
    TailObject,
    SchemaObject,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlagKeyNormalization {
    #[default]
    HyphenToUnderscore,
    Preserve,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandProjectPolicy {
    #[serde(default)]
    pub resolution: CommandProjectResolution,
    #[serde(default)]
    pub default: CommandProjectDefault,
    #[serde(default)]
    pub no_project_flag: bool,
    #[serde(default)]
    pub request_project_path: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_parameter: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandProjectResolution {
    #[default]
    None,
    Optional,
    Required,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandProjectDefault {
    #[default]
    None,
    DiscoverUpwardAi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandDispatch {
    Group,
    LocalHandler {
        handler: String,
        #[serde(default)]
        bootstrap: bool,
    },
    DirectExecuteItemRef {
        item_ref_arg: String,
        #[serde(default)]
        availability: CommandAvailability,
    },
    ExecuteRef {
        execute: String,
        #[serde(default)]
        availability: CommandAvailability,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandAvailability {
    #[default]
    Auto,
    Daemon,
    Offline,
    Both,
}

#[derive(Debug, thiserror::Error)]
pub enum CommandRegistryError {
    #[error("command '{name}' has invalid category/section: expected commands/commands, got {category}/{section}")]
    InvalidSection {
        name: String,
        category: String,
        section: String,
    },
    #[error("command '{name}' has empty tokens")]
    EmptyTokens { name: String },
    #[error("command '{name}' token '{token}' is invalid")]
    InvalidToken { name: String, token: String },
    #[error("command token collision for {tokens:?}: '{first}' and '{second}'")]
    DuplicateTokens {
        tokens: Vec<String>,
        first: String,
        second: String,
    },
    #[error("non-core command '{name}' cannot claim reserved root '{root}'")]
    ReservedRoot { name: String, root: String },
    #[error("command '{name}' dispatch kind '{kind}' is embedded-core only")]
    CoreOnlyDispatch { name: String, kind: &'static str },
    #[error("command '{name}' execute ref '{execute}' is not canonical: {detail}")]
    InvalidExecuteRef {
        name: String,
        execute: String,
        detail: String,
    },
    #[error("no command matches tokens {tokens:?}")]
    NoMatch { tokens: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct MatchedCommand {
    pub command: CommandDef,
    pub matched_tokens: Vec<String>,
    pub consumed: usize,
    pub tail: Vec<String>,
    pub alias: Option<CommandAliasDef>,
}

#[derive(Debug, Clone, Default)]
pub struct CommandRegistry {
    commands: Vec<CommandDef>,
    by_tokens: HashMap<Vec<String>, usize>,
}

const RESERVED_ROOTS: &[&str] = &[
    "help", "init", "start", "stop", "node", "system", "identity", "execute",
];

impl CommandRegistry {
    pub fn from_records(records: &[CommandDef]) -> Result<Self, CommandRegistryError> {
        let mut commands = Vec::new();
        let mut by_tokens = HashMap::new();

        for record in records {
            validate_command(record)?;
            let index = commands.len();
            insert_tokens(
                &mut by_tokens,
                &commands,
                index,
                &record.tokens,
                &record.name,
            )?;
            for alias in &record.aliases {
                validate_tokens(&record.name, &alias.tokens)?;
                insert_tokens(
                    &mut by_tokens,
                    &commands,
                    index,
                    &alias.tokens,
                    &record.name,
                )?;
            }
            commands.push(record.clone());
        }

        Ok(Self {
            commands,
            by_tokens,
        })
    }

    pub fn all_commands(&self) -> &[CommandDef] {
        &self.commands
    }

    pub fn resolve(&self, argv: &[String]) -> Result<MatchedCommand, CommandRegistryError> {
        for len in (1..=argv.len()).rev() {
            let prefix = argv[..len].to_vec();
            if let Some(index) = self.by_tokens.get(&prefix) {
                let command = self.commands[*index].clone();
                let alias = command
                    .aliases
                    .iter()
                    .find(|alias| alias.tokens == prefix)
                    .cloned();
                return Ok(MatchedCommand {
                    command,
                    matched_tokens: prefix,
                    consumed: len,
                    tail: argv[len..].to_vec(),
                    alias,
                });
            }
        }
        Err(CommandRegistryError::NoMatch {
            tokens: argv.to_vec(),
        })
    }
}

fn validate_command(record: &CommandDef) -> Result<(), CommandRegistryError> {
    if record.category != "commands" || record.section != "commands" {
        return Err(CommandRegistryError::InvalidSection {
            name: record.name.clone(),
            category: record.category.clone(),
            section: record.section.clone(),
        });
    }
    validate_tokens(&record.name, &record.tokens)?;
    validate_reserved_root(record, &record.tokens)?;
    match &record.dispatch {
        CommandDispatch::LocalHandler { .. } if record.source != CommandSource::EmbeddedCore => {
            return Err(CommandRegistryError::CoreOnlyDispatch {
                name: record.name.clone(),
                kind: "local_handler",
            });
        }
        CommandDispatch::DirectExecuteItemRef { .. }
            if record.source != CommandSource::EmbeddedCore =>
        {
            return Err(CommandRegistryError::CoreOnlyDispatch {
                name: record.name.clone(),
                kind: "direct_execute_item_ref",
            });
        }
        CommandDispatch::ExecuteRef { execute, .. } => {
            ryeos_engine::canonical_ref::CanonicalRef::parse(execute).map_err(|e| {
                CommandRegistryError::InvalidExecuteRef {
                    name: record.name.clone(),
                    execute: execute.clone(),
                    detail: e.to_string(),
                }
            })?;
        }
        _ => {}
    }
    for alias in &record.aliases {
        validate_tokens(&record.name, &alias.tokens)?;
        validate_reserved_root(record, &alias.tokens)?;
    }
    Ok(())
}

fn validate_tokens(name: &str, tokens: &[String]) -> Result<(), CommandRegistryError> {
    if tokens.is_empty() {
        return Err(CommandRegistryError::EmptyTokens { name: name.into() });
    }
    for token in tokens {
        if token.is_empty() || token.starts_with('-') {
            return Err(CommandRegistryError::InvalidToken {
                name: name.into(),
                token: token.clone(),
            });
        }
    }
    Ok(())
}

fn validate_reserved_root(
    record: &CommandDef,
    tokens: &[String],
) -> Result<(), CommandRegistryError> {
    let Some(root) = tokens.first() else {
        return Ok(());
    };
    if RESERVED_ROOTS.contains(&root.as_str()) && record.source != CommandSource::EmbeddedCore {
        return Err(CommandRegistryError::ReservedRoot {
            name: record.name.clone(),
            root: root.clone(),
        });
    }
    Ok(())
}

fn insert_tokens(
    by_tokens: &mut HashMap<Vec<String>, usize>,
    commands: &[CommandDef],
    index: usize,
    tokens: &[String],
    name: &str,
) -> Result<(), CommandRegistryError> {
    if let Some(prev_index) = by_tokens.get(tokens) {
        let first = commands
            .get(*prev_index)
            .map(|command| command.name.clone())
            .unwrap_or_else(|| name.to_string());
        return Err(CommandRegistryError::DuplicateTokens {
            tokens: tokens.to_vec(),
            first,
            second: name.to_string(),
        });
    }
    by_tokens.insert(tokens.to_vec(), index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str, tokens: &[&str]) -> CommandDef {
        CommandDef {
            category: "commands".into(),
            section: "commands".into(),
            name: name.into(),
            tokens: tokens.iter().map(|token| token.to_string()).collect(),
            description: name.into(),
            aliases: Vec::new(),
            help: None,
            arguments: Vec::new(),
            forms: Vec::new(),
            parameter_binding: None,
            project: None,
            dispatch: CommandDispatch::ExecuteRef {
                execute: format!("tool:test/{name}"),
                availability: CommandAvailability::Auto,
            },
            source_file: PathBuf::new(),
            source: CommandSource::Installed,
        }
    }

    #[test]
    fn duplicate_alias_matching_primary_errors_without_panicking() {
        let mut record = command("demo", &["demo"]);
        record.aliases.push(CommandAliasDef {
            tokens: vec!["demo".into()],
            description: None,
            deprecated: None,
            replacement_tokens: None,
            removed_in: None,
        });

        let err = CommandRegistry::from_records(&[record]).unwrap_err();
        assert!(matches!(err, CommandRegistryError::DuplicateTokens { .. }));
    }

    #[test]
    fn installed_command_cannot_claim_reserved_root() {
        let err =
            CommandRegistry::from_records(&[command("fake-execute", &["execute"])]).unwrap_err();
        assert!(matches!(err, CommandRegistryError::ReservedRoot { .. }));
    }

    #[test]
    fn installed_command_cannot_use_core_only_dispatch() {
        let mut record = command("demo", &["demo"]);
        record.dispatch = CommandDispatch::DirectExecuteItemRef {
            item_ref_arg: "item_ref".into(),
            availability: CommandAvailability::Both,
        };

        let err = CommandRegistry::from_records(&[record]).unwrap_err();
        assert!(matches!(err, CommandRegistryError::CoreOnlyDispatch { .. }));
    }
}
