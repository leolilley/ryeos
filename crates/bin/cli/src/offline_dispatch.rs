//! Descriptor-driven offline command dispatch.
//!
//! Commands that declare `availability: offline` in their service descriptor
//! can run in-process without a daemon. The trust boundary is intentionally
//! the same one the daemon uses for installed bundles: offline descriptors are
//! discovered only through verified installed bundle registrations, and every
//! alias/verb/service/tool descriptor is signature-verified before parsing.
//!
//! Service descriptors are the source of truth for both whether a command may
//! run offline and which tool implements the offline path. The CLI does not
//! keep an endpoint → handler table; adding an offline service means
//! adding/updating descriptors that point at a trusted local `bin:` tool.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use ryeos_engine::contracts::{SignatureEnvelope, TrustClass as ItemTrustClass};
use ryeos_runtime::alias_registry::{AliasDef, PositionalForm, ProjectResolution};
use serde::Deserialize;
use serde_json::Value;

use crate::error::CliError;

#[derive(Debug)]
struct OfflineCatalog {
    system_space_dir: PathBuf,
    bundle_roots: Vec<PathBuf>,
    trust_store: ryeos_engine::trust::TrustStore,
}

#[derive(Debug)]
struct Loaded<T> {
    path: PathBuf,
    value: T,
}

#[derive(Debug)]
struct LoadedAlias {
    path: PathBuf,
    def: AliasDef,
}

#[derive(Debug, Deserialize)]
struct AliasYaml {
    tokens: Vec<String>,
    verb: String,
    #[serde(default)]
    deprecated: Option<bool>,
    #[serde(default)]
    replacement_tokens: Option<Vec<String>>,
    #[serde(default)]
    removed_in: Option<String>,
    #[serde(default)]
    positional_field: Option<String>,
    #[serde(default)]
    positional_forms: Vec<PositionalForm>,
    #[serde(default)]
    project_resolution: ProjectResolution,
}

/// Parsed subset of a service YAML descriptor.
#[derive(Debug, Deserialize)]
struct ServiceDescriptor {
    /// Whether this service may run offline: "offline", "daemon", or "both".
    #[serde(default)]
    availability: Option<String>,
    /// Descriptor-declared local implementation for offline dispatch.
    #[serde(default)]
    offline_execute: Option<String>,
    /// Input schema (field name → type string). Used only for the legacy
    /// `project` → `project_path` compatibility transform after shared binding.
    #[serde(default)]
    schema: HashMap<String, String>,
}

/// Parsed subset of a verb YAML descriptor.
#[derive(Debug, Deserialize)]
struct VerbDescriptor {
    /// Execution target ref: `service:...` or `tool:...`.
    execute: String,
}

#[derive(Debug, Deserialize)]
struct ToolDescriptor {
    #[serde(default)]
    executor_id: Option<String>,
    config: ToolConfig,
}

#[derive(Debug, Deserialize)]
struct ToolConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    input_data: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    inherit_stdio: bool,
    #[serde(default)]
    inherit_env: bool,
}

