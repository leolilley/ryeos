//! Durable operation and completion publication for managed external-content
//! acquisition. Portable recipe compilation remains in
//! `managed_external_content`; this module owns node-local operation state.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::managed_external_content::ResolvedManagedExternalContentActivation;
use crate::node_policy::sections::external_content::{
    ExternalContentImportPolicyRecord, ManagedExternalContentActivationPolicy,
};

pub const MANAGED_ACTIVATION_OPERATION: &str = "external_content_activation";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionMode {
    Online,
    Offline,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationJobOperation {
    pub operation_type: String,
    pub schema: String,
    pub activation_ref: String,
    pub activation_program_digest: String,
    pub activation_id: String,
    pub consumer_ref: String,
    pub publisher_fingerprint: String,
    pub operator_fingerprint: String,
    pub operator_authority_digest: String,
    pub policy_digest: String,
    pub acquisition_mode: AcquisitionMode,
    /// Optional node-policy root containing the exact signed archives for an
    /// explicitly offline invocation. The root is node-local acquisition
    /// authority only: its path and filesystem identity remain in node policy
    /// and never enter the portable activation program or receipt.
    pub offline_archive_root: Option<String>,
    pub offline_archive_root_authority_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedActivationPublication {
    pub activation_id: String,
    pub receipt_hash: String,
    pub idempotent: bool,
}

impl ManagedActivationJobOperation {
    pub fn new(
        activation: &ResolvedManagedExternalContentActivation,
        operator_fingerprint: String,
        operator_authority_digest: String,
        policy: &ExternalContentImportPolicyRecord,
        acquisition_mode: AcquisitionMode,
        offline_archive_root: Option<String>,
    ) -> anyhow::Result<Self> {
        let managed = policy.managed_activation.require_enabled()?;
        let offline_archive_root_authority_digest = offline_archive_root
            .as_deref()
            .map(|root| offline_archive_root_authority_digest(policy, root))
            .transpose()?;
        let operation = Self {
            operation_type: MANAGED_ACTIVATION_OPERATION.to_owned(),
            schema: "ryeos.external_content_activation_operation.v3".to_owned(),
            activation_ref: activation.activation_ref.clone(),
            activation_program_digest: activation.activation_program_digest.clone(),
            activation_id:
                ryeos_state::objects::ExternalContentActivationReceipt::derive_activation_id(
                    &activation.activation_program_digest,
                    &activation.document.consumer_ref,
                    &activation.publisher_fingerprint,
                )?,
            consumer_ref: activation.document.consumer_ref.clone(),
            publisher_fingerprint: activation.publisher_fingerprint.clone(),
            operator_fingerprint,
            operator_authority_digest,
            policy_digest: managed_policy_digest(&policy.limits, managed)?,
            acquisition_mode,
            offline_archive_root,
            offline_archive_root_authority_digest,
        };
        operation.validate_against_policy(policy)?;
        operation.validate()?;
        Ok(operation)
    }

    pub fn from_value(value: Value) -> anyhow::Result<Self> {
        let operation: Self = serde_json::from_value(value)
            .context("parse managed external-content activation operation")?;
        operation.validate()?;
        Ok(operation)
    }

    pub fn to_value(&self) -> anyhow::Result<Value> {
        self.validate()?;
        Ok(serde_json::to_value(self)?)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.operation_type != MANAGED_ACTIVATION_OPERATION
            || self.schema != "ryeos.external_content_activation_operation.v3"
        {
            bail!("managed activation operation schema or type is not current");
        }
        validate_canonical_ref("activation operation ref", &self.activation_ref)?;
        validate_canonical_ref("activation operation consumer", &self.consumer_ref)?;
        for (label, digest) in [
            (
                "activation operation program",
                &self.activation_program_digest,
            ),
            ("activation operation id", &self.activation_id),
            (
                "activation operation publisher",
                &self.publisher_fingerprint,
            ),
            ("activation operation operator", &self.operator_fingerprint),
            (
                "activation operation operator authority",
                &self.operator_authority_digest,
            ),
            ("activation operation policy", &self.policy_digest),
        ] {
            validate_hash(label, digest)?;
        }
        let expected =
            ryeos_state::objects::ExternalContentActivationReceipt::derive_activation_id(
                &self.activation_program_digest,
                &self.consumer_ref,
                &self.publisher_fingerprint,
            )?;
        if self.activation_id != expected {
            bail!("managed activation operation id contradicts its authority tuple");
        }
        match (
            self.acquisition_mode,
            self.offline_archive_root.as_deref(),
            self.offline_archive_root_authority_digest.as_deref(),
        ) {
            (AcquisitionMode::Online, Some(_), _) | (AcquisitionMode::Online, None, Some(_)) => {
                bail!("online managed activation cannot name an offline archive root")
            }
            (AcquisitionMode::Offline, Some(root), Some(digest)) => {
                crate::node_policy::sections::external_content::validate_root_name(root)?;
                validate_hash("offline activation root authority", digest)?;
            }
            (AcquisitionMode::Offline, None, None) => {}
            (AcquisitionMode::Online, None, None) => {}
            (AcquisitionMode::Offline, Some(_), None)
            | (AcquisitionMode::Offline, None, Some(_)) => {
                bail!("offline activation root authority is incomplete")
            }
        }
        Ok(())
    }

    fn validate_against_policy(
        &self,
        policy: &ExternalContentImportPolicyRecord,
    ) -> anyhow::Result<()> {
        self.validate()?;
        if let Some(root) = self.offline_archive_root.as_deref() {
            let expected = offline_archive_root_authority_digest(policy, root)?;
            if self.offline_archive_root_authority_digest.as_deref() != Some(expected.as_str()) {
                bail!("offline managed activation archive root authority changed");
            }
        }
        Ok(())
    }

    pub fn validate_current(
        &self,
        activation: &ResolvedManagedExternalContentActivation,
        policy: &ExternalContentImportPolicyRecord,
        operator_authority_digest: &str,
    ) -> anyhow::Result<()> {
        self.validate()?;
        let current = Self::new(
            activation,
            self.operator_fingerprint.clone(),
            operator_authority_digest.to_owned(),
            policy,
            self.acquisition_mode,
            self.offline_archive_root.clone(),
        )?;
        if self != &current {
            bail!("managed activation operation no longer matches signed config or node policy");
        }
        Ok(())
    }
}

fn offline_archive_root_authority_digest(
    policy: &ExternalContentImportPolicyRecord,
    root: &str,
) -> anyhow::Result<String> {
    crate::node_policy::sections::external_content::validate_root_name(root)?;
    let authority = policy.roots.get(root).ok_or_else(|| {
        anyhow::anyhow!("offline managed activation archive root is not admitted by node policy")
    })?;
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "schema": "ryeos.managed_external_content_offline_root.v1",
        "root": root,
        "authority": authority,
    }))
}

