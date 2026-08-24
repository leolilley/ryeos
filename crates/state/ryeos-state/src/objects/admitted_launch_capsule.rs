use anyhow::Context as _;
use serde::{Deserialize, Serialize};

use super::{
    ExecutionLaunchDriver, ExecutionLifecycleAuthority, ExecutionProjectAuthority,
    ExecutionRecoveryAuthority, validate_trimmed_control_free,
};

pub const ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION: u32 = 14;
pub const ADMITTED_DIRECT_COMMAND_ROOT: &str = "/ryeos/admitted-direct-command";

pub fn admitted_direct_command_execution_path(
    content_hash: &str,
    source_path: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    super::thread_snapshot::validate_canonical_hash(
        "admitted direct executable blob hash",
        content_hash,
    )?;
    let file_name = source_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("admitted direct command path has no file name"))?;
    if std::path::Path::new(file_name).components().count() != 1 {
        anyhow::bail!("admitted direct command has an unsafe file name");
    }
    Ok(std::path::PathBuf::from(ADMITTED_DIRECT_COMMAND_ROOT)
        .join(content_hash)
        .join(file_name))
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Exact, secret-free execution material retained at first admission.
///
/// Recovery may apply stricter current trust and isolation policy, but it
/// must never rebuild these values from mutable runtime, protocol, executor,
/// or kind registries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "driver", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdmittedExecutionClosure {
    ManagedRuntime {
        prepared_runtime_launch: serde_json::Value,
        runtime_descriptor_document: String,
        protocol_descriptor_document: String,
        executor_blob_hash: String,
    },
    DirectItemExecutor {
        execution_plan: serde_json::Value,
        protocol_descriptor_document: String,
        command: AdmittedDirectCommandClosure,
        #[serde(deserialize_with = "deserialize_required_nullable")]
        admitted_project_root: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdmittedDirectCommandClosure {
    ContentAddressed {
        executable_blob_hash: String,
        /// Exact absolute path at which isolation presents the retained bytes
        /// to the process image. This is behavior-bearing for interpreters and
        /// binaries whose loader resolves adjacent libraries from `$ORIGIN`.
        execution_path: std::path::PathBuf,
    },
    NodePolicy,
}

impl AdmittedExecutionClosure {
    pub fn launch_driver(&self) -> ExecutionLaunchDriver {
        match self {
            Self::ManagedRuntime { .. } => ExecutionLaunchDriver::ManagedRuntime,
            Self::DirectItemExecutor { .. } => ExecutionLaunchDriver::DirectItemExecutor,
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let require_object = |label: &str, value: &serde_json::Value| {
            if value.is_object() {
                Ok(())
            } else {
                anyhow::bail!("{label} must be an object")
            }
        };
        match self {
            Self::ManagedRuntime {
                prepared_runtime_launch,
                runtime_descriptor_document,
                protocol_descriptor_document,
                executor_blob_hash,
            } => {
                require_object("prepared runtime launch", prepared_runtime_launch)?;
                validate_descriptor_document(
                    "admitted runtime descriptor",
                    runtime_descriptor_document,
                )?;
                validate_descriptor_document(
                    "admitted protocol descriptor",
                    protocol_descriptor_document,
                )?;
                super::thread_snapshot::validate_canonical_hash(
                    "admitted managed executor blob hash",
                    executor_blob_hash,
                )?;
            }
            Self::DirectItemExecutor {
                execution_plan,
                protocol_descriptor_document,
                command,
                admitted_project_root,
            } => {
                require_object("admitted direct execution plan", execution_plan)?;
                validate_descriptor_document(
                    "admitted protocol descriptor",
                    protocol_descriptor_document,
                )?;
                if let AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash,
                    execution_path,
                } = command
                {
                    super::thread_snapshot::validate_canonical_hash(
                        "admitted direct executable blob hash",
                        executable_blob_hash,
                    )?;
                    validate_absolute_normalized_path(
                        "admitted direct execution path",
                        execution_path,
                    )?;
                    let expected = admitted_direct_command_execution_path(
                        executable_blob_hash,
                        execution_path,
                    )?;
                    if &expected != execution_path {
                        anyhow::bail!(
                            "admitted direct execution path does not match its content-addressed namespace"
                        );
                    }
                }
                if let Some(root) = admitted_project_root {
                    if root.components().count() < 2
                        || !root.is_absolute()
                        || root.components().enumerate().any(|(index, component)| {
                            !matches!(
                                (index, component),
                                (0, std::path::Component::RootDir)
                                    | (_, std::path::Component::Normal(_))
                            )
                        })
                    {
                        anyhow::bail!(
                            "admitted direct project root must be absolute and normalized"
                        );
                    }
                    if root.to_str().is_none() {
                        anyhow::bail!("admitted direct project root must be valid UTF-8");
                    }
                }
            }
        }
        Ok(())
    }
}

pub const MAX_ADMITTED_DESCRIPTOR_BYTES: u64 = 256 * 1024;

fn validate_descriptor_document(label: &str, document: &str) -> anyhow::Result<()> {
    if document.is_empty()
        || u64::try_from(document.len())
            .ok()
            .is_none_or(|bytes| bytes > MAX_ADMITTED_DESCRIPTOR_BYTES)
    {
        anyhow::bail!("{label} document must contain 1..={MAX_ADMITTED_DESCRIPTOR_BYTES} bytes");
    }
    if document.contains('\0') {
        anyhow::bail!("{label} document contains NUL");
    }
    Ok(())
}

fn validate_absolute_normalized_path(label: &str, path: &std::path::Path) -> anyhow::Result<()> {
    if path.components().count() < 2
        || !path.is_absolute()
        || path.components().enumerate().any(|(index, component)| {
            !matches!(
                (index, component),
                (0, std::path::Component::RootDir) | (_, std::path::Component::Normal(_))
            )
        })
        || path.to_str().is_none()
    {
        anyhow::bail!("{label} must be an absolute normalized UTF-8 path");
    }
    Ok(())
}

fn descriptor_document_identity(label: &str, document: &str) -> anyhow::Result<(String, String)> {
    validate_descriptor_document(label, document)?;
    let header =
        lillux::signature::parse_signature_line(document.lines().next().unwrap_or(""), "#", None)
            .ok_or_else(|| anyhow::anyhow!("{label} has no valid signature header"))?;
    let body = lillux::signature::strip_signature_lines(document);
    let observed = lillux::signature::content_hash(&body);
    if observed != header.content_hash {
        anyhow::bail!("{label} body contradicts its signature content hash");
    }
    Ok((observed, header.signer_fingerprint))
}

/// Immutable daemon-minted accounting scope sealed with an admitted launch.
/// A paid descendant reserves against exactly these identities; recovery
/// rejects a ledger that cannot satisfy them and never remints allowance
/// from configured limits alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedAccountingScope {
    pub budget_authority_site_id: String,
    pub ledger_epoch: u64,
    pub execution_budget_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directive_budget_id: Option<String>,
}

