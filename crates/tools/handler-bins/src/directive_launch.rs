use std::collections::BTreeMap;

use ryeos_directive_core::{
    prepare_directive_launch, DirectiveDiagnosticScalar, DirectiveLaunchPreparationInput,
    DirectivePreparationError, DirectivePreparationErrorClass, ProviderConfigSource,
    SnapshotItemSpace, SnapshotTrustClass, VerifiedConfigItem, MODEL_BINDING,
    MODEL_PROVIDERS_INPUT, MODEL_ROUTING_INPUT, PROVIDER_CONFIG_PREFIX, PROVIDER_SNAPSHOT_KEY,
};
use ryeos_handler_protocol::{
    ConfigMergeModeWire, HandlerResponse, ItemSpaceWire, LaunchConfigContributorWire,
    LaunchConfigInputDeclWire, LaunchConfigSnapshotWire, LaunchDiagnosticScalarWire,
    LaunchPrepareError, LaunchPrepareErrorClass, LaunchPrepareRequest, LaunchPrepareResponse,
    LaunchPrepareSuccess, LaunchSecretOriginWire, LaunchSecretRequirement, RuntimeFactKindWire,
    TrustClassWire, ValidateLaunchPreparerConfigRequest, ValidateLaunchPreparerConfigResponse,
    ValidateLaunchPreparerConfigSuccess,
};

const ALLOWED_SECRET_NAMES: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "ZEN_API_KEY",
];

pub fn prepare(request: LaunchPrepareRequest) -> HandlerResponse {
    HandlerResponse::LaunchPrepare {
        response: match prepare_inner(request) {
            Ok(result) => LaunchPrepareResponse::Success { result },
            Err(error) => LaunchPrepareResponse::Error { error },
        },
    }
}

pub fn validate(request: ValidateLaunchPreparerConfigRequest) -> HandlerResponse {
    let response = match validate_contract(&request) {
        Ok(()) => ValidateLaunchPreparerConfigResponse::Valid {
            result: ValidateLaunchPreparerConfigSuccess {},
        },
        Err(message) => ValidateLaunchPreparerConfigResponse::Invalid {
            code: "directive_launch_contract_invalid".to_string(),
            message,
        },
    };
    HandlerResponse::ValidateLaunchPreparerConfig { response }
}

pub fn wrong_request() -> HandlerResponse {
    HandlerResponse::LaunchPrepare {
        response: LaunchPrepareResponse::Error {
            error: wire_error(
                "directive_launch_protocol_mismatch",
                "directive launch preparer received a parser or composer request",
                LaunchPrepareErrorClass::Internal,
                None,
            ),
        },
    }
}

fn prepare_inner(
    request: LaunchPrepareRequest,
) -> Result<LaunchPrepareSuccess, LaunchPrepareError> {
    require_empty_handler_config(&request.handler_config).map_err(|message| {
        wire_error(
            "directive_launch_config_invalid",
            message,
            LaunchPrepareErrorClass::Internal,
            None,
        )
    })?;

    if request.ref_bindings.len() != 1 || !request.ref_bindings.contains_key(MODEL_BINDING) {
        return Err(wire_error(
            "model_binding_invalid",
            "directive launch preparation requires exactly the model binding",
            LaunchPrepareErrorClass::Internal,
            Some(MODEL_BINDING),
        ));
    }
    if request.config_inputs.len() != 2
        || !request.config_inputs.contains_key(MODEL_ROUTING_INPUT)
        || !request.config_inputs.contains_key(MODEL_PROVIDERS_INPUT)
    {
        return Err(wire_error(
            "directive_config_inputs_invalid",
            "directive launch preparation requires exactly model_routing and model_providers",
            LaunchPrepareErrorClass::Internal,
            None,
        ));
    }

    let model_item = request
        .ref_bindings
        .get(MODEL_BINDING)
        .expect("binding key checked");
    let routing = item_snapshot(
        request
            .config_inputs
            .get(MODEL_ROUTING_INPUT)
            .expect("config key checked"),
        MODEL_ROUTING_INPUT,
    )?;
    let providers = catalog_snapshot(
        request
            .config_inputs
            .get(MODEL_PROVIDERS_INPUT)
            .expect("config key checked"),
        MODEL_PROVIDERS_INPUT,
    )?;

    let prepared = prepare_directive_launch(DirectiveLaunchPreparationInput {
        primary_ref: &request.primary.canonical_ref,
        primary_composed: &request.primary.composed.composed,
        model_ref: &model_item.canonical_ref,
        model_composed: &model_item.composed.composed,
        model_routing: routing.as_ref(),
        provider_catalog: &providers,
    })
    .map_err(domain_error)?;

    if prepared
        .required_secret
        .as_ref()
        .is_some_and(|secret| !ALLOWED_SECRET_NAMES.contains(&secret.name.as_str()))
    {
        return Err(wire_error(
            "provider_secret_not_allowed",
            "the selected provider requests a secret outside the signed directive allow-list",
            LaunchPrepareErrorClass::Configuration,
            Some(MODEL_BINDING),
        ));
    }

    let snapshot = serde_json::to_value(&prepared.snapshot).map_err(|error| {
        wire_error(
            "provider_snapshot_serialize_failed",
            format!("could not serialize provider snapshot: {error}"),
            LaunchPrepareErrorClass::Internal,
            None,
        )
    })?;
    let mut runtime_data = BTreeMap::new();
    runtime_data.insert(PROVIDER_SNAPSHOT_KEY.to_string(), snapshot);

    let required_secrets = prepared
        .required_secret
        .into_iter()
        .map(|secret| LaunchSecretRequirement {
            name: secret.name,
            origin: LaunchSecretOriginWire::ConfigInput {
                name: secret.config_input.to_string(),
                canonical_id: secret.canonical_id,
                value_digest: secret.value_digest,
            },
        })
        .collect();

    Ok(LaunchPrepareSuccess {
        runtime_data,
        required_secrets,
        runtime_facts: prepared.runtime_facts,
    })
}

