//! Provider-side inventory and the evidence an unknown-allocation settlement
//! must cite.
//!
//! The node records what it asked a provider for; it never asks what exists.
//! An inventory lists, through one installed binding's adapter and vault
//! credential, the sandboxes and snapshots the provider holds for that
//! binding's owner scope, and joins each with the node's journals:
//!
//! - `known_live`: the node holds it open and the provider has it;
//! - `leaked`: the node's record ended, the provider still has it running;
//! - `unknown`: the node has no record of it (another node, a hand-made
//!   resource, or the lost answer of a quarantined allocation);
//! - `gone`: the node holds it open and the provider no longer has it running
//!   (reported only from a complete listing).
//!
//! Nothing here deletes anything. The result is stored as a node-signed CAS
//! receipt whose digest `settle-unknown-allocation` must cite.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result, bail, ensure};
use base64::Engine as _;
use lillux::crypto::{Signer as _, Verifier as _};
use ryeos_external_execution_contract::provider_inventory::{
    ProviderInventoryAdapterResponse, ProviderResourceKind,
};
use serde::{Deserialize, Serialize};

use crate::handler_context::HandlerContext;
use crate::handler_error::HandlerError;
use crate::runtime_db::provider_inventory::NodeProviderRecord;
use crate::state::AppState;

pub const RECEIPT_KIND: &str = "provider_inventory_receipt";
const RECEIPT_SCHEMA: u32 = 1;
const MAX_RECEIPT_BYTES: u64 = 512 * 1024;
/// Provider and node clocks are not the same clock: a resource created this
/// close outside an allocation's contact window is treated as inside it.
pub const CONTACT_WINDOW_SKEW_MS: i64 = 120_000;

