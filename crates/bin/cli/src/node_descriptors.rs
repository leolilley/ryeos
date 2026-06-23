use std::path::Path;

use anyhow::Context;
use ryeos_app::node_config::loader::BootstrapLoader;
use ryeos_app::node_config::{NodeConfigSnapshot, SectionTable};
use ryeos_runtime::{CommandDef, CommandDispatch};

#[derive(Debug, Clone)]
pub struct LoadedCommandDescriptor {
    pub command: CommandDef,
    pub tokens: Vec<String>,
    pub description: String,
}

impl LoadedCommandDescriptor {
    pub fn execute_ref(&self) -> Option<&str> {
        match &self.command.dispatch {
            CommandDispatch::ExecuteRef { execute, .. } => Some(execute.as_str()),
            _ => None,
        }
    }
}

pub fn load_verified_snapshot(app_root: &Path) -> anyhow::Result<NodeConfigSnapshot> {
    let trust_store = ryeos_engine::trust::TrustStore::load(
        None,
        &ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).config(),
    )
    .context("load trust store for verified node config")?;
    let loader = BootstrapLoader {
        app_root,
        trust_store: &trust_store,
    };
    let bundles = loader
        .load_bundle_section()
        .context("load verified node bundle registrations")?;
    loader
        .load_full(&SectionTable::new(), &bundles)
        .context("load verified node config")
}

pub fn load_command_descriptors_from_snapshot(
    snapshot: &NodeConfigSnapshot,
) -> Vec<LoadedCommandDescriptor> {
    let mut out = Vec::new();

    for command in &snapshot.commands {
        out.push(LoadedCommandDescriptor {
            command: command.clone(),
            tokens: command.tokens.clone(),
            description: command.description.clone(),
        });
        for alias in &command.aliases {
            out.push(LoadedCommandDescriptor {
                command: command.clone(),
                tokens: alias.tokens.clone(),
                description: alias
                    .description
                    .clone()
                    .unwrap_or_else(|| command.description.clone()),
            });
        }
    }

    out
}

pub fn find_command(
    snapshot: &NodeConfigSnapshot,
    command_tokens: &[String],
) -> Option<LoadedCommandDescriptor> {
    load_command_descriptors_from_snapshot(snapshot)
        .into_iter()
        .find(|command| command.tokens == command_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn exposes_snapshot_commands_without_legacy_alias_conversion() {
        let snapshot = NodeConfigSnapshot {
            bundles: vec![],
            routes: vec![],
            commands: vec![ryeos_runtime::CommandDef {
                name: "bundle-sign".into(),
                tokens: vec!["bundle".into(), "sign".into()],
                description: "Sign bundle".into(),
                aliases: vec![],
                help: None,
                arguments: vec![],
                forms: vec![ryeos_runtime::CommandArgumentForm {
                    slots: vec![ryeos_runtime::CommandArgumentSlot {
                        field: "source".into(),
                        matcher: ryeos_runtime::CommandArgumentKind::String,
                    }],
                }],
                defaults: Default::default(),
                parameter_binding: None,
                control_flags: Vec::new(),
                project: Some(ryeos_runtime::CommandProjectPolicy {
                    resolution: ryeos_runtime::CommandProjectResolution::Optional,
                    default: ryeos_runtime::CommandProjectDefault::None,
                    no_project_flag: false,
                    request_project_path: false,
                    bind_parameter: None,
                }),
                dispatch: ryeos_runtime::CommandDispatch::ExecuteRef {
                    execute: "tool:bundle/sign".into(),
                    availability: ryeos_runtime::CommandAvailability::Auto,
                },
                source_file: PathBuf::from("/tmp/command.yaml"),
                provenance: ryeos_runtime::CommandProvenance::default(),
            }],
            hosted_node_policies: vec![],
            command_registration_policy: Default::default(),
        };

        let commands = load_command_descriptors_from_snapshot(&snapshot);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].tokens, ["bundle", "sign"]);
        assert_eq!(commands[0].command.name, "bundle-sign");
        assert_eq!(commands[0].command.forms[0].slots[0].field, "source");
        assert_eq!(
            commands[0].command.project.as_ref().unwrap().resolution,
            ryeos_runtime::CommandProjectResolution::Optional
        );
    }
}
