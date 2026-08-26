use std::collections::BTreeMap;

use ryeos_handler_bins::run_handler;
use ryeos_handler_protocol::{
    ExternalEffectAuthorityDeclWire, ExternalEffectAuthorityResultWire, FinancialAuthorityDeclWire,
    FinancialAuthorityResultWire, HandlerRequest, HandlerResponse, ItemSpaceWire,
    LaunchExecutionDependencyRequestWire, LaunchPrepareError, LaunchPrepareErrorClass,
    LaunchPrepareResponse, LaunchPrepareSuccess, LaunchPreparedItemWire, TrustClassWire,
    ValidateLaunchPreparerConfigRequest, ValidateLaunchPreparerConfigResponse,
    ValidateLaunchPreparerConfigSuccess,
};

const DEPENDENCY_NAME: &str = "session_worker";
const ENVIRONMENT_BINDING: &str = "environment";

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerSelection {
    Direct(String),
    Environment(String),
}

fn main() {
    std::process::exit(run_handler(|request| match request {
        HandlerRequest::LaunchPrepare(request) => HandlerResponse::LaunchPrepare {
            response: prepare(request),
        },
        HandlerRequest::ValidateLaunchPreparerConfig(request) => {
            HandlerResponse::ValidateLaunchPreparerConfig {
                response: validate(request),
            }
        }
        _ => HandlerResponse::LaunchPrepare {
            response: LaunchPrepareResponse::Error {
                error: wire_error(
                    "worker_execution_launch_protocol_mismatch",
                    "worker execution launch preparer received an unrelated request",
                ),
            },
        },
    }));
}

fn prepare(request: ryeos_handler_protocol::LaunchPrepareRequest) -> LaunchPrepareResponse {
    let result = (|| {
        if request.handler_config != serde_json::json!({}) {
            return Err(wire_error(
                "worker_execution_handler_config_invalid",
                "worker execution launch handler config must be empty",
            ));
        }
        if !request.config_inputs.is_empty()
            || request
                .ref_bindings
                .keys()
                .any(|name| name != ENVIRONMENT_BINDING)
        {
            return Err(wire_error(
                "worker_execution_launch_inputs_invalid",
                "worker execution launch accepts only its declared environment ref binding and no config inputs",
            ));
        }
        if !request
            .primary
            .canonical_ref
            .starts_with("worker_execution:")
        {
            return Err(wire_error(
                "worker_execution_primary_invalid",
                "worker execution launch requires a worker_execution primary item",
            ));
        }
        let config = request
            .primary
            .composed
            .composed
            .get("config")
            .cloned()
            .ok_or_else(|| {
                wire_error(
                    "worker_execution_config_missing",
                    "worker execution has no composed config",
                )
            })?;
        let selection = validate_execution_config(&config)?;
        let worker_ref = match selection {
            WorkerSelection::Direct(worker_ref) => {
                if !request.ref_bindings.is_empty() {
                    return Err(wire_error(
                        "worker_execution_environment_unexpected",
                        "direct worker execution cannot carry an environment binding",
                    ));
                }
                worker_ref
            }
            WorkerSelection::Environment(binding_name) => {
                if request.ref_bindings.len() != 1 {
                    return Err(wire_error(
                        "worker_execution_environment_missing",
                        "environment-backed worker execution requires exactly one environment binding",
                    ));
                }
                let environment = request.ref_bindings.get(&binding_name).ok_or_else(|| {
                    wire_error(
                        "worker_execution_environment_missing",
                        "worker execution's declared environment binding is absent",
                    )
                })?;
                validate_worker_environment(environment)?
            }
        };
        let mut effective_config = config;
        let effective_object = effective_config.as_object_mut().ok_or_else(|| {
            wire_error(
                "worker_execution_config_invalid",
                "worker execution config must be an object",
            )
        })?;
        effective_object.remove("environment_binding");
        effective_object.insert(
            "worker_ref".to_string(),
            serde_json::Value::String(worker_ref.clone()),
        );
        Ok(LaunchPrepareSuccess {
            runtime_data: BTreeMap::from([("worker_execution".to_owned(), effective_config)]),
            required_secrets: Vec::new(),
            runtime_facts: BTreeMap::new(),
            execution_dependencies: BTreeMap::from([(
                DEPENDENCY_NAME.to_string(),
                LaunchExecutionDependencyRequestWire {
                    item_ref: worker_ref,
                },
            )]),
            financial_authority: FinancialAuthorityResultWire::None,
            external_effect_authority: ExternalEffectAuthorityResultWire::External {
                authority: serde_json::json!({
                    "authority_family":"worker_hosted_execution",
                    "admitted_effect_class":null
                }),
            },
        })
    })();
    match result {
        Ok(result) => LaunchPrepareResponse::Success { result },
        Err(error) => LaunchPrepareResponse::Error { error },
    }
}

