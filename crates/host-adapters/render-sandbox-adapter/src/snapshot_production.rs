//! Render snapshot-creation observations for a separately owned product transfer.
//!
//! A successful create response binds an opaque provider locator to the
//! original one-shot intent. It is never evidence that the snapshot contains
//! the retained product; an independent restored-guest verifier owns that join.

use anyhow::{Context as _, Result, ensure};
use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::{RenderPlan, valid_sandbox_id, valid_snapshot_id};

const MAX_SNAPSHOT_RESPONSE_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotCreationIntent {
    pub schema: u32,
    pub operation_id: String,
    pub product_witness_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub controller_public_root: String,
    pub owner_executable_sha256: String,
    pub owner_id: String,
    pub sandbox_group_id: String,
    pub source_sandbox_id: String,
    pub plan: RenderPlan,
}

impl SnapshotCreationIntent {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported snapshot creation intent");
        for hash in [
            &self.product_witness_hash,
            &self.guest_runtime_manifest_hash,
            &self.owner_executable_sha256,
        ] {
            ensure!(
                lillux::valid_hash(hash),
                "snapshot source identity is invalid"
            );
        }
        ensure!(
            self.operation_id.len() <= 256
                && !self.operation_id.is_empty()
                && self
                    .operation_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "snapshot creation operation is invalid"
        );
        ensure!(
            self.owner_id.len() <= 256
                && !self.owner_id.is_empty()
                && self
                    .owner_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "snapshot creation owner is invalid"
        );
        ensure!(
            valid_sandbox_id(&self.source_sandbox_id)
                && self.sandbox_group_id.starts_with("sbg-")
                && self.sandbox_group_id.len() <= 256
                && self
                    .sandbox_group_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "snapshot creation source is invalid"
        );
        let root = self
            .controller_public_root
            .strip_prefix("ed25519:")
            .ok_or_else(|| anyhow::anyhow!("snapshot source has no controller public root"))?;
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, root)?;
        let bytes: [u8; 32] = decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("snapshot controller root has wrong length"))?;
        let key = lillux::crypto::VerifyingKey::from_bytes(&bytes)?;
        ensure!(
            !key.is_weak()
                && base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    key.to_bytes()
                ) == root,
            "snapshot controller root is weak or noncanonical"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        Ok(lillux::sha256_hex(
            lillux::canonical_json(&serde_json::to_value(self)?)?.as_bytes(),
        ))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RenderSnapshotCreateResponse {
    #[serde(deserialize_with = "required_nullable")]
    captured_at: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    error: Option<String>,
    expires_at: String,
    id: String,
    kind: SnapshotKind,
    #[serde(deserialize_with = "required_nullable")]
    name: Option<String>,
    plan: RenderPlan,
    requested_at: String,
    sandbox_group_id: String,
    #[serde(deserialize_with = "required_nullable")]
    size_bytes: Option<i64>,
    source_sandbox_id: String,
    status: SnapshotStatus,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SnapshotKind {
    Filesystem,
    Runtime,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SnapshotStatus {
    Creating,
    Available,
    Failed,
}

fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// A provider locator, not an installed-runtime or product qualification.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BoundSnapshotCreation {
    pub schema: u32,
    pub operation_id: String,
    pub intent_digest: String,
    pub product_witness_hash: String,
    pub guest_runtime_manifest_hash: String,
    pub controller_public_root: String,
    pub owner_executable_sha256: String,
    pub source_sandbox_id: String,
    pub sandbox_group_id: String,
    pub snapshot_id: String,
    pub requested_at: String,
    pub expires_at: String,
    pub response_sha256: String,
}

/// Provider readiness for the same opaque locator. It is not evidence of the
/// filesystem restored from this snapshot.
#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AvailableSnapshotObservation {
    pub schema: u32,
    pub operation_id: String,
    pub intent_digest: String,
    pub snapshot_id: String,
    pub source_sandbox_id: String,
    pub sandbox_group_id: String,
    pub captured_at: String,
    pub size_bytes: i64,
    pub creation_response_sha256: String,
    pub availability_response_sha256: String,
}

