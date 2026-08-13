//! Provider-owned durable call identity and answer contracts.
//!
//! These types deliberately do not live in generic state or dispatch code.
//! Provider/accounting code constructs and interprets them; the state object
//! registry only invokes their strict decoder and follows declared links.

use anyhow::{Context as _, bail};
use ryeos_effect_contract::{
    EffectClass, canonical_value_digest, require_hex64, validate_identifier,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROVIDER_CALL_RECORD_SCHEMA_VERSION: u32 = 2;
pub const PROVIDER_CALL_KEY_SCHEMA: &str = "ryeos.provider_call_effect.key.v2";
pub const PROVIDER_CALL_RECORD_KIND: &str = "provider_call_effect_record";
pub const PROVIDER_CALL_REPLAY_NAMESPACE: &str = "provider.call";
pub const LOCAL_WORKER_OBSERVATION_SCHEMA_VERSION: u32 = 1;
pub const LOCAL_WORKER_OBSERVATION_KIND: &str = "provider_local_worker_observation";
pub const LOCAL_WORKER_OBSERVATION_KEY_SCHEMA: &str =
    "ryeos.provider_local_worker_observation.key.v1";
pub const LOCAL_WORKER_OBSERVATION_REPLAY_NAMESPACE: &str = "provider.local_worker_observation";
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<Value>,
    #[serde(default)]
    pub tool_calls: Option<Vec<RecordedToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl RecordedMessage {
    fn validate(&self) -> anyhow::Result<()> {
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
                validate_identifier("recorded provider tool name", &call.name)?;
                if let Some(id) = &call.id {
                    validate_identifier("recorded provider tool id", id)?;
                }
            }
        }
        if self
            .reasoning_content
            .as_ref()
            .is_some_and(|reasoning| reasoning.len() > MAX_ANSWER_BYTES)
        {
            bail!("recorded provider reasoning exceeds the answer bound");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallAnswer {
    pub message: RecordedMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

impl ProviderCallAnswer {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.message.validate()?;
        if let Some(reason) = &self.finish_reason {
            validate_identifier("recorded provider finish reason", reason)?;
        }
        let bytes = serde_json::to_vec(self)?.len();
        if bytes > MAX_ANSWER_BYTES {
            bail!("provider call answer exceeds {MAX_ANSWER_BYTES} bytes");
        }
        Ok(())
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::to_value(self)?)
    }
}

/// Strict provider-neutral terminal contract emitted by an admitted local
/// worker. Both the daemon observation boundary and the directive adapter use
/// this one decoder; the persistent-session substrate keeps the body opaque.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedLocalWorkerFinal {
    pub answer: ProviderCallAnswer,
    pub usage: AdmittedLocalWorkerUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedLocalWorkerUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

impl AdmittedLocalWorkerFinal {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.answer.validate()?;
        self.usage
            .input_tokens
            .checked_add(self.usage.output_tokens)
            .ok_or_else(|| anyhow::anyhow!("local worker token accounting overflows"))?;
        if let Some(reasoning) = self.usage.reasoning_tokens {
            if reasoning > self.usage.output_tokens {
                bail!("local worker reasoning tokens exceed output tokens");
            }
        }
        if let Some(response_id) = &self.response_id {
            validate_identifier("local worker response id", response_id)?;
        }
        if serde_json::to_vec(self)?.len() > MAX_ANSWER_BYTES {
            bail!("local worker terminal exceeds {MAX_ANSWER_BYTES} bytes");
        }
        Ok(())
    }

    pub fn from_value(value: &Value) -> anyhow::Result<Self> {
        let terminal: Self = serde_json::from_value(value.clone())?;
        terminal.validate()?;
        Ok(terminal)
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::to_value(self)?)
    }
}

/// Daemon-observed local-worker outcome retained before it is exposed to the
/// runtime. The object is the restart boundary: a claimed attempt may replay
/// this exact terminal without contacting the worker, while a claimed attempt
/// with no observation is an unknown outcome and must not refire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalWorkerObservation {
    pub schema: u32,
    pub kind: String,
    pub attempt_id: String,
    pub request_hash: String,
    pub coordinate_key: String,
    pub capsule_hash: String,
    pub admitted_execution_realization_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_execution_realization_hash: Option<String>,
    pub observed_at: String,
    pub terminal_digest: String,
    pub terminal: AdmittedLocalWorkerFinal,
    pub execution_identity_digest: String,
    pub execution_identity_attestation_hash: String,
}

