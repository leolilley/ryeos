//! Command-surface model and registry.
//!
//! Commands are the user-facing CLI surface: token spellings, argument binding,
//! project behavior, and dispatch intent. They replace the legacy
//! alias/verb command model. A command descriptor does not grant authority;
//! execution authorization remains based on the final item ref.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDef {
    #[serde(skip)]
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub defaults: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_binding: Option<CommandParameterBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<CommandProjectPolicy>,
    pub dispatch: CommandDispatch,
    #[serde(skip)]
    pub source_file: PathBuf,
    #[serde(skip)]
    pub provenance: CommandProvenance,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandProvenance {
    pub origin: CommandOrigin,
    pub command_registration_caps: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CommandOrigin {
    #[default]
    InstalledBundle,
    SystemSpace,
    SourceLocal,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandRegistrationPolicy {
    #[serde(default)]
    pub claim_rules: Vec<CommandRegistrationRule>,
    #[serde(default)]
    pub system_source_caps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandRegistrationRule {
    pub claim: CommandRegistrationClaimPattern,
    #[serde(default)]
    pub required_caps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CommandRegistrationClaimPattern {
    pub kind: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRegistrationClaim {
    pub kind: String,
    pub value: String,
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
    #[error("command '{name}' claim {claim_kind}={claim_value} requires missing registration capability '{required_cap}' (source: {source_file})")]
    MissingRegistrationCap {
        name: String,
        claim_kind: String,
        claim_value: String,
        required_cap: String,
        source_file: PathBuf,
    },
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

impl CommandRegistry {
    pub fn from_records(
        records: &[CommandDef],
        policy: &CommandRegistrationPolicy,
    ) -> Result<Self, CommandRegistryError> {
        let mut commands = Vec::new();
        let mut by_tokens = HashMap::new();

        for record in records {
            validate_command(record, policy)?;
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

fn validate_command(
    record: &CommandDef,
    policy: &CommandRegistrationPolicy,
) -> Result<(), CommandRegistryError> {
    validate_tokens(&record.name, &record.tokens)?;
    match &record.dispatch {
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
    }
    validate_registration_caps(record, policy)?;
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

fn validate_registration_caps(
    record: &CommandDef,
    policy: &CommandRegistrationPolicy,
) -> Result<(), CommandRegistryError> {
    for claim in derive_registration_claims(record) {
        for required_cap in required_caps_for_claim(policy, &claim) {
            let granted = record
                .provenance
                .command_registration_caps
                .iter()
                .any(|grant| crate::authorizer::cap_matches(grant, required_cap));
            if !granted {
                return Err(CommandRegistryError::MissingRegistrationCap {
                    name: record.name.clone(),
                    claim_kind: claim.kind,
                    claim_value: claim.value,
                    required_cap: required_cap.clone(),
                    source_file: record.source_file.clone(),
                });
            }
        }
    }
    Ok(())
}

pub fn derive_registration_claims(record: &CommandDef) -> Vec<CommandRegistrationClaim> {
    let mut claims = Vec::new();
    if let Some(root) = record.tokens.first() {
        claims.push(CommandRegistrationClaim {
            kind: "command.root".to_string(),
            value: root.clone(),
        });
    }
    for alias in &record.aliases {
        if let Some(root) = alias.tokens.first() {
            claims.push(CommandRegistrationClaim {
                kind: "command.root".to_string(),
                value: root.clone(),
            });
        }
    }
    claims.push(CommandRegistrationClaim {
        kind: "command.dispatch.kind".to_string(),
        value: dispatch_kind_name(&record.dispatch).to_string(),
    });
    claims
}

fn required_caps_for_claim<'a>(
    policy: &'a CommandRegistrationPolicy,
    claim: &CommandRegistrationClaim,
) -> Vec<&'a String> {
    policy
        .claim_rules
        .iter()
        .filter(|rule| claim_matches(&rule.claim, claim))
        .flat_map(|rule| rule.required_caps.iter())
        .collect()
}

fn claim_matches(
    pattern: &CommandRegistrationClaimPattern,
    claim: &CommandRegistrationClaim,
) -> bool {
    pattern.kind == claim.kind && pattern.value == claim.value
}

fn dispatch_kind_name(dispatch: &CommandDispatch) -> &'static str {
    match dispatch {
        CommandDispatch::Group => "group",
        CommandDispatch::LocalHandler { .. } => "local_handler",
        CommandDispatch::DirectExecuteItemRef { .. } => "direct_execute_item_ref",
        CommandDispatch::ExecuteRef { .. } => "execute_ref",
    }
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
            name: name.into(),
            tokens: tokens.iter().map(|token| token.to_string()).collect(),
            description: name.into(),
            aliases: Vec::new(),
            help: None,
            arguments: Vec::new(),
            forms: Vec::new(),
            defaults: Default::default(),
            parameter_binding: None,
            project: None,
            dispatch: CommandDispatch::ExecuteRef {
                execute: format!("tool:test/{name}"),
                availability: CommandAvailability::Auto,
            },
            source_file: PathBuf::new(),
            provenance: CommandProvenance::default(),
        }
    }

    fn policy() -> CommandRegistrationPolicy {
        CommandRegistrationPolicy {
            claim_rules: vec![
                CommandRegistrationRule {
                    claim: CommandRegistrationClaimPattern {
                        kind: "command.root".into(),
                        value: "execute".into(),
                    },
                    required_caps: vec!["ryeos.register.command.root.execute".into()],
                },
                CommandRegistrationRule {
                    claim: CommandRegistrationClaimPattern {
                        kind: "command.dispatch.kind".into(),
                        value: "direct_execute_item_ref".into(),
                    },
                    required_caps: vec![
                        "ryeos.register.command.dispatch.direct_execute_item_ref".into()
                    ],
                },
            ],
            system_source_caps: vec![],
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

        let err = CommandRegistry::from_records(&[record], &policy()).unwrap_err();
        assert!(matches!(err, CommandRegistryError::DuplicateTokens { .. }));
    }

    #[test]
    fn command_claiming_protected_root_requires_registration_cap() {
        let err =
            CommandRegistry::from_records(&[command("fake-execute", &["execute"])], &policy())
                .unwrap_err();
        assert!(matches!(
            err,
            CommandRegistryError::MissingRegistrationCap { .. }
        ));
    }

    #[test]
    fn protected_root_passes_with_matching_registration_cap() {
        let mut record = command("fake-execute", &["execute"]);
        record
            .provenance
            .command_registration_caps
            .push("ryeos.register.command.root.execute".into());

        CommandRegistry::from_records(&[record], &policy()).unwrap();
    }

    #[test]
    fn direct_execute_item_ref_requires_registration_cap() {
        let mut record = command("demo", &["demo"]);
        record.dispatch = CommandDispatch::DirectExecuteItemRef {
            item_ref_arg: "item_ref".into(),
            availability: CommandAvailability::Both,
        };

        let err = CommandRegistry::from_records(&[record], &policy()).unwrap_err();
        assert!(matches!(
            err,
            CommandRegistryError::MissingRegistrationCap { .. }
        ));
    }

    #[test]
    fn wildcard_registration_grant_satisfies_required_cap() {
        let mut record = command("demo", &["demo"]);
        record.dispatch = CommandDispatch::DirectExecuteItemRef {
            item_ref_arg: "item_ref".into(),
            availability: CommandAvailability::Both,
        };
        record
            .provenance
            .command_registration_caps
            .push("ryeos.register.command.*".into());

        CommandRegistry::from_records(&[record], &policy()).unwrap();
    }

    #[test]
    fn derive_registration_claims_includes_primary_alias_and_dispatch() {
        let mut record = command("demo", &["demo"]);
        record.aliases.push(CommandAliasDef {
            tokens: vec!["alias".into()],
            description: None,
            deprecated: None,
            replacement_tokens: None,
            removed_in: None,
        });
        record.dispatch = CommandDispatch::DirectExecuteItemRef {
            item_ref_arg: "item_ref".into(),
            availability: CommandAvailability::Both,
        };

        let claims = derive_registration_claims(&record);
        assert_eq!(
            claims,
            vec![
                CommandRegistrationClaim {
                    kind: "command.root".into(),
                    value: "demo".into(),
                },
                CommandRegistrationClaim {
                    kind: "command.root".into(),
                    value: "alias".into(),
                },
                CommandRegistrationClaim {
                    kind: "command.dispatch.kind".into(),
                    value: "direct_execute_item_ref".into(),
                },
            ]
        );
    }
}
