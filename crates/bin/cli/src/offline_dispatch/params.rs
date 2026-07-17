//! Offline command argument binding, project normalization, and template expansion.

use std::collections::HashMap;
use std::path::Path;

use ryeos_runtime::{CommandDef, CommandProjectResolution};
use serde_json::Value;

use crate::error::CliError;

pub(super) fn bind_params_minimal(
    tail: &[String],
    command: &CommandDef,
    project_path: &str,
) -> Result<Value, CliError> {
    // Shared with daemon dispatch so the two paths cannot drift:
    // `--input` is only honored when the descriptor declares an input
    // flag (clean cut from the old always-on offline behavior), and
    // declared single-JSON-object binding now works offline too.
    if let Some(input) = crate::arg_bind::bind_declared_shortcuts(tail, command)? {
        return Ok(input);
    }

    let mut params = ryeos_runtime::arg_binder::bind_argv_with_command(tail, Some(command))
        .map_err(|e| CliError::Local { detail: e })?;

    // Project resolution
    let resolution = command_project_resolution(command);
    if resolution != CommandProjectResolution::None {
        let mut canonical_tail = params_to_tail(&params);
        let default_project = (project_path != ".").then(|| Path::new(project_path));
        match resolution {
            CommandProjectResolution::None => {}
            CommandProjectResolution::Optional => {
                canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                    &canonical_tail,
                    default_project,
                )?;
            }
            CommandProjectResolution::Required => {
                if canonical_tail.iter().any(|t| t == "--no-project") {
                    return Err(CliError::Local {
                        detail: "this command requires a project; do not pass --no-project".into(),
                    });
                }
                canonical_tail = crate::project_resolve::rewrite_project_tail_with_default(
                    &canonical_tail,
                    default_project,
                )?;
                if canonical_tail.iter().any(|t| t == "--no-project") {
                    return Err(CliError::Local {
                        detail: format!(
                            "this command requires a project; run it from a directory containing \
                             {} or pass --project <path>",
                            ryeos_engine::AI_DIR
                        ),
                    });
                }
            }
        }
        params = ryeos_runtime::bind_argv(&canonical_tail);
    }

    Ok(params)
}

pub(super) fn bind_params_with_schema(
    tail: &[String],
    command: &CommandDef,
    service_schema: &HashMap<String, String>,
    project_path: &str,
) -> Result<Value, CliError> {
    let mut params = bind_params_minimal(tail, command, project_path)?;

    // Normalize project → project_path
    params = normalize_project_param(params, service_schema, project_path);

    // Reject unknown flags
    if let Some(obj) = params.as_object() {
        for key in obj.keys() {
            if key.starts_with('_') {
                continue;
            }
            let normalized_key = key.replace('_', "-");
            if !service_schema.contains_key(key.as_str())
                && !service_schema.contains_key(&normalized_key)
                && key != "input"
            {
                return Err(CliError::Local {
                    detail: format!(
                        "unknown parameter --{normalized_key} for this command{}",
                        if service_schema.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " (expected: {})",
                                service_schema
                                    .keys()
                                    .map(|k| format!("--{}", k.replace('_', "-")))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        }
                    ),
                });
            }
        }
    }

    Ok(params)
}

fn normalize_project_param(
    mut params: Value,
    service_schema: &HashMap<String, String>,
    default_project_path: &str,
) -> Value {
    let Some(obj) = params.as_object_mut() else {
        return params;
    };

    if service_schema.contains_key("project_path") && !service_schema.contains_key("project") {
        if let Some(project) = obj.remove("project") {
            obj.entry("project_path".to_string()).or_insert(project);
        }
    }

    if !obj.contains_key("project")
        && !obj.contains_key("project_path")
        && !obj.contains_key("no_project")
    {
        if service_schema.contains_key("project_path") {
            obj.insert(
                "project_path".to_string(),
                Value::String(default_project_path.to_string()),
            );
        } else if service_schema.contains_key("project") {
            obj.insert(
                "project".to_string(),
                Value::String(default_project_path.to_string()),
            );
        }
    }

    params
}

fn command_project_resolution(command: &CommandDef) -> CommandProjectResolution {
    command
        .project
        .as_ref()
        .map(|p| p.resolution)
        .unwrap_or_default()
}

pub(super) fn expand_template(
    template: &str,
    params_json: &str,
    project_path: &str,
) -> Result<String, CliError> {
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        if let Some(end) = rest[start + 1..].find('}') {
            let token = &rest[start + 1..start + 1 + end];
            if token != "params_json" && token != "project_path" && token != "triple" {
                return Err(CliError::Local {
                    detail: format!(
                        "offline tool template references unsupported token {{{token}}}"
                    ),
                });
            }
            rest = &rest[start + 1 + end + 1..];
        } else {
            return Err(CliError::Local {
                detail: "offline tool template contains an unterminated token".into(),
            });
        }
    }

    let mut out = template.replace("{params_json}", params_json);
    out = out.replace("{project_path}", project_path);
    Ok(out)
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