fn validate(request: ValidateLaunchPreparerConfigRequest) -> ValidateLaunchPreparerConfigResponse {
    let valid = request.handler_config == serde_json::json!({})
        && request.primary_allowed_kinds == ["worker_execution"]
        && request.primary_allowed_spaces == [ItemSpaceWire::Bundle]
        && request.primary_allowed_trust == [TrustClassWire::TrustedBundle]
        && request.ref_bindings.len() == 1
        && request
            .ref_bindings
            .get(ENVIRONMENT_BINDING)
            .is_some_and(|decl| {
                !decl.required
                    && decl.allowed_kinds == ["config"]
                    && decl.allowed_spaces == [ItemSpaceWire::Bundle, ItemSpaceWire::Project]
                    && decl.allowed_trust
                        == [
                            TrustClassWire::TrustedBundle,
                            TrustClassWire::TrustedProject,
                        ]
            })
        && request.config_inputs.is_empty()
        && request.secret_policy.max_requirements == 0
        && request.secret_policy.allowed_names.is_empty()
        && request.required_runtime_data == ["worker_execution"]
        && request.runtime_facts.is_empty()
        && request.execution_dependencies.max_dependencies == 1
        && request.execution_dependencies.allowed_kinds == ["worker"]
        && request.execution_dependencies.allowed_spaces == [ItemSpaceWire::Bundle]
        && request.execution_dependencies.allowed_trust == [TrustClassWire::TrustedBundle]
        && matches!(
            request.financial_authority,
            FinancialAuthorityDeclWire::None
        )
        && matches!(
            request.external_effect_authority,
            ExternalEffectAuthorityDeclWire::External
        );
    if valid {
        ValidateLaunchPreparerConfigResponse::Valid {
            result: ValidateLaunchPreparerConfigSuccess {},
        }
    } else {
        ValidateLaunchPreparerConfigResponse::Invalid {
            code: "worker_execution_launch_contract_invalid".to_string(),
            message: "signed worker execution runtime contract differs from the pinned v1 contract"
                .to_string(),
        }
    }
}

