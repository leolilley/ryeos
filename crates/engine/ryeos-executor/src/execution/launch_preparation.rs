//! Generic runtime launch-contract preparation.
//!
//! This module knows only signed declaration shapes, canonical refs, trust,
//! configuration snapshots, and bounded handler protocol values. Runtime-domain
//! key names and value schemas remain opaque.

use std::collections::{BTreeMap, BTreeSet};

use ryeos_engine::contracts::{EffectivePrincipal, ItemSpace};
use ryeos_engine::error::EngineError;
use ryeos_engine::item_resolution::ResolutionRoots;
use ryeos_engine::parsers::ParserDispatcher;
use ryeos_engine::resolution::{ResolutionOutput, TrustClass};
use ryeos_engine::runtime_registry::{
    LaunchItemSpace, LaunchPreparationDecl, RuntimeFactKind, VerifiedRuntime,
};
use ryeos_handler_protocol::{
    ItemSpaceWire, LaunchComposedViewWire, LaunchConfigSnapshotWire, LaunchDiagnosticScalarWire,
    LaunchPrepareError, LaunchPrepareErrorClass, LaunchPrepareRequest, LaunchPrepareResponse,
    LaunchPreparedItemWire, LaunchSecretOriginWire, TrustClassWire,
};
use ryeos_runtime::authorizer::{canonical_cap, AuthorizationPolicy};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dispatch_error::DispatchError;

const MAX_RUNTIME_DATA_VALUE_BYTES: usize = 1024 * 1024;
const MAX_RUNTIME_DATA_BYTES: usize = 2 * 1024 * 1024;
const MAX_RUNTIME_FACT_BYTES: usize = 64 * 1024;
const MAX_SECRET_ORIGINS: usize = 64;
const MAX_SECRET_NAMES: usize = 32;
const MAX_JSON_DEPTH: usize = 32;
const MAX_HANDLER_ERROR_CODE_BYTES: usize = 64;
const MAX_HANDLER_ERROR_MESSAGE_BYTES: usize = 512;
const MAX_HANDLER_ERROR_DETAILS: usize = 32;
const MAX_HANDLER_ERROR_DETAIL_STRING_BYTES: usize = 256;
const MAX_HANDLER_ERROR_DETAILS_BYTES: usize = 8 * 1024;
const MAX_REF_BINDINGS: usize = 32;
const MAX_REF_BINDING_NAME_BYTES: usize = 64;
const MAX_REF_BINDING_VALUE_BYTES: usize = 2_048;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefBindingLaunchRecord {
    pub canonical_ref: String,
    pub source_space: ItemSpace,
    pub effective_trust_class: TrustClass,
    pub resolution: ryeos_engine::resolution::AsLaunchedResolutionDigest,
}

#[derive(Debug, Clone)]
pub struct PreparedSecret {
    pub name: String,
    pub origin: LaunchSecretOriginWire,
}

#[derive(Debug, Clone)]
pub struct PreparedRuntimeLaunch {
    pub runtime_data: BTreeMap<String, Value>,
    pub required_secrets: Vec<PreparedSecret>,
    pub runtime_facts: BTreeMap<String, Value>,
    pub binding_records: BTreeMap<String, RefBindingLaunchRecord>,
}

pub struct PrepareRuntimeLaunchRequest<'a> {
    pub engine: &'a ryeos_engine::engine::Engine,
    pub runtime: &'a VerifiedRuntime,
    pub primary: &'a ResolutionOutput,
    pub ref_bindings: &'a BTreeMap<String, String>,
    pub roots: &'a ResolutionRoots,
    pub parsers: &'a ParserDispatcher,
    pub principal: &'a EffectivePrincipal,
}