/// The receipt cited is not usable evidence for this settlement.
pub const SETTLE_INVENTORY_REFUSED: &str = "settle_inventory_refused";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryClass {
    KnownLive,
    Leaked,
    Unknown,
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryRow {
    pub class: InventoryClass,
    pub kind: ProviderResourceKind,
    pub provider_id: String,
    /// The provider's state; absent for a `gone` row the provider did not list.
    pub provider_state: Option<String>,
    /// Whether the provider reports the resource ended (terminated/deleted).
    pub ended: bool,
    pub created_at: Option<String>,
    /// The node journal and coordinate that name it; absent for `unknown`.
    pub node_source: Option<String>,
    pub node_reference: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryBinding {
    pub section: String,
    pub id: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryReceiptBody {
    pub schema: u32,
    pub kind: String,
    pub node_fingerprint: String,
    pub binding: InventoryBinding,
    pub backend: String,
    pub account: String,
    pub owner_scope: String,
    pub observed_at_ms: i64,
    pub complete: bool,
    pub rows: Vec<InventoryRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedReceipt {
    kind: String,
    body: InventoryReceiptBody,
    signer: String,
    signature: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InventoryReport {
    pub receipt: String,
    pub body: InventoryReceiptBody,
    pub counts: BTreeMap<InventoryClass, usize>,
}

fn ended_state(state: &str) -> bool {
    matches!(
        state.to_ascii_lowercase().as_str(),
        "terminated" | "deleted" | "destroyed" | "expired"
    )
}

/// Join one provider listing with the node's journals. Records of other
/// backends are not this listing's to judge.
pub fn classify(
    backend: &str,
    listing: &ProviderInventoryAdapterResponse,
    records: &[NodeProviderRecord],
) -> Vec<InventoryRow> {
    let mut by_id: BTreeMap<(ProviderResourceKind, &str), Vec<&NodeProviderRecord>> =
        BTreeMap::new();
    for record in records.iter().filter(|record| record.backend == backend) {
        by_id
            .entry((record.kind, record.provider_id.as_str()))
            .or_default()
            .push(record);
    }
    let mut rows = Vec::new();
    let mut listed = std::collections::BTreeSet::new();
    for resource in &listing.resources {
        listed.insert((resource.kind, resource.provider_id.as_str()));
        let ended = ended_state(&resource.state);
        let matching = by_id.get(&(resource.kind, resource.provider_id.as_str()));
        let live_record = matching.and_then(|records| records.iter().find(|r| r.live));
        let any_record = matching.and_then(|records| records.first());
        let (class, record) = match (live_record, any_record, ended) {
            (Some(record), _, false) => (InventoryClass::KnownLive, Some(*record)),
            (Some(record), _, true) => (InventoryClass::Gone, Some(*record)),
            (None, Some(record), false) => (InventoryClass::Leaked, Some(*record)),
            // Ended at the provider and ended in the node: consistent.
            (None, Some(_), true) => continue,
            (None, None, _) => (InventoryClass::Unknown, None),
        };
        rows.push(InventoryRow {
            class,
            kind: resource.kind,
            provider_id: resource.provider_id.clone(),
            provider_state: Some(resource.state.clone()),
            ended,
            created_at: resource.created_at.clone(),
            node_source: record.map(|record| record.source.to_owned()),
            node_reference: record.map(|record| record.node_reference.clone()),
        });
    }
    if listing.complete {
        for record in records.iter().filter(|record| {
            record.backend == backend
                && record.live
                && !listed.contains(&(record.kind, record.provider_id.as_str()))
        }) {
            rows.push(InventoryRow {
                class: InventoryClass::Gone,
                kind: record.kind,
                provider_id: record.provider_id.clone(),
                provider_state: None,
                ended: false,
                created_at: None,
                node_source: Some(record.source.to_owned()),
                node_reference: Some(record.node_reference.clone()),
            });
        }
    }
    rows
}

fn sign_receipt(
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    body: InventoryReceiptBody,
) -> Result<serde_json::Value> {
    let bytes = lillux::canonical_json(&body)?;
    let signature = identity.signing_key().sign(bytes.as_bytes());
    Ok(serde_json::to_value(SignedReceipt {
        kind: RECEIPT_KIND.into(),
        body,
        signer: identity.fingerprint().to_owned(),
        signature: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
    })?)
}

/// Verify a receipt object was signed by this node and return its body.
pub fn verify_receipt(
    identity: &ryeos_node_runtime::identity::NodeIdentity,
    value: serde_json::Value,
) -> Result<InventoryReceiptBody> {
    let signed: SignedReceipt =
        serde_json::from_value(value).context("object is not a provider inventory receipt")?;
    ensure!(
        signed.kind == RECEIPT_KIND
            && signed.body.kind == RECEIPT_KIND
            && signed.body.schema == RECEIPT_SCHEMA,
        "object is not a provider inventory receipt"
    );
    ensure!(
        signed.signer == identity.fingerprint()
            && signed.body.node_fingerprint == identity.fingerprint(),
        "provider inventory receipt was not signed by this node"
    );
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&signed.signature)
        .context("receipt signature is not base64")?;
    let signature = lillux::crypto::Signature::from_slice(&signature)
        .context("receipt signature is malformed")?;
    identity
        .verifying_key()
        .verify(lillux::canonical_json(&signed.body)?.as_bytes(), &signature)
        .context("provider inventory receipt signature does not verify")?;
    Ok(signed.body)
}

/// List what the provider holds for one installed binding's owner scope,
/// join it with the node's journals and store a node-signed receipt.
pub fn run_provider_inventory(
    state: &AppState,
    context: &HandlerContext,
    section: &str,
    id: &str,
) -> Result<InventoryReport> {
    crate::operator_authority::require_admitted_operator(state, context)?;
    let config = &state.node_config;
    let (binding, backend, account, adapter, settings, settings_digest, network, access, timeout) =
        match section {
            crate::node_config::sections::external_execution::SECTION_NAME => {
                let binding = config
                    .external_execution
                    .iter()
                    .find(|binding| binding.id() == id)
                    .with_context(|| format!("no installed external_execution binding `{id}`"))?;
                let contract = binding.backend_contract();
                (
                    InventoryBinding {
                        section: section.into(),
                        id: id.into(),
                        digest: binding.digest().into(),
                    },
                    contract.backend.clone(),
                    contract.account.clone(),
                    contract.backend_artifact_hash.clone(),
                    contract.settings.clone(),
                    contract.settings_digest.clone(),
                    contract.provider_network_inputs.clone(),
                    binding.credential_access()?,
                    contract.contact_timeout_seconds,
                )
            }
            crate::node_config::sections::runtime_snapshot_production::SECTION_NAME => {
                let binding = config
                    .runtime_snapshot_production
                    .iter()
                    .find(|binding| binding.id() == id)
                    .with_context(|| {
                        format!("no installed runtime_snapshot_production binding `{id}`")
                    })?;
                (
                    InventoryBinding {
                        section: section.into(),
                        id: id.into(),
                        digest: binding.digest().into(),
                    },
                    binding.backend().to_owned(),
                    binding.account().to_owned(),
                    binding.adapter_artifact_hash().to_owned(),
                    binding.settings().clone(),
                    binding.settings_digest().to_owned(),
                    binding.network_inputs().clone(),
                    binding.credential_access()?,
                    binding.contact_timeout_seconds(),
                )
            }
            other => bail!(
                "provider inventory lists through an external_execution or \
                 runtime_snapshot_production binding, not `{other}`"
            ),
        };
    let credential = access.decode(state.vault.placement_credential(&access)?)?;
    let deadline = lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(
        u64::from(timeout),
    ));
    let listing = state.external_placement_backends.list_provider_resources(
        &backend,
        &adapter,
        &crate::external_placement::ProviderInventorySource {
            network_inputs: &network,
            settings: &settings,
            settings_digest: &settings_digest,
        },
        &credential,
        deadline,
    )?;
    let observed_at_ms = lillux::time::timestamp_millis();
    let records = state.state_store.provider_resource_records()?;
    let rows = classify(&backend, &listing, &records);
    let body = InventoryReceiptBody {
        schema: RECEIPT_SCHEMA,
        kind: RECEIPT_KIND.into(),
        node_fingerprint: state.identity.fingerprint().to_owned(),
        binding,
        backend,
        account,
        owner_scope: listing.owner_scope.clone(),
        observed_at_ms,
        complete: listing.complete,
        rows,
    };
    let signed = sign_receipt(&state.identity, body.clone())?;
    let receipt = state
        .state_store
        .pinned_state_authority()?
        .cas_store()?
        .store_object(&signed)?;
    let mut counts = BTreeMap::new();
    for row in &body.rows {
        *counts.entry(row.class).or_insert(0) += 1;
    }
    Ok(InventoryReport {
        receipt,
        body,
        counts,
    })
}

/// Load and verify a receipt this node stored.
pub fn load_receipt(state: &AppState, digest: &str) -> Result<InventoryReceiptBody> {
    ensure!(
        lillux::valid_hash(digest),
        "inventory receipt digest is invalid"
    );
    let value = state
        .state_store
        .pinned_state_authority()?
        .cas_store()?
        .get_object_bounded(digest, MAX_RECEIPT_BYTES)?
        .with_context(|| {
            format!("inventory receipt {digest} is absent; run the inventory again")
        })?;
    verify_receipt(&state.identity, value)
}

/// What a settlement needs to know of the quarantined allocation.
#[derive(Debug, Clone)]
pub struct AllocationWindow {
    pub backend: String,
    pub account: String,
    pub startup_started_at_ms: i64,
    pub contact_deadline_ms: i64,
}

fn refusal(message: String, detail: serde_json::Value) -> anyhow::Error {
    HandlerError::Structured {
        code: SETTLE_INVENTORY_REFUSED.into(),
        status: 409,
        body: serde_json::json!({
            "error": SETTLE_INVENTORY_REFUSED,
            "message": message,
            "detail": detail,
            "fix": "run `ryeos node external-execution inventory --binding <section>/<id>` \
                    after the allocation's contact window, resolve any unknown resource it \
                    lists inside that window, then settle citing the new receipt",
        }),
    }
    .into()
}

/// Decide whether `receipt` is evidence enough to settle an allocation with
/// `window`: fresh (taken after the allocation's last possible contact),
/// complete, for the same provider account, and listing no unknown resource
/// still running that was created inside the contact window.
pub fn check_settlement_evidence(
    receipt: &InventoryReceiptBody,
    window: &AllocationWindow,
) -> Result<()> {
    if receipt.backend != window.backend || receipt.account != window.account {
        return Err(refusal(
            format!(
                "the receipt lists {}/{}, not the allocation's provider account {}/{}",
                receipt.backend, receipt.account, window.backend, window.account
            ),
            serde_json::json!({ "receipt_backend": receipt.backend, "receipt_account": receipt.account }),
        ));
    }
    if receipt.observed_at_ms <= window.contact_deadline_ms {
        return Err(refusal(
            "the receipt is older than the allocation's last possible provider contact".into(),
            serde_json::json!({
                "observed_at_ms": receipt.observed_at_ms,
                "contact_deadline_ms": window.contact_deadline_ms,
            }),
        ));
    }
    if !receipt.complete {
        return Err(refusal(
            "the receipt's provider listing is incomplete; absence proves nothing".into(),
            serde_json::json!({}),
        ));
    }
    let from = window.startup_started_at_ms - CONTACT_WINDOW_SKEW_MS;
    let to = window.contact_deadline_ms + CONTACT_WINDOW_SKEW_MS;
    let suspects: Vec<&InventoryRow> = receipt
        .rows
        .iter()
        .filter(|row| {
            row.class == InventoryClass::Unknown
                && row.kind == ProviderResourceKind::Sandbox
                && !row.ended
                && match row
                    .created_at
                    .as_deref()
                    .map(chrono::DateTime::parse_from_rfc3339)
                {
                    Some(Ok(at)) => (from..=to).contains(&at.timestamp_millis()),
                    // An unknown sandbox of unknown age cannot be excluded.
                    _ => true,
                }
        })
        .collect();
    if !suspects.is_empty() {
        return Err(refusal(
            format!(
                "the receipt lists {} unknown running sandbox(es) created inside the \
                 allocation's contact window; one may be its lost occurrence",
                suspects.len()
            ),
            serde_json::json!({ "unknown": suspects }),
        ));
    }
    Ok(())
}

/// The contact window and provider account of one recorded allocation.
pub fn allocation_window(state: &AppState, placement: &str) -> Result<AllocationWindow> {
    let record = state
        .state_store
        .external_allocation(placement)?
        .context("external allocation is absent")?;
    let binding = state
        .state_store
        .retained_external_binding(&record.reservation.binding_hash)?
        .context("external allocation names no retained binding")?;
    let contract = binding.backend_contract();
    Ok(AllocationWindow {
        backend: contract.backend,
        account: contract.account,
        startup_started_at_ms: record.reservation.startup_started_at_ms,
        contact_deadline_ms: record.reservation.contact_deadline_ms,
    })
}

/// Settle one quarantined allocation on the operator's word, which must cite
/// a fresh, clean inventory receipt of the allocation's provider account.
pub fn settle_unknown_allocation(
    state: &AppState,
    principal: &str,
    placement: &str,
    reason: &str,
    inventory: &str,
) -> Result<String> {
    let receipt = load_receipt(state, inventory).map_err(|error| {
        refusal(
            format!("{error:#}"),
            serde_json::json!({ "inventory": inventory }),
        )
    })?;
    let window = allocation_window(state, placement)?;
    check_settlement_evidence(&receipt, &window)?;
    state.state_store.settle_unknown_external_allocation(
        placement,
        principal,
        reason,
        &format!("provider inventory receipt {inventory}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::provider_inventory::{
        PROVIDER_INVENTORY_PROTOCOL, ProviderResource,
    };

    fn resource(kind: ProviderResourceKind, id: &str, state: &str) -> ProviderResource {
        ProviderResource {
            kind,
            provider_id: id.into(),
            state: state.into(),
            created_at: Some("2026-11-13T00:00:00Z".into()),
        }
    }

    fn record(id: &str, live: bool, backend: &str) -> NodeProviderRecord {
        NodeProviderRecord {
            kind: ProviderResourceKind::Sandbox,
            provider_id: id.into(),
            source: "allocation",
            node_reference: format!("T-{id}"),
            live,
            backend: backend.into(),
        }
    }

    fn listing(
        resources: Vec<ProviderResource>,
        complete: bool,
    ) -> ProviderInventoryAdapterResponse {
        ProviderInventoryAdapterResponse {
            protocol: PROVIDER_INVENTORY_PROTOCOL.into(),
            provider_id: "render".into(),
            owner_scope: "tea-owner".into(),
            complete,
            resources,
        }
    }

    fn class_of(rows: &[InventoryRow], id: &str) -> Option<InventoryClass> {
        rows.iter()
            .find(|row| row.provider_id == id)
            .map(|row| row.class)
    }

    #[test]
    fn inventory_classes_leaked_unknown_gone() {
        let sandbox = ProviderResourceKind::Sandbox;
        let rows = classify(
            "render",
            &listing(
                vec![
                    resource(sandbox, "sbx-live", "running"),
                    resource(sandbox, "sbx-leak", "running"),
                    resource(sandbox, "sbx-stranger", "running"),
                    resource(sandbox, "sbx-done", "terminated"),
                    resource(sandbox, "sbx-ended-early", "terminated"),
                ],
                true,
            ),
            &[
                record("sbx-live", true, "render"),
                record("sbx-leak", false, "render"),
                record("sbx-done", false, "render"),
                record("sbx-ended-early", true, "render"),
                record("sbx-missing", true, "render"),
                // Another backend's record is not this listing's to judge.
                record("sbx-elsewhere", true, "synthetic"),
            ],
        );
        assert_eq!(class_of(&rows, "sbx-live"), Some(InventoryClass::KnownLive));
        assert_eq!(class_of(&rows, "sbx-leak"), Some(InventoryClass::Leaked));
        assert_eq!(
            class_of(&rows, "sbx-stranger"),
            Some(InventoryClass::Unknown)
        );
        assert_eq!(class_of(&rows, "sbx-done"), None);
        assert_eq!(
            class_of(&rows, "sbx-ended-early"),
            Some(InventoryClass::Gone)
        );
        assert_eq!(class_of(&rows, "sbx-missing"), Some(InventoryClass::Gone));
        assert_eq!(class_of(&rows, "sbx-elsewhere"), None);

        // An incomplete listing proves nothing about absence.
        let rows = classify(
            "render",
            &listing(vec![], false),
            &[record("sbx-missing", true, "render")],
        );
        assert!(rows.is_empty());
    }

    fn identity() -> (
        tempfile::TempDir,
        ryeos_node_runtime::identity::NodeIdentity,
    ) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("node/private_key.pem");
        let identity = ryeos_node_runtime::identity::NodeIdentity::create(&path).unwrap();
        (root, identity)
    }

    fn body(identity: &ryeos_node_runtime::identity::NodeIdentity) -> InventoryReceiptBody {
        InventoryReceiptBody {
            schema: RECEIPT_SCHEMA,
            kind: RECEIPT_KIND.into(),
            node_fingerprint: identity.fingerprint().into(),
            binding: InventoryBinding {
                section: "external_execution".into(),
                id: "codex".into(),
                digest: "d".repeat(64),
            },
            backend: "render".into(),
            account: "acct".into(),
            owner_scope: "tea-owner".into(),
            observed_at_ms: 10_000_000,
            complete: true,
            rows: Vec::new(),
        }
    }

    #[test]
    fn inventory_receipt_is_node_signed() {
        let (_root, identity) = identity();
        let signed = sign_receipt(&identity, body(&identity)).unwrap();
        assert_eq!(
            verify_receipt(&identity, signed.clone()).unwrap(),
            body(&identity)
        );

        let mut tampered = signed.clone();
        tampered["body"]["complete"] = false.into();
        assert!(verify_receipt(&identity, tampered).is_err());

        let (_other_root, other) = self::identity();
        assert!(verify_receipt(&other, signed).is_err());
    }

    fn window() -> AllocationWindow {
        AllocationWindow {
            backend: "render".into(),
            account: "acct".into(),
            startup_started_at_ms: chrono::DateTime::parse_from_rfc3339("2026-11-13T00:00:00Z")
                .unwrap()
                .timestamp_millis(),
            contact_deadline_ms: chrono::DateTime::parse_from_rfc3339("2026-11-13T00:01:00Z")
                .unwrap()
                .timestamp_millis(),
        }
    }

    fn refusal_code(error: anyhow::Error) -> String {
        match crate::handler_error::extract_handler_error(&error) {
            Some(HandlerError::Structured { code, status, .. }) => {
                assert_eq!(status, 409);
                code
            }
            other => panic!("typed refusal expected: {other:?}"),
        }
    }

    fn unknown(id: &str, created_at: &str, ended: bool) -> InventoryRow {
        InventoryRow {
            class: InventoryClass::Unknown,
            kind: ProviderResourceKind::Sandbox,
            provider_id: id.into(),
            provider_state: Some(if ended { "terminated" } else { "running" }.into()),
            ended,
            created_at: Some(created_at.into()),
            node_source: None,
            node_reference: None,
        }
    }

    #[test]
    fn settle_refuses_stale_inventory() {
        let (_root, identity) = identity();
        let mut receipt = body(&identity);
        receipt.observed_at_ms = window().contact_deadline_ms;
        assert_eq!(
            refusal_code(check_settlement_evidence(&receipt, &window()).unwrap_err()),
            SETTLE_INVENTORY_REFUSED
        );
        receipt.observed_at_ms = window().contact_deadline_ms + 1;
        receipt.complete = false;
        assert!(check_settlement_evidence(&receipt, &window()).is_err());
        receipt.complete = true;
        receipt.account = "other".into();
        assert!(check_settlement_evidence(&receipt, &window()).is_err());
    }

    #[test]
    fn settle_refuses_unknown_in_contact_window() {
        let (_root, identity) = identity();
        let mut receipt = body(&identity);
        receipt.observed_at_ms = window().contact_deadline_ms + 1;
        receipt.rows = vec![unknown("sbx-lost", "2026-11-13T00:00:30Z", false)];
        assert_eq!(
            refusal_code(check_settlement_evidence(&receipt, &window()).unwrap_err()),
            SETTLE_INVENTORY_REFUSED
        );
        // An unknown of no stated age cannot be excluded either.
        receipt.rows[0].created_at = None;
        assert!(check_settlement_evidence(&receipt, &window()).is_err());
    }

    #[test]
    fn settle_accepts_clean_inventory() {
        let (_root, identity) = identity();
        let mut receipt = body(&identity);
        receipt.observed_at_ms = window().contact_deadline_ms + 1;
        receipt.rows = vec![
            // Created a day before the window.
            unknown("sbx-old", "2026-11-12T00:00:00Z", false),
            // Inside the window but already ended at the provider.
            unknown("sbx-ended", "2026-11-13T00:00:30Z", true),
        ];
        check_settlement_evidence(&receipt, &window()).unwrap();
    }
}
