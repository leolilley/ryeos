//! Shared compilation primitives for verified command invocations.
//!
//! Command descriptors define two namespaces which must never be collapsed:
//! item parameters and invocation controls.  Both the CLI and daemon token
//! entry points use this module so project selectors and execution controls
//! cannot leak into a closed service payload or acquire different meanings.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use ryeos_runtime::{
    CommandControlFlag, CommandDef, CommandProjectDefault, CommandProjectResolution,
    ControlFlagBinding,
};
use serde_json::Value;

/// Input syntax only; this grants no authority. Direct API JSON is typed by
/// default. Command arguments are normalized against the admitted callee.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParameterEncoding {
    #[default]
    Typed,
    Command,
}

pub fn normalize_selected_parameters(
    encoding: ParameterEncoding,
    parameters: &mut Value,
    engine: &ryeos_engine::engine::Engine,
    item_ref: &str,
    project_root: Option<PathBuf>,
    authority: ryeos_engine::contracts::SubjectResolutionAuthority,
) -> Result<(), String> {
    if encoding == ParameterEncoding::Typed {
        return Ok(());
    }
    let effective = engine
        .effective_item(ryeos_engine::engine::EffectiveItemRequest {
            item_ref: ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)
                .map_err(|e| e.to_string())?,
            expected_kind: None,
            project_root,
            subject_resolution_authority: authority,
        })
        .map_err(|e| format!("resolve command input contract: {e}"))?;
    if let Some(schema) = effective.composed_value.get("schema") {
        let contract =
            ryeos_runtime::InvocationInputContract::from_lightweight_schema_value(schema)?;
        *parameters = ryeos_runtime::arg_binder::normalize_params_with_contract(
            std::mem::take(parameters),
            contract.as_ref(),
        )?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum CommandProjectPolicyError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    ProjectRequired(String),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommandInvocationControls {
    pub async_launch: bool,
    pub pin_project_at_admission: bool,
    pub pin_current_head_at_admission: bool,
    pub retain_child_results: bool,
    pub exclude_operator_vault: bool,
    pub stream: Option<bool>,
    pub debug_raw: bool,
    pub call_method: Option<String>,
    pub call_args: Option<Value>,
    pub state_root: Option<String>,
    pub ref_bindings: BTreeMap<String, String>,
    pub product_selections: Option<Value>,
}

/// A compiled command is caller intent, never admitted authority. Both terminal
/// and daemon callers submit this through normal execution admission.
#[derive(Debug)]
pub struct CompiledCommandInvocation {
    pub item_ref: String,
    pub parameters: Value,
    pub project_path: Option<PathBuf>,
    pub controls: CommandInvocationControls,
    pub execution_policy: crate::execution_policy::ExecutionPolicy,
    pub validate_only: bool,
    pub direct_execute: bool,
}

/// Compile the complete signed grammar. `load_input` is a caller-owned input
/// adapter: only the terminal may read terminal files/stdin. It is never a
/// request to read arbitrary files on the daemon.
pub fn compile_command_invocation(
    command: &CommandDef,
    tail: &[String],
    arguments: &Value,
    default_project: Option<&Path>,
    caller_cwd: Option<&Path>,
    load_input: impl Fn(&str) -> Result<Value, String>,
) -> Result<CompiledCommandInvocation, CommandProjectPolicyError> {
    use CommandProjectPolicyError::Invalid;
    use ryeos_runtime::CommandDispatch;
    let mut tail = tail.to_vec();
    if command.forms.is_empty()
        && command
            .project
            .as_ref()
            .map(|p| p.resolution)
            .unwrap_or_default()
            == CommandProjectResolution::None
    {
        tail = strip_project_control_flags(&tail);
    }
    let mut controls =
        strip_declared_control_flags(&mut tail, &command.control_flags).map_err(Invalid)?;
    let mut overlay = arguments.clone();
    let direct_execute = matches!(
        command.dispatch,
        CommandDispatch::DirectExecuteItemRef { .. }
    );
    let (item_ref, validate_only) = match &command.dispatch {
        CommandDispatch::ExecuteRef { execute, .. } => (execute.clone(), false),
        CommandDispatch::DirectExecuteItemRef {
            item_ref_arg,
            validate_only,
            ..
        } => {
            // A positional target leaves all structured item fields untouched.
            // If the target is supplied structurally, consume that field only.
            let target = if tail.first().is_some_and(|s| !s.starts_with('-')) {
                tail.remove(0)
            } else {
                overlay
                    .as_object_mut()
                    .and_then(|o| o.remove(item_ref_arg))
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .ok_or_else(|| {
                        Invalid(format!(
                            "command '{}' requires argument '{}'",
                            command.name, item_ref_arg
                        ))
                    })?
            };
            ryeos_engine::canonical_ref::CanonicalRef::parse(&target)
                .map_err(|e| Invalid(format!("invalid item ref: {e}")))?;
            (target, *validate_only)
        }
        _ => {
            return Err(Invalid(format!(
                "command '{}' does not dispatch to an executable item ref",
                command.name
            )));
        }
    };
    let direct_command;
    let binding_command = if direct_execute {
        direct_command = CommandDef {
            forms: Vec::new(),
            ..command.clone()
        };
        &direct_command
    } else {
        command
    };
    let mut forbidden = vec!["--project-path".to_owned()];
    if let Some(binding) = command
        .project
        .as_ref()
        .and_then(|p| p.bind_parameter.as_deref())
    {
        let flag = format!("--{}", binding.replace('_', "-"));
        if flag != "--project" {
            forbidden.push(flag);
        }
    }
    if let Some(flag) = forbidden.iter().find(|flag| {
        tail.iter()
            .any(|s| s == *flag || s.starts_with(&format!("{flag}=")))
    }) {
        return Err(Invalid(format!(
            "{flag} is a runtime-bound service field, not a project selector; use --project <path>"
        )));
    }
    let (mut parameters, project_path) = if direct_execute {
        let (payload, selectors) = separate_project_control_flags(&tail).map_err(Invalid)?;
        let mut selectors = Value::Object(selectors);
        let project_path = apply_project_policy_with_context(
            command,
            &mut selectors,
            default_project,
            caller_cwd,
        )?;
        let mut parameters = bind_command_input(binding_command, &payload, &overlay, &load_input)
            .map_err(Invalid)?;
        for (field, value) in selectors.as_object().expect("selector object") {
            let object = parameters
                .as_object_mut()
                .ok_or_else(|| Invalid("command parameters must be a JSON object".into()))?;
            if object.contains_key(field) {
                return Err(Invalid(format!(
                    "parameter '{field}' conflicts with the command's runtime-bound project selector"
                )));
            }
            object.insert(field.clone(), value.clone());
        }
        (parameters, project_path)
    } else {
        let mut parameters =
            bind_command_input(binding_command, &tail, &overlay, &load_input).map_err(Invalid)?;
        let path = apply_project_policy_with_context(
            command,
            &mut parameters,
            default_project,
            caller_cwd,
        )?;
        (parameters, path)
    };
    // The signed descriptor is mandatory, not merely another argv default.
    controls.pin_project_at_admission |=
        command.project.as_ref().is_some_and(|p| p.pin_at_admission);
    for (requested, flag) in [
        (controls.pin_project_at_admission, "--pin-project"),
        (controls.pin_current_head_at_admission, "--current-head"),
        (controls.retain_child_results, "--retain-child-results"),
    ] {
        if requested && project_path.is_none() {
            return Err(Invalid(format!(
                "{flag} requires a project root; it cannot be combined with --no-project"
            )));
        }
    }
    if controls.pin_project_at_admission && controls.pin_current_head_at_admission {
        return Err(Invalid(
            "capture-live and current-HEAD project sources are mutually exclusive".into(),
        ));
    }
    if (controls.pin_project_at_admission || controls.pin_current_head_at_admission)
        && controls.state_root.is_some()
    {
        let flag = if controls.pin_project_at_admission {
            "--pin-project"
        } else {
            "--current-head"
        };
        return Err(Invalid(format!(
            "{flag} cannot be combined with --state-root; the pinned generation owns runtime state"
        )));
    }
    if let Some(root) = &mut controls.state_root {
        let path = Path::new(root);
        if !path.is_absolute() {
            let base = caller_cwd.ok_or_else(|| {
                Invalid("relative state-root requires explicit caller context".into())
            })?;
            *root = base.join(path).to_string_lossy().into_owned();
        }
    }
    let policy = command_execution_policy(project_path.is_some(), &controls).map_err(Invalid)?;
    // Keep structured payload values intact; effective schema normalization is
    // performed against the selected callee by execution admission.
    Ok(CompiledCommandInvocation {
        item_ref,
        parameters: std::mem::take(&mut parameters),
        project_path,
        controls,
        execution_policy: policy,
        validate_only,
        direct_execute,
    })
}

pub fn command_execution_policy(
    project_backed: bool,
    controls: &CommandInvocationControls,
) -> Result<crate::execution_policy::ExecutionPolicy, String> {
    use crate::execution_policy::{ExecutionPolicy, ExecutionResponse};
    let response = if controls.async_launch {
        ExecutionResponse::Accepted
    } else {
        ExecutionResponse::Wait
    };
    let mut policy = if controls.pin_current_head_at_admission {
        ExecutionPolicy::local_pinned_current_head(response)
    } else if controls.pin_project_at_admission {
        ExecutionPolicy::local_pinned_capture(response)
    } else if project_backed {
        ExecutionPolicy::local_live(response)
    } else {
        ExecutionPolicy::projectless(response)
    };
    if controls.retain_child_results {
        policy = policy.retain_child_results().map_err(|e| e.to_string())?;
    }
    if controls.exclude_operator_vault {
        policy = policy.exclude_operator_vault();
    }
    policy.validate().map_err(|e| e.to_string())?;
    Ok(policy)
}

/// Shared structured input grammar; external input acquisition is injected.
pub fn bind_command_input(
    command: &CommandDef,
    tail: &[String],
    overlay: &Value,
    load_input: &impl Fn(&str) -> Result<Value, String>,
) -> Result<Value, String> {
    let binding = command.parameter_binding.as_ref();
    let mut residual = Vec::new();
    let mut source = None;
    let mut tokens = tail.iter();
    while let Some(token) = tokens.next() {
        let input_flag = binding.and_then(|b| b.input_flag.as_deref());
        let input = input_flag.and_then(|name| token.strip_prefix(&format!("--{name}=")));
        let input = if input_flag.is_some_and(|name| token == &format!("--{name}")) {
            Some(
                tokens
                    .next()
                    .ok_or("--input requires an argument")?
                    .as_str(),
            )
        } else {
            input
        };
        if let Some(input) = input {
            if source.replace(input).is_some() {
                return Err("duplicate --input sources are not allowed".into());
            }
        } else {
            residual.push(token.clone());
        }
    }
    let mut input = source.map(load_input).transpose()?;
    if input.is_none() && binding.is_some_and(|b| b.single_json_object_arg) && residual.len() == 1 {
        if let Ok(value) = serde_json::from_str::<Value>(&residual[0])
            && value.is_object()
        {
            input = Some(value);
            residual.clear();
        }
    }
    if let Some(mut input) = input {
        if command.forms.is_empty() {
            if command.project.is_some() {
                let (rest, selectors) = separate_project_control_flags(&residual)?;
                residual = rest;
                if !selectors.is_empty() {
                    let object = input
                        .as_object_mut()
                        .ok_or("project selectors require an input object")?;
                    for (key, value) in selectors {
                        if object.insert(key.clone(), value).is_some() {
                            return Err(format!("duplicate project selector '{key}'"));
                        }
                    }
                }
            }
            if !residual.is_empty() {
                return Err("--input cannot be combined with undeclared positional arguments or parameter flags".into());
            }
        }
        if !overlay.is_null() && overlay != &serde_json::json!({}) {
            let object = input
                .as_object_mut()
                .ok_or("structured argument overlay requires an object")?;
            for (key, value) in overlay
                .as_object()
                .ok_or("command argument overlay must be an object")?
            {
                if object.insert(key.clone(), value.clone()).is_some() {
                    return Err(format!("duplicate argument '{key}'"));
                }
            }
        }
        if command.forms.is_empty() {
            return Ok(input);
        }
        return ryeos_runtime::arg_binder::bind_argv_with_command_and_overlay(
            &residual,
            Some(command),
            &input,
        );
    }
    ryeos_runtime::arg_binder::bind_argv_with_command_and_overlay(&residual, Some(command), overlay)
}

pub fn strip_declared_control_flags(
    tail: &mut Vec<String>,
    declared: &[CommandControlFlag],
) -> Result<CommandInvocationControls, String> {
    let mut routes: HashMap<&str, &CommandControlFlag> = HashMap::new();
    for flag in declared {
        routes.insert(flag.flag.as_str(), flag);
        for alias in &flag.aliases {
            routes.insert(alias.as_str(), flag);
        }
    }

    let mut controls = CommandInvocationControls::default();
    let mut parameters = Vec::with_capacity(tail.len());
    let mut tokens = std::mem::take(tail).into_iter();
    while let Some(token) = tokens.next() {
        let Some(raw) = token.strip_prefix("--") else {
            parameters.push(token);
            continue;
        };
        let (name, inline) = match raw.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (raw, None),
        };
        let Some(flag) = routes.get(name).copied() else {
            parameters.push(token);
            continue;
        };
        let binding = flag.binding;
        if binding.takes_value() {
            let value = inline
                .or_else(|| tokens.next())
                .ok_or_else(|| format!("flag --{name} requires a value"))?;
            match binding {
                ControlFlagBinding::CallMethod => controls.call_method = Some(value),
                ControlFlagBinding::CallArgs => {
                    controls.call_args = Some(
                        serde_json::from_str(&value)
                            .map_err(|error| format!("--{name} must be a JSON value: {error}"))?,
                    );
                }
                ControlFlagBinding::StateRoot => controls.state_root = Some(value),
                ControlFlagBinding::ProductSelections => {
                    if controls.product_selections.is_some() {
                        return Err(format!("duplicate --{name} flag"));
                    }
                    let maximum = ryeos_state::external_content::products::composition::MAX_PRODUCT_SELECTION_INPUTS_BYTES;
                    if value.len() > maximum {
                        return Err(format!("--{name} JSON exceeds {maximum} bytes"));
                    }
                    let parsed: ryeos_state::external_content::products::composition::ProductSelectionInputs =
                        serde_json::from_str(&value).map_err(|error| {
                            format!("--{name} must be a typed product-selection list: {error}")
                        })?;
                    let canonical = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(parsed)
                        .map_err(|error| format!("--{name}: {error}"))?;
                    controls.product_selections =
                        Some(serde_json::to_value(canonical).map_err(|error| error.to_string())?);
                }
                ControlFlagBinding::RefBinding => {
                    let (binding_name, item_ref) = match flag.ref_binding_name.as_deref() {
                        Some(binding_name) => (binding_name, value.as_str()),
                        None => value.split_once('=').ok_or_else(|| {
                            format!("--{name} requires name=canonical-ref, got '{value}'")
                        })?,
                    };
                    validate_ref_binding_name(binding_name)?;
                    if item_ref.is_empty() || item_ref.len() > 2048 {
                        return Err(format!(
                            "--{name} requires a non-empty canonical ref of at most 2048 bytes"
                        ));
                    }
                    if controls.ref_bindings.contains_key(binding_name) {
                        return Err(format!("duplicate --{name} binding name '{binding_name}'"));
                    }
                    if controls.ref_bindings.len() >= 32 {
                        return Err(format!("--{name} accepts at most 32 bindings"));
                    }
                    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref)
                        .map_err(|error| {
                            format!("invalid --{name} canonical ref for '{binding_name}': {error}")
                        })?;
                    controls
                        .ref_bindings
                        .insert(binding_name.to_owned(), canonical.to_string());
                }
                _ => unreachable!("value-taking binding checked above"),
            }
            continue;
        }

        let enabled = match inline.as_deref() {
            None | Some("true") => true,
            Some("false") => false,
            Some(other) => return Err(format!("invalid value for --{name}: {other}")),
        };
        if !enabled {
            continue;
        }
        match binding {
            ControlFlagBinding::LaunchModeAccepted => controls.async_launch = true,
            ControlFlagBinding::PinProjectAtAdmission => controls.pin_project_at_admission = true,
            ControlFlagBinding::PinCurrentHeadAtAdmission => {
                controls.pin_current_head_at_admission = true
            }
            ControlFlagBinding::RetainChildResults => controls.retain_child_results = true,
            ControlFlagBinding::ExcludeOperatorVault => controls.exclude_operator_vault = true,
            ControlFlagBinding::DebugRaw => controls.debug_raw = true,
            ControlFlagBinding::StreamOn => {
                if controls.stream == Some(false) {
                    return Err("conflicting flags: stream on and off".to_owned());
                }
                controls.stream = Some(true);
            }
            ControlFlagBinding::StreamOff => {
                if controls.stream == Some(true) {
                    return Err("conflicting flags: stream on and off".to_owned());
                }
                controls.stream = Some(false);
            }
            _ => unreachable!("presence binding checked above"),
        }
    }
    *tail = parameters;
    Ok(controls)
}