pub fn managed_policy_digest(
    limits: &crate::node_policy::sections::external_content::ExternalContentImportLimits,
    managed: &ManagedExternalContentActivationPolicy,
) -> anyhow::Result<String> {
    limits.validate()?;
    managed.validate()?;
    ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "schema":"ryeos.managed_external_content_node_policy.v1",
        "import_limits":limits,
        "managed_activation":managed,
    }))
}

pub fn publish_activation_receipt(
    state: &crate::state::AppState,
    activation: &ResolvedManagedExternalContentActivation,
    operation: &ManagedActivationJobOperation,
    mut components: Vec<ryeos_state::objects::ExternalContentActivationComponentReceipt>,
) -> anyhow::Result<ManagedActivationPublication> {
    operation.validate_current(
        activation,
        state.node_policy.require::<
            crate::node_policy::sections::external_content::ExternalContentImportPolicyRecord,
        >()?,
        &crate::operator_external_content::configured_operator_authority_digest(
            state,
            &operation.operator_fingerprint,
        )?,
    )?;
    components.sort_by(|left, right| left.id.cmp(&right.id));
    let receipt = ryeos_state::objects::ExternalContentActivationReceipt::new(
        activation.activation_ref.clone(),
        activation.activation_program_digest.clone(),
        activation.document.consumer_ref.clone(),
        activation.publisher_fingerprint.clone(),
        state.identity.fingerprint().to_owned(),
        operation.policy_digest.clone(),
        components,
        operation.operator_fingerprint.clone(),
    )?;
    if receipt.activation_id != operation.activation_id {
        bail!("managed activation receipt identity changed before publication");
    }
    let authority = state.state_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let namespace = ryeos_state::objects::EXTERNAL_CONTENT_ACTIVATION_HEAD_NAMESPACE;
    if let Some(current) = state
        .state_store
        .with_state_db(|db| db.read_generic_head_ref(namespace, &operation.activation_id))?
    {
        let value = cas
            .get_object(&current.target_hash)?
            .ok_or_else(|| anyhow::anyhow!("managed activation head target is absent"))?;
        let retained = ryeos_state::objects::ExternalContentActivationReceipt::from_value(&value)?;
        // This head identifies the exact activated realization, not the latest
        // invocation. A later activation under a narrower/current node policy
        // may reuse the already-bound bytes; its sync job retains that newer
        // policy/operator while the first completion receipt remains immutable.
        if !same_activated_realization(&retained, &receipt) {
            bail!("managed activation head contradicts the exact requested realization");
        }
        return Ok(ManagedActivationPublication {
            activation_id: operation.activation_id.clone(),
            receipt_hash: current.target_hash,
            idempotent: true,
        });
    }

    let _permit = state
        .write_barrier
        .acquire_with_timeout(crate::write_barrier::ONLINE_WRITE_PERMIT_TIMEOUT)
        .map_err(|error| anyhow::anyhow!("cannot acquire activation write permit: {error}"))?;
    let mut stage = authority
        .require_recovery()?
        .begin_staged_cas_roots_admitted(&guard, "managed-external-content-activation")?;
    let receipt_hash = stage.store_object_admitted(&guard, &cas, &receipt.to_value()?)?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [receipt_hash.clone()],
        ryeos_state::object_closure::ObjectClosureLimits::default(),
    )?;
    if !closure.is_complete() {
        bail!("managed activation receipt closure is incomplete");
    }
    stage.protect_cas_closure_admitted(
        &guard,
        closure.object_hashes.iter().map(String::as_str),
        closure.blob_hashes.iter().map(String::as_str),
    )?;
    for hash in &closure.large_object_hashes {
        stage.protect_large_object_hash_admitted(&guard, hash)?;
    }
    let signer = crate::state_store::NodeIdentitySigner::from_identity(&state.identity);
    state.state_store.with_state_db(|db| {
        db.advance_generic_head_ref(
            namespace,
            &operation.activation_id,
            &receipt_hash,
            None,
            &signer,
            &guard,
        )
    })?;
    if let Err(error) = stage.finish_admitted(&guard) {
        tracing::warn!(%error, activation_id = %operation.activation_id, "activation head published while temporary roots remained recoverable");
    }
    Ok(ManagedActivationPublication {
        activation_id: operation.activation_id.clone(),
        receipt_hash,
        idempotent: false,
    })
}

