//! Kind-neutral contracts for admitted durable dispatch effects.
//!
//! Kinds decide which authored operations are effects and compile those
//! decisions into [`AdmittedEffectAuthorization`] values. The substrate only
//! checks the admitted mechanical identity, executes the already-admitted
//! subject, and stores a normalized answer. No kind vocabulary belongs here.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EFFECT_RECORD_SCHEMA_VERSION: u32 = 2;
pub const EFFECT_KEY_SCHEMA: &str = "ryeos.dispatch_effect.key.v2";
pub const EFFECT_RECORD_KIND: &str = "dispatch_effect_record";
pub const EFFECT_REPLAY_NAMESPACE: &str = "dispatch.effect";
pub const EFFECT_AUTHORIZATIONS_DERIVED_KEY: &str = "admitted_effect_authorizations";
pub const RECORDABLE_EFFECT_CLASSES: &[&str] = &["recorded", "sealed"];
const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;
const MAX_WARNING_BYTES: usize = 16 * 1024;
const MAX_WARNINGS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Recorded,
    Sealed,
}

/// Kind-neutral authority for one externally performed effect family.
///
/// The family identifier is opaque to generic launch machinery. The owning
/// runtime contract and the daemon boundary that performs the effect give it
/// meaning; the executor only validates, seals, and transports this value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedExternalEffectAuthority {
    pub authority_family: String,
    /// `None` is live execution. Durable classes are explicit and never
    /// inferred from a route name, endpoint, or accounting configuration.
    pub admitted_effect_class: Option<EffectClass>,
}

impl AdmittedExternalEffectAuthority {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identifier("external-effect authority family", &self.authority_family)
    }
}

/// Kind-validator output before it is bound to a finalized source program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectAuthorizationProjection {
    pub authorization_id: String,
    pub policy_digest: String,
    pub action_contract_digest: String,
    pub class: EffectClass,
}

impl EffectAuthorizationProjection {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identifier("effect authorization id", &self.authorization_id)?;
        require_hex64("effect policy digest", &self.policy_digest)?;
        require_hex64(
            "effect action-contract digest",
            &self.action_contract_digest,
        )
    }
}

pub fn validate_authorization_projections(
    projections: &[EffectAuthorizationProjection],
) -> anyhow::Result<()> {
    let mut prior: Option<&str> = None;
    for projection in projections {
        projection.validate()?;
        if prior.is_some_and(|value| value >= projection.authorization_id.as_str()) {
            bail!("effect authorization projections must be sorted and unique by id");
        }
        prior = Some(&projection.authorization_id);
    }
    Ok(())
}

impl EffectClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Sealed => "sealed",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "recorded" => Ok(Self::Recorded),
            "sealed" => Ok(Self::Sealed),
            other => bail!("unsupported durable effect class `{other}`"),
        }
    }

    pub const fn permits(self, requested: Self) -> bool {
        matches!(
            (self, requested),
            (Self::Recorded, Self::Recorded)
                | (Self::Sealed, Self::Recorded)
                | (Self::Sealed, Self::Sealed)
        )
    }
}

/// A kind-owned semantic decision projected into a mechanical launch grant.
///
/// `authorization_id` is opaque outside the owning kind. The digests make the
/// grant self-authenticating when it is captured in a launch capsule or
/// callback capability; the substrate never decodes the policy document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedEffectAuthorization {
    pub authorization_id: String,
    pub source_definition_ref: String,
    pub source_effective_definition_digest: String,
    pub policy_digest: String,
    pub action_contract_digest: String,
    pub class: EffectClass,
}

/// Callback-bound authority ready for exact callee preparation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedEffectDispatchAuthority {
    pub authorization: AdmittedEffectAuthorization,
    pub action_digest: String,
    pub subject_effect_class_ceiling: EffectClass,
}

impl PreparedEffectDispatchAuthority {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.authorization.validate()?;
        require_hex64("prepared effect action digest", &self.action_digest)?;
        if !self
            .subject_effect_class_ceiling
            .permits(self.authorization.class)
        {
            bail!("prepared effect authority exceeds the admitted subject ceiling");
        }
        Ok(())
    }
}

impl AdmittedEffectAuthorization {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identifier("effect authorization id", &self.authorization_id)?;
        validate_identifier("effect source definition ref", &self.source_definition_ref)?;
        require_hex64(
            "effect source effective-definition digest",
            &self.source_effective_definition_digest,
        )?;
        require_hex64("effect policy digest", &self.policy_digest)?;
        require_hex64(
            "effect action-contract digest",
            &self.action_contract_digest,
        )
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_digest(self)
    }
}