impl LocalWorkerObservation {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != LOCAL_WORKER_OBSERVATION_SCHEMA_VERSION
            || self.kind != LOCAL_WORKER_OBSERVATION_KIND
        {
            bail!("local worker observation contract is not current");
        }
        validate_identifier("local worker observation attempt", &self.attempt_id)?;
        for (field, digest) in [
            ("local worker request hash", &self.request_hash),
            ("local worker coordinate key", &self.coordinate_key),
            ("local worker capsule hash", &self.capsule_hash),
            (
                "local worker admitted execution realization hash",
                &self.admitted_execution_realization_hash,
            ),
            ("local worker terminal digest", &self.terminal_digest),
            (
                "local worker execution identity digest",
                &self.execution_identity_digest,
            ),
            (
                "local worker execution identity attestation hash",
                &self.execution_identity_attestation_hash,
            ),
        ] {
            require_hex64(field, digest)?;
        }
        if let Some(hash) = &self.observed_execution_realization_hash {
            require_hex64("local worker observed execution realization hash", hash)?;
        }
        let parsed = chrono::DateTime::parse_from_rfc3339(&self.observed_at)
            .context("local worker observation has an invalid timestamp")?;
        if parsed
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            != self.observed_at
        {
            bail!("local worker observation timestamp is not canonical UTC milliseconds");
        }
        if self.terminal.digest()? != self.terminal_digest {
            bail!("local worker observation terminal digest changed");
        }
        Ok(())
    }

    pub fn observation_key(&self) -> anyhow::Result<String> {
        self.validate()?;
        local_worker_observation_key(&self.attempt_id, &self.request_hash, &self.coordinate_key)
    }

    /// Bind retained evidence back to the exact daemon-admitted attempt. This
    /// comparison is intentionally separate from structural decoding because
    /// the expected request/coordinate are authority owned by the caller.
    pub fn validate_against(
        &self,
        attempt_id: &str,
        request_hash: &str,
        coordinate: &RequestCoordinate,
    ) -> anyhow::Result<()> {
        self.validate()?;
        let coordinate_key = coordinate.cache_key()?;
        if self.attempt_id != attempt_id
            || self.request_hash != request_hash
            || self.coordinate_key != coordinate_key
        {
            bail!("local worker observation contradicts its admitted attempt");
        }
        let TransportCoordinate::AdmittedLocalWorker {
            capsule_hash,
            execution_realization_hash,
            ..
        } = &coordinate.transport
        else {
            bail!("local worker observation cannot bind a remote transport");
        };
        if self.capsule_hash != *capsule_hash
            || self.admitted_execution_realization_hash != *execution_realization_hash
        {
            bail!("local worker observation contradicts its admitted execution closure");
        }
        if self.terminal.usage.output_tokens > coordinate.requested_output_ceiling {
            bail!("local worker observation exceeds its admitted output ceiling");
        }
        Ok(())
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::to_value(self)?)
    }

    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        let observation: Self = serde_json::from_value(value.clone())?;
        observation.validate()?;
        Ok(observation)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }
}

