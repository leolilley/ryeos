//! Offline command argument binding, project normalization, and template expansion.

use std::collections::HashMap;
use std::path::Path;

use ryeos_runtime::{CommandDef, InvocationInputContract};
use serde_json::Value;

use crate::error::CliError;

pub(super) fn bind_params_minimal(
    tail: &[String],
    command: &CommandDef,
    project_path: &str,
) -> Result<Value, CliError> {
    crate::project_resolve::reject_bound_project_parameter_flag(
        tail,
        command
            .project
            .as_ref()
            .and_then(|project| project.bind_parameter.as_deref()),
    )?;
    // Declared structured-input shortcuts have the same owner as live
    // dispatch; they still undergo project-policy binding below.
    let mut params = match crate::arg_bind::bind_declared_shortcuts(tail, command)? {
        Some(input) => input,
        None => ryeos_runtime::arg_binder::bind_argv_with_command(tail, Some(command))
            .map_err(|detail| CliError::Local { detail })?,
    };

    // Keep descriptor defaults and structured input as JSON. Rebinding all
    // fields through argv here loses numbers, false/null values, and arrays.
    // Project policy and injection checks have the same owner as live CLI
    // dispatch; only project-control fields may change at this boundary.
    crate::dispatcher::apply_project_policy(
        command,
        &mut params,
        (project_path != ".").then(|| Path::new(project_path)),
    )?;

    Ok(params)
}

pub(super) fn bind_params_with_schema(
    tail: &[String],
    command: &CommandDef,
    service_schema: &HashMap<String, String>,
    project_path: &str,
) -> Result<Value, CliError> {
    let contract =
        InvocationInputContract::from_lightweight_schema_value(&serde_json::json!(service_schema))
            .map_err(|detail| CliError::Local { detail })?;
    let mut params = bind_params_minimal(tail, command, project_path)?;

    // An explicit project policy already bound (or deliberately omitted) its
    // field. Schema-only normalization must not undo --no-project or select a
    // different path after that policy has run.
    if command.project.is_none() {
        params = normalize_project_param(params, service_schema, project_path);
    }

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

    // Live dispatch uses this same descriptor-driven normalizer. Standalone
    // service transport must carry typed JSON too, not raw CLI scalar strings.
    ryeos_runtime::arg_binder::normalize_params_with_contract(params, contract.as_ref())
        .map_err(|detail| CliError::Local { detail })
}

