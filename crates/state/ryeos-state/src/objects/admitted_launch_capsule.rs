use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::{
    validate_trimmed_control_free, ExecutionLaunchDriver, ExecutionLifecycleAuthority,
    ExecutionProjectAuthority, ExecutionRecoveryAuthority,
};

pub const ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION: u32 = 3;
pub const LEGACY_ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub enum DirectExecutableIdentity {
    VerifiedContent {
        content_hash: String,
    },
    /// The exact command spelling remains sealed in `execution_plan_hash`, but
    /// executable authorization comes from the node's signed isolation policy
    /// rather than a bundle/CAS content identity. This driver is not eligible
    /// for autonomous restart recovery.
    NodePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectRuntimeSourceSpace {
    Project,
    Bundle,
}

/// Exact runtime descriptor identity selected by the executor-chain build for
/// a direct item launch. Optional only so schema-v2 rows remain decodable;
/// every schema-v3 direct capsule must carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectRuntimeIdentity {
    pub runtime_ref: String,
    pub runtime_source_space: DirectRuntimeSourceSpace,
    pub runtime_content_hash: String,
    pub runtime_signer_fingerprint: String,
    pub runtime_bundle_manifest_hash: Option<String>,
    pub runtime_bundle_signer_fingerprint: Option<String>,
}

/// Exact installed code closure selected for one admitted launch. References
/// remain useful diagnostics, but recovery authorization comes from these
/// verified content identities rather than from re-looking up those names in
/// the current registries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdmittedLaunchArtifactIdentity {
    ManagedRuntime {
        runtime_ref: String,
        runtime_content_hash: String,
        runtime_signer_fingerprint: String,
        protocol_ref: String,
        protocol_content_hash: String,
        protocol_signer_fingerprint: String,
        executor_ref: String,
        executor_content_hash: String,
        executor_bundle_manifest_hash: String,
        executor_bundle_signer_fingerprint: String,
    },
    DirectItemExecutor {
        executor_ref: String,
        executor_item_content_hash: String,
        executor_item_signer_fingerprint: Option<String>,
        executor_bundle_manifest_hash: Option<String>,
        executor_bundle_signer_fingerprint: Option<String>,
        protocol_ref: String,
        protocol_content_hash: String,
        protocol_signer_fingerprint: String,
        execution_plan_hash: String,
        executable_identity: DirectExecutableIdentity,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_identity: Option<DirectRuntimeIdentity>,
    },
}

impl AdmittedLaunchArtifactIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        let validate_hash = |label: &str, value: &str| {
            super::thread_snapshot::validate_canonical_hash(label, value)
        };
        match self {
            Self::ManagedRuntime {
                runtime_ref,
                runtime_content_hash,
                runtime_signer_fingerprint,
                protocol_ref,
                protocol_content_hash,
                protocol_signer_fingerprint,
                executor_ref,
                executor_content_hash,
                executor_bundle_manifest_hash,
                executor_bundle_signer_fingerprint,
            } => {
                for (label, value) in [
                    ("runtime ref", runtime_ref),
                    ("runtime signer", runtime_signer_fingerprint),
                    ("protocol ref", protocol_ref),
                    ("protocol signer", protocol_signer_fingerprint),
                    ("executor ref", executor_ref),
                    ("executor bundle signer", executor_bundle_signer_fingerprint),
                ] {
                    validate_trimmed_control_free(label, value, false)?;
                }
                for (label, value) in [
                    ("runtime content hash", runtime_content_hash),
                    ("protocol content hash", protocol_content_hash),
                    ("executor content hash", executor_content_hash),
                    (
                        "executor bundle manifest hash",
                        executor_bundle_manifest_hash,
                    ),
                ] {
                    validate_hash(label, value)?;
                }
            }
            Self::DirectItemExecutor {
                executor_ref,
                executor_item_content_hash,
                executor_item_signer_fingerprint,
                executor_bundle_manifest_hash,
                executor_bundle_signer_fingerprint,
                protocol_ref,
                protocol_content_hash,
                protocol_signer_fingerprint,
                execution_plan_hash,
                executable_identity,
                runtime_identity,
            } => {
                validate_trimmed_control_free("executor ref", executor_ref, false)?;
                validate_hash("executor item content hash", executor_item_content_hash)?;
                if let Some(signer) = executor_item_signer_fingerprint {
                    validate_trimmed_control_free("executor item signer", signer, false)?;
                }
                match (
                    executor_bundle_manifest_hash,
                    executor_bundle_signer_fingerprint,
                ) {
                    (Some(hash), Some(signer)) => {
                        validate_hash("executor bundle manifest hash", hash)?;
                        validate_trimmed_control_free("executor bundle signer", signer, false)?;
                    }
                    (None, None) => {}
                    _ => anyhow::bail!("executor bundle identity must be complete or absent"),
                }
                validate_trimmed_control_free("protocol ref", protocol_ref, false)?;
                validate_hash("protocol content hash", protocol_content_hash)?;
                validate_trimmed_control_free(
                    "protocol signer",
                    protocol_signer_fingerprint,
                    false,
                )?;
                validate_hash("execution plan hash", execution_plan_hash)?;
                if let DirectExecutableIdentity::VerifiedContent { content_hash } =
                    executable_identity
                {
                    validate_hash("verified executable content hash", content_hash)?;
                }
                if let Some(runtime) = runtime_identity {
                    validate_trimmed_control_free(
                        "direct runtime ref",
                        &runtime.runtime_ref,
                        false,
                    )?;
                    match runtime.runtime_source_space {
                        DirectRuntimeSourceSpace::Bundle
                            if runtime.runtime_bundle_manifest_hash.is_none()
                                || runtime.runtime_bundle_signer_fingerprint.is_none() =>
                        {
                            anyhow::bail!(
                                "bundle-backed direct runtime has no complete source-bundle generation identity"
                            )
                        }
                        DirectRuntimeSourceSpace::Project
                            if runtime.runtime_bundle_manifest_hash.is_some()
                                || runtime.runtime_bundle_signer_fingerprint.is_some() =>
                        {
                            anyhow::bail!(
                                "project direct runtime cannot carry a bundle generation identity"
                            )
                        }
                        _ => {}
                    }
                    validate_hash("direct runtime content hash", &runtime.runtime_content_hash)?;
                    validate_trimmed_control_free(
                        "direct runtime signer",
                        &runtime.runtime_signer_fingerprint,
                        false,
                    )?;
                    match (
                        &runtime.runtime_bundle_manifest_hash,
                        &runtime.runtime_bundle_signer_fingerprint,
                    ) {
                        (Some(hash), Some(signer)) => {
                            validate_hash("direct runtime bundle manifest hash", hash)?;
                            validate_trimmed_control_free(
                                "direct runtime bundle signer",
                                signer,
                                false,
                            )?;
                        }
                        (None, None) => {}
                        _ => anyhow::bail!(
                            "direct runtime bundle identity must be complete or absent"
                        ),
                    }
                }
            }
        }
        Ok(())
    }

    pub fn launch_driver(&self) -> ExecutionLaunchDriver {
        match self {
            Self::ManagedRuntime { .. } => ExecutionLaunchDriver::ManagedRuntime,
            Self::DirectItemExecutor { .. } => ExecutionLaunchDriver::DirectItemExecutor,
        }
    }

    pub fn runtime_ref(&self) -> Option<&str> {
        match self {
            Self::ManagedRuntime { runtime_ref, .. } => Some(runtime_ref),
            Self::DirectItemExecutor { .. } => None,
        }
    }

    pub fn executor_ref(&self) -> &str {
        match self {
            Self::ManagedRuntime { executor_ref, .. }
            | Self::DirectItemExecutor { executor_ref, .. } => executor_ref,
        }
    }
}

/// Secret-free, content-addressed closure of the authority that crossed one
/// managed execution's first-launch boundary. Recovery consumes the exact
/// program payload; it never asks mutable project or bundle space to recreate
/// an earlier admission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedLaunchCapsule {
    pub schema: u32,
    pub kind: String,
    pub exact_program: serde_json::Value,
    pub exact_program_hash: String,
    pub sealed_invocation: serde_json::Value,
    pub project_authority: ExecutionProjectAuthority,
    pub lifecycle_authority: ExecutionLifecycleAuthority,
    pub launch_driver: ExecutionLaunchDriver,
    pub artifact_identity: AdmittedLaunchArtifactIdentity,
    /// Exact secret-free output of launch preparation. Managed recovery
    /// consumes this CAS-rooted value rather than re-running mutable config,
    /// binding resolution, augmentations, or launch-preparer handlers.
    pub prepared_launch: Option<serde_json::Value>,
    pub effective_caps: Vec<String>,
    pub runtime_ref: String,
    pub executor_ref: String,
}

