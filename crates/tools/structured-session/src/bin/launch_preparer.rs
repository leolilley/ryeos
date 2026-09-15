use std::collections::{BTreeMap, BTreeSet};

use ryeos_handler_bins::run_handler;
use ryeos_handler_protocol::{
    ExecutableSearchPathEntryWire, ExternalEffectAuthorityDeclWire,
    ExternalEffectAuthorityResultWire, FinancialAuthorityDeclWire, FinancialAuthorityResultWire,
    HandlerRequest, HandlerResponse, ItemSpaceWire, LaunchContentDependencyRequestWire,
    LaunchEnvironmentContributionRequestWire, LaunchEnvironmentPathKindWire,
    LaunchEnvironmentValueWire, LaunchExecutionDependencyRequestWire, LaunchPrepareError,
    LaunchPrepareErrorClass, LaunchPrepareResponse, LaunchPrepareSuccess, LaunchPreparedItemWire,
    RefBindingSourceWire, TrustClassWire, ValidateLaunchPreparerConfigRequest,
    ValidateLaunchPreparerConfigResponse, ValidateLaunchPreparerConfigSuccess,
};
use serde::Deserialize;

const DEPENDENCY_NAME: &str = "session_worker";
const ENVIRONMENT_BINDING: &str = "environment";

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerSelection {
    Direct(String),
    Environment(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerExecutionMode {
    Session,
    BoundedTurn,
}

#[derive(Debug)]
struct ValidatedWorkerEnvironment {
    worker_ref: String,
    has_external_content: bool,
    executable_search: Vec<ExecutableSearchPathEntryWire>,
    process_environment: BTreeMap<String, AuthoredWorkerEnvironmentValue>,
    workload_client: Option<ryeos_runtime::workload_client::WorkloadClientRequestContract>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum AuthoredWorkerEnvironmentValue {
    Literal {
        value: String,
    },
    RealizationPath {
        realization_id: String,
        relative_path: String,
        path_kind: LaunchEnvironmentPathKindWire,
    },
    RuntimeViewDirectory {
        relative_path: String,
    },
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
        let (worker_ref, content_dependencies, environment_contributions, workload_client_request) =
            match selection {
                WorkerSelection::Direct(worker_ref) => {
                    if !request.ref_bindings.is_empty() {
                        return Err(wire_error(
                            "worker_execution_environment_unexpected",
                            "direct worker execution cannot carry an environment binding",
                        ));
                    }
                    (worker_ref, BTreeMap::new(), BTreeMap::new(), None)
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
                    let environment = validate_worker_environment(environment)?;
                    let content_dependencies = if environment.has_external_content {
                        BTreeMap::from([(
                            ENVIRONMENT_BINDING.to_owned(),
                            LaunchContentDependencyRequestWire {
                                binding: ENVIRONMENT_BINDING.to_owned(),
                                targets: vec![DEPENDENCY_NAME.to_owned()],
                                executable_search: environment.executable_search,
                            },
                        )])
                    } else {
                        BTreeMap::new()
                    };
                    let variables = environment
                        .process_environment
                        .into_iter()
                        .map(|(name, value)| {
                            let value = match value {
                                AuthoredWorkerEnvironmentValue::Literal { value } => {
                                    LaunchEnvironmentValueWire::Literal { value }
                                }
                                AuthoredWorkerEnvironmentValue::RealizationPath {
                                    realization_id,
                                    relative_path,
                                    path_kind,
                                } => LaunchEnvironmentValueWire::ContentPath {
                                    content_dependency: ENVIRONMENT_BINDING.to_owned(),
                                    realization_id,
                                    relative_path,
                                    path_kind,
                                },
                                AuthoredWorkerEnvironmentValue::RuntimeViewDirectory {
                                    relative_path,
                                } => LaunchEnvironmentValueWire::RuntimeViewDirectory {
                                    relative_path,
                                },
                            };
                            (name, value)
                        })
                        .collect::<BTreeMap<_, _>>();
                    let environment_contributions = (!variables.is_empty())
                        .then(|| {
                            BTreeMap::from([(
                                ENVIRONMENT_BINDING.to_owned(),
                                LaunchEnvironmentContributionRequestWire {
                                    targets: vec![DEPENDENCY_NAME.to_owned()],
                                    variables,
                                },
                            )])
                        })
                        .unwrap_or_default();
                    let workload_client_request = environment.workload_client;
                    (
                        environment.worker_ref,
                        content_dependencies,
                        environment_contributions,
                        workload_client_request,
                    )
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
            runtime_facts: workload_client_request
                .map(|request| {
                    BTreeMap::from([(
                        ryeos_runtime::workload_client::WORKLOAD_CLIENT_REQUEST_FACT.to_owned(),
                        serde_json::to_value(request).expect("validated request must serialize"),
                    )])
                })
                .unwrap_or_default(),
            execution_dependencies: BTreeMap::from([(
                DEPENDENCY_NAME.to_string(),
                LaunchExecutionDependencyRequestWire {
                    item_ref: worker_ref,
                },
            )]),
            content_dependencies,
            environment_contributions,
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
                    && decl.source == RefBindingSourceWire::Caller
                    && decl.project_result_requirement == ryeos_handler_protocol::ProjectResultRequirement::None
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
        && request.runtime_facts.len() == 1
        && request
            .runtime_facts
            .get(ryeos_runtime::workload_client::WORKLOAD_CLIENT_REQUEST_FACT)
            .is_some_and(|fact| {
                !fact.required
                    && fact.kind == ryeos_handler_protocol::RuntimeFactKindWire::Json
                    && fact.max_bytes
                        == ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_REQUEST_CONTRACT_BYTES
            })
        && request.execution_dependencies.max_dependencies == 1
        && request.execution_dependencies.allowed_kinds == ["worker"]
        && request.execution_dependencies.allowed_spaces == [ItemSpaceWire::Bundle]
        && request.execution_dependencies.allowed_trust == [TrustClassWire::TrustedBundle]
        && request.content_dependencies.max_dependencies == 1
        && request.content_dependencies.allowed_bindings == [ENVIRONMENT_BINDING]
        && request.content_dependencies.max_targets_per_dependency == 1
        && request.content_dependencies.max_executable_search_entries == 16
        && request
            .content_dependencies
            .external_content
            .as_ref()
            .is_some_and(|external| {
                external.max_declarations == 8
                    && external.large_content_max_total_bytes == Some(4_294_967_296)
            })
        && request.evidence_attachments.max_attachments == 16
        && request.evidence_attachments.max_total_bytes == 536_870_912
        && request.evidence_attachments.target.as_deref() == Some(DEPENDENCY_NAME)
        && request.evidence_attachments.destination_prefix.as_deref() == Some("evidence")
        && request.evidence_attachments.allowed_access
            == [ryeos_handler_protocol::EvidenceAttachmentAccessWire::ReadOnly]
        && request.environment_contributions.max_contributions == 1
        && request
            .environment_contributions
            .max_targets_per_contribution
            == 1
        && request
            .environment_contributions
            .max_variables_per_contribution
            == 32
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
        "mode",
        "candidate_disposition",
        "workload_client_delegation_caps",
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
    let mode = validate_execution_mode(object.get("mode"))?;
    let candidate_disposition = object
        .get("candidate_disposition")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    match (&mode, candidate_disposition) {
        (WorkerExecutionMode::Session, "owner_decision") => {}
        (WorkerExecutionMode::BoundedTurn, "retained_for_review")
            if require_pinned_cow == Some(true)
                && required_terminal_publication == Some("retain_result")
                && object
                    .get("recover_upstream_session")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                && object
                    .get("required_credential_state")
                    .and_then(serde_json::Value::as_str)
                    == Some("active") => {}
        _ => {
            return Err(wire_error(
                "worker_execution_policy_invalid",
                "bounded turns require an active credential, recoverable retained pinned CoW candidate; session mode remains owner-decision",
            ));
        }
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
    let workload_client_delegation_caps = object
        .get("workload_client_delegation_caps")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            wire_error(
                "worker_execution_workload_client_delegation_invalid",
                "worker execution workload-client delegation ceiling must be an array",
            )
        })?;
    let mut previous_capability: Option<&str> = None;
    if workload_client_delegation_caps.len() > 256 {
        return Err(wire_error(
            "worker_execution_workload_client_delegation_invalid",
            "worker execution workload-client delegation ceiling exceeds its bound",
        ));
    }
    for capability in workload_client_delegation_caps {
        let capability = capability.as_str().unwrap_or_default();
        if previous_capability.is_some_and(|previous| previous >= capability)
            || !capability.starts_with("ryeos.execute.")
            || ryeos_runtime::authorizer::validate_scope_pattern(capability).is_err()
        {
            return Err(wire_error(
                "worker_execution_workload_client_delegation_invalid",
                "worker execution workload-client delegation ceiling must contain sorted canonical execution capabilities",
            ));
        }
        previous_capability = Some(capability);
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

fn validate_execution_mode(
    value: Option<&serde_json::Value>,
) -> Result<WorkerExecutionMode, LaunchPrepareError> {
    let object = value
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            wire_error(
                "worker_execution_mode_invalid",
                "worker execution mode must be a closed object",
            )
        })?;
    match object.get("kind").and_then(serde_json::Value::as_str) {
        Some("session") if object.len() == 1 => Ok(WorkerExecutionMode::Session),
        Some("bounded_turn")
            if object.len() == 4
                && object.contains_key("session_start_route")
                && object.contains_key("turn_start_route")
                && object
                    .get("max_uncontacted_attempts")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|attempts| (1..=8).contains(&attempts)) =>
        {
            let session_start_route = validate_route_id(
                object.get("session_start_route"),
                "bounded session-start route",
            )?;
            let turn_start_route =
                validate_route_id(object.get("turn_start_route"), "bounded turn-start route")?;
            if session_start_route == turn_start_route {
                return Err(wire_error(
                    "worker_execution_mode_invalid",
                    "bounded session-start and turn-start routes must be distinct",
                ));
            }
            Ok(WorkerExecutionMode::BoundedTurn)
        }
        _ => Err(wire_error(
            "worker_execution_mode_invalid",
            "worker execution mode is not an admitted session or bounded-turn contract",
        )),
    }
}

fn validate_route_id(
    value: Option<&serde_json::Value>,
    label: &str,
) -> Result<String, LaunchPrepareError> {
    let value = value
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| {
            wire_error(
                "worker_execution_mode_invalid",
                &format!("{label} is not a bounded route identifier"),
            )
        })?;
    if !value.split('.').all(|segment| {
        !segment.is_empty()
            && segment.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
    }) {
        return Err(wire_error(
            "worker_execution_mode_invalid",
            &format!("{label} is not canonical"),
        ));
    }
    Ok(value.to_owned())
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
) -> Result<ValidatedWorkerEnvironment, LaunchPrepareError> {
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
        "external_content",
        "external_product_slots",
        "configuration",
        "credential_requirement",
        "portable_state_contract",
        "workload_client",
    ];
    if value.len() != KEYS.len() || value.keys().any(|key| !KEYS.contains(&key.as_str())) {
        return Err(wire_error(
            "worker_environment_invalid",
            "worker environment has an unknown or missing field",
        ));
    }
    if value.get("schema").and_then(serde_json::Value::as_str)
        != Some("ryeos.worker_environment.v6")
        || value
            .get("category")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|category| category.is_empty() || category.len() > 128)
        || value
            .get("portable_state_contract")
            .and_then(serde_json::Value::as_str)
            != Some("ryeos.worker_session.restore.v1")
    {
        return Err(wire_error(
            "worker_environment_invalid",
            "worker environment is outside the admitted v6 contract",
        ));
    }
    // These are the closed authored shape ceilings, not node/target grants.
    // Generic dependency admission applies the actual signed source/target
    // contracts after the application has resolved every pending product slot.
    let shape_contract = ryeos_engine::kind_registry::KindExternalContentDecl {
        realization_derived: ryeos_engine::external_content::EXTERNAL_REALIZATIONS_DERIVED_KEY
            .to_owned(),
        allowed_roots: Vec::new(),
        allowed_mount_roots: vec![
            ryeos_engine::external_content::ExternalContentMountRoot::Project,
            ryeos_engine::external_content::ExternalContentMountRoot::ExecutionRuntime,
        ],
        max_declarations: 8,
        large_content: None,
    };
    let shape = ryeos_engine::external_content::authored_external_content_shape(
        &environment.composed.composed,
        Some(&shape_contract),
        // Locator roots are forbidden by this syntax contract. This label
        // allows project-only slot syntax; provenance is checked just below.
        ryeos_engine::external_content::DeclaringAuthority::Project,
    )
    .map_err(|_| {
        wire_error(
            "worker_environment_content_invalid",
            "worker environment literal/slot declarations are malformed",
        )
    })?
    .ok_or_else(|| {
        wire_error(
            "worker_environment_content_invalid",
            "worker environment has no content shape",
        )
    })?;
    if !shape.product_slots.is_empty() && environment.source_space != ItemSpaceWire::Project {
        return Err(wire_error(
            "worker_environment_content_invalid",
            "worker product slots require a pinned-project environment",
        ));
    }
    let declaration_ids = shape.ids().collect::<BTreeSet<_>>();
    if shape.literal_declarations.iter().any(|declaration| {
        declaration.mode != ryeos_engine::external_content::ExternalContentMode::Pinned
            || declaration.locator.is_some()
            || declaration.digest.is_none()
    }) {
        return Err(wire_error(
            "worker_environment_content_invalid",
            "worker environment content must be unique locator-free pinned declarations",
        ));
    }
    let configuration = value
        .get("configuration")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            wire_error(
                "worker_environment_invalid",
                "worker environment configuration must be an object",
            )
        })?;
    if configuration.len() != 2
        || !configuration.contains_key("executable_search")
        || !configuration.contains_key("process_environment")
    {
        return Err(wire_error(
            "worker_environment_invalid",
            "worker environment configuration must contain executable_search and process_environment",
        ));
    }
    let executable_search: Vec<ExecutableSearchPathEntryWire> =
        serde_json::from_value(configuration["executable_search"].clone()).map_err(|_| {
            wire_error(
                "worker_environment_executable_search_invalid",
                "worker environment executable search is malformed",
            )
        })?;
    if executable_search.len() > 16 {
        return Err(wire_error(
            "worker_environment_executable_search_invalid",
            "worker environment executable search exceeds its signed ceiling",
        ));
    }
    let mut seen_search = BTreeSet::new();
    for entry in &executable_search {
        if !declaration_ids.contains(entry.realization_id.as_str())
            || !seen_search.insert((
                entry.realization_id.as_str(),
                entry.relative_directory.as_str(),
            ))
            || (entry.relative_directory != "."
                && ryeos_state::objects::validate_canonical_project_relative_path(
                    &entry.relative_directory,
                )
                .is_err())
        {
            return Err(wire_error(
                "worker_environment_executable_search_invalid",
                "worker environment executable search does not name a unique canonical declared realization directory",
            ));
        }
        if shape.kind_for_id(&entry.realization_id)
            != Some(ryeos_engine::external_content::ExternalContentKind::Tree)
        {
            return Err(wire_error(
                "worker_environment_executable_search_invalid",
                "worker environment executable search requires tree realizations",
            ));
        }
    }
    let process_environment: BTreeMap<String, AuthoredWorkerEnvironmentValue> =
        serde_json::from_value(configuration["process_environment"].clone()).map_err(|_| {
            wire_error(
                "worker_environment_variables_invalid",
                "worker environment variables are malformed",
            )
        })?;
    if process_environment.len() > 32 {
        return Err(wire_error(
            "worker_environment_variables_invalid",
            "worker environment variables exceed their signed ceiling",
        ));
    }
    for (name, value) in &process_environment {
        if ryeos_state::objects::validate_session_process_environment_name(name).is_err() {
            return Err(wire_error(
                "worker_environment_variables_invalid",
                "worker environment contains a protected or invalid variable name",
            ));
        }
        let valid = match value {
            AuthoredWorkerEnvironmentValue::Literal { value } => {
                value.len() <= 4096 && !value.chars().any(char::is_control)
            }
            AuthoredWorkerEnvironmentValue::RealizationPath {
                realization_id,
                relative_path,
                ..
            } => {
                declaration_ids.contains(realization_id.as_str())
                    && ryeos_state::objects::validate_session_process_environment_relative_path(
                        relative_path,
                    )
                    .is_ok()
            }
            AuthoredWorkerEnvironmentValue::RuntimeViewDirectory { relative_path } => {
                ryeos_state::objects::validate_session_process_environment_relative_path(
                    relative_path,
                )
                .is_ok()
            }
        };
        if !valid {
            return Err(wire_error(
                "worker_environment_variables_invalid",
                "worker environment contains an invalid value binding",
            ));
        }
    }
    let workload_client = match value.get("workload_client") {
        Some(serde_json::Value::Null) => None,
        Some(value) => {
            let request: ryeos_runtime::workload_client::WorkloadClientRequestContract =
                serde_json::from_value(value.clone()).map_err(|_| {
                    wire_error(
                        "worker_environment_workload_client_invalid",
                        "worker environment workload-client request is malformed",
                    )
                })?;
            request.validate().map_err(|_| {
                wire_error(
                    "worker_environment_workload_client_invalid",
                    "worker environment workload-client request is outside the closed contract",
                )
            })?;
            if let Some(client) = request.cli_program() {
                let declaration_kind =
                    shape.kind_for_id(&client.realization_id).ok_or_else(|| {
                        wire_error(
                            "worker_environment_workload_client_invalid",
                            "workload-client realization is not declared by the environment",
                        )
                    })?;
                if declaration_kind != ryeos_engine::external_content::ExternalContentKind::Tree {
                    return Err(wire_error(
                        "worker_environment_workload_client_invalid",
                        "workload-client realization must be a complete pinned tree",
                    ));
                }
                let parent = std::path::Path::new(&client.relative_path)
                    .parent()
                    .and_then(std::path::Path::to_str)
                    .filter(|path| !path.is_empty())
                    .unwrap_or(".");
                if !executable_search.iter().any(|entry| {
                    entry.realization_id == client.realization_id
                        && entry.relative_directory == parent
                }) {
                    return Err(wire_error(
                        "worker_environment_workload_client_invalid",
                        "workload-client executable is outside the environment executable search",
                    ));
                }
            }
            Some(request)
        }
        None => {
            return Err(wire_error(
                "worker_environment_workload_client_invalid",
                "worker environment workload_client must be present and nullable",
            ));
        }
    };
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
    let worker_ref = validate_worker_ref(
        value
            .get("worker_ref")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default(),
    )?;
    Ok(ValidatedWorkerEnvironment {
        worker_ref,
        has_external_content: !shape.is_empty(),
        executable_search,
        process_environment,
        workload_client,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_product_environment() -> LaunchPreparedItemWire {
        LaunchPreparedItemWire {
            canonical_ref: "config:fixture/environments/product".to_owned(),
            source_space: ItemSpaceWire::Project,
            effective_trust_class: TrustClassWire::TrustedProject,
            composed: ryeos_handler_protocol::LaunchComposedViewWire {
                composed: serde_json::json!({
                    "category":"fixture/environments", "schema":"ryeos.worker_environment.v6",
                    "worker_ref":"worker:fixture/hosted", "external_content":[],
                    "external_product_slots":[{
                        "id":"runtime", "relationship_ref":"config:fixture/recipe",
                        "relationship":"runtime_to_worker", "kind":"tree",
                        "mount_root":"execution_runtime", "mount":"runtime"
                    }],
                    "configuration":{
                        "executable_search":[{"realization_id":"runtime","relative_directory":"bin"}],
                        "process_environment":{
                            "FIXTURE_RUNTIME_ROOT":{"kind":"realization_path","realization_id":"runtime", "relative_path":"lib", "path_kind":"directory"}
                        }
                    },
                    "credential_requirement":{"workload_family":"fixture","required_state":"active","subject_projection_contract":"fixture.account.v1"},
                    "portable_state_contract":"ryeos.worker_session.restore.v1", "workload_client":null
                }),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            resolution_digest: serde_json::json!({"fixture":"syntax-only"}),
        }
    }

    #[test]
    fn v6_product_shape_selects_existing_dependency_without_a_witness() {
        let environment = pending_product_environment();
        let validated = validate_worker_environment(&environment).unwrap();
        assert!(validated.has_external_content);
        assert_eq!(validated.executable_search[0].realization_id, "runtime");
        let mut config = valid_config();
        config["worker_ref"] = serde_json::Value::Null;
        config["environment_binding"] = serde_json::json!(ENVIRONMENT_BINDING);
        let primary = LaunchPreparedItemWire {
            canonical_ref: "worker_execution:fixture/session".to_owned(),
            source_space: ItemSpaceWire::Project,
            effective_trust_class: TrustClassWire::TrustedProject,
            composed: ryeos_handler_protocol::LaunchComposedViewWire {
                composed: serde_json::json!({"config":config}),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            resolution_digest: serde_json::json!({"fixture":"primary"}),
        };
        let LaunchPrepareResponse::Success { result } =
            prepare(ryeos_handler_protocol::LaunchPrepareRequest {
                handler_config: serde_json::json!({}),
                primary,
                ref_bindings: BTreeMap::from([(ENVIRONMENT_BINDING.to_owned(), environment)]),
                config_inputs: BTreeMap::new(),
            })
        else {
            panic!("pending product shape must remain pure preparer input")
        };
        let dependency = &result.content_dependencies[ENVIRONMENT_BINDING];
        assert_eq!(dependency.binding, ENVIRONMENT_BINDING);
        assert_eq!(dependency.executable_search[0].realization_id, "runtime");
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("witness_hash")
        );
    }

    #[test]
    fn v6_product_shape_is_closed_and_does_not_accept_bundle_slots() {
        for predecessor in ["ryeos.worker_environment.v4", "ryeos.worker_environment.v5"] {
            let mut old = pending_product_environment();
            old.composed.composed["schema"] = serde_json::json!(predecessor);
            assert!(validate_worker_environment(&old).is_err());
        }
        let mut missing = pending_product_environment();
        missing
            .composed
            .composed
            .as_object_mut()
            .unwrap()
            .remove("external_product_slots");
        assert!(validate_worker_environment(&missing).is_err());
        let mut bundled = pending_product_environment();
        bundled.source_space = ItemSpaceWire::Bundle;
        bundled.effective_trust_class = TrustClassWire::TrustedBundle;
        assert!(validate_worker_environment(&bundled).is_err());
        for field in ["digest", "witness_hash", "relationship_binding", "locator"] {
            let mut forged = pending_product_environment();
            forged.composed.composed["external_product_slots"][0][field] =
                serde_json::json!("caller");
            assert!(validate_worker_environment(&forged).is_err(), "{field}");
        }
        let mut file_search = pending_product_environment();
        file_search.composed.composed["external_product_slots"][0]["kind"] =
            serde_json::json!("file");
        assert!(validate_worker_environment(&file_search).is_err());
        let mut undeclared = pending_product_environment();
        undeclared.composed.composed["configuration"]["process_environment"]["FIXTURE_RUNTIME_ROOT"]
            ["realization_id"] = serde_json::json!("other");
        assert!(validate_worker_environment(&undeclared).is_err());
        let mut protected = pending_product_environment();
        protected.composed.composed["configuration"]["process_environment"]["PYTHONHOME"] =
            serde_json::json!({"kind":"literal","value":"/host/python"});
        assert!(validate_worker_environment(&protected).is_err());
    }

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
            "recover_upstream_session": true,
            "mode": {"kind":"session"},
            "candidate_disposition":"owner_decision",
            "workload_client_delegation_caps": []
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
                    "schema":"ryeos.worker_environment.v6",
                    "external_product_slots":[],
                    "worker_ref":"worker:fixture/hosted",
                    "external_content":[],
                    "configuration":{
                        "executable_search":[],
                        "process_environment":{
                            "CARGO_NET_OFFLINE":{"kind":"literal","value":"true"},
                            "CARGO_HOME":{
                                "kind":"runtime_view_directory",
                                "relative_path":"cargo/home"
                            }
                        }
                    },
                    "credential_requirement":{
                        "workload_family":"fixture",
                        "required_state":"active",
                        "subject_projection_contract":"fixture.account.v1"
                    },
                    "portable_state_contract":"ryeos.worker_session.restore.v1",
                    "workload_client":null
                }),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            resolution_digest: serde_json::json!({"digest":"retained"}),
        };
        let validated = validate_worker_environment(&environment).unwrap();
        assert_eq!(validated.worker_ref, "worker:fixture/hosted");
        assert!(validated.executable_search.is_empty());
        assert_eq!(validated.process_environment.len(), 2);

        let mut malformed = environment;
        malformed.composed.composed["configuration"]["ambient_path"] =
            serde_json::json!("/usr/bin");
        assert_eq!(
            validate_worker_environment(&malformed).unwrap_err().code,
            "worker_environment_invalid"
        );
    }

    #[test]
    fn preparation_keeps_process_environment_independent_from_content() {
        let mut config = valid_config();
        config["worker_ref"] = serde_json::Value::Null;
        config["environment_binding"] = serde_json::json!(ENVIRONMENT_BINDING);
        let primary = LaunchPreparedItemWire {
            canonical_ref: "worker_execution:fixture/session".to_owned(),
            source_space: ItemSpaceWire::Bundle,
            effective_trust_class: TrustClassWire::TrustedBundle,
            composed: ryeos_handler_protocol::LaunchComposedViewWire {
                composed: serde_json::json!({"config": config}),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            resolution_digest: serde_json::json!({"digest":"primary"}),
        };
        let environment = LaunchPreparedItemWire {
            canonical_ref: "config:fixture/environments/default".to_owned(),
            source_space: ItemSpaceWire::Bundle,
            effective_trust_class: TrustClassWire::TrustedBundle,
            composed: ryeos_handler_protocol::LaunchComposedViewWire {
                composed: serde_json::json!({
                    "category":"fixture/environments",
                    "schema":"ryeos.worker_environment.v6",
                    "external_product_slots":[],
                    "worker_ref":"worker:fixture/hosted",
                    "external_content":[],
                    "configuration":{
                        "executable_search":[],
                        "process_environment":{
                            "CARGO_HOME":{
                                "kind":"runtime_view_directory",
                                "relative_path":"cargo/home"
                            }
                        }
                    },
                    "credential_requirement":{
                        "workload_family":"fixture",
                        "required_state":"active",
                        "subject_projection_contract":"fixture.account.v1"
                    },
                    "portable_state_contract":"ryeos.worker_session.restore.v1",
                    "workload_client":null
                }),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            resolution_digest: serde_json::json!({"digest":"environment"}),
        };
        let LaunchPrepareResponse::Success { result } =
            prepare(ryeos_handler_protocol::LaunchPrepareRequest {
                handler_config: serde_json::json!({}),
                primary,
                ref_bindings: BTreeMap::from([(ENVIRONMENT_BINDING.to_owned(), environment)]),
                config_inputs: BTreeMap::new(),
            })
        else {
            panic!("valid environment-backed preparation must succeed");
        };
        assert!(result.content_dependencies.is_empty());
        let contribution = result
            .environment_contributions
            .get(ENVIRONMENT_BINDING)
            .expect("process environment contribution");
        assert_eq!(contribution.targets, [DEPENDENCY_NAME]);
        assert!(matches!(
            contribution.variables.get("CARGO_HOME"),
            Some(LaunchEnvironmentValueWire::RuntimeViewDirectory { relative_path })
                if relative_path == "cargo/home"
        ));
    }

    #[test]
    fn worker_environment_rejects_protected_names_and_absent_realizations() {
        let mut environment = LaunchPreparedItemWire {
            canonical_ref: "config:fixture/environments/default".to_string(),
            source_space: ItemSpaceWire::Bundle,
            effective_trust_class: TrustClassWire::TrustedBundle,
            composed: ryeos_handler_protocol::LaunchComposedViewWire {
                composed: serde_json::json!({
                    "category":"fixture/environments",
                    "schema":"ryeos.worker_environment.v6",
                    "external_product_slots":[],
                    "worker_ref":"worker:fixture/hosted",
                    "external_content":[],
                    "configuration":{"executable_search":[],"process_environment":{}},
                    "credential_requirement":{
                        "workload_family":"fixture",
                        "required_state":"active",
                        "subject_projection_contract":"fixture.account.v1"
                    },
                    "portable_state_contract":"ryeos.worker_session.restore.v1",
                    "workload_client":null
                }),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            resolution_digest: serde_json::json!({"digest":"retained"}),
        };
        environment.composed.composed["configuration"]["process_environment"]["RYEOS_WORKSPACE"] =
            serde_json::json!({"kind":"literal","value":"forged"});
        assert_eq!(
            validate_worker_environment(&environment).unwrap_err().code,
            "worker_environment_variables_invalid"
        );
        environment.composed.composed["configuration"]["process_environment"] = serde_json::json!({
            "CARGO_HOME":{
                "kind":"realization_path",
                "realization_id":"missing",
                "relative_path":"cargo",
                "path_kind":"directory"
            }
        });
        assert_eq!(
            validate_worker_environment(&environment).unwrap_err().code,
            "worker_environment_variables_invalid"
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
    fn workload_client_request_must_name_a_declared_searchable_tree_member() {
        let environment = LaunchPreparedItemWire {
            canonical_ref: "config:fixture/environments/development".to_owned(),
            source_space: ItemSpaceWire::Project,
            effective_trust_class: TrustClassWire::TrustedProject,
            composed: ryeos_handler_protocol::LaunchComposedViewWire {
                composed: serde_json::json!({
                    "category":"fixture/environments",
                    "schema":"ryeos.worker_environment.v6",
                    "external_product_slots":[],
                    "worker_ref":"worker:fixture/hosted",
                    "external_content":[{
                        "id":"workload-client",
                        "kind":"tree",
                        "mode":"pinned",
                        "digest":"a".repeat(64),
                        "metadata_hint":"fixture-client",
                        "mount_root": "project",
                        "mount":"environment/workload-client"
                    }],
                    "configuration":{
                        "executable_search":[{
                            "realization_id":"workload-client",
                            "relative_directory":"bin"
                        }],
                        "process_environment":{}
                    },
                    "credential_requirement":{
                        "workload_family":"fixture",
                        "required_state":"active",
                        "subject_projection_contract":"fixture.account.v1"
                    },
                    "portable_state_contract":"ryeos.worker_session.restore.v1",
                    "workload_client":{
                        "protocol":ryeos_runtime::workload_client::WORKLOAD_CLIENT_PROTOCOL,
                        "bindings":[{"kind":"cli","program":{
                            "realization_id":"workload-client",
                            "relative_path":"bin/ryeos"
                        }}],
                        "executions":[{
                            "item_ref":"directive:project/check",
                            "ref_bindings":{},
                            "calls":[{"kind":"default"}],
                            "effect_classes":["live"],
                            "workspace_access":"immutable_current_generation"
                        }],
                        "max_in_flight":1,
                        "max_invocations_per_boot":8,
                        "max_lifetime_seconds":300
                    }
                }),
                derived: BTreeMap::new(),
                policy_facts: BTreeMap::new(),
            },
            resolution_digest: serde_json::json!({"digest":"retained"}),
        };
        let validated = validate_worker_environment(&environment).unwrap();
        assert!(validated.workload_client.is_some());

        let mut missing = environment;
        missing.composed.composed["workload_client"]["bindings"][0]["program"]["realization_id"] =
            serde_json::json!("absent");
        assert_eq!(
            validate_worker_environment(&missing).unwrap_err().code,
            "worker_environment_workload_client_invalid"
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

    #[test]
    fn bounded_turn_is_closed_and_requires_retained_candidate_policy() {
        let mut config = valid_config();
        config["mode"] = serde_json::json!({
            "kind":"bounded_turn",
            "session_start_route":"session.start",
            "turn_start_route":"turn.start",
            "max_uncontacted_attempts":3
        });
        config["candidate_disposition"] = serde_json::json!("retained_for_review");
        config["workload_client_delegation_caps"] = serde_json::json!(["ryeos.execute.tool.*"]);
        assert_eq!(
            validate_execution_config(&config).unwrap(),
            WorkerSelection::Direct("worker:fixture/hosted".to_owned())
        );

        for required_field in [
            "mode",
            "candidate_disposition",
            "workload_client_delegation_caps",
        ] {
            let mut missing = config.clone();
            missing.as_object_mut().unwrap().remove(required_field);
            assert_eq!(
                validate_execution_config(&missing).unwrap_err().code,
                "worker_execution_config_invalid"
            );
        }

        // The explicit ceiling is execution-only and never supplies a
        // missing signed environment grant or admits broader owner authority.
        for invalid_caps in [
            serde_json::json!(["*"]),
            serde_json::json!(["ryeos.*"]),
            serde_json::json!(["ryeos.runtime.dedicated_session.*"]),
            serde_json::json!(["ryeos.execute.tool.*", "ryeos.execute.tool.*"]),
        ] {
            let mut invalid = config.clone();
            invalid["workload_client_delegation_caps"] = invalid_caps;
            assert_eq!(
                validate_execution_config(&invalid).unwrap_err().code,
                "worker_execution_workload_client_delegation_invalid"
            );
        }

        for (field, value) in [
            ("candidate_disposition", serde_json::json!("owner_decision")),
            ("recover_upstream_session", serde_json::json!(false)),
            ("required_credential_state", serde_json::json!("any")),
        ] {
            let mut invalid = config.clone();
            invalid[field] = value;
            assert_eq!(
                validate_execution_config(&invalid).unwrap_err().code,
                "worker_execution_policy_invalid"
            );
        }

        let mut caller_selected_route = config;
        caller_selected_route["mode"]["extra_route"] = serde_json::json!("turn.steer");
        assert_eq!(
            validate_execution_config(&caller_selected_route)
                .unwrap_err()
                .code,
            "worker_execution_mode_invalid"
        );

        let mut unbounded_attempts = caller_selected_route;
        unbounded_attempts["mode"]
            .as_object_mut()
            .unwrap()
            .remove("extra_route");
        unbounded_attempts["mode"]["max_uncontacted_attempts"] = serde_json::json!(9);
        assert_eq!(
            validate_execution_config(&unbounded_attempts)
                .unwrap_err()
                .code,
            "worker_execution_mode_invalid"
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