fn validate_execution_config(
    value: &serde_json::Value,
) -> Result<WorkerSelection, LaunchPrepareError> {
    let object = value.as_object().ok_or_else(|| {
        wire_error(
            "worker_execution_config_invalid",
            "worker execution config must be an object",
        )
    })?;
    const KEYS: &[&str] = &[
        "worker_ref",
        "environment_binding",
        "required_credential_state",
        "route_set",
        "allowed_effect_classes",
        "credential_home_env",
        "workspace_env",
        "require_pinned_cow",
        "required_terminal_publication",
        "max_lifetime_seconds",
        "recover_upstream_session",
    ];
    if object.len() != KEYS.len() || object.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(wire_error(
            "worker_execution_config_invalid",
            "worker execution config has an unknown or missing field",
        ));
    }
    let worker_ref = object.get("worker_ref").and_then(serde_json::Value::as_str);
    let environment_binding = object
        .get("environment_binding")
        .and_then(serde_json::Value::as_str);
    let selection = match (worker_ref, environment_binding) {
        (Some(worker_ref), None) => WorkerSelection::Direct(validate_worker_ref(worker_ref)?),
        (None, Some(ENVIRONMENT_BINDING)) => {
            WorkerSelection::Environment(ENVIRONMENT_BINDING.to_string())
        }
        _ => {
            return Err(wire_error(
                "worker_execution_worker_invalid",
                "worker execution must select exactly one direct worker or declared environment binding",
            ));
        }
    };
    if !matches!(
        object.get("worker_ref"),
        Some(serde_json::Value::Null | serde_json::Value::String(_))
    ) || !matches!(
        object.get("environment_binding"),
        Some(serde_json::Value::Null | serde_json::Value::String(_))
    ) {
        return Err(wire_error(
            "worker_execution_worker_invalid",
            "worker and environment selectors must be required-nullable strings",
        ));
    }
    let require_pinned_cow = object
        .get("require_pinned_cow")
        .and_then(serde_json::Value::as_bool);
    let required_terminal_publication = object
        .get("required_terminal_publication")
        .and_then(serde_json::Value::as_str);
    if !matches!(
        object
            .get("required_credential_state")
            .and_then(serde_json::Value::as_str),
        Some("active" | "any")
    ) || !matches!(
        object
            .get("required_terminal_publication")
            .and_then(serde_json::Value::as_str),
        Some("retain_result" | "any")
    ) || object
        .get("max_lifetime_seconds")
        .and_then(serde_json::Value::as_u64)
        .is_none_or(|seconds| seconds == 0 || seconds > 603_600)
        || !object
            .get("require_pinned_cow")
            .is_some_and(serde_json::Value::is_boolean)
        || !object
            .get("recover_upstream_session")
            .is_some_and(serde_json::Value::is_boolean)
    {
        return Err(wire_error(
            "worker_execution_policy_invalid",
            "worker execution config is outside the admitted policy vocabulary",
        ));
    }
    if !matches!(
        (require_pinned_cow, required_terminal_publication),
        (Some(true), Some("retain_result")) | (Some(false), Some("any"))
    ) {
        return Err(wire_error(
            "worker_execution_policy_invalid",
            "pinned CoW worker execution requires retain_result; projectless execution requires any",
        ));
    }
    let route_set = object
        .get("route_set")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if route_set.is_empty()
        || route_set.len() > 128
        || !route_set.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(wire_error(
            "worker_execution_route_set_invalid",
            "worker execution route set is not a bounded portable identifier",
        ));
    }
    let effects = object
        .get("allowed_effect_classes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            wire_error(
                "worker_execution_effect_classes_invalid",
                "worker execution effect classes must be an array",
            )
        })?;
    let allowed = [
        "credential_delete",
        "credential_read",
        "credential_write",
        "external_effect",
        "pure_read",
        "session_mutation",
    ];
    let mut previous: Option<&str> = None;
    for effect in effects {
        let effect = effect.as_str().unwrap_or_default();
        if !allowed.contains(&effect) || previous.is_some_and(|prior| prior >= effect) {
            return Err(wire_error(
                "worker_execution_effect_classes_invalid",
                "worker execution effect classes must be sorted, unique, and admitted",
            ));
        }
        previous = Some(effect);
    }
    if effects.is_empty() {
        return Err(wire_error(
            "worker_execution_effect_classes_invalid",
            "worker execution must admit at least one route effect class",
        ));
    }
    for field in ["credential_home_env", "workspace_env"] {
        let value = object
            .get(field)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                wire_error(
                    "worker_execution_environment_invalid",
                    "worker execution environment slots must be strings",
                )
            })?;
        ryeos_engine::protocol_vocabulary::validate_env_name(value).map_err(|_| {
            wire_error(
                "worker_execution_environment_invalid",
                "worker execution environment slot is not canonical",
            )
        })?;
    }
    Ok(selection)
}

