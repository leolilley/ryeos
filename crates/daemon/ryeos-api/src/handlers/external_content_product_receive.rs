//! Receive one exact admitted product through the existing pinned remote owner.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::node_policy::sections::external_content::ExternalContentImportPolicyRecord;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use serde::Deserialize;
use serde_json::Value;

use crate::registry::ServiceDescriptor;
use crate::remote::client::{
    NodeAdmittedObjectsClosureRequestOptions, ObjectsClosureRequestOptions, RemoteClient,
};
use crate::remote::import::{VerifiedRemoteImportRequest, import_admitted_root_with_job};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub remote: String,
    pub witness_hash: String,
    pub origin_admission_hash: String,
    pub maximum_bytes: u64,
}

impl Request {
    fn validate(&self) -> Result<()> {
        if self.remote.is_empty() || self.remote.len() > 256 || self.maximum_bytes == 0 {
            bail!("product receipt requires an exact remote and positive byte bound");
        }
        for hash in [&self.witness_hash, &self.origin_admission_hash] {
            if !lillux::valid_hash(hash) || hash.bytes().any(|byte| byte.is_ascii_uppercase()) {
                bail!("product receipt requires exact canonical witness and admission hashes");
            }
        }
        Ok(())
    }
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ryeos_app::operator_authority::require_local_configured_operator(&state, &ctx)?;
    req.validate()?;
    let policy = state
        .node_policy
        .require::<ExternalContentImportPolicyRecord>()?;
    if req.maximum_bytes > policy.limits.max_total_bytes {
        bail!("product receipt byte bound exceeds current import policy");
    }
    // This is node-owned receipt, not project-selected trust or a caller key
    // override. Recovery later retains the authenticated key in local testimony.
    let remotes = crate::remote::config::load_remotes_layered(&state.config.app_root, None)?;
    let remote = crate::remote::config::get_remote(&remotes, &req.remote)?;
    let key = remote
        .pinned_signing_key()
        .context("product origin has no pinned signing key")?;
    let client = RemoteClient::from_remote_cfg(&state, &remote);
    let imported = import_admitted_root_with_job(
        &state,
        &client,
        VerifiedRemoteImportRequest {
            subject_hash: req.witness_hash.clone(),
            policy: ryeos_state::admission::LOCAL_ADMISSION_POLICY.into(),
            expected_issuer: format!("fp:{}", lillux::crypto::fingerprint(&key)),
            expected_key: key,
            expected_attestation_hash: Some(req.origin_admission_hash.clone()),
            source_peer: Some(remote.name.clone()),
            job_id: None,
            closure_options: NodeAdmittedObjectsClosureRequestOptions::for_node(
                &state,
                ObjectsClosureRequestOptions {
                    max_total_blob_bytes: Some(req.maximum_bytes),
                    allow_incomplete: false,
                    allow_untransported_large_objects: false,
                    ..Default::default()
                },
            )?,
        },
    )
    .await?;
    // Existing Mirrored attribution durably roots imported CAS entries. A
    // refused local acceptance leaves only mirrored bytes, never consumer
    // authority. No private head or origin qualification is copied.
    let acceptance_state = Arc::clone(&state);
    let (accepted, product) = tokio::task::spawn_blocking(move || {
        ryeos_app::operator_external_content::product_receipt::accept_imported_product(
            &acceptance_state,
            &ctx,
            &req.origin_admission_hash,
            &req.witness_hash,
            &key,
            req.maximum_bytes,
        )
    })
    .await
    .context("product receiver acceptance task stopped")??;
    Ok(serde_json::json!({
        "witness_hash": product.attestation_hash,
        "origin_admission_hash": imported.import.attestation_hash,
        "acceptance_hash": accepted.attestation_hash,
        "reused_existing": accepted.reused_existing,
        "manifest_hash": product.evidence.manifest_hash,
        "job_id": imported.job_id,
        "attempt_id": imported.attempt_id,
    }))
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:external-content/receive-product",
    endpoint: "external-content.receive-product",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.external-content/receive-product"],
    handler: |params, ctx, state| {
        Box::pin(
            async move { handle(crate::handler_error::parse_request(params)?, ctx, state).await },
        )
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_requires_exact_origin_coordinates_and_no_caller_key() {
        let valid = serde_json::json!({
            "remote": "origin",
            "witness_hash": "a".repeat(64),
            "origin_admission_hash": "b".repeat(64),
            "maximum_bytes": 1024,
        });
        serde_json::from_value::<Request>(valid.clone())
            .unwrap()
            .validate()
            .unwrap();
        for field in [
            "remote",
            "witness_hash",
            "origin_admission_hash",
            "maximum_bytes",
        ] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Request>(missing).is_err());
        }
        for (field, value) in [
            ("remote", serde_json::json!("")),
            ("witness_hash", serde_json::json!("A".repeat(64))),
            ("origin_admission_hash", serde_json::json!("b".repeat(63))),
            ("maximum_bytes", serde_json::json!(0)),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<Request>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        let mut substituted = valid;
        substituted["signing_key"] = serde_json::json!("caller-selected-key");
        assert!(serde_json::from_value::<Request>(substituted).is_err());
    }
}