/// Exact admitted callee consumed by a cache miss.
///
/// The substrate prepares this once and uses the same value for lookup and
/// execution. `admission_evidence_hash` may name a launch capsule or another
/// kind-neutral admitted-subject object; its interpretation is registry-owned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedDispatchSubject {
    pub subject_ref: String,
    pub effective_definition_digest: String,
    pub execution_closure_digest: String,
    pub admission_evidence_hash: String,
    pub content_identity_digest: String,
    pub effect_class_ceiling: EffectClass,
}

impl AdmittedDispatchSubject {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identifier("dispatch subject ref", &self.subject_ref)?;
        for (field, digest) in [
            (
                "dispatch subject effective-definition digest",
                &self.effective_definition_digest,
            ),
            (
                "dispatch subject execution-closure digest",
                &self.execution_closure_digest,
            ),
            (
                "dispatch subject admission-evidence hash",
                &self.admission_evidence_hash,
            ),
            (
                "dispatch subject content-identity digest",
                &self.content_identity_digest,
            ),
        ] {
            require_hex64(field, digest)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchEffectIdentity {
    pub authorization: AdmittedEffectAuthorization,
    pub action_digest: String,
    pub subject: AdmittedDispatchSubject,
}

impl DispatchEffectIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.authorization.validate()?;
        self.subject.validate()?;
        require_hex64("dispatch-effect action digest", &self.action_digest)?;
        if !self
            .subject
            .effect_class_ceiling
            .permits(self.authorization.class)
        {
            bail!(
                "dispatch subject ceiling {} does not permit requested {} semantics",
                self.subject.effect_class_ceiling.as_str(),
                self.authorization.class.as_str(),
            );
        }
        Ok(())
    }

    pub fn cache_key(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_value_digest(&serde_json::json!({
            "schema": EFFECT_KEY_SCHEMA,
            "identity": self,
        }))
    }
}

/// Behavior visible to the caller after daemon envelope normalization.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "envelope", rename_all = "snake_case", deny_unknown_fields)]
pub enum DispatchEffectAnswer {
    Bare {
        result: Value,
    },
    Subprocess {
        result: Value,
    },
    Native {
        result: Value,
        outputs: Value,
        warnings: Vec<String>,
    },
}

impl DispatchEffectAnswer {
    pub fn validate(&self) -> anyhow::Result<()> {
        let warnings: &[String] = match self {
            Self::Native { warnings, .. } => warnings,
            Self::Bare { .. } | Self::Subprocess { .. } => &[],
        };
        validate_warnings(warnings)?;
        validate_bounded_value("dispatch-effect answer", &serde_json::to_value(self)?)
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        canonical_digest(self)
    }