/// Try to dispatch a command through the offline descriptor path.
///
/// Returns `Ok(Some(result))` if the command was handled offline.
/// Returns `Ok(None)` if the command is not offline-capable (caller should fall
/// through to daemon dispatch). Returns `Err` if the command is offline-capable
/// but something went wrong.
pub fn try_offline_dispatch(
    argv: &[String],
    system_space_dir: &Path,
    project_path: &str,
) -> Result<Option<Value>, CliError> {
    let catalog = build_catalog(system_space_dir, project_path).map_err(local_err)?;
    if catalog.bundle_roots.is_empty() {
        return Ok(None);
    }

    let aliases = load_aliases(&catalog).map_err(local_err)?;
    let Some((alias, consumed)) = match_alias(argv, &aliases).map_err(local_err)? else {
        return Ok(None);
    };

    let Some(verb) = load_verb(&catalog, &alias.def.verb).map_err(local_err)? else {
        return Ok(None);
    };

    let Some(service) =
        resolve_service(&catalog, &alias.def.verb, &verb.value).map_err(local_err)?
    else {
        return Ok(None);
    };

    let is_offline = service
        .value
        .availability
        .as_deref()
        .is_some_and(|a| a == "offline" || a == "both");
    if !is_offline {
        return Ok(None);
    }

    let Some(offline_execute) = resolve_offline_execute(&service.value, &verb.value) else {
        return Err(CliError::Local {
            detail: format!(
                "service `{}` is declared offline-capable, but its descriptor \
                 does not declare a local tool implementation (`offline_execute: tool:<id>`)",
                alias.def.verb
            ),
        });
    };

    let tail = &argv[consumed..];
    let mut params =
        bind_params_shared(tail, &alias.def, &service.value, project_path).map_err(local_err)?;
    if let Some(obj) = params.as_object_mut() {
        obj.insert("_verb".to_string(), Value::String(alias.def.verb.clone()));
    }

    // Strip internal routing fields before passing to the subprocess tool.
    // These are injected by the dispatch layer for daemon use but not
    // consumed by offline tools (which use strict serde structs).
    if let Some(obj) = params.as_object_mut() {
        obj.retain(|key, _| !key.starts_with('_'));
    }

    let result = execute_offline_tool(&catalog, &offline_execute, params, project_path)
        .map_err(local_err)?;

    Ok(Some(result))
}

fn local_err(error: anyhow::Error) -> CliError {
    CliError::Local {
        detail: format!("{error:#}"),
    }
}

fn build_catalog(system_space_dir: &Path, project_path: &str) -> Result<OfflineCatalog> {
    let user_root = ryeos_engine::roots::user_root().ok();
    let records = ryeos_bundle::installed::load_installed_bundle_records(
        system_space_dir,
        user_root.as_deref(),
    )
    .context("offline dispatch: load verified installed bundle registrations")?;
    let bundle_roots: Vec<PathBuf> = records.into_iter().map(|r| r.bundle_root).collect();
    let trust_store = ryeos_engine::trust::TrustStore::load_three_tier(
        Some(Path::new(project_path)),
        user_root.as_deref(),
        &bundle_roots,
    )
    .context("offline dispatch: load operator trust store")?;

    Ok(OfflineCatalog {
        system_space_dir: system_space_dir.to_path_buf(),
        bundle_roots,
        trust_store,
    })
}

fn resolve_offline_execute(service: &ServiceDescriptor, verb: &VerbDescriptor) -> Option<String> {
    service.offline_execute.clone().or_else(|| {
        verb.execute
            .starts_with("tool:")
            .then(|| verb.execute.clone())
    })
}