pub fn prepare_runtime_launch(
    request: PrepareRuntimeLaunchRequest<'_>,
) -> Result<PreparedRuntimeLaunch, DispatchError> {
    validate_ref_bindings(request.ref_bindings)?;
    let contract = &request.runtime.yaml.launch_contract;
    validate_prepared_item(
        PreparedItemRole::Primary,
        &request.primary.root.resolved_ref,
        request.primary.root.source_space,
        request.primary.effective_trust_class,
        &contract.primary_allowed_kinds,
        &contract.primary_allowed_spaces,
        &contract.primary_allowed_trust,
    )?;

    for (name, declaration) in &contract.ref_bindings {
        if declaration.required && !request.ref_bindings.contains_key(name) {
            return Err(preparation_error_with_binding(
                "ref_binding_required",
                format!("required ref binding `{name}` is missing"),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
    }
    for name in request.ref_bindings.keys() {
        if !contract.ref_bindings.contains_key(name) {
            return Err(preparation_error_with_binding(
                "invalid_ref_binding",
                format!("ref binding `{name}` is not declared by the selected runtime"),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
    }

    let scopes = principal_scopes(request.principal);
    let mut binding_wires = BTreeMap::new();
    let mut binding_records = BTreeMap::new();
    for (name, raw_ref) in request.ref_bindings {
        let declaration = &contract.ref_bindings[name];
        let canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(raw_ref).map_err(|_| {
                preparation_error_with_binding(
                    "invalid_ref_binding",
                    format!("ref binding `{name}` is not a canonical ref"),
                    LaunchPrepareErrorClass::Caller,
                    Some(name.clone()),
                )
            })?;
        let required_cap = canonical_cap(&canonical.kind, &canonical.bare_id, "execute");
        ryeos_runtime::authorizer::Authorizer::new()
            .authorize(&scopes, &AuthorizationPolicy::require(&required_cap))
            .map_err(|_| {
                launch_policy_forbidden(
                    "ref_binding_unauthorized",
                    format!("ref binding `{name}` is not authorized"),
                    Some(name.clone()),
                )
            })?;
        if !declaration.allowed_kinds.contains(&canonical.kind) {
            return Err(preparation_error_with_binding(
                "ref_binding_kind_not_allowed",
                format!("binding `{name}` kind `{}` is not allowed", canonical.kind),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
        let resolution = ryeos_engine::resolution::run_resolution_pipeline(
            &canonical,
            &request.engine.kinds,
            request.parsers,
            request.roots,
            &request.engine.trust_store,
            &request.engine.composers,
        )
        .map_err(|error| map_binding_resolution_error(name, error))?;
        validate_prepared_item(
            PreparedItemRole::Binding(name),
            &resolution.root.resolved_ref,
            resolution.root.source_space,
            resolution.effective_trust_class,
            &declaration.allowed_kinds,
            &declaration.allowed_spaces,
            &declaration.allowed_trust,
        )?;
        binding_records.insert(
            name.clone(),
            RefBindingLaunchRecord {
                canonical_ref: canonical.to_string(),
                source_space: resolution.root.source_space,
                effective_trust_class: resolution.effective_trust_class,
                resolution: resolution.as_launched_digest(),
            },
        );
        binding_wires.insert(name.clone(), prepared_item_wire(&resolution)?);
    }

    let launch_config_roots = request.engine.launch_config_roots(request.roots);
    let config_inputs = ryeos_engine::launch_config::load_launch_config_snapshots(
        &contract.config_inputs,
        &launch_config_roots,
        request.parsers,
        &request.engine.kinds,
        &request.engine.trust_store,
    )
    .map_err(map_launch_config_error)?;

    let mut result = match &contract.preparation {
        LaunchPreparationDecl::None => ryeos_handler_protocol::LaunchPrepareSuccess {
            runtime_data: BTreeMap::new(),
            required_secrets: Vec::new(),
            runtime_facts: BTreeMap::new(),
        },
        LaunchPreparationDecl::Handler { config, .. } => {
            let handler_request = LaunchPrepareRequest {
                handler_config: config.clone(),
                primary: prepared_item_wire(request.primary)?,
                ref_bindings: binding_wires,
                config_inputs: config_inputs.clone(),
            };
            match request
                .engine
                .launch_preparers
                .prepare(&request.runtime.canonical_ref, handler_request)
                .map_err(map_launch_preparer_host_error)?
            {
                LaunchPrepareResponse::Success { result } => result,
                LaunchPrepareResponse::Error { error } => {
                    return Err(handler_preparation_error(error, request.ref_bindings));
                }
            }
        }
    };

    validate_result(contract, request.ref_bindings, &config_inputs, &mut result)?;
    Ok(PreparedRuntimeLaunch {
        runtime_data: result.runtime_data,
        required_secrets: result
            .required_secrets
            .into_iter()
            .map(|requirement| PreparedSecret {
                name: requirement.name,
                origin: requirement.origin,
            })
            .collect(),
        runtime_facts: result.runtime_facts,
        binding_records,
    })
}

/// Validate daemon-wide syntax and size caps for a serialized secondary
/// execution identity before authorization, forwarding, or preparation.
pub fn validate_ref_bindings(ref_bindings: &BTreeMap<String, String>) -> Result<(), DispatchError> {
    if ref_bindings.len() > MAX_REF_BINDINGS {
        return Err(preparation_error(
            "invalid_ref_binding",
            format!("ref_bindings exceeds the daemon limit of {MAX_REF_BINDINGS}"),
            LaunchPrepareErrorClass::Caller,
        ));
    }
    for (name, raw_ref) in ref_bindings {
        if !valid_ref_binding_name(name) {
            return Err(preparation_error_with_binding(
                "invalid_ref_binding",
                format!(
                    "ref binding names must be lower snake case and at most \
                     {MAX_REF_BINDING_NAME_BYTES} bytes"
                ),
                LaunchPrepareErrorClass::Caller,
                None,
            ));
        }
        if raw_ref.len() > MAX_REF_BINDING_VALUE_BYTES {
            return Err(preparation_error_with_binding(
                "invalid_ref_binding",
                format!("ref binding `{name}` exceeds {MAX_REF_BINDING_VALUE_BYTES} UTF-8 bytes"),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
        let canonical =
            ryeos_engine::canonical_ref::CanonicalRef::parse(raw_ref).map_err(|_| {
                preparation_error_with_binding(
                    "invalid_ref_binding",
                    format!("ref binding `{name}` is not a canonical ref"),
                    LaunchPrepareErrorClass::Caller,
                    Some(name.clone()),
                )
            })?;
        if canonical.to_string() != *raw_ref {
            return Err(preparation_error_with_binding(
                "invalid_ref_binding",
                format!("ref binding `{name}` is not in canonical form"),
                LaunchPrepareErrorClass::Caller,
                Some(name.clone()),
            ));
        }
    }
    Ok(())
}

fn valid_ref_binding_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_REF_BINDING_NAME_BYTES {
        return false;
    }
    let mut segments = name.split('_');
    let Some(first) = segments.next() else {
        return false;
    };
    first.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && first
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && segments.all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

#[derive(Clone, Copy)]
enum PreparedItemRole<'a> {
    Primary,
    Binding(&'a str),
}

fn validate_prepared_item(
    role: PreparedItemRole<'_>,
    canonical_ref: &str,
    source_space: ItemSpace,
    trust: TrustClass,
    allowed_kinds: &[String],
    allowed_spaces: &[LaunchItemSpace],
    allowed_trust: &[TrustClass],
) -> Result<(), DispatchError> {
    let display_name = match role {
        PreparedItemRole::Primary => "primary",
        PreparedItemRole::Binding(name) => name,
    };
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(canonical_ref)
        .map_err(|error| DispatchError::InvalidRef(canonical_ref.to_owned(), error.to_string()))?;
    if !allowed_kinds.contains(&canonical.kind) {
        let (code, binding) = match role {
            PreparedItemRole::Primary => ("invalid_primary_kind", None),
            PreparedItemRole::Binding(name) => {
                ("ref_binding_kind_not_allowed", Some(name.to_owned()))
            }
        };
        return Err(preparation_error_with_binding(
            code,
            format!("{display_name} kind `{}` is not allowed", canonical.kind),
            LaunchPrepareErrorClass::Caller,
            binding,
        ));
    }
    let space = match source_space {
        ItemSpace::Bundle => LaunchItemSpace::Bundle,
        ItemSpace::Project => LaunchItemSpace::Project,
    };
    if !allowed_spaces.contains(&space) {
        let (code, binding) = match role {
            PreparedItemRole::Primary => ("primary_space_not_allowed", None),
            PreparedItemRole::Binding(name) => {
                ("ref_binding_space_not_allowed", Some(name.to_owned()))
            }
        };
        return Err(launch_policy_forbidden(
            code,
            format!("{display_name} source space is not allowed"),
            binding,
        ));
    }
    if !allowed_trust.contains(&trust) {
        let (code, binding) = match role {
            PreparedItemRole::Primary => ("primary_untrusted", None),
            PreparedItemRole::Binding(name) => ("ref_binding_untrusted", Some(name.to_owned())),
        };
        return Err(launch_policy_forbidden(
            code,
            format!("{display_name} trust class is not allowed"),
            binding,
        ));
    }
    Ok(())
}

fn prepared_item_wire(
    resolution: &ResolutionOutput,
) -> Result<LaunchPreparedItemWire, DispatchError> {
    Ok(LaunchPreparedItemWire {
        canonical_ref: resolution.root.resolved_ref.clone(),
        source_space: item_space_wire(resolution.root.source_space),
        effective_trust_class: trust_wire(resolution.effective_trust_class),
        composed: LaunchComposedViewWire {
            composed: resolution.composed.composed.clone(),
            derived: resolution
                .composed
                .derived
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            policy_facts: resolution
                .composed
                .policy_facts
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        },
        resolution_digest: serde_json::to_value(resolution.as_launched_digest())
            .map_err(|error| DispatchError::Internal(error.into()))?,
    })
}

fn validate_result(
    contract: &ryeos_engine::runtime_registry::LaunchContractDecl,
    ref_bindings: &BTreeMap<String, String>,
    config_inputs: &BTreeMap<String, LaunchConfigSnapshotWire>,
    result: &mut ryeos_handler_protocol::LaunchPrepareSuccess,
) -> Result<(), DispatchError> {
    let expected: BTreeSet<_> = contract.required_runtime_data.iter().collect();
    let actual: BTreeSet<_> = result.runtime_data.keys().collect();
    if actual != expected {
        return Err(preparation_error("launch_preparer_runtime_data_mismatch", format!("runtime_data keys do not match signed contract: expected {expected:?}, got {actual:?}"), LaunchPrepareErrorClass::Internal));
    }
    let mut aggregate = 0usize;
    for (name, value) in &result.runtime_data {
        aggregate = aggregate.saturating_add(validate_json_value(
            name,
            value,
            MAX_RUNTIME_DATA_VALUE_BYTES,
        )?);
    }
    if aggregate > MAX_RUNTIME_DATA_BYTES {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "aggregate runtime_data exceeds daemon limit",
            LaunchPrepareErrorClass::Internal,
        ));
    }

    if result.required_secrets.len() > MAX_SECRET_ORIGINS {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "too many symbolic secret origins",
            LaunchPrepareErrorClass::Internal,
        ));
    }
    let allowed_secrets: BTreeSet<_> = contract.secret_policy.allowed_names.iter().collect();
    let mut unique_names = BTreeSet::new();
    let mut unique_origins = BTreeMap::new();
    for requirement in &result.required_secrets {
        if !allowed_secrets.contains(&requirement.name) {
            return Err(preparation_error(
                "launch_secret_not_allowed",
                format!(
                    "secret `{}` is not allowed by the signed contract",
                    requirement.name
                ),
                LaunchPrepareErrorClass::Internal,
            ));
        }
        validate_secret_origin(&requirement.origin, ref_bindings, config_inputs)?;
        let origin_value = serde_json::to_value(&requirement.origin)
            .map_err(|error| DispatchError::Internal(error.into()))?;
        let origin = lillux::canonical_json(&origin_value).map_err(|error| {
            preparation_error(
                "launch_secret_origin_invalid",
                format!("secret origin cannot be represented as canonical JSON: {error}"),
                LaunchPrepareErrorClass::Internal,
            )
        })?;
        unique_origins
            .entry((requirement.name.clone(), origin))
            .or_insert_with(|| requirement.clone());
        unique_names.insert(requirement.name.clone());
    }
    result.required_secrets = unique_origins.into_values().collect();
    if result.required_secrets.len() > MAX_SECRET_ORIGINS {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "too many deduplicated symbolic secret origins",
            LaunchPrepareErrorClass::Internal,
        ));
    }
    if unique_names.len() > MAX_SECRET_NAMES
        || unique_names.len() > usize::from(contract.secret_policy.max_requirements)
    {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "symbolic secret requirement limit exceeded",
            LaunchPrepareErrorClass::Internal,
        ));
    }

    let mut facts_bytes = 0usize;
    for (name, declaration) in &contract.runtime_facts {
        if declaration.required && !result.runtime_facts.contains_key(name) {
            return Err(preparation_error(
                "runtime_fact_missing",
                format!("required runtime fact `{name}` is missing"),
                LaunchPrepareErrorClass::Internal,
            ));
        }
    }
    for (name, value) in &result.runtime_facts {
        let declaration = contract.runtime_facts.get(name).ok_or_else(|| {
            preparation_error(
                "runtime_fact_undeclared",
                format!("runtime fact `{name}` is undeclared"),
                LaunchPrepareErrorClass::Internal,
            )
        })?;
        let kind_ok = match declaration.kind {
            RuntimeFactKind::Bool => value.is_boolean(),
            RuntimeFactKind::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            RuntimeFactKind::String => value.is_string(),
            RuntimeFactKind::Json => true,
        };
        if !kind_ok {
            return Err(preparation_error(
                "runtime_fact_type_invalid",
                format!("runtime fact `{name}` has the wrong type"),
                LaunchPrepareErrorClass::Internal,
            ));
        }
        let bytes = canonical_json_len(name, value)?;
        if bytes > declaration.max_bytes as usize {
            return Err(preparation_error(
                "runtime_fact_too_large",
                format!("runtime fact `{name}` exceeds its signed size"),
                LaunchPrepareErrorClass::Internal,
            ));
        }
        facts_bytes = facts_bytes.saturating_add(bytes);
    }
    if facts_bytes > MAX_RUNTIME_FACT_BYTES {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            "aggregate runtime facts exceed daemon limit",
            LaunchPrepareErrorClass::Internal,
        ));
    }
    Ok(())
}

fn validate_secret_origin(
    origin: &LaunchSecretOriginWire,
    ref_bindings: &BTreeMap<String, String>,
    config_inputs: &BTreeMap<String, LaunchConfigSnapshotWire>,
) -> Result<(), DispatchError> {
    match origin {
        LaunchSecretOriginWire::Binding { name } if ref_bindings.contains_key(name) => Ok(()),
        LaunchSecretOriginWire::Binding { name } => Err(preparation_error(
            "launch_secret_origin_invalid",
            format!("unknown binding origin `{name}`"),
            LaunchPrepareErrorClass::Internal,
        )),
        LaunchSecretOriginWire::ConfigInput {
            name,
            canonical_id,
            value_digest,
        } => {
            let valid = match config_inputs.get(name) {
                Some(LaunchConfigSnapshotWire::Item {
                    present: true,
                    value_digest: Some(actual),
                    contributors,
                    ..
                }) => {
                    actual == value_digest
                        && contributors
                            .iter()
                            .any(|source| source.canonical_id == *canonical_id)
                }
                Some(LaunchConfigSnapshotWire::Catalog { entries }) => entries
                    .get(canonical_id)
                    .is_some_and(|entry| entry.value_digest == *value_digest),
                _ => false,
            };
            if valid {
                Ok(())
            } else {
                Err(preparation_error("launch_secret_origin_invalid", format!("config origin `{name}/{canonical_id}` does not match its verified snapshot"), LaunchPrepareErrorClass::Internal))
            }
        }
    }
}

fn validate_json_value(
    name: &str,
    value: &Value,
    max_bytes: usize,
) -> Result<usize, DispatchError> {
    if json_depth(value) > MAX_JSON_DEPTH {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            format!("`{name}` exceeds JSON depth limit"),
            LaunchPrepareErrorClass::Internal,
        ));
    }
    let bytes = canonical_json_len(name, value)?;
    if bytes > max_bytes {
        return Err(preparation_error(
            "launch_preparer_limit_exceeded",
            format!("`{name}` exceeds byte limit"),
            LaunchPrepareErrorClass::Internal,
        ));
    }
    Ok(bytes)
}