fn validate_worker_ref(worker_ref: &str) -> Result<String, LaunchPrepareError> {
    if worker_ref.is_empty() || worker_ref.len() > 256 || worker_ref.chars().any(char::is_control) {
        return Err(wire_error(
            "worker_execution_worker_invalid",
            "worker execution worker ref is not canonical",
        ));
    }
    let parsed = ryeos_engine::canonical_ref::CanonicalRef::parse(worker_ref).map_err(|_| {
        wire_error(
            "worker_execution_worker_invalid",
            "worker execution worker ref is not canonical",
        )
    })?;
    if parsed.kind != "worker" || parsed.suffix.is_some() {
        return Err(wire_error(
            "worker_execution_worker_invalid",
            "worker execution worker ref must be an unsuffixed worker item",
        ));
    }
    Ok(parsed.to_string())
}

fn validate_worker_environment(
    environment: &LaunchPreparedItemWire,
) -> Result<String, LaunchPrepareError> {
    if !environment.canonical_ref.starts_with("config:")
        || !matches!(
            (
                &environment.source_space,
                &environment.effective_trust_class
            ),
            (ItemSpaceWire::Bundle, TrustClassWire::TrustedBundle)
                | (ItemSpaceWire::Project, TrustClassWire::TrustedProject)
        )
    {
        return Err(wire_error(
            "worker_environment_identity_invalid",
            "worker environment must be a trusted bundle or trusted project config",
        ));
    }
    let value = environment.composed.composed.as_object().ok_or_else(|| {
        wire_error(
            "worker_environment_invalid",
            "worker environment must be an object",
        )
    })?;
    const KEYS: &[&str] = &[
        "category",
        "schema",
        "worker_ref",
        "configuration",
        "credential_requirement",
        "portable_state_contract",
    ];
    if value.len() != KEYS.len() || value.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(wire_error(
            "worker_environment_invalid",
            "worker environment has an unknown or missing field",
        ));
    }
    if value.get("schema").and_then(serde_json::Value::as_str)
        != Some("ryeos.worker_environment.v1")
        || value
            .get("category")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|category| category.is_empty() || category.len() > 128)
        || value
            .get("configuration")
            .and_then(serde_json::Value::as_object)
            .is_none_or(|configuration| !configuration.is_empty())
        || value
            .get("portable_state_contract")
            .and_then(serde_json::Value::as_str)
            != Some("ryeos.worker_session.restore.v1")
    {
        return Err(wire_error(
            "worker_environment_invalid",
            "worker environment is outside the admitted v1 contract",
        ));
    }
    let credential = value
        .get("credential_requirement")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            wire_error(
                "worker_environment_credential_invalid",
                "worker environment credential requirement must be an object",
            )
        })?;
    const CREDENTIAL_KEYS: &[&str] = &[
        "workload_family",
        "required_state",
        "subject_projection_contract",
    ];
    if credential.len() != CREDENTIAL_KEYS.len()
        || credential
            .keys()
            .any(|key| !CREDENTIAL_KEYS.contains(&key.as_str()))
        || credential
            .get("workload_family")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|value| value.is_empty() || value.len() > 64)
        || credential
            .get("required_state")
            .and_then(serde_json::Value::as_str)
            != Some("active")
        || credential
            .get("subject_projection_contract")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|value| value.is_empty() || value.len() > 128)
    {
        return Err(wire_error(
            "worker_environment_credential_invalid",
            "worker environment credential requirement is outside the admitted vocabulary",
        ));
    }
    validate_worker_ref(
        value
            .get("worker_ref")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> serde_json::Value {
        serde_json::json!({
            "worker_ref": "worker:fixture/hosted",
            "environment_binding": null,
            "required_credential_state": "active",
            "route_set": "session",
            "allowed_effect_classes": ["external_effect", "pure_read", "session_mutation"],
            "credential_home_env": "RYEOS_WORKLOAD_HOME",
            "workspace_env": "RYEOS_WORKSPACE",
            "require_pinned_cow": true,
            "required_terminal_publication": "retain_result",
            "max_lifetime_seconds": 86_400,
            "recover_upstream_session": true
        })
    }

    #[test]
    fn accepts_only_the_closed_worker_execution_policy_shape() {
        assert_eq!(
            validate_execution_config(&valid_config()).unwrap(),
            WorkerSelection::Direct("worker:fixture/hosted".to_string())
        );

        let mut unknown = valid_config();
        unknown["extension"] = serde_json::Value::Bool(true);
        assert_eq!(
            validate_execution_config(&unknown).unwrap_err().code,
            "worker_execution_config_invalid"
        );
    }

    #[test]
    fn rejects_noncanonical_or_authority_suffixed_worker_refs() {
        for worker_ref in [
            "worker:../hosted",
            "worker:fixture/hosted@t:2026-08-19T00:00:00Z",
            "directive:fixture/hosted",
        ] {
            let mut config = valid_config();
            config["worker_ref"] = serde_json::Value::String(worker_ref.to_owned());
            assert_eq!(
                validate_execution_config(&config).unwrap_err().code,
                "worker_execution_worker_invalid"
            );
        }
    }

    #[test]
    fn environment_selection_is_exclusive_and_uses_the_declared_slot() {
        let mut config = valid_config();
        config["worker_ref"] = serde_json::Value::Null;
        config["environment_binding"] = serde_json::json!(ENVIRONMENT_BINDING);
        assert_eq!(
            validate_execution_config(&config).unwrap(),
            WorkerSelection::Environment(ENVIRONMENT_BINDING.to_string())
        );

        config["worker_ref"] = serde_json::json!("worker:fixture/hosted");
        assert_eq!(
            validate_execution_config(&config).unwrap_err().code,
            "worker_execution_worker_invalid"
        );
    }

    #[test]
    fn validates_the_closed_portable_environment_declaration() {
        let environment = LaunchPreparedItemWire {
            canonical_ref: "config:fixture/environments/default".to_string(),
            source_space: ItemSpaceWire::Bundle,
            effective_trust_class: TrustClassWire::TrustedBundle,
            composed: ryeos_handler_protocol::LaunchComposedViewWire {
                composed: serde_json::json!({
                    "category":"fixture/environments",
                    "schema":"ryeos.worker_environment.v1",
                    "worker_ref":"worker:fixture/hosted",
                    "configuration":{},
                    "credential_requirement":{
                        "workload_family":"fixture",
                        "required_state":"active",
                        "subject_projection_contract":"fixture.account.v1"
                    },
                    "portable_state_contract":"ryeos.worker_session.restore.v1"
                }),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            resolution_digest: serde_json::json!({"digest":"retained"}),
        };
        assert_eq!(
            validate_worker_environment(&environment).unwrap(),
            "worker:fixture/hosted"
        );

        let mut malformed = environment;
        malformed.composed.composed["configuration"]["ambient_path"] =
            serde_json::json!("/usr/bin");
        assert_eq!(
            validate_worker_environment(&malformed).unwrap_err().code,
            "worker_environment_invalid"
        );
    }

    #[test]
    fn rejects_environment_and_lifetime_expansion() {
        let mut environment = valid_config();
        environment["workspace_env"] = serde_json::Value::String("PATH=/tmp".to_owned());
        assert_eq!(
            validate_execution_config(&environment).unwrap_err().code,
            "worker_execution_environment_invalid"
        );

        let mut lifetime = valid_config();
        lifetime["max_lifetime_seconds"] = serde_json::Value::from(603_601_u64);
        assert_eq!(
            validate_execution_config(&lifetime).unwrap_err().code,
            "worker_execution_policy_invalid"
        );
    }

    #[test]
    fn rejects_cross_field_terminal_publication_expansion() {
        for (pinned, publication) in [
            (true, "any"),
            (true, "discard"),
            (true, "advance_head"),
            (false, "retain_result"),
        ] {
            let mut config = valid_config();
            config["require_pinned_cow"] = serde_json::Value::Bool(pinned);
            config["required_terminal_publication"] =
                serde_json::Value::String(publication.to_owned());
            assert_eq!(
                validate_execution_config(&config).unwrap_err().code,
                "worker_execution_policy_invalid"
            );
        }
    }
}

fn wire_error(code: &str, message: &str) -> LaunchPrepareError {
    LaunchPrepareError {
        code: code.to_string(),
        message: message.to_string(),
        classification: LaunchPrepareErrorClass::Internal,
        binding: None,
        details: BTreeMap::new(),
    }
}