fn item_snapshot(
    snapshot: &LaunchConfigSnapshotWire,
    name: &str,
) -> Result<Option<VerifiedConfigItem>, LaunchPrepareError> {
    let LaunchConfigSnapshotWire::Item {
        present,
        value,
        value_digest,
        contributors,
    } = snapshot
    else {
        return Err(wire_error(
            "directive_config_input_kind_invalid",
            format!("config input {name} must be an item snapshot"),
            LaunchPrepareErrorClass::Internal,
            None,
        ));
    };

    if !present {
        if value.is_some() || value_digest.is_some() || !contributors.is_empty() {
            return Err(wire_error(
                "directive_config_snapshot_invalid",
                format!("absent config input {name} has value or provenance"),
                LaunchPrepareErrorClass::Internal,
                None,
            ));
        }
        return Ok(None);
    }

    let value = value.clone().ok_or_else(|| {
        wire_error(
            "directive_config_snapshot_invalid",
            format!("present config input {name} has no value"),
            LaunchPrepareErrorClass::Internal,
            None,
        )
    })?;
    let value_digest = value_digest.clone().ok_or_else(|| {
        wire_error(
            "directive_config_snapshot_invalid",
            format!("present config input {name} has no value digest"),
            LaunchPrepareErrorClass::Internal,
            None,
        )
    })?;
    Ok(Some(VerifiedConfigItem {
        value,
        value_digest,
        contributors: contributors.iter().map(convert_contributor).collect(),
    }))
}

fn catalog_snapshot(
    snapshot: &LaunchConfigSnapshotWire,
    name: &str,
) -> Result<BTreeMap<String, VerifiedConfigItem>, LaunchPrepareError> {
    let LaunchConfigSnapshotWire::Catalog { entries } = snapshot else {
        return Err(wire_error(
            "directive_config_input_kind_invalid",
            format!("config input {name} must be a catalog snapshot"),
            LaunchPrepareErrorClass::Internal,
            None,
        ));
    };
    Ok(entries
        .iter()
        .map(|(canonical_id, entry)| {
            (
                canonical_id.clone(),
                VerifiedConfigItem {
                    value: entry.value.clone(),
                    value_digest: entry.value_digest.clone(),
                    contributors: entry.contributors.iter().map(convert_contributor).collect(),
                },
            )
        })
        .collect())
}

fn convert_contributor(value: &LaunchConfigContributorWire) -> ProviderConfigSource {
    ProviderConfigSource {
        space: match value.space {
            ItemSpaceWire::Bundle => SnapshotItemSpace::Bundle,
            ItemSpaceWire::Project => SnapshotItemSpace::Project,
        },
        root_label: value.root_label.clone(),
        canonical_id: value.canonical_id.clone(),
        content_digest: value.content_digest.clone(),
        trust_class: match value.trust_class {
            TrustClassWire::TrustedBundle => SnapshotTrustClass::TrustedBundle,
            TrustClassWire::TrustedProject => SnapshotTrustClass::TrustedProject,
            TrustClassWire::UntrustedProject => SnapshotTrustClass::UntrustedProject,
            TrustClassWire::Unsigned => SnapshotTrustClass::Unsigned,
        },
    }
}

