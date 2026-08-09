//! Boot-stable identity of the node execution substrate.
//!
//! This object contains only facts that honestly apply to every workload on
//! the node. Program/runtime/interpreter/backend/numerics/artifact identity is
//! launch-scoped and belongs in an admitted execution realization, never in
//! this node-global coordinate.

use anyhow::bail;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const EXECUTION_IDENTITY_KIND: &str = "execution_identity";
pub const EXECUTION_IDENTITY_SCHEMA_VERSION: u32 = 2;
pub const MAX_EXECUTION_IDENTITY_BYTES: usize = 64 * 1024;
pub const EXECUTION_IDENTITY_ATTESTATION_CLAIM: &str = "ryeos.node.execution_substrate";
pub const EXECUTION_IDENTITY_ATTESTATION_POLICY: &str = "node-execution-substrate/current";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSubstrateBuild {
    pub version: String,
    pub revision: String,
    pub build_date: String,
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOperatingSystemIdentity {
    pub family: String,
    pub architecture: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCpuIdentity {
    /// Exact model text reported by the kernel when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Canonically sorted, duplicate-free codegen-relevant feature set.
    pub features: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionIdentity {
    pub schema: u32,
    pub kind: String,
    pub daemon: ExecutionSubstrateBuild,
    pub operating_system: ExecutionOperatingSystemIdentity,
    pub cpu: ExecutionCpuIdentity,
    /// Fingerprint of the node signer that attests and publishes this object.
    pub node_signer_fingerprint: String,
}

fn validate_text(field: &str, value: &str) -> anyhow::Result<()> {
    super::validate_trimmed_control_free(field, value, false)
}

fn require_hex64(field: &str, value: &str) -> anyhow::Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{field} must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

impl ExecutionIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.kind != EXECUTION_IDENTITY_KIND {
            bail!("unexpected execution identity kind: {}", self.kind);
        }
        if self.schema != EXECUTION_IDENTITY_SCHEMA_VERSION {
            bail!(
                "unexpected execution identity schema: {} (current {})",
                self.schema,
                EXECUTION_IDENTITY_SCHEMA_VERSION
            );
        }
        for (field, value) in [
            ("daemon version", &self.daemon.version),
            ("daemon revision", &self.daemon.revision),
            ("daemon build date", &self.daemon.build_date),
            ("daemon profile", &self.daemon.profile),
            ("operating-system family", &self.operating_system.family),
            (
                "operating-system architecture",
                &self.operating_system.architecture,
            ),
        ] {
            validate_text(field, value)?;
        }
        if let Some(model) = &self.cpu.model {
            validate_text("CPU model", model)?;
        }
        let mut prior: Option<&str> = None;
        for feature in &self.cpu.features {
            validate_text("CPU feature", feature)?;
            if prior.is_some_and(|value| value >= feature.as_str()) {
                bail!("CPU features must be sorted and duplicate-free");
            }
            prior = Some(feature);
        }
        require_hex64("node signer fingerprint", &self.node_signer_fingerprint)?;
        let bytes = serde_json::to_vec(self)?.len();
        if bytes > MAX_EXECUTION_IDENTITY_BYTES {
            bail!("execution identity exceeds {MAX_EXECUTION_IDENTITY_BYTES} bytes");
        }
        Ok(())
    }

    pub fn identity_digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        let canonical = lillux::canonical_json(&serde_json::to_value(self)?)?;
        Ok(lillux::cas::sha256_hex(canonical.as_bytes()))
    }

    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("execution identity has no string kind"))?;
        if kind != EXECUTION_IDENTITY_KIND {
            bail!("unexpected execution identity kind: {kind}");
        }
        let schema = value
            .get("schema")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("execution identity has no numeric schema"))?;
        if schema != u64::from(EXECUTION_IDENTITY_SCHEMA_VERSION) {
            return Err(super::IncompatibleCurrentObjectSchema::new(
                "execution identity",
                schema,
                EXECUTION_IDENTITY_SCHEMA_VERSION,
            )
            .into());
        }
        let identity: Self = serde_json::from_value(value.clone())?;
        identity.validate()?;
        Ok(identity)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ExecutionIdentity {
        ExecutionIdentity {
            schema: EXECUTION_IDENTITY_SCHEMA_VERSION,
            kind: EXECUTION_IDENTITY_KIND.to_owned(),
            daemon: ExecutionSubstrateBuild {
                version: "1.2.3".to_owned(),
                revision: "abc".to_owned(),
                build_date: "2026-08-08".to_owned(),
                profile: "release".to_owned(),
            },
            operating_system: ExecutionOperatingSystemIdentity {
                family: "unix".to_owned(),
                architecture: "x86_64".to_owned(),
            },
            cpu: ExecutionCpuIdentity {
                model: Some("Example CPU".to_owned()),
                features: vec!["avx2".to_owned(), "sse4_2".to_owned()],
            },
            node_signer_fingerprint: "a".repeat(64),
        }
    }

    #[test]
    fn current_identity_round_trips_and_is_component_sensitive() {
        let value = identity().to_value().unwrap();
        assert_eq!(
            ExecutionIdentity::from_current_value(&value).unwrap(),
            identity()
        );
        let base = identity().identity_digest().unwrap();
        let mut changed = identity();
        changed.cpu.features.push("xsave".to_owned());
        assert_ne!(base, changed.identity_digest().unwrap());
    }

    #[test]
    fn predecessor_schema_and_noncanonical_features_refuse() {
        let mut value = identity().to_value().unwrap();
        value["schema"] = serde_json::json!(1);
        assert!(ExecutionIdentity::from_current_value(&value).is_err());
        let mut invalid = identity();
        invalid.cpu.features.reverse();
        assert!(invalid.validate().is_err());
    }
}
