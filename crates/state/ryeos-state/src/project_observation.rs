//! Canonical contract for source-scoped project observations.
//!
//! The contract lives in state because the durable event projection must
//! validate and index it without depending on a runtime crate. Higher layers
//! re-export the same types; there is no second decoder.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const PROJECT_OBSERVATION_SCHEMA: &str = "ryeos.project_observation.v1";
pub const MAX_PROJECT_OBSERVATION_NAMESPACE_BYTES: usize = 128;
pub const MAX_PROJECT_OBSERVATION_STABLE_ID_BYTES: usize = 256;
pub const MAX_PROJECT_OBSERVATION_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_PROJECT_OBSERVATION_JSON_DEPTH: usize = 32;
pub const MAX_PROJECT_OBSERVATION_JSON_VALUES: usize = 4_096;
pub const MAX_PROJECT_OBSERVATIONS_PER_ACTION: usize = 256;

/// Meaning-blind request returned by a graph action. The graph runtime adds
/// the occurrence coordinate and the daemon binds the admitted source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectObservationRequest {
    pub namespace: String,
    pub stable_id: String,
    pub payload: Value,
}

impl ProjectObservationRequest {
    pub fn from_value(value: Value) -> Result<Self> {
        let request: Self = serde_json::from_value(value)
            .context("project observation request is not the exact current contract")?;
        request.validate()?;
        Ok(request)
    }

    pub fn validate(&self) -> Result<()> {
        validate_namespaced_identifier(
            &self.namespace,
            MAX_PROJECT_OBSERVATION_NAMESPACE_BYTES,
            "project observation namespace",
        )?;
        validate_stable_identifier(
            &self.stable_id,
            MAX_PROJECT_OBSERVATION_STABLE_ID_BYTES,
            "project observation stable_id",
        )?;
        validate_bounded_json(
            &self.payload,
            MAX_PROJECT_OBSERVATION_PAYLOAD_BYTES,
            MAX_PROJECT_OBSERVATION_JSON_DEPTH,
            MAX_PROJECT_OBSERVATION_JSON_VALUES,
            "project observation payload",
        )
        .map_err(anyhow::Error::msg)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectObservationOccurrence {
    pub graph_run_id: String,
    pub node: String,
    pub step: u32,
}

impl ProjectObservationOccurrence {
    pub fn validate(&self) -> Result<()> {
        validate_stable_identifier(&self.graph_run_id, 256, "project observation graph_run_id")?;
        validate_stable_identifier(&self.node, 256, "project observation node")
    }
}

/// Canonical daemon-authored durable event. Source identity and
/// `observation_id` never come from runtime input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectObservationRecordedPayload {
    pub schema_version: String,
    pub observation_id: String,
    pub namespace: String,
    pub stable_id: String,
    pub source_definition_ref: String,
    pub source_effective_definition_digest: String,
    pub occurrence: ProjectObservationOccurrence,
    pub payload_fingerprint: String,
    pub payload: Value,
}

impl ProjectObservationRecordedPayload {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != PROJECT_OBSERVATION_SCHEMA {
            bail!(
                "project observation schema is {}, expected {PROJECT_OBSERVATION_SCHEMA}",
                self.schema_version
            );
        }
        validate_source_definition(
            &self.source_definition_ref,
            &self.source_effective_definition_digest,
        )?;
        validate_stable_identifier(
            &self.observation_id,
            128,
            "project observation observation_id",
        )?;
        ProjectObservationRequest {
            namespace: self.namespace.clone(),
            stable_id: self.stable_id.clone(),
            payload: self.payload.clone(),
        }
        .validate()?;
        self.occurrence.validate()?;
        if !lillux::valid_hash(&self.payload_fingerprint) {
            bail!("project observation payload_fingerprint is invalid");
        }
        let canonical = lillux::canonical_json(&self.payload)
            .context("canonicalize project observation payload")?;
        let observed = lillux::sha256_hex(canonical.as_bytes());
        if observed != self.payload_fingerprint {
            bail!("project observation payload fingerprint mismatch");
        }
        Ok(())
    }