pub fn local_worker_observation_key(
    attempt_id: &str,
    request_hash: &str,
    coordinate_key: &str,
) -> anyhow::Result<String> {
    validate_identifier("local worker observation attempt", attempt_id)?;
    require_hex64("local worker observation request hash", request_hash)?;
    require_hex64("local worker observation coordinate key", coordinate_key)?;
    canonical_value_digest(&serde_json::json!({
        "schema": LOCAL_WORKER_OBSERVATION_KEY_SCHEMA,
        "attempt_id": attempt_id,
        "request_hash": request_hash,
        "coordinate_key": coordinate_key,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicHeaderCoordinate {
    pub name: String,
    pub value_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedRequestProjection {
    pub public_headers: Vec<PublicHeaderCoordinate>,
    pub credential_header_names: Vec<String>,
    pub body_sha256: String,
    pub requested_output_ceiling: u64,
}

/// Provider-runtime intent supplied at attempt preparation. This is not an
/// admitted transport coordinate: the daemon must derive that coordinate from
/// sealed authority before reserving or contacting anything. In particular,
/// an admitted-worker intent contains no runtime-asserted capsule or
/// realization identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PreparedTransportIntent {
    RemoteHttp { method: String, url: String },
    AdmittedLocalWorker { execute: String },
}

impl PreparedTransportIntent {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::RemoteHttp { method, url } => {
                let normalized = normalize_transport(TransportCoordinate::RemoteHttp {
                    method: method.clone(),
                    url: url.clone(),
                })?;
                let TransportCoordinate::RemoteHttp {
                    method: normalized_method,
                    url: normalized_url,
                } = normalized
                else {
                    unreachable!("remote intent normalizes to remote transport")
                };
                if method != &normalized_method || url != &normalized_url {
                    bail!("provider remote transport intent is not canonical");
                }
            }
            Self::AdmittedLocalWorker { execute } => {
                validate_identifier("provider admitted-worker execute ref", execute)?;
                if !execute.contains(':') {
                    bail!("provider admitted-worker execute ref is not canonical");
                }
            }
        }
        Ok(())
    }
}

impl PreparedRequestProjection {
    pub fn new(
        public_headers: impl IntoIterator<Item = (String, String)>,
        credential_header_names: impl IntoIterator<Item = String>,
        body_sha256: String,
        requested_output_ceiling: u64,
    ) -> anyhow::Result<Self> {
        require_hex64("provider request body digest", &body_sha256)?;
        if requested_output_ceiling == 0 {
            bail!("provider request output ceiling must be positive");
        }
        let (public_headers, credential_header_names) =
            header_projection(public_headers, credential_header_names)?;
        Ok(Self {
            public_headers,
            credential_header_names,
            body_sha256,
            requested_output_ceiling,
        })
    }

    /// Construct from an already-secret-free projection. This is the daemon
    /// boundary used by provider-attempt preparation: values were digested by
    /// the provider-owned request preparer, so the daemon validates canonical
    /// names, ordering, separation, and digest shape without asking the
    /// runtime to disclose public header values again.
    pub fn from_coordinates(
        public_headers: Vec<PublicHeaderCoordinate>,
        credential_header_names: Vec<String>,
        body_sha256: String,
        requested_output_ceiling: u64,
    ) -> anyhow::Result<Self> {
        require_hex64("provider request body digest", &body_sha256)?;
        if requested_output_ceiling == 0 {
            bail!("provider request output ceiling must be positive");
        }
        validate_normalized_headers(&public_headers, &credential_header_names)?;
        Ok(Self {
            public_headers,
            credential_header_names,
            body_sha256,
            requested_output_ceiling,
        })
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        Self::from_coordinates(
            self.public_headers.clone(),
            self.credential_header_names.clone(),
            self.body_sha256.clone(),
            self.requested_output_ceiling,
        )
        .map(|_| ())
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "public_headers": self.public_headers,
            "credential_header_names": self.credential_header_names,
            "body_sha256": self.body_sha256,
            "requested_output_ceiling": self.requested_output_ceiling,
        }))
    }
}