fn normalize_project_param(
    mut params: Value,
    service_schema: &HashMap<String, String>,
    default_project_path: &str,
) -> Value {
    let Some(obj) = params.as_object_mut() else {
        return params;
    };

    if service_schema.contains_key("project_path")
        && !service_schema.contains_key("project")
        && let Some(project) = obj.remove("project")
    {
        obj.entry("project_path".to_string()).or_insert(project);
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

pub(super) fn expand_template(
    template: &str,
    params_json: &str,
    project_path: &str,
) -> Result<String, CliError> {
    let compilation_limits = ryeos_runtime::CompilationLimits::default();
    let compiled = ryeos_runtime::compile_template_for(
        template,
        "offline subprocess template",
        &compilation_limits,
    )
    .map_err(|error| CliError::Local {
        detail: format!("invalid rye-expr/1 offline subprocess template: {error}"),
    })?;
    ryeos_runtime::reject_removed_single_brace_interpolation(
        &compiled,
        ["params_json", "project_path"],
    )
    .map_err(|error| CliError::Local {
        detail: format!("invalid rye-expr/1 offline subprocess template: {error}"),
    })?;
    let context = serde_json::json!({
        "params_json": params_json,
        "project_path": project_path,
    });
    let evaluation_limits = ryeos_runtime::EvaluationLimits::default();
    let rendered = ryeos_runtime::render_template(&compiled, &context, &evaluation_limits)
        .map_err(|error| CliError::Local {
            detail: format!("render rye-expr/1 offline subprocess template: {error}"),
        })?;
    rendered
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| CliError::Local {
            detail: "rye-expr/1 offline subprocess template must produce string".into(),
        })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ryeos_runtime::{
        CommandAvailability, CommandDef, CommandDispatch, CommandProjectDefault,
        CommandProjectPolicy, CommandProjectResolution, CommandProvenance,
    };
    use serde_json::json;

    use super::{bind_params_minimal, bind_params_with_schema};

    fn project_command(bind_parameter: &str) -> CommandDef {
        CommandDef {
            name: "status".into(),
            tokens: vec!["status".into()],
            description: String::new(),
            aliases: Vec::new(),
            help: None,
            arguments: Vec::new(),
            forms: Vec::new(),
            sensitive_fields: Vec::new(),
            defaults: Default::default(),
            parameter_binding: None,
            control_flags: Vec::new(),
            project: Some(CommandProjectPolicy {
                resolution: CommandProjectResolution::Required,
                default: CommandProjectDefault::None,
                no_project_flag: false,
                request_project_path: true,
                pin_at_admission: false,
                bind_parameter: Some(bind_parameter.into()),
                bind_no_project_parameter: None,
            }),
            dispatch: CommandDispatch::ExecuteRef {
                execute: "example:namespace/status".into(),
                availability: CommandAvailability::Local,
            },
            source_file: PathBuf::new(),
            provenance: CommandProvenance::default(),
        }
    }

    #[test]
    fn offline_binding_honors_declared_project_parameter() {
        let project_path = std::env::current_dir()
            .expect("current directory")
            .canonicalize()
            .expect("canonical current directory");
        let params = bind_params_minimal(
            &[],
            &project_command("project_path"),
            project_path.to_str().expect("UTF-8 project path"),
        )
        .expect("bind offline parameters");

        assert_eq!(
            params,
            json!({"project_path": project_path.to_string_lossy()})
        );
        assert!(params.get("project").is_none());
    }

    #[test]
    fn projectless_projection_is_declared_and_shared_with_live_binding() {
        let root = ryeos_engine::test_support::workspace_root();
        for (file, expected, positionals) in [
            ("remote-execute", true, vec!["test", "tool:test/read"]),
            ("remote-status", true, vec!["test"]),
            ("remote-list", true, vec![]),
            ("remote-doctor", false, vec!["test"]),
            ("fetch", false, vec!["tool:test/read"]),
            (
                "external-content-bind",
                false,
                vec![
                    "stage",
                    "request",
                    "manifest",
                    "worker:test/run",
                    "installed_bundle",
                ],
            ),
        ] {
            let command: CommandDef = serde_yaml::from_str(
                &std::fs::read_to_string(
                    root.join(format!("bundles/core/.ai/node/commands/{file}.yaml")),
                )
                .unwrap(),
            )
            .unwrap();
            let args: Vec<String> = positionals
                .into_iter()
                .chain(["--no-project"])
                .map(str::to_owned)
                .collect();
            let offline = bind_params_minimal(&args, &command, ".").unwrap();
            let mut live = json!({"no_project":true});
            crate::dispatcher::apply_project_policy(&command, &mut live, None).unwrap();
            assert_eq!(offline.get("no_project"), live.get("no_project"), "{file}");
            assert_eq!(
                live.get("no_project"),
                expected.then_some(&json!(true)),
                "{file}"
            );
            assert!(live.get("project").is_none());
        }
        let mut command = project_command("project_path");
        let policy = command.project.as_mut().unwrap();
        policy.no_project_flag = true;
        policy.bind_no_project_parameter = Some("without_context".to_owned());
        let mut supplied = json!({"no_project":true,"without_context":false});
        assert!(crate::dispatcher::apply_project_policy(&command, &mut supplied, None).is_err());
        assert_eq!(supplied["without_context"], false);
        let mut explicit = json!({"no_project":true});
        crate::dispatcher::apply_project_policy(&command, &mut explicit, None).unwrap();
        assert_eq!(explicit, json!({"without_context":true}));
        let mut positive = json!({});
        crate::dispatcher::apply_project_policy(&command, &mut positive, Some(&root)).unwrap();
        assert_eq!(positive["project_path"], root.to_string_lossy().as_ref());
        assert!(positive.get("without_context").is_none());
    }

    #[test]
    fn offline_binding_honors_project_named_parameter() {
        let project_path = std::env::current_dir()
            .expect("current directory")
            .canonicalize()
            .expect("canonical current directory");
        let params = bind_params_minimal(
            &[],
            &project_command("project"),
            project_path.to_str().expect("UTF-8 project path"),
        )
        .expect("bind offline parameters");

        assert_eq!(params, json!({"project": project_path.to_string_lossy()}));
    }

    #[test]
    fn offline_binding_refuses_injected_runtime_project_parameter() {
        let project_path = std::env::current_dir()
            .expect("current directory")
            .canonicalize()
            .expect("canonical current directory");
        let error = bind_params_minimal(
            &[
                "--project-path".to_string(),
                "/tmp/other-project".to_string(),
            ],
            &project_command("project_path"),
            project_path.to_str().expect("UTF-8 project path"),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("--project-path is a runtime-bound service field")
        );
    }

    #[test]
    fn offline_project_binding_preserves_all_typed_defaults() {
        let project = tempfile::tempdir().unwrap();
        let mut command = project_command("project_path");
        command.defaults = serde_json::from_value(json!({
            "limit": 37,
            "enabled": false,
            "nullable": null,
            "items": [1, false, {"nested": [null]}],
            "empty_items": [],
            "settings": {"ratio": 0.5},
            "numeric_name": "5000"
        }))
        .unwrap();

        let params = bind_params_minimal(&[], &command, project.path().to_str().unwrap()).unwrap();
        for (field, expected) in &command.defaults {
            assert_eq!(&params[field], expected, "typed default {field}");
        }
        assert_eq!(
            params["project_path"],
            project
                .path()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn offline_schema_binding_uses_shared_typed_normalization() {
        let project = tempfile::tempdir().unwrap();
        let command = project_command("project_path");
        let schema = serde_json::from_value(json!({
            "project_path": "string",
            "limit": "integer",
            "enabled": "boolean?",
            "settings": "object?",
            "label": "string?"
        }))
        .unwrap();
        let argv = [
            "--limit=37",
            "--enabled=false",
            "--settings",
            r#"{"nested":[1,false]}"#,
            "--label=5000",
        ]
        .map(str::to_owned);
        let offline =
            bind_params_with_schema(&argv, &command, &schema, project.path().to_str().unwrap())
                .unwrap();
        assert_eq!(offline["limit"], 37);
        assert_eq!(offline["enabled"], false);
        assert_eq!(offline["settings"], json!({"nested": [1, false]}));
        assert_eq!(offline["label"], "5000");

        // Compare the actual live CLI owners, not a second test-only decoder.
        let mut live =
            ryeos_runtime::arg_binder::bind_argv_with_command(&argv, Some(&command)).unwrap();
        crate::dispatcher::apply_project_policy(&command, &mut live, Some(project.path())).unwrap();
        let contract =
            ryeos_runtime::InvocationInputContract::from_lightweight_schema_value(&json!(schema))
                .unwrap();
        let live =
            ryeos_runtime::arg_binder::normalize_params_with_contract(live, contract.as_ref())
                .unwrap();
        assert_eq!(offline, live);

        let error = bind_params_with_schema(
            &["--limit=not-a-number".into()],
            &command,
            &schema,
            project.path().to_str().unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("--limit must be an integer"));
    }

    #[test]
    fn offline_snapshot_status_keeps_authored_budget_typed_before_standalone_transport() {
        let command: CommandDef = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/node/commands/snapshot-status.yaml"
        )))
        .unwrap();
        let service: serde_json::Value = serde_yaml::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../bundles/core/.ai/services/project/snapshot-status.yaml"
        )))
        .unwrap();
        let schema = serde_json::from_value(service["schema"].clone()).unwrap();
        let project = tempfile::tempdir().unwrap();
        for (argv, expected_budget, expected_include) in [
            (vec![], command.defaults["time_budget_ms"].clone(), None),
            (
                vec![
                    "--time-budget-ms=0".to_owned(),
                    "--include-unchanged=false".to_owned(),
                ],
                json!(0),
                Some(json!(false)),
            ),
        ] {
            let params =
                bind_params_with_schema(&argv, &command, &schema, project.path().to_str().unwrap())
                    .unwrap();
            // This is the JSON wire consumed by ryeosd run-service, not argv.
            let wire: serde_json::Value =
                serde_json::from_slice(&serde_json::to_vec(&params).unwrap()).unwrap();
            assert_eq!(wire["time_budget_ms"], expected_budget);
            assert!(wire["time_budget_ms"].as_u64().is_some());
            assert_eq!(wire.get("include_unchanged"), expected_include.as_ref());
            assert_eq!(
                wire["project_path"],
                project
                    .path()
                    .canonicalize()
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            );
        }
    }

    #[test]
    fn offline_explicit_no_project_is_not_replaced_by_schema_default() {
        let project = tempfile::tempdir().unwrap();
        let mut command = project_command("project_path");
        let policy = command.project.as_mut().unwrap();
        policy.resolution = CommandProjectResolution::Optional;
        policy.no_project_flag = true;
        let schema = serde_json::from_value(json!({"project_path": "string?"})).unwrap();
        let params = bind_params_with_schema(
            &["--no-project".into()],
            &command,
            &schema,
            project.path().to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(params, json!({}));
    }

    #[test]
    fn offline_structured_input_still_obeys_project_policy() {
        let project = tempfile::tempdir().unwrap();
        let mut command = project_command("project_path");
        command.parameter_binding = Some(ryeos_runtime::CommandParameterBinding {
            mode: ryeos_runtime::CommandParameterBindingMode::SchemaObject,
            input_flag: None,
            single_json_object_arg: true,
            flag_key_normalization: Default::default(),
        });
        let params = bind_params_minimal(
            &[r#"{"limit":37,"enabled":false,"items":[]}"#.into()],
            &command,
            project.path().to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(params["limit"], 37);
        assert_eq!(params["enabled"], false);
        assert_eq!(params["items"], json!([]));
        assert!(params["project_path"].as_str().is_some());

        let error = bind_params_minimal(
            &[r#"{"project_path":"/unselected"}"#.into()],
            &command,
            project.path().to_str().unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("runtime-bound"));
    }

    #[test]
    fn offline_and_live_project_controls_refuse_malformed_values_before_defaults() {
        let project = tempfile::tempdir().unwrap();
        let mut command = project_command("project_path");
        let policy = command.project.as_mut().unwrap();
        policy.resolution = CommandProjectResolution::Optional;
        policy.no_project_flag = true;
        command.parameter_binding = Some(ryeos_runtime::CommandParameterBinding {
            mode: ryeos_runtime::CommandParameterBindingMode::SchemaObject,
            input_flag: None,
            single_json_object_arg: true,
            flag_key_normalization: Default::default(),
        });
        let invalid = [
            (json!({"project": null}), "--project must be a path string"),
            (json!({"project": true}), "--project must be a path string"),
            (json!({"project": 37}), "--project must be a path string"),
            (json!({"project": []}), "--project must be a path string"),
            (json!({"project": {}}), "--project must be a path string"),
            (
                json!({"project": ""}),
                "--project must be a non-empty path string",
            ),
            (
                json!({"no_project": null}),
                "--no-project must be a boolean",
            ),
            (
                json!({"no_project": "true"}),
                "--no-project must be a boolean",
            ),
            (
                json!({"no_project": "false"}),
                "--no-project must be a boolean",
            ),
            (json!({"no_project": 37}), "--no-project must be a boolean"),
            (json!({"no_project": []}), "--no-project must be a boolean"),
            (json!({"no_project": {}}), "--no-project must be a boolean"),
            (
                json!({"project": "/unselected", "no_project": true}),
                "cannot pass both --no-project and --project",
            ),
        ];
        for (input, expected) in invalid {
            let offline = bind_params_minimal(
                &[input.to_string()],
                &command,
                project.path().to_str().unwrap(),
            )
            .unwrap_err();
            let mut live_input = input.clone();
            let live = crate::dispatcher::apply_project_policy(
                &command,
                &mut live_input,
                Some(project.path()),
            )
            .unwrap_err();
            assert!(
                offline.to_string().contains(expected),
                "input {input}: {offline}"
            );
            assert_eq!(offline.to_string(), live.to_string(), "input {input}");
            assert_eq!(
                live_input, input,
                "refusal must not erase supplied controls"
            );
        }
    }

    #[test]
    fn shared_project_controls_preserve_declared_selection_policy() {
        let project = tempfile::tempdir().unwrap();
        let canonical_project = project.path().canonicalize().unwrap();
        let mut command = project_command("project_path");
        for resolution in [
            CommandProjectResolution::Required,
            CommandProjectResolution::Optional,
        ] {
            command.project.as_mut().unwrap().resolution = resolution;
            for input in [json!({}), json!({"no_project": false})] {
                let mut parameters = input;
                let path = crate::dispatcher::apply_project_policy(
                    &command,
                    &mut parameters,
                    Some(project.path()),
                )
                .unwrap();
                assert_eq!(path.as_deref(), Some(canonical_project.as_path()));
                assert_eq!(parameters, json!({"project_path": canonical_project}));
            }
            let mut denied = json!({"no_project": true});
            let error = crate::dispatcher::apply_project_policy(
                &command,
                &mut denied,
                Some(project.path()),
            )
            .unwrap_err();
            assert!(error.to_string().contains("does not accept --no-project"));
            assert_eq!(denied, json!({"no_project": true}));
        }

        let policy = command.project.as_mut().unwrap();
        policy.resolution = CommandProjectResolution::Optional;
        policy.no_project_flag = true;
        let mut parameters = json!({"no_project": true});
        assert!(
            crate::dispatcher::apply_project_policy(
                &command,
                &mut parameters,
                Some(project.path()),
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(parameters, json!({}));

        let explicit = tempfile::tempdir().unwrap();
        let canonical_explicit = explicit.path().canonicalize().unwrap();
        let mut parameters = json!({"no_project": false, "project": explicit.path()});
        assert_eq!(
            crate::dispatcher::apply_project_policy(
                &command,
                &mut parameters,
                Some(project.path()),
            )
            .unwrap()
            .as_deref(),
            Some(canonical_explicit.as_path())
        );
        assert_eq!(parameters, json!({"project_path": canonical_explicit}));
    }
}