pub fn strip_project_control_flags(tail: &[String]) -> Vec<String> {
    let mut output = Vec::with_capacity(tail.len());
    let mut index = 0;
    while index < tail.len() {
        let token = &tail[index];
        if token == "--no-project" || token.starts_with("--project=") || token.starts_with("-p=") {
            index += 1;
        } else if token == "--project" || token == "-p" {
            index += if index + 1 < tail.len() { 2 } else { 1 };
        } else {
            output.push(token.clone());
            index += 1;
        }
    }
    output
}

/// Split argv project selectors from an item's data-plane tail. Structured
/// overlays are intentionally not inspected: a field named `project` inside
/// an item's JSON input belongs to that item, not to command routing.
pub fn separate_project_control_flags(
    tail: &[String],
) -> Result<(Vec<String>, serde_json::Map<String, Value>), String> {
    let mut parameters = Vec::with_capacity(tail.len());
    let mut controls = serde_json::Map::new();
    let mut index = 0;
    while index < tail.len() {
        let token = &tail[index];
        if token == "--input" {
            parameters.push(token.clone());
            if let Some(source) = tail.get(index + 1) {
                parameters.push(source.clone());
                index += 1;
            }
            index += 1;
        } else if token == "--no-project" || token.starts_with("--no-project=") {
            let value = match token.strip_prefix("--no-project=") {
                None | Some("true") => Value::Bool(true),
                Some("false") => Value::Bool(false),
                Some(value) => Value::String(value.to_owned()),
            };
            insert_project_control(&mut controls, "no_project", value)?;
            index += 1;
        } else if let Some(path) = token
            .strip_prefix("--project=")
            .or_else(|| token.strip_prefix("-p="))
        {
            insert_project_control(&mut controls, "project", Value::String(path.to_owned()))?;
            index += 1;
        } else if token == "--project" || token == "-p" {
            let path = tail
                .get(index + 1)
                .filter(|path| !path.starts_with('-'))
                .ok_or_else(|| format!("{token} requires a value (path to the project root)"))?;
            insert_project_control(&mut controls, "project", Value::String(path.clone()))?;
            index += 2;
        } else {
            parameters.push(token.clone());
            index += 1;
        }
    }
    Ok((parameters, controls))
}