fn domain_error(error: DirectivePreparationError) -> LaunchPrepareError {
    let classification = match error.classification {
        DirectivePreparationErrorClass::Caller => LaunchPrepareErrorClass::Caller,
        DirectivePreparationErrorClass::Configuration => LaunchPrepareErrorClass::Configuration,
        DirectivePreparationErrorClass::Internal => LaunchPrepareErrorClass::Internal,
    };
    LaunchPrepareError {
        code: error.code.to_string(),
        message: safe_message(error.message),
        classification,
        binding: error.binding.map(str::to_string),
        details: error
            .details
            .into_iter()
            .map(|(key, value)| {
                let value = match value {
                    DirectiveDiagnosticScalar::Bool(value) => {
                        LaunchDiagnosticScalarWire::Bool(value)
                    }
                    DirectiveDiagnosticScalar::Integer(value) => {
                        LaunchDiagnosticScalarWire::Integer(value)
                    }
                    DirectiveDiagnosticScalar::String(value) => {
                        LaunchDiagnosticScalarWire::String(value)
                    }
                };
                (key, value)
            })
            .collect(),
    }
}

fn wire_error(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: LaunchPrepareErrorClass,
    binding: Option<&str>,
) -> LaunchPrepareError {
    LaunchPrepareError {
        code: code.into(),
        message: safe_message(message.into()),
        classification,
        binding: binding.map(str::to_string),
        details: BTreeMap::new(),
    }
}