impl AdmittedLaunchCapsule {
    /// Decode the current contract plus the one bounded rollout predecessor.
    ///
    /// Inspecting the outer identity first ensures a predecessor nested
    /// authority is rejected as an old capsule, before serde interprets any
    /// of its fields.
    pub fn from_current_value(value: serde_json::Value) -> anyhow::Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("admitted launch capsule must be an object"))?;
        let kind = object
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("admitted launch capsule has no string kind"))?;
        if kind != "admitted_launch_capsule" {
            anyhow::bail!("unexpected admitted launch capsule kind: {kind}");
        }
        let schema = object
            .get("schema")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("admitted launch capsule has no numeric schema"))?;
        if schema != u64::from(ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION)
            && schema != u64::from(LEGACY_ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION)
        {
            anyhow::bail!(
                "admitted launch capsule schema is unsupported: stored schema={schema}, current schema={ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION}"
            );
        }
        let capsule: Self =
            serde_json::from_value(value).context("deserialize current admitted launch capsule")?;
        capsule.validate()?;
        Ok(capsule)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !matches!(
            self.schema,
            LEGACY_ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION | ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION
        ) || self.kind != "admitted_launch_capsule"
        {
            anyhow::bail!("invalid admitted launch capsule wire identity");
        }
        if !self.exact_program.is_object() {
            anyhow::bail!("admitted launch capsule exact_program must be an object");
        }
        if !self.sealed_invocation.is_object() {
            anyhow::bail!("admitted launch capsule sealed_invocation must be an object");
        }
        super::thread_snapshot::validate_canonical_hash(
            "launch capsule exact program hash",
            &self.exact_program_hash,
        )?;
        let canonical_program = lillux::canonical_json(&self.exact_program)?;
        let observed_program_hash = lillux::sha256_hex(canonical_program.as_bytes());
        if observed_program_hash != self.exact_program_hash {
            anyhow::bail!(
                "admitted launch capsule exact program hash mismatch: declared {}, observed {}",
                self.exact_program_hash,
                observed_program_hash
            );
        }
        let mut invocation_program = self.sealed_invocation.clone();
        let invocation_object = invocation_program
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("sealed invocation must be an object"))?;
        for invocation_field in [
            "parameters",
            "requested_by",
            "planning_principal",
            "project_context",
            "usage_subject",
            "usage_subject_asserted_by",
        ] {
            invocation_object.remove(invocation_field).ok_or_else(|| {
                anyhow::anyhow!(
                    "admitted launch capsule sealed invocation is missing {invocation_field}"
                )
            })?;
        }
        if invocation_program != self.exact_program {
            anyhow::bail!(
                "admitted launch capsule sealed invocation does not match its exact program"
            );
        }
        self.project_authority.validate()?;
        self.lifecycle_authority.validate()?;
        self.artifact_identity.validate()?;
        if self.schema == ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION
            && matches!(
                self.artifact_identity,
                AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                    runtime_identity: None,
                    ..
                }
            )
        {
            anyhow::bail!("schema-v3 direct launch capsule has no runtime identity");
        }
        if self.lifecycle_authority.recovery == ExecutionRecoveryAuthority::RestartRecoverable
            && matches!(
                self.artifact_identity,
                AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                    executable_identity: DirectExecutableIdentity::NodePolicy,
                    ..
                }
            )
        {
            anyhow::bail!(
                "node-policy direct execution is not eligible for autonomous restart recovery"
            );
        }
        if self.artifact_identity.launch_driver() != self.launch_driver {
            anyhow::bail!("admitted launch artifact identity contradicts launch driver");
        }
        match (self.launch_driver, self.prepared_launch.as_ref()) {
            (ExecutionLaunchDriver::ManagedRuntime, Some(value)) if value.is_object() => {}
            (ExecutionLaunchDriver::ManagedRuntime, _) => {
                anyhow::bail!("managed admitted launch capsule has no prepared launch object")
            }
            (ExecutionLaunchDriver::DirectItemExecutor, None) => {}
            (ExecutionLaunchDriver::DirectItemExecutor, Some(_)) => anyhow::bail!(
                "direct admitted launch capsule cannot carry managed prepared launch state"
            ),
        }
        if self.artifact_identity.executor_ref() != self.executor_ref {
            anyhow::bail!("admitted launch artifact identity contradicts executor ref");
        }
        if self.launch_driver == ExecutionLaunchDriver::ManagedRuntime
            && self.artifact_identity.runtime_ref() != Some(self.runtime_ref.as_str())
        {
            anyhow::bail!("admitted launch artifact identity contradicts runtime ref");
        }
        validate_trimmed_control_free("launch capsule runtime ref", &self.runtime_ref, false)?;
        validate_trimmed_control_free("launch capsule executor ref", &self.executor_ref, false)?;
        let mut canonical_caps = self.effective_caps.clone();
        for capability in &canonical_caps {
            validate_trimmed_control_free("launch capsule capability", capability, false)?;
        }
        canonical_caps.sort();
        canonical_caps.dedup();
        if canonical_caps != self.effective_caps {
            anyhow::bail!("admitted launch capsule capabilities are not canonical");
        }
        Ok(())
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("admitted launch capsule serialization cannot fail")
    }

    pub fn content_hash(&self) -> anyhow::Result<String> {
        self.validate()?;
        let canonical = lillux::canonical_json(&self.to_value())?;
        Ok(lillux::sha256_hex(canonical.as_bytes()))
    }

    /// Compare the immutable admission authority shared by continuation
    /// segments. `sealed_invocation` records the birth segment's realization
    /// and remains rooted in the original capsule; a later operational
    /// invocation is validated separately and may point at the chain's next
    /// pinned-COW realization without minting a replacement capsule.
    pub fn same_continuation_admission(&self, other: &Self) -> anyhow::Result<bool> {
        self.validate()?;
        other.validate()?;
        let schema_compatible = self.schema == other.schema
            || (matches!(
                (&self.artifact_identity, &other.artifact_identity),
                (
                    AdmittedLaunchArtifactIdentity::ManagedRuntime { .. },
                    AdmittedLaunchArtifactIdentity::ManagedRuntime { .. }
                )
            ) && matches!(
                (self.schema, other.schema),
                (
                    LEGACY_ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
                    ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION
                ) | (
                    ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
                    LEGACY_ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION
                )
            ));
        Ok(schema_compatible
            && self.kind == other.kind
            && self.exact_program == other.exact_program
            && self.exact_program_hash == other.exact_program_hash
            && self.project_authority == other.project_authority
            && self.lifecycle_authority == other.lifecycle_authority
            && self.launch_driver == other.launch_driver
            && self.artifact_identity == other.artifact_identity
            && self.prepared_launch == other.prepared_launch
            && self.effective_caps == other.effective_caps
            && self.runtime_ref == other.runtime_ref
            && self.executor_ref == other.executor_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{
        ExecutionOwnershipAuthority, ExecutionProjectAuthority, ExecutionRecoveryAuthority,
    };

    fn direct_capsule(executable_identity: DirectExecutableIdentity) -> AdmittedLaunchCapsule {
        let exact_program = serde_json::json!({
            "item_ref": "tool:test/run",
            "runtime_ref": "runtime:direct",
            "executor_ref": "tool:test/executor",
        });
        let exact_program_hash =
            lillux::sha256_hex(lillux::canonical_json(&exact_program).unwrap().as_bytes());
        let mut sealed_invocation = exact_program.clone();
        let object = sealed_invocation.as_object_mut().unwrap();
        for field in [
            "parameters",
            "requested_by",
            "planning_principal",
            "project_context",
            "usage_subject",
            "usage_subject_asserted_by",
        ] {
            object.insert(field.to_string(), serde_json::Value::Null);
        }
        AdmittedLaunchCapsule {
            schema: ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
            kind: "admitted_launch_capsule".to_string(),
            exact_program,
            exact_program_hash,
            sealed_invocation,
            project_authority: ExecutionProjectAuthority::PROJECTLESS,
            lifecycle_authority: ExecutionLifecycleAuthority {
                ownership: ExecutionOwnershipAuthority::DaemonOwned,
                recovery: ExecutionRecoveryAuthority::RestartRecoverable,
            },
            launch_driver: ExecutionLaunchDriver::DirectItemExecutor,
            artifact_identity: AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                executor_ref: "tool:test/executor".to_string(),
                executor_item_content_hash: "a".repeat(64),
                executor_item_signer_fingerprint: Some("fp:test".to_string()),
                executor_bundle_manifest_hash: Some("b".repeat(64)),
                executor_bundle_signer_fingerprint: Some("fp:bundle".to_string()),
                protocol_ref: "protocol:test/direct".to_string(),
                protocol_content_hash: "c".repeat(64),
                protocol_signer_fingerprint: "fp:protocol".to_string(),
                execution_plan_hash: "e".repeat(64),
                executable_identity,
                runtime_identity: Some(DirectRuntimeIdentity {
                    runtime_ref: "tool:test/runtime".to_string(),
                    runtime_source_space: DirectRuntimeSourceSpace::Bundle,
                    runtime_content_hash: "f".repeat(64),
                    runtime_signer_fingerprint: "fp:runtime".to_string(),
                    runtime_bundle_manifest_hash: Some("1".repeat(64)),
                    runtime_bundle_signer_fingerprint: Some("fp:bundle".to_string()),
                }),
            },
            prepared_launch: None,
            effective_caps: vec!["ryeos.read.project.live".to_string()],
            runtime_ref: "runtime:direct".to_string(),
            executor_ref: "tool:test/executor".to_string(),
        }
    }

    fn managed_capsule(prepared_launch: Option<serde_json::Value>) -> AdmittedLaunchCapsule {
        let exact_program = serde_json::json!({
            "item_ref": "directive:test/run",
            "runtime_ref": "runtime:test/directive",
            "executor_ref": "executor:test/subprocess",
        });
        let exact_program_hash =
            lillux::sha256_hex(lillux::canonical_json(&exact_program).unwrap().as_bytes());
        let mut sealed_invocation = exact_program.clone();
        let object = sealed_invocation.as_object_mut().unwrap();
        for field in [
            "parameters",
            "requested_by",
            "planning_principal",
            "project_context",
            "usage_subject",
            "usage_subject_asserted_by",
        ] {
            object.insert(field.to_string(), serde_json::Value::Null);
        }
        AdmittedLaunchCapsule {
            schema: ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
            kind: "admitted_launch_capsule".to_string(),
            exact_program,
            exact_program_hash,
            sealed_invocation,
            project_authority: ExecutionProjectAuthority::PROJECTLESS,
            lifecycle_authority: ExecutionLifecycleAuthority {
                ownership: ExecutionOwnershipAuthority::DaemonOwned,
                recovery: ExecutionRecoveryAuthority::RestartRecoverable,
            },
            launch_driver: ExecutionLaunchDriver::ManagedRuntime,
            artifact_identity: AdmittedLaunchArtifactIdentity::ManagedRuntime {
                runtime_ref: "runtime:test/directive".to_string(),
                runtime_content_hash: "a".repeat(64),
                runtime_signer_fingerprint: "fp:runtime".to_string(),
                protocol_ref: "protocol:test/directive".to_string(),
                protocol_content_hash: "b".repeat(64),
                protocol_signer_fingerprint: "fp:protocol".to_string(),
                executor_ref: "executor:test/subprocess".to_string(),
                executor_content_hash: "c".repeat(64),
                executor_bundle_manifest_hash: "d".repeat(64),
                executor_bundle_signer_fingerprint: "fp:executor-bundle".to_string(),
            },
            prepared_launch,
            effective_caps: vec!["ryeos.read.project.live".to_string()],
            runtime_ref: "runtime:test/directive".to_string(),
            executor_ref: "executor:test/subprocess".to_string(),
        }
    }

    #[test]
    fn restart_recovery_accepts_a_content_verified_direct_executable() {
        let capsule = direct_capsule(DirectExecutableIdentity::VerifiedContent {
            content_hash: "f".repeat(64),
        });
        capsule.validate().unwrap();
        assert_eq!(capsule.content_hash().unwrap().len(), 64);
    }

    #[test]
    fn current_decoder_rejects_predecessor_epoch_before_nested_authority_decode() {
        let mut value = direct_capsule(DirectExecutableIdentity::VerifiedContent {
            content_hash: "f".repeat(64),
        })
        .to_value();
        let object = value.as_object_mut().unwrap();
        object.insert(
            "schema".to_string(),
            serde_json::json!(LEGACY_ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION - 1),
        );
        object.insert(
            "project_authority".to_string(),
            serde_json::json!({"authority": "predecessor_shape"}),
        );

        let error = AdmittedLaunchCapsule::from_current_value(value).unwrap_err();
        assert!(
            error.to_string().contains("schema is unsupported"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn legacy_direct_capsule_preserves_absent_runtime_identity_on_round_trip() {
        let mut value = direct_capsule(DirectExecutableIdentity::VerifiedContent {
            content_hash: "f".repeat(64),
        })
        .to_value();
        let object = value.as_object_mut().unwrap();
        object.insert(
            "schema".to_string(),
            serde_json::json!(LEGACY_ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION),
        );
        object
            .get_mut("artifact_identity")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove("runtime_identity");
        let expected_hash = lillux::sha256_hex(lillux::canonical_json(&value).unwrap().as_bytes());

        let decoded = AdmittedLaunchCapsule::from_current_value(value).unwrap();
        let encoded = decoded.to_value();
        assert!(encoded["artifact_identity"]
            .get("runtime_identity")
            .is_none());
        assert_eq!(decoded.content_hash().unwrap(), expected_hash);
    }

    #[test]
    fn legacy_managed_capsule_is_semantically_compatible_with_current_schema() {
        let current = managed_capsule(Some(serde_json::json!({"prepared": true})));
        let mut legacy_value = current.to_value();
        legacy_value.as_object_mut().unwrap().insert(
            "schema".to_string(),
            serde_json::json!(LEGACY_ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION),
        );
        let legacy = AdmittedLaunchCapsule::from_current_value(legacy_value).unwrap();
        assert!(legacy.same_continuation_admission(&current).unwrap());
        assert!(current.same_continuation_admission(&legacy).unwrap());
        assert_ne!(
            legacy.content_hash().unwrap(),
            current.content_hash().unwrap()
        );
    }

    #[test]
    fn bundle_runtime_requires_its_source_bundle_generation_identity() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::VerifiedContent {
            content_hash: "9".repeat(64),
        });
        let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            runtime_identity: Some(runtime),
            ..
        } = &mut capsule.artifact_identity
        else {
            panic!("direct fixture must carry runtime identity");
        };
        runtime.runtime_bundle_manifest_hash = None;
        runtime.runtime_bundle_signer_fingerprint = None;
        let error = capsule.validate().unwrap_err();
        assert!(error
            .to_string()
            .contains("no complete source-bundle generation identity"));
    }

    #[test]
    fn current_decoder_accepts_valid_current_capsule() {
        let expected = direct_capsule(DirectExecutableIdentity::VerifiedContent {
            content_hash: "f".repeat(64),
        });
        let decoded = AdmittedLaunchCapsule::from_current_value(expected.to_value()).unwrap();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn current_decoder_requires_numeric_epoch_before_typed_decode() {
        let mut value = direct_capsule(DirectExecutableIdentity::VerifiedContent {
            content_hash: "f".repeat(64),
        })
        .to_value();
        value.as_object_mut().unwrap().insert(
            "schema".to_string(),
            serde_json::json!(ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION.to_string()),
        );

        let error = AdmittedLaunchCapsule::from_current_value(value).unwrap_err();
        assert!(
            error.to_string().contains("no numeric schema"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn restart_recovery_rejects_a_node_policy_direct_executable() {
        let capsule = direct_capsule(DirectExecutableIdentity::NodePolicy);
        let error = capsule.validate().unwrap_err();
        assert!(error
            .to_string()
            .contains("not eligible for autonomous restart recovery"));
    }

    #[test]
    fn request_scoped_execution_accepts_a_node_policy_direct_executable() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::NodePolicy);
        capsule.lifecycle_authority = ExecutionLifecycleAuthority::REQUEST_SCOPED;
        capsule.validate().unwrap();
    }

    #[test]
    fn exact_program_hash_is_verified_not_trusted() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::VerifiedContent {
            content_hash: "f".repeat(64),
        });
        capsule.exact_program_hash = "0".repeat(64);
        assert!(capsule
            .validate()
            .unwrap_err()
            .to_string()
            .contains("mismatch"));
    }

    #[test]
    fn managed_recovery_requires_and_accepts_exact_prepared_launch_state() {
        let capsule = managed_capsule(Some(serde_json::json!({
            "argv": ["ryeos-directive-runtime"],
            "environment_names": ["OPENROUTER_API_KEY"],
        })));
        capsule.validate().unwrap();

        let error = managed_capsule(None).validate().unwrap_err();
        assert!(error.to_string().contains("no prepared launch object"));
    }

    #[test]
    fn direct_capsule_rejects_managed_prepared_launch_state() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::VerifiedContent {
            content_hash: "f".repeat(64),
        });
        capsule.prepared_launch = Some(serde_json::json!({"argv": ["unexpected"]}));
        assert!(capsule
            .validate()
            .unwrap_err()
            .to_string()
            .contains("cannot carry managed prepared launch state"));
    }
}