fn bind_params_shared(
    tail: &[String],
    alias: &AliasDef,
    service: &ServiceDescriptor,
    project_path: &str,
) -> Result<Value> {
    if let Some(input) = crate::arg_bind::parse_input_arg(tail)? {
        return Ok(normalize_project_param(input, service, project_path));
    }

    let normalized = normalize_bare_key_value_args(tail);
    let mut params = ryeos_runtime::arg_binder::bind_argv_with_alias(&normalized, Some(alias))
        .map_err(|e| anyhow::anyhow!(e))?;

    if alias.project_resolution != ProjectResolution::None {
        let mut canonical_tail = params_to_tail(&params);
        let default_project = (project_path != ".").then(|| Path::new(project_path));
        match alias.project_resolution {
            ProjectResolution::None => {}
            ProjectResolution::Optional => {
                canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                    &canonical_tail,
                    default_project,
                )?;
            }
            ProjectResolution::Required => {
                if canonical_tail.iter().any(|t| t == "--no-project") {
                    bail!("this command requires a project; do not pass --no-project");
                }
                canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                    &canonical_tail,
                    default_project,
                )?;
                if canonical_tail.iter().any(|t| t == "--no-project") {
                    bail!(
                        "this command requires a project; run it from a directory containing \
                         {} or pass --project <path>",
                        ryeos_engine::AI_DIR
                    );
                }
            }
        }
        params = ryeos_runtime::bind_argv(&canonical_tail);
    }

    let params = normalize_project_param(params, service, project_path);

    // Reject unknown flags. Typos like `--regstry-root` would otherwise
    // silently pass through as an extra parameter.
    // Internal routing fields (prefixed with `_`) are excluded — they are
    // injected by the dispatch layer, not by the user.
    if let Some(obj) = params.as_object() {
        for key in obj.keys() {
            if key.starts_with('_') {
                continue;
            }
            let normalized_key = key.replace('_', "-");
            if !service.schema.contains_key(key.as_str())
                && !service.schema.contains_key(&normalized_key)
                && key != "input"
            {
                bail!(
                    "unknown parameter --{normalized_key} for this command{}",
                    if service.schema.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " (expected: {})",
                            service
                                .schema
                                .keys()
                                .map(|k| format!("--{}", k.replace('_', "-")))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }
                );
            }
        }
    }

    Ok(params)
}

fn normalize_project_param(
    mut params: Value,
    service: &ServiceDescriptor,
    default_project_path: &str,
) -> Value {
    let Some(obj) = params.as_object_mut() else {
        return params;
    };

    if service.schema.contains_key("project_path") && !service.schema.contains_key("project") {
        if let Some(project) = obj.remove("project") {
            obj.entry("project_path".to_string()).or_insert(project);
        }
    }

    if !obj.contains_key("project")
        && !obj.contains_key("project_path")
        && !obj.contains_key("no_project")
    {
        if service.schema.contains_key("project_path") {
            obj.insert(
                "project_path".to_string(),
                Value::String(default_project_path.to_string()),
            );
        } else if service.schema.contains_key("project") {
            obj.insert(
                "project".to_string(),
                Value::String(default_project_path.to_string()),
            );
        }
    }

    params
}

fn normalize_bare_key_value_args(rest: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(rest.len());
    for token in rest {
        if token.starts_with('-') {
            out.push(token.clone());
            continue;
        }
        if let Some((key, value)) = token.split_once('=') {
            if !key.is_empty() && !value.is_empty() {
                out.push(format!("--{key}"));
                out.push(value.to_string());
                continue;
            }
        }
        out.push(token.clone());
    }
    out
}

fn params_to_tail(params: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = params.as_object() else {
        return out;
    };
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    for key in keys {
        emit_param(&mut out, key, &obj[key]);
    }
    out
}

fn emit_param(out: &mut Vec<String>, key: &str, value: &Value) {
    match value {
        Value::Bool(true) => out.push(format!("--{}", key.replace('_', "-"))),
        Value::Bool(false) | Value::Null => {}
        Value::Array(values) => {
            for v in values {
                emit_param(out, key, v);
            }
        }
        other => {
            out.push(format!("--{}", key.replace('_', "-")));
            out.push(match other {
                Value::String(s) => s.clone(),
                _ => other.to_string(),
            });
        }
    }
}