pub(crate) fn bind_snapshot_create_response(
    intent: &SnapshotCreationIntent,
    status: u16,
    body: &[u8],
) -> Result<BoundSnapshotCreation> {
    intent.validate()?;
    ensure!(
        status == 202 && !body.is_empty() && body.len() <= MAX_SNAPSHOT_RESPONSE_BYTES,
        "snapshot creation has no complete accepted provider response"
    );
    // Deserialize directly: a generic Value can silently collapse duplicate
    // keys before the exact response is checked.
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let snapshot = RenderSnapshotCreateResponse::deserialize(&mut decoder)?;
    decoder.end()?;
    ensure!(
        valid_snapshot_id(&snapshot.id)
            && snapshot.kind == SnapshotKind::Filesystem
            && snapshot.source_sandbox_id == intent.source_sandbox_id
            && snapshot.sandbox_group_id == intent.sandbox_group_id
            && snapshot.plan == intent.plan
            && snapshot.name.is_none()
            && snapshot.error.is_none(),
        "snapshot create response differs from exact source or filesystem kind"
    );
    let requested = DateTime::parse_from_rfc3339(&snapshot.requested_at)?;
    let expires = DateTime::parse_from_rfc3339(&snapshot.expires_at)?;
    ensure!(
        expires > requested,
        "snapshot expiry is not after its request"
    );
    match snapshot.status {
        SnapshotStatus::Creating => ensure!(
            snapshot.captured_at.is_none() && snapshot.size_bytes.is_none(),
            "creating snapshot claims captured content"
        ),
        SnapshotStatus::Available => ensure!(
            snapshot
                .captured_at
                .as_deref()
                .is_some_and(|value| DateTime::parse_from_rfc3339(value)
                    .is_ok_and(|captured| captured >= requested && captured < expires))
                && snapshot.size_bytes.is_some_and(|bytes| bytes > 0),
            "available snapshot lacks complete capture metadata"
        ),
        SnapshotStatus::Failed => anyhow::bail!("provider reported failed snapshot creation"),
    }
    Ok(BoundSnapshotCreation {
        schema: 1,
        operation_id: intent.operation_id.clone(),
        intent_digest: intent.digest()?,
        product_witness_hash: intent.product_witness_hash.clone(),
        guest_runtime_manifest_hash: intent.guest_runtime_manifest_hash.clone(),
        controller_public_root: intent.controller_public_root.clone(),
        owner_executable_sha256: intent.owner_executable_sha256.clone(),
        source_sandbox_id: intent.source_sandbox_id.clone(),
        sandbox_group_id: intent.sandbox_group_id.clone(),
        snapshot_id: snapshot.id,
        requested_at: snapshot.requested_at,
        expires_at: snapshot.expires_at,
        response_sha256: lillux::sha256_hex(body),
    })
}

