//! CLI help — static lifecycle section + dynamic command discovery.
//!
//! `ryeos help` prints lifecycle commands (always available) and discovers
//! the rest from installed bundle descriptors on disk. No daemon required.
//! `ryeos help <command>` uses installed descriptors first and only queries the
//! daemon when no local descriptor help is available.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::CliError;
use anyhow::Context;
use ryeos_app::node_config::NodeConfigSnapshot;
use ryeos_engine::canonical_ref::CanonicalRef;
use ryeos_engine::engine::{EffectiveItemRequest, Engine};
use serde_json::Value;

use crate::node_descriptors::LoadedCommandDescriptor;

/// Print top-level help. Static lifecycle help always renders. Dynamic command
/// discovery is included only after installed node config verifies; verification
/// failures are surfaced as warnings instead of being treated as absent config.
///
/// The snapshot is loaded once per invocation in `dispatcher::run` and
/// threaded through so help never re-reads node config from disk.
pub fn print_help(
    mut out: impl Write,
    app_root: &Path,
    snapshot: &anyhow::Result<NodeConfigSnapshot>,
) -> std::io::Result<()> {
    writeln!(out, "ryeos — CLI for Rye OS")?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "  ryeos [-p PROJECT] [--debug] <command...> [args...]")?;
    writeln!(out)?;
    writeln!(out, "LIFECYCLE:")?;
    writeln!(
        out,
        "  {:<30} {}",
        "init", "Bootstrap local node state and packaged bundles"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "start", "Bring the local node runtime online"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "stop", "Gracefully stop the local node runtime"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "node status", "Show local node lifecycle status"
    )?;
    writeln!(out)?;
    writeln!(out, "UNIVERSAL ESCAPE HATCH:")?;
    writeln!(
        out,
        "  {:<30} {}",
        "execute [--async] <item_ref>", "Execute any canonical item ref directly"
    )?;
    writeln!(
        out,
        "  {:<30} {}",
        "  --input <file>", "  pass JSON parameters from file (or - for stdin)"
    )?;
    writeln!(out)?;

    // ── Dynamic command discovery from installed bundles ──
    let snapshot = match snapshot {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            eprintln!("warning: installed node config failed verification: {err:#}");
            None
        }
    };
    let bundle_roots = snapshot.map(snapshot_bundle_roots).unwrap_or_default();
    let engine = (!bundle_roots.is_empty())
        .then(|| build_help_engine(app_root, ".", &bundle_roots).ok())
        .flatten();
    let discovered = snapshot
        .map(|snapshot| discover_commands_from_snapshot(snapshot, engine.as_ref(), "."))
        .unwrap_or_default();

    if !discovered.is_empty() {
        let mut offline_cmds: Vec<(&str, &str)> = Vec::new();
        let mut daemon_cmds: Vec<(&str, &str)> = Vec::new();

        for (tokens_str, description, is_offline) in &discovered {
            if *is_offline {
                offline_cmds.push((tokens_str, description));
            } else {
                daemon_cmds.push((tokens_str, description));
            }
        }

        if !offline_cmds.is_empty() {
            writeln!(out, "OFFLINE (no daemon required):")?;
            offline_cmds.sort_by_key(|c| c.0);
            for (tokens_str, description) in &offline_cmds {
                writeln!(out, "    {:<28} {}", tokens_str, description)?;
            }
            writeln!(out)?;
        }

        if !daemon_cmds.is_empty() {
            writeln!(out, "DAEMON (requires running daemon):")?;
            daemon_cmds.sort_by_key(|c| c.0);
            for (tokens_str, description) in &daemon_cmds {
                writeln!(out, "    {:<28} {}", tokens_str, description)?;
            }
            writeln!(out)?;
        }
    }

    writeln!(out, "Run `ryeos help <command>` for command-specific help.")?;
    Ok(())
}