fn execute_offline_tool(
    catalog: &OfflineCatalog,
    tool_ref: &str,
    params: Value,
    project_path: &str,
) -> Result<Value> {
    let tool = find_tool(catalog, tool_ref)
        .with_context(|| format!("resolve offline tool descriptor `{tool_ref}`"))?;

    match tool.value.executor_id.as_deref() {
        Some("@subprocess") | Some("tool:ryeos/core/subprocess/execute") => {}
        other => bail!(
            "offline tool `{tool_ref}` must use @subprocess executor, got {:?}",
            other
        ),
    }

    let params_json = serde_json::to_string(&params).context("serialize offline params")?;
    let cmd_template = expand_template(&tool.value.config.command, &params_json, project_path)?;
    let cmd = resolve_trusted_bin_command(catalog, &cmd_template, &tool.path)?;
    let args = tool
        .value
        .config
        .args
        .iter()
        .map(|arg| expand_template(arg, &params_json, project_path))
        .collect::<Result<Vec<_>>>()?;
    let stdin_data = tool
        .value
        .config
        .input_data
        .as_deref()
        .map(|input| expand_template(input, &params_json, project_path))
        .transpose()?;
    let cwd = match tool.value.config.cwd.as_deref() {
        Some(cwd) => Some(expand_template(cwd, &params_json, project_path)?),
        None => Some(std::env::current_dir()?.to_string_lossy().into_owned()),
    };
    let mut envs: Vec<(String, String)> = tool
        .value
        .config
        .env
        .iter()
        .map(|(k, v)| expand_template(v, &params_json, project_path).map(|v| (k.clone(), v)))
        .collect::<Result<Vec<_>>>()?;
    envs.push((
        "RYEOS_SYSTEM_SPACE_DIR".to_string(),
        catalog.system_space_dir.to_string_lossy().into_owned(),
    ));

    if tool.value.config.inherit_stdio {
        return execute_inherited_offline_tool(
            tool_ref,
            &cmd,
            &args,
            cwd.as_deref(),
            &envs,
            tool.value.config.inherit_env,
        );
    }

    let result = lillux::run(lillux::SubprocessRequest {
        cmd: cmd.to_string_lossy().into_owned(),
        args,
        cwd,
        envs,
        stdin_data,
        timeout: tool.value.config.timeout_secs.unwrap_or(60) as f64,
    });

    if !result.success {
        bail!(
            "offline tool `{tool_ref}` failed with exit {:?}\nstdout:\n{}\nstderr:\n{}",
            result.exit_code,
            result.stdout,
            result.stderr
        );
    }

    serde_json::from_str(&result.stdout).or_else(|_| Ok(Value::String(result.stdout)))
}

fn execute_inherited_offline_tool(
    tool_ref: &str,
    cmd: &Path,
    args: &[String],
    cwd: Option<&str>,
    envs: &[(String, String)],
    inherit_env: bool,
) -> Result<Value> {
    let mut command = std::process::Command::new(cmd);
    command.args(args);
    if !inherit_env {
        command.env_clear();
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());

    let status = command
        .status()
        .with_context(|| format!("run inherited offline tool `{tool_ref}`"))?;
    if !status.success() {
        bail!(
            "offline tool `{tool_ref}` failed with exit {:?}",
            status.code()
        );
    }
    Ok(serde_json::json!({ "status": "ok" }))
}

fn expand_template(template: &str, params_json: &str, project_path: &str) -> Result<String> {
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        if let Some(end) = rest[start + 1..].find('}') {
            let token = &rest[start + 1..start + 1 + end];
            if token != "params_json" && token != "project_path" {
                bail!("offline tool template references unsupported token {{{token}}}");
            }
            rest = &rest[start + 1 + end + 1..];
        } else {
            bail!("offline tool template contains an unterminated token");
        }
    }

    let mut out = template.replace("{params_json}", params_json);
    out = out.replace("{project_path}", project_path);
    Ok(out)
}

fn resolve_trusted_bin_command(
    catalog: &OfflineCatalog,
    command: &str,
    tool_path: &Path,
) -> Result<PathBuf> {
    if !command.starts_with("bin:") {
        bail!("offline subprocess tools may only execute trusted `bin:` commands, got `{command}`");
    }

    let bundle_root = bundle_root_for_path(catalog, tool_path).with_context(|| {
        format!(
            "resolve installed bundle root for offline tool descriptor {}",
            tool_path.display()
        )
    })?;

    let resolved = ryeos_engine::binary_resolver::resolve_bundle_binary_ref(
        command,
        bundle_root,
        |fp| catalog.trust_store.is_trusted(fp),
        ryeos_engine::resolution::TrustClass::TrustedSystem,
    )
    .with_context(|| format!("resolve offline binary `{command}`"))?;
    Ok(resolved.absolute_path)
}

