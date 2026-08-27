//! Durable operation and completion publication for managed external-content
//! acquisition. Portable recipe compilation remains in
//! `managed_external_content`; this module owns node-local operation state.

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::managed_external_content::ResolvedManagedExternalContentActivation;
use crate::node_config::sections::external_content::ManagedExternalContentActivationPolicy;

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
    pub policy_digest: String,
    pub acquisition_mode: AcquisitionMode,
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
        policy: &ManagedExternalContentActivationPolicy,
        acquisition_mode: AcquisitionMode,
    ) -> anyhow::Result<Self> {
        let operation = Self {
            operation_type: MANAGED_ACTIVATION_OPERATION.to_owned(),
            schema: "ryeos.external_content_activation_operation.v1".to_owned(),
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
            policy_digest: managed_policy_digest(policy)?,
            acquisition_mode,
        };
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
            || self.schema != "ryeos.external_content_activation_operation.v1"
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
        Ok(())
    }

    pub fn validate_current(
        &self,
        activation: &ResolvedManagedExternalContentActivation,
        policy: &ManagedExternalContentActivationPolicy,
    ) -> anyhow::Result<()> {
        self.validate()?;
        let current = Self::new(
            activation,
            self.operator_fingerprint.clone(),
            policy,
            self.acquisition_mode,
        )?;
        if self != &current {
            bail!("managed activation operation no longer matches signed config or node policy");
        }
        Ok(())
    }
}

pub fn managed_policy_digest(
    policy: &ManagedExternalContentActivationPolicy,
) -> anyhow::Result<String> {
    policy.validate()?;
    ryeos_state::objects::canonical_value_digest(&serde_json::to_value(policy)?)
}

pub fn publish_activation_receipt(
    state: &crate::state::AppState,
    activation: &ResolvedManagedExternalContentActivation,
    operation: &ManagedActivationJobOperation,
    mut components: Vec<ryeos_state::objects::ExternalContentActivationComponentReceipt>,
) -> anyhow::Result<ManagedActivationPublication> {
    operation.validate_current(
        activation,
        state
            .node_config
            .external_content_import_policy
            .as_ref()
            .and_then(|policy| policy.managed_activation.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!("node has no managed external-content activation policy")
            })?,
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
        let mut expected = receipt.clone();
        expected.recorded_at = retained.recorded_at.clone();
        if retained != expected {
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