    pub fn validate_for_chain(&self, chain_root_id: &str) -> Result<()> {
        self.validate()?;
        let expected = project_observation_id(
            chain_root_id,
            &self.source_definition_ref,
            &self.source_effective_definition_digest,
            &self.namespace,
            &self.stable_id,
        )?;
        if self.observation_id != expected {
            bail!("project observation ID does not match its chain-scoped identity seed");
        }
        Ok(())
    }
}

pub fn project_observation_id(
    chain_root_id: &str,
    source_definition_ref: &str,
    source_effective_definition_digest: &str,
    namespace: &str,
    stable_id: &str,
) -> Result<String> {
    validate_stable_identifier(chain_root_id, 256, "project observation chain_root_id")?;
    validate_source_definition(source_definition_ref, source_effective_definition_digest)?;
    validate_namespaced_identifier(
        namespace,
        MAX_PROJECT_OBSERVATION_NAMESPACE_BYTES,
        "project observation namespace",
    )?;
    validate_stable_identifier(
        stable_id,
        MAX_PROJECT_OBSERVATION_STABLE_ID_BYTES,
        "project observation stable_id",
    )?;
    let seed = json!({
        "chain_root_id": chain_root_id,
        "source_definition_ref": source_definition_ref,
        "source_effective_definition_digest": source_effective_definition_digest,
        "namespace": namespace,
        "stable_id": stable_id,
    });
    let canonical =
        lillux::canonical_json(&seed).context("canonicalize project observation identity seed")?;
    Ok(format!(
        "project-observation:{}",
        lillux::sha256_hex(canonical.as_bytes())
    ))
}

fn validate_source_definition(definition_ref: &str, effective_digest: &str) -> Result<()> {
    if definition_ref.trim().is_empty()
        || definition_ref.len() > 1_024
        || definition_ref.chars().any(char::is_control)
    {
        bail!("project observation source_definition_ref is invalid");
    }
    if !lillux::valid_hash(effective_digest) {
        bail!("project observation source effective definition digest is invalid");
    }
    Ok(())
}

fn validate_namespaced_identifier(value: &str, max_bytes: usize, label: &str) -> Result<()> {
    if value.is_empty() || value.len() > max_bytes {
        bail!("{label} must be 1..={max_bytes} bytes");
    }
    let mut segments = value.split('.');
    let mut count = 0usize;
    for segment in &mut segments {
        count += 1;
        let mut bytes = segment.bytes();
        if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
            || !bytes.all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
            })
        {
            bail!("{label} must use lowercase namespaced segments");
        }
    }
    if count < 2 {
        bail!("{label} must be namespaced");
    }
    Ok(())
}

fn validate_stable_identifier(value: &str, max_bytes: usize, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        bail!("{label} must be a nonempty canonical string of at most {max_bytes} bytes");
    }
    Ok(())
}

fn validate_bounded_json(
    value: &Value,
    max_bytes: usize,
    max_depth: usize,
    max_values: usize,
    label: &str,
) -> std::result::Result<(), String> {
    let canonical = lillux::canonical_json(value)
        .map_err(|error| format!("{label} cannot be represented as canonical JSON: {error}"))?;
    if canonical.len() > max_bytes {
        return Err(format!(
            "{label} is {} bytes; maximum is {max_bytes}",
            canonical.len()
        ));
    }
    let mut stack = vec![(value, 1usize)];
    let mut values = 0usize;
    while let Some((current, depth)) = stack.pop() {
        values = values.saturating_add(1);
        if values > max_values {
            return Err(format!("{label} exceeds {max_values} JSON values"));
        }
        if depth > max_depth {
            return Err(format!("{label} exceeds {max_depth} JSON levels"));
        }
        match current {
            Value::Array(items) => stack.extend(items.iter().map(|item| (item, depth + 1))),
            Value::Object(items) => {
                stack.extend(items.values().map(|item| (item, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_strict_namespaced_and_bounded() {
        let request = ProjectObservationRequest::from_value(json!({
            "namespace": "example.classified",
            "stable_id": "classification:abc",
            "payload": {"status": "pending"},
        }))
        .unwrap();
        assert_eq!(request.namespace, "example.classified");
        assert!(
            ProjectObservationRequest::from_value(json!({
                "namespace": "not_namespaced",
                "stable_id": "x",
                "payload": null,
            }))
            .is_err()
        );
    }
}