/// Discover commands from verified installed node config.
/// Returns (token_string, description, is_offline) tuples.
fn discover_commands_from_snapshot(
    snapshot: &NodeConfigSnapshot,
    engine: Option<&Engine>,
    project_path: &str,
) -> Vec<(String, String, bool)> {
    let mut results = Vec::new();
    let commands = crate::node_descriptors::load_command_descriptors_from_snapshot(snapshot);

    for command in commands {
        // Skip short aliases (s, f) — they're abbreviations.
        if command.tokens.len() == 1 && command.tokens[0].len() <= 1 {
            continue;
        }

        let metadata = command.execute_ref().and_then(|execute| {
            engine.and_then(|engine| resolve_effective_help(engine, execute, project_path))
        });
        let is_offline = metadata
            .as_ref()
            .is_some_and(ItemHelpMetadata::is_offline_dispatch);
        results.push((command.tokens.join(" "), command.description, is_offline));
    }

    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

/// Print command-specific help from installed descriptors or local bootstrap help.
pub async fn print_command_help(
    command_tokens: &[String],
    app_root: &std::path::Path,
    project_path: &str,
    snapshot: &anyhow::Result<NodeConfigSnapshot>,
) -> Result<(), CliError> {
    // Prefer installed descriptor help. This keeps help kind-agnostic in the
    // CLI: command descriptors are node config, and the help renderer does not
    // need to classify the execute ref before deciding whether it can print usage.
    if print_installed_command_help(command_tokens, app_root, project_path, snapshot)? {
        return Ok(());
    }

    print_lifecycle_command_help(command_tokens)?;

    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct ItemHelpMetadata {
    #[serde(default)]
    description: String,
    #[serde(default)]
    required_caps: Vec<String>,
    #[serde(default)]
    schema: BTreeMap<String, String>,
    #[serde(default)]
    availability: Option<String>,
    #[serde(default)]
    has_launch_binary_ref: bool,
    #[serde(default)]
    has_tool_command: bool,
    #[serde(default)]
    has_offline_execute: bool,
}

impl ItemHelpMetadata {
    fn from_composed(value: &Value) -> Self {
        Self {
            description: value
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            required_caps: value
                .get("required_caps")
                .and_then(Value::as_array)
                .map(|caps| {
                    caps.iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            schema: value
                .get("schema")
                .and_then(Value::as_object)
                .map(|schema| {
                    schema
                        .iter()
                        .map(|(field, ty)| {
                            (
                                field.clone(),
                                ty.as_str()
                                    .map(ToString::to_string)
                                    .unwrap_or_else(|| ty.to_string()),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
            availability: value
                .get("availability")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            has_launch_binary_ref: value
                .get("launch")
                .and_then(|launch| launch.get("binary_ref"))
                .and_then(Value::as_str)
                .is_some(),
            has_tool_command: value
                .get("config")
                .and_then(|config| config.get("command"))
                .and_then(Value::as_str)
                .is_some(),
            has_offline_execute: value
                .get("offline_execute")
                .and_then(Value::as_str)
                .is_some(),
        }
    }

    fn is_offline_dispatch(&self) -> bool {
        matches!(self.availability.as_deref(), Some("offline" | "both"))
            || self.has_launch_binary_ref
            || self.has_tool_command
            || self.has_offline_execute
    }
}

fn print_installed_command_help(
    command_tokens: &[String],
    app_root: &std::path::Path,
    project_path: &str,
    snapshot: &anyhow::Result<NodeConfigSnapshot>,
) -> std::io::Result<bool> {
    let snapshot = snapshot.as_ref().map_err(|err| {
        std::io::Error::other(format!(
            "installed node config failed verification: {err:#}"
        ))
    })?;
    let bundle_roots = snapshot_bundle_roots(&snapshot);
    let Some(command_descriptor) = crate::node_descriptors::find_command(&snapshot, command_tokens)
    else {
        return Ok(false);
    };
    let execute_ref = command_descriptor.execute_ref();
    let engine = build_help_engine(app_root, project_path, &bundle_roots).ok();
    let item = execute_ref.and_then(|execute| {
        engine
            .as_ref()
            .and_then(|engine| resolve_effective_help(engine, execute, project_path))
    });

    let mut out = std::io::stdout();
    let command = command_descriptor.tokens.join(" ");
    let description = item
        .as_ref()
        .map(|s| s.description.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&command_descriptor.description);
    writeln!(out, "ryeos {command} — {description}")?;
    writeln!(out)?;
    if let Some(item) = &item {
        if item.is_offline_dispatch() {
            writeln!(out, "DISPATCH: offline (no daemon required)")?;
            writeln!(out)?;
        }
    } else if let Some(execute_ref) = execute_ref {
        writeln!(out, "EXECUTE: {execute_ref}")?;
        writeln!(out)?;
    }
    writeln!(out, "USAGE:")?;
    writeln!(
        out,
        "  {}",
        installed_usage_line(&command_descriptor, item.as_ref())
    )?;

    // Control flags, rendered from the command's declared `control_flags`
    // (data-driven — no hardcoded flag list). Value flags (method/args) show a
    // `<value>` hint; aliases are listed alongside the primary spelling.
    if !command_descriptor.command.control_flags.is_empty() {
        writeln!(out)?;
        writeln!(out, "CONTROL FLAGS:")?;
        for cf in &command_descriptor.command.control_flags {
            let mut names = vec![format!("--{}", cf.flag)];
            names.extend(cf.aliases.iter().map(|alias| format!("--{alias}")));
            let value_hint = if cf.binding.takes_value() {
                " <value>"
            } else {
                ""
            };
            writeln!(
                out,
                "  {:<24} {}",
                format!("{}{}", names.join(", "), value_hint),
                cf.help
            )?;
        }
    }

    // Parameter passing, from the command's declared `parameter_binding`.
    if let Some(binding) = &command_descriptor.command.parameter_binding {
        if !matches!(
            binding.mode,
            ryeos_runtime::CommandParameterBindingMode::None
        ) {
            writeln!(out)?;
            writeln!(out, "PARAMETERS:")?;
            if let Some(input_flag) = &binding.input_flag {
                writeln!(
                    out,
                    "  {:<24} Read JSON parameters from a file (or - for stdin)",
                    format!("--{input_flag} <file>")
                )?;
            }
            writeln!(
                out,
                "  {:<24} Set parameter <key> (repeatable)",
                "--<key> <value>"
            )?;
            if binding.single_json_object_arg {
                writeln!(
                    out,
                    "  {:<24} A single JSON object of parameters",
                    "'<json>'"
                )?;
            }
        }
    }

    let project_resolution = command_descriptor
        .command
        .project
        .as_ref()
        .map(|project| project.resolution)
        .unwrap_or_default();
    if project_resolution != ryeos_runtime::CommandProjectResolution::None {
        writeln!(out)?;
        writeln!(out, "PROJECT:")?;
        writeln!(
            out,
            "  --project <DIR>       Project root; accepted before or after the command"
        )?;
        if project_resolution == ryeos_runtime::CommandProjectResolution::Optional {
            writeln!(out, "  --no-project          Resolve against bundles only")?;
        }
    }

    if let Some(item) = &item {
        if !item.schema.is_empty() {
            writeln!(out)?;
            writeln!(out, "FIELDS:")?;
            for (field, ty) in &item.schema {
                let flag = field.replace('_', "-");
                writeln!(out, "  --{:<20} {}", flag, ty)?;
            }
        }
        if !item.required_caps.is_empty() {
            writeln!(out)?;
            writeln!(out, "REQUIRED CAPABILITIES:")?;
            for cap in &item.required_caps {
                writeln!(out, "  {cap}")?;
            }
        }
    }

    Ok(true)
}

fn usage_tail(command: &LoadedCommandDescriptor, item: Option<&ItemHelpMetadata>) -> String {
    let mut parts = Vec::new();
    if !command.command.forms.is_empty() {
        for form in &command.command.forms {
            let shape = form
                .slots
                .iter()
                .map(|slot| {
                    let field = slot.field.replace('_', "-");
                    let required = !command.command.defaults.contains_key(&slot.field)
                        && !command.command.defaults.contains_key(&field);
                    if required {
                        format!("<{field}>")
                    } else {
                        format!("[<{field}>]")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            if !shape.is_empty() {
                parts.push(shape);
            }
        }
    }

    if let Some(item) = item {
        for (field, ty) in &item.schema {
            let required = !ty.ends_with('?');
            if field == "project" || parts.iter().any(|p| p.contains(&field.replace('_', "-"))) {
                continue;
            }
            let flag = format!(
                "--{} <{}>",
                field.replace('_', "-"),
                field.replace('_', "-")
            );
            if required {
                parts.push(flag);
            } else {
                parts.push(format!("[{flag}]"));
            }
        }
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", parts.join(" "))
    }
}

fn installed_usage_line(
    command: &LoadedCommandDescriptor,
    item: Option<&ItemHelpMetadata>,
) -> String {
    let command_tokens = command.tokens.join(" ");
    let control_tail = if matches!(
        command.command.dispatch,
        ryeos_runtime::CommandDispatch::DirectExecuteItemRef { .. }
    ) {
        " [--async]"
    } else {
        ""
    };
    format!(
        "ryeos {command_tokens}{control_tail}{}",
        usage_tail(command, item)
    )
}

fn snapshot_bundle_roots(snapshot: &NodeConfigSnapshot) -> Vec<PathBuf> {
    snapshot
        .bundles
        .iter()
        .map(|record| record.path.clone())
        .collect()
}

fn build_help_engine(
    app_root: &Path,
    project_path: &str,
    bundle_roots: &[PathBuf],
) -> anyhow::Result<Engine> {
    let config = ryeos_app::config::Config::load(&ryeos_app::config::ConfigSources {
        app_root: Some(app_root.to_path_buf()),
        ..Default::default()
    })?;
    let project_root = (project_path != ".").then(|| PathBuf::from(project_path));

    ryeos_app::engine_init::build_engine_for_roots(
        &config,
        bundle_roots,
        project_root.as_deref(),
        None,
    )
    .context("build help effective-item engine")
}

fn resolve_effective_help(
    engine: &Engine,
    execute_ref: &str,
    project_path: &str,
) -> Option<ItemHelpMetadata> {
    let item_ref = CanonicalRef::parse(execute_ref).ok()?;
    let project_root = (project_path != ".").then(|| PathBuf::from(project_path));
    let item = engine
        .effective_item(EffectiveItemRequest {
            item_ref,
            expected_kind: None,
            project_root,
        })
        .ok()?;
    Some(ItemHelpMetadata::from_composed(&item.composed_value))
}

/// Print help for lifecycle commands when installed descriptors are unavailable.
fn print_lifecycle_command_help(command_tokens: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout();
    match command_tokens.first().map(|s| s.as_str()) {
        Some("init") => {
            writeln!(out, "ryeos init — Bootstrap operator keys and core bundle")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos init [OPTIONS]")?;
            writeln!(out)?;
            writeln!(out, "OPTIONS:")?;
            writeln!(
                out,
                "  --source <DIR>           Bundle source directory (default: /usr/share/ryeos)"
            )?;
            writeln!(
                out,
                "  --trust-file <FILE>      Additional publisher trust doc (repeatable)"
            )?;
            writeln!(out, "  --app-root <DIR>        App root")?;
        }
        Some("node") if command_tokens.get(1).map(String::as_str) == Some("status") => {
            writeln!(out, "ryeos node status — Show local node lifecycle status")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos node status [--json] [--app-root <DIR>]")?;
        }
        Some("system") if command_tokens.get(1).map(String::as_str) == Some("status") => {
            writeln!(
                out,
                "ryeos system status — Show local node lifecycle status"
            )?;
            writeln!(out)?;
            writeln!(
                out,
                "USAGE: ryeos system status [--json] [--app-root <DIR>]"
            )?;
        }
        Some("start") => {
            writeln!(out, "ryeos start — Bring the local node runtime online")?;
            writeln!(out)?;
            writeln!(
                out,
                "USAGE: ryeos start [--app-root <DIR>] [--bind <ADDR>] [--uds-path <PATH>]"
            )?;
        }
        Some("stop") => {
            writeln!(out, "ryeos stop — Gracefully stop the local node runtime")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos stop [--force] [--app-root <DIR>]")?;
        }
        Some("execute") => {
            writeln!(out, "ryeos execute — Universal escape hatch")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos execute [--async] <item_ref> [flags...]")?;
            writeln!(out)?;
            writeln!(out, "CONTROL FLAGS:")?;
            writeln!(
                out,
                "  --async         Accepted/background launch for root-executable refs; returns a thread_id"
            )?;
            writeln!(out)?;
            writeln!(out, "PARAMETER INPUT:")?;
            writeln!(out, "  --input <FILE>   Read JSON parameters from a file")?;
            writeln!(out, "  --input -        Read JSON parameters from stdin")?;
            writeln!(
                out,
                "  --key value      Heuristic flag binding (hyphens normalised to underscores)"
            )?;
        }
        Some("sign") => {
            writeln!(
                out,
                "ryeos sign — Sign RyeOS items by canonical ref or glob"
            )?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos sign <item_ref_or_glob> [OPTIONS]")?;
            writeln!(out)?;
            writeln!(out, "EXAMPLES:")?;
            writeln!(out, "  ryeos sign knowledge:my/entry")?;
            writeln!(out, "  ryeos sign 'tool:agent-kiwi/*'")?;
            writeln!(out, "  ryeos sign 'node:routes/*'")?;
            writeln!(out)?;
            writeln!(out, "OPTIONS:")?;
            writeln!(
                out,
                "  --project <DIR>       Project root (parent of .ai/); default: cwd"
            )?;
            writeln!(
                out,
                "  --source <SOURCE>     Where to look: project (default)"
            )?;
        }
        Some("identity") => {
            writeln!(out, "ryeos identity — Print the local node public identity")?;
            writeln!(out)?;
            writeln!(out, "USAGE: ryeos identity [--app-root <DIR>]")?;
        }
        Some(other) => {
            writeln!(out, "no local help available for '{}'", other)?;
            writeln!(out, "run `ryeos init` if Rye OS has not been initialized")?;
        }
        None => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::node_config::NodeConfigSnapshot;

    #[test]
    fn installed_help_reads_command_and_effective_item_metadata() {
        let snapshot = NodeConfigSnapshot {
            bundles: vec![],
            routes: vec![],
            commands: vec![ryeos_runtime::CommandDef {
                name: "remote-doctor".into(),
                tokens: vec!["remote".into(), "doctor".into()],
                description: "Diagnose remote setup".into(),
                aliases: vec![],
                help: None,
                arguments: vec![],
                forms: vec![ryeos_runtime::CommandArgumentForm {
                    slots: vec![ryeos_runtime::CommandArgumentSlot {
                        field: "remote".into(),
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
                    execute: "tool:remote/doctor".into(),
                    availability: ryeos_runtime::CommandAvailability::Auto,
                },
                source_file: PathBuf::from("/tmp/remote-doctor.yaml"),
                provenance: ryeos_runtime::CommandProvenance::default(),
            }],
            hosted_node_policies: vec![],
            command_registration_policy: Default::default(),
        };
        let tokens = vec!["remote".to_string(), "doctor".to_string()];
        let command = crate::node_descriptors::find_command(&snapshot, &tokens).unwrap();
        assert_eq!(command.command.name, "remote-doctor");
        assert_eq!(
            command.command.project.as_ref().unwrap().resolution,
            ryeos_runtime::CommandProjectResolution::Optional
        );
        assert_eq!(command.execute_ref(), Some("tool:remote/doctor"));

        let item = ItemHelpMetadata::from_composed(&serde_json::json!({
            "required_caps": ["ryeos.execute.tool.remote.doctor"],
            "schema": {
                "remote": "string?",
                "project": "string?"
            },
            "description": "Diagnose remote node authorization and project setup",
            "availability": "offline"
        }));
        assert!(item.is_offline_dispatch());
        assert_eq!(item.schema.get("project").unwrap(), "string?");
        assert_eq!(usage_tail(&command, Some(&item)), " <remote>");
    }

    #[test]
    fn installed_direct_execute_help_usage_includes_async_control_flag() {
        let command = LoadedCommandDescriptor {
            command: ryeos_runtime::CommandDef {
                name: "execute".into(),
                tokens: vec!["execute".into()],
                description: "Execute an item".into(),
                aliases: vec![],
                help: None,
                arguments: vec![],
                forms: vec![ryeos_runtime::CommandArgumentForm {
                    slots: vec![ryeos_runtime::CommandArgumentSlot {
                        field: "item_ref".into(),
                        matcher: ryeos_runtime::CommandArgumentKind::CanonicalRef,
                    }],
                }],
                defaults: Default::default(),
                parameter_binding: None,
                control_flags: Vec::new(),
                project: None,
                dispatch: ryeos_runtime::CommandDispatch::DirectExecuteItemRef {
                    item_ref_arg: "item_ref".into(),
                    availability: ryeos_runtime::CommandAvailability::Both,
                },
                source_file: PathBuf::from("/tmp/execute.yaml"),
                provenance: ryeos_runtime::CommandProvenance::default(),
            },
            tokens: vec!["execute".into()],
            description: "Execute an item".into(),
        };

        assert_eq!(
            installed_usage_line(&command, None),
            "ryeos execute [--async] <item-ref>"
        );
    }

    #[test]
    fn installed_help_renders_defaulted_form_slots_as_optional() {
        let mut defaults = std::collections::BTreeMap::new();
        defaults.insert(
            "surface".to_string(),
            serde_json::Value::String("surface:ryeos/studio/atlas".to_string()),
        );
        let command = LoadedCommandDescriptor {
            command: ryeos_runtime::CommandDef {
                name: "web".into(),
                tokens: vec!["web".into()],
                description: "Open Studio".into(),
                aliases: vec![],
                help: None,
                arguments: vec![],
                forms: vec![ryeos_runtime::CommandArgumentForm {
                    slots: vec![ryeos_runtime::CommandArgumentSlot {
                        field: "surface".into(),
                        matcher: ryeos_runtime::CommandArgumentKind::String,
                    }],
                }],
                defaults,
                parameter_binding: None,
                control_flags: Vec::new(),
                project: None,
                dispatch: ryeos_runtime::CommandDispatch::ExecuteRef {
                    execute: "client:ryeos/web".into(),
                    availability: ryeos_runtime::CommandAvailability::Auto,
                },
                source_file: PathBuf::from("/tmp/web.yaml"),
                provenance: ryeos_runtime::CommandProvenance::default(),
            },
            tokens: vec!["web".into()],
            description: "Open Studio".into(),
        };

        assert_eq!(
            installed_usage_line(&command, None),
            "ryeos web [<surface>]"
        );
    }
}
