//! Durable record of one provider call's observed response.
//!
//! A record exists so a later run of the same program can replay the
//! response instead of re-paying the provider. The replay identity is the
//! prepared request's digest — computed over the exact bytes sent, with the
//! credential value excluded — scoped under the run's effective definition
//! digest, so equal keys mean the same program asked the same question.
//! Records are written only by the daemon under the pinned state authority,
//! and publication binds to the accounting reservation carrying the same
//! request digest: a sandboxed runtime can request replay but can never
//! bank a response for a request the daemon never saw reserved.

use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROVIDER_CALL_EFFECT_RECORD_KIND: &str = "provider_call_effect_record";
pub const PROVIDER_CALL_EFFECT_RECORD_SCHEMA_VERSION: u32 = 1;

/// Ceiling for the serialized response payload. Provider responses run
/// larger than node results (long generations with reasoning), so this is
/// generous; the generic object store bound still applies above it.
pub const MAX_PROVIDER_CALL_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCallEffectRecord {
    pub schema: u32,
    pub kind: String,
    /// The replay identity this record answers for — see
    /// [`provider_call_cache_key`].
    pub cache_key: String,
    pub effective_definition_digest: String,
    /// The prepared request's digest: method, url, sorted header names,
    /// body digest, and output ceiling, credential value excluded. Computed
    /// over what was actually sent, never re-derived from evidence.
    pub request_digest: String,
    /// Digest of the exact request body bytes. The body itself is not
    /// stored here; when request bytes are kept for divergence forensics
    /// they live as a CAS blob this digest addresses.
    pub body_sha256: String,
    /// `recorded` today; `sealed` when local inference makes the response a
    /// re-derivable function of admitted content.
    pub class: String,
    /// The response as the runtime consumed it at first execution. Opaque
    /// here: it carries no CAS references and contributes no closure edges.
    pub response: Value,
    /// Settled usage the daemon observed for the recorded call, when the
    /// producing turn reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_accounting: Option<Value>,
    /// Thread that paid for the recorded call, for provenance.
    pub produced_by_thread: String,
    /// Digest of the execution identity the producing node attested at
    /// boot — the coordinate beside the program digest. Provenance, never
    /// key material: cache_key stays (program, request) exactly, and a
    /// remote provider's record simply carries the caller node's identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_identity: Option<String>,
}

fn require_hex64(field: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("provider call effect record {field} must be 64 lowercase hex characters");
    }
    Ok(())
}

/// Derive the replay identity for one provider call.
///
/// The daemon derives this; a runtime never names its own cache key, so a
/// lying runtime cannot poison the index. Scoping under the effective
/// definition digest keeps the no-cross-digest-reuse rule structural: a
/// changed program cannot key into another program's records even for an
/// identical request.
pub fn provider_call_cache_key(
    effective_definition_digest: &str,
    request_digest: &str,
) -> anyhow::Result<String> {
    require_hex64("effective_definition_digest", effective_definition_digest)?;
    require_hex64("request_digest", request_digest)?;
    let seed = serde_json::json!({
        "schema": "ryeos.provider_call_record.key.v1",
        "effective_definition_digest": effective_definition_digest,
        "request_digest": request_digest,
    });
    let canonical = lillux::canonical_json(&seed)?;
    Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
}

