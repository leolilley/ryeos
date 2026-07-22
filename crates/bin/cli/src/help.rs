//! CLI help — static lifecycle section + dynamic command discovery.
//!
//! `ryeos help` prints lifecycle commands (always available) and discovers
//! the rest from installed bundle descriptors on disk. No daemon required.
//! `ryeos help <command>` uses installed descriptors first and only queries the
//! daemon when no local descriptor help is available.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::error::CliError;
use ryeos_app::node_config::NodeConfigSnapshot;
use ryeos_engine::engine::Engine;
use ryeos_runtime::CommandAvailability;
use serde_json::Value;

use crate::node_descriptors::LoadedCommandDescriptor;

const HELP_CACHE_SCHEMA: &str = "ryeos/help-descriptor-cache/v1";
const HELP_CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024;
const HELP_CACHE_MAX_ROWS: usize = 10_000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CachedHelpRow {
    pub tokens: String,
    pub description: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub category: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct HelpDescriptorCache {
    schema: String,
    registration_identity: String,
    rows: Vec<CachedHelpRow>,
}

#[derive(Debug, Clone)]
pub(crate) struct DescriptorHelpRow {
    pub tokens: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub category: String,
    pub dispatch_kind: String,
    pub target_ref: Option<String>,
    pub availability: Option<ryeos_runtime::CommandAvailability>,
    pub descriptor: LoadedCommandDescriptor,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ResolvedCommandHelp {
    pub description: String,
    pub required_caps: Vec<String>,
    pub schema: BTreeMap<String, String>,
    pub availability: Option<String>,
    pub is_offline: bool,
}

/// Build the fast help index directly from verified command descriptors.
///
/// This path intentionally has no app/project arguments: it cannot construct
/// an effective-item engine or resolve an execution target.
pub(crate) fn descriptor_rows(snapshot: &NodeConfigSnapshot) -> Vec<DescriptorHelpRow> {
    let mut rows = snapshot
        .commands
        .iter()
        .filter(|command| !(command.tokens.len() == 1 && command.tokens[0].len() <= 1))
        .map(|command| {
            let descriptor = LoadedCommandDescriptor {
                command: command.clone(),
                tokens: command.tokens.clone(),
                description: command.description.clone(),
            };
            let (dispatch_kind, target_ref, availability) = match &command.dispatch {
                ryeos_runtime::CommandDispatch::Group => ("group", None, None),
                ryeos_runtime::CommandDispatch::LocalHandler { handler, .. } => {
                    ("local_handler", Some(handler.clone()), None)
                }
                ryeos_runtime::CommandDispatch::DirectExecuteItemRef { availability, .. } => {
                    ("direct_execute_item_ref", None, Some(*availability))
                }
                ryeos_runtime::CommandDispatch::ExecuteRef {
                    execute,
                    availability,
                } => ("execute_ref", Some(execute.clone()), Some(*availability)),
            };
            DescriptorHelpRow {
                tokens: command.tokens.join(" "),
                description: command.description.clone(),
                aliases: command
                    .aliases
                    .iter()
                    .map(|alias| alias.tokens.join(" "))
                    .collect(),
                category: command
                    .tokens
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "command".to_string()),
                dispatch_kind: dispatch_kind.to_string(),
                target_ref,
                availability,
                descriptor,
            }
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.tokens.cmp(&right.tokens));
    rows
}

pub(crate) fn read_cached_descriptor_rows(app_root: &Path) -> Vec<CachedHelpRow> {
    let path = help_cache_path(app_root);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= HELP_CACHE_MAX_BYTES =>
        {
            metadata
        }
        _ => return Vec::new(),
    };
    if metadata.len() == 0 {
        return Vec::new();
    }
    let source = match lillux::read_regular_file_bounded_no_follow(&path, HELP_CACHE_MAX_BYTES)
        .and_then(|bytes| String::from_utf8(bytes).map_err(anyhow::Error::from))
    {
        Ok(source) => source,
        Err(_) => return Vec::new(),
    };
    let cache: HelpDescriptorCache = match serde_json::from_str(&source) {
        Ok(cache) => cache,
        Err(_) => return Vec::new(),
    };
    if cache.schema != HELP_CACHE_SCHEMA
        || cache.registration_identity.len() != 64
        || !cache
            .registration_identity
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        || cache.rows.len() > HELP_CACHE_MAX_ROWS
    {
        return Vec::new();
    }
    cache
        .rows
        .into_iter()
        .filter(|row| {
            valid_cache_field(&row.tokens)
                && valid_cache_field(&row.description)
                && valid_cache_field(&row.category)
                && row.aliases.len() <= 64
                && row.aliases.iter().all(|alias| valid_cache_field(alias))
        })
        .collect()
}

pub(crate) fn write_verified_descriptor_cache(
    app_root: &Path,
    snapshot: &NodeConfigSnapshot,
    rows: &[DescriptorHelpRow],
) -> anyhow::Result<()> {
    let cache = HelpDescriptorCache {
        schema: HELP_CACHE_SCHEMA.to_string(),
        registration_identity: verified_registration_identity(snapshot)?,
        rows: rows
            .iter()
            .map(|row| CachedHelpRow {
                tokens: row.tokens.clone(),
                description: row.description.clone(),
                aliases: row.aliases.clone(),
                category: row.category.clone(),
            })
            .collect(),
    };
    let bytes = serde_json::to_vec(&cache)?;
    if bytes.len() as u64 > HELP_CACHE_MAX_BYTES {
        anyhow::bail!("verified help descriptor cache exceeds size limit");
    }
    lillux::atomic_write(&help_cache_path(app_root), &bytes)
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(())
}

fn help_cache_path(app_root: &Path) -> std::path::PathBuf {
    app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("cache")
        .join("cli")
        .join("help-descriptors-v1.json")
}

fn verified_registration_identity(snapshot: &NodeConfigSnapshot) -> anyhow::Result<String> {
    let mut identity = Vec::new();
    for bundle in &snapshot.bundles {
        identity.extend_from_slice(bundle.name.as_bytes());
        identity.push(0);
        identity.extend_from_slice(&fs::read(&bundle.source_file).map_err(|error| {
            anyhow::anyhow!(
                "read verified registration {}: {error}",
                bundle.source_file.display()
            )
        })?);
        identity.push(0xff);
    }
    identity.extend_from_slice(&serde_json::to_vec(&snapshot.commands)?);
    Ok(lillux::cas::sha256_hex(&identity))
}

fn valid_cache_field(value: &str) -> bool {
    value.len() <= 16 * 1024
        && !value
            .chars()
            .any(|value| value.is_control() && !matches!(value, '\n' | '\t'))
}

/// Resolve effective metadata for one selected descriptor. Callers are
/// expected to run this off the terminal event task.
pub(crate) fn resolve_selected_command_help(
    descriptor: &LoadedCommandDescriptor,
    snapshot: &NodeConfigSnapshot,
    app_root: &Path,
    project_path: &str,
) -> Result<ResolvedCommandHelp, String> {
    let Some(execute_ref) = descriptor.execute_ref() else {
        return Ok(ResolvedCommandHelp::default());
    };
    let bundle_roots = crate::effective_metadata::snapshot_bundle_roots(snapshot);
    let project_root = (project_path != ".").then(|| Path::new(project_path));
    let engine = crate::effective_metadata::build_effective_item_engine(
        app_root,
        project_root,
        &bundle_roots,
    )
    .map_err(|error| format!("{error:#}"))?;
    let value = crate::effective_metadata::resolve_effective_composed_value(
        &engine,
        execute_ref,
        project_root,
    )
    .map_err(|error| format!("{error:#}"))?
    .ok_or_else(|| format!("effective target '{execute_ref}' was not found"))?;
    let metadata = ItemHelpMetadata::from_composed(&value);
    let is_offline = command_is_offline_capable(
        command_availability(descriptor),
        metadata.is_offline_dispatch(),
    );
    Ok(ResolvedCommandHelp {
        description: metadata.description,
        required_caps: metadata.required_caps,
        schema: metadata.schema,
        availability: metadata.availability,
        is_offline,
    })
}

/// Immediate shell-native reference used by `ryeos --help` and `ryeos -h`.
/// It deliberately avoids loading installed node state.
pub fn print_quick_help(console: &crate::tty::Console) -> std::io::Result<()> {
    let mut document = crate::tty::Document::titled("CLI FOR RYEOS");
    document.sections.push(
        crate::tty::Section::named("usage")
            .row("ryeos <command> [args...]", "Run a RyeOS command")
            .row("ryeos help", "Browse and search verified commands")
            .row("ryeos commands", "Print the exhaustive command reference"),
    );
    let mut lifecycle = crate::tty::Section::named("lifecycle");
    lifecycle.rows = crate::lifecycle_commands::local_command_descriptors()
        .iter()
        .filter(|descriptor| {
            !matches!(
                descriptor.tokens,
                ["help"] | ["help", "--all"] | ["commands"]
            )
        })
        .map(|descriptor| {
            crate::tty::Row::key_value(descriptor.tokens.join(" "), descriptor.summary)
        })
        .collect();
    document.sections.push(lifecycle);
    document.hints.push(crate::tty::Hint::new(
        "Run `ryeos help` for interactive search or `ryeos help --plain` for deterministic text.",
    ));
    console.document(&document)
}

/// Print top-level help. Static lifecycle help always renders. Dynamic command
/// discovery is included only after installed node config verifies; verification
/// failures are surfaced as warnings instead of being treated as absent config.
///
/// The snapshot is loaded once per invocation in `dispatcher::run` and
/// threaded through so help never re-reads node config from disk.
pub fn print_help(
    console: &crate::tty::Console,
    app_root: &Path,
    snapshot: &anyhow::Result<NodeConfigSnapshot>,
) -> std::io::Result<()> {
    let (document, warning) = build_top_level_help(app_root, snapshot);
    if let Some(warning) = warning {
        console.warning(&warning)?;
    }
    console.document(&document)
}

fn build_top_level_help(
    app_root: &Path,
    snapshot: &anyhow::Result<NodeConfigSnapshot>,
) -> (crate::tty::Document, Option<crate::tty::Diagnostic>) {
    let mut document = crate::tty::Document::titled("CLI FOR RYEOS");
    let mut usage = crate::tty::Section::named("usage");
    usage.rows.push(crate::tty::Row::text(
        "ryeos [-p PROJECT] [--debug] <command...> [args...]",
    ));
    document.sections.push(usage);
    let mut lifecycle = crate::tty::Section::named("lifecycle");
    lifecycle.rows = vec![
        crate::tty::Row::key_value("init", "Bootstrap local node state and packaged bundles"),
        crate::tty::Row::key_value("start", "Bring the local node runtime online"),
        crate::tty::Row::key_value("stop", "Gracefully stop the local node runtime"),
        crate::tty::Row::key_value("node status", "Show local node lifecycle status"),
        crate::tty::Row::key_value(
            "node doctor",
            "Offline checklist answering \"why won't it start\"",
        ),
        crate::tty::Row::key_value("node gc", "Run explicit offline node garbage collection"),
        crate::tty::Row::key_value(
            "node auth-reset",
            "Reset authorized keys for a schema cutover",
        ),
    ];
    document.sections.push(lifecycle);
    document.sections.push(
        crate::tty::Section::named("universal escape hatch")
            .row(
                "execute [--async] <item_ref>",
                "Execute any canonical item ref directly",
            )
            .row(
                "--input <file>",
                "Pass JSON parameters from file (or - for stdin)",
            ),
    );

    // ── Dynamic command discovery from installed bundles ──
    let (snapshot, warning) = match snapshot {
        Ok(snapshot) => (Some(snapshot), None),
        Err(err) => (
            None,
            Some(crate::tty::Diagnostic::warning(format!(
                "installed node config failed verification: {err:#}"
            ))),
        ),
    };
    let discovered = snapshot
        .map(|snapshot| command_rows(snapshot, app_root, "."))
        .unwrap_or_default();

    if !discovered.is_empty() {
        let mut offline_cmds: Vec<(&str, &str)> = Vec::new();
        let mut daemon_cmds: Vec<(&str, &str)> = Vec::new();

        for row in &discovered {
            if row.is_offline {
                offline_cmds.push((&row.tokens, &row.description));
            } else {
                daemon_cmds.push((&row.tokens, &row.description));
            }
        }

        if !offline_cmds.is_empty() {
            offline_cmds.sort_by_key(|c| c.0);
            let mut section = crate::tty::Section::named("offline (no daemon required)");
            for (tokens_str, description) in &offline_cmds {
                section
                    .rows
                    .push(crate::tty::Row::key_value(*tokens_str, *description));
            }
            document.sections.push(section);
        }

        if !daemon_cmds.is_empty() {
            daemon_cmds.sort_by_key(|c| c.0);
            let mut section = crate::tty::Section::named("daemon (requires running daemon)");
            for (tokens_str, description) in &daemon_cmds {
                section
                    .rows
                    .push(crate::tty::Row::key_value(*tokens_str, *description));
            }
            document.sections.push(section);
        }
    }

    document.hints.push(crate::tty::Hint::new(
        "Run `ryeos help <command>` for command-specific help.",
    ));
    document.hints.push(crate::tty::Hint::new(
        "Set `RYEOS_TTY=always` or `RYEOS_TTY=never` to override terminal presentation detection.",
    ));
    (document, warning)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandHelpRow {
    pub tokens: String,
    pub description: String,
    pub is_offline: bool,
}

/// Authoritative row model shared by exhaustive and compact help.
pub(crate) fn command_rows(
    snapshot: &NodeConfigSnapshot,
    app_root: &Path,
    project_path: &str,
) -> Vec<CommandHelpRow> {
    let mut results = Vec::new();
    let bundle_roots = crate::effective_metadata::snapshot_bundle_roots(snapshot);
    let project_root = (project_path != ".").then(|| Path::new(project_path));
    let engine = crate::effective_metadata::build_effective_item_engine(
        app_root,
        project_root,
        &bundle_roots,
    )
    .ok();
    let commands = crate::node_descriptors::load_command_descriptors_from_snapshot(snapshot);
    let mut metadata_by_ref = BTreeMap::<String, Option<ItemHelpMetadata>>::new();

    for command in commands {
        // Skip short aliases (s, f) — they're abbreviations.
        if command.tokens.len() == 1 && command.tokens[0].len() <= 1 {
            continue;
        }

        // Aliases share the same dispatch item as their canonical command.
        // Resolve and verify that item once instead of repeating the complete
        // effective-item pipeline for every spelling shown in help.
        let metadata = command.execute_ref().and_then(|execute| {
            metadata_by_ref
                .entry(execute.to_string())
                .or_insert_with(|| {
                    engine
                        .as_ref()
                        .and_then(|engine| resolve_effective_help(engine, execute, project_path))
                })
                .as_ref()
        });
        let is_offline = command_is_offline_capable(
            command_availability(&command),
            metadata.is_some_and(ItemHelpMetadata::is_offline_dispatch),
        );
        let description = metadata
            .map(|metadata| metadata.description.as_str())
            .filter(|description| !description.is_empty())
            .unwrap_or(&command.description)
            .to_string();
        results.push(CommandHelpRow {
            tokens: command.tokens.join(" "),
            description,
            is_offline,
        });
    }

    results.sort_by(|a, b| a.tokens.cmp(&b.tokens));
    results
}

/// Print command-specific help from installed descriptors or local bootstrap help.
pub async fn print_command_help(
    console: &crate::tty::Console,
    command_tokens: &[String],
    app_root: &std::path::Path,
    project_path: &str,
    snapshot: &anyhow::Result<NodeConfigSnapshot>,
) -> Result<(), CliError> {
    let document = build_command_help(command_tokens, app_root, project_path, snapshot)?;
    console.document(&document)?;
    Ok(())
}

fn build_command_help(
    command_tokens: &[String],
    app_root: &Path,
    project_path: &str,
    snapshot: &anyhow::Result<NodeConfigSnapshot>,
) -> std::io::Result<crate::tty::Document> {
    let is_lifecycle = crate::lifecycle_commands::local_command_descriptors()
        .iter()
        .any(|descriptor| {
            descriptor.tokens.len() == command_tokens.len()
                && descriptor
                    .tokens
                    .iter()
                    .zip(command_tokens)
                    .all(|(expected, actual)| *expected == actual)
        });
    if is_lifecycle {
        return Ok(build_lifecycle_command_help(command_tokens));
    }
    if let Some(document) =
        build_installed_command_help(command_tokens, app_root, project_path, snapshot)?
    {
        return Ok(document);
    }
    Ok(build_lifecycle_command_help(command_tokens))
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

fn command_availability(descriptor: &LoadedCommandDescriptor) -> Option<CommandAvailability> {
    match &descriptor.command.dispatch {
        ryeos_runtime::CommandDispatch::DirectExecuteItemRef { availability, .. }
        | ryeos_runtime::CommandDispatch::ExecuteRef { availability, .. } => Some(*availability),
        ryeos_runtime::CommandDispatch::Group
        | ryeos_runtime::CommandDispatch::LocalHandler { .. } => None,
    }
}

fn command_is_offline_capable(
    availability: Option<CommandAvailability>,
    target_is_offline_capable: bool,
) -> bool {
    match availability {
        Some(CommandAvailability::Daemon) => false,
        Some(CommandAvailability::Offline | CommandAvailability::Both) => true,
        Some(CommandAvailability::Auto) | None => target_is_offline_capable,
    }
}

fn command_dispatch_mode(
    availability: Option<CommandAvailability>,
    target_is_offline_capable: Option<bool>,
) -> Option<&'static str> {
    match availability {
        Some(CommandAvailability::Daemon) => Some("daemon (requires running daemon)"),
        Some(CommandAvailability::Offline) => Some("offline (no daemon required)"),
        Some(CommandAvailability::Both) => Some("offline or daemon"),
        Some(CommandAvailability::Auto) => target_is_offline_capable.map(|is_offline| {
            if is_offline {
                "offline (no daemon required)"
            } else {
                "daemon (requires running daemon)"
            }
        }),
        None => None,
    }
}

fn build_installed_command_help(
    command_tokens: &[String],
    app_root: &std::path::Path,
    project_path: &str,
    snapshot: &anyhow::Result<NodeConfigSnapshot>,
) -> std::io::Result<Option<crate::tty::Document>> {
    let snapshot = snapshot.as_ref().map_err(|err| {
        std::io::Error::other(format!(
            "installed node config failed verification: {err:#}"
        ))
    })?;
    let bundle_roots = crate::effective_metadata::snapshot_bundle_roots(snapshot);
    let Some(command_descriptor) = crate::node_descriptors::find_command(snapshot, command_tokens)
    else {
        return Ok(None);
    };
    let execute_ref = command_descriptor.execute_ref();
    let project_root = (project_path != ".").then(|| Path::new(project_path));
    let engine = crate::effective_metadata::build_effective_item_engine(
        app_root,
        project_root,
        &bundle_roots,
    )
    .ok();
    let item = execute_ref.and_then(|execute| {
        engine
            .as_ref()
            .and_then(|engine| resolve_effective_help(engine, execute, project_path))
    });

    let command = command_descriptor.tokens.join(" ");
    let description = item
        .as_ref()
        .map(|s| s.description.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&command_descriptor.description);
    let mut document = crate::tty::Document::titled(format!("ryeos {command}"));
    let mut overview = crate::tty::Section::named("overview");
    overview
        .rows
        .push(crate::tty::Row::text(description.to_string()));
    document.sections.push(overview);
    if let Some(mode) = command_dispatch_mode(
        command_availability(&command_descriptor),
        item.as_ref().map(ItemHelpMetadata::is_offline_dispatch),
    ) {
        document
            .sections
            .push(crate::tty::Section::named("dispatch").row("mode", mode));
    } else if let Some(execute_ref) = execute_ref {
        document
            .sections
            .push(crate::tty::Section::named("execute").row("item", execute_ref));
    }
    let mut usage = crate::tty::Section::named("usage");
    usage.rows.push(crate::tty::Row::text(installed_usage_line(
        &command_descriptor,
        item.as_ref(),
    )));
    document.sections.push(usage);

    // Control flags, rendered from the command's declared `control_flags`
    // (data-driven — no hardcoded flag list). Value flags (method/args) show a
    // `<value>` hint; aliases are listed alongside the primary spelling.
    if !command_descriptor.command.control_flags.is_empty() {
        let mut section = crate::tty::Section::named("control flags");
        for cf in &command_descriptor.command.control_flags {
            let mut names = vec![format!("--{}", cf.flag)];
            names.extend(cf.aliases.iter().map(|alias| format!("--{alias}")));
            let value_hint = if cf.binding.takes_value() {
                " <value>"
            } else {
                ""
            };
            section.rows.push(crate::tty::Row::key_value(
                format!("{}{}", names.join(", "), value_hint),
                &cf.help,
            ));
        }
        document.sections.push(section);
    }

    // Parameter passing, from the command's declared `parameter_binding`.
    if let Some(binding) = &command_descriptor.command.parameter_binding {
        if !matches!(
            binding.mode,
            ryeos_runtime::CommandParameterBindingMode::None
        ) {
            let mut section = crate::tty::Section::named("parameters");
            if let Some(input_flag) = &binding.input_flag {
                section.rows.push(crate::tty::Row::key_value(
                    format!("--{input_flag} <file>"),
                    "Read JSON parameters from a file (or - for stdin)",
                ));
            }
            section.rows.push(crate::tty::Row::key_value(
                "--<key> <value>",
                "Set parameter <key> (repeatable)",
            ));
            if binding.single_json_object_arg {
                section.rows.push(crate::tty::Row::key_value(
                    "'<json>'",
                    "A single JSON object of parameters",
                ));
            }
            document.sections.push(section);
        }
    }

    let project_resolution = command_descriptor
        .command
        .project
        .as_ref()
        .map(|project| project.resolution)
        .unwrap_or_default();
    if project_resolution != ryeos_runtime::CommandProjectResolution::None {
        let mut section = crate::tty::Section::named("project");
        section.rows.push(crate::tty::Row::key_value(
            "--project <DIR>",
            "Project root; accepted before or after the command",
        ));
        if project_resolution == ryeos_runtime::CommandProjectResolution::Optional {
            section.rows.push(crate::tty::Row::key_value(
                "--no-project",
                "Resolve against bundles only",
            ));
        }
        document.sections.push(section);
    }

    if let Some(item) = &item {
        if !item.schema.is_empty() {
            let mut section = crate::tty::Section::named("fields");
            for (field, ty) in &item.schema {
                let flag = field.replace('_', "-");
                section
                    .rows
                    .push(crate::tty::Row::key_value(format!("--{flag}"), ty));
            }
            document.sections.push(section);
        }
        if !item.required_caps.is_empty() {
            let mut section = crate::tty::Section::named("required capabilities");
            for cap in &item.required_caps {
                section.rows.push(crate::tty::Row::text(cap));
            }
            document.sections.push(section);
        }
    }

    Ok(Some(document))
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

fn resolve_effective_help(
    engine: &Engine,
    execute_ref: &str,
    project_path: &str,
) -> Option<ItemHelpMetadata> {
    let project_root = (project_path != ".").then(|| Path::new(project_path));
    crate::effective_metadata::resolve_effective_composed_value(engine, execute_ref, project_root)
        .ok()
        .flatten()
        .map(|value| ItemHelpMetadata::from_composed(&value))
}

/// Build help for lifecycle commands when installed descriptors are unavailable.
fn build_lifecycle_command_help(command_tokens: &[String]) -> crate::tty::Document {
    let command = command_tokens.join(" ");
    let (title, description, usage) = match command.as_str() {
        "init" => (
            "ryeos init",
            "Run interactive first-contact onboarding, or bootstrap non-interactively",
            "ryeos init [--non-interactive | --json] [OPTIONS]",
        ),
        "setup" => (
            "ryeos setup",
            "Configure a verified model provider and default model",
            "ryeos setup [--app-root <DIR>]",
        ),
        "node status" => (
            "ryeos node status",
            "Show local node lifecycle status",
            "ryeos node status [--json] [--app-root <DIR>]",
        ),
        "node doctor" => (
            "ryeos node doctor",
            "Diagnose the local node environment",
            "ryeos node doctor [--json] [--no-bundles] [--app-root <DIR>]",
        ),
        "node gc" => (
            "ryeos node gc",
            "Run explicit offline node garbage collection",
            "ryeos node gc [--json] [--dry-run] [--app-root <DIR>]",
        ),
        "start" => (
            "ryeos start",
            "Bring the local node runtime online",
            "ryeos start [--app-root <DIR>] [--bind <ADDR>] [--uds-path <PATH>]",
        ),
        "stop" => (
            "ryeos stop",
            "Gracefully stop the local node runtime",
            "ryeos stop [--force] [--app-root <DIR>]",
        ),
        "execute" => (
            "ryeos execute",
            "Universal escape hatch",
            "ryeos execute [--async] <item_ref> [flags...]",
        ),
        "sign" => (
            "ryeos sign",
            "Sign RyeOS items by canonical ref, glob, or .ai path",
            "ryeos sign <item_ref_or_glob_or_path> [...more] [OPTIONS]",
        ),
        "identity" => (
            "ryeos identity",
            "Print the local node public identity",
            "ryeos identity [--json] [--app-root <DIR>]",
        ),
        _ => {
            let mut document =
                crate::tty::Document::titled(format!("no local help available for '{command}'"));
            document.hints.push(crate::tty::Hint::new(
                "Run `ryeos init` if RyeOS has not been initialized.",
            ));
            return document;
        }
    };
    let mut document = crate::tty::Document::titled(title);
    let mut overview = crate::tty::Section::named("overview");
    overview.rows.push(crate::tty::Row::text(description));
    document.sections.push(overview);
    let mut usage_section = crate::tty::Section::named("usage");
    usage_section.rows.push(crate::tty::Row::text(usage));
    document.sections.push(usage_section);

    let rows: &[(&str, &str)] = match command.as_str() {
        "init" => &[
            (
                "--non-interactive",
                "Run initialization without onboarding prompts",
            ),
            ("--json", "Emit the structured non-interactive report"),
            (
                "--source <DIR>",
                "Bundle source directory (default: /usr/share/ryeos)",
            ),
            (
                "--trust-file <FILE>",
                "Additional publisher trust document (repeatable)",
            ),
            ("--app-root <DIR>", "Application root"),
        ],
        "setup" => &[("--app-root <DIR>", "Application root")],
        "execute" => &[
            ("--async", "Launch in the background and return a thread ID"),
            (
                "--input <FILE>",
                "Read JSON parameters from a file, or - for stdin",
            ),
            (
                "--key <value>",
                "Bind a parameter; hyphens normalize to underscores",
            ),
        ],
        "sign" => &[
            (
                "--project <DIR>",
                "Project root (parent of .ai/); default: cwd",
            ),
            ("--source <SOURCE>", "Item source; default: project"),
        ],
        _ => &[],
    };
    if !rows.is_empty() {
        let mut options = crate::tty::Section::named("options");
        options.rows.extend(
            rows.iter()
                .map(|(key, value)| crate::tty::Row::key_value(*key, *value)),
        );
        document.sections.push(options);
    }
    if command == "node doctor" {
        let mut checks = crate::tty::Section::named("checks");
        checks.rows.push(crate::tty::Row::text(
            "Initialization, lifecycle and binary skew, writable storage, socket bindability, verified node configuration, and per-bundle static diagnostics.",
        ));
        document.sections.push(checks);
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::node_config::NodeConfigSnapshot;
    use std::path::PathBuf;

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
    fn command_availability_is_authoritative_over_target_offline_shape() {
        assert!(!command_is_offline_capable(
            Some(CommandAvailability::Daemon),
            true
        ));
        assert!(command_is_offline_capable(
            Some(CommandAvailability::Offline),
            false
        ));
        assert!(command_is_offline_capable(
            Some(CommandAvailability::Both),
            false
        ));
        assert!(command_is_offline_capable(
            Some(CommandAvailability::Auto),
            true
        ));

        assert_eq!(
            command_dispatch_mode(Some(CommandAvailability::Daemon), Some(true)),
            Some("daemon (requires running daemon)")
        );
        assert_eq!(
            command_dispatch_mode(Some(CommandAvailability::Offline), Some(false)),
            Some("offline (no daemon required)")
        );
        assert_eq!(
            command_dispatch_mode(Some(CommandAvailability::Both), Some(false)),
            Some("offline or daemon")
        );
    }

    #[test]
    fn installed_help_renders_defaulted_form_slots_as_optional() {
        let mut defaults = std::collections::BTreeMap::new();
        defaults.insert(
            "surface".to_string(),
            serde_json::Value::String("surface:ryeos/ui/atlas".to_string()),
        );
        let command = LoadedCommandDescriptor {
            command: ryeos_runtime::CommandDef {
                name: "web".into(),
                tokens: vec!["web".into()],
                description: "Open RyeOS UI".into(),
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
            description: "Open RyeOS UI".into(),
        };

        assert_eq!(
            installed_usage_line(&command, None),
            "ryeos web [<surface>]"
        );
    }
}