pub(crate) fn observe_snapshot_available(
    intent: &SnapshotCreationIntent,
    creation: &BoundSnapshotCreation,
    status: u16,
    body: &[u8],
) -> Result<AvailableSnapshotObservation> {
    intent.validate()?;
    ensure!(
        creation.schema == 1
            && creation.operation_id == intent.operation_id
            && creation.intent_digest == intent.digest()?
            && creation.product_witness_hash == intent.product_witness_hash
            && creation.guest_runtime_manifest_hash == intent.guest_runtime_manifest_hash
            && creation.controller_public_root == intent.controller_public_root
            && creation.owner_executable_sha256 == intent.owner_executable_sha256
            && creation.source_sandbox_id == intent.source_sandbox_id
            && creation.sandbox_group_id == intent.sandbox_group_id
            && valid_snapshot_id(&creation.snapshot_id)
            && lillux::valid_hash(&creation.response_sha256),
        "snapshot readiness is not bound to its exact creation"
    );
    ensure!(
        status == 200 && !body.is_empty() && body.len() <= MAX_SNAPSHOT_RESPONSE_BYTES,
        "snapshot readiness has no complete provider response"
    );
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let snapshot = RenderSnapshotCreateResponse::deserialize(&mut decoder)?;
    decoder.end()?;
    ensure!(
        snapshot.id == creation.snapshot_id
            && snapshot.kind == SnapshotKind::Filesystem
            && snapshot.status == SnapshotStatus::Available
            && snapshot.source_sandbox_id == intent.source_sandbox_id
            && snapshot.sandbox_group_id == intent.sandbox_group_id
            && snapshot.plan == intent.plan
            && snapshot.name.is_none()
            && snapshot.error.is_none()
            && snapshot.requested_at == creation.requested_at
            && snapshot.expires_at == creation.expires_at,
        "available snapshot differs from its exact creation"
    );
    let requested = DateTime::parse_from_rfc3339(&snapshot.requested_at)?;
    let expires = DateTime::parse_from_rfc3339(&snapshot.expires_at)?;
    let captured_at = snapshot
        .captured_at
        .context("available snapshot has no capture timestamp")?;
    let captured = DateTime::parse_from_rfc3339(&captured_at)?;
    let size_bytes = snapshot
        .size_bytes
        .context("available snapshot has no byte count")?;
    ensure!(
        requested <= captured && captured < expires && size_bytes > 0,
        "available snapshot has invalid capture metadata"
    );
    Ok(AvailableSnapshotObservation {
        schema: 1,
        operation_id: intent.operation_id.clone(),
        intent_digest: creation.intent_digest.clone(),
        snapshot_id: creation.snapshot_id.clone(),
        source_sandbox_id: creation.source_sandbox_id.clone(),
        sandbox_group_id: creation.sandbox_group_id.clone(),
        captured_at,
        size_bytes,
        creation_response_sha256: creation.response_sha256.clone(),
        availability_response_sha256: lillux::sha256_hex(body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn intent() -> SnapshotCreationIntent {
        let key = lillux::crypto::SigningKey::from_bytes(&[43; 32]).verifying_key();
        SnapshotCreationIntent {
            schema: 1,
            operation_id: "snapshot-op-1".into(),
            product_witness_hash: "1".repeat(64),
            guest_runtime_manifest_hash: "2".repeat(64),
            controller_public_root: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(key.to_bytes())
            ),
            owner_executable_sha256: "3".repeat(64),
            owner_id: "owner".into(),
            sandbox_group_id: "sbg-exact".into(),
            source_sandbox_id: "sbx-exact".into(),
            plan: RenderPlan::Starter,
        }
    }

    fn response() -> serde_json::Value {
        serde_json::json!({
            "capturedAt": null, "error": null,
            "expiresAt": "2026-10-01T00:00:00Z", "id": "snp-exact",
            "kind": "filesystem", "name": null, "plan": "starter",
            "requestedAt": "2026-09-28T00:00:00Z",
            "sandboxGroupId": "sbg-exact", "sizeBytes": null,
            "sourceSandboxId": "sbx-exact", "status": "creating"
        })
    }

    #[test]
    fn accepted_create_only_binds_a_locator_for_the_exact_source() {
        let intent = intent();
        let body = serde_json::to_vec(&response()).unwrap();
        let bound = bind_snapshot_create_response(&intent, 202, &body).unwrap();
        assert_eq!(bound.snapshot_id, "snp-exact");
        assert_eq!(bound.product_witness_hash, intent.product_witness_hash);
        assert_eq!(bound.intent_digest, intent.digest().unwrap());
        assert!(bind_snapshot_create_response(&intent, 201, &body).is_err());
        for (field, value) in [
            ("kind", "runtime"),
            ("sourceSandboxId", "sbx-other"),
            ("sandboxGroupId", "sbg-other"),
            ("status", "failed"),
        ] {
            let mut changed = response();
            changed[field] = serde_json::json!(value);
            assert!(
                bind_snapshot_create_response(&intent, 202, &serde_json::to_vec(&changed).unwrap())
                    .is_err()
            );
        }
        let duplicated = String::from_utf8(body).unwrap().replace(
            "\"id\":\"snp-exact\"",
            "\"id\":\"snp-exact\",\"id\":\"snp-other\"",
        );
        assert!(bind_snapshot_create_response(&intent, 202, duplicated.as_bytes()).is_err());
        let mut missing = response();
        missing.as_object_mut().unwrap().remove("sizeBytes");
        assert!(
            bind_snapshot_create_response(&intent, 202, &serde_json::to_vec(&missing).unwrap())
                .is_err()
        );
    }

    #[test]
    fn availability_is_an_exact_readiness_observation_not_content_proof() {
        let intent = intent();
        let creation =
            bind_snapshot_create_response(&intent, 202, &serde_json::to_vec(&response()).unwrap())
                .unwrap();
        let mut available = response();
        available["status"] = serde_json::json!("available");
        available["capturedAt"] = serde_json::json!("2026-09-28T00:01:00Z");
        available["sizeBytes"] = serde_json::json!(4096);
        let body = serde_json::to_vec(&available).unwrap();
        let observed = observe_snapshot_available(&intent, &creation, 200, &body).unwrap();
        assert_eq!(observed.snapshot_id, creation.snapshot_id);
        assert_eq!(observed.creation_response_sha256, creation.response_sha256);
        assert_eq!(
            observed.availability_response_sha256,
            lillux::sha256_hex(&body)
        );
        assert!(observe_snapshot_available(&intent, &creation, 202, &body).is_err());
        for (field, replacement) in [
            ("status", serde_json::json!("creating")),
            ("kind", serde_json::json!("runtime")),
            ("sourceSandboxId", serde_json::json!("sbx-other")),
            ("id", serde_json::json!("snp-other")),
            ("requestedAt", serde_json::json!("2026-09-27T00:00:00Z")),
            ("sizeBytes", serde_json::json!(0)),
        ] {
            let mut changed = available.clone();
            changed[field] = replacement;
            assert!(
                observe_snapshot_available(
                    &intent,
                    &creation,
                    200,
                    &serde_json::to_vec(&changed).unwrap()
                )
                .is_err(),
                "{field}"
            );
        }
        let duplicated = String::from_utf8(body).unwrap().replace(
            "\"id\":\"snp-exact\"",
            "\"id\":\"snp-exact\",\"id\":\"snp-other\"",
        );
        assert!(
            observe_snapshot_available(&intent, &creation, 200, duplicated.as_bytes()).is_err()
        );
    }
}
