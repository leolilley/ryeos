use std::collections::BTreeMap;

use ryeos_handler_bins::run_handler;
use ryeos_handler_protocol::{
    ExternalEffectAuthorityDeclWire, ExternalEffectAuthorityResultWire, FinancialAuthorityDeclWire,
    FinancialAuthorityResultWire, HandlerRequest, HandlerResponse, ItemSpaceWire,
    LaunchExecutionDependencyRequestWire, LaunchPrepareError, LaunchPrepareErrorClass,
    LaunchPrepareResponse, LaunchPrepareSuccess, TrustClassWire,
    ValidateLaunchPreparerConfigRequest, ValidateLaunchPreparerConfigResponse,
    ValidateLaunchPreparerConfigSuccess,
};

const DEPENDENCY_NAME: &str = "session_worker";

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
        if !request.ref_bindings.is_empty() || !request.config_inputs.is_empty() {
            return Err(wire_error(
                "worker_execution_launch_inputs_invalid",
                "worker execution launch accepts no ref bindings or config inputs",
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
        let worker_ref = validate_execution_config(&config)?;
        Ok(LaunchPrepareSuccess {
            runtime_data: BTreeMap::from([("worker_execution".to_owned(), config)]),
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
        && request.ref_bindings.is_empty()
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

fn validate_execution_config(value: &serde_json::Value) -> Result<String, LaunchPrepareError> {
    let object = value.as_object().ok_or_else(|| {
        wire_error(
            "worker_execution_config_invalid",
            "worker execution config must be an object",
        )
    })?;
    const KEYS: &[&str] = &[
        "worker_ref",
        "required_credential_state",
        "route_set",
        "allowed_effect_classes",
        "credential_home_env",
        "workspace_env",
        "require_pinned_cow",
        "required_terminal_publication",
        "max_lifetime_seconds",
        "recover_remote_session",
    ];
    if object.len() != KEYS.len() || object.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(wire_error(
            "worker_execution_config_invalid",
            "worker execution config has an unknown or missing field",
        ));
    }
    let worker_ref = object
        .get("worker_ref")
        .and_then(serde_json::Value::as_str)
        .filter(|value| value.len() <= 256 && !value.chars().any(char::is_control))
        .ok_or_else(|| {
            wire_error(
                "worker_execution_worker_invalid",
                "worker execution worker ref is not canonical",
            )
        })?;
    let parsed_worker_ref =
        ryeos_engine::canonical_ref::CanonicalRef::parse(worker_ref).map_err(|_| {
            wire_error(
                "worker_execution_worker_invalid",
                "worker execution worker ref is not canonical",
            )
        })?;
    if parsed_worker_ref.kind != "worker" || parsed_worker_ref.suffix.is_some() {
        return Err(wire_error(
            "worker_execution_worker_invalid",
            "worker execution worker ref must be an unsuffixed worker item",
        ));
    }
    if !matches!(
        object
            .get("required_credential_state")
            .and_then(serde_json::Value::as_str),
        Some("active" | "any")
    ) || !matches!(
        object
            .get("required_terminal_publication")
            .and_then(serde_json::Value::as_str),
        Some("retain_result" | "discard" | "advance_head" | "any")
    ) || object
        .get("max_lifetime_seconds")
        .and_then(serde_json::Value::as_u64)
        .is_none_or(|seconds| seconds == 0 || seconds > 603_600)
        || !object
            .get("require_pinned_cow")
            .is_some_and(serde_json::Value::is_boolean)
        || !object
            .get("recover_remote_session")
            .is_some_and(serde_json::Value::is_boolean)
    {
        return Err(wire_error(
            "worker_execution_policy_invalid",
            "worker execution config is outside the admitted policy vocabulary",
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
    Ok(worker_ref.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> serde_json::Value {
        serde_json::json!({
            "worker_ref": "worker:codex/hosted",
            "required_credential_state": "active",
            "route_set": "session",
            "allowed_effect_classes": ["external_effect", "pure_read", "session_mutation"],
            "credential_home_env": "RYEOS_WORKLOAD_HOME",
            "workspace_env": "RYEOS_WORKSPACE",
            "require_pinned_cow": true,
            "required_terminal_publication": "any",
            "max_lifetime_seconds": 86_400,
            "recover_remote_session": true
        })
    }

    #[test]
    fn accepts_only_the_closed_worker_execution_policy_shape() {
        assert_eq!(
            validate_execution_config(&valid_config()).unwrap(),
            "worker:codex/hosted"
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
            "worker:codex/hosted@t:2026-08-19T00:00:00Z",
            "directive:codex/hosted",
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