impl AdmittedAccountingScope {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_trimmed_control_free(
            "accounting scope site",
            &self.budget_authority_site_id,
            false,
        )?;
        validate_trimmed_control_free(
            "accounting scope execution budget id",
            &self.execution_budget_id,
            false,
        )?;
        if let Some(directive_budget_id) = &self.directive_budget_id {
            validate_trimmed_control_free(
                "accounting scope directive budget id",
                directive_budget_id,
                false,
            )?;
        }
        if self.ledger_epoch == 0 {
            anyhow::bail!("accounting scope ledger epoch must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "authority", rename_all = "snake_case", deny_unknown_fields)]
pub enum DirectExecutableIdentity {
    BundleExecutor {
        content_hash: String,
        executor_manifest_hash: String,
        executor_manifest_signer_fingerprint: String,
    },
    CapturedContent {
        content_hash: String,
    },
    /// The exact command spelling remains sealed in `execution_plan_hash`, but
    /// executable authorization comes from the node's signed isolation policy
    /// rather than a bundle/CAS content identity. This driver is not eligible
    /// for autonomous restart recovery.
    NodePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "space", rename_all = "snake_case", deny_unknown_fields)]
pub enum DirectRootSourceIdentity {
    Project,
    Bundle {
        manifest_hash: String,
        manifest_signer_fingerprint: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectRuntimeSourceSpace {
    Project,
    Bundle,
}

/// Exact runtime descriptor identity selected by the executor-chain build for
/// a direct item launch.
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
        root_subject_source_content_digest: String,
        root_subject_signer_fingerprint: Option<String>,
        root_subject_source_identity: DirectRootSourceIdentity,
        protocol_ref: String,
        protocol_content_hash: String,
        protocol_signer_fingerprint: String,
        execution_plan_hash: String,
        executable_identity: DirectExecutableIdentity,
        runtime_identity: DirectRuntimeIdentity,
    },
}

impl AdmittedLaunchArtifactIdentity {
    pub fn validate(&self) -> anyhow::Result<()> {
        let validate_hash = |label: &str, value: &str| {
            super::thread_snapshot::validate_canonical_hash(label, value)
        };
        let validate_signer = |label: &str, value: &str| validate_hash(label, value);
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
                    ("protocol ref", protocol_ref),
                    ("executor ref", executor_ref),
                ] {
                    validate_trimmed_control_free(label, value, false)?;
                }
                for (label, value) in [
                    ("runtime signer", runtime_signer_fingerprint),
                    ("protocol signer", protocol_signer_fingerprint),
                    ("executor bundle signer", executor_bundle_signer_fingerprint),
                ] {
                    validate_signer(label, value)?;
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
                root_subject_source_content_digest,
                root_subject_signer_fingerprint,
                root_subject_source_identity,
                protocol_ref,
                protocol_content_hash,
                protocol_signer_fingerprint,
                execution_plan_hash,
                executable_identity,
                runtime_identity,
            } => {
                validate_trimmed_control_free("executor ref", executor_ref, false)?;
                validate_hash(
                    "root subject source content digest",
                    root_subject_source_content_digest,
                )?;
                if let Some(signer) = root_subject_signer_fingerprint {
                    validate_signer("root subject signer", signer)?;
                }
                match root_subject_source_identity {
                    DirectRootSourceIdentity::Project => {}
                    DirectRootSourceIdentity::Bundle {
                        manifest_hash,
                        manifest_signer_fingerprint,
                    } => {
                        if root_subject_signer_fingerprint.is_none() {
                            anyhow::bail!("bundle direct root has no verified subject signer");
                        }
                        validate_hash("root source manifest hash", manifest_hash)?;
                        validate_signer(
                            "root source manifest signer",
                            manifest_signer_fingerprint,
                        )?;
                    }
                }
                validate_trimmed_control_free("protocol ref", protocol_ref, false)?;
                validate_hash("protocol content hash", protocol_content_hash)?;
                validate_signer("protocol signer", protocol_signer_fingerprint)?;
                validate_hash("execution plan hash", execution_plan_hash)?;
                match executable_identity {
                    DirectExecutableIdentity::BundleExecutor {
                        content_hash,
                        executor_manifest_hash,
                        executor_manifest_signer_fingerprint,
                    } => {
                        validate_hash("verified executable content hash", content_hash)?;
                        validate_hash("executor bundle manifest hash", executor_manifest_hash)?;
                        validate_signer(
                            "executor bundle manifest signer",
                            executor_manifest_signer_fingerprint,
                        )?;
                    }
                    DirectExecutableIdentity::CapturedContent { content_hash } => {
                        validate_hash("captured executable content hash", content_hash)?;
                    }
                    DirectExecutableIdentity::NodePolicy => {}
                }
                {
                    let runtime = runtime_identity;
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
                    validate_signer("direct runtime signer", &runtime.runtime_signer_fingerprint)?;
                    match (
                        &runtime.runtime_bundle_manifest_hash,
                        &runtime.runtime_bundle_signer_fingerprint,
                    ) {
                        (Some(hash), Some(signer)) => {
                            validate_hash("direct runtime bundle manifest hash", hash)?;
                            validate_signer("direct runtime bundle signer", signer)?;
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
/// execution's first-launch boundary. Recovery consumes the exact program and
/// execution closure; it never asks mutable project or bundle space to
/// recreate an earlier admission.
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
    /// Complete exact execution closure selected at admission.
    pub execution_closure: AdmittedExecutionClosure,
    /// Exact pre-admission execution realization. The realization commits the
    /// canonical launch-authority digest; it never back-references this final
    /// capsule hash, avoiding an impossible content-addressed cycle.
    pub execution_realization_hash: String,
    /// Exact adjacent-source authority retained by this launch, when the
    /// effective program declares one. Required-nullable on the current wire.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub source_binding_hash: Option<String>,
    /// Sealed accounting scope for launches whose runtime declares a
    /// financial authority. `None` states the launch performs no direct paid
    /// provider work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounting_scope: Option<AdmittedAccountingScope>,
    pub effective_caps: Vec<String>,
    pub runtime_ref: String,
    pub executor_ref: String,
}

/// Canonical pre-capsule authority shared by the realization and capsule.
/// Invocation-local stimulus is intentionally excluded, matching the exact
/// program boundary used for continuation admission.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedLaunchAuthority {
    pub exact_program_hash: String,
    pub project_authority: ExecutionProjectAuthority,
    pub lifecycle_authority: ExecutionLifecycleAuthority,
    pub launch_driver: ExecutionLaunchDriver,
    pub artifact_identity: AdmittedLaunchArtifactIdentity,
    pub execution_closure: AdmittedExecutionClosure,
    pub accounting_scope: Option<AdmittedAccountingScope>,
    pub effective_caps: Vec<String>,
    pub runtime_ref: String,
    pub executor_ref: String,
}

impl AdmittedLaunchAuthority {
    pub fn validate(&self) -> anyhow::Result<()> {
        super::thread_snapshot::validate_canonical_hash(
            "launch authority exact program hash",
            &self.exact_program_hash,
        )?;
        self.project_authority.validate()?;
        self.lifecycle_authority.validate()?;
        self.artifact_identity.validate()?;
        self.execution_closure.validate()?;
        if self.artifact_identity.launch_driver() != self.launch_driver
            || self.execution_closure.launch_driver() != self.launch_driver
        {
            anyhow::bail!("launch authority driver and admitted material disagree");
        }
        if let Some(scope) = &self.accounting_scope {
            scope.validate()?;
        }
        validate_trimmed_control_free("launch authority runtime ref", &self.runtime_ref, false)?;
        validate_trimmed_control_free("launch authority executor ref", &self.executor_ref, false)?;
        let mut caps = self.effective_caps.clone();
        for cap in &caps {
            validate_trimmed_control_free("launch authority capability", cap, false)?;
        }
        caps.sort();
        caps.dedup();
        if caps != self.effective_caps {
            anyhow::bail!("launch authority capabilities are not canonical");
        }
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

impl AdmittedLaunchCapsule {
    pub fn source_binding_hash_in_program(
        exact_program: &serde_json::Value,
    ) -> anyhow::Result<Option<String>> {
        let Some(value) = exact_program
            .get("resolution_output")
            .and_then(|resolution| resolution.get("composed"))
            .and_then(|composed| composed.get("derived"))
            .and_then(|derived| derived.get(super::SOURCE_CLOSURE_DERIVED_KEY))
        else {
            return Ok(None);
        };
        Ok(Some(
            super::EffectiveSourceClosureProjection::from_value(value)?.binding_hash,
        ))
    }

    /// Verify the complete realization/substrate evidence named by this
    /// capsule against retained bytes and the current trust store.
    pub fn verify_retained_execution_realization(
        &self,
        cas: &lillux::CasStore,
        large_store: &crate::large_object_store::LargeObjectStore,
        trust: &crate::refs::TrustStore,
    ) -> anyhow::Result<super::AdmittedExecutionRealization> {
        self.validate()?;
        let value = cas
            .get_object(&self.execution_realization_hash)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "admitted execution realization {} is missing",
                    self.execution_realization_hash
                )
            })?;
        let realization = super::AdmittedExecutionRealization::from_current_value(&value)?;
        if realization.content_hash()? != self.execution_realization_hash {
            anyhow::bail!("admitted execution realization content hash is inconsistent");
        }
        let authority = self.launch_authority();
        let capsule_launch_authority_digest = authority.digest()?;
        if realization.launch_authority_digest != capsule_launch_authority_digest {
            anyhow::bail!(
                "admitted execution realization contradicts the complete launch authority"
            );
        }
        if realization.artifact_identity_digest != authority.artifact_identity_digest()? {
            anyhow::bail!(
                "admitted execution realization contradicts the launch artifact identity"
            );
        }
        if realization.execution_closure_digest != authority.execution_closure_digest()? {
            anyhow::bail!(
                "admitted execution realization contradicts the launch execution closure"
            );
        }
        let effective_definition_digest = self
            .exact_program
            .get("effective_definition_digest")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("admitted launch exact program has no effective-definition digest")
            })?;
        if realization.effective_definition_digest != effective_definition_digest {
            anyhow::bail!("execution realization contradicts effective definition identity");
        }
        let (contract_ref, contract_digest) = match &self.artifact_identity {
            AdmittedLaunchArtifactIdentity::ManagedRuntime {
                runtime_ref,
                runtime_content_hash,
                ..
            } => (runtime_ref, runtime_content_hash),
            AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                runtime_identity, ..
            } => (
                &runtime_identity.runtime_ref,
                &runtime_identity.runtime_content_hash,
            ),
        };
        if &realization.contract_ref != contract_ref
            || &realization.contract_digest != contract_digest
        {
            anyhow::bail!("execution realization contradicts its owning execution contract");
        }
        realization.verify_retained_components(cas, large_store)?;

        let identity_value = cas
            .get_object(&realization.substrate_identity_hash)?
            .ok_or_else(|| anyhow::anyhow!("execution substrate identity is missing"))?;
        let identity = super::ExecutionIdentity::from_current_value(&identity_value)?;
        let attestation_value = cas
            .get_object(&realization.substrate_attestation_hash)?
            .ok_or_else(|| anyhow::anyhow!("execution substrate attestation is missing"))?;
        let attestation = super::Attestation::from_value(&attestation_value)?;
        if attestation.subject_hash != realization.substrate_identity_hash
            || attestation.claim != super::EXECUTION_IDENTITY_ATTESTATION_CLAIM
            || attestation.policy != super::EXECUTION_IDENTITY_ATTESTATION_POLICY
            || attestation.issuer_fingerprint()? != identity.node_signer_fingerprint
        {
            anyhow::bail!("execution substrate attestation contradicts retained identity");
        }
        attestation.verify_with_trust_store(trust)?;
        if attestation.is_expired_at(&lillux::time::iso8601_now())? {
            anyhow::bail!("execution substrate attestation is expired");
        }
        Ok(realization)
    }

    pub fn launch_authority(&self) -> AdmittedLaunchAuthority {
        AdmittedLaunchAuthority {
            exact_program_hash: self.exact_program_hash.clone(),
            project_authority: self.project_authority.clone(),
            lifecycle_authority: self.lifecycle_authority,
            launch_driver: self.launch_driver,
            artifact_identity: self.artifact_identity.clone(),
            execution_closure: self.execution_closure.clone(),
            accounting_scope: self.accounting_scope.clone(),
            effective_caps: self.effective_caps.clone(),
            runtime_ref: self.runtime_ref.clone(),
            executor_ref: self.executor_ref.clone(),
        }
    }

    pub fn launch_authority_digest(&self) -> anyhow::Result<String> {
        self.launch_authority().digest()
    }

    /// External realization set sealed into this capsule's exact program,
    /// parsed with the shared wire type. `Ok(None)` when the program sealed
    /// no realization slot; malformed derived data is an error, never an
    /// empty inheritance.
    pub fn external_realization_set(
        &self,
    ) -> anyhow::Result<Option<super::ExternalContentRealizationSet>> {
        let Some(value) = self
            .exact_program
            .get("resolution_output")
            .and_then(|resolution| resolution.get("composed"))
            .and_then(|composed| composed.get("derived"))
            .and_then(|derived| derived.get(super::EXTERNAL_REALIZATIONS_DERIVED_KEY))
        else {
            return Ok(None);
        };
        super::ExternalContentRealizationSet::from_value(value)
            .map(Some)
            .context("admitted capsule carries an invalid external realization set")
    }

    /// Decode only the exact current CAS wire contract.
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
        if schema != u64::from(ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION) {
            return Err(super::IncompatibleCurrentObjectSchema::new(
                "admitted launch capsule",
                schema,
                ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION,
            )
            .into());
        }
        let capsule: Self =
            serde_json::from_value(value).context("deserialize current admitted launch capsule")?;
        capsule.validate()?;
        Ok(capsule)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema != ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION
            || self.kind != "admitted_launch_capsule"
        {
            anyhow::bail!("invalid admitted launch capsule wire identity");
        }
        if self.launch_driver == ExecutionLaunchDriver::InProcessHandler {
            anyhow::bail!(
                "in-process handler launch drivers cannot carry admitted subprocess capsules"
            );
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
        let invocation_project_authority: ExecutionProjectAuthority = serde_json::from_value(
            invocation_object
                .get("project_authority")
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "admitted launch capsule sealed invocation is missing project_authority"
                    )
                })?,
        )
        .context("decode sealed invocation project authority")?;
        invocation_project_authority.validate()?;
        if invocation_project_authority != self.project_authority {
            anyhow::bail!(
                "admitted launch capsule sealed invocation project authority differs from its outer authority"
            );
        }
        for invocation_field in [
            "parameters",
            "requested_by",
            "planning_principal",
            "project_context",
            "project_authority",
            "project_binding_subject_authority",
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
        self.execution_closure.validate()?;
        super::thread_snapshot::validate_canonical_hash(
            "launch capsule execution realization hash",
            &self.execution_realization_hash,
        )?;
        if self.source_binding_hash != Self::source_binding_hash_in_program(&self.exact_program)? {
            anyhow::bail!("launch capsule source binding contradicts its exact program");
        }
        if let Some(hash) = &self.source_binding_hash {
            super::thread_snapshot::validate_canonical_hash("launch capsule source binding", hash)?;
        }
        match (&self.artifact_identity, &self.execution_closure) {
            (
                AdmittedLaunchArtifactIdentity::ManagedRuntime {
                    runtime_content_hash,
                    runtime_signer_fingerprint,
                    protocol_content_hash,
                    protocol_signer_fingerprint,
                    ..
                },
                AdmittedExecutionClosure::ManagedRuntime {
                    runtime_descriptor_document,
                    protocol_descriptor_document,
                    ..
                },
            ) => {
                let runtime = descriptor_document_identity(
                    "admitted runtime descriptor",
                    runtime_descriptor_document,
                )?;
                let protocol = descriptor_document_identity(
                    "admitted protocol descriptor",
                    protocol_descriptor_document,
                )?;
                if runtime
                    != (
                        runtime_content_hash.clone(),
                        runtime_signer_fingerprint.clone(),
                    )
                    || protocol
                        != (
                            protocol_content_hash.clone(),
                            protocol_signer_fingerprint.clone(),
                        )
                {
                    anyhow::bail!(
                        "admitted managed descriptor documents contradict artifact identity"
                    );
                }
            }
            (
                AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                    protocol_content_hash,
                    protocol_signer_fingerprint,
                    execution_plan_hash,
                    ..
                },
                AdmittedExecutionClosure::DirectItemExecutor {
                    execution_plan,
                    protocol_descriptor_document,
                    ..
                },
            ) => {
                let protocol = descriptor_document_identity(
                    "admitted protocol descriptor",
                    protocol_descriptor_document,
                )?;
                if protocol
                    != (
                        protocol_content_hash.clone(),
                        protocol_signer_fingerprint.clone(),
                    )
                {
                    anyhow::bail!(
                        "admitted direct protocol document contradicts artifact identity"
                    );
                }
                let observed_plan_hash =
                    lillux::sha256_hex(lillux::canonical_json(execution_plan)?.as_bytes());
                if &observed_plan_hash != execution_plan_hash {
                    anyhow::bail!("admitted direct execution plan contradicts artifact identity");
                }
            }
            _ => anyhow::bail!("admitted execution closure and artifact drivers disagree"),
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
        if let (
            AdmittedLaunchArtifactIdentity::ManagedRuntime {
                executor_content_hash,
                ..
            },
            AdmittedExecutionClosure::ManagedRuntime {
                executor_blob_hash, ..
            },
        ) = (&self.artifact_identity, &self.execution_closure)
            && executor_content_hash != executor_blob_hash
        {
            anyhow::bail!("admitted managed executor blob hash contradicts executable identity");
        }
        if let (
            AdmittedLaunchArtifactIdentity::DirectItemExecutor {
                executable_identity,
                ..
            },
            AdmittedExecutionClosure::DirectItemExecutor { command, .. },
        ) = (&self.artifact_identity, &self.execution_closure)
        {
            let consistent = matches!(
                (executable_identity, command),
                (
                    DirectExecutableIdentity::NodePolicy,
                    AdmittedDirectCommandClosure::NodePolicy
                ) | (
                    DirectExecutableIdentity::BundleExecutor { .. }
                        | DirectExecutableIdentity::CapturedContent { .. },
                    AdmittedDirectCommandClosure::ContentAddressed { .. }
                )
            );
            if !consistent {
                anyhow::bail!("admitted direct command closure contradicts executable identity");
            }
            if let (
                DirectExecutableIdentity::BundleExecutor { content_hash, .. }
                | DirectExecutableIdentity::CapturedContent { content_hash },
                AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash,
                    execution_path: _,
                },
            ) = (executable_identity, command)
                && content_hash != executable_blob_hash
            {
                anyhow::bail!(
                    "admitted direct executable blob hash contradicts executable identity"
                );
            }
        }
        if self.artifact_identity.launch_driver() != self.launch_driver {
            anyhow::bail!("admitted launch artifact identity contradicts launch driver");
        }
        if self.execution_closure.launch_driver() != self.launch_driver {
            anyhow::bail!("admitted execution closure contradicts launch driver");
        }
        if let Some(scope) = &self.accounting_scope {
            scope.validate()?;
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
    /// segments. `exact_program` is already the canonical projection of the
    /// sealed request with segment-local invocation stimulus, principal, usage,
    /// and project realization removed. Each segment validates its complete
    /// sealed invocation against its own resume ledger before this comparison;
    /// comparing those per-segment envelopes to one another would reject
    /// legitimate continuation inputs.
    pub fn same_continuation_admission(&self, other: &Self) -> anyhow::Result<bool> {
        Ok(self.same_continuation_program_admission(other)?
            && self.execution_realization_hash == other.execution_realization_hash)
    }

    /// Compare the immutable program and execution-policy authority shared by
    /// continuation segments while permitting the realization object itself
    /// to be rebound to an explicitly advanced pinned project generation.
    /// Callers must separately prove that the two realization objects differ
    /// only in their launch-authority digest.
    pub fn same_continuation_program_admission(&self, other: &Self) -> anyhow::Result<bool> {
        self.validate()?;
        other.validate()?;
        Ok(self.schema == other.schema
            && self.kind == other.kind
            && self.exact_program == other.exact_program
            && self.exact_program_hash == other.exact_program_hash
            && self.lifecycle_authority == other.lifecycle_authority
            && self.launch_driver == other.launch_driver
            && self.artifact_identity == other.artifact_identity
            && self.execution_closure == other.execution_closure
            && self.accounting_scope == other.accounting_scope
            && self.effective_caps == other.effective_caps
            && self.runtime_ref == other.runtime_ref
            && self.executor_ref == other.executor_ref
            && self
                .project_authority
                .same_continuation_lineage(&other.project_authority)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{
        ExecutionOwnershipAuthority, ExecutionProjectAuthority, ExecutionRecoveryAuthority,
    };

    fn signed_descriptor(body: &str, seed: u8) -> (String, String, String) {
        let key = lillux::crypto::SigningKey::from_bytes(&[seed; 32]);
        let document = lillux::signature::sign_content(body, &key, "#", None);
        let header =
            lillux::signature::parse_signature_line(document.lines().next().unwrap(), "#", None)
                .unwrap();
        (document, header.content_hash, header.signer_fingerprint)
    }

    fn direct_capsule(executable_identity: DirectExecutableIdentity) -> AdmittedLaunchCapsule {
        let command = match &executable_identity {
            DirectExecutableIdentity::BundleExecutor { content_hash, .. }
            | DirectExecutableIdentity::CapturedContent { content_hash } => {
                AdmittedDirectCommandClosure::ContentAddressed {
                    executable_blob_hash: content_hash.clone(),
                    execution_path: std::path::PathBuf::from("/admitted/bin/executor"),
                }
            }
            DirectExecutableIdentity::NodePolicy => AdmittedDirectCommandClosure::NodePolicy,
        };
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
        object.insert(
            "project_authority".to_string(),
            serde_json::to_value(ExecutionProjectAuthority::PROJECTLESS).unwrap(),
        );
        object.insert(
            "project_binding_subject_authority".to_string(),
            serde_json::json!({"kind": "projectless"}),
        );
        let (protocol_descriptor_document, protocol_content_hash, protocol_signer_fingerprint) =
            signed_descriptor("protocol: direct\n", 31);
        let execution_plan = serde_json::json!({"plan_id": "test"});
        let execution_plan_hash =
            lillux::sha256_hex(lillux::canonical_json(&execution_plan).unwrap().as_bytes());
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
                root_subject_source_content_digest: "a".repeat(64),
                root_subject_signer_fingerprint: Some("2".repeat(64)),
                root_subject_source_identity: DirectRootSourceIdentity::Bundle {
                    manifest_hash: "b".repeat(64),
                    manifest_signer_fingerprint: "3".repeat(64),
                },
                protocol_ref: "protocol:test/direct".to_string(),
                protocol_content_hash,
                protocol_signer_fingerprint,
                execution_plan_hash,
                executable_identity,
                runtime_identity: DirectRuntimeIdentity {
                    runtime_ref: "tool:test/runtime".to_string(),
                    runtime_source_space: DirectRuntimeSourceSpace::Bundle,
                    runtime_content_hash: "f".repeat(64),
                    runtime_signer_fingerprint: "4".repeat(64),
                    runtime_bundle_manifest_hash: Some("1".repeat(64)),
                    runtime_bundle_signer_fingerprint: Some("3".repeat(64)),
                },
            },
            execution_closure: AdmittedExecutionClosure::DirectItemExecutor {
                execution_plan,
                protocol_descriptor_document,
                command,
                admitted_project_root: None,
            },
            execution_realization_hash: "9".repeat(64),
            source_binding_hash: None,
            accounting_scope: None,
            effective_caps: vec!["ryeos.read.project.live".to_string()],
            runtime_ref: "runtime:direct".to_string(),
            executor_ref: "tool:test/executor".to_string(),
        }
    }

    fn managed_capsule(prepared_runtime_launch: serde_json::Value) -> AdmittedLaunchCapsule {
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
        object.insert(
            "project_authority".to_string(),
            serde_json::to_value(ExecutionProjectAuthority::PROJECTLESS).unwrap(),
        );
        object.insert(
            "project_binding_subject_authority".to_string(),
            serde_json::json!({"kind": "projectless"}),
        );
        let (runtime_descriptor_document, runtime_content_hash, runtime_signer_fingerprint) =
            signed_descriptor("runtime: managed\n", 32);
        let (protocol_descriptor_document, protocol_content_hash, protocol_signer_fingerprint) =
            signed_descriptor("protocol: managed\n", 33);
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
                runtime_content_hash,
                runtime_signer_fingerprint,
                protocol_ref: "protocol:test/directive".to_string(),
                protocol_content_hash,
                protocol_signer_fingerprint,
                executor_ref: "executor:test/subprocess".to_string(),
                executor_content_hash: "c".repeat(64),
                executor_bundle_manifest_hash: "d".repeat(64),
                executor_bundle_signer_fingerprint: "e".repeat(64),
            },
            execution_closure: AdmittedExecutionClosure::ManagedRuntime {
                prepared_runtime_launch,
                runtime_descriptor_document,
                protocol_descriptor_document,
                executor_blob_hash: "c".repeat(64),
            },
            execution_realization_hash: "9".repeat(64),
            source_binding_hash: None,
            accounting_scope: None,
            effective_caps: vec!["ryeos.read.project.live".to_string()],
            runtime_ref: "runtime:test/directive".to_string(),
            executor_ref: "executor:test/subprocess".to_string(),
        }
    }

    #[test]
    fn restart_recovery_accepts_a_content_verified_direct_executable() {
        let capsule = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        });
        capsule.validate().unwrap();
        assert_eq!(capsule.content_hash().unwrap().len(), 64);
    }

    #[test]
    fn in_process_handler_driver_is_not_an_admitted_subprocess_capsule() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        });
        capsule.launch_driver = ExecutionLaunchDriver::InProcessHandler;
        let error = capsule.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot carry admitted subprocess capsules")
        );
    }

    #[test]
    fn current_decoder_rejects_predecessor_epoch_before_nested_authority_decode() {
        let mut value = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        })
        .to_value();
        let object = value.as_object_mut().unwrap();
        object.insert(
            "schema".to_string(),
            serde_json::json!(ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION - 1),
        );
        object.insert(
            "project_authority".to_string(),
            serde_json::json!({"authority": "predecessor_shape"}),
        );

        let error = AdmittedLaunchCapsule::from_current_value(value).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::objects::IncompatibleCurrentObjectSchema>()
                .is_some()
        );
        assert!(
            error.to_string().contains("not the exact current contract"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn bundle_runtime_requires_its_source_bundle_generation_identity() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "9".repeat(64),
        });
        let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            runtime_identity: runtime,
            ..
        } = &mut capsule.artifact_identity
        else {
            panic!("direct fixture must carry runtime identity");
        };
        runtime.runtime_bundle_manifest_hash = None;
        runtime.runtime_bundle_signer_fingerprint = None;
        let error = capsule.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no complete source-bundle generation identity")
        );
    }

    #[test]
    fn current_decoder_accepts_valid_current_capsule() {
        let expected = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        });
        let decoded = AdmittedLaunchCapsule::from_current_value(expected.to_value()).unwrap();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn continuation_program_comparison_separates_realization_rebinding() {
        let mut source = direct_capsule(DirectExecutableIdentity::NodePolicy);
        source.lifecycle_authority = ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE;
        let mut successor = source.clone();
        successor.execution_realization_hash = "8".repeat(64);

        assert!(
            source
                .same_continuation_program_admission(&successor)
                .unwrap()
        );
        assert!(!source.same_continuation_admission(&successor).unwrap());
    }

    #[test]
    fn capsule_rejects_divergent_inner_and_outer_project_authority() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        });
        capsule.project_authority =
            ExecutionProjectAuthority::projectless(crate::objects::EnvironmentAuthority::Vault {
                namespace: "test".to_string(),
                name_authority: crate::objects::EnvironmentNameAuthority::DeclaredRequired,
            })
            .unwrap();
        let error = capsule.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("sealed invocation project authority differs")
        );
    }

    #[test]
    fn current_decoder_requires_explicit_execution_closure_field() {
        let mut value = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        })
        .to_value();
        value
            .as_object_mut()
            .expect("capsule object")
            .remove("execution_closure");
        let error = AdmittedLaunchCapsule::from_current_value(value).unwrap_err();
        assert!(
            format!("{error:#}").contains("missing field `execution_closure`"),
            "unexpected error chain: {error:#}"
        );
    }

    #[test]
    fn current_capsule_requires_typed_sealed_invocation_project_authority() {
        let mut missing = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        });
        missing
            .sealed_invocation
            .as_object_mut()
            .unwrap()
            .remove("project_authority");
        let error = missing.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("sealed invocation is missing project_authority")
        );

        let mut malformed = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        });
        malformed.sealed_invocation.as_object_mut().unwrap().insert(
            "project_authority".to_string(),
            serde_json::json!({"kind": "predecessor_shape"}),
        );
        let error = malformed.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("decode sealed invocation project authority")
        );
    }

    #[test]
    fn current_decoder_requires_numeric_epoch_before_typed_decode() {
        let mut value = direct_capsule(DirectExecutableIdentity::CapturedContent {
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
    fn current_direct_closure_requires_explicit_project_root_authority() {
        let mut value = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        })
        .to_value();
        value["execution_closure"]
            .as_object_mut()
            .unwrap()
            .remove("admitted_project_root");

        let error = AdmittedLaunchCapsule::from_current_value(value).unwrap_err();
        assert!(
            format!("{error:#}").contains("admitted_project_root"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn restart_recovery_rejects_a_node_policy_direct_executable() {
        let capsule = direct_capsule(DirectExecutableIdentity::NodePolicy);
        let error = capsule.validate().unwrap_err();
        assert!(
            error
                .to_string()
                .contains("not eligible for autonomous restart recovery")
        );
    }

    #[test]
    fn request_scoped_execution_accepts_a_node_policy_direct_executable() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::NodePolicy);
        capsule.lifecycle_authority = ExecutionLifecycleAuthority::REQUEST_SCOPED;
        capsule.validate().unwrap();
    }

    #[test]
    fn source_only_root_seals_distinct_source_and_executor_manifests() {
        let root_manifest_hash = "a".repeat(64);
        let executor_manifest_hash = "b".repeat(64);
        let mut capsule = direct_capsule(DirectExecutableIdentity::BundleExecutor {
            content_hash: "e".repeat(64),
            executor_manifest_hash: executor_manifest_hash.clone(),
            executor_manifest_signer_fingerprint: "5".repeat(64),
        });
        let AdmittedLaunchArtifactIdentity::DirectItemExecutor {
            root_subject_source_identity,
            ..
        } = &mut capsule.artifact_identity
        else {
            unreachable!()
        };
        *root_subject_source_identity = DirectRootSourceIdentity::Bundle {
            manifest_hash: root_manifest_hash.clone(),
            manifest_signer_fingerprint: "6".repeat(64),
        };

        capsule.validate().unwrap();
        assert_ne!(root_manifest_hash, executor_manifest_hash);
    }

    #[test]
    fn exact_program_hash_is_verified_not_trusted() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        });
        capsule.exact_program_hash = "0".repeat(64);
        assert!(
            capsule
                .validate()
                .unwrap_err()
                .to_string()
                .contains("mismatch")
        );
    }

    #[test]
    fn managed_recovery_requires_an_object_prepared_launch_state() {
        let capsule = managed_capsule(serde_json::json!({
            "argv": ["ryeos-directive-runtime"],
            "environment_names": ["OPENROUTER_API_KEY"],
        }));
        capsule.validate().unwrap();

        let error = managed_capsule(serde_json::Value::Null)
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains("must be an object"));
    }

    #[test]
    fn direct_capsule_rejects_a_managed_execution_closure() {
        let mut capsule = direct_capsule(DirectExecutableIdentity::CapturedContent {
            content_hash: "f".repeat(64),
        });
        capsule.execution_closure =
            managed_capsule(serde_json::json!({"argv": ["unexpected"]})).execution_closure;
        let error = capsule.validate().unwrap_err();
        assert_eq!(
            error.to_string(),
            "admitted execution closure and artifact drivers disagree"
        );
    }
}