impl ProviderCallEffectRecord {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.kind != PROVIDER_CALL_EFFECT_RECORD_KIND {
            bail!("unexpected provider call effect record kind: {}", self.kind);
        }
        if self.schema != PROVIDER_CALL_EFFECT_RECORD_SCHEMA_VERSION {
            bail!(
                "unexpected provider call effect record schema: {} (current {})",
                self.schema,
                PROVIDER_CALL_EFFECT_RECORD_SCHEMA_VERSION
            );
        }
        require_hex64("cache_key", &self.cache_key)?;
        require_hex64(
            "effective_definition_digest",
            &self.effective_definition_digest,
        )?;
        require_hex64("request_digest", &self.request_digest)?;
        require_hex64("body_sha256", &self.body_sha256)?;
        if !super::RECORDABLE_EFFECT_CLASSES.contains(&self.class.as_str()) {
            bail!(
                "provider call effect record class `{}` is not recordable; \
                 live calls are never recorded",
                self.class
            );
        }
        let derived = provider_call_cache_key(
            &self.effective_definition_digest,
            &self.request_digest,
        )?;
        if derived != self.cache_key {
            bail!(
                "provider call effect record cache_key does not answer for its own \
                 identity fields"
            );
        }
        super::validate_trimmed_control_free(
            "provider call effect record produced_by_thread",
            &self.produced_by_thread,
            false,
        )?;
        if let Some(execution_identity) = &self.execution_identity {
            require_hex64("execution_identity", execution_identity)?;
        }
        let response_bytes = serde_json::to_vec(&self.response)?.len();
        if response_bytes > MAX_PROVIDER_CALL_RESPONSE_BYTES {
            bail!(
                "provider call effect record response is {response_bytes} bytes; \
                 the bound is {MAX_PROVIDER_CALL_RESPONSE_BYTES}"
            );
        }
        Ok(())
    }

    /// Decode only the exact current wire contract, rejecting other schemas
    /// before serde interprets any field.
    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("provider call effect record has no string kind"))?;
        if kind != PROVIDER_CALL_EFFECT_RECORD_KIND {
            bail!("unexpected provider call effect record kind: {kind}");
        }
        let schema = value
            .get("schema")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                anyhow::anyhow!("provider call effect record has no numeric schema")
            })?;
        if schema != u64::from(PROVIDER_CALL_EFFECT_RECORD_SCHEMA_VERSION) {
            return Err(super::IncompatibleCurrentObjectSchema::new(
                "provider call effect record",
                schema,
                PROVIDER_CALL_EFFECT_RECORD_SCHEMA_VERSION,
            )
            .into());
        }
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
    use serde_json::json;

    fn record() -> ProviderCallEffectRecord {
        let effective = "b".repeat(64);
        let request = "c".repeat(64);
        ProviderCallEffectRecord {
            schema: PROVIDER_CALL_EFFECT_RECORD_SCHEMA_VERSION,
            kind: PROVIDER_CALL_EFFECT_RECORD_KIND.to_string(),
            cache_key: provider_call_cache_key(&effective, &request).unwrap(),
            effective_definition_digest: effective,
            request_digest: request,
            body_sha256: "d".repeat(64),
            class: "recorded".to_string(),
            response: json!({"content": "the answer", "tool_calls": []}),
            provider_accounting: Some(json!({"input_tokens": 100, "output_tokens": 12})),
            produced_by_thread: "T-1234".to_string(),
            execution_identity: None,
       }
    }

    #[test]
    fn a_record_round_trips_through_the_current_contract() {
        let value = record().to_value().unwrap();
        let decoded = ProviderCallEffectRecord::from_current_value(&value).unwrap();
        assert_eq!(decoded, record());
    }

    #[test]
    fn the_cache_key_must_answer_for_its_identity_fields() {
        let mut forged = record();
        forged.cache_key = "a".repeat(64);
        let error = forged.validate().unwrap_err();
        assert!(
            error.to_string().contains("does not answer"),
            "got {error}"
        );
    }

    #[test]
    fn live_calls_are_never_recordable() {
        let mut invalid = record();
        invalid.class = "live".to_string();
        let error = invalid.validate().unwrap_err();
        assert!(error.to_string().contains("never recorded"));
    }

    #[test]
    fn a_predecessor_schema_is_rejected_before_field_interpretation() {
        let mut value = record().to_value().unwrap();
        value["schema"] = json!(0);
        value["cache_key"] = json!("not-hex");
        let error = ProviderCallEffectRecord::from_current_value(&value).unwrap_err();
        assert!(error.to_string().contains("schema"));
    }

    #[test]
    fn the_cache_key_derivation_is_deterministic_and_fail_closed() {
        let effective = "1".repeat(64);
        let request = "2".repeat(64);
        assert_eq!(
            provider_call_cache_key(&effective, &request).unwrap(),
            provider_call_cache_key(&effective, &request).unwrap(),
        );
        assert_ne!(
            provider_call_cache_key(&effective, &request).unwrap(),
            provider_call_cache_key(&request, &effective).unwrap(),
        );
        assert!(provider_call_cache_key("junk", &request).is_err());
        assert!(provider_call_cache_key(&effective, "junk").is_err());
    }
}
