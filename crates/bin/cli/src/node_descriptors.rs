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
    load_verified_snapshot_with_trust(app_root, &trust_store)
}

pub fn load_verified_snapshot_with_trust(
    app_root: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> anyhow::Result<NodeConfigSnapshot> {
    let loader = BootstrapLoader {
        app_root,
        trust_store,
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

    fn source_command(bundle: &str, file: &str) -> ryeos_runtime::CommandDef {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../bundles")
            .join(bundle)
            .join(".ai/node/commands")
            .join(file);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
        serde_yaml::from_str(&source)
            .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
    }

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

    #[test]
    fn subject_oriented_commands_bind_their_natural_positional_forms() {
        let cases = [
            (
                "core",
                "vault-set.yaml",
                vec!["API_KEY", "secret"],
                serde_json::json!({"name": "API_KEY", "value": "secret"}),
            ),
            (
                "core",
                "vault-delete.yaml",
                vec!["API_KEY"],
                serde_json::json!({"name": "API_KEY"}),
            ),
            (
                "core",
                "remote-bundle-install.yaml",
                vec!["prod", "standard"],
                serde_json::json!({"remote": "prod", "bundle_name": "standard"}),
            ),
            (
                "core",
                "remote-bundle-install.yaml",
                vec!["standard"],
                serde_json::json!({"bundle_name": "standard"}),
            ),
            (
                "standard",
                "thread-cancel.yaml",
                vec!["T-123"],
                serde_json::json!({"thread_id": "T-123"}),
            ),
            (
                "standard",
                "thread-get.yaml",
                vec!["T-123"],
                serde_json::json!({"thread_id": "T-123"}),
            ),
            (
                "standard",
                "thread-chain.yaml",
                vec!["T-123"],
                serde_json::json!({"thread_id": "T-123"}),
            ),
            (
                "standard",
                "thread-children.yaml",
                vec!["T-123"],
                serde_json::json!({"thread_id": "T-123"}),
            ),
            (
                "standard",
                "thread-receipts.yaml",
                vec!["T-123"],
                serde_json::json!({"thread_id": "T-123"}),
            ),
            (
                "standard",
                "events-replay.yaml",
                vec!["T-123"],
                serde_json::json!({"thread_id": "T-123"}),
            ),
            (
                "standard",
                "events-chain-replay.yaml",
                vec!["T-root"],
                serde_json::json!({"chain_root_id": "T-root"}),
            ),
            (
                "standard",
                "scheduler-pause.yaml",
                vec!["nightly"],
                serde_json::json!({"schedule_id": "nightly"}),
            ),
            (
                "standard",
                "scheduler-resume.yaml",
                vec!["nightly"],
                serde_json::json!({"schedule_id": "nightly"}),
            ),
            (
                "standard",
                "scheduler-explain.yaml",
                vec!["nightly"],
                serde_json::json!({"schedule_id": "nightly"}),
            ),
            (
                "standard",
                "scheduler-deregister.yaml",
                vec!["nightly"],
                serde_json::json!({"schedule_id": "nightly"}),
            ),
            (
                "standard",
                "scheduler-show-fires.yaml",
                vec!["nightly"],
                serde_json::json!({"schedule_id": "nightly"}),
            ),
            (
                "standard",
                "commands-submit.yaml",
                vec!["T-123", "cancel"],
                serde_json::json!({"thread_id": "T-123", "command_type": "cancel"}),
            ),
        ];

        for (bundle, file, argv, expected) in cases {
            let command = source_command(bundle, file);
            let argv = argv.into_iter().map(str::to_string).collect::<Vec<_>>();
            let actual = ryeos_runtime::arg_binder::bind_argv_with_command(&argv, Some(&command))
                .unwrap_or_else(|error| panic!("bind {bundle}/{file}: {error}"));
            assert_eq!(actual, expected, "unexpected binding for {bundle}/{file}");
        }
    }
}