fn bundle_root_for_path<'a>(catalog: &'a OfflineCatalog, path: &Path) -> Option<&'a PathBuf> {
    catalog
        .bundle_roots
        .iter()
        .find(|root| path.starts_with(root))
}

fn load_aliases(catalog: &OfflineCatalog) -> Result<Vec<LoadedAlias>> {
    let mut out = Vec::new();
    for root in &catalog.bundle_roots {
        let aliases_dir = root.join(ryeos_engine::AI_DIR).join("node").join("aliases");
        let Ok(files) = std::fs::read_dir(&aliases_dir) else {
            continue;
        };
        for file in files {
            let path = file?.path();
            if !is_yaml_file(&path) {
                continue;
            }
            let alias: AliasYaml = load_trusted_yaml(catalog, &path)
                .with_context(|| format!("load trusted alias descriptor {}", path.display()))?;
            out.push(LoadedAlias {
                path,
                def: AliasDef {
                    tokens: alias.tokens,
                    verb: alias.verb,
                    deprecated: alias.deprecated.unwrap_or(false),
                    replacement_tokens: alias.replacement_tokens,
                    removed_in: alias.removed_in,
                    positional_field: alias.positional_field,
                    positional_forms: alias.positional_forms,
                    project_resolution: alias.project_resolution,
                },
            });
        }
    }
    Ok(out)
}

fn load_verb(catalog: &OfflineCatalog, verb_name: &str) -> Result<Option<Loaded<VerbDescriptor>>> {
    let rel = Path::new(ryeos_engine::AI_DIR)
        .join("node")
        .join("verbs")
        .join(format!("{verb_name}.yaml"));
    find_unique_descriptor(catalog, &rel, "verb")
}

fn resolve_service(
    catalog: &OfflineCatalog,
    verb_name: &str,
    verb: &VerbDescriptor,
) -> Result<Option<Loaded<ServiceDescriptor>>> {
    if let Some(service_ref) = verb.execute.strip_prefix("service:") {
        find_service(catalog, service_ref)
    } else {
        let direct = find_service(catalog, verb_name)?;
        if direct.is_some() {
            return Ok(direct);
        }
        find_service(catalog, &verb_name.replace('-', "/"))
    }
}

fn find_service(
    catalog: &OfflineCatalog,
    service_rel: &str,
) -> Result<Option<Loaded<ServiceDescriptor>>> {
    let rel = descriptor_rel_path("services", service_rel)?;
    find_unique_descriptor(catalog, &rel, "service")
}

fn find_tool(catalog: &OfflineCatalog, tool_ref: &str) -> Result<Loaded<ToolDescriptor>> {
    let rel = tool_ref.strip_prefix("tool:").ok_or_else(|| {
        anyhow::anyhow!("offline_execute must be a tool:<id> ref, got `{tool_ref}`")
    })?;
    let rel = descriptor_rel_path("tools", rel)?;
    find_unique_descriptor(catalog, &rel, "tool")?
        .ok_or_else(|| anyhow::anyhow!("offline tool descriptor `{tool_ref}` not found"))
}

fn descriptor_rel_path(section: &str, item_rel: &str) -> Result<PathBuf> {
    if item_rel.contains("..") {
        bail!("descriptor ref `{item_rel}` must not contain path traversal");
    }
    let file = if item_rel.ends_with(".yaml") || item_rel.ends_with(".yml") {
        item_rel.to_string()
    } else {
        format!("{item_rel}.yaml")
    };
    Ok(Path::new(ryeos_engine::AI_DIR).join(section).join(file))
}

