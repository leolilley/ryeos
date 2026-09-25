//! Signed process placement, independent of daemon routing and suitability.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// A node binding identifier selects installed authority, never an executable,
/// provider URL, account, credential, or alternate workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionEndpointRequirement {
    Local {},
    External {
        binding_id: String,
        stdout_max_bytes: u64,
        stderr_max_bytes: u64,
    },
}

impl ExecutionEndpointRequirement {
    pub fn validate(&self) -> Result<()> {
        if let Self::External {
            binding_id,
            stdout_max_bytes,
            stderr_max_bytes,
        } = self
        {
            validate_binding_id(binding_id)?;
            ryeos_external_execution_contract::ExternalExecutionMode::DirectCommand {
                stdout_max_bytes: *stdout_max_bytes,
                stderr_max_bytes: *stderr_max_bytes,
            }
            .validate()?;
        }
        Ok(())
    }
}

/// Portable identity of the exact node-signed generation selected by the app
/// before sealing a direct plan. This is testimony, not installed authority:
/// deserializing it cannot authorize provider contact or replace revalidation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalEndpointBindingIdentity {
    pub binding_id: String,
    pub binding_digest: String,
}

impl ExternalEndpointBindingIdentity {
    pub fn validate(&self) -> Result<()> {
        validate_binding_id(&self.binding_id)?;
        ensure!(
            self.binding_digest.len() == 64
                && self
                    .binding_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "external endpoint binding digest is not canonical"
        );
        Ok(())
    }
}

fn validate_binding_id(value: &str) -> Result<()> {
    // External node bindings have flat path-derived IDs. Restrict the signed
    // selector to a bounded name, with no path, URL, or whitespace syntax.
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && !matches!(value, "." | "..")
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')),
        "external endpoint binding id is not a bounded flat identifier"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_requirement_is_closed_and_bounded() {
        let value = serde_json::json!({"kind":"external","binding_id":"farm-direct",
            "stdout_max_bytes":1024,"stderr_max_bytes":1024});
        serde_json::from_value::<ExecutionEndpointRequirement>(value.clone())
            .unwrap()
            .validate()
            .unwrap();
        for id in [
            "",
            ".",
            "..",
            "https://provider",
            "nested/binding",
            "with space",
        ] {
            let mut changed = value.clone();
            changed["binding_id"] = id.into();
            assert!(
                serde_json::from_value::<ExecutionEndpointRequirement>(changed)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        for bound in [0, 64 * 1024 * 1024, u64::MAX] {
            let mut changed = value.clone();
            changed["stdout_max_bytes"] = bound.into();
            assert!(
                serde_json::from_value::<ExecutionEndpointRequirement>(changed)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        for field in ["binding_id", "stdout_max_bytes", "stderr_max_bytes"] {
            let mut changed = value.clone();
            changed.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<ExecutionEndpointRequirement>(changed).is_err());
        }
        for changed in [
            serde_json::json!({"kind":"local","binding_id":"farm-direct"}),
            serde_json::json!({"kind":"external","binding_id":"farm-direct",
                "stdout_max_bytes":1,"stderr_max_bytes":1,"binding_digest":"a".repeat(64)}),
            serde_json::json!({"kind":"site","site_id":"remote"}),
        ] {
            assert!(serde_json::from_value::<ExecutionEndpointRequirement>(changed).is_err());
        }
    }
}
