//! Dormant v2 effect-answer and record contracts.
//!
//! These types are deliberately not the active CAS readers yet. They let the
//! answer normalizers, exact callee/provider identity builders, and immutable
//! replay index land and prove themselves before one clean v1 -> v2
//! activation. An answer contains only behavior visible to the caller;
//! occurrence, cost, thread, and transport evidence live in the retained first
//! observation.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EFFECT_RECORD_SCHEMA_V2: u32 = 2;
pub const GRAPH_EFFECT_KEY_SCHEMA_V2: &str = "ryeos.graph_node_effect.key.v2";
pub const PROVIDER_EFFECT_KEY_SCHEMA_V2: &str = "ryeos.provider_call_effect.key.v2";
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;
const MAX_WARNING_BYTES: usize = 16 * 1024;
const MAX_WARNINGS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableEffectClass {
    Recorded,
    Sealed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "envelope", rename_all = "snake_case", deny_unknown_fields)]
pub enum GraphNodeEffectAnswerV2 {
    /// A leaf returned an authored value with no daemon envelope markers.
    Bare { result: Value },
    /// A successful subprocess/tool terminator. Operational exit spelling,
    /// cost, thread snapshot, and timing are intentionally absent.
    Subprocess { result: Value },
    /// A successful managed/native terminator. Only authored outputs and
    /// replay-safe warnings survive normalization.
    Native {
        result: Value,
        outputs: Value,
        warnings: Vec<String>,
    },
}

impl GraphNodeEffectAnswerV2 {
    pub fn validate(&self) -> anyhow::Result<()> {
        let warnings: &[String] = match self {
            Self::Native { warnings, .. } => warnings,
            Self::Bare { .. } | Self::Subprocess { .. } => &[],
        };
        validate_warnings(warnings)?;
        validate_bounded_value("graph effect answer", &serde_json::to_value(self)?)
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_digest(self)
    }