fn find_unique_descriptor<T: serde::de::DeserializeOwned>(
    catalog: &OfflineCatalog,
    rel: &Path,
    label: &str,
) -> Result<Option<Loaded<T>>> {
    let mut matches = Vec::new();
    for root in &catalog.bundle_roots {
        let path = root.join(rel);
        if path.exists() {
            matches.push(path);
        }
    }

    match matches.len() {
        0 => Ok(None),
        1 => {
            let path = matches.pop().unwrap();
            let value = load_trusted_yaml(catalog, &path)
                .with_context(|| format!("load trusted {label} descriptor {}", path.display()))?;
            Ok(Some(Loaded { path, value }))
        }
        _ => {
            let paths = matches
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "duplicate {label} descriptor matches for {}: {paths}",
                rel.display()
            )
        }
    }
}

fn match_alias<'a>(
    argv: &[String],
    aliases: &'a [LoadedAlias],
) -> Result<Option<(&'a LoadedAlias, usize)>> {
    for len in (1..=argv.len()).rev() {
        let prefix = &argv[..len];
        let matches: Vec<&LoadedAlias> = aliases
            .iter()
            .filter(|alias| alias.def.tokens == prefix)
            .collect();
        match matches.len() {
            0 => {}
            1 => return Ok(Some((matches[0], len))),
            _ => {
                let paths = matches
                    .iter()
                    .map(|alias| alias.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("duplicate alias descriptor matches for tokens {prefix:?}: {paths}");
            }
        }
    }
    Ok(None)
}

fn load_trusted_yaml<T: serde::de::DeserializeOwned>(
    catalog: &OfflineCatalog,
    path: &Path,
) -> Result<T> {
    let file_type = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat descriptor {}", path.display()))?
        .file_type();
    if file_type.is_symlink() || !file_type.is_file() {
        bail!(
            "descriptor {} is not a regular file (symlinks rejected)",
            path.display()
        );
    }

    if bundle_root_for_path(catalog, path).is_none() {
        bail!(
            "descriptor {} is not under a verified installed bundle root",
            path.display()
        );
    }

    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read descriptor {}", path.display()))?;
    verify_trusted_yaml_signature(&raw, path, &catalog.trust_store)?;
    let body = lillux::signature::strip_signature_lines(&raw);
    serde_yaml::from_str(&body).with_context(|| format!("parse descriptor YAML {}", path.display()))
}

fn verify_trusted_yaml_signature(
    raw: &str,
    path: &Path,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<()> {
    let envelope = yaml_signature_envelope();
    let header = ryeos_engine::item_resolution::parse_signature_header(raw, &envelope)
        .with_context(|| format!("{} has no valid signature line", path.display()))?;
    let (trust_class, _) =
        ryeos_engine::trust::verify_item_signature(raw, &header, &envelope, trust_store)
            .with_context(|| format!("signature verification failed for {}", path.display()))?;
    if trust_class != ItemTrustClass::Trusted {
        bail!(
            "{} is not trusted (trust_class: {:?}); offline descriptors must be signed by a trusted publisher",
            path.display(),
            trust_class
        );
    }
    Ok(())
}

fn yaml_signature_envelope() -> SignatureEnvelope {
    SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    }
}

fn is_yaml_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("yaml") | Some("yml")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::SigningKey;
    use rand::rngs::OsRng;

    struct Fixture {
        _tmp: tempfile::TempDir,
        _env_guard: std::sync::MutexGuard<'static, ()>,
        system: PathBuf,
        project: PathBuf,
        bundle: PathBuf,
        key: SigningKey,
    }

    impl Fixture {
        fn new() -> Self {
            let env_guard = crate::test_env::lock();
            let tmp = tempfile::tempdir().unwrap();
            let system = tmp.path().join("system");
            let user = tmp.path().join("user");
            let project = tmp.path().join("project");
            std::fs::create_dir_all(project.join(ryeos_engine::AI_DIR)).unwrap();
            let trust_dir = user
                .join(ryeos_engine::AI_DIR)
                .join("config")
                .join("keys")
                .join("trusted");
            std::fs::create_dir_all(&trust_dir).unwrap();
            let key = SigningKey::generate(&mut OsRng);
            ryeos_engine::trust::pin_key(&key.verifying_key(), "test", &trust_dir, None).unwrap();
            std::env::set_var("USER_SPACE", &user);

            let bundle = system
                .join(ryeos_engine::AI_DIR)
                .join("bundles")
                .join("test");
            std::fs::create_dir_all(bundle.join(ryeos_engine::AI_DIR)).unwrap();

            let this = Self {
                _tmp: tmp,
                _env_guard: env_guard,
                system,
                project,
                bundle,
                key,
            };
            this.write_manifest();
            this.write_registration();
            this.write_echo_bin();
            this.write_standard_descriptors(Some("offline_execute: tool:custom/echo\n"));
            this
        }

        fn write_standard_descriptors(&self, offline_execute_line: Option<&str>) {
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("node")
                    .join("aliases")
                    .join("custom.yaml"),
                "tokens: [\"custom\"]\nverb: custom\npositional_field: name\n",
            );
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("node")
                    .join("verbs")
                    .join("custom.yaml"),
                "name: custom\nexecute: service:custom\n",
            );
            let offline_execute_line = offline_execute_line.unwrap_or("");
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("services")
                    .join("custom.yaml"),
                &format!(
                    "kind: service\nendpoint: custom\navailability: offline\n{offline_execute_line}schema:\n  name: string?\n  project_path: string?\n"
                ),
            );
            self.write_signed(
                &self
                    .bundle
                    .join(ryeos_engine::AI_DIR)
                    .join("tools")
                    .join("custom")
                    .join("echo.yaml"),
                "executor_id: \"@subprocess\"\nconfig:\n  command: \"bin:echo-json\"\n  input_data: \"{params_json}\"\n",
            );
        }

        fn write_manifest(&self) {
            self.write_signed(
                &self.bundle.join(ryeos_engine::AI_DIR).join("manifest.yaml"),
                "name: test\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n",
            );
        }

        fn write_registration(&self) {
            let path = self
                .system
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("bundles")
                .join("test.yaml");
            self.write_signed(
                &path,
                &format!(
                    "kind: node\nsection: bundles\nid: test\npath: {}\n",
                    self.bundle.display()
                ),
            );
        }

        fn write_echo_bin(&self) {
            let triple = host_triple();
            let ai_dir = self.bundle.join(ryeos_engine::AI_DIR);
            let bin_path = ai_dir.join("bin").join(triple).join("echo-json");
            std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
            let script = b"#!/bin/sh\ncat\n";
            std::fs::write(&bin_path, script).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&bin_path, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }

            let cas = lillux::CasStore::new(ai_dir.join("objects"));
            let content_blob_hash = lillux::sha256_hex(script);
            let item_source = serde_json::json!({
                "content_blob_hash": content_blob_hash,
                "signature_info": {
                    "fingerprint": lillux::signature::compute_fingerprint(&self.key.verifying_key())
                }
            });
            let item_source_hash = cas.store_object(&item_source).unwrap();
            let manifest = serde_json::json!({
                "item_source_hashes": {
                    format!("bin/{triple}/echo-json"): item_source_hash
                }
            });
            let manifest_hash = cas.store_object(&manifest).unwrap();
            let ref_path = ai_dir.join("refs").join("bundles").join("manifest");
            std::fs::create_dir_all(ref_path.parent().unwrap()).unwrap();
            std::fs::write(ref_path, manifest_hash).unwrap();
        }

        fn write_signed(&self, path: &Path, body: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let signed = lillux::signature::sign_content(body, &self.key, "#", None);
            std::fs::write(path, signed).unwrap();
        }

        fn project_str(&self) -> String {
            self.project.to_string_lossy().into_owned()
        }
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "gnu"))]
    fn host_triple() -> &'static str {
        "x86_64-unknown-linux-gnu"
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux", target_env = "musl"))]
    fn host_triple() -> &'static str {
        "x86_64-unknown-linux-musl"
    }

    #[cfg(all(target_arch = "aarch64", target_os = "linux", target_env = "gnu"))]
    fn host_triple() -> &'static str {
        "aarch64-unknown-linux-gnu"
    }

    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    fn host_triple() -> &'static str {
        "aarch64-apple-darwin"
    }

    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    fn host_triple() -> &'static str {
        "x86_64-apple-darwin"
    }

    #[test]
    fn offline_dispatch_executes_descriptor_declared_tool() {
        let fixture = Fixture::new();
        let argv = vec!["custom".to_string(), "leo".to_string()];

        let result = try_offline_dispatch(&argv, &fixture.system, &fixture.project_str())
            .unwrap()
            .expect("handled offline");

        assert_eq!(result["name"], "leo");
        assert_eq!(result["project_path"], fixture.project_str());
    }

    #[test]
    fn offline_service_without_tool_impl_errors_loudly() {
        let fixture = Fixture::new();
        fixture.write_standard_descriptors(None);

        let err = try_offline_dispatch(&["custom".to_string()], &fixture.system, ".").unwrap_err();

        match err {
            CliError::Local { detail } => {
                assert!(detail.contains("offline_execute: tool:<id>"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn offline_dispatch_maps_project_flag_to_project_path_schema() {
        let fixture = Fixture::new();

        let result = try_offline_dispatch(
            &[
                "custom".to_string(),
                "--project".to_string(),
                "/tmp/project".to_string(),
            ],
            &fixture.system,
            ".",
        )
        .unwrap()
        .expect("handled offline");

        assert_eq!(result["project_path"], "/tmp/project");
        assert!(result.get("project").is_none());
    }

    #[test]
    fn duplicate_alias_matches_error_loudly() {
        let fixture = Fixture::new();
        let second_bundle = fixture
            .system
            .join(ryeos_engine::AI_DIR)
            .join("bundles")
            .join("second");
        std::fs::create_dir_all(second_bundle.join(ryeos_engine::AI_DIR)).unwrap();
        fixture.write_signed(
            &second_bundle.join(ryeos_engine::AI_DIR).join("manifest.yaml"),
            "name: second\nversion: '1.0'\nprovides_kinds: []\nrequires_kinds: []\nuses_kinds: []\n",
        );
        fixture.write_signed(
            &fixture
                .system
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("bundles")
                .join("second.yaml"),
            &format!(
                "kind: node\nsection: bundles\nid: second\npath: {}\n",
                second_bundle.display()
            ),
        );
        fixture.write_signed(
            &second_bundle
                .join(ryeos_engine::AI_DIR)
                .join("node")
                .join("aliases")
                .join("custom.yaml"),
            "tokens: [\"custom\"]\nverb: other\n",
        );

        let err = try_offline_dispatch(&["custom".to_string()], &fixture.system, ".").unwrap_err();
        match err {
            CliError::Local { detail } => {
                assert!(
                    detail.contains("duplicate alias descriptor matches"),
                    "{detail}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn offline_tool_rejects_non_bin_command() {
        let fixture = Fixture::new();
        fixture.write_signed(
            &fixture
                .bundle
                .join(ryeos_engine::AI_DIR)
                .join("tools")
                .join("custom")
                .join("echo.yaml"),
            "executor_id: \"@subprocess\"\nconfig:\n  command: \"cat\"\n  input_data: \"{params_json}\"\n",
        );

        let err = try_offline_dispatch(&["custom".to_string()], &fixture.system, ".").unwrap_err();
        match err {
            CliError::Local { detail } => {
                assert!(detail.contains("trusted `bin:` commands"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
