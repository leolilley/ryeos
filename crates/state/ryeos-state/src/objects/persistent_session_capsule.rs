//! Immutable admission authority for a reusable callback-free subprocess.
//!
//! This object is deliberately domain-neutral.  It retains an exact effective
//! program, direct execution closure, framed transport contract, and execution
//! realization.  The daemon may pool a process admitted by this capsule; the
//! owning adapter assigns meaning to request and response bodies.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    AdmittedExecutionClosure, AdmittedLaunchArtifactIdentity, DirectExecutableIdentity,
    ExecutionLaunchDriver, validate_trimmed_control_free,
};

pub const PERSISTENT_SESSION_CAPSULE_KIND: &str = "persistent_session_capsule";
pub const PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION: u32 = 4;

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
pub const MAX_PERSISTENT_SESSION_EXACT_PROGRAM_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionLifecycleContract {
    pub max_processes: u16,
    pub max_inflight_per_process: u16,
    pub max_address_space_bytes: u64,
    pub max_cpu_seconds: u64,
    /// Kernel RLIMIT_NPROC ceiling for the admitted process's real UID.
    pub real_uid_process_limit: u64,
    pub ready_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub idle_timeout_ms: u64,
}

impl PersistentSessionLifecycleContract {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.max_processes == 0
            || self.max_processes > 64
            || self.max_inflight_per_process != 1
            || self.max_address_space_bytes < 64 * 1024 * 1024
            || self.max_address_space_bytes > 1024 * 1024 * 1024 * 1024
            || self.max_cpu_seconds == 0
            || self.max_cpu_seconds > 7 * 24 * 60 * 60
            || self.real_uid_process_limit == 0
            || self.real_uid_process_limit > 4096
            || self.ready_timeout_ms == 0
            || self.ready_timeout_ms > 10 * 60 * 1000
            || self.request_timeout_ms == 0
            || self.request_timeout_ms > 60 * 60 * 1000
            || self.idle_timeout_ms == 0
            || self.idle_timeout_ms > 24 * 60 * 60 * 1000
        {
            anyhow::bail!("persistent-session lifecycle contract is outside substrate bounds");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionWireContract {
    pub channel_env: String,
    pub wire_protocol: String,
    pub wire_version: u32,
    pub max_frame_bytes: u32,
}

impl PersistentSessionWireContract {
    pub fn validate(&self) -> anyhow::Result<()> {
        let valid_env = !self.channel_env.is_empty()
            && self.channel_env.len() <= 128
            && self.channel_env.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_uppercase() || (index != 0 && byte.is_ascii_digit())
            });
        validate_trimmed_control_free(
            "persistent-session wire protocol",
            &self.wire_protocol,
            false,
        )?;
        if !valid_env
            || self.wire_protocol.len() > 128
            || self.wire_version == 0
            || self.max_frame_bytes == 0
            || self.max_frame_bytes > 16 * 1024 * 1024
        {
            anyhow::bail!("persistent-session wire contract is not canonical and bounded");
        }
        Ok(())
    }
}

/// Fields shared by the capsule and its admitted execution realization.
/// Keeping this projection explicit avoids a content-addressed cycle: the
/// realization commits this digest, while the final capsule points to the
/// realization object.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PersistentSessionAuthority {
    pub exact_program_hash: String,
    pub lifecycle: PersistentSessionLifecycleContract,
    pub wire: PersistentSessionWireContract,
    pub artifact_identity: AdmittedLaunchArtifactIdentity,
    pub execution_closure: AdmittedExecutionClosure,
    pub runtime_ref: String,
    pub executor_ref: String,
}