    /// Synthesize a fresh callback leaf envelope for replay.
    ///
    /// Bare values are wrapped in the canonical subprocess-success envelope so
    /// replay provenance never mutates the authored result object.
    pub fn replay_leaf_envelope(&self, record_hash: &str) -> anyhow::Result<Value> {
        require_hex64("graph replay record_hash", record_hash)?;
        self.validate()?;
        Ok(match self {
            Self::Bare { result } | Self::Subprocess { result } => serde_json::json!({
                "outcome_code": null,
                "result": result,
                "error": null,
                "artifacts": [],
                "replayed_from": record_hash,
            }),
            Self::Native {
                result,
                outputs,
                warnings,
            } => serde_json::json!({
                "success": true,
                "status": "completed",
                "result": result,
                "outputs": outputs,
                "warnings": warnings,
                "cost": null,
                "replayed_from": record_hash,
            }),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedProviderToolCallV2 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedProviderMessageV2 {
    pub role: String,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Option<Vec<RecordedProviderToolCallV2>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl RecordedProviderMessageV2 {
    fn validate(&self) -> anyhow::Result<()> {
        super::validate_trimmed_control_free("recorded provider message role", &self.role, false)?;
        if self.role != "assistant" {
            bail!("recorded provider answer role must be assistant");
        }
        if self.tool_call_id.is_some() {
            bail!("recorded assistant provider answer cannot carry tool_call_id");
        }
        if let Some(calls) = &self.tool_calls {
            if calls.len() > 4_096 {
                bail!("recorded provider answer has too many tool calls");
            }
            for call in calls {
                super::validate_trimmed_control_free(
                    "recorded provider tool name",
                    &call.name,
                    false,
                )?;
                if let Some(id) = &call.id {
                    super::validate_trimmed_control_free("recorded provider tool id", id, false)?;
                }
            }
        }
        if let Some(reasoning) = &self.reasoning_content
            && reasoning.len() > MAX_ANSWER_BYTES
        {
            bail!("recorded provider reasoning exceeds the answer bound");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallEffectAnswerV2 {
    pub message: RecordedProviderMessageV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

impl ProviderCallEffectAnswerV2 {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.message.validate()?;
        if let Some(reason) = &self.finish_reason {
            super::validate_trimmed_control_free("recorded provider finish_reason", reason, false)?;
        }
        validate_bounded_value("provider effect answer", &serde_json::to_value(self)?)
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_digest(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNodeEffectIdentityV2 {
    pub caller_effective_definition_digest: String,
    pub root_ref: String,
    pub graph_id: String,
    pub node: String,
    pub action_digest: String,
    pub dispatch_subject_ref: String,
    pub dispatch_subject_effective_definition_digest: String,
    pub dispatch_subject_capsule_hash: String,
    pub admitted_effect_class: DurableEffectClass,
}

impl GraphNodeEffectIdentityV2 {
    pub fn validate(&self) -> anyhow::Result<()> {
        for (field, digest) in [
            (
                "caller_effective_definition_digest",
                &self.caller_effective_definition_digest,
            ),
            ("action_digest", &self.action_digest),
            (
                "dispatch_subject_effective_definition_digest",
                &self.dispatch_subject_effective_definition_digest,
            ),
            (
                "dispatch_subject_capsule_hash",
                &self.dispatch_subject_capsule_hash,
            ),
        ] {
            require_hex64(field, digest)?;
        }
        for (field, value) in [
            ("root_ref", &self.root_ref),
            ("graph_id", &self.graph_id),
            ("node", &self.node),
            ("dispatch_subject_ref", &self.dispatch_subject_ref),
        ] {
            super::validate_trimmed_control_free(
                &format!("graph effect identity {field}"),
                value,
                false,
            )?;
        }
        Ok(())
    }

    pub fn cache_key(&self) -> anyhow::Result<String> {
        self.validate()?;
        let seed = serde_json::json!({
            "schema": GRAPH_EFFECT_KEY_SCHEMA_V2,
            "caller_effective_definition_digest": self.caller_effective_definition_digest,
            "root_ref": self.root_ref,
            "graph_id": self.graph_id,
            "node": self.node,
            "action_digest": self.action_digest,
            "dispatch_subject_ref": self.dispatch_subject_ref,
            "dispatch_subject_effective_definition_digest": self.dispatch_subject_effective_definition_digest,
            "dispatch_subject_capsule_hash": self.dispatch_subject_capsule_hash,
            "admitted_effect_class": self.admitted_effect_class,
        });
        canonical_value_digest(&seed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicHeaderCoordinateV2 {
    pub name: String,
    pub value_digest: String,
}

/// Bounded request projection a transport may report without naming any
/// daemon-owned identity or credential value. Public and credential-bearing
/// headers are different types so collision checks cannot depend on a list of
/// magic names such as `authorization`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedProviderRequestProjectionV2 {
    pub public_headers: Vec<PublicHeaderCoordinateV2>,
    pub credential_header_names: Vec<String>,
    pub body_sha256: String,
    pub requested_output_ceiling: u64,
}

impl PreparedProviderRequestProjectionV2 {
    pub fn new(
        public_headers: impl IntoIterator<Item = (String, String)>,
        credential_header_names: impl IntoIterator<Item = String>,
        body_sha256: String,
        requested_output_ceiling: u64,
    ) -> anyhow::Result<Self> {
        require_hex64("provider request body_sha256", &body_sha256)?;
        if requested_output_ceiling == 0 {
            bail!("provider request output ceiling must be positive");
        }

        let (public_headers, credential_header_names) =
            provider_header_projection_v2(public_headers, credential_header_names)?;

        Ok(Self {
            public_headers,
            credential_header_names,
            body_sha256,
            requested_output_ceiling,
        })
    }
}

pub fn provider_header_projection_v2(
    public_headers: impl IntoIterator<Item = (String, String)>,
    credential_header_names: impl IntoIterator<Item = String>,
) -> anyhow::Result<(Vec<PublicHeaderCoordinateV2>, Vec<String>)> {
    let mut credential_header_names = credential_header_names
        .into_iter()
        .map(|name| normalize_http_header_name(&name))
        .collect::<anyhow::Result<Vec<_>>>()?;
    credential_header_names.sort();
    if credential_header_names
        .windows(2)
        .any(|names| names[0] == names[1])
    {
        bail!("provider credential header names must be unique");
    }

    let mut public_headers = public_headers
        .into_iter()
        .map(|(name, value)| {
            let name = normalize_http_header_name(&name)?;
            if value.contains(['\r', '\n']) {
                bail!("provider public header `{name}` contains a line break");
            }
            Ok(PublicHeaderCoordinateV2 {
                name,
                value_digest: lillux::sha256_hex(value.as_bytes()),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    public_headers.sort_by(|left, right| left.name.cmp(&right.name));
    if public_headers
        .windows(2)
        .any(|headers| headers[0].name == headers[1].name)
    {
        bail!("provider public header names must be unique");
    }
    if let Some(collision) = public_headers
        .iter()
        .find(|header| credential_header_names.binary_search(&header.name).is_ok())
    {
        bail!(
            "provider header `{}` is declared as both public and credential-bearing",
            collision.name
        );
    }

    Ok((public_headers, credential_header_names))
}

fn normalize_http_header_name(value: &str) -> anyhow::Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty()
        || !normalized.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
    {
        bail!("provider header name `{value}` is not a valid HTTP field name");
    }
    Ok(normalized)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequestAuthorityV2 {
    pub outer_effective_definition_digest: String,
    pub provider_family: String,
    pub provider_config_hash: String,
    pub provider_config_value_digest: String,
    pub provider_id: String,
    pub profile_id: Option<String>,
    pub model_name: String,
    pub credential_binding_hmac: String,
    pub credential_authority_generation: String,
    pub authority_digest: String,
    pub admitted_effect_class: DurableEffectClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderTransportCoordinateV2 {
    RemoteHttp {
        method: String,
        url: String,
    },
    AdmittedLocalWorker {
        worker_ref: String,
        effective_definition_digest: String,
        capsule_hash: String,
        execution_realization_hash: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRequestCoordinateV2 {
    pub outer_effective_definition_digest: String,
    pub transport: ProviderTransportCoordinateV2,
    pub provider_family: String,
    pub provider_config_hash: String,
    pub provider_config_value_digest: String,
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub model_name: String,
    pub public_headers: Vec<PublicHeaderCoordinateV2>,
    pub credential_header_names: Vec<String>,
    pub body_sha256: String,
    pub requested_output_ceiling: u64,
    pub credential_binding_hmac: String,
    pub credential_authority_generation: String,
    pub authority_digest: String,
    pub admitted_effect_class: DurableEffectClass,
}

impl ProviderRequestCoordinateV2 {
    pub fn build(
        authority: ProviderRequestAuthorityV2,
        transport: ProviderTransportCoordinateV2,
        request: PreparedProviderRequestProjectionV2,
    ) -> anyhow::Result<Self> {
        let transport = match transport {
            ProviderTransportCoordinateV2::RemoteHttp { method, url } => {
                let method = method.trim().to_ascii_uppercase();
                if method.is_empty()
                    || !method.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric()
                            || matches!(
                                byte,
                                b'!' | b'#'
                                    | b'$'
                                    | b'%'
                                    | b'&'
                                    | b'\''
                                    | b'*'
                                    | b'+'
                                    | b'-'
                                    | b'.'
                                    | b'^'
                                    | b'_'
                                    | b'`'
                                    | b'|'
                                    | b'~'
                            )
                    })
                {
                    bail!("provider remote method is not a valid HTTP token");
                }
                let parsed = url::Url::parse(&url)
                    .map_err(|error| anyhow::anyhow!("invalid provider remote URL: {error}"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("provider remote URL must use http or https");
                }
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    bail!("provider remote URL cannot contain user information");
                }
                if parsed.fragment().is_some() {
                    bail!("provider remote URL cannot contain a fragment");
                }
                ProviderTransportCoordinateV2::RemoteHttp {
                    method,
                    url: parsed.to_string(),
                }
            }
            local @ ProviderTransportCoordinateV2::AdmittedLocalWorker { .. } => local,
        };
        let coordinate = Self {
            outer_effective_definition_digest: authority.outer_effective_definition_digest,
            transport,
            provider_family: authority.provider_family,
            provider_config_hash: authority.provider_config_hash,
            provider_config_value_digest: authority.provider_config_value_digest,
            provider_id: authority.provider_id,
            profile_id: authority.profile_id,
            model_name: authority.model_name,
            public_headers: request.public_headers,
            credential_header_names: request.credential_header_names,
            body_sha256: request.body_sha256,
            requested_output_ceiling: request.requested_output_ceiling,
            credential_binding_hmac: authority.credential_binding_hmac,
            credential_authority_generation: authority.credential_authority_generation,
            authority_digest: authority.authority_digest,
            admitted_effect_class: authority.admitted_effect_class,
        };
        coordinate.validate()?;
        Ok(coordinate)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for (field, digest) in [
            (
                "outer_effective_definition_digest",
                &self.outer_effective_definition_digest,
            ),
            (
                "provider_config_value_digest",
                &self.provider_config_value_digest,
            ),
            ("body_sha256", &self.body_sha256),
            ("credential_binding_hmac", &self.credential_binding_hmac),
            ("authority_digest", &self.authority_digest),
        ] {
            require_hex64(field, digest)?;
        }
        if self.requested_output_ceiling == 0 {
            bail!("provider request coordinate output ceiling must be positive");
        }
        for (field, value) in [
            ("provider_family", &self.provider_family),
            ("provider_config_hash", &self.provider_config_hash),
            ("provider_id", &self.provider_id),
            ("model_name", &self.model_name),
            (
                "credential_authority_generation",
                &self.credential_authority_generation,
            ),
        ] {
            super::validate_trimmed_control_free(
                &format!("provider request coordinate {field}"),
                value,
                false,
            )?;
        }
        if let Some(profile) = &self.profile_id {
            super::validate_trimmed_control_free(
                "provider request coordinate profile_id",
                profile,
                false,
            )?;
        }
        let mut previous: Option<&str> = None;
        for header in &self.public_headers {
            super::validate_trimmed_control_free(
                "provider public header name",
                &header.name,
                false,
            )?;
            require_hex64("provider public header value_digest", &header.value_digest)?;
            if header.name.bytes().any(|byte| byte.is_ascii_uppercase()) {
                bail!("provider public header names must be lowercase");
            }
            if previous.is_some_and(|prior| prior >= header.name.as_str()) {
                bail!("provider public header coordinates must be sorted and unique");
            }
            previous = Some(&header.name);
        }
        let mut previous: Option<&str> = None;
        for header in &self.credential_header_names {
            if normalize_http_header_name(header)? != *header {
                bail!("provider credential header names must be normalized lowercase");
            }
            if previous.is_some_and(|prior| prior >= header.as_str()) {
                bail!("provider credential header names must be sorted and unique");
            }
            if self
                .public_headers
                .binary_search_by(|candidate| candidate.name.cmp(header))
                .is_ok()
            {
                bail!("provider public and credential header names collide");
            }
            previous = Some(header);
        }
        match &self.transport {
            ProviderTransportCoordinateV2::RemoteHttp { method, url } => {
                super::validate_trimmed_control_free("provider remote method", method, false)?;
                super::validate_trimmed_control_free("provider remote URL", url, false)?;
                if method.bytes().any(|byte| byte.is_ascii_lowercase()) {
                    bail!("provider remote method must be normalized uppercase ASCII");
                }
                let normalized = url::Url::parse(url)
                    .map_err(|error| anyhow::anyhow!("invalid provider remote URL: {error}"))?;
                if normalized.as_str() != url {
                    bail!("provider remote URL must be canonical");
                }
                if !matches!(normalized.scheme(), "http" | "https")
                    || !normalized.username().is_empty()
                    || normalized.password().is_some()
                    || normalized.fragment().is_some()
                {
                    bail!("provider remote URL violates remote transport policy");
                }
                if self.admitted_effect_class == DurableEffectClass::Sealed {
                    bail!("remote provider transport cannot claim sealed effect class");
                }
            }
            ProviderTransportCoordinateV2::AdmittedLocalWorker {
                worker_ref,
                effective_definition_digest,
                capsule_hash,
                execution_realization_hash,
            } => {
                super::validate_trimmed_control_free(
                    "admitted local worker ref",
                    worker_ref,
                    false,
                )?;
                require_hex64(
                    "admitted local worker effective_definition_digest",
                    effective_definition_digest,
                )?;
                require_hex64("admitted local worker capsule_hash", capsule_hash)?;
                require_hex64(
                    "admitted local worker execution_realization_hash",
                    execution_realization_hash,
                )?;
            }
        }
        Ok(())
    }

    pub fn cache_key(&self) -> anyhow::Result<String> {
        self.validate()?;
        let seed = serde_json::json!({
            "schema": PROVIDER_EFFECT_KEY_SCHEMA_V2,
            "coordinate": self,
        });
        canonical_value_digest(&seed)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphEffectFirstObservationV2 {
    pub produced_by_thread: String,
    pub response_digest: String,
    pub observed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_identity_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_identity_attestation_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_execution_realization_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_execution_realization_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderObservationClassV2 {
    RuntimeTransportObserved,
    DaemonWorkerObserved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEffectFirstObservationV2 {
    pub produced_by_thread: String,
    pub attempt_id: String,
    pub response_digest: String,
    pub observed_at: String,
    pub observation_class: ProviderObservationClassV2,
    pub provider_accounting: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_identity_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_identity_attestation_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_execution_realization_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_execution_realization_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphNodeEffectRecordV2 {
    pub schema: u32,
    pub kind: String,
    pub cache_key: String,
    pub identity: GraphNodeEffectIdentityV2,
    pub answer_digest: String,
    pub answer: GraphNodeEffectAnswerV2,
    pub first_observation: GraphEffectFirstObservationV2,
}

impl GraphNodeEffectRecordV2 {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_record_header(
            self.schema,
            &self.kind,
            super::GRAPH_NODE_EFFECT_RECORD_KIND,
            &self.cache_key,
        )?;
        if self.identity.cache_key()? != self.cache_key {
            bail!("graph effect v2 cache key contradicts its identity");
        }
        if self.answer.digest()? != self.answer_digest {
            bail!("graph effect v2 answer digest contradicts its answer");
        }
        validate_graph_observation(&self.first_observation)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallEffectRecordV2 {
    pub schema: u32,
    pub kind: String,
    pub cache_key: String,
    pub coordinate: ProviderRequestCoordinateV2,
    pub answer_digest: String,
    pub answer: ProviderCallEffectAnswerV2,
    pub first_observation: ProviderEffectFirstObservationV2,
}

impl ProviderCallEffectRecordV2 {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_record_header(
            self.schema,
            &self.kind,
            super::PROVIDER_CALL_EFFECT_RECORD_KIND,
            &self.cache_key,
        )?;
        if self.coordinate.cache_key()? != self.cache_key {
            bail!("provider effect v2 cache key contradicts its request coordinate");
        }
        if self.answer.digest()? != self.answer_digest {
            bail!("provider effect v2 answer digest contradicts its answer");
        }
        validate_provider_observation(&self.coordinate, &self.first_observation)?;
        Ok(())
    }
}

fn validate_record_header(
    schema: u32,
    kind: &str,
    expected_kind: &str,
    cache_key: &str,
) -> anyhow::Result<()> {
    if schema != EFFECT_RECORD_SCHEMA_V2 {
        bail!("effect v2 record has schema {schema}, expected {EFFECT_RECORD_SCHEMA_V2}");
    }
    if kind != expected_kind {
        bail!("effect v2 record kind is {kind}, expected {expected_kind}");
    }
    require_hex64("effect v2 cache_key", cache_key)
}

fn validate_graph_observation(observation: &GraphEffectFirstObservationV2) -> anyhow::Result<()> {
    validate_observation_common(
        &observation.produced_by_thread,
        &observation.response_digest,
        &observation.observed_at,
        observation.execution_identity_digest.as_deref(),
        observation.execution_identity_attestation_hash.as_deref(),
        observation.admitted_execution_realization_hash.as_deref(),
        observation.observed_execution_realization_hash.as_deref(),
    )
}

fn validate_provider_observation(
    coordinate: &ProviderRequestCoordinateV2,
    observation: &ProviderEffectFirstObservationV2,
) -> anyhow::Result<()> {
    validate_observation_common(
        &observation.produced_by_thread,
        &observation.response_digest,
        &observation.observed_at,
        observation.execution_identity_digest.as_deref(),
        observation.execution_identity_attestation_hash.as_deref(),
        observation.admitted_execution_realization_hash.as_deref(),
        observation.observed_execution_realization_hash.as_deref(),
    )?;
    super::validate_trimmed_control_free(
        "provider effect observation attempt_id",
        &observation.attempt_id,
        false,
    )?;
    let expected = match coordinate.transport {
        ProviderTransportCoordinateV2::RemoteHttp { .. } => {
            ProviderObservationClassV2::RuntimeTransportObserved
        }
        ProviderTransportCoordinateV2::AdmittedLocalWorker { .. } => {
            ProviderObservationClassV2::DaemonWorkerObserved
        }
    };
    if observation.observation_class != expected {
        bail!("provider observation class contradicts its transport");
    }
    validate_bounded_value(
        "provider accounting observation",
        &observation.provider_accounting,
    )
}

#[allow(clippy::too_many_arguments)]
fn validate_observation_common(
    produced_by_thread: &str,
    response_digest: &str,
    observed_at: &str,
    execution_identity_digest: Option<&str>,
    execution_identity_attestation_hash: Option<&str>,
    admitted_execution_realization_hash: Option<&str>,
    observed_execution_realization_hash: Option<&str>,
) -> anyhow::Result<()> {
    super::validate_trimmed_control_free(
        "effect observation produced_by_thread",
        produced_by_thread,
        false,
    )?;
    require_hex64("effect observation response_digest", response_digest)?;
    super::parse_canonical_timestamp(observed_at)
        .context("effect observation has a non-canonical observed_at")?;
    for (field, digest) in [
        ("execution_identity_digest", execution_identity_digest),
        (
            "execution_identity_attestation_hash",
            execution_identity_attestation_hash,
        ),
        (
            "admitted_execution_realization_hash",
            admitted_execution_realization_hash,
        ),
        (
            "observed_execution_realization_hash",
            observed_execution_realization_hash,
        ),
    ] {
        if let Some(digest) = digest {
            require_hex64(field, digest)?;
        }
    }
    Ok(())
}

fn validate_warnings(warnings: &[String]) -> anyhow::Result<()> {
    if warnings.len() > MAX_WARNINGS {
        bail!("effect answer has too many warnings");
    }
    for warning in warnings {
        if warning.len() > MAX_WARNING_BYTES {
            bail!("effect answer warning exceeds {MAX_WARNING_BYTES} bytes");
        }
        super::validate_trimmed_control_free("effect answer warning", warning, false)?;
    }
    Ok(())
}

fn validate_bounded_value(label: &str, value: &Value) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(value)?.len();
    if bytes > MAX_ANSWER_BYTES {
        bail!("{label} is {bytes} bytes; maximum is {MAX_ANSWER_BYTES}");
    }
    Ok(())
}

fn canonical_digest(value: &impl Serialize) -> anyhow::Result<String> {
    canonical_value_digest(&serde_json::to_value(value)?)
}

pub fn canonical_value_digest(value: &Value) -> anyhow::Result<String> {
    let canonical = lillux::canonical_json(value)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

fn require_hex64(field: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("{field} must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_identity() -> GraphNodeEffectIdentityV2 {
        GraphNodeEffectIdentityV2 {
            caller_effective_definition_digest: "11".repeat(32),
            root_ref: "graph:arc/solve".to_string(),
            graph_id: "arc/solve".to_string(),
            node: "classify".to_string(),
            action_digest: "22".repeat(32),
            dispatch_subject_ref: "tool:arc/classify".to_string(),
            dispatch_subject_effective_definition_digest: "33".repeat(32),
            dispatch_subject_capsule_hash: "44".repeat(32),
            admitted_effect_class: DurableEffectClass::Recorded,
        }
    }

    #[test]
    fn graph_key_commits_to_the_exact_callee_and_ceiling() {
        let base = graph_identity();
        let mut changed_callee = base.clone();
        changed_callee.dispatch_subject_effective_definition_digest = "55".repeat(32);
        let mut changed_class = base.clone();
        changed_class.admitted_effect_class = DurableEffectClass::Sealed;
        assert_ne!(
            base.cache_key().unwrap(),
            changed_callee.cache_key().unwrap()
        );
        assert_ne!(
            base.cache_key().unwrap(),
            changed_class.cache_key().unwrap()
        );
    }

    #[test]
    fn answer_digest_excludes_observation_and_replay_keeps_authored_result_exact() {
        let answer = GraphNodeEffectAnswerV2::Bare {
            result: serde_json::json!({"ok": true}),
        };
        let digest = answer.digest().unwrap();
        let replay = answer.replay_leaf_envelope(&"aa".repeat(32)).unwrap();
        assert_eq!(replay["result"], serde_json::json!({"ok": true}));
        assert_eq!(answer.digest().unwrap(), digest);
        assert_eq!(replay["replayed_from"], "aa".repeat(32));
    }

    #[test]
    fn provider_coordinate_moves_for_public_behavior_and_refuses_remote_sealed() {
        let authority = ProviderRequestAuthorityV2 {
            outer_effective_definition_digest: "11".repeat(32),
            provider_family: "openai_compatible".to_string(),
            provider_config_hash: "resolved-config-hash".to_string(),
            provider_config_value_digest: "22".repeat(32),
            provider_id: "example".to_string(),
            profile_id: None,
            model_name: "model".to_string(),
            credential_binding_hmac: "55".repeat(32),
            credential_authority_generation: "credential-generation-1".to_string(),
            authority_digest: "66".repeat(32),
            admitted_effect_class: DurableEffectClass::Recorded,
        };
        let request = PreparedProviderRequestProjectionV2::new(
            [("Content-Type".to_string(), "application/json".to_string())],
            ["Authorization".to_string()],
            "44".repeat(32),
            1024,
        )
        .unwrap();
        let mut coordinate = ProviderRequestCoordinateV2::build(
            authority,
            ProviderTransportCoordinateV2::RemoteHttp {
                method: "post".to_string(),
                url: "https://example.invalid:443/v1/chat".to_string(),
            },
            request,
        )
        .unwrap();
        assert!(matches!(
            &coordinate.transport,
            ProviderTransportCoordinateV2::RemoteHttp { method, url }
                if method == "POST" && url == "https://example.invalid/v1/chat"
        ));
        assert_eq!(coordinate.public_headers[0].name, "content-type");
        assert_eq!(coordinate.credential_header_names, ["authorization"]);
        let original = coordinate.cache_key().unwrap();
        coordinate.model_name = "other".to_string();
        assert_ne!(coordinate.cache_key().unwrap(), original);
        coordinate.admitted_effect_class = DurableEffectClass::Sealed;
        assert!(
            coordinate
                .validate()
                .unwrap_err()
                .to_string()
                .contains("remote")
        );
    }

    #[test]
    fn provider_coordinate_key_covers_every_authoritative_dimension() {
        let base = || {
            ProviderRequestCoordinateV2::build(
                ProviderRequestAuthorityV2 {
                    outer_effective_definition_digest: "11".repeat(32),
                    provider_family: "chat_completions".to_string(),
                    provider_config_hash: "resolved-config-hash".to_string(),
                    provider_config_value_digest: "22".repeat(32),
                    provider_id: "route".to_string(),
                    profile_id: Some("profile".to_string()),
                    model_name: "model".to_string(),
                    credential_binding_hmac: "33".repeat(32),
                    credential_authority_generation: "credential-generation-7".to_string(),
                    authority_digest: "44".repeat(32),
                    admitted_effect_class: DurableEffectClass::Recorded,
                },
                ProviderTransportCoordinateV2::RemoteHttp {
                    method: "POST".to_string(),
                    url: "https://example.invalid/v1".to_string(),
                },
                PreparedProviderRequestProjectionV2::new(
                    [("x-mode".to_string(), "fast".to_string())],
                    ["x-secret".to_string()],
                    "55".repeat(32),
                    2048,
                )
                .unwrap(),
            )
            .unwrap()
        };
        let expected = base().cache_key().unwrap();
        let mut mutations: Vec<Box<dyn Fn(&mut ProviderRequestCoordinateV2)>> = vec![
            Box::new(|value| value.provider_config_hash = "other-config-hash".to_string()),
            Box::new(|value| value.provider_config_value_digest = "66".repeat(32)),
            Box::new(|value| value.credential_binding_hmac = "77".repeat(32)),
            Box::new(|value| value.model_name = "other".to_string()),
            Box::new(|value| value.body_sha256 = "88".repeat(32)),
            Box::new(|value| value.requested_output_ceiling += 1),
            Box::new(|value| value.public_headers[0].value_digest = "99".repeat(32)),
            Box::new(|value| {
                value.credential_authority_generation = "credential-generation-8".to_string()
            }),
            Box::new(|value| {
                value.transport = ProviderTransportCoordinateV2::RemoteHttp {
                    method: "POST".to_string(),
                    url: "https://other.invalid/v1".to_string(),
                }
            }),
            Box::new(|value| {
                value.transport = ProviderTransportCoordinateV2::AdmittedLocalWorker {
                    worker_ref: "provider_worker:local/qwen".to_string(),
                    effective_definition_digest: "aa".repeat(32),
                    capsule_hash: "bb".repeat(32),
                    execution_realization_hash: "cc".repeat(32),
                }
            }),
        ];
        for mutate in mutations.drain(..) {
            let mut changed = base();
            mutate(&mut changed);
            assert_ne!(changed.cache_key().unwrap(), expected);
        }
    }

    #[test]
    fn provider_header_partition_is_case_insensitive_and_secret_free() {
        let error = PreparedProviderRequestProjectionV2::new(
            [("Authorization".to_string(), "public-value".to_string())],
            ["authorization".to_string()],
            "11".repeat(32),
            1,
        )
        .unwrap_err();
        assert!(error.to_string().contains("both public and credential"));

        let secret = "this-secret-must-not-appear";
        let request = PreparedProviderRequestProjectionV2::new(
            [("X-Public".to_string(), "visible-behavior".to_string())],
            ["X-Credential".to_string()],
            "11".repeat(32),
            1,
        )
        .unwrap();
        let encoded = serde_json::to_string(&request.public_headers).unwrap();
        assert!(!encoded.contains("visible-behavior"));
        assert!(!encoded.contains(secret));
    }
}