fn insert_project_control(
    controls: &mut serde_json::Map<String, Value>,
    field: &str,
    value: Value,
) -> Result<(), String> {
    if controls.insert(field.to_owned(), value).is_some() {
        return Err(format!("duplicate --{} selector", field.replace('_', "-")));
    }
    Ok(())
}

/// Apply a descriptor's source-project selector policy to already-bound
/// parameters. The selected source path is returned only when the descriptor
/// asks for an outer execution project; selector-to-parameter bindings are
/// written solely where the signed descriptor declares them.
pub fn apply_project_policy(
    command: &CommandDef,
    parameters: &mut Value,
    default_project: Option<&Path>,
    caller_cwd: &Path,
) -> Result<Option<PathBuf>, CommandProjectPolicyError> {
    apply_project_policy_with_context(command, parameters, default_project, Some(caller_cwd))
}

/// Absence of caller context is not permission to discover a daemon project.
pub fn apply_project_policy_with_context(
    command: &CommandDef,
    parameters: &mut Value,
    default_project: Option<&Path>,
    caller_cwd: Option<&Path>,
) -> Result<Option<PathBuf>, CommandProjectPolicyError> {
    let no_project = match parameters.get("no_project") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return Err(CommandProjectPolicyError::Invalid(
                "--no-project must be a boolean".to_owned(),
            ));
        }
    };
    let Some(project) = command.project.as_ref() else {
        if let Some(object) = parameters.as_object_mut() {
            object.remove("no_project");
        }
        return Ok(None);
    };
    let object = parameters.as_object_mut().ok_or_else(|| {
        CommandProjectPolicyError::Invalid("command parameters must be a JSON object".to_owned())
    })?;
    if let Some(binding) = project.bind_parameter.as_ref()
        && binding != "project"
        && object.contains_key(binding)
    {
        return Err(CommandProjectPolicyError::Invalid(format!(
            "--{} is runtime-bound from the command's project selector; use --project <path> instead",
            binding.replace('_', "-")
        )));
    }
    if let Some(binding) = project.bind_no_project_parameter.as_ref()
        && binding != "no_project"
        && object.contains_key(binding)
    {
        return Err(CommandProjectPolicyError::Invalid(format!(
            "--{} is runtime-bound from the projectless selector; use --no-project instead",
            binding.replace('_', "-")
        )));
    }
    if no_project && !project.no_project_flag {
        return Err(CommandProjectPolicyError::ProjectRequired(format!(
            "command '{}' does not accept --no-project",
            command.name
        )));
    }
    let mut selected = match object.get("project") {
        None => None,
        Some(Value::String(path)) if !path.is_empty() => Some(PathBuf::from(path)),
        Some(Value::String(_)) => {
            return Err(CommandProjectPolicyError::Invalid(
                "--project must be a non-empty path string".into(),
            ));
        }
        Some(_) => {
            return Err(CommandProjectPolicyError::Invalid(
                "--project must be a path string".into(),
            ));
        }
    };
    if no_project && selected.is_some() {
        return Err(CommandProjectPolicyError::Invalid(
            "cannot pass both --no-project and --project: choose one".into(),
        ));
    }
    if selected.is_none() && !no_project {
        selected = default_project.map(PathBuf::from);
    }
    if selected.is_none()
        && !no_project
        && project.default == CommandProjectDefault::DiscoverUpwardAi
        && let Some(caller_cwd) = caller_cwd
    {
        selected =
            discover_upward_ai_project(caller_cwd).map_err(CommandProjectPolicyError::Invalid)?;
    }
    if let Some(path) = selected.take() {
        let base = caller_cwd
            .or_else(|| path.is_absolute().then_some(path.as_path()))
            .ok_or_else(|| {
                CommandProjectPolicyError::Invalid(
                    "relative project selector requires explicit caller context".into(),
                )
            })?;
        selected = Some(
            canonicalize_project_path(&path, base).map_err(CommandProjectPolicyError::Invalid)?,
        );
    }
    if project.resolution == CommandProjectResolution::Required && selected.is_none() && !no_project
    {
        return Err(CommandProjectPolicyError::ProjectRequired(format!(
            "command '{}' requires a project",
            command.name
        )));
    }
    object.remove("no_project");
    object.remove("project");
    if let (Some(binding), Some(path)) = (&project.bind_parameter, &selected) {
        object.insert(
            binding.clone(),
            Value::String(path.to_string_lossy().into_owned()),
        );
    }
    if no_project && let Some(binding) = &project.bind_no_project_parameter {
        object.insert(binding.clone(), Value::Bool(true));
    }
    Ok(project.request_project_path.then_some(selected).flatten())
}