    pub fn replay_leaf_envelope(&self, record_hash: &str) -> anyhow::Result<Value> {
        require_hex64("dispatch-effect record hash", record_hash)?;
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
pub struct EffectFirstObservation {
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

impl EffectFirstObservation {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_identifier("effect observation thread", &self.produced_by_thread)?;
        require_hex64("effect observation response digest", &self.response_digest)?;
        parse_canonical_timestamp(&self.observed_at)
            .context("effect observation has a non-canonical observed_at")?;
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
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchEffectRecord {
    pub schema: u32,
    pub kind: String,
    pub cache_key: String,
    pub identity: DispatchEffectIdentity,
    pub answer_digest: String,
    pub answer: DispatchEffectAnswer,
    pub first_observation: EffectFirstObservation,
}

impl DispatchEffectRecord {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != EFFECT_RECORD_SCHEMA_VERSION {
            bail!(
                "dispatch-effect record has schema {}, expected {}",
                self.schema,
                EFFECT_RECORD_SCHEMA_VERSION
            );
        }
        if self.kind != EFFECT_RECORD_KIND {
            bail!(
                "dispatch-effect record kind is {}, expected {}",
                self.kind,
                EFFECT_RECORD_KIND
            );
        }
        require_hex64("dispatch-effect cache key", &self.cache_key)?;
        if self.identity.cache_key()? != self.cache_key {
            bail!("dispatch-effect cache key contradicts its identity");
        }
        if self.answer.digest()? != self.answer_digest {
            bail!("dispatch-effect answer digest contradicts its answer");
        }
        self.first_observation.validate()
    }

    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("dispatch-effect record has no string kind"))?;
        if kind != EFFECT_RECORD_KIND {
            bail!("unexpected dispatch-effect record kind: {kind}");
        }
        let schema = value
            .get("schema")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("dispatch-effect record has no numeric schema"))?;
        if schema != u64::from(EFFECT_RECORD_SCHEMA_VERSION) {
            bail!(
                "dispatch-effect record schema {schema} is not current schema {}",
                EFFECT_RECORD_SCHEMA_VERSION
            );
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

pub fn canonical_value_digest(value: &Value) -> anyhow::Result<String> {
    let canonical = lillux::canonical_json(value)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

/// Produce the canonical timestamp spelling used by durable effect and
/// provider observations.
///
/// Authoritative thread/state timestamps deliberately use a different,
/// whole-second domain. Observation producers must call this constructor
/// instead of the general state clock so they cannot emit a value their own
/// durable contract refuses.
pub fn canonical_observation_timestamp_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn require_hex64(field: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("{field} must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

pub fn validate_identifier(field: &str, value: &str) -> anyhow::Result<()> {
    if value.trim() != value || value.is_empty() || value.chars().any(char::is_control) {
        bail!("{field} must be non-empty, trimmed, and control-free");
    }
    Ok(())
}

fn canonical_digest(value: &impl Serialize) -> anyhow::Result<String> {
    canonical_value_digest(&serde_json::to_value(value)?)
}

fn parse_canonical_timestamp(value: &str) -> anyhow::Result<()> {
    let parsed = chrono::DateTime::parse_from_rfc3339(value)
        .map_err(|error| anyhow::anyhow!("invalid RFC3339 timestamp: {error}"))?;
    let canonical = parsed
        .with_timezone(&chrono::Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    if canonical != value {
        bail!("timestamp is not canonical UTC millisecond RFC3339");
    }
    Ok(())
}

fn validate_warnings(warnings: &[String]) -> anyhow::Result<()> {
    if warnings.len() > MAX_WARNINGS {
        bail!("dispatch-effect answer has too many warnings");
    }
    for warning in warnings {
        if warning.len() > MAX_WARNING_BYTES {
            bail!("dispatch-effect answer warning exceeds {MAX_WARNING_BYTES} bytes");
        }
        validate_identifier("dispatch-effect answer warning", warning)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> DispatchEffectIdentity {
        DispatchEffectIdentity {
            authorization: AdmittedEffectAuthorization {
                authorization_id: "step:classify".to_string(),
                source_definition_ref: "workflow:example".to_string(),
                source_effective_definition_digest: "11".repeat(32),
                policy_digest: "22".repeat(32),
                action_contract_digest: "33".repeat(32),
                class: EffectClass::Recorded,
            },
            action_digest: "44".repeat(32),
            subject: AdmittedDispatchSubject {
                subject_ref: "tool:example/classify".to_string(),
                effective_definition_digest: "55".repeat(32),
                execution_closure_digest: "66".repeat(32),
                admission_evidence_hash: "77".repeat(32),
                content_identity_digest: "88".repeat(32),
                effect_class_ceiling: EffectClass::Sealed,
            },
        }
    }

    #[test]
    fn key_commits_to_authorization_action_and_exact_subject() {
        let base = identity();
        let expected = base.cache_key().unwrap();
        let mut changed = base.clone();
        changed.authorization.authorization_id = "step:other".to_string();
        assert_ne!(changed.cache_key().unwrap(), expected);
        let mut changed = base.clone();
        changed.action_digest = "99".repeat(32);
        assert_ne!(changed.cache_key().unwrap(), expected);
        let mut changed = base;
        changed.subject.content_identity_digest = "aa".repeat(32);
        assert_ne!(changed.cache_key().unwrap(), expected);
    }

    #[test]
    fn key_moves_when_the_admitted_callee_definition_moves() {
        let base = identity();
        let expected = base.cache_key().unwrap();
        let mut changed = base;
        changed.subject.effective_definition_digest = "aa".repeat(32);

        assert_ne!(changed.cache_key().unwrap(), expected);
    }

    #[test]
    fn ceiling_is_fail_closed() {
        let mut identity = identity();
        identity.authorization.class = EffectClass::Sealed;
        identity.subject.effect_class_ceiling = EffectClass::Recorded;
        assert!(identity.validate().is_err());
    }

    #[test]
    fn replay_provenance_does_not_mutate_authored_result() {
        let answer = DispatchEffectAnswer::Bare {
            result: serde_json::json!({"answer": 42}),
        };
        let digest = answer.digest().unwrap();
        let replay = answer.replay_leaf_envelope(&"ab".repeat(32)).unwrap();
        assert_eq!(replay["result"], serde_json::json!({"answer": 42}));
        assert_eq!(answer.digest().unwrap(), digest);
        assert_eq!(replay["replayed_from"], "ab".repeat(32));
    }

    #[test]
    fn observation_clock_emits_the_contracts_canonical_timestamp() {
        let timestamp = canonical_observation_timestamp_now();
        parse_canonical_timestamp(&timestamp).unwrap();
        assert!(timestamp.ends_with('Z'));
        assert_eq!(timestamp.len(), "2026-08-11T00:00:00.000Z".len());
    }
}