fn canonical_json_len(name: &str, value: &Value) -> Result<usize, DispatchError> {
    lillux::canonical_json(value)
        .map(|canonical| canonical.len())
        .map_err(|error| {
            preparation_error(
                "launch_preparer_value_not_canonical",
                format!("`{name}` cannot be represented as canonical JSON: {error}"),
                LaunchPrepareErrorClass::Internal,
            )
        })
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

fn principal_scopes(principal: &EffectivePrincipal) -> Vec<String> {
    match principal {
        EffectivePrincipal::Local(principal) => principal.scopes.clone(),
        EffectivePrincipal::Delegated(principal) => principal.delegated_scopes.clone(),
    }
}

fn item_space_wire(space: ItemSpace) -> ItemSpaceWire {
    match space {
        ItemSpace::Bundle => ItemSpaceWire::Bundle,
        ItemSpace::Project => ItemSpaceWire::Project,
    }
}

fn trust_wire(trust: TrustClass) -> TrustClassWire {
    match trust {
        TrustClass::TrustedBundle => TrustClassWire::TrustedBundle,
        TrustClass::TrustedProject => TrustClassWire::TrustedProject,
        TrustClass::UntrustedProject => TrustClassWire::UntrustedProject,
        TrustClass::Unsigned => TrustClassWire::Unsigned,
    }
}

fn map_binding_resolution_error(
    binding: &str,
    error: ryeos_engine::resolution::ResolutionError,
) -> DispatchError {
    use ryeos_engine::resolution::ResolutionError;

    let detail = error.to_string();
    match error {
        ResolutionError::MissingItem { .. } => DispatchError::LaunchResourceNotFound {
            code: "ref_binding_not_found".to_owned(),
            message: format!("ref binding `{binding}` was not found"),
            binding: Some(binding.to_owned()),
        },
        ResolutionError::CycleDetected { .. }
        | ResolutionError::MaxDepthExceeded { .. }
        | ResolutionError::AliasMaxDepthExceeded { .. }
        | ResolutionError::AliasCycle { .. }
        | ResolutionError::UnknownAlias { .. }
        | ResolutionError::IntegrityFailure { .. }
        | ResolutionError::MetadataAnchoringFailed { .. }
        | ResolutionError::KindNotExecutable { .. }
        | ResolutionError::ComposedValueContractViolation { .. } => preparation_error_with_binding(
            "ref_binding_resolution_failed",
            format!("binding `{binding}` has an invalid definition: {detail}"),
            LaunchPrepareErrorClass::Configuration,
            Some(binding.to_owned()),
        ),
        ResolutionError::StepFailed { class, .. } => {
            use ryeos_engine::resolution::ResolutionFailureClass;

            let classification = match class {
                ResolutionFailureClass::InvalidDefinition => LaunchPrepareErrorClass::Configuration,
                ResolutionFailureClass::DependencyUnavailable => {
                    return host_preparation_error_with_binding(
                        "ref_binding_resolution_failed",
                        format!(
                            "binding `{binding}` resolution dependency is unavailable: {detail}"
                        ),
                        "unavailable",
                        Some(binding.to_owned()),
                    );
                }
                ResolutionFailureClass::InternalInvariant => LaunchPrepareErrorClass::Internal,
            };
            preparation_error_with_binding(
                "ref_binding_resolution_failed",
                format!("binding `{binding}` resolution failed: {detail}"),
                classification,
                Some(binding.to_owned()),
            )
        }
    }
}

fn map_launch_preparer_host_error(error: EngineError) -> DispatchError {
    match error {
        EngineError::LaunchPreparerUnavailable { detail, .. } => {
            host_preparation_error("launch_preparer_unavailable", detail, "unavailable")
        }
        EngineError::LaunchPreparerLimitExceeded { detail, .. } => {
            host_preparation_error("launch_preparer_limit_exceeded", detail, "internal")
        }
        EngineError::LaunchPreparerProtocolInvalid { detail, .. } => {
            host_preparation_error("launch_preparer_protocol_invalid", detail, "internal")
        }
        other => host_preparation_error(
            "launch_preparer_protocol_invalid",
            other.to_string(),
            "internal",
        ),
    }
}

fn map_launch_config_error(error: EngineError) -> DispatchError {
    match error {
        EngineError::LaunchConfigMissing { input, detail } => preparation_error(
            "launch_config_missing",
            format!("launch config input `{input}` is missing: {detail}"),
            LaunchPrepareErrorClass::Configuration,
        ),
        EngineError::LaunchConfigPolicyDenied {
            code,
            input,
            detail,
        } => launch_policy_forbidden(
            code,
            format!("launch config input `{input}` is forbidden: {detail}"),
            None,
        ),
        other => preparation_error(
            "launch_config_invalid",
            other.to_string(),
            LaunchPrepareErrorClass::Configuration,
        ),
    }
}

fn handler_preparation_error(
    error: LaunchPrepareError,
    ref_bindings: &BTreeMap<String, String>,
) -> DispatchError {
    if let Err(reason) = validate_handler_error(&error, ref_bindings) {
        return host_preparation_error("launch_preparer_protocol_invalid", reason, "internal");
    }
    let classification = match error.classification {
        LaunchPrepareErrorClass::Caller => "caller",
        LaunchPrepareErrorClass::Configuration => "configuration",
        LaunchPrepareErrorClass::Internal => "internal",
    };
    DispatchError::LaunchPreparationFailed {
        code: error.code,
        message: error.message,
        classification: classification.to_owned(),
        binding: error.binding,
        details: Box::new(error.details),
    }
}

fn validate_handler_error(
    error: &LaunchPrepareError,
    ref_bindings: &BTreeMap<String, String>,
) -> Result<(), String> {
    if !valid_launch_name(&error.code, MAX_HANDLER_ERROR_CODE_BYTES) {
        return Err("launch-preparer error code is not a bounded lower-snake-case name".to_owned());
    }
    if error.message.len() > MAX_HANDLER_ERROR_MESSAGE_BYTES
        || error.message.contains('\n')
        || error.message.contains('\r')
    {
        return Err("launch-preparer error message is not a bounded single line".to_owned());
    }
    if let Some(binding) = &error.binding {
        if !ref_bindings.contains_key(binding) {
            return Err(format!(
                "launch-preparer error names unknown binding `{binding}`"
            ));
        }
    }
    if error.details.len() > MAX_HANDLER_ERROR_DETAILS {
        return Err("launch-preparer error details exceed the key limit".to_owned());
    }
    for (key, value) in &error.details {
        if !valid_launch_name(key, MAX_HANDLER_ERROR_CODE_BYTES) {
            return Err(format!(
                "launch-preparer error detail key `{key}` is invalid"
            ));
        }
        if let LaunchDiagnosticScalarWire::String(value) = value {
            if value.len() > MAX_HANDLER_ERROR_DETAIL_STRING_BYTES {
                return Err(format!(
                    "launch-preparer error detail `{key}` exceeds the string limit"
                ));
            }
        }
    }
    let value = serde_json::to_value(&error.details)
        .map_err(|encode| format!("encode launch-preparer error details: {encode}"))?;
    let canonical = lillux::canonical_json(&value)
        .map_err(|encode| format!("canonicalize launch-preparer error details: {encode}"))?;
    if canonical.len() > MAX_HANDLER_ERROR_DETAILS_BYTES {
        return Err("launch-preparer error details exceed the aggregate byte limit".to_owned());
    }
    Ok(())
}

fn valid_launch_name(name: &str, max_bytes: usize) -> bool {
    !name.is_empty()
        && name.len() <= max_bytes
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !name.ends_with('_')
        && !name.contains("__")
}

fn preparation_error(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: LaunchPrepareErrorClass,
) -> DispatchError {
    preparation_error_with_binding(code, message, classification, None)
}

fn preparation_error_with_binding(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: LaunchPrepareErrorClass,
    binding: Option<String>,
) -> DispatchError {
    let classification = match classification {
        LaunchPrepareErrorClass::Caller => "caller",
        LaunchPrepareErrorClass::Configuration => "configuration",
        LaunchPrepareErrorClass::Internal => "internal",
    };
    DispatchError::LaunchPreparationFailed {
        code: code.into(),
        message: message.into(),
        classification: classification.to_owned(),
        binding,
        details: Box::new(BTreeMap::new()),
    }
}

fn host_preparation_error(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: &'static str,
) -> DispatchError {
    host_preparation_error_with_binding(code, message, classification, None)
}

fn host_preparation_error_with_binding(
    code: impl Into<String>,
    message: impl Into<String>,
    classification: &'static str,
    binding: Option<String>,
) -> DispatchError {
    DispatchError::LaunchPreparationFailed {
        code: code.into(),
        message: message.into(),
        classification: classification.to_owned(),
        binding,
        details: Box::new(BTreeMap::new()),
    }
}

fn launch_policy_forbidden(
    code: impl Into<String>,
    message: impl Into<String>,
    binding: Option<String>,
) -> DispatchError {
    DispatchError::LaunchPolicyForbidden {
        code: code.into(),
        message: message.into(),
        binding,
    }
}