fn safe_message(value: String) -> String {
    let mut value: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    if value.len() <= 512 {
        return value;
    }
    let mut end = 512;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

fn validate_contract(request: &ValidateLaunchPreparerConfigRequest) -> Result<(), String> {
    require_empty_handler_config(&request.handler_config)?;
    exact_strings(
        "primary_allowed_kinds",
        &request.primary_allowed_kinds,
        &["directive"],
    )?;
    exact_values(
        "primary_allowed_spaces",
        &request.primary_allowed_spaces,
        &[ItemSpaceWire::Bundle, ItemSpaceWire::Project],
    )?;
    exact_trust_values(
        "primary_allowed_trust",
        &request.primary_allowed_trust,
        &[
            TrustClassWire::TrustedBundle,
            TrustClassWire::TrustedProject,
        ],
    )?;

    if request.ref_bindings.len() != 1 {
        return Err("ref_bindings must declare exactly model".to_string());
    }
    let model = request
        .ref_bindings
        .get(MODEL_BINDING)
        .ok_or_else(|| "ref_bindings.model is required".to_string())?;
    if !model.required {
        return Err("ref_bindings.model must be required".to_string());
    }
    exact_strings("model.allowed_kinds", &model.allowed_kinds, &["directive"])?;
    exact_values(
        "model.allowed_spaces",
        &model.allowed_spaces,
        &[ItemSpaceWire::Bundle, ItemSpaceWire::Project],
    )?;
    exact_trust_values(
        "model.allowed_trust",
        &model.allowed_trust,
        &[
            TrustClassWire::TrustedBundle,
            TrustClassWire::TrustedProject,
        ],
    )?;

    if request.config_inputs.len() != 2 {
        return Err("config_inputs must declare exactly model_routing and model_providers".into());
    }
    match request.config_inputs.get(MODEL_ROUTING_INPUT) {
        Some(LaunchConfigInputDeclWire::Item {
            id,
            required,
            merge,
            allowed_spaces,
            allowed_trust,
        }) if id == "ryeos-runtime/model_routing"
            && !required
            && *merge == ConfigMergeModeWire::DeepMerge =>
        {
            exact_values(
                "model_routing.allowed_spaces",
                allowed_spaces,
                &[ItemSpaceWire::Bundle, ItemSpaceWire::Project],
            )?;
            exact_trust_values(
                "model_routing.allowed_trust",
                allowed_trust,
                &[
                    TrustClassWire::TrustedBundle,
                    TrustClassWire::TrustedProject,
                ],
            )?;
        }
        _ => return Err("model_routing must be the optional deep-merged routing item".into()),
    }
    match request.config_inputs.get(MODEL_PROVIDERS_INPUT) {
        Some(LaunchConfigInputDeclWire::Catalog {
            prefix,
            required,
            entry_merge,
            allowed_spaces,
            allowed_trust,
        }) if prefix == PROVIDER_CONFIG_PREFIX
            && *required
            && *entry_merge == ConfigMergeModeWire::DeepMerge =>
        {
            exact_values(
                "model_providers.allowed_spaces",
                allowed_spaces,
                &[ItemSpaceWire::Bundle],
            )?;
            exact_trust_values(
                "model_providers.allowed_trust",
                allowed_trust,
                &[TrustClassWire::TrustedBundle],
            )?;
        }
        _ => return Err("model_providers must be the trusted-bundle provider catalog".into()),
    }

    if request.secret_policy.max_requirements != 4 {
        return Err("secret_policy.max_requirements must be 4".into());
    }
    exact_strings(
        "secret_policy.allowed_names",
        &request.secret_policy.allowed_names,
        ALLOWED_SECRET_NAMES,
    )?;
    exact_strings(
        "required_runtime_data",
        &request.required_runtime_data,
        &[PROVIDER_SNAPSHOT_KEY],
    )?;

    if request.runtime_facts.len() != 8 {
        return Err("runtime_facts must declare the eight directive fact fields".into());
    }
    fact(
        request,
        "provider_id",
        true,
        RuntimeFactKindWire::String,
        128,
    )?;
    fact(
        request,
        "model_name",
        true,
        RuntimeFactKindWire::String,
        256,
    )?;
    fact(
        request,
        "context_window",
        true,
        RuntimeFactKindWire::Integer,
        32,
    )?;
    fact(request, "sampling", true, RuntimeFactKindWire::Json, 4096)?;
    fact(
        request,
        "matched_profile",
        false,
        RuntimeFactKindWire::String,
        128,
    )?;
    fact(
        request,
        "config_hash",
        true,
        RuntimeFactKindWire::String,
        66,
    )?;
    fact(
        request,
        "config_value_digest",
        true,
        RuntimeFactKindWire::String,
        66,
    )?;
    fact(
        request,
        "config_sources",
        true,
        RuntimeFactKindWire::Json,
        4096,
    )?;
    Ok(())
}

fn require_empty_handler_config(value: &serde_json::Value) -> Result<(), String> {
    match value.as_object() {
        Some(config) if config.is_empty() => Ok(()),
        _ => Err("handler_config must be an empty object".to_string()),
    }
}

fn exact_strings(label: &str, actual: &[String], expected: &[&str]) -> Result<(), String> {
    if actual.len() == expected.len()
        && expected
            .iter()
            .all(|expected| actual.iter().any(|actual| actual.as_str() == *expected))
    {
        Ok(())
    } else {
        Err(format!(
            "{label} does not match the directive launch contract"
        ))
    }
}

fn exact_trust_values(
    label: &str,
    actual: &[TrustClassWire],
    expected: &[TrustClassWire],
) -> Result<(), String> {
    if actual.len() == expected.len()
        && expected.iter().all(|expected| {
            actual
                .iter()
                .any(|actual| trust_class_key(actual) == trust_class_key(expected))
        })
    {
        Ok(())
    } else {
        Err(format!(
            "{label} does not match the directive launch contract"
        ))
    }
}

fn trust_class_key(value: &TrustClassWire) -> u8 {
    match value {
        TrustClassWire::TrustedBundle => 0,
        TrustClassWire::TrustedProject => 1,
        TrustClassWire::UntrustedProject => 2,
        TrustClassWire::Unsigned => 3,
    }
}

fn exact_values<T: PartialEq>(label: &str, actual: &[T], expected: &[T]) -> Result<(), String> {
    if actual.len() == expected.len()
        && expected
            .iter()
            .all(|expected| actual.iter().any(|actual| actual == expected))
    {
        Ok(())
    } else {
        Err(format!(
            "{label} does not match the directive launch contract"
        ))
    }
}

fn fact(
    request: &ValidateLaunchPreparerConfigRequest,
    name: &str,
    required: bool,
    kind: RuntimeFactKindWire,
    max_bytes: u32,
) -> Result<(), String> {
    let declaration = request
        .runtime_facts
        .get(name)
        .ok_or_else(|| format!("runtime_facts.{name} is required"))?;
    if declaration.required != required
        || declaration.kind != kind
        || declaration.max_bytes != max_bytes
    {
        return Err(format!(
            "runtime_facts.{name} does not match the directive launch contract"
        ));
    }
    Ok(())
}