pub fn header_projection(
    public_headers: impl IntoIterator<Item = (String, String)>,
    credential_header_names: impl IntoIterator<Item = String>,
) -> anyhow::Result<(Vec<PublicHeaderCoordinate>, Vec<String>)> {
    let mut credential_header_names = credential_header_names
        .into_iter()
        .map(|name| normalize_header_name(&name))
        .collect::<anyhow::Result<Vec<_>>>()?;
    credential_header_names.sort();
    if credential_header_names
        .windows(2)
        .any(|pair| pair[0] == pair[1])
    {
        bail!("provider credential header names must be unique");
    }

    let mut public_headers = public_headers
        .into_iter()
        .map(|(name, value)| {
            let name = normalize_header_name(&name)?;
            if value.contains(['\r', '\n']) {
                bail!("provider public header `{name}` contains a line break");
            }
            Ok(PublicHeaderCoordinate {
                name,
                value_digest: lillux::sha256_hex(value.as_bytes()),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    public_headers.sort_by(|left, right| left.name.cmp(&right.name));
    if public_headers
        .windows(2)
        .any(|pair| pair[0].name == pair[1].name)
    {
        bail!("provider public header names must be unique");
    }
    if let Some(collision) = public_headers
        .iter()
        .find(|header| credential_header_names.binary_search(&header.name).is_ok())
    {
        bail!(
            "provider header `{}` is both public and credential-bearing",
            collision.name
        );
    }
    Ok((public_headers, credential_header_names))
}

fn normalize_header_name(value: &str) -> anyhow::Result<String> {
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
        bail!("provider header name `{value}` is invalid");
    }
    Ok(normalized)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestAuthority {
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
    pub admitted_effect_class: Option<EffectClass>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransportCoordinate {
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
pub struct RequestCoordinate {
    pub outer_effective_definition_digest: String,
    pub transport: TransportCoordinate,
    pub provider_family: String,
    pub provider_config_hash: String,
    pub provider_config_value_digest: String,
    pub provider_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub model_name: String,
    pub public_headers: Vec<PublicHeaderCoordinate>,
    pub credential_header_names: Vec<String>,
    pub body_sha256: String,
    pub requested_output_ceiling: u64,
    pub credential_binding_hmac: String,
    pub credential_authority_generation: String,
    pub authority_digest: String,
    pub admitted_effect_class: Option<EffectClass>,
}

impl RequestCoordinate {
    pub fn build(
        authority: RequestAuthority,
        transport: TransportCoordinate,
        request: PreparedRequestProjection,
    ) -> anyhow::Result<Self> {
        let transport = normalize_transport(transport)?;
        let value = Self {
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
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for (field, digest) in [
            (
                "outer effective-definition digest",
                &self.outer_effective_definition_digest,
            ),
            (
                "provider config value digest",
                &self.provider_config_value_digest,
            ),
            ("provider request body digest", &self.body_sha256),
            ("credential binding HMAC", &self.credential_binding_hmac),
            ("provider authority digest", &self.authority_digest),
        ] {
            require_hex64(field, digest)?;
        }
        if self.requested_output_ceiling == 0 {
            bail!("provider request output ceiling must be positive");
        }
        for (field, value) in [
            ("provider family", &self.provider_family),
            ("provider config hash", &self.provider_config_hash),
            ("provider id", &self.provider_id),
            ("provider model name", &self.model_name),
            (
                "credential authority generation",
                &self.credential_authority_generation,
            ),
        ] {
            validate_identifier(field, value)?;
        }
        if let Some(profile) = &self.profile_id {
            validate_identifier("provider profile id", profile)?;
        }
        validate_normalized_headers(&self.public_headers, &self.credential_header_names)?;
        match &self.transport {
            TransportCoordinate::RemoteHttp { method, url } => {
                if normalize_transport(self.transport.clone())? != self.transport {
                    bail!("provider remote transport is not canonical");
                }
                if self.admitted_effect_class == Some(EffectClass::Sealed) {
                    bail!("remote provider transport cannot claim sealed effect class");
                }
                validate_identifier("provider remote method", method)?;
                validate_identifier("provider remote URL", url)?;
            }
            TransportCoordinate::AdmittedLocalWorker {
                worker_ref,
                effective_definition_digest,
                capsule_hash,
                execution_realization_hash,
            } => {
                validate_identifier("admitted local worker ref", worker_ref)?;
                require_hex64(
                    "admitted local worker effective-definition digest",
                    effective_definition_digest,
                )?;
                require_hex64("admitted local worker capsule hash", capsule_hash)?;
                require_hex64(
                    "admitted local worker execution realization hash",
                    execution_realization_hash,
                )?;
            }
        }
        Ok(())
    }

    pub fn cache_key(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "schema": PROVIDER_CALL_KEY_SCHEMA,
            "coordinate": self,
        }))
    }
}

fn normalize_transport(transport: TransportCoordinate) -> anyhow::Result<TransportCoordinate> {
    match transport {
        TransportCoordinate::RemoteHttp { method, url } => {
            let method = method.trim().to_ascii_uppercase();
            validate_identifier("provider remote method", &method)?;
            let parsed = url::Url::parse(&url)
                .map_err(|error| anyhow::anyhow!("invalid provider remote URL: {error}"))?;
            if !matches!(parsed.scheme(), "http" | "https")
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                bail!("provider remote URL violates transport policy");
            }
            Ok(TransportCoordinate::RemoteHttp {
                method,
                url: parsed.to_string(),
            })
        }
        local @ TransportCoordinate::AdmittedLocalWorker { .. } => Ok(local),
    }
}

fn validate_normalized_headers(
    public: &[PublicHeaderCoordinate],
    credentials: &[String],
) -> anyhow::Result<()> {
    let mut prior: Option<&str> = None;
    for header in public {
        if normalize_header_name(&header.name)? != header.name {
            bail!("provider public header names must be normalized");
        }
        require_hex64("provider public header value digest", &header.value_digest)?;
        if prior.is_some_and(|value| value >= header.name.as_str()) {
            bail!("provider public headers must be sorted and unique");
        }
        prior = Some(&header.name);
    }
    let mut prior: Option<&str> = None;
    for header in credentials {
        if normalize_header_name(header)? != *header {
            bail!("provider credential header names must be normalized");
        }
        if prior.is_some_and(|value| value >= header.as_str()) {
            bail!("provider credential headers must be sorted and unique");
        }
        if public
            .binary_search_by(|candidate| candidate.name.cmp(header))
            .is_ok()
        {
            bail!("provider public and credential header names collide");
        }
        prior = Some(header);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationClass {
    RuntimeTransportObserved,
    DaemonWorkerObserved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirstObservation {
    pub produced_by_thread: String,
    pub attempt_id: String,
    pub response_digest: String,
    pub observed_at: String,
    pub observation_class: ObservationClass,
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

impl FirstObservation {
    fn validate(&self, coordinate: &RequestCoordinate) -> anyhow::Result<()> {
        validate_identifier("provider observation thread", &self.produced_by_thread)?;
        validate_identifier("provider observation attempt id", &self.attempt_id)?;
        require_hex64(
            "provider observation response digest",
            &self.response_digest,
        )?;
        let parsed = chrono::DateTime::parse_from_rfc3339(&self.observed_at)
            .context("provider observation has an invalid timestamp")?;
        if parsed
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            != self.observed_at
        {
            bail!("provider observation timestamp is not canonical UTC milliseconds");
        }
        let expected = match coordinate.transport {
            TransportCoordinate::RemoteHttp { .. } => ObservationClass::RuntimeTransportObserved,
            TransportCoordinate::AdmittedLocalWorker { .. } => {
                ObservationClass::DaemonWorkerObserved
            }
        };
        if self.observation_class != expected {
            bail!("provider observation class contradicts its transport");
        }
        for (field, digest) in [
            (
                "execution identity digest",
                self.execution_identity_digest.as_deref(),
            ),
            (
                "execution identity attestation hash",
                self.execution_identity_attestation_hash.as_deref(),
            ),
            (
                "admitted execution realization hash",
                self.admitted_execution_realization_hash.as_deref(),
            ),
            (
                "observed execution realization hash",
                self.observed_execution_realization_hash.as_deref(),
            ),
        ] {
            if let Some(digest) = digest {
                require_hex64(field, digest)?;
            }
        }
        if serde_json::to_vec(&self.provider_accounting)?.len() > MAX_ANSWER_BYTES {
            bail!("provider accounting observation exceeds the bounded size");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallRecord {
    pub schema: u32,
    pub kind: String,
    pub cache_key: String,
    pub coordinate: RequestCoordinate,
    pub answer_digest: String,
    pub answer: ProviderCallAnswer,
    pub first_observation: FirstObservation,
}

impl ProviderCallRecord {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PROVIDER_CALL_RECORD_SCHEMA_VERSION {
            bail!("provider call record schema is not current");
        }
        if self.kind != PROVIDER_CALL_RECORD_KIND {
            bail!("provider call record kind is not current");
        }
        require_hex64("provider call cache key", &self.cache_key)?;
        if self.coordinate.cache_key()? != self.cache_key {
            bail!("provider call cache key contradicts its coordinate");
        }
        if self.answer.digest()? != self.answer_digest {
            bail!("provider call answer digest contradicts its answer");
        }
        self.first_observation.validate(&self.coordinate)?;
        if self.first_observation.response_digest != self.answer_digest {
            bail!("provider first-observation digest contradicts its answer");
        }
        match &self.coordinate.transport {
            TransportCoordinate::RemoteHttp { .. } => {
                if self.first_observation.execution_identity_digest.is_some()
                    || self
                        .first_observation
                        .execution_identity_attestation_hash
                        .is_some()
                    || self
                        .first_observation
                        .admitted_execution_realization_hash
                        .is_some()
                    || self
                        .first_observation
                        .observed_execution_realization_hash
                        .is_some()
                {
                    bail!("remote provider observation cannot claim local execution evidence");
                }
            }
            TransportCoordinate::AdmittedLocalWorker {
                execution_realization_hash,
                ..
            } => {
                let identity_digest = self
                    .first_observation
                    .execution_identity_digest
                    .as_deref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("local provider observation lacks execution identity")
                    })?;
                let attestation_hash = self
                    .first_observation
                    .execution_identity_attestation_hash
                    .as_deref()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "local provider observation lacks execution identity attestation"
                        )
                    })?;
                let admitted = self
                    .first_observation
                    .admitted_execution_realization_hash
                    .as_deref()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "local provider observation lacks admitted execution realization"
                        )
                    })?;
                require_hex64("local provider execution identity", identity_digest)?;
                require_hex64(
                    "local provider execution identity attestation",
                    attestation_hash,
                )?;
                if admitted != execution_realization_hash {
                    bail!("local provider observation contradicts its admitted realization");
                }
            }
        }
        Ok(())
    }

    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        let record: Self = serde_json::from_value(value.clone())?;
        record.validate()?;
        Ok(record)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_coordinate() -> RequestCoordinate {
        RequestCoordinate::build(
            RequestAuthority {
                outer_effective_definition_digest: "1".repeat(64),
                provider_family: "chat_completions".to_owned(),
                provider_config_hash: "provider-config".to_owned(),
                provider_config_value_digest: "2".repeat(64),
                provider_id: "local-tinygrad".to_owned(),
                profile_id: None,
                model_name: "qwen3-0.6b".to_owned(),
                credential_binding_hmac: "3".repeat(64),
                credential_authority_generation: "none".to_owned(),
                authority_digest: "4".repeat(64),
                admitted_effect_class: Some(EffectClass::Recorded),
            },
            TransportCoordinate::AdmittedLocalWorker {
                worker_ref: "worker:local-inference/local-tinygrad".to_owned(),
                effective_definition_digest: "5".repeat(64),
                capsule_hash: "6".repeat(64),
                execution_realization_hash: "7".repeat(64),
            },
            PreparedRequestProjection::new(
                std::iter::empty(),
                std::iter::empty(),
                "8".repeat(64),
                16,
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn local_terminal() -> AdmittedLocalWorkerFinal {
        AdmittedLocalWorkerFinal {
            answer: ProviderCallAnswer {
                message: RecordedMessage {
                    role: "assistant".to_owned(),
                    content: Some(Value::String("done".to_owned())),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                },
                finish_reason: Some("stop".to_owned()),
            },
            usage: AdmittedLocalWorkerUsage {
                input_tokens: 5,
                output_tokens: 1,
                reasoning_tokens: None,
            },
            response_id: Some("response-1".to_owned()),
        }
    }

    fn local_record() -> ProviderCallRecord {
        let coordinate = local_coordinate();
        let answer = local_terminal().answer;
        let answer_digest = answer.digest().unwrap();
        ProviderCallRecord {
            schema: PROVIDER_CALL_RECORD_SCHEMA_VERSION,
            kind: PROVIDER_CALL_RECORD_KIND.to_owned(),
            cache_key: coordinate.cache_key().unwrap(),
            coordinate,
            answer_digest: answer_digest.clone(),
            answer,
            first_observation: FirstObservation {
                produced_by_thread: "T-local".to_owned(),
                attempt_id: "attempt-1".to_owned(),
                response_digest: answer_digest,
                observed_at: "2026-08-09T00:00:00.000Z".to_owned(),
                observation_class: ObservationClass::DaemonWorkerObserved,
                provider_accounting: serde_json::json!({
                    "input_tokens": 5,
                    "output_tokens": 1,
                }),
                execution_identity_digest: Some("9".repeat(64)),
                execution_identity_attestation_hash: Some("a".repeat(64)),
                admitted_execution_realization_hash: Some("7".repeat(64)),
                observed_execution_realization_hash: None,
            },
        }
    }

    #[test]
    fn header_partition_is_case_insensitive_and_secret_free() {
        assert!(
            PreparedRequestProjection::new(
                [("Authorization".to_string(), "public".to_string())],
                ["authorization".to_string()],
                "11".repeat(32),
                1,
            )
            .is_err()
        );
        let projection = PreparedRequestProjection::new(
            [("X-Public".to_string(), "behavior".to_string())],
            ["X-Credential".to_string()],
            "11".repeat(32),
            1,
        )
        .unwrap();
        assert_eq!(projection.public_headers[0].name, "x-public");
        assert_eq!(projection.credential_header_names, ["x-credential"]);
        assert!(
            !serde_json::to_string(&projection.public_headers)
                .unwrap()
                .contains("behavior")
        );
    }

    #[test]
    fn local_record_requires_daemon_provenance_and_cross_field_identity() {
        let record = local_record();
        record.validate().unwrap();

        let mut wrong_class = record.clone();
        wrong_class.first_observation.observation_class =
            ObservationClass::RuntimeTransportObserved;
        assert!(
            wrong_class
                .validate()
                .unwrap_err()
                .to_string()
                .contains("observation class")
        );

        let mut wrong_answer = record.clone();
        wrong_answer.first_observation.response_digest = "b".repeat(64);
        assert!(
            wrong_answer
                .validate()
                .unwrap_err()
                .to_string()
                .contains("digest contradicts its answer")
        );

        let mut wrong_realization = record;
        wrong_realization
            .first_observation
            .admitted_execution_realization_hash = Some("c".repeat(64));
        assert!(
            wrong_realization
                .validate()
                .unwrap_err()
                .to_string()
                .contains("contradicts its admitted realization")
        );
    }

    #[test]
    fn local_observation_binds_terminal_and_admitted_coordinate_without_fabricating_observed_state()
    {
        let coordinate = local_coordinate();
        let terminal = local_terminal();
        let request_hash = "d".repeat(64);
        let mut observation = LocalWorkerObservation {
            schema: LOCAL_WORKER_OBSERVATION_SCHEMA_VERSION,
            kind: LOCAL_WORKER_OBSERVATION_KIND.to_owned(),
            attempt_id: "attempt-1".to_owned(),
            request_hash: request_hash.clone(),
            coordinate_key: coordinate.cache_key().unwrap(),
            capsule_hash: "6".repeat(64),
            admitted_execution_realization_hash: "7".repeat(64),
            observed_execution_realization_hash: None,
            observed_at: "2026-08-09T00:00:00.000Z".to_owned(),
            terminal_digest: terminal.digest().unwrap(),
            terminal,
            execution_identity_digest: "9".repeat(64),
            execution_identity_attestation_hash: "a".repeat(64),
        };
        observation
            .validate_against("attempt-1", &request_hash, &coordinate)
            .unwrap();

        observation.terminal.usage.output_tokens = 17;
        observation.terminal_digest = observation.terminal.digest().unwrap();
        assert!(
            observation
                .validate_against("attempt-1", &request_hash, &coordinate)
                .unwrap_err()
                .to_string()
                .contains("output ceiling")
        );
    }
}