fn validate_ref_binding_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name.split('_').enumerate().all(|(index, segment)| {
            !segment.is_empty()
                && segment.chars().enumerate().all(|(char_index, ch)| {
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() && (index > 0 || char_index > 0)
                })
                && (index > 0
                    || segment
                        .chars()
                        .next()
                        .is_some_and(|ch| ch.is_ascii_lowercase()))
        });
    valid.then_some(()).ok_or_else(|| {
        format!(
            "invalid ref binding name '{name}'; expected [a-z][a-z0-9]*(?:_[a-z0-9]+)* (max 64 bytes)"
        )
    })
}

fn canonicalize_project_path(path: &Path, caller_cwd: &Path) -> Result<PathBuf, String> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        caller_cwd.join(path)
    };
    absolute.canonicalize().map_err(|error| {
        format!(
            "cannot canonicalize project path '{}': {error}. Ensure the path exists and is accessible.",
            absolute.display()
        )
    })
}

fn discover_upward_ai_project(caller_cwd: &Path) -> Result<Option<PathBuf>, String> {
    for ancestor in caller_cwd.ancestors() {
        if ancestor.join(ryeos_engine::AI_DIR).is_dir() {
            return ancestor.canonicalize().map(Some).map_err(|error| {
                format!(
                    "cannot canonicalize project path '{}': {error}",
                    ancestor.display()
                )
            });
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_runtime::{
        CommandControlFlag, CommandDispatch, CommandProjectPolicy, ControlFlagBinding,
    };

    fn command() -> CommandDef {
        CommandDef {
            name: "test".into(),
            tokens: vec!["test".into()],
            description: "test".into(),
            aliases: vec![],
            help: None,
            arguments: vec![],
            forms: vec![],
            sensitive_fields: vec![],
            defaults: BTreeMap::new(),
            parameter_binding: None,
            control_flags: vec![],
            project: Some(CommandProjectPolicy {
                resolution: CommandProjectResolution::Optional,
                default: CommandProjectDefault::None,
                no_project_flag: true,
                request_project_path: false,
                pin_at_admission: false,
                bind_parameter: Some("source_project".into()),
                bind_no_project_parameter: None,
            }),
            dispatch: CommandDispatch::ExecuteRef {
                execute: "service:test".into(),
                availability: Default::default(),
            },
            source_file: PathBuf::new(),
            provenance: Default::default(),
        }
    }

    #[test]
    fn project_selector_is_bound_only_to_the_declared_parameter() {
        let directory = tempfile::tempdir().unwrap();
        let mut parameters = serde_json::json!({"project": directory.path()});
        let outer =
            apply_project_policy(&command(), &mut parameters, None, directory.path()).unwrap();
        assert!(outer.is_none());
        assert_eq!(
            parameters["source_project"],
            directory
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert!(parameters.get("project").is_none());
    }

    #[test]
    fn projectless_selector_never_leaks_into_closed_parameters() {
        let directory = tempfile::tempdir().unwrap();
        let mut parameters = serde_json::json!({"no_project": true});
        assert!(
            apply_project_policy(&command(), &mut parameters, None, directory.path())
                .unwrap()
                .is_none()
        );
        assert_eq!(parameters, serde_json::json!({}));
    }

    #[test]
    fn declared_controls_are_removed_from_the_parameter_tail() {
        let declared = vec![CommandControlFlag {
            flag: "async".into(),
            help: "test".into(),
            binding: ControlFlagBinding::LaunchModeAccepted,
            ref_binding_name: None,
            aliases: vec![],
        }];
        let mut tail = vec!["--async".into(), "--value".into(), "x".into()];
        let controls = strip_declared_control_flags(&mut tail, &declared).unwrap();
        assert!(controls.async_launch);
        assert_eq!(tail, vec!["--value", "x"]);
    }

    #[test]
    fn direct_item_project_fields_remain_data_plane() {
        let tail = vec!["--project".into(), "/source".into(), "--x".into()];
        let (parameters, selectors) = separate_project_control_flags(&tail).unwrap();
        assert_eq!(parameters, vec!["--x"]);
        assert_eq!(selectors["project"], "/source");

        let mut projectless_command = command();
        projectless_command.project = None;
        let mut item_parameters = serde_json::json!({"project": "item-owned"});
        apply_project_policy(
            &projectless_command,
            &mut item_parameters,
            None,
            std::path::Path::new("/"),
        )
        .unwrap();
        assert_eq!(item_parameters["project"], "item-owned");
    }

    #[test]
    fn compiler_preserves_direct_item_selectors_and_nested_policy() {
        let mut command = command();
        command.dispatch = CommandDispatch::DirectExecuteItemRef {
            item_ref_arg: "item_ref".into(),
            validate_only: false,
            availability: Default::default(),
        };
        command.project.as_mut().unwrap().bind_parameter = None;
        command.project.as_mut().unwrap().request_project_path = true;
        let input = serde_json::json!({"project":"item-owned", "no_project":false,
            "execution_policy":{"project":{"kind":"projectless"}}});
        let compiled = compile_command_invocation(
            &command,
            &["service:test".into(), "--no-project".into()],
            &input,
            None,
            None,
            |_| panic!("no file input"),
        )
        .unwrap();
        assert_eq!(compiled.parameters, input);
        assert!(compiled.project_path.is_none());
    }

    #[test]
    fn absent_caller_context_never_discovers_and_relative_paths_fail() {
        let mut command = command();
        command.project.as_mut().unwrap().default = CommandProjectDefault::DiscoverUpwardAi;
        let mut empty = serde_json::json!({});
        assert!(
            apply_project_policy_with_context(&command, &mut empty, None, None)
                .unwrap()
                .is_none()
        );
        assert!(
            apply_project_policy_with_context(
                &command,
                &mut serde_json::json!({"project":"relative"}),
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn typed_encoding_is_the_closed_default() {
        assert_eq!(ParameterEncoding::default(), ParameterEncoding::Typed);
        assert!(serde_json::from_value::<ParameterEncoding>(serde_json::json!("guess")).is_err());
    }

    #[test]
    fn typed_parameters_are_not_reinterpreted_or_resolved() {
        let engine = ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            Vec::new(),
        );
        let mut parameters = serde_json::json!({"limit":"007", "enabled":"false"});
        let before = parameters.clone();
        normalize_selected_parameters(
            ParameterEncoding::Typed,
            &mut parameters,
            &engine,
            "not a ref",
            None,
            ryeos_engine::contracts::SubjectResolutionAuthority::Projectless,
        )
        .unwrap();
        assert_eq!(parameters, before);
    }
}