fn same_activated_realization(
    retained: &ryeos_state::objects::ExternalContentActivationReceipt,
    expected: &ryeos_state::objects::ExternalContentActivationReceipt,
) -> bool {
    retained.activation_id == expected.activation_id
        && retained.activation_ref == expected.activation_ref
        && retained.activation_program_digest == expected.activation_program_digest
        && retained.consumer_ref == expected.consumer_ref
        && retained.publisher_fingerprint == expected.publisher_fingerprint
        && retained.node_fingerprint == expected.node_fingerprint
        && retained.components == expected.components
}

fn validate_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(value) || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        bail!("{label} is not a canonical sha256 digest");
    }
    Ok(())
}

fn validate_canonical_ref(label: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 512
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        bail!("{label} is empty, unbounded, or non-canonical");
    }
    let parsed = ryeos_engine::canonical_ref::CanonicalRef::parse(value)
        .map_err(|error| anyhow::anyhow!("invalid {label}: {error}"))?;
    if parsed.to_string() != value {
        bail!("{label} is not canonical");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed_policy() -> ManagedExternalContentActivationPolicy {
        ManagedExternalContentActivationPolicy {
            allow_online: true,
            allowed_https_hosts: vec!["releases.example.test".to_owned()],
            max_redirects: 0,
            max_archives: 1,
            max_compressed_bytes: 4096,
            max_expanded_bytes: 8192,
            max_members: 8,
            max_member_bytes: 4096,
            max_concurrent_activations: 1,
            cache_budget_bytes: 16384,
            store_budget_bytes: 32768,
            minimum_free_bytes: 4096,
            max_attempts: 3,
        }
    }

    fn import_limits() -> crate::node_policy::sections::external_content::ExternalContentImportLimits
    {
        crate::node_policy::sections::external_content::ExternalContentImportLimits {
            max_depth: 8,
            max_entries: 64,
            max_file_bytes: 8192,
            max_total_bytes: 16384,
            store_budget_bytes: 32768,
            minimum_free_bytes: 4096,
        }
    }

    fn receipt(
        policy: char,
        operator: char,
    ) -> ryeos_state::objects::ExternalContentActivationReceipt {
        ryeos_state::objects::ExternalContentActivationReceipt::new(
            "config:fixture/activation".to_owned(),
            "a".repeat(64),
            "worker:fixture/hosted".to_owned(),
            "b".repeat(64),
            "c".repeat(64),
            policy.to_string().repeat(64),
            vec![
                ryeos_state::objects::ExternalContentActivationComponentReceipt {
                    id: "runtime".to_owned(),
                    binding_hash: "d".repeat(64),
                },
            ],
            operator.to_string().repeat(64),
        )
        .unwrap()
    }

    #[test]
    fn immutable_activation_head_can_satisfy_a_later_narrower_invocation() {
        let first = receipt('e', 'f');
        let later = receipt('1', '2');
        assert!(same_activated_realization(&first, &later));

        let mut different = later;
        different.components[0].binding_hash = "3".repeat(64);
        assert!(!same_activated_realization(&first, &different));
    }

    #[test]
    fn durable_policy_digest_includes_import_and_acquisition_ceilings() {
        let managed = managed_policy();
        let limits = import_limits();
        let baseline = managed_policy_digest(&limits, &managed).unwrap();

        let mut narrower_import = limits.clone();
        narrower_import.max_entries -= 1;
        assert_ne!(
            baseline,
            managed_policy_digest(&narrower_import, &managed).unwrap()
        );

        let mut narrower_acquisition = managed;
        narrower_acquisition.max_members -= 1;
        assert_ne!(
            baseline,
            managed_policy_digest(&limits, &narrower_acquisition).unwrap()
        );
    }

    #[test]
    fn offline_root_authority_digest_binds_only_the_selected_node_root() {
        let mut policy = ExternalContentImportPolicyRecord {
            schema: 1,
            roots: std::collections::BTreeMap::from([
                (
                    "archives".to_owned(),
                    crate::node_policy::sections::external_content::ExternalContentImportRoot {
                        path: std::path::PathBuf::from("/srv/ryeos-offline"),
                        containing_device: 7,
                        root_inode: 11,
                    },
                ),
                (
                    "unrelated".to_owned(),
                    crate::node_policy::sections::external_content::ExternalContentImportRoot {
                        path: std::path::PathBuf::from("/srv/unrelated"),
                        containing_device: 8,
                        root_inode: 12,
                    },
                ),
            ]),
            limits: import_limits(),
            managed_activation:
                crate::node_policy::sections::external_content::ManagedExternalContentPolicy {
                    enabled: true,
                    limits: Some(managed_policy()),
                },
        };
        let baseline = offline_archive_root_authority_digest(&policy, "archives").unwrap();

        policy.roots.get_mut("unrelated").unwrap().root_inode += 1;
        assert_eq!(
            baseline,
            offline_archive_root_authority_digest(&policy, "archives").unwrap(),
            "unselected roots must not perturb an offline acquisition job"
        );

        policy.roots.get_mut("archives").unwrap().root_inode += 1;
        assert_ne!(
            baseline,
            offline_archive_root_authority_digest(&policy, "archives").unwrap(),
            "selected root replacement must move durable operation identity"
        );
    }
}