impl PersistentSessionAuthority {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::thread_snapshot::validate_canonical_hash(
            "persistent-session exact program hash",
            &self.exact_program_hash,
        )?;
        self.lifecycle.validate()?;
        self.wire.validate()?;
        self.artifact_identity.validate()?;
        self.execution_closure.validate()?;
        if self.artifact_identity.launch_driver() != ExecutionLaunchDriver::DirectItemExecutor
            || self.execution_closure.launch_driver() != ExecutionLaunchDriver::DirectItemExecutor
        {
            anyhow::bail!("persistent session must retain a direct-item execution closure");
        }
        if self.artifact_identity.executor_ref() != self.executor_ref {
            anyhow::bail!("persistent-session artifact identity contradicts executor ref");
        }
        if matches!(
            self.artifact_identity,
            AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                executable_identity: DirectExecutableIdentity::NodePolicy,
                ..
            }
        ) {
            anyhow::bail!("persistent session cannot execute a mutable node-policy command");
        }
        validate_trimmed_control_free("persistent-session runtime ref", &self.runtime_ref, false)?;
        validate_trimmed_control_free(
            "persistent-session executor ref",
            &self.executor_ref,
            false,
        )?;
        Ok(())
    }

    pub fn digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }

    pub fn artifact_identity_digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(&self.artifact_identity)?)?.as_bytes(),
        ))
    }

    pub fn execution_closure_digest(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(&self.execution_closure)?)?.as_bytes(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedPersistentSessionCapsule {
    pub schema: u32,
    pub kind: String,
    pub exact_program: Value,
    pub exact_program_hash: String,
    pub lifecycle: PersistentSessionLifecycleContract,
    pub wire: PersistentSessionWireContract,
    pub artifact_identity: AdmittedLaunchArtifactIdentity,
    pub execution_closure: AdmittedExecutionClosure,
    pub execution_realization_hash: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub source_binding_hash: Option<String>,
    /// Admission-compiled identity for the closed structured-session protocol
    /// family. Other persistent-session protocol families retain `null`.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub structured_session_profile: Option<AdmittedStructuredSessionProfile>,
    pub runtime_ref: String,
    pub executor_ref: String,
}

impl AdmittedPersistentSessionCapsule {
    pub fn authority(&self) -> PersistentSessionAuthority {
        PersistentSessionAuthority {
            exact_program_hash: self.exact_program_hash.clone(),
            lifecycle: self.lifecycle.clone(),
            wire: self.wire.clone(),
            artifact_identity: self.artifact_identity.clone(),
            execution_closure: self.execution_closure.clone(),
            runtime_ref: self.runtime_ref.clone(),
            executor_ref: self.executor_ref.clone(),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION
            || self.kind != PERSISTENT_SESSION_CAPSULE_KIND
        {
            anyhow::bail!("invalid persistent-session capsule wire identity");
        }
        if !self.exact_program.is_object() {
            anyhow::bail!("persistent-session exact program must be an object");
        }
        let canonical = lillux::canonical_json(&self.exact_program)?;
        if canonical.len() > MAX_PERSISTENT_SESSION_EXACT_PROGRAM_BYTES {
            anyhow::bail!("persistent-session exact program exceeds its retained byte bound");
        }
        let observed = lillux::sha256_hex(canonical.as_bytes());
        if observed != self.exact_program_hash {
            anyhow::bail!("persistent-session exact program hash mismatch");
        }
        self.authority().validate()?;
        super::thread_snapshot::validate_canonical_hash(
            "persistent-session execution realization hash",
            &self.execution_realization_hash,
        )?;
        let admitted = self
            .exact_program
            .get("resolution_output")
            .and_then(|resolution| resolution.get("composed"))
            .and_then(|composed| composed.get("derived"))
            .and_then(|derived| derived.get(super::SOURCE_CLOSURE_DERIVED_KEY))
            .map(super::EffectiveSourceClosureProjection::from_value)
            .transpose()?
            .map(|projection| projection.binding_hash);
        if self.source_binding_hash != admitted {
            anyhow::bail!("persistent-session source binding contradicts its exact program");
        }
        if let Some(hash) = &self.source_binding_hash {
            super::thread_snapshot::validate_canonical_hash(
                "persistent-session source binding",
                hash,
            )?;
        }
        if let Some(profile) = &self.structured_session_profile {
            profile.validate()?;
            if self.wire.wire_protocol != "ryeos.structured-session" {
                anyhow::bail!("structured-session profile is attached to another wire protocol");
            }
        } else if self.wire.wire_protocol == "ryeos.structured-session" {
            anyhow::bail!("structured-session capsule has no admitted profile identity");
        }
        Ok(())
    }

    pub fn from_current_value(value: &Value) -> anyhow::Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("persistent-session capsule must be an object"))?;
        let kind = object.get("kind").and_then(Value::as_str).unwrap_or("");
        if kind != PERSISTENT_SESSION_CAPSULE_KIND {
            anyhow::bail!("unexpected persistent-session capsule kind: {kind}");
        }
        let schema = object.get("schema").and_then(Value::as_u64).unwrap_or(0);
        if schema != u64::from(PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION) {
            return Err(super::IncompatibleCurrentObjectSchema::new(
                "persistent-session capsule",
                schema,
                PERSISTENT_SESSION_CAPSULE_SCHEMA_VERSION,
            )
            .into());
        }
        let capsule: Self = serde_json::from_value(value.clone())?;
        capsule.validate()?;
        Ok(capsule)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&self.to_value()?)?.as_bytes(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedStructuredSessionProfile {
    pub profile_hash: String,
    /// The complete canonical, admission-compiled protocol contract.  This is
    /// authority-bearing policy, not a hint for the workload to reinterpret.
    pub contract: serde_json::Value,
    pub schema_hashes: std::collections::BTreeMap<String, String>,
    pub baseline_source: String,
    pub baseline_destination: String,
}

impl AdmittedStructuredSessionProfile {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::thread_snapshot::validate_canonical_hash(
            "structured-session profile hash",
            &self.profile_hash,
        )?;
        let contract = self
            .contract
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("structured-session contract is not an object"))?;
        if contract.is_empty() {
            anyhow::bail!("structured-session contract is empty");
        }
        let canonical = lillux::canonical_json(&self.contract)?;
        if canonical.len() > 64 * 1024
            || lillux::sha256_hex(canonical.as_bytes()) != self.profile_hash
        {
            anyhow::bail!("structured-session contract contradicts its admitted hash");
        }
        if self.schema_hashes.is_empty() || self.schema_hashes.len() > 512 {
            anyhow::bail!("structured-session schema identity set is empty or too large");
        }
        for (identity, hash) in &self.schema_hashes {
            let path = std::path::Path::new(identity);
            if identity.len() > 4096
                || path.is_absolute()
                || path.as_os_str().is_empty()
                || path
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
            {
                anyhow::bail!("structured-session schema identity is not a safe local path");
            }
            super::thread_snapshot::validate_canonical_hash(
                "structured-session schema hash",
                hash,
            )?;
        }
        for (label, value) in [
            ("structured-session baseline source", &self.baseline_source),
            (
                "structured-session baseline destination",
                &self.baseline_destination,
            ),
        ] {
            let mut components = std::path::Path::new(value).components();
            if value.len() > 128
                || !matches!(components.next(), Some(std::path::Component::Normal(_)))
                || components.next().is_some()
            {
                anyhow::bail!("{label} is not one bounded relative file name");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_refuses_unimplemented_process_multiplexing() {
        let contract = PersistentSessionLifecycleContract {
            max_processes: 1,
            max_inflight_per_process: 2,
            max_address_space_bytes: 64 * 1024 * 1024,
            max_cpu_seconds: 1,
            real_uid_process_limit: 1,
            ready_timeout_ms: 1,
            request_timeout_ms: 1,
            idle_timeout_ms: 1,
        };
        assert!(contract.validate().is_err());
    }

    #[test]
    fn predecessor_capsule_schema_is_refused_without_translation() {
        let value = serde_json::json!({
            "schema": 1,
            "kind": PERSISTENT_SESSION_CAPSULE_KIND
        });
        let error = AdmittedPersistentSessionCapsule::from_current_value(&value).unwrap_err();
        assert!(error.to_string().contains("schema"), "got: {error:#}");
    }
}
