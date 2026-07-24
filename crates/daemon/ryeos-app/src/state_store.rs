use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use ryeos_runtime::checkpoint::{checkpoint_shape_limits, validate_checkpoint_shape};
use ryeos_runtime::RuntimeJsonArrayBudget;
use ryeos_state::chain::{ChainLock, SnapshotUpdate};
use ryeos_state::objects::thread_snapshot::{parse_canonical_timestamp, ThreadStatus};
use ryeos_state::objects::ThreadSnapshot;
use ryeos_state::objects::ThreadUsage;
use ryeos_state::queries;
use ryeos_state::signer::Signer;
use ryeos_state::StateDb;
use ryeos_state::UsageSubject;

use crate::projection_health::ThreadProjectionHealth;
use crate::runtime_db;
use crate::write_barrier::{WriteBarrier, WritePermit};
pub use runtime_db::{
    CommandRecord, HookDispatchReservation, LaunchPlanningCapacityExceeded, LaunchPlanningRecord,
    NewCommandRecord, NewHookDispatch, RuntimeInfo, StopIntent,
};

mod projection_access;

use projection_access::committed_value;

const MAX_THREAD_ARTIFACT_ITEMS: usize = 512;
const MAX_THREAD_ARTIFACT_TYPE_BYTES: usize = 1024;
const MAX_THREAD_ARTIFACT_METADATA_BYTES: usize = 256 * 1024;
const MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_THREAD_ARTIFACT_RESPONSE_BYTES: usize = 6 * 1024 * 1024;
const MAX_THREAD_FACET_ITEMS: usize = 128;
const MAX_THREAD_FACET_KEY_BYTES: usize = 4 * 1024;
const MAX_THREAD_FACET_VALUE_BYTES: usize = 256 * 1024;
const MAX_THREAD_FACET_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_THREAD_FACET_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_THREAD_LIST_ENRICHMENT_THREADS: usize = 2_000;
const MAX_THREAD_LIST_FACET_ITEMS: usize = 8 * 1024;
const MAX_THREAD_LIST_FACET_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_THREAD_LIST_FACET_RESPONSE_BYTES: usize = 6 * 1024 * 1024;
const MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_THREAD_LIST_ERROR_PREVIEW_BYTES: usize = 2 * 1024;
const MAX_ACTIVE_LAUNCH_SIGNALS: usize = 4_096;
/// Exact JSON budget for the response-facing thread result record. The
/// projection content itself is capped by the 512 KiB ThreadEvent ceiling;
/// four MiB also covers worst-case JSON escaping of a malformed stored error
/// converted to a JSON string.
const MAX_THREAD_RESULT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchCancellationResolution {
    Cancelled,
    Bound {
        thread_id: String,
    },
    Terminal {
        state: String,
        outcome_code: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchTaskAbortRegistrationError {
    #[error("active launch task signal registry reached its bounded capacity")]
    CapacityExceeded,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchPlanningReservationError {
    #[error(transparent)]
    CapacityExceeded(#[from] LaunchPlanningCapacityExceeded),
    #[error(transparent)]
    Internal(anyhow::Error),
}

fn map_launch_planning_reservation_error(error: anyhow::Error) -> LaunchPlanningReservationError {
    if error
        .chain()
        .any(|cause| cause.is::<LaunchPlanningCapacityExceeded>())
    {
        LaunchPlanningReservationError::CapacityExceeded(LaunchPlanningCapacityExceeded)
    } else {
        LaunchPlanningReservationError::Internal(error)
    }
}

/// A durable planning cancel or daemon-generation fence won before the
/// authoritative root or continuation row could be published.
///
/// This typed marker may travel through `anyhow` context so route-independent
/// launchers can preserve the stable public `launch_cancelled` contract without
/// inspecting an error message.
#[derive(Debug, thiserror::Error)]
#[error("launch planning admission is no longer active")]
pub struct LaunchPlanningInactive;

#[derive(Debug, Clone, Serialize)]
pub struct PersistedEventRecord {
    pub event_id: i64,
    /// CAS hash of the signed thread event object for durable records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_hash: Option<String>,
    pub chain_root_id: String,
    pub chain_seq: i64,
    pub thread_id: String,
    pub thread_seq: i64,
    pub event_type: String,
    pub storage_class: String,
    pub ts: String,
    /// Hash links into the braid (truth chrome): present on durable rows,
    /// absent on synthetic/ephemeral records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_chain_event_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_thread_event_hash: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct PersistedEventPage {
    pub events: Vec<PersistedEventRecord>,
    pub has_more: bool,
}

pub struct NodeIdentitySigner {
    fingerprint: String,
    signing_key: lillux::crypto::SigningKey,
}

impl NodeIdentitySigner {
    pub fn from_identity(identity: &crate::identity::NodeIdentity) -> Self {
        Self {
            fingerprint: identity.fingerprint().to_string(),
            signing_key: identity.signing_key().clone(),
        }
    }
}

impl Signer for NodeIdentitySigner {
    fn sign(&self, data: &[u8]) -> Vec<u8> {
        use lillux::crypto::Signer as Ed25519Signer;
        self.signing_key.sign(data).to_bytes().to_vec()
    }

    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
        self.signing_key.verifying_key()
    }
}

#[derive(Debug, Clone)]
pub struct NewThreadRecord {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_thread_id: Option<String>,
    pub requested_by: Option<String>,
    pub project_root: Option<PathBuf>,
    pub project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    /// Immutable project generation that authorizes this thread from birth.
    pub base_project_snapshot_hash: Option<String>,
    pub usage_subject: Option<UsageSubject>,
    pub usage_subject_asserted_by: Option<String>,
    /// Destructive history authority captured only on a new chain root.
    /// Continuation members leave this absent and inherit the root policy.
    pub captured_history_policy: Option<ryeos_state::objects::CapturedThreadHistoryPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEventRecord {
    pub event_type: String,
    pub storage_class: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct NewArtifactRecord {
    pub artifact_type: String,
    pub uri: String,
    pub content_hash: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug)]
pub struct NewBundleEventAttachment {
    pub name: String,
    pub bytes: Vec<u8>,
    pub media_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FinalizeThreadRecord {
    pub status: String,
    pub outcome_code: Option<String>,
    pub result_json: Option<Value>,
    pub error_json: Option<Value>,
    pub artifacts: Vec<NewArtifactRecord>,
    pub final_cost: Option<ryeos_engine::contracts::FinalCost>,
    /// Exact native runtime envelope received at callback/fallback settlement.
    /// Persisted in the signed snapshot so later stdout reconciliation and API
    /// responses have one payload authority, not a second process claim.
    pub managed_envelope: Option<Value>,
    /// Immutable generation produced by this exact execution owner. It may be
    /// established only by the terminal transition.
    pub result_project_snapshot_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedTerminalEnvelope {
    success: bool,
    child_thread_id: String,
    status: ryeos_runtime::envelope::RuntimeResultStatus,
    result: Value,
    outputs: Value,
    warnings: Vec<String>,
    /// Kept as `Value` so `cost` is a required key even when explicitly null.
    cost: Value,
}

fn runtime_status_for_thread_status(
    status: ThreadStatus,
) -> Result<ryeos_runtime::envelope::RuntimeResultStatus> {
    use ryeos_runtime::envelope::RuntimeResultStatus;

    match status {
        ThreadStatus::Completed => Ok(RuntimeResultStatus::Completed),
        ThreadStatus::Failed => Ok(RuntimeResultStatus::Failed),
        ThreadStatus::Cancelled => Ok(RuntimeResultStatus::Cancelled),
        ThreadStatus::Killed => Ok(RuntimeResultStatus::Killed),
        ThreadStatus::TimedOut => Ok(RuntimeResultStatus::TimedOut),
        ThreadStatus::Continued => Ok(RuntimeResultStatus::Continued),
        ThreadStatus::Created | ThreadStatus::Running => {
            bail!("managed runtime envelope requires a terminal thread status")
        }
    }
}

fn validate_managed_terminal_envelope(
    raw: &Value,
    thread_id: &str,
    status: ThreadStatus,
    result: Option<&Value>,
    error: Option<&Value>,
    final_cost: Option<&ryeos_engine::contracts::FinalCost>,
) -> Result<()> {
    validate_checkpoint_shape(raw, "managed runtime terminal envelope")
        .context("validate managed runtime terminal envelope")?;
    let envelope: ManagedTerminalEnvelope =
        serde_json::from_value(raw.clone()).context("decode managed runtime terminal envelope")?;
    if envelope.child_thread_id != thread_id {
        bail!(
            "managed runtime envelope child_thread_id `{}` contradicts settlement thread `{thread_id}`",
            envelope.child_thread_id
        );
    }
    let expected_runtime_status = runtime_status_for_thread_status(status)?;
    if envelope.status != expected_runtime_status {
        bail!(
            "managed runtime envelope status `{}` contradicts settlement status `{}`",
            envelope.status.as_str(),
            status
        );
    }
    if envelope.success != expected_runtime_status.is_success() {
        bail!(
            "managed runtime envelope success contradicts settlement status `{}`",
            status
        );
    }
    if status == ThreadStatus::Completed && error.is_some() {
        bail!("completed settlement must not carry a terminal error");
    }
    if status == ThreadStatus::Continued && error.is_some() {
        bail!("continued settlement must not carry a terminal error");
    }
    let expected_payload = result.or(error).cloned().unwrap_or(Value::Null);
    if envelope.result != expected_payload {
        bail!("managed runtime envelope result contradicts settlement result/error payload");
    }

    let envelope_cost = if envelope.cost.is_null() {
        None
    } else {
        let cost: ryeos_runtime::envelope::RuntimeCost = serde_json::from_value(envelope.cost)
            .context("decode managed runtime envelope cost")?;
        cost.validate()
            .context("validate managed runtime envelope cost")?;
        Some(cost)
    };
    match (final_cost, envelope_cost.as_ref()) {
        (None, None) => {}
        (Some(final_cost), Some(runtime_cost))
            if final_cost.input_tokens == runtime_cost.input_tokens
                && final_cost.output_tokens == runtime_cost.output_tokens
                && final_cost.spend == runtime_cost.total_usd
                && final_cost.basis == runtime_cost.basis => {}
        (Some(_), Some(_)) => {
            bail!("managed runtime envelope cost contradicts settlement final cost")
        }
        (Some(_), None) | (None, Some(_)) => {
            bail!("managed runtime envelope cost presence contradicts settlement final cost")
        }
    }

    // Deserializing these required fields is itself the contract check. Keep the
    // reads explicit so future removal does not accidentally make them optional.
    let _ = (&envelope.outputs, &envelope.warnings);
    Ok(())
}

fn validate_final_cost_for_settlement(cost: &ryeos_engine::contracts::FinalCost) -> Result<()> {
    if !cost.spend.is_finite() {
        bail!("final cost spend must be finite");
    }
    if cost.spend < 0.0 {
        bail!("final cost spend must be non-negative");
    }
    if cost.input_tokens > i64::MAX as u64 {
        bail!("final cost input_tokens exceeds the settlement storage maximum");
    }
    if cost.output_tokens > i64::MAX as u64 {
        bail!("final cost output_tokens exceeds the settlement storage maximum");
    }
    match cost.basis.as_deref() {
        None | Some(ryeos_engine::launch_envelope_types::COST_BASIS_ROLLUP) => {}
        Some(basis) => {
            bail!("final cost basis `{basis}` is invalid; expected `rollup` or null");
        }
    }
    Ok(())
}

fn terminal_facets(
    final_cost: Option<&ryeos_engine::contracts::FinalCost>,
    managed_envelope: Option<&Value>,
) -> Result<BTreeMap<String, String>> {
    let mut facets = BTreeMap::new();
    if let Some(cost) = final_cost {
        facets.insert("cost.turns".to_string(), cost.turns.to_string());
        facets.insert(
            "cost.input_tokens".to_string(),
            cost.input_tokens.to_string(),
        );
        facets.insert(
            "cost.output_tokens".to_string(),
            cost.output_tokens.to_string(),
        );
        facets.insert("cost.spend".to_string(), cost.spend.to_string());
        if let Some(provider) = cost.provider.as_ref() {
            facets.insert("cost.provider".to_string(), provider.clone());
        }
        if let Some(basis) = cost.basis.as_ref() {
            facets.insert("cost.basis".to_string(), basis.clone());
        }
        if let Some(metadata) = cost.metadata.as_ref() {
            facets.insert(
                "cost.metadata_json".to_string(),
                serde_json::to_string(metadata).context("encode final cost metadata")?,
            );
        }
    }
    if let Some(envelope) = managed_envelope {
        facets.insert(
            "runtime.terminal_envelope_json".to_string(),
            serde_json::to_string(envelope).context("encode managed runtime terminal envelope")?,
        );
    }
    Ok(facets)
}

const FOLLOW_ENVELOPE_LIMIT_CODE: &str = "follow_terminal_envelope_limit_exceeded";

fn follow_envelope_limit_failure(child_thread_id: &str, cost: Option<&Value>) -> Value {
    let status = ryeos_runtime::envelope::RuntimeResultStatus::Failed;
    json!({
        "success": false,
        "child_thread_id": child_thread_id,
        "status": status,
        "result": {
            "code": FOLLOW_ENVELOPE_LIMIT_CODE,
            "message": "follow child terminal envelope exceeded the bounded parent resume payload",
        },
        "outputs": Value::Null,
        "warnings": [FOLLOW_ENVELOPE_LIMIT_CODE],
        "cost": cost.cloned(),
    })
}

fn follow_envelope_limit_reservation() -> Value {
    let maximum_cost = json!({
        "input_tokens": i64::MAX as u64,
        "output_tokens": i64::MAX as u64,
        "total_usd": f64::MAX,
        "basis": ryeos_engine::launch_envelope_types::COST_BASIS_ROLLUP,
    });
    let maximum_thread_id = format!("T-{}", "x".repeat(126));
    follow_envelope_limit_failure(&maximum_thread_id, Some(&maximum_cost))
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ValidatedFinalCost {
    completed_turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    spend_usd: f64,
}

fn validate_final_cost(cost: &ryeos_engine::contracts::FinalCost) -> Result<ValidatedFinalCost> {
    validate_final_cost_for_settlement(cost)?;
    Ok(ValidatedFinalCost {
        completed_turns: cost.turns,
        input_tokens: cost.input_tokens,
        output_tokens: cost.output_tokens,
        spend_usd: cost.spend,
    })
}

fn validated_follow_candidate_cost(candidate: &Value) -> Result<Option<Value>> {
    let Some(raw_cost) = candidate.get("cost") else {
        return Ok(None);
    };
    if raw_cost.is_null() {
        return Ok(None);
    }
    let cost: ryeos_runtime::envelope::RuntimeCost =
        serde_json::from_value(raw_cost.clone()).context("decode follow terminal cost")?;
    cost.validate().context("validate follow terminal cost")?;
    Ok(Some(
        serde_json::to_value(cost).context("encode validated follow terminal cost")?,
    ))
}

fn validate_follow_reservation_shape(seed: &runtime_db::NewFollowWaiter) -> Result<()> {
    if let Some(authority) = &seed.child_project_authority {
        authority.validate()?;
    }
    let expected = usize::try_from(seed.expected_children)
        .context("follow expected_children does not fit usize")?;
    if expected == 0 {
        bail!("follow waiter {} expects no children", seed.follow_key);
    }
    if !seed.fanout && expected != 1 {
        bail!(
            "non-fanout waiter {} must expect exactly one child",
            seed.follow_key
        );
    }
    let limits = checkpoint_shape_limits();
    if expected > limits.max_container_elements {
        bail!(
            "follow waiter {} expects {expected} children; maximum is {}",
            seed.follow_key,
            limits.max_container_elements
        );
    }
    if !seed.fanout {
        validate_checkpoint_shape(
            &follow_envelope_limit_reservation(),
            "reserved follow parent resume payload",
        )
        .context("validate reserved follow parent resume payload")?;
        return Ok(());
    }

    let pending = follow_envelope_limit_reservation();
    let mut budget = follow_fanout_items_budget(expected)?;
    for _ in 0..expected {
        budget
            .append(&pending)
            .context("validate reserved follow fanout parent resume payload")?;
    }
    Ok(())
}

fn follow_fanout_items_budget(expected: usize) -> Result<RuntimeJsonArrayBudget> {
    let mut limits = checkpoint_shape_limits();
    // `"completed"` is the longest closed fanout status (11 serialized
    // bytes); reserve one comma per entry as well, with the leading `[` taking
    // the remaining byte in `1 + 12*n` (the closing `]` replaces the final
    // comma reservation).
    let status_bytes = 1usize
        .checked_add(
            12usize
                .checked_mul(expected)
                .context("follow status JSON byte count overflow")?,
        )
        .context("follow status JSON byte count overflow")?;
    let fixed_payload = json!({
        "fanout": true,
        "items": [],
        "statuses": [],
        "failed": expected,
        "expected": expected,
    });
    let fixed_bytes = serde_json::to_vec(&fixed_payload)
        .context("encode follow fanout fixed payload")?
        .len()
        .checked_sub(4)
        .expect("two empty JSON arrays contain four bytes");
    limits.max_result_bytes = limits
        .max_result_bytes
        .checked_sub(fixed_bytes)
        .and_then(|remaining| remaining.checked_sub(status_bytes))
        .context("follow fanout fixed payload exceeds runtime JSON byte limit")?;
    // Final nodes are the item-array nodes plus one status scalar per child,
    // the status array, root object, fanout boolean, failed count, and expected
    // count. The incremental budget owns the item array and its children.
    limits.max_result_nodes = limits
        .max_result_nodes
        .checked_sub(expected)
        .and_then(|remaining| remaining.checked_sub(5))
        .context("follow fanout fixed payload exceeds runtime JSON node limit")?;
    // The item budget treats its array as depth one; in the final payload that
    // array is nested under the root object and therefore starts at depth two.
    limits.max_result_depth = limits
        .max_result_depth
        .checked_sub(1)
        .context("follow fanout root exceeds runtime JSON depth limit")?;
    Ok(RuntimeJsonArrayBudget::with_limits(
        "follow fanout terminal-envelope cohort",
        limits,
    ))
}

fn validate_prospective_follow_resume_payload(
    waiter: &runtime_db::FollowWaiter,
    child_chain_root_id: &str,
    candidate: &Value,
) -> Result<()> {
    let limits = checkpoint_shape_limits();
    let expected = usize::try_from(waiter.expected_children)
        .context("follow expected_children does not fit usize")?;
    if expected == 0 {
        bail!("follow waiter {} expects no children", waiter.follow_key);
    }
    if expected > limits.max_container_elements {
        bail!(
            "follow waiter {} expects {expected} children; maximum is {}",
            waiter.follow_key,
            limits.max_container_elements
        );
    }
    if !waiter.fanout && expected != 1 {
        bail!(
            "non-fanout waiter {} must expect exactly one child",
            waiter.follow_key
        );
    }

    let mut children = HashMap::with_capacity(waiter.children.len());
    for child in &waiter.children {
        if children.insert(child.item_index, child).is_some() {
            bail!(
                "follow waiter {} has duplicate child index {}",
                waiter.follow_key,
                child.item_index
            );
        }
    }
    let pending = follow_envelope_limit_reservation();
    let mut found_candidate = false;
    let mut budget = waiter
        .fanout
        .then(|| follow_fanout_items_budget(expected))
        .transpose()?;
    for item_index in 0..waiter.expected_children {
        let envelope = match children.get(&item_index) {
            Some(child) if child.child_chain_root_id == child_chain_root_id => {
                found_candidate = true;
                candidate
            }
            Some(child) => child.terminal_envelope.as_ref().unwrap_or(&pending),
            None => &pending,
        };
        if let Some(budget) = budget.as_mut() {
            budget.append(envelope)?;
        } else {
            validate_checkpoint_shape(envelope, "follow parent resume payload")?;
        }
    }
    if !found_candidate {
        bail!(
            "follow waiter {} does not contain child chain {child_chain_root_id}",
            waiter.follow_key
        );
    }

    Ok(())
}

fn admit_follow_terminal_envelope(
    waiter: &runtime_db::FollowWaiter,
    child_chain_root_id: &str,
    child_terminal_thread_id: &str,
    candidate: &Value,
) -> Result<(Value, bool)> {
    let decoded = ryeos_runtime::envelope::decode_follow_terminal_envelope(candidate)
        .map_err(anyhow::Error::msg)
        .context("validate canonical follow terminal envelope")?;
    if decoded.child_thread_id != child_terminal_thread_id {
        bail!(
            "follow terminal envelope child_thread_id `{}` does not match terminal child `{child_terminal_thread_id}`",
            decoded.child_thread_id
        );
    }
    match validate_prospective_follow_resume_payload(waiter, child_chain_root_id, candidate) {
        Ok(()) => Ok((candidate.clone(), false)),
        Err(candidate_error) => {
            let cost = validated_follow_candidate_cost(candidate)?;
            let degraded = follow_envelope_limit_failure(child_terminal_thread_id, cost.as_ref());
            validate_prospective_follow_resume_payload(waiter, child_chain_root_id, &degraded)
                .with_context(|| {
                    format!(
                        "follow terminal envelope exceeded bounds ({candidate_error}); bounded failure envelope also did not fit"
                    )
                })?;
            Ok((degraded, true))
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadArtifactRecord {
    pub artifact_id: i64,
    pub artifact_type: String,
    pub uri: String,
    pub content_hash: Option<String>,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadEdgeRecord {
    pub edge_id: i64,
    pub chain_root_id: String,
    pub source_thread_id: String,
    pub target_thread_id: String,
    pub edge_type: String,
    pub created_at: String,
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TerminalChainRetirement {
    pub candidate_chains: usize,
    pub retired_chains: usize,
    pub deleted_projection_rows: usize,
    pub deleted_runtime_rows: usize,
    pub deleted_runtime_files: usize,
    pub pending_retirements_recovered: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PendingHeadTransitionStatus {
    pub pending: usize,
    pub pending_sets: usize,
    pub pending_removes: usize,
    pub oldest_prepared_at: Option<String>,
    pub oldest_age_seconds: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ThreadListItem {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub status: String,
    pub item_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    /// The predecessor this thread continues, if any. Lets the client identify
    /// chain heads from authoritative edges instead of inferring them.
    pub upstream_thread_id: Option<String>,
    /// The continuation successor this thread handed off to, if any. A thread
    /// with no successor is the current chain head — the only place a follow-up
    /// can braid onto. Derived from the `thread_continued` event, terminal
    /// threads only (mirrors `ThreadDetail`).
    pub successor_thread_id: Option<String>,
    pub requested_by: Option<String>,
    pub project_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_authority: Option<ryeos_state::objects::ExecutionProjectAuthority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_authority: Option<ryeos_state::objects::ExecutionLifecycleAuthority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted_launch_capsule_hash: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// One durable row in an execution-tree closure. The embedded list item keeps
/// the same bounded enrichment path as the thread dashboard; the extra fields
/// are structural facts consumed by a hierarchy-aware view.
#[derive(Debug)]
pub struct ExecutionTreeItem {
    pub item: ThreadListItem,
    pub tree_parent_thread_id: Option<String>,
    pub relation: String,
    pub depth: usize,
    pub has_children: bool,
}

#[derive(Debug)]
pub struct ExecutionTreePage {
    pub items: Vec<ExecutionTreeItem>,
    pub truncated: bool,
}

/// Auxiliary facts for one thread-list page, loaded under one store lock and
/// grouped in memory. Keeps the UI list path from reacquiring the global store
/// mutex and rerunning projection queries for every row.
#[derive(Debug, Default)]
pub struct ThreadListEnrichment {
    pub follow_waiters: Vec<runtime_db::FollowWaiterSummary>,
    pub facets: HashMap<String, Vec<(String, String)>>,
    pub current_graph_nodes: HashMap<String, (String, u32)>,
    pub terminal_error_previews: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct FollowParentListSnapshot {
    pub waiters: Vec<runtime_db::FollowWaiterSummary>,
    pub parents: Vec<ThreadListItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadResultRecord {
    pub outcome_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub metadata: Option<Value>,
}

/// CAS-authoritative terminal fields used to reconcile a runtime's process
/// result with an earlier callback finalization. This deliberately reads the
/// signed thread snapshot rather than treating subprocess stdout as a second
/// terminal authority.
#[derive(Debug, Clone)]
pub struct ThreadTerminalAuthority {
    pub status: ryeos_state::objects::ThreadStatus,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub final_cost: Option<ryeos_engine::contracts::FinalCost>,
    pub managed_envelope: Option<Value>,
}

#[derive(Debug, Serialize)]
pub struct ThreadDetail {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub status: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_thread_id: Option<String>,
    /// The continuation successor this thread handed off to, if any. Exposed for
    /// every settled status, not only `continued`: an operator follow-up
    /// preserves a `completed`/`failed` predecessor's status, so those expose a
    /// successor too. Derived from the `thread_continued` event; lets a graph
    /// reconciler / client follow a continuation without scraping event
    /// payloads. `None` for a thread that has not been continued.
    pub successor_thread_id: Option<String>,
    pub requested_by: Option<String>,
    pub project_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_authority: Option<ryeos_state::objects::ExecutionProjectAuthority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle_authority: Option<ryeos_state::objects::ExecutionLifecycleAuthority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted_launch_capsule_hash: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub runtime: RuntimeInfo,
}

#[derive(Debug)]
pub(crate) struct CreatedThreadPublication {
    pub(crate) persisted: Vec<PersistedEventRecord>,
    pub(crate) successor: ThreadDetail,
}

/// Result of an idempotent operator continuation create-or-get.
#[derive(Debug)]
pub enum ContinuationOutcome {
    /// A new successor was created with this request's fingerprint persisted on
    /// its edge. The caller should launch it. Carries the persisted events.
    Created(Vec<PersistedEventRecord>),
    /// The source already has a successor whose recorded fingerprint MATCHES this
    /// request — a duplicate submit. The caller returns this id WITHOUT
    /// relaunching (the existing successor is already launching or done).
    Existing { successor_thread_id: String },
    /// The source is already continued by a request with a DIFFERENT fingerprint.
    Conflict { successor_thread_id: String },
}

/// Truthful result of the atomic portable child-lineage append. `Appended`
/// means this call advanced the signed parent braid; `AlreadyPresent` means an
/// earlier drive already recorded the same parent/child edge; `ParentSettled`
/// means the parent can no longer author the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildLineageAppendOutcome {
    Appended,
    AlreadyPresent,
    ParentSettled,
}

pub struct ChildLineageAppend {
    pub outcome: ChildLineageAppendOutcome,
    pub persisted: Vec<PersistedEventRecord>,
}

/// Result of the atomic pre-launch cleanup transition used after child-lineage
/// admission fails. The store only finalizes when the row is still `created`,
/// has no attached process identity, and has no launch claim. Callers therefore
/// never turn a concurrently launching/running child terminal from a stale
/// status read.
#[derive(Debug)]
pub enum FinalizeCreatedUnattachedOutcome {
    Finalized {
        persisted: Vec<PersistedEventRecord>,
        effective: Box<FinalizeThreadRecord>,
    },
    AlreadyTerminal,
    NotCurrent {
        status: String,
        process_attached: bool,
        launch_claimed: bool,
    },
}

/// Result of an atomic finalize-if-live transition. The terminal check,
/// shutdown fence, durable-stop dominance, and terminal write all share one
/// StateStore lock, so a callback winner is observed as `AlreadyTerminal`
/// rather than surfacing an invalid-transition race.
#[derive(Debug)]
pub enum FinalizeIfNonterminalOutcome {
    Finalized {
        persisted: Vec<PersistedEventRecord>,
        effective: Box<FinalizeThreadRecord>,
    },
    AlreadyTerminal {
        status: String,
    },
    PreservedForShutdown,
}

/// Result of atomically admitting an execution-owner stop against shutdown and
/// lifecycle finalization.
#[derive(Debug)]
pub enum StopIfAdmissionOpenOutcome {
    Requested(Box<RuntimeInfo>),
    AlreadyTerminal,
    PreservedForFollow,
    PreservedForShutdown,
}

#[derive(Clone, Copy)]
enum ProcessAttachmentMode<'a> {
    Idempotent { launch_owner: Option<&'a str> },
    New { launch_owner: Option<&'a str> },
}

impl<'a> ProcessAttachmentMode<'a> {
    fn launch_owner(self) -> Option<&'a str> {
        match self {
            Self::Idempotent { launch_owner } | Self::New { launch_owner } => launch_owner,
        }
    }

    fn requires_empty(self) -> bool {
        matches!(self, Self::New { .. })
    }
}

struct Inner {
    state_db: StateDb,
    runtime_db: runtime_db::RuntimeDb,
    signer: Arc<dyn Signer>,
}

/// StateStore guard paired with Lillux's fork-sensitive descriptor lease.
///
/// Authoritative state operations may acquire per-chain flocks beneath the
/// store mutex. Retaining the shared lease for the entire mutex scope prevents
/// a direct attachment child from forking with any such transient lock in its
/// descriptor table. The order is always descriptor lease, then StateStore
/// mutex.
struct StateStoreGuard<'a> {
    inner: std::sync::MutexGuard<'a, Inner>,
    _fork_sensitive_descriptors: lillux::ForkSensitiveDescriptorLease,
}

impl std::ops::Deref for StateStoreGuard<'_> {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for StateStoreGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub struct StateStore {
    state_authority: ryeos_state::PinnedStateAuthority,
    thread_runtime_authority: Option<ThreadRuntimeAuthority>,
    inner: Mutex<Inner>,
    projection_health: Arc<ThreadProjectionHealth>,
    read_only: bool,
    allow_projection_rebuild: bool,
    /// Kept outside the state mutex so lock order is always write permit then
    /// StateStore mutex (never a mutex probe followed by permit acquisition).
    write_barrier: WriteBarrier,
    process_attachment_admission_open: AtomicBool,
    /// Exact durable owners whose executor guards are alive in this process.
    /// Persisted claims prove fencing identity; this registry separately proves
    /// that a current task still owns the post-exit settlement window.
    active_launch_owners: Mutex<HashSet<String>>,
    /// Process-local cancellation signals for durable, still-unbound launch
    /// admissions. Persisted planning state remains the authority; this map
    /// only stops work promptly after that state commits cancelled.
    launch_task_abort_handles: Mutex<HashMap<String, tokio::task::AbortHandle>>,
}

/// Enforces the global mutation order for every StateStore write: the
/// cross-process CAS/GC guard is acquired before the daemon write permit and
/// both remain held until the operation has released its store/chain locks.
struct StateMutationPermit {
    cas_guard: ryeos_state::CasMutationGuard,
    _write_permit: WritePermit,
    _fork_sensitive_descriptors: lillux::ForkSensitiveDescriptorLease,
}

impl StateMutationPermit {
    fn cas_guard(&self) -> &ryeos_state::CasMutationGuard {
        &self.cas_guard
    }
}

fn attach_admitted_launch_capsule(
    state_authority: &ryeos_state::PinnedStateAuthority,
    cas_guard: &ryeos_state::CasMutationGuard,
    mut snapshot: ThreadSnapshot,
    launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
) -> Result<ThreadSnapshot> {
    let Some(capsule) = launch_metadata
        .map(crate::launch_metadata::RuntimeLaunchMetadata::admitted_launch_capsule)
        .transpose()?
        .flatten()
    else {
        return Ok(snapshot);
    };
    if capsule.project_authority != snapshot.project_authority {
        bail!(
            "admitted launch capsule project authority contradicts thread {} birth authority",
            snapshot.thread_id
        );
    }
    if capsule.executor_ref != snapshot.executor_ref || capsule.runtime_ref.is_empty() {
        bail!(
            "admitted launch capsule runtime identity contradicts thread {} birth identity",
            snapshot.thread_id
        );
    }
    state_authority.ensure_guard(cas_guard)?;
    let expected_hash = capsule.content_hash()?;
    let stored_hash = state_authority
        .cas_store()?
        .store_object(&capsule.to_value())
        .context("store admitted launch capsule before thread birth")?;
    if stored_hash != expected_hash {
        bail!(
            "admitted launch capsule hash mismatch: expected {expected_hash}, stored {stored_hash}"
        );
    }
    snapshot.admitted_launch_capsule_hash = Some(stored_hash);
    Ok(snapshot)
}

fn load_admitted_launch_capsule(
    state_authority: &ryeos_state::PinnedStateAuthority,
    capsule_hash: &str,
) -> Result<ryeos_state::objects::AdmittedLaunchCapsule> {
    let value = state_authority
        .cas_store()?
        .get_object(capsule_hash)?
        .ok_or_else(|| anyhow!("admitted launch capsule is missing from CAS: {capsule_hash}"))?;
    let capsule = ryeos_state::objects::AdmittedLaunchCapsule::from_current_value(value)
        .context("decode supported admitted launch capsule")?;
    if capsule.content_hash()? != capsule_hash {
        bail!("admitted launch capsule object hash is not canonical: {capsule_hash}");
    }
    Ok(capsule)
}

fn attach_continuation_launch_capsule(
    state_authority: &ryeos_state::PinnedStateAuthority,
    inner: &Inner,
    chain_root_id: &str,
    source_thread_id: &str,
    snapshot: ThreadSnapshot,
    launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
) -> Result<ThreadSnapshot> {
    let source_snapshot =
        authoritative_snapshot_for_transition(inner, chain_root_id, source_thread_id)?;
    let source_capsule = source_snapshot
        .admitted_launch_capsule_hash
        .as_deref()
        .map(|hash| load_admitted_launch_capsule(state_authority, hash))
        .transpose()?;
    let successor_capsule = launch_metadata
        .map(crate::launch_metadata::RuntimeLaunchMetadata::admitted_launch_capsule)
        .transpose()?
        .flatten();
    match (&source_capsule, &successor_capsule) {
        (None, None) => {}
        (Some(_), None) => bail!(
            "continuation successor {} dropped source {} admitted program authority",
            snapshot.thread_id,
            source_thread_id
        ),
        (None, Some(_)) => bail!(
            "continuation successor {} introduced admitted program authority absent from source {}",
            snapshot.thread_id,
            source_thread_id
        ),
        (Some(source), Some(successor)) if !source.same_continuation_admission(successor)? => {
            bail!(
                "continuation successor {} changed immutable admitted launch capsule from source {}",
                snapshot.thread_id,
                source_thread_id
            );
        }
        (Some(_), Some(_)) => {}
    }
    let mut snapshot = snapshot;
    snapshot.admitted_launch_capsule_hash = source_snapshot.admitted_launch_capsule_hash;
    Ok(snapshot)
}

fn verify_admitted_launch_capsule(
    state_authority: &ryeos_state::PinnedStateAuthority,
    snapshot: &ThreadSnapshot,
    launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
) -> Result<()> {
    let expected = launch_metadata
        .map(crate::launch_metadata::RuntimeLaunchMetadata::admitted_launch_capsule)
        .transpose()?
        .flatten();
    match (snapshot.admitted_launch_capsule_hash.as_deref(), expected) {
        (None, None) => Ok(()),
        (Some(_), None) => bail!(
            "thread {} has an admitted launch capsule but no matching runtime launch identity",
            snapshot.thread_id
        ),
        (None, Some(_)) => bail!(
            "thread {} managed launch is missing its authoritative admitted capsule",
            snapshot.thread_id
        ),
        (Some(rooted_hash), Some(expected)) => {
            let expected_hash = expected.content_hash()?;
            let rooted = load_admitted_launch_capsule(state_authority, rooted_hash)?;
            if snapshot.upstream_thread_id.is_none()
                && rooted.schema == expected.schema
                && rooted_hash != expected_hash
            {
                bail!(
                    "thread {} admitted capsule drift: authoritative {}, attempted {}",
                    snapshot.thread_id,
                    rooted_hash,
                    expected_hash
                );
            }
            if !rooted.same_continuation_admission(&expected)? {
                bail!(
                    "thread {} launch attempt changed immutable admitted capsule authority",
                    snapshot.thread_id
                );
            }
            Ok(())
        }
    }
}

impl std::fmt::Debug for StateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateStore")
            .field("inner", &"<Mutex<Inner>>")
            .finish()
    }
}

fn thread_detail_from_created_snapshot(
    snapshot: ThreadSnapshot,
    runtime: RuntimeInfo,
) -> ThreadDetail {
    let lifecycle_authority = runtime
        .launch_metadata
        .as_ref()
        .and_then(|metadata| metadata.resume_context.as_ref())
        .map(|resume| resume.lifecycle_authority);
    ThreadDetail {
        thread_id: snapshot.thread_id,
        chain_root_id: snapshot.chain_root_id,
        kind: snapshot.kind_name,
        status: snapshot.status.as_str().to_string(),
        item_ref: snapshot.item_ref,
        executor_ref: snapshot.executor_ref,
        launch_mode: snapshot.launch_mode,
        current_site_id: snapshot.current_site_id,
        origin_site_id: snapshot.origin_site_id,
        upstream_thread_id: snapshot.upstream_thread_id,
        successor_thread_id: None,
        requested_by: snapshot.requested_by,
        project_root: snapshot
            .project_root
            .map(|path| path.to_string_lossy().into_owned()),
        project_authority: Some(snapshot.project_authority),
        lifecycle_authority,
        admitted_launch_capsule_hash: snapshot.admitted_launch_capsule_hash,
        created_at: snapshot.created_at,
        updated_at: snapshot.updated_at,
        started_at: snapshot.started_at,
        finished_at: snapshot.finished_at,
        runtime,
    }
}

fn build_snapshot(thread: &NewThreadRecord) -> ThreadSnapshot {
    let now = lillux::time::iso8601_now();
    ThreadSnapshot {
        schema: ryeos_state::objects::THREAD_SNAPSHOT_SCHEMA_VERSION,
        kind: "thread_snapshot".to_string(),
        thread_id: thread.thread_id.clone(),
        chain_root_id: thread.chain_root_id.clone(),
        status: ThreadStatus::Created,
        kind_name: thread.kind.clone(),
        item_ref: thread.item_ref.clone(),
        executor_ref: thread.executor_ref.clone(),
        launch_mode: thread.launch_mode.clone(),
        current_site_id: thread.current_site_id.clone(),
        origin_site_id: thread.origin_site_id.clone(),
        upstream_thread_id: thread.upstream_thread_id.clone(),
        requested_by: thread.requested_by.clone(),
        project_root: thread.project_root.clone(),
        project_authority: thread.project_authority.clone(),
        admitted_launch_capsule_hash: None,
        base_project_snapshot_hash: thread.base_project_snapshot_hash.clone(),
        result_project_snapshot_hash: None,
        created_at: now.clone(),
        updated_at: now,
        started_at: None,
        finished_at: None,
        result: None,
        outcome_code: None,
        error: None,
        budget: None,
        artifacts: vec![],
        captured_history_policy: thread.captured_history_policy.clone(),
        facets: Default::default(),
        last_event_hash: None,
        last_chain_seq: 0,
        last_thread_seq: 0,
    }
}

fn build_continuation_snapshot(
    thread: &NewThreadRecord,
    resume: &crate::launch_metadata::ResumeContext,
) -> Result<ThreadSnapshot> {
    let (project_root, base_project_snapshot_hash) = resume
        .authoritative_project_identity()
        .context("derive continuation successor project identity")?;
    if thread.project_root.as_deref() != project_root.as_deref() {
        bail!(
            "continuation successor project root {:?} contradicts captured launch root {:?}",
            thread.project_root,
            project_root
        );
    }
    if thread.base_project_snapshot_hash.as_deref() != base_project_snapshot_hash.as_deref() {
        bail!(
            "continuation successor base snapshot {:?} contradicts captured launch snapshot {:?}",
            thread.base_project_snapshot_hash,
            base_project_snapshot_hash
        );
    }
    if thread.project_authority != resume.project_authority {
        bail!("continuation successor project authority contradicts captured launch authority");
    }
    let mut snapshot = build_snapshot(thread);
    snapshot.project_root = project_root;
    snapshot.base_project_snapshot_hash = base_project_snapshot_hash;
    snapshot.project_authority = resume.project_authority.clone();
    Ok(snapshot)
}

fn authoritative_snapshot_for_transition(
    inner: &Inner,
    chain_root_id: &str,
    thread_id: &str,
) -> Result<ThreadSnapshot> {
    inner
        .state_db
        .read_authoritative_thread_snapshot(chain_root_id, thread_id)?
        .ok_or_else(|| {
            anyhow!(
                "authoritative snapshot missing for projected thread {thread_id} in chain {chain_root_id}"
            )
        })
}

fn continued_snapshot_for_transition(
    inner: &Inner,
    thread: &queries::ThreadRow,
    now: &str,
) -> Result<ThreadSnapshot> {
    let mut snapshot =
        authoritative_snapshot_for_transition(inner, &thread.chain_root_id, &thread.thread_id)?;
    snapshot.status = ThreadStatus::Continued;
    snapshot.updated_at = now.to_string();
    snapshot.finished_at = Some(now.to_string());
    snapshot.result = None;
    snapshot.outcome_code = Some("continued".to_string());
    snapshot.error = None;
    snapshot.budget = None;
    snapshot.artifacts.clear();
    snapshot.facets.clear();
    Ok(snapshot)
}

fn convert_events(
    events: &[NewEventRecord],
    chain_root_id: &str,
    thread_id: &str,
) -> Vec<ryeos_state::objects::ThreadEvent> {
    let now = lillux::time::iso8601_now();
    events
        .iter()
        .enumerate()
        .map(|(idx, event)| ryeos_state::objects::ThreadEvent {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_event".to_string(),
            chain_root_id: chain_root_id.to_string(),
            chain_seq: 0,
            thread_id: thread_id.to_string(),
            thread_seq: (idx + 1) as u64,
            event_type: event.event_type.clone(),
            durability: match event.storage_class.as_str() {
                "indexed" => ryeos_state::objects::EventDurability::Durable,
                "journal" | "journal_only" => ryeos_state::objects::EventDurability::Journal,
                "ephemeral" => ryeos_state::objects::EventDurability::Ephemeral,
                _ => ryeos_state::objects::EventDurability::Durable,
            },
            ts: now.clone(),
            prev_chain_event_hash: None,
            prev_thread_event_hash: None,
            payload: event.payload.clone(),
        })
        .collect()
}

fn persisted_from_append(
    result: &ryeos_state::chain::AppendResult,
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    persisted_from_stored_events(&result.events, events)
}

fn persisted_from_add_thread_with_events(
    result: &ryeos_state::chain::AddThreadWithEventsResult,
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    persisted_from_stored_events(&result.events, events)
}

fn persisted_from_stored_events(
    stored_events: &[ryeos_state::objects::ThreadEvent],
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    stored_events
        .iter()
        .zip(events.iter())
        .map(|(stored, input)| {
            Ok(PersistedEventRecord {
                event_id: stored.chain_seq as i64,
                event_hash: Some(
                    ryeos_state::objects::thread_event::hash_event(stored)
                        .context("failed to canonicalize stored thread event")?,
                ),
                chain_root_id: stored.chain_root_id.clone(),
                chain_seq: stored.chain_seq as i64,
                thread_id: stored.thread_id.clone(),
                thread_seq: stored.thread_seq as i64,
                event_type: input.event_type.clone(),
                storage_class: input.storage_class.clone(),
                ts: stored.ts.clone(),
                prev_chain_event_hash: stored.prev_chain_event_hash.clone(),
                prev_thread_event_hash: stored.prev_thread_event_hash.clone(),
                payload: input.payload.clone(),
            })
        })
        .collect()
}

fn ephemeral_record(
    chain_root_id: &str,
    thread_id: &str,
    event: &NewEventRecord,
) -> PersistedEventRecord {
    PersistedEventRecord {
        event_id: 0,
        event_hash: None,
        chain_root_id: chain_root_id.to_string(),
        chain_seq: 0,
        thread_id: thread_id.to_string(),
        thread_seq: 0,
        event_type: event.event_type.clone(),
        storage_class: event.storage_class.clone(),
        ts: lillux::time::iso8601_now(),
        prev_chain_event_hash: None,
        prev_thread_event_hash: None,
        payload: event.payload.clone(),
    }
}

fn append_events_locked(
    g: &Inner,
    cas_mutation_guard: Option<&ryeos_state::CasMutationGuard>,
    chain_root_id: &str,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<Vec<PersistedEventRecord>> {
    validate_artifact_event_admission(g, thread_id, events)?;
    validate_facet_event_admission(g, thread_id, events)?;
    let mut records: Vec<Option<PersistedEventRecord>> = vec![None; events.len()];
    let mut durable_events = Vec::new();
    let mut durable_thread_events = Vec::new();
    let mut durable_indices = Vec::new();
    let converted_events = convert_events(events, chain_root_id, thread_id);

    for (idx, (event, thread_event)) in events.iter().zip(converted_events).enumerate() {
        // Validate before separating ephemeral records: ephemeral events bypass
        // CAS, but must observe the same complete-event byte ceiling as every
        // durable writer.
        thread_event.validate()?;
        if event.storage_class == "ephemeral" {
            records[idx] = Some(ephemeral_record(chain_root_id, thread_id, event));
        } else {
            durable_indices.push(idx);
            durable_events.push(event.clone());
            durable_thread_events.push(thread_event);
        }
    }

    if !durable_events.is_empty() {
        let cas_mutation_guard = cas_mutation_guard.ok_or_else(|| {
            anyhow!("durable event append requires an admitted CAS mutation guard")
        })?;
        let result = committed_value(g.state_db.append_events_admitted(
            chain_root_id,
            thread_id,
            durable_thread_events,
            vec![],
            g.signer.as_ref(),
            &g.runtime_db,
            cas_mutation_guard,
        )?);
        for (idx, record) in durable_indices
            .into_iter()
            .zip(persisted_from_append(&result, &durable_events)?)
        {
            records[idx] = Some(record);
        }
    }

    records
        .into_iter()
        .map(|record| record.ok_or_else(|| anyhow!("append event record missing")))
        .collect()
}

fn has_indexed_collection_events(events: &[NewEventRecord]) -> bool {
    events.iter().any(|event| {
        event.storage_class == "indexed"
            && matches!(
                event.event_type.as_str(),
                ryeos_state::event_types::ARTIFACT_PUBLISHED
                    | ryeos_state::event_types::THREAD_FACET_SET
            )
    })
}

const LAUNCH_ATTEMPT_AUDIT_TYPES: [ryeos_runtime::RuntimeEventType; 3] = [
    ryeos_runtime::RuntimeEventType::AsLaunchedResolution,
    ryeos_runtime::RuntimeEventType::AsLaunchedRefBindings,
    ryeos_runtime::RuntimeEventType::RuntimeLaunchFacts,
];

fn validate_launch_attempt_audit(events: &[NewEventRecord]) -> Result<()> {
    if events.len() < LAUNCH_ATTEMPT_AUDIT_TYPES.len() {
        bail!(
            "launch attempt audit must contain the {} required authority events, received {}",
            LAUNCH_ATTEMPT_AUDIT_TYPES.len(),
            events.len()
        );
    }
    for (index, (event, expected)) in events
        .iter()
        .take(LAUNCH_ATTEMPT_AUDIT_TYPES.len())
        .zip(LAUNCH_ATTEMPT_AUDIT_TYPES)
        .enumerate()
    {
        if event.event_type != expected.as_str() {
            bail!(
                "launch attempt audit event {index} must be '{}', received '{}'",
                expected.as_str(),
                event.event_type
            );
        }
        let expected_storage = expected.storage_class().as_str();
        if event.storage_class != expected_storage {
            bail!(
                "launch attempt audit event '{}' must use canonical storage class '{}', received '{}'",
                event.event_type,
                expected_storage,
                event.storage_class
            );
        }
    }
    for (index, event) in events
        .iter()
        .enumerate()
        .skip(LAUNCH_ATTEMPT_AUDIT_TYPES.len())
    {
        if event.event_type != ryeos_runtime::RuntimeEventType::LaunchAugmentationCacheHit.as_str()
        {
            bail!(
                "launch attempt audit event {index} must be a canonical launch augmentation audit, received '{}'",
                event.event_type
            );
        }
    }
    Ok(())
}

fn validate_artifact_shape(artifact_type: &str, metadata: Option<&Value>) -> Result<usize> {
    if artifact_type.is_empty() || artifact_type.len() > MAX_THREAD_ARTIFACT_TYPE_BYTES {
        bail!("artifact_type must be 1..={MAX_THREAD_ARTIFACT_TYPE_BYTES} UTF-8 bytes");
    }
    let metadata_bytes = metadata
        .map(serde_json::to_vec)
        .transpose()?
        .map_or(0, |bytes| bytes.len());
    if metadata_bytes > MAX_THREAD_ARTIFACT_METADATA_BYTES {
        bail!(
            "artifact metadata is {metadata_bytes} bytes; maximum is {MAX_THREAD_ARTIFACT_METADATA_BYTES}"
        );
    }
    Ok(metadata_bytes)
}

fn validate_new_artifact_shape(artifact_type: &str, metadata: Option<&Value>) -> Result<usize> {
    let null_metadata = Value::Null;
    validate_artifact_shape(artifact_type, Some(metadata.unwrap_or(&null_metadata)))
}

fn ensure_artifact_projection_capacity(
    g: &Inner,
    thread_id: &str,
    additional_items: usize,
    additional_kind_bytes: usize,
    additional_metadata_bytes: usize,
) -> Result<()> {
    let (current_items, current_kind_bytes, current_metadata_bytes) =
        queries::thread_artifact_stats(g.state_db.projection(), thread_id)?;
    let final_items = current_items
        .checked_add(additional_items)
        .ok_or_else(|| anyhow!("thread artifact count overflow"))?;
    let final_metadata_bytes = current_metadata_bytes
        .checked_add(additional_metadata_bytes)
        .ok_or_else(|| anyhow!("thread artifact byte total overflow"))?;
    let final_kind_bytes = current_kind_bytes
        .checked_add(additional_kind_bytes)
        .ok_or_else(|| anyhow!("thread artifact kind byte total overflow"))?;
    if final_items > MAX_THREAD_ARTIFACT_ITEMS {
        bail!(
            "thread {thread_id} would have {final_items} artifacts; maximum is {MAX_THREAD_ARTIFACT_ITEMS}"
        );
    }
    if final_metadata_bytes > MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES {
        bail!(
            "thread {thread_id} artifact metadata would total {final_metadata_bytes} bytes; maximum is {MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES}"
        );
    }
    // JSON escaping can expand an arbitrary UTF-8 kind by at most six bytes
    // per source byte. Metadata is already stored as serialized JSON. Include
    // conservative fixed record overhead so every newly admitted collection is
    // guaranteed to fit the same response ceiling enforced by readers.
    let fixed_record_bytes = final_items
        .checked_mul(160)
        .ok_or_else(|| anyhow!("thread artifact response estimate overflow"))?;
    let estimated_response_bytes = final_kind_bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(final_metadata_bytes))
        .and_then(|bytes| bytes.checked_add(fixed_record_bytes))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("thread artifact response estimate overflow"))?;
    if estimated_response_bytes > MAX_THREAD_ARTIFACT_RESPONSE_BYTES {
        bail!(
            "thread {thread_id} artifacts would exceed the {MAX_THREAD_ARTIFACT_RESPONSE_BYTES}-byte response maximum"
        );
    }
    Ok(())
}

fn validate_artifact_event_admission(
    g: &Inner,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<()> {
    let mut additional_items = 0usize;
    let mut additional_kind_bytes = 0usize;
    let mut additional_metadata_bytes = 0usize;
    for event in events.iter().filter(|event| {
        event.storage_class == "indexed"
            && event.event_type == ryeos_state::event_types::ARTIFACT_PUBLISHED
    }) {
        let artifact_type = event
            .payload
            .get("artifact_type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("artifact_published requires string artifact_type"))?;
        let metadata_bytes = validate_artifact_shape(artifact_type, event.payload.get("metadata"))?;
        additional_items = additional_items
            .checked_add(1)
            .ok_or_else(|| anyhow!("artifact batch count overflow"))?;
        additional_kind_bytes = additional_kind_bytes
            .checked_add(artifact_type.len())
            .ok_or_else(|| anyhow!("artifact batch kind byte total overflow"))?;
        additional_metadata_bytes = additional_metadata_bytes
            .checked_add(metadata_bytes)
            .ok_or_else(|| anyhow!("artifact batch byte total overflow"))?;
    }
    if additional_items > 0 {
        ensure_artifact_projection_capacity(
            g,
            thread_id,
            additional_items,
            additional_kind_bytes,
            additional_metadata_bytes,
        )?;
    }
    Ok(())
}

fn validate_facet_event_admission(
    g: &Inner,
    thread_id: &str,
    events: &[NewEventRecord],
) -> Result<()> {
    let mut updates = HashMap::<String, usize>::new();
    for event in events.iter().filter(|event| {
        event.storage_class == "indexed"
            && event.event_type == ryeos_state::event_types::THREAD_FACET_SET
    }) {
        let key = event
            .payload
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("thread_facet_set requires string key"))?;
        let value = event
            .payload
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("thread_facet_set requires string value"))?;
        if key.is_empty() || key.len() > MAX_THREAD_FACET_KEY_BYTES {
            bail!("facet key must be 1..={MAX_THREAD_FACET_KEY_BYTES} UTF-8 bytes");
        }
        if value.len() > MAX_THREAD_FACET_VALUE_BYTES {
            bail!(
                "facet value is {} bytes; maximum is {MAX_THREAD_FACET_VALUE_BYTES}",
                value.len()
            );
        }
        // Multiple updates to one key in a batch project sequentially; only the
        // final value contributes to the durable facet collection.
        updates.insert(key.to_string(), value.len());
    }
    if updates.is_empty() {
        return Ok(());
    }

    let (mut final_items, mut final_content_bytes) =
        queries::thread_facet_stats(g.state_db.projection(), thread_id)?;
    for (key, value_bytes) in updates {
        match queries::thread_facet_value_bytes(g.state_db.projection(), thread_id, &key)? {
            Some(previous_value_bytes) => {
                final_content_bytes = final_content_bytes
                    .checked_sub(previous_value_bytes)
                    .and_then(|bytes| bytes.checked_add(value_bytes))
                    .ok_or_else(|| anyhow!("thread facet byte total overflow"))?;
            }
            None => {
                final_items = final_items
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("thread facet count overflow"))?;
                final_content_bytes = final_content_bytes
                    .checked_add(key.len())
                    .and_then(|bytes| bytes.checked_add(value_bytes))
                    .ok_or_else(|| anyhow!("thread facet byte total overflow"))?;
            }
        }
    }
    ensure_facet_collection_bounds(thread_id, final_items, final_content_bytes)
}

fn ensure_facet_collection_bounds(
    thread_id: &str,
    final_items: usize,
    final_content_bytes: usize,
) -> Result<()> {
    if final_items > MAX_THREAD_FACET_ITEMS {
        bail!(
            "thread {thread_id} would have {final_items} facets; maximum is {MAX_THREAD_FACET_ITEMS}"
        );
    }
    if final_content_bytes > MAX_THREAD_FACET_CONTENT_BYTES {
        bail!(
            "thread {thread_id} facet content would total {final_content_bytes} bytes; maximum is {MAX_THREAD_FACET_CONTENT_BYTES}"
        );
    }
    let fixed_entry_bytes = final_items
        .checked_mul(8)
        .ok_or_else(|| anyhow!("thread facet response estimate overflow"))?;
    let estimated_response_bytes = final_content_bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(fixed_entry_bytes))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("thread facet response estimate overflow"))?;
    if estimated_response_bytes > MAX_THREAD_FACET_RESPONSE_BYTES {
        bail!(
            "thread {thread_id} facets would exceed the {MAX_THREAD_FACET_RESPONSE_BYTES}-byte response maximum"
        );
    }
    Ok(())
}

fn load_bounded_facets_many(g: &Inner, thread_ids: &[String]) -> Result<Vec<queries::FacetRow>> {
    let mut seen = HashSet::new();
    let thread_ids = thread_ids
        .iter()
        .filter(|thread_id| seen.insert(thread_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if thread_ids.len() > MAX_THREAD_LIST_ENRICHMENT_THREADS {
        bail!(
            "thread-list enrichment requested {} threads; maximum is {MAX_THREAD_LIST_ENRICHMENT_THREADS}",
            thread_ids.len()
        );
    }
    let stats = queries::thread_facet_stats_many(g.state_db.projection(), &thread_ids)?;
    let mut total_items = 0usize;
    let mut total_content_bytes = 0usize;
    for (thread_id, items, content_bytes) in stats {
        ensure_facet_collection_bounds(&thread_id, items, content_bytes)?;
        total_items = total_items
            .checked_add(items)
            .ok_or_else(|| anyhow!("thread-list facet count overflow"))?;
        total_content_bytes = total_content_bytes
            .checked_add(content_bytes)
            .ok_or_else(|| anyhow!("thread-list facet byte total overflow"))?;
    }
    if total_items > MAX_THREAD_LIST_FACET_ITEMS {
        bail!(
            "thread-list facets contain {total_items} entries; maximum is {MAX_THREAD_LIST_FACET_ITEMS}"
        );
    }
    if total_content_bytes > MAX_THREAD_LIST_FACET_CONTENT_BYTES {
        bail!(
            "thread-list facet content is {total_content_bytes} bytes; maximum is {MAX_THREAD_LIST_FACET_CONTENT_BYTES}"
        );
    }
    let fixed_entry_bytes = total_items
        .checked_mul(8)
        .ok_or_else(|| anyhow!("thread-list facet response estimate overflow"))?;
    let estimated_response_bytes = total_content_bytes
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(fixed_entry_bytes))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(|| anyhow!("thread-list facet response estimate overflow"))?;
    if estimated_response_bytes > MAX_THREAD_LIST_FACET_RESPONSE_BYTES {
        bail!(
            "thread-list facets would exceed the {MAX_THREAD_LIST_FACET_RESPONSE_BYTES}-byte response maximum"
        );
    }
    queries::get_facets_many_bounded(
        g.state_db.projection(),
        &thread_ids,
        MAX_THREAD_LIST_FACET_ITEMS,
        MAX_THREAD_FACET_KEY_BYTES,
        MAX_THREAD_FACET_VALUE_BYTES,
        MAX_THREAD_LIST_FACET_CONTENT_BYTES,
    )
}

/// Which kind of running-source continuation successor to create. Both kinds
/// cut a still-running source, seed the successor's resume context, and settle
/// the source `continued`; they differ only in the edge `reason` recorded and
/// in whether the autonomous chain-depth cap applies. The `GraphFollowResume`
/// marker is daemon-trusted (selectable only via the dedicated method, never a
/// caller-supplied reason).
enum RunningContinuationKind<'a> {
    Machine { sanitized_reason: Option<&'a str> },
    GraphFollowResume,
}

fn validate_thread_id_path_component(thread_id: &str) -> Result<()> {
    let mut components = Path::new(thread_id).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(component)), None)
            if !component.is_empty() && component == std::ffi::OsStr::new(thread_id) =>
        {
            Ok(())
        }
        _ => bail!("invalid authoritative thread ID for runtime cleanup: {thread_id}"),
    }
}

struct ThreadRuntimeRemoval {
    threads_root: lillux::PinnedDirectory,
    directory: lillux::PinnedDirectory,
    name: std::ffi::OsString,
}

struct ThreadRuntimeAuthority {
    app_root: lillux::PinnedDirectory,
    threads_root: Option<lillux::PinnedDirectory>,
}

impl ThreadRuntimeAuthority {
    fn capture(
        app_root: &Path,
        runtime_state: &lillux::PinnedDirectory,
        create_threads_root: bool,
    ) -> Result<Self> {
        let app_root = lillux::PinnedDirectory::open(app_root)?.ok_or_else(|| {
            anyhow!(
                "app runtime root is absent while capturing cleanup authority: {}",
                app_root.display()
            )
        })?;
        let ai_root = app_root
            .open_child_directory(std::ffi::OsStr::new(ryeos_engine::AI_DIR))?
            .ok_or_else(|| anyhow!("app root has no .ai directory"))?;
        let configured_runtime_state = ai_root
            .open_child_directory(std::ffi::OsStr::new("state"))?
            .ok_or_else(|| anyhow!("app root has no .ai/state directory"))?;
        if !configured_runtime_state.is_same_directory(runtime_state)? {
            bail!(
                "runtime state directory does not belong to captured app root: app_root={}, runtime_state={}",
                app_root.path().display(),
                runtime_state.path().display()
            );
        }
        let threads_name = std::ffi::OsStr::new("threads");
        let threads_root = if create_threads_root {
            Some(app_root.open_or_create_child(threads_name, 0o700)?)
        } else {
            app_root.open_child_directory(threads_name)?
        };
        if threads_root.is_none() && app_root.open_regular(threads_name, false)?.is_some() {
            bail!("thread state root is not a directory");
        }
        Ok(Self {
            app_root,
            threads_root,
        })
    }

    /// Confirm the public namespace still names the captured inodes. The
    /// reopened path is diagnostic only; every destructive operation below is
    /// rooted in the retained descriptors.
    fn ensure_current_binding(&self) -> Result<()> {
        let current_app =
            lillux::PinnedDirectory::open(self.app_root.path())?.ok_or_else(|| {
                anyhow!(
                    "captured app runtime root disappeared: {}",
                    self.app_root.path().display()
                )
            })?;
        if !self.app_root.is_same_directory(&current_app)? {
            bail!(
                "app runtime root changed after cleanup authority was captured: {}",
                self.app_root.path().display()
            );
        }

        let current_threads = self
            .app_root
            .open_child_directory(std::ffi::OsStr::new("threads"))?;
        match (&self.threads_root, current_threads) {
            (Some(expected), Some(current)) if expected.is_same_directory(&current)? => Ok(()),
            (None, None) => Ok(()),
            _ => bail!(
                "thread runtime root changed after cleanup authority was captured: {}",
                self.app_root.path().join("threads").display()
            ),
        }
    }
}

fn count_runtime_tree_entries(directory: &lillux::PinnedDirectory) -> Result<usize> {
    let mut count = 1usize;
    for name in directory.entry_names()? {
        let child_count = match directory.open_entry(&name, false)? {
            Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
                count_runtime_tree_entries(&child)?
            }
            Some(lillux::PinnedDirectoryEntry::Regular(_)) => 1,
            // An entry removed after enumeration is already absent. Links and
            // special files fail in the no-follow mixed-entry open above.
            None => continue,
        };
        count = count
            .checked_add(child_count)
            .ok_or_else(|| anyhow!("runtime file count overflow"))?;
    }
    Ok(count)
}

/// Resolve exactly the per-thread daemon state directories named by signed
/// chain truth beneath the startup-captured thread-root inode. This never
/// reopens a pathname as authority, never walks the global thread directory,
/// and rejects links or special entries.
fn inspect_thread_runtime_files(
    authority: &ThreadRuntimeAuthority,
    thread_ids: &[String],
) -> Result<Vec<ThreadRuntimeRemoval>> {
    authority.ensure_current_binding()?;
    let Some(threads_root) = authority.threads_root.as_ref() else {
        return Ok(Vec::new());
    };

    let mut paths = Vec::new();
    for thread_id in thread_ids {
        validate_thread_id_path_component(thread_id)?;
        let name = std::ffi::OsString::from(thread_id);
        let Some(directory) = threads_root.open_child_directory(&name)? else {
            if threads_root.open_regular(&name, false)?.is_some() {
                bail!("thread runtime cleanup target is not a directory: {thread_id}");
            }
            continue;
        };
        // Inspect the complete tree before the signed head is unlinked. This
        // fails closed on links/special entries while retaining exact handles
        // for the later post-boundary cleanup.
        let _entries = count_runtime_tree_entries(&directory)?;
        paths.push(ThreadRuntimeRemoval {
            threads_root: threads_root.try_clone()?,
            directory,
            name,
        });
    }
    Ok(paths)
}

fn delete_runtime_tree_contents(directory: &lillux::PinnedDirectory) -> Result<usize> {
    let mut deleted = 0usize;
    for name in directory.entry_names()? {
        match directory.open_entry(&name, false)? {
            Some(lillux::PinnedDirectoryEntry::Directory(child)) => {
                deleted = deleted
                    .checked_add(delete_runtime_tree_contents(&child)?)
                    .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
                if !directory.remove_empty_child_if_same(&name, &child)? {
                    bail!(
                        "thread runtime directory changed during cleanup: {}",
                        child.path().display()
                    );
                }
                deleted = deleted
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
            }
            Some(lillux::PinnedDirectoryEntry::Regular(file)) => {
                directory.remove_if_same(&name, &file)?;
                deleted = deleted
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
            }
            None => {}
        }
    }
    Ok(deleted)
}

fn delete_thread_runtime_files(paths: &[ThreadRuntimeRemoval]) -> Result<usize> {
    let mut deleted = 0usize;
    for target in paths {
        deleted = deleted
            .checked_add(delete_runtime_tree_contents(&target.directory)?)
            .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
        if !target
            .threads_root
            .remove_empty_child_if_same(&target.name, &target.directory)?
        {
            bail!(
                "thread runtime cleanup target changed during deletion: {}",
                target.directory.path().display()
            );
        }
        deleted = deleted
            .checked_add(1)
            .ok_or_else(|| anyhow!("deleted runtime file count overflow"))?;
    }
    Ok(deleted)
}

/// Inspect or clear the complete per-thread runtime directory for the explicit
/// offline all-history GC path. The `threads/` root itself remains as the
/// current empty runtime namespace.
pub(crate) fn discard_all_thread_runtime_files(
    app_root: &Path,
    runtime_state: &lillux::PinnedDirectory,
    dry_run: bool,
) -> Result<usize> {
    let authority = ThreadRuntimeAuthority::capture(app_root, runtime_state, false)?;
    authority.ensure_current_binding()?;
    let Some(threads_root) = authority.threads_root.as_ref() else {
        return Ok(0);
    };
    if dry_run {
        return count_runtime_tree_entries(threads_root).map(|count| count.saturating_sub(1));
    }
    delete_runtime_tree_contents(threads_root)
}

impl StateStore {
    fn thread_runtime_authority(&self) -> Result<&ThreadRuntimeAuthority> {
        self.thread_runtime_authority
            .as_ref()
            .ok_or_else(|| anyhow!("this StateStore has no destructive thread-runtime authority"))
    }

    /// Open production state with the trust authority used to verify every
    /// authoritative chain head during baseline rebuild and journal replay.
    pub fn new_with_head_trust(
        app_root: PathBuf,
        runtime_state_dir: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
    ) -> Result<Self> {
        Self::open(
            app_root,
            runtime_state_dir,
            runtime_db_path,
            signer,
            write_barrier,
            head_trust,
            None,
        )
    }

    /// Strict standalone verification store. Only established on-disk
    /// authoritative/projection state is opened; runtime scaffolding is
    /// in-memory and every mutation API is rejected at its common permit gate.
    pub fn new_for_projection_verification(
        runtime_state_dir: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
    ) -> Result<Self> {
        let projection_health = Arc::new(ThreadProjectionHealth::default());
        let state_db = StateDb::open_for_projection_verification(&runtime_state_dir, head_trust)?;
        let runtime_db = runtime_db::RuntimeDb::new_in_memory()?;
        projection_health.observe_pending_transitions(state_db.pending_chain_transitions()?.len());
        let state_authority = state_db.pinned_authority()?;
        Ok(Self {
            state_authority,
            thread_runtime_authority: None,
            inner: Mutex::new(Inner {
                state_db,
                runtime_db,
                signer,
            }),
            projection_health,
            read_only: true,
            allow_projection_rebuild: false,
            write_barrier,
            process_attachment_admission_open: AtomicBool::new(true),
            active_launch_owners: Mutex::new(HashSet::new()),
            launch_task_abort_handles: Mutex::new(HashMap::new()),
        })
    }

    /// Offline projection-rebuild control store. Opens the authoritative roots,
    /// durable recovery journal, and persisted RuntimeDb liveness state, but
    /// deliberately does not read a generation pointer, replay a transition,
    /// or build a baseline. The verified service invocation owns that one
    /// explicit rebuild; all unrelated mutation APIs remain fail-closed.
    pub fn new_for_projection_rebuild(
        app_root: PathBuf,
        runtime_state_dir: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
    ) -> Result<Self> {
        let projection_health = Arc::new(ThreadProjectionHealth::default());
        let runtime_state_authority = lillux::PinnedDirectory::open(&runtime_state_dir)?
            .ok_or_else(|| anyhow!("runtime state directory is absent"))?;
        let thread_runtime_authority =
            ThreadRuntimeAuthority::capture(&app_root, &runtime_state_authority, false)?;
        let runtime_db = runtime_db::RuntimeDb::open_existing_current(&runtime_db_path)?;
        let state_db = StateDb::open_for_projection_rebuild(&runtime_state_dir, head_trust)?;
        projection_health.observe_pending_transitions(state_db.pending_chain_transitions()?.len());
        let state_authority = state_db.pinned_authority()?;
        Ok(Self {
            state_authority,
            thread_runtime_authority: Some(thread_runtime_authority),
            inner: Mutex::new(Inner {
                state_db,
                runtime_db,
                signer,
            }),
            projection_health,
            read_only: true,
            allow_projection_rebuild: true,
            write_barrier,
            process_attachment_admission_open: AtomicBool::new(true),
            active_launch_owners: Mutex::new(HashSet::new()),
            launch_task_abort_handles: Mutex::new(HashMap::new()),
        })
    }

    /// Open production state with trusted head verification plus observable,
    /// cancellable projection recovery. The observer is called from the
    /// blocking open task and must remain non-blocking.
    pub fn new_with_head_trust_and_recovery_observer(
        app_root: PathBuf,
        runtime_state_dir: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
        recovery_observer: Arc<dyn ryeos_state::ProjectionRecoveryObserver>,
    ) -> Result<Self> {
        Self::open(
            app_root,
            runtime_state_dir,
            runtime_db_path,
            signer,
            write_barrier,
            head_trust,
            Some(recovery_observer),
        )
    }

    fn open(
        app_root: PathBuf,
        runtime_state_dir: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
        head_trust: Arc<ryeos_state::refs::TrustStore>,
        recovery_observer: Option<Arc<dyn ryeos_state::ProjectionRecoveryObserver>>,
    ) -> Result<Self> {
        let runtime_state_directory = lillux::PinnedDirectory::open_or_create(&runtime_state_dir)
            .context("failed to establish no-follow runtime_state_dir")?;
        let runtime_state_lock = runtime_state_directory
            .lock_exclusive()
            .context("lock live runtime-state namespace")?;
        let thread_runtime_authority =
            ThreadRuntimeAuthority::capture(&app_root, &runtime_state_directory, true)?;
        ryeos_state::CasMutationGuard::ensure_anchor(&runtime_state_dir)
            .context("initialize persistent CAS mutation lock anchor")?;
        ryeos_state::gc::GcLock::ensure_anchor(&runtime_state_dir)
            .context("initialize persistent GC lock anchor")?;

        let projection_health = Arc::new(ThreadProjectionHealth::default());
        // Runtime state must be readable before projection recovery can decide
        // whether a headless Set's replaceable rows are safe to discard.
        let runtime_db_parent_path = runtime_db_path.parent().unwrap_or_else(|| Path::new("."));
        let runtime_db_parent = lillux::PinnedDirectory::open_or_create(runtime_db_parent_path)
            .context("pin runtime database namespace")?;
        let runtime_db = if runtime_state_directory.is_same_directory(&runtime_db_parent)? {
            runtime_db::RuntimeDb::open_with_namespace_authority(
                &runtime_db_path,
                runtime_db_parent,
                runtime_state_lock.clone(),
            )?
        } else {
            runtime_db::RuntimeDb::open(&runtime_db_path)?
        };
        // Launch claims are durable owner evidence. They must survive process
        // restart so reconciliation can revoke the exact abandoned claim after
        // proving its attached process identity dead (or classify a genuinely
        // pre-attach window). Never erase ownership history wholesale here.
        let state_db = match recovery_observer {
            Some(recovery_observer) => {
                StateDb::open_with_recovery_observer_runtime_liveness_and_namespace_authority(
                    &runtime_state_dir,
                    runtime_state_directory,
                    runtime_state_lock,
                    projection_health.clone(),
                    head_trust,
                    recovery_observer,
                    &runtime_db,
                )?
            }
            None => {
                StateDb::open_with_projection_repair_sink_runtime_liveness_and_namespace_authority(
                    &runtime_state_dir,
                    runtime_state_directory,
                    runtime_state_lock,
                    projection_health.clone(),
                    head_trust,
                    &runtime_db,
                )?
            }
        };
        projection_health.observe_pending_transitions(state_db.pending_chain_transitions()?.len());
        let state_authority = state_db.pinned_authority()?;
        Ok(Self {
            state_authority,
            thread_runtime_authority: Some(thread_runtime_authority),
            inner: Mutex::new(Inner {
                state_db,
                runtime_db,
                signer,
            }),
            projection_health,
            read_only: false,
            allow_projection_rebuild: false,
            write_barrier,
            process_attachment_admission_open: AtomicBool::new(true),
            active_launch_owners: Mutex::new(HashSet::new()),
            launch_task_abort_handles: Mutex::new(HashMap::new()),
        })
    }

    pub fn is_launch_owner_active(&self, launch_owner: &str) -> bool {
        self.active_launch_owners
            .lock()
            .map(|active| active.contains(launch_owner))
            .unwrap_or(false)
    }

    /// Persist a bounded post-exit settlement observation only while the
    /// exact current-daemon launch owner remains registered and claimed.
    pub fn observe_active_owner_dead_process(
        &self,
        thread_id: &str,
        claim_id: &str,
        launch_owner: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        observed_at_ms: i64,
    ) -> Result<Option<i64>> {
        let g = self.lock()?;
        let claim = g.runtime_db.get_launch_claim(thread_id)?;
        if !claim.as_ref().is_some_and(|claim| {
            claim.claim_id == claim_id
                && claim.claimed_by == launch_owner
                && self.is_launch_owner_active(launch_owner)
        }) {
            return Ok(None);
        }
        g.runtime_db
            .observe_dead_process_if_matches(thread_id, process_identity, observed_at_ms)
    }

    /// Get the CAS root path for raw CAS access.
    pub fn cas_root(&self) -> Result<std::path::PathBuf> {
        let g = self.lock()?;
        Ok(g.state_db.cas_root().to_path_buf())
    }

    /// Get the refs root path for ref system access.
    pub fn refs_root(&self) -> Result<std::path::PathBuf> {
        let g = self.lock()?;
        Ok(g.state_db.refs_root().to_path_buf())
    }

    pub fn pending_head_transition_status(&self) -> Result<PendingHeadTransitionStatus> {
        let g = self.lock()?;
        let transitions = g.state_db.pending_chain_transitions()?;
        let oldest_prepared_at = transitions
            .iter()
            .map(|transition| transition.prepared_at.as_str())
            .min()
            .map(str::to_owned);
        let oldest_age_seconds = oldest_prepared_at
            .as_deref()
            .map(|prepared_at| -> Result<u64> {
                let prepared_at = parse_canonical_timestamp(prepared_at)
                    .context("invalid pending transition prepared_at")?;
                let now = parse_canonical_timestamp(&lillux::time::iso8601_now())
                    .context("invalid current time while reading transition diagnostics")?;
                Ok(now.signed_duration_since(prepared_at).num_seconds().max(0) as u64)
            })
            .transpose()?;
        Ok(PendingHeadTransitionStatus {
            pending: transitions.len(),
            pending_sets: transitions
                .iter()
                .filter(|transition| transition.operation == ryeos_state::HeadOperation::Set)
                .count(),
            pending_removes: transitions
                .iter()
                .filter(|transition| transition.operation == ryeos_state::HeadOperation::Remove)
                .count(),
            oldest_prepared_at,
            oldest_age_seconds,
        })
    }

    /// Run a closure with access to the underlying StateDb.
    pub fn with_state_db<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&StateDb) -> Result<T>,
    {
        let g = self.lock()?;
        f(&g.state_db)
    }

    /// Run a state publication while the exact execution launch owner remains
    /// current. This closes the stale-waiter window for side effects such as a
    /// signed project HEAD compare-and-swap.
    pub fn with_state_db_owned<F, T>(&self, thread_id: &str, launch_owner: &str, f: F) -> Result<T>
    where
        F: FnOnce(&StateDb) -> Result<T>,
    {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner cannot publish state for thread {thread_id}");
        }
        f(&g.state_db)
    }

    /// Strict, non-mutating verification of the selected projection against a
    /// stable snapshot of trusted heads and CAS.
    pub fn verify_projection_generation(
        &self,
    ) -> Result<ryeos_state::rebuild::ProjectionVerificationReport> {
        let _fork_sensitive_descriptors = lillux::retain_fork_sensitive_descriptors();
        let _cas_guard = self
            .state_authority
            .acquire_exclusive_guard(!self.read_only)?;
        let _write_permit = self.write_barrier.try_acquire().map_err(|error| {
            anyhow!("cannot acquire write permit for projection verification: {error}")
        })?;
        let g = self.lock()?;
        g.state_db.verify_projection_generation()
    }

    /// Publish a freshly rebuilt projection generation and switch
    /// this live store to it. The offline service path still obeys the global
    /// mutation hierarchy so no direct/import publisher can overlap the head
    /// snapshot or generation publication.
    pub fn rebuild_projection_generation(&self) -> Result<ryeos_state::rebuild::RebuildReport> {
        if !self.allow_projection_rebuild {
            bail!(
                "projection rebuild is available only in the authored offline rebuild bootstrap mode"
            );
        }
        let _fork_sensitive_descriptors = lillux::retain_fork_sensitive_descriptors();
        let cas_guard = self.state_authority.acquire_exclusive_guard(true)?;
        let _write_permit = self.write_barrier.try_acquire().map_err(|error| {
            anyhow!("cannot acquire write permit for projection rebuild: {error}")
        })?;
        let mut g = self.lock()?;
        let Inner {
            state_db,
            runtime_db,
            ..
        } = &mut *g;
        state_db.rebuild_projection_generation_admitted(Some(&*runtime_db), &cas_guard)
    }

    /// Consume a staged remote chain import through the normal mutation
    /// hierarchy. The CAS guard is acquired before the StateStore mutex and is
    /// passed into the journaled head publisher; StateDb never reacquires it
    /// from beneath the mutex.
    pub fn finalize_staged_chain_import(
        &self,
        staged: ryeos_state::sync::StagedChainImport,
    ) -> Result<ryeos_state::sync::ImportResult> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        ryeos_state::sync::finalize_import(
            &g.state_db,
            staged,
            g.signer.as_ref(),
            permit.cas_guard(),
        )
    }

    fn lock(&self) -> Result<StateStoreGuard<'_>> {
        let started = std::time::Instant::now();
        let fork_sensitive_descriptors = lillux::retain_fork_sensitive_descriptors();
        let inner = self
            .inner
            .lock()
            .map_err(|e| anyhow!("StateStore lock poisoned: {e}"))?;
        let waited = started.elapsed();
        if waited >= std::time::Duration::from_millis(100) {
            tracing::warn!(
                wait_ms = waited.as_millis() as u64,
                "StateStore lock acquisition was delayed"
            );
        }
        Ok(StateStoreGuard {
            _fork_sensitive_descriptors: fork_sensitive_descriptors,
            inner,
        })
    }

    fn warn_slow_lock_hold(operation: &'static str, started: std::time::Instant) {
        let held = started.elapsed();
        if held >= std::time::Duration::from_millis(100) {
            tracing::warn!(
                operation,
                hold_ms = held.as_millis() as u64,
                "StateStore lock was held by a slow operation"
            );
        }
    }

    /// Acquire a write permit from the write barrier.
    /// Fails if the daemon is quiescing for GC.
    fn acquire_write_permit(&self) -> Result<StateMutationPermit> {
        if self.read_only {
            bail!("state store is open for strict read-only verification");
        }
        let fork_sensitive_descriptors = lillux::retain_fork_sensitive_descriptors();
        let cas_guard = self.state_authority.acquire_shared_guard()?;
        let write_permit = self
            .write_barrier
            .try_acquire()
            .map_err(|e| anyhow!("cannot acquire write permit: {e}"))?;
        Ok(StateMutationPermit {
            _fork_sensitive_descriptors: fork_sensitive_descriptors,
            cas_guard,
            _write_permit: write_permit,
        })
    }

    /// Serialize a terminal-GC dry-run with ordinary writers using only
    /// already-established lock anchors. This retains the normal barrier and
    /// lock order but cannot create recovery state merely by inspecting it.
    fn acquire_gc_inspection_permit(&self) -> Result<StateMutationPermit> {
        if self.read_only {
            bail!("state store is open for strict read-only verification");
        }
        let fork_sensitive_descriptors = lillux::retain_fork_sensitive_descriptors();
        let cas_guard = self.state_authority.acquire_shared_guard()?;
        let write_permit = self
            .write_barrier
            .try_acquire()
            .map_err(|e| anyhow!("cannot acquire GC inspection permit: {e}"))?;
        Ok(StateMutationPermit {
            _fork_sensitive_descriptors: fork_sensitive_descriptors,
            cas_guard,
            _write_permit: write_permit,
        })
    }

    /// Narrow mutation permit for converging journaled Remove records as part
    /// of the authored offline projection-rebuild bootstrap. It does not widen
    /// the common write gate, so unrelated StateStore mutations remain denied.
    fn acquire_recovery_cleanup_permit(&self, _dry_run: bool) -> Result<StateMutationPermit> {
        if self.read_only && !self.allow_projection_rebuild {
            bail!("state store is open for strict read-only verification");
        }
        let fork_sensitive_descriptors = lillux::retain_fork_sensitive_descriptors();
        let cas_guard = self.state_authority.acquire_shared_guard()?;
        let write_permit = self
            .write_barrier
            .try_acquire()
            .map_err(|e| anyhow!("cannot acquire recovery cleanup permit: {e}"))?;
        Ok(StateMutationPermit {
            _fork_sensitive_descriptors: fork_sensitive_descriptors,
            cas_guard,
            _write_permit: write_permit,
        })
    }

    fn authorize_runtime_pin_for_thread(
        g: &Inner,
        thread_id: &str,
    ) -> Result<ryeos_state::AuthoritativeRuntimePinAdmission> {
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("runtime-pin thread {thread_id} does not exist"))?;
        g.state_db.authorize_runtime_pin(&thread.chain_root_id)
    }

    #[tracing::instrument(
        name = "state:create_thread",
        skip(self, thread),
        fields(
            thread_id = %thread.thread_id,
            chain_root_id = %thread.chain_root_id,
            kind = %thread.kind,
            item_ref = %thread.item_ref,
        )
    )]
    pub(crate) fn create_child_thread_admitted(
        &self,
        thread: &NewThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        if thread.thread_id == thread.chain_root_id {
            bail!("child persistence requires thread_id != chain_root_id");
        }
        if thread.captured_history_policy.is_some() {
            bail!("non-root threads cannot carry a captured history policy");
        }
        self.create_thread_inner(thread)
    }

    /// Raw root persistence is crate-private. The only production callers are
    /// lifecycle methods which first consume an opaque, current admission.
    pub(crate) fn create_admitted_root_thread(
        &self,
        thread: &NewThreadRecord,
    ) -> Result<CreatedThreadPublication> {
        if thread.thread_id != thread.chain_root_id {
            bail!("admitted root persistence requires thread_id == chain_root_id");
        }
        if thread.captured_history_policy.is_none() {
            bail!(
                "new chain root {} has no verified captured history policy",
                thread.thread_id
            );
        }
        self.create_root_thread_with_events_and_launch_metadata(thread, Vec::new(), None)
    }

    /// State-layer fixture for tests which exercise persistence below the
    /// engine admission boundary. It is absent from production builds and
    /// deliberately names the bypass; application/runtime tests should prefer
    /// a real lifecycle admission.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_thread_for_test(
        &self,
        thread: &NewThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        if thread.thread_id == thread.chain_root_id {
            self.create_admitted_root_thread(thread)
                .map(|publication| publication.persisted)
        } else {
            self.create_child_thread_admitted(thread)
        }
    }

    fn create_thread_inner(&self, thread: &NewThreadRecord) -> Result<Vec<PersistedEventRecord>> {
        if thread.thread_id == thread.chain_root_id {
            return self
                .create_root_thread_with_events_and_launch_metadata(thread, Vec::new(), None)
                .map(|publication| publication.persisted);
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("thread creation is closed for daemon shutdown");
        }
        let mut payload = json!({
            "kind": &thread.kind,
            "item_ref": &thread.item_ref,
            "executor_ref": &thread.executor_ref,
            "launch_mode": &thread.launch_mode,
        });
        if let Some(usage_subject) = &thread.usage_subject {
            usage_subject.validate()?;
            payload["usage_subject"] =
                serde_json::to_value(usage_subject).context("failed to encode usage_subject")?;
            if let Some(asserted_by) = &thread.usage_subject_asserted_by {
                payload["usage_subject_asserted_by"] = json!(asserted_by);
            }
        }

        let create_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload,
        };

        let te = convert_events(
            std::slice::from_ref(&create_event),
            &thread.chain_root_id,
            &thread.thread_id,
        );
        {
            let _admission = g.state_db.authorize_runtime_pin(&thread.chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&thread.thread_id, &thread.chain_root_id)?;
        }
        let committed = g.state_db.add_thread_with_events_admitted(
            &thread.chain_root_id,
            build_snapshot(thread),
            te,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        );
        let result = match committed {
            Ok(committed) => committed_value(committed),
            Err(error) => {
                let _ = g.runtime_db.delete_thread_runtime(&thread.thread_id);
                return Err(error);
            }
        };

        persisted_from_add_thread_with_events(&result, &[create_event])
    }

    /// Create a root thread together with caller-supplied initial durable
    /// events. The snapshot, `thread_created`, and supplied events share one
    /// authoritative chain-head commit and projection transaction.
    pub fn create_root_thread_with_events(
        &self,
        thread: &NewThreadRecord,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_root_thread_with_events_and_launch_metadata(thread, initial_events, None)
            .map(|publication| publication.persisted)
    }

    /// Create a managed-launch root with its resume identity installed before
    /// the authoritative chain head becomes visible. The runtime row is
    /// auxiliary, so it is prepared first and removed if chain creation fails.
    pub(crate) fn create_root_thread_with_events_and_launch_metadata(
        &self,
        thread: &NewThreadRecord,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
    ) -> Result<CreatedThreadPublication> {
        if thread.thread_id != thread.chain_root_id || thread.upstream_thread_id.is_some() {
            bail!("create_root_thread_with_events requires a root thread record");
        }
        if initial_events
            .iter()
            .any(|event| event.storage_class == "ephemeral")
        {
            bail!("root initial events must be durable");
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let launch_planning = g.runtime_db.launch_planning_by_thread(&thread.thread_id)?;
        if let Some(planning) = launch_planning.as_ref() {
            if planning.state != "planning"
                || planning.daemon_generation_id != runtime_db::daemon_generation_id()
            {
                return Err(LaunchPlanningInactive.into());
            }
        }
        // Initial facet events are subject to the same collection/key/value
        // limits as ordinary appends. The new thread is not projected yet, so
        // the validator correctly evaluates this batch against an empty set.
        validate_facet_event_admission(&g, &thread.thread_id, &initial_events)?;
        let mut payload = json!({
            "kind": &thread.kind,
            "item_ref": &thread.item_ref,
            "executor_ref": &thread.executor_ref,
            "launch_mode": &thread.launch_mode,
        });
        if let Some(usage_subject) = &thread.usage_subject {
            usage_subject.validate()?;
            payload["usage_subject"] =
                serde_json::to_value(usage_subject).context("failed to encode usage_subject")?;
            if let Some(asserted_by) = &thread.usage_subject_asserted_by {
                payload["usage_subject_asserted_by"] = json!(asserted_by);
            }
        }
        let mut events = Vec::with_capacity(initial_events.len() + 1);
        events.push(NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_CREATED.to_string(),
            storage_class: "indexed".to_string(),
            payload,
        });
        events.extend(initial_events);
        let thread_events = convert_events(&events, &thread.chain_root_id, &thread.thread_id);
        let birth_snapshot = attach_admitted_launch_capsule(
            &self.state_authority,
            permit.cas_guard(),
            build_snapshot(thread),
            launch_metadata,
        )?;
        let thread_runtime = RuntimeInfo {
            launch_metadata: launch_metadata.cloned(),
            ..RuntimeInfo::default()
        };
        // Establish the auxiliary runtime row before the authoritative commit.
        // If chain creation fails, remove it; an orphan auxiliary row is
        // recoverable, while a committed launch row with no runtime ledger is
        // not safe to hand off.
        {
            g.runtime_db
                .insert_thread_runtime(&thread.thread_id, &thread.chain_root_id)?;
            if let Some(launch_metadata) = launch_metadata {
                if let Err(error) = g
                    .runtime_db
                    .set_launch_metadata(&thread.thread_id, launch_metadata)
                {
                    let _ = g.runtime_db.delete_thread_runtime(&thread.thread_id);
                    return Err(error);
                }
            }
        }
        let committed = g.state_db.create_chain_with_events_admitted(
            &thread.chain_root_id,
            birth_snapshot,
            thread_events,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        );
        let result = match committed {
            Ok(committed) => committed_value(committed),
            Err(error) => {
                if let Err(settle_error) = g.runtime_db.fail_launch_planning(&thread.thread_id) {
                    tracing::error!(
                        thread_id = %thread.thread_id,
                        error = %settle_error,
                        "failed to settle launch planning after root-chain creation failed"
                    );
                }
                if let Err(cleanup_error) = g.runtime_db.delete_thread_runtime(&thread.thread_id) {
                    tracing::error!(
                        thread_id = %thread.thread_id,
                        error = %cleanup_error,
                        "failed to remove auxiliary runtime row after root-chain creation failed"
                    );
                }
                return Err(error);
            }
        };
        // The signed root birth is authoritative from this point onward.
        // Planning settlement and live publication are auxiliary and must not
        // turn a committed root into a pre-launch error.
        if let Some(planning) = launch_planning {
            match g.runtime_db.bind_launch_planning(&thread.thread_id) {
                Ok(true) => {}
                Ok(false) => tracing::error!(
                    thread_id = %thread.thread_id,
                    launch_id = %planning.launch_id,
                    "authoritative root committed but launch planning bind changed no row"
                ),
                Err(error) => tracing::error!(
                    thread_id = %thread.thread_id,
                    launch_id = %planning.launch_id,
                    error = %error,
                    "authoritative root committed but launch planning bind failed"
                ),
            }
            match self.launch_task_abort_handles.lock() {
                Ok(mut handles) => {
                    handles.remove(&planning.launch_id);
                }
                Err(poisoned) => {
                    tracing::error!(
                        thread_id = %thread.thread_id,
                        launch_id = %planning.launch_id,
                        "launch task abort registry was poisoned after root commit"
                    );
                    poisoned.into_inner().remove(&planning.launch_id);
                }
            }
        }
        let persisted = match persisted_from_add_thread_with_events(&result, &events) {
            Ok(persisted) => persisted,
            Err(error) => {
                tracing::error!(
                    thread_id = %thread.thread_id,
                    chain_root_id = %thread.chain_root_id,
                    error = %error,
                    "authoritative root committed but live event reconstruction failed"
                );
                Vec::new()
            }
        };
        let successor = thread_detail_from_created_snapshot(result.snapshot, thread_runtime);
        Ok(CreatedThreadPublication {
            persisted,
            successor,
        })
    }

    #[tracing::instrument(
        name = "state:create_trace_branch",
        skip(self, thread, branch_payload),
        fields(
            thread_id = %thread.thread_id,
            chain_root_id = %thread.chain_root_id,
            item_ref = %thread.item_ref,
        )
    )]
    pub fn create_trace_branch(
        &self,
        thread: &NewThreadRecord,
        branch_payload: Value,
    ) -> Result<Vec<PersistedEventRecord>> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("trace-branch creation is closed for daemon shutdown");
        }

        if thread.thread_id == thread.chain_root_id {
            bail!("trace branch child must not be a chain root thread");
        }
        if thread.upstream_thread_id.is_some() {
            bail!("trace branch child must not use upstream_thread_id");
        }
        if g.state_db.get_thread(&thread.thread_id)?.is_some() {
            bail!("thread already exists: {}", thread.thread_id);
        }

        let create_event = NewEventRecord {
            event_type: ryeos_state::event_types::THREAD_CREATED.to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &thread.kind,
                "item_ref": &thread.item_ref,
                "executor_ref": &thread.executor_ref,
                "launch_mode": &thread.launch_mode,
                "trace_branch": true,
            }),
        };
        let branch_event = NewEventRecord {
            event_type: ryeos_state::event_types::EDGE_RECORDED.to_string(),
            storage_class: "indexed".to_string(),
            payload: branch_payload,
        };
        let events_to_append = vec![create_event, branch_event];
        let te = convert_events(&events_to_append, &thread.chain_root_id, &thread.thread_id);
        let result = committed_value(g.state_db.add_thread_with_events_admitted(
            &thread.chain_root_id,
            build_snapshot(thread),
            te,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        )?);

        {
            let _admission = g.state_db.authorize_runtime_pin(&thread.chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&thread.thread_id, &thread.chain_root_id)?;
        }

        persisted_from_add_thread_with_events(&result, &events_to_append)
    }

    #[tracing::instrument(
        name = "state:mark_thread_running",
        skip(self),
        fields(thread_id = %thread_id)
    )]
    pub fn mark_thread_running(
        &self,
        thread_id: &str,
        base_project_snapshot_hash: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.mark_thread_running_with_events(thread_id, base_project_snapshot_hash, Vec::new())
    }

    /// Atomically append launch-attempt audit and cross a created thread into
    /// `running`. Existing running recovery attempts append only their new
    /// audit; they never emit a duplicate `thread_started` event.
    pub fn mark_thread_running_with_events(
        &self,
        thread_id: &str,
        base_project_snapshot_hash: Option<&str>,
        mut initial_events: Vec<NewEventRecord>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("mark_running is fenced during daemon shutdown");
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing while marking running: {thread_id}"))?;
        if let Some(intent) = runtime.stop_intent {
            bail!(
                "mark_running is fenced after {} request for thread {thread_id}",
                intent.as_str()
            );
        }
        let authoritative_snapshot = authoritative_snapshot_for_transition(
            &g,
            &thread_row.chain_root_id,
            &thread_row.thread_id,
        )?;
        verify_admitted_launch_capsule(
            &self.state_authority,
            &authoritative_snapshot,
            runtime.launch_metadata.as_ref(),
        )?;

        let transition_created = match thread_row.status.as_str() {
            // Fresh launch: fall through to the created -> running transition
            // (appends `thread_started`, sets `started_at`).
            "created" => true,
            // Same-thread crash recovery re-spawns a row that is still `running`,
            // and the resumed runtime calls `mark_running` again. Idempotent
            // no-op: do NOT append a second `thread_started` or rewrite
            // `started_at` — an empty persisted-events list means "already
            // running". (`drain_running_threads` still sees `running`, so the
            // shutdown kill window stays intact — no transient non-running state.)
            "running" if initial_events.is_empty() => return Ok(Vec::new()),
            "running" => false,
            other => {
                bail!("invalid status transition: {other} -> running");
            }
        };

        let snapshot_updates = if transition_created {
            let mut updated_snapshot = authoritative_snapshot;
            let authoritative_base = updated_snapshot
                .project_authority
                .base_snapshot_projection()
                .map(str::to_owned);
            if let Some(requested_base) = base_project_snapshot_hash {
                if Some(requested_base) != authoritative_base.as_deref() {
                    bail!(
                        "mark_running project snapshot mismatch for {thread_id}: authoritative {:?}, requested {:?}",
                        authoritative_base,
                        requested_base,
                    );
                }
            }

            let now = lillux::time::iso8601_now();
            updated_snapshot.status = ThreadStatus::Running;
            updated_snapshot.updated_at.clone_from(&now);
            updated_snapshot.started_at = Some(now);
            updated_snapshot.finished_at = None;
            updated_snapshot.base_project_snapshot_hash = authoritative_base;
            initial_events.push(NewEventRecord {
                event_type: "thread_started".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({}),
            });
            vec![SnapshotUpdate {
                thread_id: thread_id.to_string(),
                new_snapshot: updated_snapshot,
            }]
        } else {
            Vec::new()
        };

        let te = convert_events(&initial_events, &thread_row.chain_root_id, thread_id);
        let result = committed_value(g.state_db.append_events_admitted(
            &thread_row.chain_root_id,
            thread_id,
            te,
            snapshot_updates,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        )?);

        persisted_from_append(&result, &initial_events)
    }

    #[tracing::instrument(
        name = "state:finalize_thread",
        skip(self, update),
        fields(thread_id = %thread_id, status = %update.status)
    )]
    pub fn finalize_thread(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.finalize_thread_locked(thread_id, update)
            .map(|(events, _)| events)
    }

    /// Generic lifecycle finalization that also returns the effective record
    /// after global durable-stop dominance. Higher layers must use this form so
    /// scheduler/command/follow side effects match the persisted terminal event.
    pub fn finalize_thread_effective(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        self.finalize_thread_locked(thread_id, update)
    }

    pub fn finalize_thread_effective_owned(
        &self,
        thread_id: &str,
        launch_owner: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("thread {thread_id} has no launch owner"))?;
        if claim.claimed_by != launch_owner {
            anyhow::bail!("stale launch owner cannot finalize thread {thread_id}");
        }
        self.finalize_thread_with_guard(&g, permit.cas_guard(), thread_id, update, false)
    }

    /// Runtime-callback finalization with stop/shutdown policy enforced under
    /// the same StateStore lock as the terminal commit. A durable Cancel/Kill
    /// dominates any self-reported status; shutdown without an explicit stop
    /// rejects the commit so recovery can resume the preserved row.
    pub fn finalize_thread_from_runtime(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        self.finalize_thread_locked(thread_id, update)
    }

    fn finalize_thread_locked(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        self.finalize_thread_with_guard(&g, permit.cas_guard(), thread_id, update, false)
    }

    /// Atomically finalize a child-link failure only while the child is still a
    /// never-launched row. This conditional transition is deliberately allowed
    /// after shutdown admission closes: unlike a generic finalizer, the guarded
    /// row has no process for shutdown to own and no launcher entitled to attach
    /// one. A durable stop still dominates the requested failure outcome.
    pub fn finalize_created_unattached_if_current(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<FinalizeCreatedUnattachedOutcome> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;

        if is_terminal_status(&thread_row.status) {
            return Ok(FinalizeCreatedUnattachedOutcome::AlreadyTerminal);
        }

        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        let process_attached =
            runtime.pid.is_some() || runtime.pgid.is_some() || runtime.process_identity.is_some();
        let launch_claimed = g.runtime_db.get_launch_claim(thread_id)?.is_some();
        if thread_row.status != ThreadStatus::Created.as_str() || process_attached || launch_claimed
        {
            return Ok(FinalizeCreatedUnattachedOutcome::NotCurrent {
                status: thread_row.status,
                process_attached,
                launch_claimed,
            });
        }

        let (persisted, effective) = self.finalize_thread_with_rows(
            &g,
            permit.cas_guard(),
            thread_id,
            thread_row,
            runtime,
            update,
            true,
        )?;
        Ok(FinalizeCreatedUnattachedOutcome::Finalized {
            persisted,
            effective: Box::new(effective),
        })
    }

    /// Atomically finalize a nonterminal row, or report the terminal/shutdown
    /// winner without a check-then-write race. A durable Cancel/Kill tombstone
    /// is folded into the effective terminal record by
    /// [`Self::finalize_thread_with_rows`].
    pub fn finalize_if_nonterminal(
        &self,
        thread_id: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<FinalizeIfNonterminalOutcome> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        if is_terminal_status(&thread_row.status) {
            return Ok(FinalizeIfNonterminalOutcome::AlreadyTerminal {
                status: thread_row.status,
            });
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        if runtime.stop_intent.is_none()
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            g.runtime_db.reset_resume_attempts(thread_id)?;
            return Ok(FinalizeIfNonterminalOutcome::PreservedForShutdown);
        }
        let (persisted, effective) = self.finalize_thread_with_rows(
            &g,
            permit.cas_guard(),
            thread_id,
            thread_row,
            runtime,
            update,
            false,
        )?;
        Ok(FinalizeIfNonterminalOutcome::Finalized {
            persisted,
            effective: Box::new(effective),
        })
    }

    /// Owner-qualified form of [`Self::finalize_if_nonterminal`] for daemon
    /// supervised method/runtime completion. The terminal check, exact owner
    /// comparison, stop dominance, and winning write share one StateStore lock.
    pub fn finalize_if_nonterminal_owned(
        &self,
        thread_id: &str,
        launch_owner: &str,
        update: &FinalizeThreadRecord,
    ) -> Result<FinalizeIfNonterminalOutcome> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        if is_terminal_status(&thread_row.status) {
            return Ok(FinalizeIfNonterminalOutcome::AlreadyTerminal {
                status: thread_row.status,
            });
        }
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner cannot finalize thread {thread_id}");
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        if runtime.stop_intent.is_none()
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            g.runtime_db.reset_resume_attempts(thread_id)?;
            return Ok(FinalizeIfNonterminalOutcome::PreservedForShutdown);
        }
        let (persisted, effective) = self.finalize_thread_with_rows(
            &g,
            permit.cas_guard(),
            thread_id,
            thread_row,
            runtime,
            update,
            false,
        )?;
        Ok(FinalizeIfNonterminalOutcome::Finalized {
            persisted,
            effective: Box::new(effective),
        })
    }

    fn finalize_thread_with_guard(
        &self,
        g: &Inner,
        cas_mutation_guard: &ryeos_state::CasMutationGuard,
        thread_id: &str,
        update: &FinalizeThreadRecord,
        allow_closed_admission: bool,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing during finalization: {thread_id}"))?;
        self.finalize_thread_with_rows(
            g,
            cas_mutation_guard,
            thread_id,
            thread_row,
            runtime,
            update,
            allow_closed_admission,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finalize_thread_with_rows(
        &self,
        g: &Inner,
        cas_mutation_guard: &ryeos_state::CasMutationGuard,
        thread_id: &str,
        thread_row: queries::ThreadRow,
        runtime: RuntimeInfo,
        update: &FinalizeThreadRecord,
        allow_closed_admission: bool,
    ) -> Result<(Vec<PersistedEventRecord>, FinalizeThreadRecord)> {
        let validated_final_cost = update
            .final_cost
            .as_ref()
            .map(validate_final_cost)
            .transpose()?;
        let mut effective_update = update.clone();
        // Stop intent dominates every later finalizer, including administrative
        // failure nets. This is intentionally global: a check-then-finalize
        // caller must not be able to overwrite a stop that committed between
        // its check and this lock acquisition.
        if let Some(intent) = runtime.stop_intent {
            let status = match intent {
                StopIntent::Cancel => ThreadStatus::Cancelled,
                StopIntent::Kill => ThreadStatus::Killed,
            };
            effective_update.status = status.as_str().to_string();
            effective_update.outcome_code = Some(status.as_str().to_string());
            effective_update.result_json = None;
            effective_update.error_json = Some(json!({
                "reason": "durable_stop_intent",
                "intent": intent.as_str(),
            }));
            // The runtime supplied envelope describes its reported outcome,
            // not the daemon-owned durable-stop winner above. Never sign that
            // contradictory process claim into the effective terminal
            // snapshot. Incurred cost remains authoritative and is retained.
            effective_update.managed_envelope = None;
            effective_update.result_project_snapshot_hash = None;
        } else if !allow_closed_admission
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            // Every terminal writer shares the shutdown fence. Otherwise an
            // execution-result fallback could turn a shutdown-owned kill into a
            // terminal failure after drain had taken ownership of the process.
            // A durable stop remains the one exception: it must be allowed to
            // settle to its dominant cancelled/killed outcome while draining.
            bail!("thread finalization is fenced during daemon shutdown");
        }
        let update = &effective_update;

        if is_terminal_status(&thread_row.status) {
            bail!(
                "invalid status transition: {} -> {}",
                thread_row.status,
                update.status
            );
        }

        let terminal_status = ThreadStatus::from_str_lossy(&update.status)
            .ok_or_else(|| anyhow!("invalid terminal status: {}", update.status))?;
        if !terminal_status.is_terminal() {
            bail!("finalize_thread requires a terminal status");
        }
        if let Some(cost) = update.final_cost.as_ref() {
            validate_final_cost_for_settlement(cost)?;
        }
        if let Some(envelope) = update.managed_envelope.as_ref() {
            validate_managed_terminal_envelope(
                envelope,
                thread_id,
                terminal_status,
                update.result_json.as_ref(),
                update.error_json.as_ref(),
                update.final_cost.as_ref(),
            )?;
        }

        let (additional_artifact_kind_bytes, additional_artifact_metadata_bytes) =
            update.artifacts.iter().try_fold(
                (0usize, 0usize),
                |(kind_total, metadata_total), artifact| {
                    let metadata_bytes = validate_new_artifact_shape(
                        &artifact.artifact_type,
                        artifact.metadata.as_ref(),
                    )?;
                    Ok::<_, anyhow::Error>((
                        kind_total
                            .checked_add(artifact.artifact_type.len())
                            .ok_or_else(|| anyhow!("terminal artifact kind byte total overflow"))?,
                        metadata_total.checked_add(metadata_bytes).ok_or_else(|| {
                            anyhow!("terminal artifact metadata byte total overflow")
                        })?,
                    ))
                },
            )?;
        if !update.artifacts.is_empty() {
            if !self.projection_health.is_current() {
                bail!("artifact admission requires a current thread projection");
            }
            ensure_artifact_projection_capacity(
                g,
                thread_id,
                update.artifacts.len(),
                additional_artifact_kind_bytes,
                additional_artifact_metadata_bytes,
            )?;
        }

        let now = lillux::time::iso8601_now();
        let facets = terminal_facets(update.final_cost.as_ref(), update.managed_envelope.as_ref())?;

        let artifacts_json: Vec<Value> = update
            .artifacts
            .iter()
            .map(|a| serde_json::to_value(a).unwrap())
            .collect();

        let mut updated_snapshot = authoritative_snapshot_for_transition(
            g,
            &thread_row.chain_root_id,
            &thread_row.thread_id,
        )?;
        let usage_started_at = updated_snapshot
            .started_at
            .clone()
            .unwrap_or_else(|| updated_snapshot.created_at.clone());
        updated_snapshot.status = terminal_status;
        updated_snapshot.updated_at.clone_from(&now);
        updated_snapshot.finished_at = Some(now.clone());
        updated_snapshot.result.clone_from(&update.result_json);
        updated_snapshot
            .outcome_code
            .clone_from(&update.outcome_code);
        updated_snapshot.error.clone_from(&update.error_json);
        updated_snapshot.budget = validated_final_cost.as_ref().map(|cost| {
            ThreadUsage {
                completed_turns: cost.completed_turns,
                input_tokens: cost.input_tokens,
                output_tokens: cost.output_tokens,
                spend_usd: cost.spend_usd,
                spawns_used: 0, // not tracked in FinalCost
                started_at: usage_started_at.clone(),
                settled_at: now.clone(),
                last_settled_turn_seq: u64::from(cost.completed_turns),
                elapsed_ms: 0, // daemon doesn't track wall-clock time
                provider_id: None,
                model: None,
                profile: None,
            }
        });
        updated_snapshot.artifacts = artifacts_json;
        updated_snapshot.facets = facets;
        updated_snapshot
            .result_project_snapshot_hash
            .clone_from(&update.result_project_snapshot_hash);

        let snapshot_update = SnapshotUpdate {
            thread_id: thread_id.to_string(),
            new_snapshot: updated_snapshot,
        };

        let mut events_to_append = Vec::new();

        for artifact in &update.artifacts {
            events_to_append.push(NewEventRecord {
                event_type: "artifact_published".to_string(),
                storage_class: "indexed".to_string(),
                payload: json!({
                    "artifact_type": artifact.artifact_type,
                    "uri": artifact.uri,
                    "content_hash": artifact.content_hash,
                    "metadata": artifact.metadata,
                }),
            });
        }

        let mut terminal_payload = json!({
            "outcome_code": update.outcome_code,
            "result": update.result_json,
            "has_error": update.error_json.is_some(),
            "artifact_count": update.artifacts.len(),
        });
        if let Some(err) = &update.error_json {
            if let Some(map) = terminal_payload.as_object_mut() {
                map.insert("error".to_string(), err.clone());
            }
        }
        events_to_append.push(NewEventRecord {
            event_type: terminal_event_type(&update.status)?.to_string(),
            storage_class: "indexed".to_string(),
            payload: terminal_payload,
        });

        let te = convert_events(&events_to_append, &thread_row.chain_root_id, thread_id);
        let result = committed_value(g.state_db.append_events_admitted(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![snapshot_update],
            g.signer.as_ref(),
            &g.runtime_db,
            cas_mutation_guard,
        )?);

        Ok((
            persisted_from_append(&result, &events_to_append)?,
            effective_update,
        ))
    }

    #[tracing::instrument(
        name = "state:create_continuation",
        skip(self, successor),
        fields(
            thread_id = %successor.thread_id,
            chain_root_id = %chain_root_id,
            source_thread_id = %source_thread_id,
        )
    )]
    pub(crate) fn create_continuation_admitted(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        initial_events: Vec<NewEventRecord>,
        launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
    ) -> Result<CreatedThreadPublication> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let launch_planning = g
            .runtime_db
            .launch_planning_by_thread(&successor.thread_id)?;
        if let Some(planning) = launch_planning.as_ref() {
            if planning.state != "planning"
                || planning.daemon_generation_id != runtime_db::daemon_generation_id()
            {
                return Err(LaunchPlanningInactive.into());
            }
        }
        validate_facet_event_admission(&g, &successor.thread_id, &initial_events)?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("continuation creation is closed for daemon shutdown");
        }
        let source_row = g
            .state_db
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {source_thread_id}"))?;

        if is_terminal_status(&source_row.status)
            && source_row.status != ThreadStatus::Failed.as_str()
            && source_row.status != ThreadStatus::Completed.as_str()
        {
            bail!(
                "cannot continue thread in terminal status '{}'",
                source_row.status
            );
        }

        // Single-successor invariant: a thread is continued at most once. The
        // write permit + lock held here serialize `create_continuation`, so this
        // check-then-create is atomic — a double-submit or race cannot mint
        // sibling successors (which would make `successor_thread_id` ambiguous).
        if let Some(existing) =
            queries::continuation_successor(g.state_db.projection(), source_thread_id)?
        {
            bail!("thread {source_thread_id} already continued as {existing}");
        }

        // Predecessor-immutability contract: an already-terminal source's
        // terminal SNAPSHOT (status + result + outcome) is never rewritten — an
        // operator follow-up onto a completed/failed turn preserves it. The
        // chain still records the handoff as a single append-only
        // `thread_continued` event on the source (the chain log is append-only by
        // nature; the single-successor guard above keeps it to exactly one), plus
        // the successor's `upstream_thread_id` link. "Immutable" therefore means
        // the terminal snapshot/result, not "no further chain events."
        //
        // Settle the source to `continued` only when it is still running — a
        // machine handoff (limit-exhausted) ends the run there. A terminal source
        // is left as-is (rewriting it would erase its result).
        let source_snapshot_updates = if is_terminal_status(&source_row.status) {
            Vec::new()
        } else {
            let now = lillux::time::iso8601_now();
            let source_snapshot = continued_snapshot_for_transition(&g, &source_row, &now)?;
            vec![SnapshotUpdate {
                thread_id: source_thread_id.to_string(),
                new_snapshot: source_snapshot,
            }]
        };

        // Ensure successor has upstream_thread_id set to source for edge derivation
        let mut successor_with_upstream = successor.clone();
        if successor_with_upstream.upstream_thread_id.is_none() {
            successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());
        }
        let successor_snapshot = attach_continuation_launch_capsule(
            &self.state_authority,
            &g,
            chain_root_id,
            source_thread_id,
            build_snapshot(&successor_with_upstream),
            launch_metadata,
        )?;
        let successor_runtime = RuntimeInfo {
            launch_metadata: launch_metadata.cloned(),
            ..RuntimeInfo::default()
        };
        {
            let _admission = g.state_db.authorize_runtime_pin(chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&successor.thread_id, chain_root_id)?;
            if let Some(launch_metadata) = launch_metadata {
                if let Err(error) = g
                    .runtime_db
                    .set_launch_metadata(&successor.thread_id, launch_metadata)
                {
                    let _ = g.runtime_db.delete_thread_runtime(&successor.thread_id);
                    return Err(error);
                }
            }
        }

        let successor_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &successor.kind,
                "item_ref": &successor.item_ref,
                "continuation_from": source_thread_id,
            }),
        };
        let mut successor_events = Vec::with_capacity(initial_events.len() + 1);
        successor_events.push(successor_event);
        successor_events.extend(initial_events);
        let successor_thread_events =
            convert_events(&successor_events, chain_root_id, &successor.thread_id);
        let source_event = NewEventRecord {
            event_type: "thread_continued".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "successor_thread_id": &successor.thread_id,
                "reason": reason,
            }),
        };
        let ste = convert_events(
            std::slice::from_ref(&source_event),
            chain_root_id,
            source_thread_id,
        );
        let successor_commit = g.state_db.add_thread_with_events_and_append_admitted(
            chain_root_id,
            successor_snapshot,
            successor_thread_events,
            source_thread_id,
            ste,
            source_snapshot_updates,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        );
        let successor_result = match successor_commit {
            Ok(committed) => committed_value(committed),
            Err(error) => {
                if let Err(settle_error) = g.runtime_db.fail_launch_planning(&successor.thread_id) {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        error = %settle_error,
                        "failed to settle launch planning after continuation creation failed"
                    );
                }
                if let Err(cleanup_error) = g.runtime_db.delete_thread_runtime(&successor.thread_id)
                {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        error = %cleanup_error,
                        "failed to remove runtime row after atomic continuation birth failed"
                    );
                }
                return Err(error);
            }
        };
        // The signed successor/source transition is authoritative from this
        // point onward. Auxiliary planning and live-notification bookkeeping
        // must not turn that committed continuation into a pre-launch error.
        if let Some(planning) = launch_planning {
            match g.runtime_db.bind_launch_planning(&successor.thread_id) {
                Ok(true) => {}
                Ok(false) => tracing::error!(
                    thread_id = %successor.thread_id,
                    launch_id = %planning.launch_id,
                    "authoritative continuation committed but launch planning bind changed no row"
                ),
                Err(error) => tracing::error!(
                    thread_id = %successor.thread_id,
                    launch_id = %planning.launch_id,
                    error = %error,
                    "authoritative continuation committed but launch planning bind failed"
                ),
            }
            match self.launch_task_abort_handles.lock() {
                Ok(mut handles) => {
                    handles.remove(&planning.launch_id);
                }
                Err(poisoned) => {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        launch_id = %planning.launch_id,
                        "launch task abort registry was poisoned after continuation commit"
                    );
                    poisoned.into_inner().remove(&planning.launch_id);
                }
            }
        }
        let mut all_input_events = successor_events;
        all_input_events.push(source_event);
        let persisted =
            match persisted_from_add_thread_with_events(&successor_result, &all_input_events) {
                Ok(persisted) => persisted,
                Err(error) => {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        source_thread_id,
                        chain_root_id,
                        error = %error,
                        "authoritative continuation committed but live event reconstruction failed"
                    );
                    Vec::new()
                }
            };
        let successor =
            thread_detail_from_created_snapshot(successor_result.snapshot, successor_runtime);
        Ok(CreatedThreadPublication {
            persisted,
            successor,
        })
    }

    /// Raw continuation fixture for state-layer tests. Absent in production.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_continuation_for_test(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_continuation_admitted(
            successor,
            source_thread_id,
            chain_root_id,
            reason,
            Vec::new(),
            None,
        )
        .map(|publication| publication.persisted)
    }

    /// Machine continuation handoff (limit cut-off) — the autonomous path.
    ///
    /// Unlike the operator-follow-up continuation path, which
    /// accepts a terminal source and leaves it as-is), this enforces the machine
    /// invariants atomically under the write permit + lock:
    ///
    /// - the source must be **exactly `running`** — a cut-off live run. Re-checked
    ///   here, not just by the caller, so a source that goes terminal between the
    ///   caller's check and this commit cannot mint a successor.
    /// - the source must carry a captured `ResumeContext` (spawn-time launch
    ///   identity) — a successor we cannot launch is worse than none.
    /// - the successor's launch metadata is **seeded before the source is settled
    ///   `continued`**, and the runtime-db writes (which can fail independently of
    ///   the source's terminal snapshot) happen first — so any failure aborts the
    ///   handoff with the source still `running` (the runner then fails terminal),
    ///   never `continued` behind an unlaunchable successor.
    pub fn create_machine_continuation(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let sanitized_reason =
            reason.filter(|r| !queries::ContinuationReasonMarker::is_reserved_str(r));
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::Machine { sanitized_reason },
            None,
            None,
            None,
            Vec::new(),
        )
    }

    // Source lineage, resume proof, launch metadata, and initial durable events
    // stay explicit because each is validated under the same write permit.
    #[allow(clippy::too_many_arguments)]
    pub fn create_machine_continuation_with_events(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        expected_resume_context: &crate::launch_metadata::ResumeContext,
        successor_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<Vec<PersistedEventRecord>> {
        // The machine handoff carries a free-form runtime LOG reason. Scrub ALL
        // daemon-reserved markers so a runtime cannot mint an edge the chain-depth
        // walk would treat as an operator reset or a depth-exempt follow.
        let sanitized_reason =
            reason.filter(|r| !queries::ContinuationReasonMarker::is_reserved_str(r));
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::Machine { sanitized_reason },
            Some(expected_resume_context),
            Some(successor_launch_metadata),
            None,
            initial_events,
        )
    }

    /// Create the parent's follow-resume successor: a running-source continuation
    /// marked `graph_follow_resume`. Created and seeded only — NOT launched (the
    /// resume path launches it later, once the child's result is available) and
    /// NOT subject to the autonomous chain-depth cap (a follow is structural
    /// progress, not an autonomous run). Daemon-only: the trusted marker cannot be
    /// reached through a runtime-supplied reason. The running source must declare
    /// native resume, and that exact policy is projected into the successor so a
    /// resumed segment remains eligible to suspend at another follow node.
    pub fn create_follow_resume_successor(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::GraphFollowResume,
            None,
            None,
            None,
            Vec::new(),
        )
    }

    pub fn create_follow_resume_successor_with_launch_metadata(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        successor_launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        result_project_snapshot_hash: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        self.create_running_continuation_successor(
            successor,
            source_thread_id,
            chain_root_id,
            RunningContinuationKind::GraphFollowResume,
            successor_launch_metadata.resume_context.as_ref(),
            Some(successor_launch_metadata),
            result_project_snapshot_hash,
            Vec::new(),
        )
    }

    /// Shared core for both running-source continuations (machine handoff and
    /// follow-resume). One atomic op under the write permit + lock: re-verify the
    /// source is running, enforce the single-successor invariant, require the
    /// source's captured ResumeContext, seed the successor (runtime-db writes
    /// first), then settle the source `continued`. A race or seed failure aborts
    /// with the source still running — never `continued` behind an unlaunchable
    /// successor.
    #[allow(clippy::too_many_arguments)]
    fn create_running_continuation_successor(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        kind: RunningContinuationKind<'_>,
        expected_resume_context: Option<&crate::launch_metadata::ResumeContext>,
        successor_launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
        source_result_snapshot_hash: Option<&str>,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        validate_facet_event_admission(&g, &successor.thread_id, &initial_events)?;
        let source_row = g
            .state_db
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {source_thread_id}"))?;

        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("continuation authoring is closed for daemon shutdown");
        }
        let source_runtime = g
            .runtime_db
            .get_runtime_info(source_thread_id)?
            .ok_or_else(|| anyhow!("source runtime row missing: {source_thread_id}"))?;
        if let Some(intent) = source_runtime.stop_intent.as_ref() {
            bail!(
                "cannot continue stop-requested thread {source_thread_id} ({})",
                intent.as_str()
            );
        }

        // A running-source continuation cuts a still-running source. Re-checked
        // under the lock to close the caller's check-then-commit race; a terminal
        // source is the operator follow-up path, not this one.
        if source_row.status != ThreadStatus::Running.as_str() {
            bail!(
                "running continuation requires a running source; \
                 thread {source_thread_id} is '{}'",
                source_row.status
            );
        }

        // Never braid a successor into the wrong chain.
        if chain_root_id != source_row.chain_root_id {
            bail!(
                "chain_root_id mismatch: requested {chain_root_id}, source \
                 {source_thread_id} is in chain {}",
                source_row.chain_root_id
            );
        }

        // Single-successor invariant (atomic under the held permit + lock).
        if let Some(existing) =
            queries::continuation_successor(g.state_db.projection(), source_thread_id)?
        {
            bail!("thread {source_thread_id} already continued as {existing}");
        }

        // Chain-level ceiling: bound the length of an AUTONOMOUS continuation run.
        // MACHINE handoffs only — a follow-resume edge is structural progress and
        // must be allowed even when the parent chain is already at the cap.
        if let RunningContinuationKind::Machine { .. } = &kind {
            let machine_depth = queries::consecutive_machine_continuation_depth(
                g.state_db.projection(),
                source_thread_id,
                crate::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH,
            )?;
            if machine_depth >= crate::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH {
                bail!(
                    "continuation depth limit reached ({machine_depth}/{}); the autonomous \
                     chain will not continue",
                    crate::thread_lifecycle::MAX_CONTINUATION_CHAIN_DEPTH
                );
            }
        }

        // Require the source's complete captured launch identity: the successor
        // must be able to fold the chain, and a follow successor must retain the
        // replay declaration needed to suspend again after it resumes.
        let source_launch_metadata = source_runtime.launch_metadata.ok_or_else(|| {
            anyhow!(
                "source thread {source_thread_id} has no captured ResumeContext; \
                 cannot create a launchable continuation successor"
            )
        })?;
        let source_resume_context = source_launch_metadata
            .resume_context
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "source thread {source_thread_id} has no captured ResumeContext; \
                     cannot create a launchable continuation successor"
                )
            })?;
        if matches!(&kind, RunningContinuationKind::GraphFollowResume)
            && source_launch_metadata.native_resume.is_none()
        {
            bail!("follow-resume source thread {source_thread_id} does not declare native_resume");
        }
        // Successor preconditions BEFORE any write: it must belong to the source's
        // chain and, if it names an upstream, name THIS source — never braid a
        // successor into the wrong chain or contradict the edge being created (a
        // later StateDb reject would leave an orphan runtime row behind).
        if successor.chain_root_id != source_row.chain_root_id {
            bail!(
                "successor {} chain_root_id {} does not match source chain {}",
                successor.thread_id,
                successor.chain_root_id,
                source_row.chain_root_id
            );
        }
        match successor.upstream_thread_id.as_deref() {
            None => {}
            Some(id) if id == source_thread_id => {}
            Some(other) => bail!(
                "successor {} declares upstream {other}, not the continuation source {source_thread_id}",
                successor.thread_id
            ),
        }
        let mut successor_with_upstream = successor.clone();
        successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());

        let mut successor_meta = successor_launch_metadata.cloned().unwrap_or_else(|| {
            source_launch_metadata.continuation_successor_seed(source_resume_context.clone())
        });
        let successor_resume_context =
            successor_meta
                .resume_context
                .as_ref()
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "successor {} has no captured ResumeContext",
                        successor.thread_id
                    )
                })?;
        if expected_resume_context.is_some_and(|expected| expected != &successor_resume_context) {
            bail!(
                "successor {} ResumeContext changed during authoritative preparation",
                successor.thread_id
            );
        }
        successor_resume_context.validate_continuation_transition_from(
            &source_resume_context,
            source_result_snapshot_hash,
        )?;
        if successor_meta
            .continuation_source_thread_id
            .as_deref()
            .is_some_and(|source| source != source_thread_id)
        {
            bail!("prepared successor names a different continuation source");
        }
        successor_meta.continuation_source_thread_id = Some(source_thread_id.to_string());
        let successor_snapshot = attach_continuation_launch_capsule(
            &self.state_authority,
            &g,
            chain_root_id,
            source_thread_id,
            build_continuation_snapshot(&successor_with_upstream, &successor_resume_context)?,
            Some(&successor_meta),
        )?;

        // Runtime-db writes FIRST: insert the successor runtime row and seed its
        // launch identity before any state-db successor snapshot or source
        // settle. If the seed fails, only an orphan runtime row exists — no
        // state-db successor edge, source untouched and still running.
        {
            if let Some(prepared) = successor_launch_metadata {
                if prepared.native_resume != source_launch_metadata.native_resume
                    || prepared.cancellation_mode != source_launch_metadata.cancellation_mode
                    || prepared.launch_driver != source_launch_metadata.launch_driver
                {
                    bail!(
                        "prepared successor execution policy differs from its source launch metadata"
                    );
                }
            }
            let _admission = g.state_db.authorize_runtime_pin(chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&successor.thread_id, chain_root_id)?;
            if let Err(error) = g
                .runtime_db
                .set_launch_metadata(&successor.thread_id, &successor_meta)
            {
                let _ = g.runtime_db.delete_thread_runtime(&successor.thread_id);
                return Err(error);
            }
        }

        // The successor becomes observable with its creation record and complete
        // authoritative launch audit in the same signed chain head. ResumeContext
        // was seeded above, so every visible successor is immediately relaunchable.
        let successor_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &successor.kind,
                "item_ref": &successor.item_ref,
                "continuation_from": source_thread_id,
            }),
        };
        let mut successor_events = Vec::with_capacity(initial_events.len() + 1);
        successor_events.push(successor_event);
        successor_events.extend(initial_events);
        let successor_thread_events =
            convert_events(&successor_events, chain_root_id, &successor.thread_id);
        // Settle the source to `continued` in the same signed head that creates
        // the successor and records its authoritative birth events.
        let now = lillux::time::iso8601_now();
        let mut source_snapshot = continued_snapshot_for_transition(&g, &source_row, &now)?;
        source_snapshot.result_project_snapshot_hash =
            source_result_snapshot_hash.map(ToOwned::to_owned);
        if let Some(result_hash) = source_result_snapshot_hash {
            if successor_with_upstream
                .base_project_snapshot_hash
                .as_deref()
                != Some(result_hash)
            {
                bail!(
                    "continuation successor {} base snapshot does not match frozen source result {}",
                    successor.thread_id,
                    result_hash
                );
            }
        }
        let edge_reason: Option<&str> = match &kind {
            RunningContinuationKind::Machine { sanitized_reason } => *sanitized_reason,
            RunningContinuationKind::GraphFollowResume => {
                Some(queries::ContinuationReasonMarker::GraphFollowResume.as_str())
            }
        };
        let source_event = NewEventRecord {
            event_type: "thread_continued".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "successor_thread_id": &successor.thread_id,
                "reason": edge_reason,
            }),
        };
        let ste = convert_events(
            std::slice::from_ref(&source_event),
            chain_root_id,
            source_thread_id,
        );
        let successor_commit = g.state_db.add_thread_with_events_and_append_admitted(
            chain_root_id,
            successor_snapshot,
            successor_thread_events,
            source_thread_id,
            ste,
            vec![SnapshotUpdate {
                thread_id: source_thread_id.to_string(),
                new_snapshot: source_snapshot,
            }],
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        );
        let successor_result = match successor_commit {
            Ok(committed) => committed_value(committed),
            Err(error) => {
                if let Err(cleanup_error) = g.runtime_db.delete_thread_runtime(&successor.thread_id)
                {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        error = %cleanup_error,
                        "failed to remove runtime row after atomic running-continuation birth failed"
                    );
                }
                return Err(error);
            }
        };
        let mut all_input_events = successor_events;
        all_input_events.push(source_event);
        persisted_from_add_thread_with_events(&successor_result, &all_input_events)
    }

    /// Operator follow-up continuation, made idempotent by a request fingerprint.
    ///
    /// Unlike [`Self::create_continuation`] (whose single-successor guard FAILS on
    /// a second follow-up), this dedups: under the write permit + lock, if the
    /// source already has a successor it compares the recorded
    /// `successor_request_fingerprint` to this request's — a match returns the
    /// existing successor (`Existing`, a double-submit), a mismatch is a
    /// `Conflict`. Otherwise it creates the successor and persists the fingerprint
    /// on the `thread_continued` edge in the SAME critical section, so dedup works
    /// even if the daemon crashes before the runtime emits anything. A terminal
    /// (completed/failed) source keeps its status; a running source is settled
    /// `continued` (same as `create_continuation`).
    // Idempotency identity, source lineage, launch metadata, and initial events
    // remain explicit at this atomic admission boundary.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_or_get_continuation_admitted(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        request_fingerprint: &str,
        launch_metadata: Option<&crate::launch_metadata::RuntimeLaunchMetadata>,
        initial_events: Vec<NewEventRecord>,
    ) -> Result<ContinuationOutcome> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        validate_facet_event_admission(&g, &successor.thread_id, &initial_events)?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("continuation creation is closed for daemon shutdown");
        }
        let source_row = g
            .state_db
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {source_thread_id}"))?;

        // The caller-supplied chain root must match the source's — a continuation
        // stays in the predecessor's chain. Fail loud on a mismatch rather than
        // braiding a successor into the wrong chain.
        if chain_root_id != source_row.chain_root_id {
            bail!(
                "chain_root_id mismatch: caller passed '{chain_root_id}' but source \
                 '{source_thread_id}' belongs to chain '{}'",
                source_row.chain_root_id
            );
        }

        if is_terminal_status(&source_row.status)
            && source_row.status != ThreadStatus::Failed.as_str()
            && source_row.status != ThreadStatus::Completed.as_str()
        {
            bail!(
                "cannot continue thread in terminal status '{}'",
                source_row.status
            );
        }

        // Idempotent get-or-conflict: a source already continued is deduped by
        // fingerprint instead of failing the single-successor guard.
        if let Some(existing) =
            queries::continuation_successor(g.state_db.projection(), source_thread_id)?
        {
            let existing_fp =
                queries::continuation_fingerprint(g.state_db.projection(), source_thread_id)?;
            return Ok(if existing_fp.as_deref() == Some(request_fingerprint) {
                ContinuationOutcome::Existing {
                    successor_thread_id: existing,
                }
            } else {
                ContinuationOutcome::Conflict {
                    successor_thread_id: existing,
                }
            });
        }

        // No successor yet — create it, persisting the fingerprint on the edge.
        // (Body mirrors `create_continuation`.) A terminal source keeps its
        // snapshot; a running source is settled `continued`.
        let source_snapshot_updates = if is_terminal_status(&source_row.status) {
            Vec::new()
        } else {
            let now = lillux::time::iso8601_now();
            let source_snapshot = continued_snapshot_for_transition(&g, &source_row, &now)?;
            vec![SnapshotUpdate {
                thread_id: source_thread_id.to_string(),
                new_snapshot: source_snapshot,
            }]
        };

        let mut successor_with_upstream = successor.clone();
        if successor_with_upstream.upstream_thread_id.is_none() {
            successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());
        }
        let effective_launch_metadata = launch_metadata
            .map(|metadata| {
                let mut metadata = metadata.clone();
                if metadata
                    .continuation_source_thread_id
                    .as_deref()
                    .is_some_and(|source| source != source_thread_id)
                {
                    bail!("prepared successor names a different continuation source");
                }
                metadata.continuation_source_thread_id = Some(source_thread_id.to_string());
                Ok(metadata)
            })
            .transpose()?;
        let base_successor_snapshot = match effective_launch_metadata
            .as_ref()
            .and_then(|metadata| metadata.resume_context.as_ref())
        {
            Some(resume) => build_continuation_snapshot(&successor_with_upstream, resume)?,
            None => build_snapshot(&successor_with_upstream),
        };
        let successor_snapshot = attach_continuation_launch_capsule(
            &self.state_authority,
            &g,
            chain_root_id,
            source_thread_id,
            base_successor_snapshot,
            effective_launch_metadata.as_ref(),
        )?;
        // Seed runtime state before the atomic signed-head transition. Failure
        // leaves at most an auxiliary runtime row, which is removed below; the
        // successor snapshot and source edge are indivisible.
        {
            let _admission = g.state_db.authorize_runtime_pin(chain_root_id)?;
            g.runtime_db
                .insert_thread_runtime(&successor.thread_id, chain_root_id)?;

            // Seed the operator launch context before the successor is visible.
            if let Some(meta) = effective_launch_metadata.as_ref() {
                if let Err(error) = g.runtime_db.set_launch_metadata(&successor.thread_id, meta) {
                    let _ = g.runtime_db.delete_thread_runtime(&successor.thread_id);
                    return Err(error);
                }
            }
        }

        let successor_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &successor.kind,
                "item_ref": &successor.item_ref,
                "continuation_from": source_thread_id,
            }),
        };
        let mut successor_events = Vec::with_capacity(initial_events.len() + 1);
        successor_events.push(successor_event);
        successor_events.extend(initial_events);
        let successor_thread_events =
            convert_events(&successor_events, chain_root_id, &successor.thread_id);
        let source_event = NewEventRecord {
            event_type: "thread_continued".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "successor_thread_id": &successor.thread_id,
                "reason": reason,
                "successor_request_fingerprint": request_fingerprint,
            }),
        };
        let ste = convert_events(
            std::slice::from_ref(&source_event),
            chain_root_id,
            source_thread_id,
        );
        let successor_commit = g.state_db.add_thread_with_events_and_append_admitted(
            chain_root_id,
            successor_snapshot,
            successor_thread_events,
            source_thread_id,
            ste,
            source_snapshot_updates,
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        );
        let successor_result = match successor_commit {
            Ok(committed) => committed_value(committed),
            Err(error) => {
                if let Err(cleanup_error) = g.runtime_db.delete_thread_runtime(&successor.thread_id)
                {
                    tracing::error!(
                        thread_id = %successor.thread_id,
                        error = %cleanup_error,
                        "failed to remove runtime row after atomic operator continuation birth failed"
                    );
                }
                return Err(error);
            }
        };
        let mut all_input_events = successor_events;
        all_input_events.push(source_event);
        Ok(ContinuationOutcome::Created(
            persisted_from_add_thread_with_events(&successor_result, &all_input_events)?,
        ))
    }

    /// Raw idempotent continuation fixture for state-layer tests. Absent in
    /// production.
    #[cfg(any(test, feature = "test-support"))]
    pub fn create_or_get_continuation_for_test(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
        request_fingerprint: &str,
        resume_context: Option<&crate::launch_metadata::ResumeContext>,
    ) -> Result<ContinuationOutcome> {
        let launch_metadata = resume_context.cloned().map(|resume_context| {
            crate::launch_metadata::RuntimeLaunchMetadata::default()
                .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::ManagedRuntime)
                .with_resume_context(resume_context)
        });
        self.create_or_get_continuation_admitted(
            successor,
            source_thread_id,
            chain_root_id,
            reason,
            request_fingerprint,
            launch_metadata.as_ref(),
            Vec::new(),
        )
    }

    /// The `successor_request_fingerprint` recorded on a source's
    /// `thread_continued` edge, if any — used to dedup operator double-submits.
    pub fn get_continuation_fingerprint(&self, thread_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        queries::continuation_fingerprint(g.state_db.projection(), thread_id)
    }

    /// Whether `source_thread_id`'s continuation edge is a follow-resume edge
    /// pointing at `successor_thread_id`. Such a successor has the same shape as a
    /// stranded machine continuation but must NOT be auto-launched — it waits for
    /// the followed child's result. Target-aware: another created row that merely
    /// names the same upstream is NOT matched.
    pub fn is_follow_resume_successor(
        &self,
        source_thread_id: &str,
        successor_thread_id: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        Ok(matches!(
            queries::continuation_edge(g.state_db.projection(), source_thread_id)?,
            Some((succ, Some(reason), _))
                if succ == successor_thread_id
                    && reason == queries::ContinuationReasonMarker::GraphFollowResume.as_str()
        ))
    }

    /// Reserve a durable, owner-bound planning handle before any launch task is
    /// spawned. The reserved thread id remains internal until authoritative
    /// thread publication binds it.
    pub fn reserve_launch_planning(
        &self,
        reserved_thread_id: &str,
        requested_by: &str,
    ) -> std::result::Result<String, LaunchPlanningReservationError> {
        let _permit = self
            .acquire_write_permit()
            .map_err(LaunchPlanningReservationError::Internal)?;
        let g = self
            .lock()
            .map_err(LaunchPlanningReservationError::Internal)?;
        let launch_id = format!("L-{}", uuid::Uuid::new_v4().simple());
        g.runtime_db
            .reserve_launch_planning(&launch_id, reserved_thread_id, requested_by)
            .map_err(map_launch_planning_reservation_error)?;
        Ok(launch_id)
    }

    pub fn ensure_launch_planning_active(&self, reserved_thread_id: &str) -> Result<()> {
        let g = self.lock()?;
        let Some(record) = g.runtime_db.launch_planning_by_thread(reserved_thread_id)? else {
            return Ok(());
        };
        if record.state != "planning"
            || record.daemon_generation_id != runtime_db::daemon_generation_id()
        {
            return Err(LaunchPlanningInactive.into());
        }
        Ok(())
    }

    pub fn register_launch_task_abort(
        &self,
        reserved_thread_id: &str,
        abort_handle: tokio::task::AbortHandle,
    ) -> std::result::Result<(), LaunchTaskAbortRegistrationError> {
        self.register_launch_task_abort_bounded(
            reserved_thread_id,
            abort_handle,
            MAX_ACTIVE_LAUNCH_SIGNALS,
        )
    }

    fn register_launch_task_abort_bounded(
        &self,
        reserved_thread_id: &str,
        abort_handle: tokio::task::AbortHandle,
        max_active_signals: usize,
    ) -> std::result::Result<(), LaunchTaskAbortRegistrationError> {
        let g = self.lock()?;
        let planning = g.runtime_db.launch_planning_by_thread(reserved_thread_id)?;
        let Some(record) = planning else {
            return Ok(());
        };
        if record.state == "bound" {
            return Ok(());
        }
        if record.state != "planning"
            || record.daemon_generation_id != runtime_db::daemon_generation_id()
        {
            abort_handle.abort();
            return Ok(());
        }
        let launch_id = record.launch_id;
        let mut handles = self
            .launch_task_abort_handles
            .lock()
            .map_err(|_| anyhow!("launch task abort registry lock poisoned"))?;
        if handles.len() >= max_active_signals && !handles.contains_key(&launch_id) {
            abort_handle.abort();
            return Err(LaunchTaskAbortRegistrationError::CapacityExceeded);
        }
        if let Some(previous) = handles.insert(launch_id, abort_handle) {
            previous.abort();
        }
        drop(handles);
        drop(g);
        Ok(())
    }

    pub fn unregister_launch_task_abort(&self, reserved_thread_id: &str) -> Result<()> {
        let launch_id = {
            let g = self.lock()?;
            g.runtime_db
                .launch_planning_by_thread(reserved_thread_id)?
                .map(|record| record.launch_id)
        };
        if let Some(launch_id) = launch_id {
            self.launch_task_abort_handles
                .lock()
                .map_err(|_| anyhow!("launch task abort registry lock poisoned"))?
                .remove(&launch_id);
        }
        Ok(())
    }

    /// Settle a pre-bind task exit, including unwind/abort paths. If the
    /// authoritative row committed first, repair the planning record to bound;
    /// otherwise mark it terminal failed. Already-terminal records are a no-op.
    pub fn settle_launch_planning_task_exit(&self, reserved_thread_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some(record) = g.runtime_db.launch_planning_by_thread(reserved_thread_id)? else {
            return Ok(());
        };
        if record.state != "planning" {
            return Ok(());
        }
        if g.state_db.get_thread(reserved_thread_id)?.is_some() {
            g.runtime_db.bind_launch_planning(reserved_thread_id)?;
        } else {
            g.runtime_db.fail_launch_planning(reserved_thread_id)?;
        }
        Ok(())
    }

    /// Resolve cancel-vs-bind while holding the same store mutex used by root
    /// and continuation publication. A committed authoritative row always wins
    /// and is repaired to `bound` before the caller delegates to normal durable
    /// thread cancel.
    pub fn cancel_launch_planning(
        &self,
        launch_id: &str,
        requested_by: &str,
    ) -> Result<Option<LaunchCancellationResolution>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let Some(record) = g.runtime_db.launch_planning_by_id(launch_id)? else {
            return Ok(None);
        };
        if record.requested_by != requested_by {
            return Ok(None);
        }
        if record.state == "planning" {
            if g.state_db.get_thread(&record.reserved_thread_id)?.is_some() {
                g.runtime_db
                    .bind_launch_planning(&record.reserved_thread_id)?;
                self.launch_task_abort_handles
                    .lock()
                    .map_err(|_| anyhow!("launch task abort registry lock poisoned"))?
                    .remove(launch_id);
                return Ok(Some(LaunchCancellationResolution::Bound {
                    thread_id: record.reserved_thread_id,
                }));
            }
            if g.runtime_db.cancel_unbound_launch_planning(launch_id)? {
                drop(g);
                if let Some(abort_handle) = self
                    .launch_task_abort_handles
                    .lock()
                    .map_err(|_| anyhow!("launch task abort registry lock poisoned"))?
                    .remove(launch_id)
                {
                    abort_handle.abort();
                }
                return Ok(Some(LaunchCancellationResolution::Cancelled));
            }
        }
        if record.state == "bound" {
            let thread_id = record.bound_thread_id.ok_or_else(|| {
                anyhow!(
                    "bound launch planning record `{launch_id}` has no authoritative thread binding"
                )
            })?;
            if thread_id != record.reserved_thread_id {
                bail!(
                    "bound launch planning record `{launch_id}` has divergent reserved and authoritative thread identities"
                );
            }
            return Ok(Some(LaunchCancellationResolution::Bound { thread_id }));
        }
        Ok(Some(LaunchCancellationResolution::Terminal {
            state: record.state,
            outcome_code: record.outcome_code,
        }))
    }

    pub fn reconcile_launch_planning(&self) -> Result<usize> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let mut repaired = 0usize;
        for record in g.runtime_db.pending_launch_planning()? {
            if g.state_db.get_thread(&record.reserved_thread_id)?.is_some()
                && g.runtime_db
                    .bind_launch_planning(&record.reserved_thread_id)?
            {
                repaired += 1;
            }
        }
        repaired += g.runtime_db.expire_stale_launch_planning()?;
        Ok(repaired)
    }

    pub fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadDetail>> {
        let g = self.lock()?;
        let thread_row = match g.state_db.get_thread(thread_id)? {
            Some(row) => row,
            None => return Ok(None),
        };

        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .unwrap_or_default();
        let lifecycle_authority = runtime
            .launch_metadata
            .as_ref()
            .and_then(|metadata| metadata.resume_context.as_ref())
            .map(|resume| resume.lifecycle_authority);
        let project_authority = g
            .state_db
            .read_authoritative_thread_snapshot(&thread_row.chain_root_id, thread_id)?
            .map(|snapshot| snapshot.project_authority);

        let successor_thread_id = if is_terminal_status(&thread_row.status) {
            queries::continuation_successor(g.state_db.projection(), thread_id)?
        } else {
            None
        };

        Ok(Some(ThreadDetail {
            thread_id: thread_row.thread_id,
            chain_root_id: thread_row.chain_root_id,
            kind: thread_row.kind,
            status: thread_row.status,
            item_ref: thread_row.item_ref,
            executor_ref: thread_row.executor_ref,
            launch_mode: thread_row.launch_mode,
            current_site_id: thread_row.current_site_id,
            origin_site_id: thread_row.origin_site_id,
            upstream_thread_id: thread_row.upstream_thread_id,
            successor_thread_id,
            requested_by: thread_row.requested_by,
            project_root: thread_row.project_root,
            project_authority,
            lifecycle_authority,
            admitted_launch_capsule_hash: thread_row.admitted_launch_capsule_hash,
            created_at: thread_row.created_at,
            updated_at: thread_row.updated_at,
            started_at: thread_row.started_at,
            finished_at: thread_row.finished_at,
            runtime,
        }))
    }

    /// Read the immutable generation pinned by authoritative signed history.
    /// Runtime metadata and projection paths are never consulted.
    pub fn authoritative_project_generation(
        &self,
        thread_id: &str,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        let g = self.lock()?;
        let Some(row) = g.state_db.get_thread(thread_id)? else {
            return Ok(None);
        };
        let snapshot =
            authoritative_snapshot_for_transition(&g, &row.chain_root_id, &row.thread_id)?;
        Ok(Some((
            snapshot.base_project_snapshot_hash,
            snapshot.result_project_snapshot_hash,
        )))
    }

    /// Read a newly-created thread from signed CAS authority. This is used
    /// immediately after a continuation commit, when projection repair may be
    /// pending even though the successor is already authoritative.
    pub(crate) fn get_created_thread_authoritatively(
        &self,
        chain_root_id: &str,
        thread_id: &str,
    ) -> Result<Option<ThreadDetail>> {
        let g = self.lock()?;
        let Some(snapshot) = g
            .state_db
            .read_authoritative_thread_snapshot(chain_root_id, thread_id)?
        else {
            return Ok(None);
        };
        if snapshot.status != ThreadStatus::Created {
            bail!(
                "authoritative continuation successor {thread_id} has status '{}', expected created",
                snapshot.status
            );
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow!("continuation successor {thread_id} is missing runtime state")
        })?;
        Ok(Some(thread_detail_from_created_snapshot(snapshot, runtime)))
    }

    pub fn touch_seat_lease(
        &self,
        thread_id: &str,
        owner: &str,
        surface: &str,
        client_ref: &str,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("seat thread {thread_id} does not exist"))?;
        if thread.kind != "seat_session"
            || thread.status != ThreadStatus::Running.as_str()
            || thread.requested_by.as_deref() != Some(owner)
            || thread.item_ref != surface
        {
            bail!("seat thread {thread_id} is not a matching running owned seat");
        }
        let _admission = g.state_db.authorize_runtime_pin(&thread.chain_root_id)?;
        if !g
            .runtime_db
            .touch_seat_lease(thread_id, owner, surface, client_ref)?
        {
            bail!("seat thread {thread_id} is being reaped");
        }
        Ok(())
    }

    pub fn touch_existing_seat_lease(&self, thread_id: &str) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.touch_existing_seat_lease(thread_id)
    }

    pub fn remove_seat_lease(&self, thread_id: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.remove_seat_lease(thread_id)
    }

    pub fn expired_seat_leases(&self, cutoff_ms: i64) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.expired_seat_leases(cutoff_ms)
    }

    pub fn claim_expired_seat_lease(&self, thread_id: &str, cutoff_ms: i64) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.claim_expired_seat_lease(thread_id, cutoff_ms)
    }

    fn inspect_terminal_chain_pins<F>(
        g: &Inner,
        chain: &ryeos_state::AuthoritativeTerminalChain,
        scheduler_pin_count: &F,
    ) -> Result<runtime_db::ChainRecoveryPins>
    where
        F: Fn(&[String]) -> Result<u64>,
    {
        let mut pins = g
            .runtime_db
            .inspect_chain_recovery_pins(&chain.chain_root_id, &chain.thread_ids)?;
        let members = chain
            .thread_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        for (parent_thread_id, child_thread_id) in
            g.runtime_db.chain_child_links(&chain.thread_ids)?
        {
            let parent_is_member = members.contains(parent_thread_id.as_str());
            let child_is_member = members.contains(child_thread_id.as_str());
            if parent_is_member && child_is_member {
                continue;
            }
            let counterpart = if parent_is_member {
                &child_thread_id
            } else {
                &parent_thread_id
            };
            // A missing counterpart while its operational edge remains is an
            // inconsistent read, not proof of safety. Fail closed by retaining
            // the chain. A terminal counterpart no longer needs cancellation
            // cascade state and therefore does not pin this history.
            if g.state_db
                .get_thread(counterpart)?
                .is_none_or(|thread| !is_terminal_status(&thread.status))
            {
                pins.child_links = pins
                    .child_links
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("child-link recovery pin count overflow"))?;
            }
        }
        pins.scheduler_fires = scheduler_pin_count(&chain.thread_ids)?;
        Ok(pins)
    }

    fn finish_terminal_chain_removal(
        g: &Inner,
        chain: &ryeos_state::AuthoritativeTerminalChain,
        chain_lock: &ChainLock,
        runtime_paths: &[ThreadRuntimeRemoval],
        head_already_absent: bool,
        result: &mut TerminalChainRetirement,
    ) -> Result<()> {
        if !head_already_absent
            && !g
                .state_db
                .remove_chain_head_ref(&chain.chain_root_id, chain_lock)?
        {
            bail!(
                "authoritative chain head disappeared during retirement: {}",
                chain.chain_root_id
            );
        }
        result.deleted_runtime_rows += g
            .runtime_db
            .delete_chain_runtime(&chain.chain_root_id, &chain.thread_ids)?;
        result.deleted_runtime_files += delete_thread_runtime_files(runtime_paths)?;
        result.deleted_projection_rows += g
            .state_db
            .delete_chain_projection(&chain.chain_root_id, chain_lock)?;
        if !g
            .state_db
            .acknowledge_chain_removal_cleanup(&chain.chain_root_id, chain_lock)?
        {
            bail!(
                "pending chain removal disappeared before acknowledgement: {}",
                chain.chain_root_id
            );
        }
        Ok(())
    }

    fn recover_pending_terminal_chain_removals_with<F>(
        &self,
        now: &str,
        dry_run: bool,
        scheduler_pin_count: &F,
    ) -> Result<TerminalChainRetirement>
    where
        F: Fn(&[String]) -> Result<u64>,
    {
        let mut result = TerminalChainRetirement::default();
        let mut pending_removals = {
            let g = self.lock()?;
            g.state_db.pending_remove_cursor()?
        };
        while let Some(observed) = pending_removals.next_transition()? {
            let chain_root_id = observed.chain_root_id.clone();
            let _permit = self.acquire_recovery_cleanup_permit(dry_run)?;
            let g = self.lock()?;
            let chain_lock = if dry_run {
                g.state_db.acquire_existing_chain_lock(&chain_root_id)?
            } else {
                g.state_db.acquire_chain_lock(&chain_root_id)?
            };
            let still_pending = g
                .state_db
                .pending_chain_transition(&chain_root_id)?
                .is_some_and(|pending| {
                    pending.transition_id == observed.transition_id
                        && pending.operation == ryeos_state::HeadOperation::Remove
                });
            if !still_pending {
                continue;
            }

            match g.state_db.pending_chain_removal_head_state(
                &chain_root_id,
                &observed.transition_id,
                &chain_lock,
            )? {
                ryeos_state::PendingRemoveHeadState::HeadAbsent => {
                    let chain = g
                        .state_db
                        .pending_removed_terminal_chain_under_lock(&chain_root_id, &chain_lock)?
                        .ok_or_else(|| {
                            anyhow!(
                                "pending Remove for {chain_root_id} names a nonterminal or invalid closure"
                            )
                        })?;
                    // The absent trusted head proves that this Remove crossed
                    // its authoritative deletion boundary. Operational rows,
                    // scheduler fires, follow state, and runtime files left by
                    // a crash at that boundary are cleanup targets, not fresh
                    // vetoes: new pin admission is already forbidden once the
                    // Remove publishes. Rechecking them here would strand the
                    // exact partial cleanup this replay owns forever.
                    let runtime_paths = inspect_thread_runtime_files(
                        self.thread_runtime_authority()?,
                        &chain.thread_ids,
                    )?;
                    result.pending_retirements_recovered += 1;
                    if !dry_run {
                        Self::finish_terminal_chain_removal(
                            &g,
                            &chain,
                            &chain_lock,
                            &runtime_paths,
                            true,
                            &mut result,
                        )?;
                    }
                    continue;
                }
                ryeos_state::PendingRemoveHeadState::AdvancedHeadVisible { current_head_hash } => {
                    result.pending_retirements_recovered += 1;
                    tracing::warn!(
                        chain_root_id,
                        current_head_hash,
                        "pending terminal-history Remove is stale against an advanced signed head; repairing the current head"
                    );
                    if dry_run {
                        g.state_db.verify_advanced_head_after_stale_chain_removal(
                            &chain_root_id,
                            &observed.transition_id,
                            &current_head_hash,
                            &chain_lock,
                        )?;
                    } else {
                        g.state_db.repair_advanced_head_after_stale_chain_removal(
                            &chain_root_id,
                            &observed.transition_id,
                            &chain_lock,
                        )?;
                    }
                    continue;
                }
                ryeos_state::PendingRemoveHeadState::ExpectedHeadVisible => {}
            }

            let Some(chain) = g
                .state_db
                .authoritative_terminal_chain_under_lock(&chain_root_id, &chain_lock)?
            else {
                result.pending_retirements_recovered += 1;
                if !dry_run {
                    g.state_db
                        .cancel_pending_chain_removal(&chain_root_id, &chain_lock)?;
                }
                continue;
            };
            let due = chain.is_due_at(now)?;
            let pins = Self::inspect_terminal_chain_pins(&g, &chain, scheduler_pin_count)?;
            if !due || !pins.is_empty() {
                result.pending_retirements_recovered += 1;
                if !dry_run {
                    g.state_db
                        .cancel_pending_chain_removal(&chain_root_id, &chain_lock)?;
                }
                continue;
            }
            let runtime_paths =
                inspect_thread_runtime_files(self.thread_runtime_authority()?, &chain.thread_ids)?;
            result.pending_retirements_recovered += 1;
            result.retired_chains += 1;
            if !dry_run {
                Self::finish_terminal_chain_removal(
                    &g,
                    &chain,
                    &chain_lock,
                    &runtime_paths,
                    false,
                    &mut result,
                )?;
            }
        }
        Ok(result)
    }

    /// Complete or cancel only the bounded durable Remove records left by an
    /// interrupted retention transaction. Startup calls this before readiness;
    /// it never scans current chain heads or selects new GC candidates.
    pub fn recover_pending_terminal_chain_removals<F>(
        &self,
        now: &str,
        dry_run: bool,
        scheduler_pin_count: F,
    ) -> Result<TerminalChainRetirement>
    where
        F: Fn(&[String]) -> Result<u64>,
    {
        let result =
            self.recover_pending_terminal_chain_removals_with(now, dry_run, &scheduler_pin_count);
        match self
            .lock()
            .and_then(|g| Ok(g.state_db.pending_chain_transitions()?.len()))
        {
            Ok(pending) => self.projection_health.observe_pending_transitions(pending),
            Err(error) => self
                .projection_health
                .observe_pending_transition_error(&error),
        }
        result
    }

    /// Retire terminal chain history solely from the signed policy captured on
    /// its root. Candidate rows are a bounded read-model optimization; every
    /// destructive decision is repeated from the trust-verified CAS closure
    /// under the global guard/permit/mutex/chain-lock hierarchy.
    pub fn retire_due_terminal_chains<F>(
        &self,
        now: &str,
        dry_run: bool,
        scheduler_pin_count: F,
    ) -> Result<TerminalChainRetirement>
    where
        F: Fn(&[String]) -> Result<u64>,
    {
        const CANDIDATE_BATCH: usize = 128;
        let mut result =
            self.recover_pending_terminal_chain_removals_with(now, dry_run, &scheduler_pin_count)?;
        let mut cursor: Option<ryeos_state::DueTerminalChainCursor> = None;
        loop {
            let candidates = {
                let g = self.lock()?;
                g.state_db
                    .list_due_terminal_chains(now, CANDIDATE_BATCH, cursor.as_ref())?
            };
            if candidates.is_empty() {
                break;
            }
            result.candidate_chains += candidates.len();
            cursor = candidates
                .last()
                .map(|candidate| ryeos_state::DueTerminalChainCursor {
                    retire_after: candidate.retire_after,
                    chain_root_id: candidate.chain_root_id.clone(),
                });

            for candidate in &candidates {
                let _permit = if dry_run {
                    self.acquire_gc_inspection_permit()?
                } else {
                    self.acquire_write_permit()?
                };
                let g = self.lock()?;
                let chain_lock = if dry_run {
                    g.state_db
                        .acquire_existing_chain_lock(&candidate.chain_root_id)?
                } else {
                    g.state_db.acquire_chain_lock(&candidate.chain_root_id)?
                };
                // Any unresolved transition owns the chain's publication
                // slot. In particular, a Prepared Set may still name a newer
                // closure even while the old head/projection match this stale
                // candidate row; retirement must never try to converge or
                // overwrite that publication intent.
                if g.state_db
                    .pending_chain_transition(&candidate.chain_root_id)?
                    .is_some()
                {
                    continue;
                }
                let Some(chain) = g.state_db.authoritative_terminal_chain_under_lock(
                    &candidate.chain_root_id,
                    &chain_lock,
                )?
                else {
                    continue;
                };
                if chain.head_hash != candidate.indexed_chain_state_hash
                    || chain.terminal_at != candidate.terminal_at
                    || !chain.is_due_at(now)?
                {
                    continue;
                }
                let pins = Self::inspect_terminal_chain_pins(&g, &chain, &scheduler_pin_count)?;
                if !pins.is_empty() {
                    continue;
                }
                let runtime_paths = inspect_thread_runtime_files(
                    self.thread_runtime_authority()?,
                    &chain.thread_ids,
                )?;
                result.retired_chains += 1;
                if !dry_run {
                    Self::finish_terminal_chain_removal(
                        &g,
                        &chain,
                        &chain_lock,
                        &runtime_paths,
                        false,
                        &mut result,
                    )?;
                }
            }
            if candidates.len() < CANDIDATE_BATCH {
                break;
            }
        }
        Ok(result)
    }

    pub fn get_thread_result(&self, thread_id: &str) -> Result<Option<ThreadResultRecord>> {
        let result_row = {
            let g = self.lock()?;
            queries::get_thread_result(g.state_db.projection(), thread_id)?
        };
        let Some(row) = result_row else {
            return Ok(None);
        };
        // JSON parsing and exact response serialization happen after releasing
        // the global store mutex. The query has already bounded both source
        // columns before allocating them.
        let result = match row.result {
            Some(bytes) => Some(serde_json::from_slice::<Value>(&bytes).with_context(|| {
                format!(
                    "malformed JSON in thread_results.result for thread_id {}",
                    thread_id
                )
            })?),
            None => None,
        };
        let record = ThreadResultRecord {
            outcome_code: row.outcome_code,
            result,
            error: row
                .error
                .map(|error| serde_json::from_str::<Value>(&error).unwrap_or(Value::String(error))),
            metadata: None,
        };
        let response_bytes = serde_json::to_vec(&record)?.len();
        if response_bytes > MAX_THREAD_RESULT_RESPONSE_BYTES {
            bail!(
                "thread {thread_id} result response is {response_bytes} bytes; maximum is {MAX_THREAD_RESULT_RESPONSE_BYTES}"
            );
        }
        Ok(Some(record))
    }

    pub fn get_thread_terminal_authority(
        &self,
        thread_id: &str,
    ) -> Result<Option<ThreadTerminalAuthority>> {
        let g = self.lock()?;
        let Some(thread) = g.state_db.get_thread(thread_id)? else {
            return Ok(None);
        };
        let Some(status) = ThreadStatus::from_str_lossy(&thread.status) else {
            bail!("thread {thread_id} has unknown status `{}`", thread.status);
        };
        if !status.is_terminal() {
            return Ok(None);
        }

        let snapshot = g
            .state_db
            .read_authoritative_thread_snapshot(&thread.chain_root_id, thread_id)?
            .ok_or_else(|| anyhow!("terminal thread {thread_id} is missing its CAS snapshot"))?;
        let ThreadSnapshot {
            status: snapshot_status,
            result,
            error,
            budget,
            facets,
            ..
        } = snapshot;
        if snapshot_status != status {
            bail!(
                "terminal thread {thread_id} projection status `{status}` contradicts CAS status `{}`",
                snapshot_status
            );
        }

        let final_cost = budget
            .map(|usage| {
                let metadata = facets
                    .get("cost.metadata_json")
                    .map(|raw| serde_json::from_str(raw))
                    .transpose()
                    .context("decode authoritative final cost metadata")?;
                Ok::<_, anyhow::Error>(ryeos_engine::contracts::FinalCost {
                    turns: usage.completed_turns,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    spend: usage.spend_usd,
                    provider: facets.get("cost.provider").cloned(),
                    basis: facets.get("cost.basis").cloned(),
                    metadata,
                })
            })
            .transpose()?;
        let managed_envelope = facets
            .get("runtime.terminal_envelope_json")
            .map(|raw| serde_json::from_str(raw))
            .transpose()
            .context("decode authoritative managed runtime terminal envelope")?;

        Ok(Some(ThreadTerminalAuthority {
            status,
            result,
            error,
            final_cost,
            managed_envelope,
        }))
    }

    pub fn list_thread_artifacts(&self, thread_id: &str) -> Result<Vec<ThreadArtifactRecord>> {
        let artifact_rows = {
            let g = self.lock()?;
            // Aggregate/count-only preflight happens before the guarded SELECT,
            // so no collection BLOB is copied until the whole collection fits.
            ensure_artifact_projection_capacity(&g, thread_id, 0, 0, 0)?;
            queries::list_thread_artifacts_bounded(
                g.state_db.projection(),
                thread_id,
                MAX_THREAD_ARTIFACT_ITEMS,
                MAX_THREAD_ARTIFACT_TYPE_BYTES,
                MAX_THREAD_ARTIFACT_METADATA_BYTES,
                MAX_THREAD_ARTIFACT_METADATA_TOTAL_BYTES,
            )?
        };
        let mut records = Vec::with_capacity(artifact_rows.len());
        let mut response_bytes = b"[]".len();
        for (idx, row) in artifact_rows.into_iter().enumerate() {
            let metadata = match row.metadata {
                Some(bytes) => {
                    Some(serde_json::from_slice::<Value>(&bytes).with_context(|| {
                        format!(
                            "malformed JSON in thread_artifacts.metadata \
                             for artifact at index {idx} of thread_id {}",
                            thread_id
                        )
                    })?)
                }
                None => None,
            };
            let record = ThreadArtifactRecord {
                artifact_id: idx as i64 + 1,
                artifact_type: row.kind,
                uri: String::new(),
                content_hash: None,
                metadata,
            };
            let encoded = serde_json::to_vec(&record)?;
            response_bytes = response_bytes
                .checked_add(encoded.len())
                .and_then(|bytes| bytes.checked_add(usize::from(!records.is_empty())))
                .ok_or_else(|| anyhow!("thread artifact response size overflow"))?;
            if response_bytes > MAX_THREAD_ARTIFACT_RESPONSE_BYTES {
                bail!(
                    "thread {thread_id} artifact response exceeds the {MAX_THREAD_ARTIFACT_RESPONSE_BYTES}-byte maximum"
                );
            }
            records.push(record);
        }
        Ok(records)
    }

    #[tracing::instrument(
        name = "state:publish_artifact",
        skip(self, artifact),
        fields(thread_id = %thread_id, artifact_type = %artifact.artifact_type)
    )]
    pub fn publish_artifact(
        &self,
        thread_id: &str,
        artifact: &NewArtifactRecord,
    ) -> Result<(ThreadArtifactRecord, PersistedEventRecord)> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;

        // Artifact projection is derived from the artifact_published event
        // during project_event (see projection.rs). No direct write needed.

        let event = NewEventRecord {
            event_type: "artifact_published".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "artifact_type": artifact.artifact_type,
                "uri": artifact.uri,
                "content_hash": artifact.content_hash,
                "metadata": artifact.metadata,
            }),
        };
        if !self.projection_health.is_current() {
            bail!("artifact admission requires a current thread projection");
        }
        validate_artifact_event_admission(&g, thread_id, std::slice::from_ref(&event))?;

        let te = convert_events(
            std::slice::from_ref(&event),
            &thread_row.chain_root_id,
            thread_id,
        );
        let result = committed_value(g.state_db.append_events_admitted(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![],
            g.signer.as_ref(),
            &g.runtime_db,
            permit.cas_guard(),
        )?);

        let persisted = persisted_from_append(&result, &[event])?;

        let persisted_event = persisted.into_iter().next().ok_or_else(|| {
            anyhow!("artifact_published event was not persisted for thread {thread_id}")
        })?;

        let artifact_id = persisted_event.event_id;

        let record = ThreadArtifactRecord {
            artifact_id,
            artifact_type: artifact.artifact_type.clone(),
            uri: artifact.uri.clone(),
            content_hash: artifact.content_hash.clone(),
            metadata: artifact.metadata.clone(),
        };

        Ok((record, persisted_event))
    }

    pub fn list_threads(&self, limit: usize) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows = queries::list_threads(g.state_db.projection(), limit)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// List threads with optional principal filtering.
    ///
    /// When `filter_principal` is `Some(fp)`, only threads with
    /// `requested_by = fp` are returned. `None` returns all threads
    /// (used by internal callers that intentionally request an unfiltered view).
    pub fn list_threads_filtered(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
    ) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows =
                queries::list_threads_filtered(g.state_db.projection(), limit, filter_principal)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// As [`Self::list_threads_filtered`] but with an explicit
    /// [`queries::ThreadSort`] — `Watch` orders active-before-terminal then
    /// newest for the operator dashboard, without changing the default order.
    pub fn list_threads_sorted(
        &self,
        limit: usize,
        filter_principal: Option<&str>,
        sort: queries::ThreadSort,
    ) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let rows = queries::list_threads_sorted(
                g.state_db.projection(),
                limit,
                filter_principal,
                sort,
            )?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// Chain-wide execution usage totals (tokens, cost, turns, thread count)
    /// for a `chain_root_id` — the deep-watch summary of an execution and its
    /// continuations.
    pub fn chain_usage_totals(&self, chain_root_id: &str) -> Result<queries::ThreadUsageTotals> {
        let g = self.lock()?;
        queries::sum_thread_usage_latest_by_chain(g.state_db.projection(), chain_root_id)
    }

    /// Node-wide usage settled since `since_iso` (inclusive), continuation-
    /// deduped — what cognition on this node cost inside the window.
    pub fn node_usage_totals_since(&self, since_iso: &str) -> Result<queries::ThreadUsageTotals> {
        let g = self.lock()?;
        queries::sum_thread_usage_latest_since(g.state_db.projection(), since_iso)
    }

    /// Per-status thread counts for the node pulse: non-terminal statuses
    /// count unconditionally, terminal ones only inside the window.
    pub fn thread_status_counts(&self, since_iso: &str) -> Result<Vec<(String, i64)>> {
        let g = self.lock()?;
        queries::thread_status_counts(g.state_db.projection(), since_iso)
    }

    /// As [`Self::list_threads_sorted`] but with the full optional filter set
    /// (status / kind / requested_by) the operator dashboard narrows by.
    pub fn list_threads_query(
        &self,
        limit: usize,
        filter: &queries::ThreadListFilter,
        sort: queries::ThreadSort,
    ) -> Result<Vec<ThreadListItem>> {
        let (thread_rows, successor_payloads) = {
            let g = self.lock()?;
            let hold_started = std::time::Instant::now();
            let rows = queries::list_threads_query(g.state_db.projection(), limit, filter, sort)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            Self::warn_slow_lock_hold("list_threads_query", hold_started);
            (rows, payloads)
        };
        Self::rows_to_list_items(thread_rows, successor_payloads)
    }

    /// Bounded execution closure containing continuation and cross-chain spawn
    /// edges. The query resolves an arbitrary selected thread to its oldest
    /// reachable ancestor, so opening the tree from a child still shows its
    /// surrounding execution rather than an orphaned subtree.
    pub fn execution_tree(
        &self,
        selected_thread_id: &str,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<ExecutionTreePage> {
        let (mut tree_rows, successor_payloads) = {
            let g = self.lock()?;
            let hold_started = std::time::Instant::now();
            let rows = queries::execution_tree(
                g.state_db.projection(),
                selected_thread_id,
                max_depth,
                max_nodes.saturating_add(1),
            )?;
            let thread_rows = rows
                .iter()
                .take(max_nodes)
                .map(|row| row.thread.clone())
                .collect::<Vec<_>>();
            let payloads = Self::continuation_payloads_for_rows(&g, &thread_rows)?;
            Self::warn_slow_lock_hold("execution_tree", hold_started);
            (rows, payloads)
        };
        let node_truncated = tree_rows.len() > max_nodes;
        tree_rows.truncate(max_nodes);
        let depth_truncated = tree_rows
            .iter()
            .any(|row| row.depth >= max_depth && row.has_children);
        let thread_rows = tree_rows
            .iter()
            .map(|row| row.thread.clone())
            .collect::<Vec<_>>();
        let items = Self::rows_to_list_items(thread_rows, successor_payloads)?;
        let items = tree_rows
            .into_iter()
            .zip(items)
            .map(|(tree, item)| ExecutionTreeItem {
                item,
                tree_parent_thread_id: tree.tree_parent_thread_id,
                relation: tree.relation,
                depth: tree.depth,
                has_children: tree.has_children,
            })
            .collect();
        Ok(ExecutionTreePage {
            items,
            truncated: node_truncated || depth_truncated,
        })
    }

    /// Project thread rows into `ThreadListItem`s, resolving each terminal
    /// thread's continuation successor so the client can identify chain heads
    /// (a head has no successor). Shared by the filtered and unfiltered list
    /// paths.
    fn continuation_payloads_for_rows(
        g: &Inner,
        rows: &[queries::ThreadRow],
    ) -> Result<HashMap<String, Vec<u8>>> {
        let terminal_thread_ids = rows
            .iter()
            .filter(|row| is_terminal_status(&row.status))
            .map(|row| row.thread_id.clone())
            .collect::<Vec<_>>();
        queries::continuation_successor_payloads(
            g.state_db.projection(),
            &terminal_thread_ids,
            MAX_THREAD_LIST_ENRICHMENT_THREADS,
            MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES,
            MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES,
        )
    }

    fn rows_to_list_items(
        rows: Vec<queries::ThreadRow>,
        successor_payloads: HashMap<String, Vec<u8>>,
    ) -> Result<Vec<ThreadListItem>> {
        let mut successors = HashMap::new();
        for (thread_id, payload) in successor_payloads {
            let value: serde_json::Value = serde_json::from_slice(&payload)
                .context("parse thread_continued payload for thread-list enrichment")?;
            if let Some(successor) = value
                .get("successor_thread_id")
                .and_then(serde_json::Value::as_str)
            {
                successors.insert(thread_id, successor.to_string());
            }
        }
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let successor_thread_id = successors.remove(&row.thread_id);
            items.push(ThreadListItem {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                project_authority: Some(row.project_authority),
                // The high-frequency list remains projection-only. Lifecycle
                // authority is operational metadata; project authority is now
                // a signed-snapshot-derived projection column.
                lifecycle_authority: None,
                admitted_launch_capsule_hash: row.admitted_launch_capsule_hash,
                created_at: row.created_at,
                updated_at: row.updated_at,
            });
        }
        Ok(items)
    }

    /// Load the non-thread-row facts used to decorate one list page. Facets,
    /// current graph nodes, and live follow waiters share one outer store lock
    /// instead of relocking the store for every row.
    pub fn thread_list_enrichment(&self, thread_ids: &[String]) -> Result<ThreadListEnrichment> {
        let (facet_rows, graph_node_payloads, follow_waiters, terminal_error_previews) = {
            let g = self.lock()?;
            let hold_started = std::time::Instant::now();
            let result = (
                load_bounded_facets_many(&g, thread_ids)?,
                queries::current_graph_node_payloads(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES,
                )?,
                g.runtime_db.follow_waiter_summaries_for_threads(
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                )?,
                queries::thread_result_error_previews(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_ERROR_PREVIEW_BYTES,
                )?,
            );
            Self::warn_slow_lock_hold("thread_list_enrichment", hold_started);
            result
        };
        Self::assemble_thread_list_enrichment(
            facet_rows,
            graph_node_payloads,
            follow_waiters,
            terminal_error_previews,
        )
    }

    pub fn thread_list_enrichment_with_waiters(
        &self,
        thread_ids: &[String],
        follow_waiters: Vec<runtime_db::FollowWaiterSummary>,
    ) -> Result<ThreadListEnrichment> {
        let (facet_rows, graph_node_payloads, terminal_error_previews) = {
            let g = self.lock()?;
            let hold_started = std::time::Instant::now();
            let result = (
                load_bounded_facets_many(&g, thread_ids)?,
                queries::current_graph_node_payloads(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_BYTES,
                    MAX_THREAD_LIST_EVENT_PAYLOAD_TOTAL_BYTES,
                )?,
                queries::thread_result_error_previews(
                    g.state_db.projection(),
                    thread_ids,
                    MAX_THREAD_LIST_ENRICHMENT_THREADS,
                    MAX_THREAD_LIST_ERROR_PREVIEW_BYTES,
                )?,
            );
            Self::warn_slow_lock_hold("thread_list_enrichment_with_waiters", hold_started);
            result
        };
        Self::assemble_thread_list_enrichment(
            facet_rows,
            graph_node_payloads,
            follow_waiters,
            terminal_error_previews,
        )
    }

    fn assemble_thread_list_enrichment(
        facet_rows: Vec<queries::FacetRow>,
        graph_node_payloads: HashMap<String, Vec<u8>>,
        follow_waiters: Vec<runtime_db::FollowWaiterSummary>,
        terminal_error_previews: HashMap<String, String>,
    ) -> Result<ThreadListEnrichment> {
        let mut facets: HashMap<String, Vec<(String, String)>> = HashMap::new();
        for row in facet_rows {
            let value =
                String::from_utf8(row.value).context("thread-list facet value is not UTF-8")?;
            facets
                .entry(row.thread_id)
                .or_default()
                .push((row.key, value));
        }
        let mut current_graph_nodes = HashMap::new();
        for (thread_id, payload) in graph_node_payloads {
            let payload: serde_json::Value = match serde_json::from_slice(&payload) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!(
                        %thread_id,
                        %error,
                        "ignoring malformed graph_step_started payload in thread-list enrichment"
                    );
                    continue;
                }
            };
            let Some(node) = payload.get("node").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let step = payload
                .get("step")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32;
            current_graph_nodes.insert(thread_id, (node.to_string(), step));
        }
        Ok(ThreadListEnrichment {
            follow_waiters,
            facets,
            current_graph_nodes,
            terminal_error_previews,
        })
    }

    /// One consistent runtime waiter snapshot plus its projected suspended
    /// parents. Parent rows are fetched in bounded batches under the same store
    /// lock, avoiding one lock/query cycle per waiter.
    pub fn follow_parent_list_snapshot(&self) -> Result<FollowParentListSnapshot> {
        let (waiters, rows, successor_payloads) = {
            let g = self.lock()?;
            let hold_started = std::time::Instant::now();
            let waiters = g
                .runtime_db
                .follow_waiter_summaries_bounded(MAX_THREAD_LIST_ENRICHMENT_THREADS)?;
            let mut parent_ids = waiters
                .iter()
                .map(|waiter| waiter.parent_thread_id.clone())
                .collect::<Vec<_>>();
            parent_ids.sort();
            parent_ids.dedup();
            let rows = queries::get_threads_many(g.state_db.projection(), &parent_ids)?;
            let payloads = Self::continuation_payloads_for_rows(&g, &rows)?;
            Self::warn_slow_lock_hold("follow_parent_list_snapshot", hold_started);
            (waiters, rows, payloads)
        };
        let parents = Self::rows_to_list_items(rows, successor_payloads)?;
        Ok(FollowParentListSnapshot { waiters, parents })
    }

    pub fn summarize_usage_by_subject(
        &self,
        filter: queries::UsageSummaryFilter<'_>,
    ) -> Result<Vec<queries::UsageSummaryRow>> {
        let g = self.lock()?;
        queries::summarize_usage_by_subject(g.state_db.projection(), filter)
    }

    pub fn list_thread_children(&self, thread_id: &str) -> Result<Vec<ThreadDetail>> {
        let g = self.lock()?;
        let child_rows = queries::list_thread_children(g.state_db.projection(), thread_id)?;
        let mut children = Vec::new();
        for row in child_rows {
            let runtime = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .unwrap_or_default();
            let successor_thread_id = if is_terminal_status(&row.status) {
                queries::continuation_successor(g.state_db.projection(), &row.thread_id)?
            } else {
                None
            };
            children.push(ThreadDetail {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                executor_ref: row.executor_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                project_authority: Some(row.project_authority),
                lifecycle_authority: runtime
                    .launch_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.resume_context.as_ref())
                    .map(|resume| resume.lifecycle_authority),
                admitted_launch_capsule_hash: row.admitted_launch_capsule_hash,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                finished_at: row.finished_at,

                runtime,
            });
        }
        Ok(children)
    }

    pub fn list_chain_threads(&self, chain_root_id: &str) -> Result<Vec<ThreadDetail>> {
        let g = self.lock()?;
        let thread_rows = queries::list_threads_by_chain(g.state_db.projection(), chain_root_id)?;
        let mut threads = Vec::new();
        for row in thread_rows {
            let runtime = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .unwrap_or_default();
            let successor_thread_id = if is_terminal_status(&row.status) {
                queries::continuation_successor(g.state_db.projection(), &row.thread_id)?
            } else {
                None
            };
            threads.push(ThreadDetail {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                executor_ref: row.executor_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                project_authority: Some(row.project_authority),
                lifecycle_authority: runtime
                    .launch_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.resume_context.as_ref())
                    .map(|resume| resume.lifecycle_authority),
                admitted_launch_capsule_hash: row.admitted_launch_capsule_hash,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                finished_at: row.finished_at,

                runtime,
            });
        }
        Ok(threads)
    }

    pub fn list_chain_edges(&self, chain_root_id: &str) -> Result<Vec<ThreadEdgeRecord>> {
        let g = self.lock()?;
        let edge_rows = queries::list_thread_edges(g.state_db.projection(), chain_root_id)?;
        Ok(edge_rows
            .into_iter()
            .enumerate()
            .map(|(idx, row)| ThreadEdgeRecord {
                edge_id: idx as i64 + 1,
                chain_root_id: row.chain_root_id,
                source_thread_id: row.parent_thread_id,
                target_thread_id: row.child_thread_id,
                edge_type: "spawned".to_string(),
                created_at: String::new(),
                metadata: row.spawn_reason.map(|r| json!(r)),
            })
            .collect())
    }

    pub fn list_threads_by_status(&self, statuses: &[&str]) -> Result<Vec<ThreadDetail>> {
        let g = self.lock()?;
        let thread_rows = queries::list_threads_by_status(g.state_db.projection(), statuses)?;
        let mut details = Vec::new();
        for row in thread_rows {
            let runtime = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .unwrap_or_default();
            let successor_thread_id = if is_terminal_status(&row.status) {
                queries::continuation_successor(g.state_db.projection(), &row.thread_id)?
            } else {
                None
            };
            details.push(ThreadDetail {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                executor_ref: row.executor_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                upstream_thread_id: row.upstream_thread_id,
                successor_thread_id,
                requested_by: row.requested_by,
                project_root: row.project_root,
                project_authority: Some(row.project_authority),
                lifecycle_authority: runtime
                    .launch_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.resume_context.as_ref())
                    .map(|resume| resume.lifecycle_authority),
                admitted_launch_capsule_hash: row.admitted_launch_capsule_hash,
                created_at: row.created_at,
                updated_at: row.updated_at,
                started_at: row.started_at,
                finished_at: row.finished_at,

                runtime,
            });
        }
        Ok(details)
    }

    pub fn active_thread_count(&self) -> Result<i64> {
        let g = self.lock()?;
        queries::active_thread_count(g.state_db.projection())
    }

    pub fn wait_for_project_authority(
        &self,
        thread_id: &str,
        reason: &str,
        detail: &str,
        now_ms: i64,
        deadline_at_ms: i64,
    ) -> Result<runtime_db::RecoveryWaitDisposition> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.wait_for_project_authority(
            thread_id,
            reason,
            detail,
            now_ms,
            deadline_at_ms,
        )
    }

    pub fn clear_recovery_wait(&self, thread_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?.runtime_db.clear_recovery_wait(thread_id)
    }

    pub fn authoritative_result_project_snapshot(&self, thread_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        Ok(g.state_db
            .get_thread(thread_id)?
            .and_then(|snapshot| snapshot.result_project_snapshot_hash))
    }

    pub fn admitted_launch_capsule_hash(&self, thread_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        Ok(g.state_db
            .get_thread(thread_id)?
            .and_then(|thread| thread.admitted_launch_capsule_hash))
    }

    /// Load the authoritative CAS-rooted launch capsule for an admitted
    /// thread. Operational SQLite metadata is deliberately not consulted.
    pub fn admitted_launch_capsule(
        &self,
        thread_id: &str,
    ) -> Result<Option<ryeos_state::objects::AdmittedLaunchCapsule>> {
        let g = self.lock()?;
        let Some(snapshot) = g.state_db.get_thread(thread_id)? else {
            return Ok(None);
        };
        snapshot
            .admitted_launch_capsule_hash
            .as_deref()
            .map(|hash| load_admitted_launch_capsule(&self.state_authority, hash))
            .transpose()
    }

    /// Compare a recovery attempt with the CAS-rooted artifact identity before
    /// any process is permitted to execute. Runtime SQLite metadata is an
    /// operational index; the signed thread snapshot and its capsule are the
    /// authority boundary.
    pub fn verify_admitted_artifact_identity(
        &self,
        thread_id: &str,
        attempted: &ryeos_state::objects::AdmittedLaunchArtifactIdentity,
    ) -> Result<()> {
        attempted.validate()?;
        let g = self.lock()?;
        let snapshot = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;
        let capsule_hash = snapshot
            .admitted_launch_capsule_hash
            .as_deref()
            .ok_or_else(|| {
                anyhow!("thread {thread_id} has no authoritative admitted launch capsule")
            })?;
        let capsule = load_admitted_launch_capsule(&self.state_authority, capsule_hash)?;
        if &capsule.artifact_identity != attempted {
            bail!(
                "thread {thread_id} recovery artifact identity differs from its authoritative admitted capsule"
            );
        }
        Ok(())
    }

    /// Immutable project snapshots required by active or queued runtimes.
    ///
    /// These runtime-DB references are not signed CAS heads, so online GC must
    /// add them as daemon-authoritative transient roots. Retain both fields
    /// defensively if a record carries both; resume selection intentionally
    /// chooses one, while reachability must not collect either active pin.
    pub fn active_resume_snapshot_roots(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        let statuses = [
            ThreadStatus::Created.as_str(),
            ThreadStatus::Running.as_str(),
        ];
        let rows = queries::list_threads_by_status(g.state_db.projection(), &statuses)?;
        let mut roots = std::collections::BTreeSet::new();
        for row in rows {
            let Some(metadata) = g
                .runtime_db
                .get_runtime_info(&row.thread_id)?
                .and_then(|info| info.launch_metadata)
            else {
                continue;
            };
            let Some(resume) = metadata.resume_context else {
                continue;
            };
            if let Some(hash) = resume.original_snapshot_hash {
                roots.insert(hash);
            }
            if let Some(pushed) = resume.original_pushed_head_ref {
                roots.insert(pushed.snapshot_hash);
            }
        }
        // Workspace reservation precedes thread birth so a crash cannot leave
        // an unjournaled mount. Those lower generations are therefore not
        // always reachable through authoritative thread history yet. Keep
        // every non-closed journal generation as an operational GC root until
        // verified backend cleanup closes the record.
        for workspace in g.runtime_db.open_workspaces()? {
            roots.insert(workspace.lower_snapshot);
            if let Some(frozen) = workspace.frozen_snapshot_hash {
                roots.insert(frozen);
            }
        }
        roots.extend(g.runtime_db.handoff_cas_object_roots()?);
        Ok(roots.into_iter().collect())
    }

    /// Read a thread's persisted launch metadata (resume context), if any.
    pub fn get_launch_metadata(
        &self,
        thread_id: &str,
    ) -> Result<Option<crate::launch_metadata::RuntimeLaunchMetadata>> {
        let g = self.lock()?;
        Ok(g.runtime_db
            .get_runtime_info(thread_id)?
            .and_then(|info| info.launch_metadata))
    }

    /// Seed a thread's launch identity (resume context / continuation spec) at
    /// spawn time so a continuation successor can be relaunched later with no
    /// live request. Metadata-only (does not touch pid/pgid); the
    /// clobber-preserving attach keeps it against a later empty self-attach.
    pub fn seed_launch_metadata(
        &self,
        thread_id: &str,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.set_launch_metadata(thread_id, launch_metadata)
    }

    /// Atomically add the exact isolation generation/plan identity to a
    /// thread's already-seeded launch metadata without disturbing resume or
    /// cancellation authority.
    pub fn seed_isolation_provenance(
        &self,
        thread_id: &str,
        provenance: ryeos_engine::isolation::IsolationLaunchProvenance,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        let mut metadata = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .and_then(|info| info.launch_metadata)
            .unwrap_or_default();
        metadata.isolation = Some(provenance);
        g.runtime_db.set_launch_metadata(thread_id, &metadata)
    }

    #[tracing::instrument(
        name = "state:attach_thread_process",
        skip(self, launch_metadata),
        fields(thread_id = %thread_id, pid = pid, pgid = pgid)
    )]
    pub fn attach_thread_process(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        expected_launch_owner: Option<&str>,
    ) -> Result<()> {
        self.attach_thread_process_with_mode(
            thread_id,
            pid,
            pgid,
            process_identity,
            launch_metadata,
            ProcessAttachmentMode::Idempotent {
                launch_owner: expected_launch_owner,
            },
        )
    }

    /// Persist a daemon-held process at the attachment-before-execution
    /// boundary. The runtime cannot have self-attached before this call, so an
    /// existing identity (including an exact repeat) is rejected.
    pub fn attach_new_thread_process(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        expected_launch_owner: Option<&str>,
    ) -> Result<()> {
        self.attach_thread_process_with_mode(
            thread_id,
            pid,
            pgid,
            process_identity,
            launch_metadata,
            ProcessAttachmentMode::New {
                launch_owner: expected_launch_owner,
            },
        )
    }

    fn attach_thread_process_with_mode(
        &self,
        thread_id: &str,
        pid: i64,
        pgid: i64,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        mode: ProcessAttachmentMode<'_>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if let Some(expected) = mode.launch_owner() {
            let claim = g
                .runtime_db
                .get_launch_claim(thread_id)?
                .ok_or_else(|| anyhow::anyhow!("thread {thread_id} has no launch owner"))?;
            if claim.claimed_by != expected {
                anyhow::bail!("stale launch owner cannot attach process to {thread_id}");
            }
        }
        // The projection row is the authoritative lifecycle identity. A bare
        // runtime row must never acquire a process that reconcile/drain cannot
        // subsequently account for.
        let thread = g.state_db.get_thread(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("thread not found before process attach: {thread_id}")
        })?;
        let exact_repeat = !mode.requires_empty()
            && g.runtime_db
                .get_runtime_info(thread_id)?
                .is_some_and(|runtime| {
                    runtime.pid == Some(pid)
                        && runtime.pgid == Some(pgid)
                        && runtime.process_identity.as_ref() == Some(process_identity)
                });
        if !exact_repeat
            && !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            anyhow::bail!("process attachment admission is closed for daemon shutdown");
        }
        // Defensive: skip attach if the thread was already finalized
        // (e.g. cancelled while the runner was between spawn and attach). An
        // exact identity repeat remains safe and idempotent: fast callback
        // runtimes can self-attach and finalize before their in-process owner
        // reaches the same attach call.
        if is_terminal_status(&thread.status) && !exact_repeat {
            tracing::warn!(
                thread_id,
                status = %thread.status,
                pid,
                pgid,
                "skipping attach_process — thread already terminal"
            );
            anyhow::bail!(
                "refusing to attach process {pid}/{pgid} to terminal thread {thread_id} ({})",
                thread.status
            );
        }
        if !exact_repeat
            && g.runtime_db
                .launch_window_is_cancelled(&thread.chain_root_id)?
        {
            anyhow::bail!(
                "refusing to attach process {pid}/{pgid} to cancelled launch-window member {thread_id}"
            );
        }
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        if mode.requires_empty() {
            g.runtime_db
                .attach_new_process(thread_id, pid, pgid, process_identity, launch_metadata)
        } else {
            g.runtime_db
                .attach_process(thread_id, pid, pgid, process_identity, launch_metadata)
        }
    }

    /// Linearization point between durable process attachment and target
    /// release. A stop that wins this lock prevents release; a stop that lands
    /// afterward observes the exact attached identity and terminates it.
    pub fn authorize_attached_process_release(
        &self,
        thread_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        expected_launch_owner: Option<&str>,
    ) -> Result<()> {
        let g = self.lock()?;
        if let Some(expected) = expected_launch_owner {
            let claim = g
                .runtime_db
                .get_launch_claim(thread_id)?
                .ok_or_else(|| anyhow::anyhow!("thread {thread_id} has no launch owner"))?;
            if claim.claimed_by != expected {
                anyhow::bail!("stale launch owner cannot release process for {thread_id}");
            }
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            anyhow::bail!("process release is fenced during daemon shutdown");
        }
        let thread = g.state_db.get_thread(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("thread not found before process release: {thread_id}")
        })?;
        if is_terminal_status(&thread.status) {
            anyhow::bail!(
                "refusing to release process for terminal thread {thread_id} ({})",
                thread.status
            );
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("runtime row missing before process release: {thread_id}")
        })?;
        if let Some(intent) = runtime.stop_intent {
            anyhow::bail!(
                "process release is fenced after {} request for thread {thread_id}",
                intent.as_str()
            );
        }
        if runtime.pid != Some(process_identity.target_pid)
            || runtime.pgid != Some(process_identity.group_leader_pid)
            || runtime.process_identity.as_ref() != Some(process_identity)
        {
            anyhow::bail!(
                "process release identity does not match durable attachment for thread {thread_id}"
            );
        }
        Ok(())
    }

    /// Close process attachment admission at the shutdown serialization point.
    /// Taking the StateStore lock first waits for every prior attach to commit;
    /// every later attach acquires the lock and observes the closed gate.
    pub fn close_process_attachment_admission(&self) -> Result<()> {
        let _g = self.lock()?;
        self.process_attachment_admission_open
            .store(false, Ordering::Release);
        Ok(())
    }

    /// Whether this daemon instance still admits/finalizes live executions.
    /// Shutdown closes the gate before draining; process owners that wake from
    /// a shutdown-owned signal use this to preserve resumable rows instead of
    /// misclassifying the interruption as an execution failure.
    pub fn process_attachment_admission_is_open(&self) -> bool {
        self.process_attachment_admission_open
            .load(Ordering::Acquire)
    }

    /// Fail closed before a runtime callback authors or dispatches new work.
    /// Durable child-link/continuation mutations repeat this check or inherit
    /// the stop under the same store lock; this front-door check avoids doing
    /// expensive resolution after the authoring fence is already closed.
    pub fn ensure_running_runtime_mutation_allowed(&self, thread_id: &str) -> Result<()> {
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            anyhow::bail!("runtime authoring is closed for daemon shutdown");
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("runtime row missing for callback thread {thread_id}")
        })?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("callback thread not found: {thread_id}"))?;
        if thread.status != ThreadStatus::Running.as_str() {
            anyhow::bail!(
                "runtime mutation requires a running thread; {thread_id} is {}",
                thread.status
            );
        }
        if let Some(intent) = runtime.stop_intent {
            anyhow::bail!(
                "runtime mutation is closed for stop-requested thread {thread_id} ({})",
                intent.as_str()
            );
        }
        Ok(())
    }

    /// Atomically admit a non-read runtime callback against authoritative
    /// lifecycle, stop, and shutdown state. This is the callback
    /// linearization point: a request admitted here may finish, while every
    /// request that arrives after terminal/stop/shutdown is refused.
    pub fn ensure_runtime_callback_mutation_allowed(
        &self,
        thread_id: &str,
        stop_completion: bool,
    ) -> Result<()> {
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("callback thread not found: {thread_id}"))?;
        if is_terminal_status(&thread.status) {
            anyhow::bail!(
                "runtime callback mutation is fenced for terminal thread {thread_id} ({})",
                thread.status
            );
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow::anyhow!("runtime row missing for callback thread {thread_id}")
        })?;
        if let Some(intent) = runtime.stop_intent {
            if stop_completion {
                return Ok(());
            }
            anyhow::bail!(
                "runtime callback mutation is fenced after {} request for thread {thread_id}",
                intent.as_str()
            );
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            anyhow::bail!("runtime callback mutation is fenced during daemon shutdown");
        }
        Ok(())
    }

    /// Atomically tombstone an explicit stop against process attachment and
    /// return the identity (if any) that won the attach race before the stop.
    pub fn request_thread_stop(
        &self,
        thread_id: &str,
        intent: runtime_db::StopIntent,
    ) -> Result<RuntimeInfo> {
        let g = self.lock()?;
        g.runtime_db.request_thread_stop(thread_id, intent)
    }

    /// Request a stop only if ordinary execution ownership is still open.
    ///
    /// The gate check and tombstone share the StateStore lock with shutdown's
    /// gate close. Cancellation cleanup therefore either wins before shutdown
    /// and owns the durable stop, or observes the closed gate and leaves the row
    /// untouched for the shutdown coordinator.
    pub fn request_thread_stop_if_admission_open(
        &self,
        thread_id: &str,
        intent: runtime_db::StopIntent,
    ) -> Result<StopIfAdmissionOpenOutcome> {
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found before owner-drop stop: {thread_id}"))?;
        if is_terminal_status(&thread.status) {
            return Ok(StopIfAdmissionOpenOutcome::AlreadyTerminal);
        }
        // A durable follow waiter transfers lifecycle ownership to the daemon.
        // Keep this check under the same store lock as the stop tombstone so a
        // request drop cannot race waiter reservation and kill the new chain.
        if g.runtime_db
            .get_follow_waiter_by_parent_thread(thread_id)?
            .is_some()
            || g.runtime_db
                .get_follow_waiter_by_successor(thread_id)?
                .is_some()
        {
            return Ok(StopIfAdmissionOpenOutcome::PreservedForFollow);
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            return Ok(StopIfAdmissionOpenOutcome::PreservedForShutdown);
        }
        g.runtime_db
            .request_thread_stop(thread_id, intent)
            .map(|runtime| StopIfAdmissionOpenOutcome::Requested(Box::new(runtime)))
    }

    pub fn clear_thread_process_if_matches(
        &self,
        thread_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
    ) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db
            .clear_process_if_matches(thread_id, process_identity)
    }

    pub fn clear_thread_process_if_matches_owned(
        &self,
        thread_id: &str,
        process_identity: &crate::process::ExecutionProcessIdentity,
        launch_owner: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner cannot detach process from {thread_id}");
        }
        g.runtime_db
            .clear_process_if_matches(thread_id, process_identity)
    }

    pub fn list_attached_thread_ids(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.list_attached_thread_ids()
    }

    /// Read the auto-resume attempt counter for a thread.
    pub fn get_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let g = self.lock()?;
        g.runtime_db.get_resume_attempts(thread_id)
    }

    /// Atomically bump the auto-resume counter and return the
    /// post-increment value.
    pub fn bump_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.bump_resume_attempts(thread_id)
    }

    /// Atomically claim the right to launch a thread. The sole authorization for
    /// a spawn — see [`runtime_db::RuntimeDb::claim_thread_launch`].
    pub fn claim_thread_launch(
        &self,
        thread_id: &str,
        claim_id: &str,
        claimed_by: &str,
    ) -> Result<runtime_db::LaunchClaimOutcome> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db
            .claim_thread_launch(thread_id, claim_id, claimed_by)
    }

    /// Claim an existing thread and publish its in-process owner before the
    /// StateStore lock is released. Live reconciliation reads durable claims
    /// through this same lock, so it can never observe the new claim without
    /// also observing the active owner that is responsible for it.
    pub fn claim_thread_launch_active(
        &self,
        thread_id: &str,
        claim_id: &str,
        daemon_generation_id: &str,
    ) -> Result<Option<runtime_db::LaunchClaim>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        match g
            .runtime_db
            .claim_thread_launch(thread_id, claim_id, daemon_generation_id)?
        {
            runtime_db::LaunchClaimOutcome::AlreadyClaimed => Ok(None),
            runtime_db::LaunchClaimOutcome::Claimed => {
                let claim = g
                    .runtime_db
                    .get_launch_claim(thread_id)?
                    .ok_or_else(|| anyhow!("launch owner disappeared after claim"))?;
                let mut active = self
                    .active_launch_owners
                    .lock()
                    .map_err(|_| anyhow!("active launch-owner registry poisoned"))?;
                if !active.insert(claim.claimed_by.clone()) {
                    g.runtime_db
                        .release_thread_launch_claim(thread_id, claim_id)?;
                    bail!("launch owner is already registered in this daemon");
                }
                Ok(Some(claim))
            }
        }
    }

    /// Reserve launch ownership for a pre-minted thread ID before its row is
    /// published. Fresh execution paths use this to make creation visible only
    /// after durable spawn ownership exists. The owned executor guard removes
    /// the reservation if creation fails; daemon startup clears reservations
    /// left by an interrupted process.
    pub fn reserve_fresh_thread_launch(
        &self,
        thread_id: &str,
        claim_id: &str,
        claimed_by: &str,
    ) -> Result<runtime_db::LaunchClaimOutcome> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if g.state_db.get_thread(thread_id)?.is_some() {
            bail!(
                "fresh launch reservation requires an unpublished thread ID; thread already exists: {thread_id}"
            );
        }
        g.runtime_db
            .claim_thread_launch(thread_id, claim_id, claimed_by)
    }

    /// Fresh-thread counterpart to [`Self::claim_thread_launch_active`]. The
    /// unpublished ID and its active durable owner become observable as one
    /// StateStore operation.
    pub fn reserve_fresh_thread_launch_active(
        &self,
        thread_id: &str,
        claim_id: &str,
        daemon_generation_id: &str,
    ) -> Result<Option<runtime_db::LaunchClaim>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if g.state_db.get_thread(thread_id)?.is_some() {
            bail!(
                "fresh launch reservation requires an unpublished thread ID; thread already exists: {thread_id}"
            );
        }
        match g
            .runtime_db
            .claim_thread_launch(thread_id, claim_id, daemon_generation_id)?
        {
            runtime_db::LaunchClaimOutcome::AlreadyClaimed => Ok(None),
            runtime_db::LaunchClaimOutcome::Claimed => {
                let claim = g
                    .runtime_db
                    .get_launch_claim(thread_id)?
                    .ok_or_else(|| anyhow!("fresh launch owner disappeared"))?;
                let mut active = self
                    .active_launch_owners
                    .lock()
                    .map_err(|_| anyhow!("active launch-owner registry poisoned"))?;
                if !active.insert(claim.claimed_by.clone()) {
                    g.runtime_db
                        .release_thread_launch_claim(thread_id, claim_id)?;
                    bail!("launch owner is already registered in this daemon");
                }
                Ok(Some(claim))
            }
        }
    }

    /// Release a launch claim the caller owns (matched by `claim_id`).
    pub fn release_thread_launch_claim(&self, thread_id: &str, claim_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db
            .release_thread_launch_claim(thread_id, claim_id)
    }

    /// Atomically retire the in-process owner and its exact durable claim with
    /// respect to live reconciliation. A replacement claim cannot be confused
    /// with the owner being dropped because deletion remains claim-id-qualified.
    pub fn release_active_thread_launch_claim(
        &self,
        thread_id: &str,
        claim_id: &str,
        launch_owner: &str,
    ) -> Result<bool> {
        let g = self.lock()?;
        let mut active = self
            .active_launch_owners
            .lock()
            .map_err(|_| anyhow!("active launch-owner registry poisoned"))?;
        active.remove(launch_owner);
        g.runtime_db
            .release_thread_launch_claim(thread_id, claim_id)
    }

    /// Read the current launch claim, if any — distinguishes an unlaunched
    /// successor from one mid-launch for the reconciler.
    pub fn get_launch_claim(&self, thread_id: &str) -> Result<Option<runtime_db::LaunchClaim>> {
        let g = self.lock()?;
        g.runtime_db.get_launch_claim(thread_id)
    }

    pub fn assert_launch_owner(&self, thread_id: &str, launch_owner: &str) -> Result<()> {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner for thread {thread_id}");
        }
        Ok(())
    }

    /// Return the descriptor-stable process identity only when the caller is
    /// still the thread's exact launch owner. Owner validation and identity
    /// read share the StateStore lock so callback quiescence cannot borrow a
    /// replacement launch's process.
    pub fn execution_process_identity_owned(
        &self,
        thread_id: &str,
        launch_owner: &str,
    ) -> Result<crate::process::ExecutionProcessIdentity> {
        let g = self.lock()?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner for thread {thread_id}");
        }
        g.runtime_db
            .get_runtime_info(thread_id)?
            .and_then(|runtime| runtime.process_identity)
            .ok_or_else(|| anyhow!("thread {thread_id} has no attached process identity"))
    }

    pub fn reserve_execution_workspace(
        &self,
        workspace_id: &str,
        lower_snapshot: &str,
        root_path: &str,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.runtime_db
            .reserve_workspace(workspace_id, lower_snapshot, root_path)
    }

    pub fn bind_execution_workspace(
        &self,
        binding: runtime_db::WorkspaceBinding<'_>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, binding.thread_id)?;
        g.runtime_db.bind_workspace(binding)
    }

    pub fn claim_execution_workspace_construction(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner for thread {thread_id}");
        }
        g.runtime_db
            .claim_workspace_construction(workspace_id, thread_id, launch_owner)
    }

    pub fn prepare_execution_workspace_backend(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        backend_id: &str,
        backend_version: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.prepare_workspace_backend(
            workspace_id,
            thread_id,
            launch_owner,
            backend_id,
            backend_version,
        )
    }

    pub fn transition_execution_workspace(
        &self,
        workspace_id: &str,
        expected: &[runtime_db::WorkspaceState],
        next: runtime_db::WorkspaceState,
        process_identity: Option<&str>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.runtime_db
            .transition_workspace(workspace_id, expected, next, process_identity)
    }

    pub fn transition_execution_workspace_owned(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        expected: &[runtime_db::WorkspaceState],
        next: runtime_db::WorkspaceState,
        process_identity: Option<&str>,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        let claim = g
            .runtime_db
            .get_launch_claim(thread_id)?
            .ok_or_else(|| anyhow!("thread {thread_id} has no current launch owner"))?;
        if claim.claimed_by != launch_owner {
            bail!("stale launch owner for thread {thread_id}");
        }
        g.runtime_db.transition_workspace_owned(
            workspace_id,
            thread_id,
            launch_owner,
            expected,
            next,
            process_identity,
        )
    }

    /// Transition an exact dead owner's workspace during reconciliation. A
    /// replacement launch may exist for the same thread, so this fence is the
    /// immutable workspace owner tuple rather than the thread's current claim.
    pub fn transition_abandoned_execution_workspace_owned(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        expected: &[runtime_db::WorkspaceState],
        next: runtime_db::WorkspaceState,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let workspace = g
            .runtime_db
            .workspace(workspace_id)?
            .ok_or_else(|| anyhow!("workspace {workspace_id} disappeared"))?;
        if workspace.thread_id.as_deref() != Some(thread_id)
            || workspace.launch_owner.as_deref() != Some(launch_owner)
        {
            bail!("abandoned workspace owner tuple changed during reconciliation");
        }
        if g.runtime_db
            .get_launch_claim(thread_id)?
            .is_some_and(|claim| {
                claim.claimed_by == launch_owner && self.is_launch_owner_active(&claim.claimed_by)
            })
        {
            bail!("workspace launch owner is still active in this daemon");
        }
        g.runtime_db.transition_workspace_owned(
            workspace_id,
            thread_id,
            launch_owner,
            expected,
            next,
            None,
        )
    }

    /// Publish a callback-frozen generation under the same StateStore lock as
    /// launch-owner fencing. RuntimeDb performs the workspace-journal and
    /// ResumeContext updates in one immediate SQLite transaction.
    pub fn bind_frozen_execution_workspace(
        &self,
        workspace_id: &str,
        thread_id: &str,
        launch_owner: &str,
        snapshot_hash: &str,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.bind_frozen_workspace_generation(
            workspace_id,
            thread_id,
            launch_owner,
            snapshot_hash,
        )
    }

    pub fn open_execution_workspaces(&self) -> Result<Vec<runtime_db::WorkspaceRecord>> {
        let g = self.lock()?;
        g.runtime_db.open_workspaces()
    }

    pub fn execution_workspace(
        &self,
        workspace_id: &str,
    ) -> Result<Option<runtime_db::WorkspaceRecord>> {
        let g = self.lock()?;
        g.runtime_db.workspace(workspace_id)
    }

    // ── Hook dispatch ledger ─────────────────────────────────────────────

    pub fn reserve_hook_dispatch(
        &self,
        seed: &runtime_db::NewHookDispatch,
    ) -> Result<runtime_db::HookDispatchReservation> {
        let g = self.lock()?;
        g.runtime_db.reserve_hook_dispatch(seed)
    }

    pub fn complete_hook_dispatch(
        &self,
        dispatch_key: &str,
        request_hash: &str,
        response: &Value,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .complete_hook_dispatch(dispatch_key, request_hash, response)
    }

    /// Reserve the stable child identity for one detached callback operation
    /// while the authoritative parent chain is still admitted.
    pub fn reserve_detached_spawn_intent(
        &self,
        operation_id: &str,
        parent_thread_id: &str,
        request_hash: &str,
        proposed_child_thread_id: &str,
        child_project_authority: Option<&ryeos_state::objects::ExecutionProjectAuthority>,
    ) -> Result<String> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let parent = g
            .state_db
            .get_thread(parent_thread_id)?
            .ok_or_else(|| anyhow!("detached parent thread {parent_thread_id} does not exist"))?;
        let _admission = g.state_db.authorize_runtime_pin(&parent.chain_root_id)?;
        g.runtime_db.reserve_detached_spawn_intent(
            operation_id,
            parent_thread_id,
            request_hash,
            proposed_child_thread_id,
            child_project_authority,
        )
    }

    pub fn detached_spawn_intents(&self) -> Result<Vec<runtime_db::DetachedSpawnIntent>> {
        self.lock()?.runtime_db.detached_spawn_intents()
    }

    pub fn abort_unsealed_detached_spawn_intent(&self, operation_id: &str) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .abort_unsealed_detached_spawn_intent(operation_id)
    }

    pub fn get_detached_spawn_intent(
        &self,
        operation_id: &str,
    ) -> Result<Option<runtime_db::DetachedSpawnIntent>> {
        self.lock()?
            .runtime_db
            .get_detached_spawn_intent(operation_id)
    }

    pub fn bind_detached_spawn_project_authority(
        &self,
        operation_id: &str,
        child_project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_detached_spawn_project_authority(operation_id, child_project_authority)
    }

    pub fn seal_detached_spawn_intent(
        &self,
        operation_id: &str,
        child_project_authority: &ryeos_state::objects::ExecutionProjectAuthority,
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
        initial_events: &[NewEventRecord],
    ) -> Result<()> {
        let permit = self.acquire_write_permit()?;
        let capsule = launch_metadata
            .admitted_launch_capsule()?
            .ok_or_else(|| anyhow!("detached launch cannot seal without an admitted capsule"))?;
        if &capsule.project_authority != child_project_authority {
            bail!("detached launch capsule contradicts selected child project authority");
        }
        self.state_authority.ensure_guard(permit.cas_guard())?;
        let expected_capsule_hash = capsule.content_hash()?;
        let admitted_launch_capsule_hash = self
            .state_authority
            .cas_store()?
            .store_object(&capsule.to_value())
            .context("store detached admitted launch capsule")?;
        if admitted_launch_capsule_hash != expected_capsule_hash {
            bail!(
                "detached admitted capsule hash mismatch: expected {expected_capsule_hash}, stored {admitted_launch_capsule_hash}"
            );
        }
        self.lock()?.runtime_db.seal_detached_spawn_intent(
            operation_id,
            child_project_authority,
            &admitted_launch_capsule_hash,
            launch_metadata,
            initial_events,
        )
    }

    // ── Follow waiters ───────────────────────────────────────────────────

    pub fn reserve_follow(
        &self,
        seed: &runtime_db::NewFollowWaiter,
    ) -> Result<runtime_db::FollowWaiter> {
        validate_follow_reservation_shape(seed)?;
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = g
            .state_db
            .authorize_runtime_pin(&seed.parent_chain_root_id)?;
        g.runtime_db.reserve_follow(seed)
    }

    pub fn bind_follow_project_authority(
        &self,
        follow_key: &str,
        authority: &ryeos_state::objects::ExecutionProjectAuthority,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        self.lock()?
            .runtime_db
            .bind_follow_project_authority(follow_key, authority)
    }

    // Slot identity, item/spec identity, child lineage, and sealed authority
    // stay explicit because each is independently verified under the store lock.
    #[allow(clippy::too_many_arguments)]
    pub fn set_follow_child(
        &self,
        follow_key: &str,
        item_index: u32,
        item_ref: &str,
        spec_hash: &str,
        child_thread_id: &str,
        child_chain_root_id: &str,
        sealed_root_request: &crate::thread_lifecycle::SealedRootExecutionRequest,
    ) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let waiter = g
            .runtime_db
            .get_follow_waiter_by_key(follow_key)?
            .ok_or_else(|| anyhow!("follow waiter {follow_key} does not exist"))?;
        let _admission = match g.state_db.authorize_runtime_pin(child_chain_root_id) {
            Ok(admission) => admission,
            Err(_) => g
                .state_db
                .authorize_future_runtime_pin(&waiter.parent_chain_root_id, child_chain_root_id)?,
        };
        g.runtime_db.set_follow_child(
            follow_key,
            item_index,
            item_ref,
            spec_hash,
            child_thread_id,
            child_chain_root_id,
            sealed_root_request,
        )
    }

    pub fn get_follow_child(
        &self,
        follow_key: &str,
        item_index: u32,
    ) -> Result<Option<runtime_db::FollowWaiterChild>> {
        let g = self.lock()?;
        g.runtime_db.get_follow_child(follow_key, item_index)
    }

    pub fn set_follow_parent_successor(
        &self,
        follow_key: &str,
        successor_thread_id: &str,
    ) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db
            .set_follow_parent_successor(follow_key, successor_thread_id)
    }

    pub fn mark_follow_waiting(&self, follow_key: &str) -> Result<String> {
        let g = self.lock()?;
        g.runtime_db.mark_follow_waiting(follow_key)
    }

    pub fn mark_follow_resuming(&self, follow_key: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.mark_follow_resuming(follow_key)
    }

    pub fn mark_follow_child_terminal(
        &self,
        child_chain_root_id: &str,
        child_terminal_thread_id: &str,
        child_terminal_status: &str,
        terminal_envelope: &serde_json::Value,
    ) -> Result<bool> {
        let g = self.lock()?;
        let Some(waiter) = g
            .runtime_db
            .get_follow_waiter_by_child_chain(child_chain_root_id)?
        else {
            return Ok(false);
        };
        let (terminal_envelope, degraded) = admit_follow_terminal_envelope(
            &waiter,
            child_chain_root_id,
            child_terminal_thread_id,
            terminal_envelope,
        )?;
        if degraded {
            tracing::warn!(
                child_chain_root_id,
                child_terminal_thread_id,
                follow_key = %waiter.follow_key,
                "follow child terminal envelope exceeded parent resume bounds; storing bounded failure envelope"
            );
        }
        g.runtime_db.mark_follow_child_terminal(
            child_chain_root_id,
            child_terminal_thread_id,
            child_terminal_status,
            &terminal_envelope,
        )
    }

    pub fn get_follow_waiter_by_key(
        &self,
        follow_key: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db.get_follow_waiter_by_key(follow_key)
    }

    pub fn get_follow_waiter_by_child_chain(
        &self,
        child_chain_root_id: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db
            .get_follow_waiter_by_child_chain(child_chain_root_id)
    }

    /// The live waiter for a SUSPENDED PARENT thread (the follow issuer), used to
    /// decorate a `continued` thread with its follow lineage.
    pub fn get_follow_waiter_by_parent_thread(
        &self,
        parent_thread_id: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db
            .get_follow_waiter_by_parent_thread(parent_thread_id)
    }

    /// The live waiter whose recorded resume successor is `successor_thread_id`,
    /// used to decorate a follow-resume successor with its live lineage before the
    /// waiter is cleared.
    pub fn get_follow_waiter_by_successor(
        &self,
        successor_thread_id: &str,
    ) -> Result<Option<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db
            .get_follow_waiter_by_successor(successor_thread_id)
    }

    pub fn list_follow_waiters(&self) -> Result<Vec<runtime_db::FollowWaiter>> {
        let g = self.lock()?;
        g.runtime_db.list_follow_waiters()
    }

    pub fn clear_follow_waiter(&self, follow_key: &str) -> Result<()> {
        let g = self.lock()?;
        g.runtime_db.clear_follow_waiter(follow_key)
    }

    /// Append the portable parent→child spawn edge exactly once. The write
    /// permit and store lock cover the appendability check, projected-edge
    /// lookup, and signed-chain append, so concurrent RESERVED re-drives cannot
    /// both observe absence. Projection must be current because it is the
    /// idempotency index for the signed event stream.
    pub fn append_child_thread_spawned_once(
        &self,
        chain_root_id: &str,
        parent_thread_id: &str,
        child_thread_id: &str,
        payload: Value,
    ) -> Result<ChildLineageAppend> {
        if payload.get("child_thread_id").and_then(Value::as_str) != Some(child_thread_id) {
            bail!("child lineage payload does not name child {child_thread_id}");
        }
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self.projection_health.is_current() {
            bail!("child lineage admission requires a current thread projection");
        }
        let Some(parent) = g.state_db.get_thread(parent_thread_id)? else {
            return Ok(ChildLineageAppend {
                outcome: ChildLineageAppendOutcome::ParentSettled,
                persisted: Vec::new(),
            });
        };
        if parent.chain_root_id != chain_root_id {
            bail!(
                "parent thread {parent_thread_id} belongs to chain {}, not {chain_root_id}",
                parent.chain_root_id
            );
        }
        if g.state_db.get_thread(child_thread_id)?.is_none() {
            bail!("child thread not found while recording lineage: {child_thread_id}");
        }
        if queries::thread_edge_exists(g.state_db.projection(), parent_thread_id, child_thread_id)?
        {
            return Ok(ChildLineageAppend {
                outcome: ChildLineageAppendOutcome::AlreadyPresent,
                persisted: Vec::new(),
            });
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(parent_thread_id)?
            .ok_or_else(|| anyhow!("parent runtime row missing: {parent_thread_id}"))?;
        if parent.status != ThreadStatus::Running.as_str()
            || runtime.stop_intent.is_some()
            || !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            return Ok(ChildLineageAppend {
                outcome: ChildLineageAppendOutcome::ParentSettled,
                persisted: Vec::new(),
            });
        }
        let persisted = append_events_locked(
            &g,
            Some(permit.cas_guard()),
            chain_root_id,
            parent_thread_id,
            &[NewEventRecord {
                event_type: ryeos_state::event_types::CHILD_THREAD_SPAWNED.to_string(),
                storage_class: "indexed".to_string(),
                payload,
            }],
        )?;
        Ok(ChildLineageAppend {
            outcome: ChildLineageAppendOutcome::Appended,
            persisted,
        })
    }
    #[tracing::instrument(
        name = "state:append_events",
        skip(self, events),
        fields(
            thread_id = %thread_id,
            chain_root_id = %chain_root_id,
            event_count = events.len(),
        )
    )]
    pub fn append_events(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Vec<PersistedEventRecord>> {
        let has_cas_events = events
            .iter()
            .any(|event| event.storage_class != "ephemeral");
        let permit = if has_cas_events {
            Some(self.acquire_write_permit()?)
        } else {
            None
        };
        let g = self.lock()?;
        if has_indexed_collection_events(events) && !self.projection_health.is_current() {
            bail!("collection event admission requires a current thread projection");
        }
        append_events_locked(
            &g,
            permit.as_ref().map(StateMutationPermit::cas_guard),
            chain_root_id,
            thread_id,
            events,
        )
    }

    #[tracing::instrument(
        name = "state:append_events_if_thread_running",
        skip(self, events),
        fields(
            thread_id = %thread_id,
            chain_root_id = %chain_root_id,
            event_count = events.len(),
        )
    )]
    pub fn append_events_if_thread_running(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Option<Vec<PersistedEventRecord>>> {
        let has_cas_events = events
            .iter()
            .any(|event| event.storage_class != "ephemeral");
        let permit = if has_cas_events {
            Some(self.acquire_write_permit()?)
        } else {
            None
        };
        let g = self.lock()?;
        if has_indexed_collection_events(events) && !self.projection_health.is_current() {
            bail!("collection event admission requires a current thread projection");
        }
        let Some(thread) = g.state_db.get_thread(thread_id)? else {
            return Ok(None);
        };
        if thread.status != ThreadStatus::Running.as_str() {
            return Ok(None);
        }
        let runtime = g
            .runtime_db
            .get_runtime_info(thread_id)?
            .ok_or_else(|| anyhow!("runtime row missing while appending events: {thread_id}"))?;
        if runtime.stop_intent.is_some()
            || !self
                .process_attachment_admission_open
                .load(Ordering::Acquire)
        {
            return Ok(None);
        }

        append_events_locked(
            &g,
            permit.as_ref().map(StateMutationPermit::cas_guard),
            chain_root_id,
            thread_id,
            events,
        )
        .map(Some)
    }

    /// Append the exact daemon-authored launch-attempt audit immediately before
    /// a claimed managed-runtime spawn. Unlike the runtime callback event
    /// boundary, this deliberately admits a still-`created` row: continuation
    /// and follow successors must remain `created` until their process is
    /// attached, but their recomputed attempt authority must be durable before
    /// the spawn handoff becomes observable.
    ///
    /// Every lifecycle and operational guard is re-checked under the same state
    /// lock as the append. This remains a daemon-only boundary; runtime-authored
    /// events continue to require an already-`running` thread through
    /// [`Self::append_events_if_thread_running`].
    #[tracing::instrument(
        name = "state:append_launch_attempt_audit",
        skip(self, events),
        fields(
            thread_id = %thread_id,
            chain_root_id = %chain_root_id,
            event_count = events.len(),
        )
    )]
    pub(crate) fn append_launch_attempt_audit(
        &self,
        chain_root_id: &str,
        thread_id: &str,
        events: &[NewEventRecord],
    ) -> Result<Vec<PersistedEventRecord>> {
        validate_launch_attempt_audit(events)?;
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("launch attempt audit thread not found: {thread_id}"))?;
        if thread.chain_root_id != chain_root_id {
            bail!(
                "launch attempt audit chain mismatch for {thread_id}: expected {}, received {chain_root_id}",
                thread.chain_root_id
            );
        }
        if !matches!(
            ThreadStatus::from_str_lossy(&thread.status),
            Some(ThreadStatus::Created | ThreadStatus::Running)
        ) {
            bail!(
                "launch attempt audit requires a created or running thread; {thread_id} is '{}'",
                thread.status
            );
        }
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            bail!("launch attempt audit refused: process attachment admission is closed");
        }
        let runtime = g.runtime_db.get_runtime_info(thread_id)?.ok_or_else(|| {
            anyhow!("runtime row missing while appending launch audit: {thread_id}")
        })?;
        if let Some(intent) = runtime.stop_intent {
            bail!(
                "launch attempt audit refused: thread {thread_id} has durable stop intent '{}'",
                intent.as_str()
            );
        }
        if runtime.pid.is_some() || runtime.pgid.is_some() || runtime.process_identity.is_some() {
            bail!(
                "launch attempt audit refused: thread {thread_id} already has a process attachment"
            );
        }
        if g.runtime_db.get_launch_claim(thread_id)?.is_none() {
            bail!("launch attempt audit refused: thread {thread_id} has no active launch claim");
        }

        append_events_locked(
            &g,
            Some(permit.cas_guard()),
            chain_root_id,
            thread_id,
            events,
        )
    }

    /// The thread a live tail of `chain_root_id` should currently follow: the
    /// owner of the chain's highest-`chain_seq` event. `None` when the chain
    /// has no events yet.
    pub fn chain_head_thread(&self, chain_root_id: &str) -> Result<Option<String>> {
        let g = self.lock()?;
        queries::chain_head_thread(g.state_db.projection(), chain_root_id)
    }

    pub fn replay_events(
        &self,
        chain_root_id: &str,
        thread_id: Option<&str>,
        after_seq: Option<i64>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<PersistedEventPage> {
        let g = self.lock()?;
        let page = queries::replay_events_bounded(
            g.state_db.projection(),
            chain_root_id,
            thread_id,
            after_seq,
            limit,
            max_serialized_bytes,
        )?;
        drop(g);
        let events = page
            .rows
            .into_iter()
            .map(|row| {
                let payload: Value = serde_json::from_slice(&row.payload).with_context(|| {
                    format!(
                        "malformed JSON payload for event {} (chain_seq {})",
                        row.event_id, row.chain_seq
                    )
                })?;
                Ok(PersistedEventRecord {
                    event_id: row.event_id,
                    event_hash: Some(row.event_hash),
                    chain_root_id: row.chain_root_id,
                    chain_seq: row.chain_seq,
                    thread_id: row.thread_id,
                    thread_seq: row.thread_seq,
                    event_type: row.event_type,
                    storage_class: row.durability,
                    ts: row.ts,
                    prev_chain_event_hash: row.prev_chain_event_hash,
                    prev_thread_event_hash: row.prev_thread_event_hash,
                    payload,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(PersistedEventPage {
            events,
            has_more: page.has_more,
        })
    }

    /// Latest durable events across every thread on the node — the feed
    /// behind the node activity lens. `exclude_types` is caller-declared
    /// (content decides what counts as noise), never a baked-in vocabulary.
    pub fn latest_node_events(
        &self,
        limit: usize,
        exclude_types: &[String],
    ) -> Result<Vec<PersistedEventRecord>> {
        let g = self.lock()?;
        let event_rows =
            queries::latest_node_events(g.state_db.projection(), limit, exclude_types)?;
        event_rows
            .into_iter()
            .map(|row| {
                let payload: Value = serde_json::from_slice(&row.payload).with_context(|| {
                    format!(
                        "malformed JSON payload for event {} (chain_seq {})",
                        row.event_id, row.chain_seq
                    )
                })?;
                Ok(PersistedEventRecord {
                    event_id: row.event_id,
                    event_hash: Some(row.event_hash),
                    chain_root_id: row.chain_root_id,
                    chain_seq: row.chain_seq,
                    thread_id: row.thread_id,
                    thread_seq: row.thread_seq,
                    event_type: row.event_type,
                    storage_class: row.durability,
                    ts: row.ts,
                    prev_chain_event_hash: row.prev_chain_event_hash,
                    prev_thread_event_hash: row.prev_thread_event_hash,
                    payload,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn latest_thread_events(
        &self,
        thread_id: &str,
        limit: usize,
    ) -> Result<Vec<PersistedEventRecord>> {
        let g = self.lock()?;
        let event_rows = queries::latest_thread_events(g.state_db.projection(), thread_id, limit)?;
        event_rows
            .into_iter()
            .map(|row| {
                let payload: Value = serde_json::from_slice(&row.payload).with_context(|| {
                    format!(
                        "malformed JSON payload for event {} (chain_seq {})",
                        row.event_id, row.chain_seq
                    )
                })?;
                Ok(PersistedEventRecord {
                    event_id: row.event_id,
                    event_hash: Some(row.event_hash),
                    chain_root_id: row.chain_root_id,
                    chain_seq: row.chain_seq,
                    thread_id: row.thread_id,
                    thread_seq: row.thread_seq,
                    event_type: row.event_type,
                    storage_class: row.durability,
                    ts: row.ts,
                    prev_chain_event_hash: row.prev_chain_event_hash,
                    prev_thread_event_hash: row.prev_thread_event_hash,
                    payload,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn append_bundle_event(
        &self,
        request: ryeos_state::BundleEventAppendRequest,
    ) -> Result<ryeos_state::BundleEventAppendResult> {
        let permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        g.state_db
            .append_bundle_event_admitted(request, g.signer.as_ref(), permit.cas_guard())
    }

    pub fn append_bundle_event_with_attachments(
        &self,
        mut request: ryeos_state::BundleEventAppendRequest,
        attachments: Vec<NewBundleEventAttachment>,
    ) -> Result<ryeos_state::BundleEventAppendResult> {
        let permit = self.acquire_write_permit()?;
        self.state_authority.ensure_guard(permit.cas_guard())?;
        let cas = self.state_authority.cas_store()?;
        request.attachments = attachments
            .into_iter()
            .map(|attachment| {
                let size_bytes = u64::try_from(attachment.bytes.len())
                    .map_err(|_| anyhow!("bundle event attachment size does not fit u64"))?;
                let stored = cas.put_blob(&attachment.bytes).with_context(|| {
                    format!("store bundle event attachment '{}' in CAS", attachment.name)
                })?;
                Ok(ryeos_state::BundleEventAttachment {
                    name: attachment.name,
                    blob_hash: stored.hash,
                    size_bytes,
                    media_type: attachment.media_type,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let g = self.lock()?;
        g.state_db
            .append_bundle_event_admitted(request, g.signer.as_ref(), permit.cas_guard())
    }

    pub fn read_bundle_event_attachment(
        &self,
        event_hash: &str,
        expected_bundle_id: &str,
        expected_event_kind: &str,
        attachment_name: &str,
    ) -> Result<(
        ryeos_state::BundleEventRecord,
        ryeos_state::BundleEventAttachment,
        Vec<u8>,
    )> {
        let record = {
            let g = self.lock()?;
            g.state_db.read_bundle_event_by_hash(event_hash)?
        };
        if record.event.bundle_id != expected_bundle_id
            || record.event.event_kind != expected_event_kind
        {
            bail!(
                "bundle event attachment identity mismatch: expected {expected_bundle_id}/{expected_event_kind}, event belongs to {}/{}",
                record.event.bundle_id,
                record.event.event_kind
            );
        }
        let attachment = record
            .event
            .attachments
            .iter()
            .find(|attachment| attachment.name == attachment_name)
            .cloned()
            .ok_or_else(|| {
                anyhow!("bundle event {event_hash} has no attachment named {attachment_name:?}")
            })?;
        let cas = self.state_authority.cas_store()?;
        let bytes = cas
            .get_blob(&attachment.blob_hash)?
            .ok_or_else(|| anyhow!("bundle event attachment blob is missing"))?;
        if bytes.len() as u64 != attachment.size_bytes {
            bail!(
                "bundle event attachment '{}' size mismatch: event says {}, CAS contains {}",
                attachment.name,
                attachment.size_bytes,
                bytes.len()
            );
        }
        Ok((record, attachment, bytes))
    }

    pub fn read_bundle_event_chain_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        chain_id: &str,
        cursor: Option<&ryeos_state::BundleEventCursor>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<ryeos_state::BundleEventChainPage> {
        let g = self.lock()?;
        g.state_db.read_bundle_event_chain_page(
            bundle_id,
            event_kind,
            chain_id,
            cursor,
            limit,
            max_serialized_bytes,
            g.signer.as_ref(),
        )
    }

    pub fn scan_bundle_events_page(
        &self,
        bundle_id: &str,
        event_kind: &str,
        cursor: Option<&ryeos_state::BundleEventCursor>,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<ryeos_state::BundleEventScanPage> {
        let g = self.lock()?;
        g.state_db.scan_bundle_events_page(
            bundle_id,
            event_kind,
            cursor,
            limit,
            max_serialized_bytes,
            g.signer.as_ref(),
        )
    }

    pub fn submit_command(&self, cmd: &NewCommandRecord) -> Result<CommandRecord> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, &cmd.thread_id)?;
        g.runtime_db.submit_command(cmd)
    }

    pub fn claim_commands(
        &self,
        thread_id: &str,
        limit: usize,
        max_serialized_bytes: usize,
    ) -> Result<Vec<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .claim_commands(thread_id, limit, max_serialized_bytes)
    }

    pub fn reset_resume_attempts(&self, thread_id: &str) -> Result<()> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = Self::authorize_runtime_pin_for_thread(&g, thread_id)?;
        g.runtime_db.reset_resume_attempts(thread_id)
    }

    /// Enqueue a detached child chain into a launch window and admit as many
    /// queued members as the window width (and optional global live ceiling)
    /// allow. Returns the chain roots admitted NOW — the caller launches
    /// them; the enqueued child is queued iff its id is absent.
    pub fn launch_window_enqueue(
        &self,
        child_chain_root_id: &str,
        window_key: &str,
        width: u32,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = g.state_db.authorize_runtime_pin(child_chain_root_id)?;
        g.runtime_db
            .launch_window_insert(child_chain_root_id, window_key, width, now_ms)?;
        g.runtime_db
            .launch_window_admit(window_key, global_live_limit, now_ms)
    }

    /// Repair membership without admitting it; used when launch metadata proves
    /// the child was originally windowed but the membership write was lost.
    pub fn launch_window_insert_only(
        &self,
        child_chain_root_id: &str,
        window_key: &str,
        width: u32,
        now_ms: i64,
    ) -> Result<bool> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let _admission = g.state_db.authorize_runtime_pin(child_chain_root_id)?;
        g.runtime_db
            .launch_window_insert(child_chain_root_id, window_key, width, now_ms)
    }

    /// Release a window slot for a chain that reached a hard terminal and
    /// admit the window's next queued members (returned for launching).
    pub fn launch_window_release(
        &self,
        child_chain_root_id: &str,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db
            .launch_window_release(child_chain_root_id, global_live_limit, now_ms)
    }

    pub fn launch_window_is_queued(&self, child_chain_root_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.launch_window_is_queued(child_chain_root_id)
    }

    pub fn launch_window_cancel_queued(
        &self,
        chain_roots: &[String],
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let mut g = self.lock()?;
        g.runtime_db
            .launch_window_cancel_queued(chain_roots, now_ms)
    }

    pub fn launch_window_cancel_members(
        &self,
        chain_roots: &[String],
        now_ms: i64,
    ) -> Result<Vec<String>> {
        self.lock()?
            .runtime_db
            .launch_window_cancel_members(chain_roots, now_ms)
    }

    pub fn launch_window_is_cancelled(&self, chain_root: &str) -> Result<bool> {
        self.lock()?
            .runtime_db
            .launch_window_is_cancelled(chain_root)
    }

    pub fn list_cancelled_window_members(&self) -> Result<Vec<String>> {
        self.lock()?.runtime_db.launch_window_cancelled_members()
    }

    pub fn discard_window_member(&self, chain_root: &str) -> Result<()> {
        self.lock()?
            .runtime_db
            .launch_window_discard_member(chain_root)
    }

    pub fn launch_window_is_member(&self, child_chain_root_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.launch_window_is_member(child_chain_root_id)
    }

    pub fn launch_window_launched_members(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.launch_window_launched_members()
    }

    pub fn launch_window_keys_with_queue(&self) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.launch_window_keys_with_queue()
    }

    pub fn launch_window_admit(
        &self,
        window_key: &str,
        global_live_limit: Option<u32>,
        now_ms: i64,
    ) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db
            .launch_window_admit(window_key, global_live_limit, now_ms)
    }

    pub fn complete_command(
        &self,
        command_id: i64,
        status: &str,
        result: Option<&Value>,
    ) -> Result<CommandRecord> {
        let g = self.lock()?;
        g.runtime_db.complete_command(command_id, status, result)
    }

    /// Read one command by id, or `None` if it does not exist.
    pub fn get_command(&self, command_id: i64) -> Result<Option<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db.get_command(command_id)
    }

    /// Whether a `kill` command was ever submitted for `thread_id` (the launcher's
    /// kill-intent marker). See [`RuntimeDb::thread_has_kill_command`].
    pub fn thread_has_kill_command(&self, thread_id: &str) -> Result<bool> {
        let g = self.lock()?;
        g.runtime_db.thread_has_kill_command(thread_id)
    }

    /// Settle every still-open command for a finalized thread (fulfilled →
    /// `completed`, else `rejected`), returning the affected records so waiters
    /// can be woken. See [`RuntimeDb::settle_open_commands`].
    pub fn settle_open_commands(
        &self,
        thread_id: &str,
        terminal_status: &str,
    ) -> Result<Vec<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db
            .settle_open_commands(thread_id, terminal_status)
    }

    /// Record that `parent_thread_id` spawned `child_thread_id` (operational
    /// lineage for cancel/kill cascade). An exact replay is idempotent;
    /// conflicting parent or relation authority is rejected. If the parent was
    /// already stop-tombstoned, atomically inherit that monotonic intent on the
    /// child before releasing the store lock, closing the late-link race.
    pub fn record_child_link(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        relation: &str,
    ) -> Result<Option<StopIntent>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        if !self
            .process_attachment_admission_open
            .load(Ordering::Acquire)
        {
            // The shutdown gate may close after a callback created its child but
            // before it reached this second durable mutation. Tombstone the child
            // first so no later attach can win, then preserve operational lineage
            // on a best-effort basis.
            let stopped = g
                .runtime_db
                .request_thread_stop(child_thread_id, StopIntent::Cancel)?;
            if let Err(error) =
                g.runtime_db
                    .record_child_link(parent_thread_id, child_thread_id, relation)
            {
                tracing::warn!(
                    parent_thread_id,
                    child_thread_id,
                    relation,
                    error = %error,
                    "failed to preserve child lineage after shutdown tombstone"
                );
            }
            return Ok(Some(stopped.stop_intent.unwrap_or(StopIntent::Cancel)));
        }
        let parent = g
            .state_db
            .get_thread(parent_thread_id)?
            .ok_or_else(|| anyhow!("parent thread {parent_thread_id} does not exist"))?;
        let child = g
            .state_db
            .get_thread(child_thread_id)?
            .ok_or_else(|| anyhow!("child thread {child_thread_id} does not exist"))?;
        let mut chain_roots = vec![parent.chain_root_id, child.chain_root_id];
        chain_roots.sort();
        chain_roots.dedup();
        let mut _admissions = Vec::with_capacity(chain_roots.len());
        for chain_root_id in chain_roots {
            _admissions.push(g.state_db.authorize_runtime_pin(&chain_root_id)?);
        }
        let inherited_stop = g
            .runtime_db
            .get_runtime_info(parent_thread_id)?
            .ok_or_else(|| anyhow::anyhow!("parent runtime row missing: {parent_thread_id}"))?
            .stop_intent;
        // A dispatch admitted immediately before its parent finalized may reach
        // this linkage point afterward. Preserve the lineage and tombstone the
        // newly-created child in the same runtime-DB transaction. An exact
        // replay is not a late child and must not synthesize a new stop merely
        // because the parent has since continued. Follow children are linked
        // before the generic launch path reaches this point, so its later
        // `dispatch` registration must replay the authoritative `follow`
        // relation rather than contradict it. Continuations are different:
        // their predecessor is intentionally terminal/continued.
        let existing_relation = g.runtime_db.child_link_relation(child_thread_id)?;
        let replays_follow_relation =
            relation == "dispatch" && existing_relation.as_deref() == Some("follow");
        let effective_relation = if replays_follow_relation {
            "follow"
        } else {
            relation
        };
        let stop_policy = if let Some(intent) = inherited_stop {
            runtime_db::ChildLinkStopPolicy::Always(intent)
        } else if relation == "dispatch"
            && !replays_follow_relation
            && is_terminal_status(&parent.status)
        {
            runtime_db::ChildLinkStopPolicy::IfInserted(StopIntent::Cancel)
        } else {
            runtime_db::ChildLinkStopPolicy::None
        };
        let (_, effective_stop) = g.runtime_db.record_child_link_with_stop_policy(
            parent_thread_id,
            child_thread_id,
            effective_relation,
            stop_policy,
        )?;
        Ok(effective_stop)
    }

    /// Every transitive descendant thread id of `root_thread_id`, breadth-first
    /// in spawn order (excludes `root`).
    pub fn descendant_thread_ids(&self, root_thread_id: &str) -> Result<Vec<String>> {
        let g = self.lock()?;
        g.runtime_db.descendant_thread_ids(root_thread_id)
    }

    pub fn get_facets(&self, thread_id: &str) -> Result<Vec<(String, String)>> {
        let facet_rows = {
            let g = self.lock()?;
            let (items, content_bytes) =
                queries::thread_facet_stats(g.state_db.projection(), thread_id)?;
            ensure_facet_collection_bounds(thread_id, items, content_bytes)?;
            queries::get_facets_bounded(
                g.state_db.projection(),
                thread_id,
                MAX_THREAD_FACET_ITEMS,
                MAX_THREAD_FACET_KEY_BYTES,
                MAX_THREAD_FACET_VALUE_BYTES,
                MAX_THREAD_FACET_CONTENT_BYTES,
            )?
        };
        let mut facets = Vec::with_capacity(facet_rows.len());
        let mut response_bytes = b"{}".len();
        for row in facet_rows {
            let value = String::from_utf8(row.value)
                .with_context(|| format!("thread {thread_id} facet value is not UTF-8"))?;
            let key_bytes = serde_json::to_vec(&row.key)?.len();
            let value_bytes = serde_json::to_vec(&value)?.len();
            response_bytes = response_bytes
                .checked_add(key_bytes)
                .and_then(|bytes| bytes.checked_add(value_bytes))
                .and_then(|bytes| bytes.checked_add(1)) // colon
                .and_then(|bytes| bytes.checked_add(usize::from(!facets.is_empty())))
                .ok_or_else(|| anyhow!("thread facet response size overflow"))?;
            if response_bytes > MAX_THREAD_FACET_RESPONSE_BYTES {
                bail!(
                    "thread {thread_id} facet response exceeds the {MAX_THREAD_FACET_RESPONSE_BYTES}-byte maximum"
                );
            }
            facets.push((row.key, value));
        }
        Ok(facets)
    }

    /// A graph thread's current `(node, step)` from its latest
    /// `graph_step_started` — the cheap live "where is it now". See
    /// [`queries::current_graph_node`].
    pub fn current_graph_node(&self, thread_id: &str) -> Result<Option<(String, u32)>> {
        let g = self.lock()?;
        queries::current_graph_node(g.state_db.projection(), thread_id)
    }
}

/// Whether a thread's persisted `status` is terminal. The canonical predicate —
/// `ThreadTerminalStatus`'s string mapping omits daemon-owned `timed_out`, so it
/// is not usable for this; callers outside this module (e.g. the cancel cascade)
/// reuse this rather than re-listing the statuses.
pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "completed" | "failed" | "cancelled" | "killed" | "timed_out" | "continued"
    )
}

fn terminal_event_type(status: &str) -> Result<&'static str> {
    match status {
        "completed" => Ok("thread_completed"),
        "failed" => Ok("thread_failed"),
        "cancelled" => Ok("thread_cancelled"),
        "killed" => Ok("thread_killed"),
        "timed_out" => Ok("thread_timed_out"),
        "continued" => Ok("thread_continued"),
        other => bail!("invalid terminal event status: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::contracts::{EffectivePrincipal, ExecutionHints, Principal, ProjectContext};
    use tempfile::tempdir;

    fn test_store() -> StateStore {
        let tmp = tempdir().expect("tempdir").keep();
        let runtime_state_dir = tmp.join(".ai/state");
        let identity = crate::identity::NodeIdentity::create(&tmp.join("node-key.pem"))
            .expect("test node identity");
        let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut head_trust = ryeos_state::refs::TrustStore::new();
        head_trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        StateStore::new_with_head_trust(
            tmp.clone(),
            runtime_state_dir.clone(),
            runtime_state_dir.join("runtime.sqlite3"),
            signer,
            WriteBarrier::new(),
            Arc::new(head_trust),
        )
        .expect("state store")
    }

    #[tokio::test]
    async fn unbound_launch_cancel_commits_then_aborts_registered_task() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-planning", "fp:owner")
            .expect("reserve planning");
        assert!(store
            .get_thread("T-planning")
            .expect("read absent authoritative row")
            .is_none());
        let task = tokio::spawn(std::future::pending::<()>());
        store
            .register_launch_task_abort("T-planning", task.abort_handle())
            .expect("register abort signal");

        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("cancel planning"),
            Some(LaunchCancellationResolution::Cancelled)
        );
        assert!(task.await.expect_err("task must be aborted").is_cancelled());
        store
            .settle_launch_planning_task_exit("T-planning")
            .expect("cancelled task exit must preserve cancelled outcome");
        assert!(store
            .get_thread("T-planning")
            .expect("read absent authoritative row after cancel")
            .is_none());
        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("read terminal planning outcome"),
            Some(LaunchCancellationResolution::Terminal {
                state: "cancelled".to_string(),
                outcome_code: Some("cancelled_by_requester".to_string()),
            })
        );
    }

    #[tokio::test]
    async fn cancel_before_task_registration_aborts_at_registration() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-late-registration", "fp:owner")
            .expect("reserve planning");
        store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("cancel planning");
        let task = tokio::spawn(std::future::pending::<()>());
        store
            .register_launch_task_abort("T-late-registration", task.abort_handle())
            .expect("late registration");
        assert!(task.await.expect_err("task must be aborted").is_cancelled());
    }

    #[tokio::test]
    async fn launch_abort_registry_capacity_refusal_aborts_and_can_be_settled() {
        let store = test_store();
        store
            .reserve_launch_planning("T-capacity-one", "fp:owner")
            .expect("reserve first planning");
        let refused_launch_id = store
            .reserve_launch_planning("T-capacity-two", "fp:owner")
            .expect("reserve second planning");
        let first = tokio::spawn(std::future::pending::<()>());
        store
            .register_launch_task_abort_bounded("T-capacity-one", first.abort_handle(), 1)
            .expect("register first signal");
        let refused = tokio::spawn(std::future::pending::<()>());
        assert!(matches!(
            store.register_launch_task_abort_bounded("T-capacity-two", refused.abort_handle(), 1,),
            Err(LaunchTaskAbortRegistrationError::CapacityExceeded)
        ));
        assert!(refused
            .await
            .expect_err("capacity-refused task must be aborted")
            .is_cancelled());

        store
            .settle_launch_planning_task_exit("T-capacity-two")
            .expect("settle refused planning");
        assert_eq!(
            store
                .cancel_launch_planning(&refused_launch_id, "fp:owner")
                .expect("read settled planning"),
            Some(LaunchCancellationResolution::Terminal {
                state: "failed".to_string(),
                outcome_code: Some("thread_creation_failed".to_string()),
            })
        );
        first.abort();
        assert!(first
            .await
            .expect_err("first task must be aborted during cleanup")
            .is_cancelled());
    }

    #[test]
    fn planning_reservation_maps_only_typed_capacity_and_preserves_internal_errors() {
        let capacity = map_launch_planning_reservation_error(
            anyhow::Error::new(LaunchPlanningCapacityExceeded)
                .context("reserve durable planning row"),
        );
        assert!(matches!(
            capacity,
            LaunchPlanningReservationError::CapacityExceeded(_)
        ));

        let internal =
            map_launch_planning_reservation_error(anyhow::anyhow!("runtime database unavailable"));
        assert!(matches!(
            internal,
            LaunchPlanningReservationError::Internal(_)
        ));
    }

    #[test]
    fn foreign_launch_id_is_indistinguishable_from_nonexistent_and_does_not_cancel() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-owned-planning", "fp:owner")
            .expect("reserve planning");
        let foreign = store
            .cancel_launch_planning(&launch_id, "fp:other")
            .expect("hide non-owner");
        let nonexistent = store
            .cancel_launch_planning("L-does-not-exist", "fp:other")
            .expect("hide nonexistent");
        assert_eq!(foreign, nonexistent);
        assert_eq!(foreign, None);
        store
            .ensure_launch_planning_active("T-owned-planning")
            .expect("non-owner must not mutate planning");
    }

    #[tokio::test]
    async fn row_commit_before_handoff_routes_cancel_to_thread_without_aborting_launch_task() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-row-before-handoff", "fp:owner")
            .expect("reserve planning");
        let task = tokio::spawn(std::future::pending::<()>());
        store
            .register_launch_task_abort("T-row-before-handoff", task.abort_handle())
            .expect("register launch task");
        let mut root = thread_record("T-row-before-handoff", "T-row-before-handoff");
        root.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&root)
            .expect("commit and bind authoritative root");

        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("resolve post-row cancel"),
            Some(LaunchCancellationResolution::Bound {
                thread_id: "T-row-before-handoff".to_string(),
            })
        );
        assert!(
            !task.is_finished(),
            "binding must transfer cancellation to the durable thread rather than aborting its launch task"
        );
        task.abort();
        assert!(task
            .await
            .expect_err("test cleanup must abort launch task")
            .is_cancelled());
        store
            .settle_launch_planning_task_exit("T-row-before-handoff")
            .expect("post-bind task exit must preserve thread binding");
        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("read binding after task exit"),
            Some(LaunchCancellationResolution::Bound {
                thread_id: "T-row-before-handoff".to_string(),
            })
        );
    }

    #[test]
    fn cancel_after_handoff_resolves_to_canonical_durable_thread_stop() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-after-handoff", "fp:owner")
            .expect("reserve planning");
        let mut root = thread_record("T-after-handoff", "T-after-handoff");
        root.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&root)
            .expect("commit and bind authoritative root");

        let foreign = store
            .cancel_launch_planning(&launch_id, "fp:other")
            .expect("hide foreign handed-off launch");
        let nonexistent = store
            .cancel_launch_planning("L-missing-after-handoff", "fp:other")
            .expect("hide nonexistent launch");
        assert_eq!(foreign, nonexistent);
        assert_eq!(foreign, None);

        let resolution = store
            .cancel_launch_planning(&launch_id, "fp:owner")
            .expect("resolve handed-off launch")
            .expect("owned launch");
        let LaunchCancellationResolution::Bound { thread_id } = resolution else {
            panic!("handed-off launch must resolve to its durable thread");
        };
        let runtime = store
            .request_thread_stop(&thread_id, StopIntent::Cancel)
            .expect("request canonical durable stop");
        assert_eq!(runtime.stop_intent, Some(StopIntent::Cancel));
        assert_eq!(thread_id, "T-after-handoff");
    }

    #[test]
    fn pre_bind_augmentation_or_launch_task_exit_settles_failed() {
        let store = test_store();
        let launch_id = store
            .reserve_launch_planning("T-pre-bind-task-exit", "fp:owner")
            .expect("reserve planning");

        store
            .settle_launch_planning_task_exit("T-pre-bind-task-exit")
            .expect("settle pre-bind task exit");

        assert!(store
            .get_thread("T-pre-bind-task-exit")
            .expect("read absent authoritative row")
            .is_none());
        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("read settled launch outcome"),
            Some(LaunchCancellationResolution::Terminal {
                state: "failed".to_string(),
                outcome_code: Some("thread_creation_failed".to_string()),
            })
        );
    }

    #[test]
    fn root_publication_binds_planning_before_return() {
        let store = test_store();
        let thread_id = "T-root-direct-bind";
        let launch_id = store
            .reserve_launch_planning(thread_id, "fp:owner")
            .expect("reserve root planning");
        let mut root = thread_record(thread_id, thread_id);
        root.requested_by = Some("fp:owner".to_string());

        let publication = store
            .create_root_thread_with_events_and_launch_metadata(&root, Vec::new(), None)
            .expect("publish root and bind planning");
        assert_eq!(publication.successor.thread_id, thread_id);
        assert_eq!(publication.successor.chain_root_id, thread_id);
        assert!(publication.successor.upstream_thread_id.is_none());
        assert_eq!(publication.successor.status, ThreadStatus::Created.as_str());
        assert!(publication.successor.runtime.pid.is_none());
        assert!(publication.successor.runtime.launch_metadata.is_none());

        let g = store.lock().expect("inspect root publication");
        let planning = g
            .runtime_db
            .launch_planning_by_thread(thread_id)
            .expect("read planning directly")
            .expect("root planning record");
        assert_eq!(planning.launch_id, launch_id);
        assert_eq!(planning.state, "bound");
        assert_eq!(planning.bound_thread_id.as_deref(), Some(thread_id));
        assert!(g
            .state_db
            .get_thread(thread_id)
            .expect("read authoritative root directly")
            .is_some());
    }

    #[test]
    fn launch_cancel_and_authoritative_root_bind_have_one_atomic_winner() {
        let store = Arc::new(test_store());
        let launch_id = store
            .reserve_launch_planning("T-bind-race", "fp:owner")
            .expect("reserve planning");
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let creator_store = store.clone();
        let creator_barrier = barrier.clone();
        let creator = std::thread::spawn(move || {
            creator_barrier.wait();
            let mut root = thread_record("T-bind-race", "T-bind-race");
            root.requested_by = Some("fp:owner".to_string());
            creator_store.create_thread_for_test(&root)
        });

        let canceller_store = store.clone();
        let canceller_barrier = barrier.clone();
        let canceller = std::thread::spawn(move || {
            canceller_barrier.wait();
            canceller_store.cancel_launch_planning(&launch_id, "fp:owner")
        });

        barrier.wait();
        let created = creator.join().expect("creator thread");
        let cancelled = canceller
            .join()
            .expect("canceller thread")
            .expect("cancel resolution")
            .expect("owned planning record");
        match (created, cancelled) {
            (Ok(_), LaunchCancellationResolution::Bound { thread_id }) => {
                assert_eq!(thread_id, "T-bind-race")
            }
            (Err(error), LaunchCancellationResolution::Cancelled) => {
                assert!(
                    error
                        .chain()
                        .any(|cause| cause.is::<LaunchPlanningInactive>()),
                    "cancel-won publication must retain typed planning-inactive authority: {error:#}"
                );
            }
            (created, cancelled) => {
                panic!("invalid cancel-vs-bind race outcome: created={created:?}, cancelled={cancelled:?}")
            }
        }
    }

    #[test]
    fn continuation_publication_binds_planning_before_return() {
        let store = test_store();
        let source_thread_id = "T-continuation-direct-bind-source";
        let successor_thread_id = "T-continuation-direct-bind-successor";
        let mut source = thread_record(source_thread_id, source_thread_id);
        source.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&source)
            .expect("create continuation source");
        let launch_id = store
            .reserve_launch_planning(successor_thread_id, "fp:owner")
            .expect("reserve continuation planning");
        let mut successor = thread_record(successor_thread_id, source_thread_id);
        successor.requested_by = Some("fp:owner".to_string());

        let publication = store
            .create_continuation_admitted(
                &successor,
                source_thread_id,
                source_thread_id,
                Some("chained_resume"),
                Vec::new(),
                None,
            )
            .expect("publish continuation and bind planning");
        assert_eq!(publication.successor.thread_id, successor_thread_id);
        assert_eq!(publication.successor.chain_root_id, source_thread_id);
        assert_eq!(
            publication.successor.upstream_thread_id.as_deref(),
            Some(source_thread_id)
        );
        assert_eq!(publication.successor.status, ThreadStatus::Created.as_str());
        assert!(publication.successor.runtime.pid.is_none());
        assert!(publication.successor.runtime.launch_metadata.is_none());

        let g = store.lock().expect("inspect continuation publication");
        let planning = g
            .runtime_db
            .launch_planning_by_thread(successor_thread_id)
            .expect("read planning directly")
            .expect("continuation planning record");
        assert_eq!(planning.launch_id, launch_id);
        assert_eq!(planning.state, "bound");
        assert_eq!(
            planning.bound_thread_id.as_deref(),
            Some(successor_thread_id)
        );
        assert!(g
            .state_db
            .get_thread(successor_thread_id)
            .expect("read authoritative successor directly")
            .is_some());
        assert_eq!(
            queries::continuation_successor(g.state_db.projection(), source_thread_id)
                .expect("read continuation edge directly")
                .as_deref(),
            Some(successor_thread_id)
        );
    }

    #[test]
    fn cancelled_planning_refuses_continuation_publication_with_typed_inactivity() {
        let store = test_store();
        let source_thread_id = "T-continuation-cancelled-source";
        let successor_thread_id = "T-continuation-cancelled-successor";
        let mut source = thread_record(source_thread_id, source_thread_id);
        source.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&source)
            .expect("create continuation source");
        let launch_id = store
            .reserve_launch_planning(successor_thread_id, "fp:owner")
            .expect("reserve continuation planning");
        assert_eq!(
            store
                .cancel_launch_planning(&launch_id, "fp:owner")
                .expect("cancel continuation planning"),
            Some(LaunchCancellationResolution::Cancelled)
        );

        let mut successor = thread_record(successor_thread_id, source_thread_id);
        successor.requested_by = Some("fp:owner".to_string());
        let error = store
            .create_continuation_for_test(
                &successor,
                source_thread_id,
                source_thread_id,
                Some("chained_resume"),
            )
            .expect_err("cancelled planning must fence continuation publication");
        assert!(
            error
                .chain()
                .any(|cause| cause.is::<LaunchPlanningInactive>()),
            "continuation publication must retain typed planning inactivity: {error:#}"
        );
        assert!(store
            .get_thread(successor_thread_id)
            .expect("read absent continuation successor")
            .is_none());
        let source = store
            .get_thread(source_thread_id)
            .expect("read unchanged continuation source")
            .expect("continuation source");
        assert_eq!(source.status, ThreadStatus::Created.as_str());
        assert!(source.successor_thread_id.is_none());
    }

    #[test]
    fn launch_cancel_and_authoritative_continuation_bind_have_one_atomic_winner() {
        let store = Arc::new(test_store());
        let source_thread_id = "T-continuation-bind-race-source";
        let successor_thread_id = "T-continuation-bind-race-successor";
        let mut source = thread_record(source_thread_id, source_thread_id);
        source.requested_by = Some("fp:owner".to_string());
        store
            .create_thread_for_test(&source)
            .expect("create continuation source");
        let launch_id = store
            .reserve_launch_planning(successor_thread_id, "fp:owner")
            .expect("reserve continuation planning");
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let creator_store = store.clone();
        let creator_barrier = barrier.clone();
        let creator = std::thread::spawn(move || {
            creator_barrier.wait();
            let mut successor = thread_record(successor_thread_id, source_thread_id);
            successor.requested_by = Some("fp:owner".to_string());
            creator_store.create_continuation_for_test(
                &successor,
                source_thread_id,
                source_thread_id,
                Some("chained_resume"),
            )
        });

        let canceller_store = store.clone();
        let canceller_barrier = barrier.clone();
        let canceller = std::thread::spawn(move || {
            canceller_barrier.wait();
            canceller_store.cancel_launch_planning(&launch_id, "fp:owner")
        });

        barrier.wait();
        let created = creator.join().expect("creator thread");
        let cancelled = canceller
            .join()
            .expect("canceller thread")
            .expect("cancel resolution")
            .expect("owned planning record");
        match (created, cancelled) {
            (Ok(_), LaunchCancellationResolution::Bound { thread_id }) => {
                assert_eq!(thread_id, successor_thread_id);
                let source = store
                    .get_thread(source_thread_id)
                    .expect("read continued source")
                    .expect("continuation source");
                assert_eq!(source.status, ThreadStatus::Continued.as_str());
                assert_eq!(
                    source.successor_thread_id.as_deref(),
                    Some(successor_thread_id)
                );
                assert!(store
                    .get_thread(successor_thread_id)
                    .expect("read published successor")
                    .is_some());
            }
            (Err(error), LaunchCancellationResolution::Cancelled) => {
                assert!(
                    error
                        .chain()
                        .any(|cause| cause.is::<LaunchPlanningInactive>()),
                    "cancel-won continuation publication must retain typed planning inactivity: {error:#}"
                );
                let source = store
                    .get_thread(source_thread_id)
                    .expect("read unchanged source")
                    .expect("continuation source");
                assert_eq!(source.status, ThreadStatus::Created.as_str());
                assert!(source.successor_thread_id.is_none());
                assert!(store
                    .get_thread(successor_thread_id)
                    .expect("read absent successor")
                    .is_none());
            }
            (created, cancelled) => {
                panic!(
                    "invalid continuation cancel-vs-bind race outcome: \
                     created={created:?}, cancelled={cancelled:?}"
                )
            }
        }
    }

    #[test]
    fn direct_attachment_fork_waits_for_state_store_descriptor_scope() {
        let store = Arc::new(test_store());
        let guard = store.lock().expect("hold state store");
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let pending = lillux::spawn_awaiting_attachment(lillux::SubprocessRequest {
                cmd: "/bin/sh".to_string(),
                argv0: None,
                args: vec!["-c".to_string(), "exit 0".to_string()],
                cwd: None,
                envs: Vec::new(),
                stdin_data: None,
                timeout: 5.0,
                limits: None,
                inherited_fds: Vec::new(),
                supervised_status: None,
            })
            .expect("spawn after state store scope quiesces");
            sender.send(pending).expect("publish pending process");
        });

        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "direct child forked while the StateStore descriptor scope was retained"
        );
        drop(guard);

        let pending = receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("direct fork resumes after StateStore scope");
        pending.abort_and_reap().expect("abort held test target");
        worker.join().expect("spawn worker");
    }

    #[test]
    fn direct_projection_access_fails_closed_while_repair_is_pending() {
        let store = test_store();
        ryeos_state::ProjectionRepairSink::request_repair(
            &*store.projection_health(),
            ryeos_state::ProjectionRepairRequest {
                chain_root_id: "T-root".into(),
                committed_head_hash: "head".into(),
                operation: "append_events",
                error: "projection failed".into(),
            },
        );
        let error = store
            .with_projection(|_| Ok(()))
            .expect_err("stale projection read must fail");
        assert!(error.to_string().contains("not current"));
    }

    #[test]
    fn settlement_cost_boundary_rejects_invalid_values() {
        let valid = ryeos_engine::contracts::FinalCost {
            turns: 0,
            input_tokens: 1,
            output_tokens: 2,
            spend: 0.01,
            provider: None,
            basis: None,
            metadata: None,
        };
        assert!(validate_final_cost_for_settlement(&valid).is_ok());

        for invalid in [
            ryeos_engine::contracts::FinalCost {
                spend: -0.01,
                ..valid.clone()
            },
            ryeos_engine::contracts::FinalCost {
                spend: f64::NAN,
                ..valid.clone()
            },
            ryeos_engine::contracts::FinalCost {
                input_tokens: i64::MAX as u64 + 1,
                ..valid.clone()
            },
            ryeos_engine::contracts::FinalCost {
                output_tokens: i64::MAX as u64 + 1,
                ..valid.clone()
            },
            ryeos_engine::contracts::FinalCost {
                basis: Some("estimated".to_string()),
                ..valid.clone()
            },
        ] {
            assert!(validate_final_cost_for_settlement(&invalid).is_err());
        }
    }

    fn follow_waiter_for_admission() -> runtime_db::FollowWaiter {
        runtime_db::FollowWaiter {
            follow_key: "follow-key".to_string(),
            parent_thread_id: "T-parent".to_string(),
            parent_chain_root_id: "T-parent".to_string(),
            parent_successor_thread_id: Some("T-successor".to_string()),
            follow_node: "fanout".to_string(),
            graph_run_id: "run-1".to_string(),
            step_count: 1,
            frontier_id: None,
            fanout: true,
            expected_children: 2,
            child_project_authority: None,
            children: vec![runtime_db::FollowWaiterChild {
                item_index: 0,
                item_ref: "tool:test/one".to_string(),
                spec_hash: "spec".to_string(),
                child_thread_id: "T-child".to_string(),
                child_chain_root_id: "T-child".to_string(),
                sealed_root_request:
                    crate::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture(),
                terminal_thread_id: None,
                terminal_status: None,
                terminal_envelope: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            }],
            phase: runtime_db::follow_phase::RESERVED.to_string(),
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn follow_terminal_admission_reserves_space_for_pending_children() {
        let waiter = follow_waiter_for_admission();
        let candidate = json!({
            "success": true,
            "child_thread_id": "T-child",
            "status": "completed",
            "result": 1,
            "outputs": null,
            "warnings": [],
            "cost": null,
        });

        validate_prospective_follow_resume_payload(&waiter, "T-child", &candidate).unwrap();
    }

    #[test]
    fn oversized_follow_terminal_is_replaced_before_persistence() {
        let waiter = follow_waiter_for_admission();
        let candidate = json!({
            "success": true,
            "child_thread_id": "T-terminal",
            "status": "completed",
            "result": "x".repeat(checkpoint_shape_limits().max_result_bytes),
            "outputs": null,
            "warnings": [],
            "cost": {
                "input_tokens": 11,
                "output_tokens": 7,
                "total_usd": 0.03,
            },
        });

        let (admitted, degraded) =
            admit_follow_terminal_envelope(&waiter, "T-child", "T-terminal", &candidate).unwrap();
        assert!(degraded);
        assert_eq!(admitted["success"], false);
        assert_eq!(admitted["child_thread_id"], "T-terminal");
        assert_eq!(admitted["result"]["code"], FOLLOW_ENVELOPE_LIMIT_CODE);
        assert_eq!(admitted["cost"]["input_tokens"], 11);
        assert_eq!(admitted["cost"]["output_tokens"], 7);
    }

    #[test]
    fn follow_terminal_admission_rejects_mismatched_child_identity() {
        let waiter = follow_waiter_for_admission();
        let candidate = json!({
            "success": true,
            "child_thread_id": "T-other",
            "status": "completed",
            "result": 1,
            "outputs": null,
            "warnings": [],
            "cost": null,
        });
        let error = admit_follow_terminal_envelope(&waiter, "T-child", "T-terminal", &candidate)
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not match terminal child"), "{error}");
    }

    #[test]
    fn impossible_follow_cohort_is_rejected_before_reservation() {
        let seed = runtime_db::NewFollowWaiter {
            follow_key: "too-wide".to_string(),
            parent_thread_id: "T-parent".to_string(),
            parent_chain_root_id: "T-parent".to_string(),
            follow_node: "fanout".to_string(),
            graph_run_id: "run-1".to_string(),
            step_count: 1,
            frontier_id: None,
            fanout: true,
            expected_children: u32::MAX,
            child_project_authority: None,
        };

        let error = validate_follow_reservation_shape(&seed).unwrap_err();
        assert!(error.to_string().contains("maximum"));
    }

    fn thread_record(thread_id: &str, chain_root_id: &str) -> NewThreadRecord {
        let captured_history_policy = (thread_id == chain_root_id).then(|| {
            let hash = "a".repeat(64);
            ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: "directive:test".to_string(),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
                },
            }
        });
        NewThreadRecord {
            thread_id: thread_id.to_string(),
            chain_root_id: chain_root_id.to_string(),
            kind: "directive".to_string(),
            item_ref: "directive:test".to_string(),
            executor_ref: "native:test".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            upstream_thread_id: None,
            requested_by: Some("fp:test".to_string()),
            project_root: None,
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
            base_project_snapshot_hash: None,
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy,
        }
    }

    fn launch_attempt_audit_events() -> Vec<NewEventRecord> {
        LAUNCH_ATTEMPT_AUDIT_TYPES
            .into_iter()
            .enumerate()
            .map(|(index, event_type)| NewEventRecord {
                event_type: event_type.as_str().to_string(),
                storage_class: event_type.storage_class().as_str().to_string(),
                payload: json!({"fixture": index}),
            })
            .collect()
    }

    fn replayed_event_types(store: &StateStore, thread_id: &str) -> Vec<String> {
        store
            .replay_events(thread_id, Some(thread_id), None, 32, 1024 * 1024)
            .expect("replay thread events")
            .events
            .into_iter()
            .map(|event| event.event_type)
            .collect()
    }

    #[test]
    fn fresh_launch_reservation_precedes_thread_publication() {
        let store = test_store();
        let thread_id = "T-fresh-launch-reservation";

        assert_eq!(
            store
                .reserve_fresh_thread_launch(thread_id, "claim-fresh", "daemon:test")
                .expect("reserve fresh launch"),
            runtime_db::LaunchClaimOutcome::Claimed
        );
        assert!(store
            .get_thread(thread_id)
            .expect("inspect unpublished thread")
            .is_none());

        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("publish reserved thread");
        let claim = store
            .get_launch_claim(thread_id)
            .expect("read launch claim")
            .expect("claim remains attached after publication");
        assert_eq!(claim.claim_id, "claim-fresh");
        assert!(store
            .release_thread_launch_claim(thread_id, "claim-fresh")
            .expect("release launch reservation"));
    }

    #[test]
    fn claimed_created_thread_accepts_exact_launch_attempt_audit() {
        let store = test_store();
        let thread_id = "T-launch-audit";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create thread");
        let audit = launch_attempt_audit_events();

        // The runtime-authored boundary remains running-only.
        assert!(store
            .append_events_if_thread_running(thread_id, thread_id, &audit)
            .expect("runtime append guard")
            .is_none());
        assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);

        assert_eq!(
            store
                .claim_thread_launch(thread_id, "claim-audit", "daemon:test")
                .expect("claim launch"),
            runtime_db::LaunchClaimOutcome::Claimed
        );
        let persisted = store
            .append_launch_attempt_audit(thread_id, thread_id, &audit)
            .expect("append claimed launch audit");
        assert_eq!(persisted.len(), 3);
        assert_eq!(
            persisted
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            LAUNCH_ATTEMPT_AUDIT_TYPES
                .iter()
                .map(|event_type| event_type.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            replayed_event_types(&store, thread_id),
            [
                "thread_created",
                ryeos_runtime::RuntimeEventType::AsLaunchedResolution.as_str(),
                ryeos_runtime::RuntimeEventType::AsLaunchedRefBindings.as_str(),
                ryeos_runtime::RuntimeEventType::RuntimeLaunchFacts.as_str(),
            ]
        );
    }

    #[test]
    fn launch_attempt_audit_rejections_do_not_mutate_the_chain() {
        let store = test_store();
        let thread_id = "T-launch-audit-reject";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create thread");
        let audit = launch_attempt_audit_events();

        let unclaimed = store
            .append_launch_attempt_audit(thread_id, thread_id, &audit)
            .expect_err("unclaimed launch audit must fail");
        assert!(unclaimed.to_string().contains("no active launch claim"));
        assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);

        assert_eq!(
            store
                .claim_thread_launch(thread_id, "claim-reject", "daemon:test")
                .expect("claim launch"),
            runtime_db::LaunchClaimOutcome::Claimed
        );
        let mut wrong_event = audit.clone();
        wrong_event.swap(0, 1);
        let wrong_event_error = store
            .append_launch_attempt_audit(thread_id, thread_id, &wrong_event)
            .expect_err("out-of-order audit must fail");
        assert!(wrong_event_error.to_string().contains("event 0"));
        assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);

        let mut wrong_storage = audit;
        wrong_storage[0].storage_class = "journal".to_string();
        let wrong_storage_error = store
            .append_launch_attempt_audit(thread_id, thread_id, &wrong_storage)
            .expect_err("non-canonical audit storage must fail");
        assert!(wrong_storage_error
            .to_string()
            .contains("canonical storage class"));
        assert_eq!(replayed_event_types(&store, thread_id), ["thread_created"]);
    }

    #[test]
    fn launch_attempt_audit_rejects_terminal_and_process_attached_rows_without_mutation() {
        let audit = launch_attempt_audit_events();

        let terminal_store = test_store();
        let terminal_id = "T-launch-audit-terminal";
        terminal_store
            .create_thread_for_test(&thread_record(terminal_id, terminal_id))
            .expect("create terminal fixture");
        terminal_store
            .claim_thread_launch(terminal_id, "claim-terminal", "daemon:test")
            .expect("claim terminal fixture");
        terminal_store
            .finalize_thread(
                terminal_id,
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_string(),
                    outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                },
            )
            .expect("finalize fixture");
        let terminal_before = replayed_event_types(&terminal_store, terminal_id);
        let terminal_error = terminal_store
            .append_launch_attempt_audit(terminal_id, terminal_id, &audit)
            .expect_err("terminal launch audit must fail");
        assert!(terminal_error
            .to_string()
            .contains("requires a created or running thread"));
        assert_eq!(
            replayed_event_types(&terminal_store, terminal_id),
            terminal_before
        );

        let attached_store = test_store();
        let attached_id = "T-launch-audit-attached";
        attached_store
            .create_thread_for_test(&thread_record(attached_id, attached_id))
            .expect("create attached fixture");
        attached_store
            .claim_thread_launch(attached_id, "claim-attached", "daemon:test")
            .expect("claim attached fixture");
        attached_store
            .attach_thread_process(
                attached_id,
                12345,
                67890,
                &crate::process::ExecutionProcessIdentity {
                    schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
                    boot_id: "test-boot".to_string(),
                    target_pid: 12345,
                    target_start_time_ticks: 10,
                    group_leader_pid: 67890,
                    group_leader_start_time_ticks: 20,
                },
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect("attach fixture process");
        let attached_before = replayed_event_types(&attached_store, attached_id);
        let attached_error = attached_store
            .append_launch_attempt_audit(attached_id, attached_id, &audit)
            .expect_err("process-attached launch audit must fail");
        assert!(attached_error.to_string().contains("process attachment"));
        assert_eq!(
            replayed_event_types(&attached_store, attached_id),
            attached_before
        );
    }

    #[test]
    fn pre_release_attachment_is_strict_and_release_observes_stop_tombstone() {
        let store = test_store();
        let thread_id = "T-pre-release-attachment";
        store
            .create_thread_for_test(&thread_record(thread_id, thread_id))
            .expect("create attachment fixture");
        let identity = crate::process::ExecutionProcessIdentity {
            schema_version: crate::process::PROCESS_IDENTITY_SCHEMA_VERSION,
            boot_id: "test-boot".to_string(),
            target_pid: 12345,
            target_start_time_ticks: 10,
            group_leader_pid: 12345,
            group_leader_start_time_ticks: 10,
        };
        store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect("attach exact held identity");
        let repeat = store
            .attach_new_thread_process(
                thread_id,
                identity.target_pid,
                identity.group_leader_pid,
                &identity,
                &crate::launch_metadata::RuntimeLaunchMetadata::default(),
                None,
            )
            .expect_err("pre-release exact repeat must be rejected");
        assert!(repeat.to_string().contains("already attached"));

        store
            .authorize_attached_process_release(thread_id, &identity, None)
            .expect("release exact attached identity");
        store
            .request_thread_stop(thread_id, StopIntent::Cancel)
            .expect("persist stop tombstone");
        let stopped = store
            .authorize_attached_process_release(thread_id, &identity, None)
            .expect_err("stop tombstone must fence release");
        assert!(stopped.to_string().contains("cancel request"));
    }

    fn continuation_resume_context(
        project_context: ProjectContext,
    ) -> crate::launch_metadata::ResumeContext {
        let stable_project_identity = match &project_context {
            ProjectContext::LocalPath { path } => Some(
                crate::launch_metadata::StableProjectIdentity::from_path(path, "site:test")
                    .unwrap(),
            ),
            _ => None,
        };
        let local_overlay_root = Some(PathBuf::from("/work/project"));
        let original_snapshot_hash = Some("a".repeat(64));
        let project_root = PathBuf::from("/work/project");
        let project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            format!("local:{}", project_root.display()),
            Some(project_root),
            original_snapshot_hash.clone().unwrap(),
            ryeos_state::objects::PinnedProjectRealization::Cow {
                terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
            },
            ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
                project_authority_id: lillux::sha256_hex(
                    b"live-project\0local:/work/project\0/work/project",
                ),
                source_identity: "dotenv:/work/project/.env".to_string(),
                include_operator_vault: true,
                name_authority: ryeos_state::objects::EnvironmentNameAuthority::DeclaredRequired,
            },
            Vec::new(),
        )
        .unwrap();
        crate::launch_metadata::ResumeContext {
            kind: "directive".to_string(),
            item_ref: "directive:test".to_string(),
            ref_bindings: std::collections::BTreeMap::new(),
            launch_mode: "wait".to_string(),
            parameters: json!({}),
            project_context,
            project_authority,
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity,
            local_overlay_root,
            original_snapshot_hash,
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            requested_by: EffectivePrincipal::Local(Principal {
                fingerprint: "fp:test".to_string(),
                scopes: vec!["execute".to_string()],
            }),
            execution_hints: ExecutionHints::default(),
            effective_caps: Vec::new(),
            parent_delegation_caps: None,
            executor_ref: Some("native:test".to_string()),
            runtime_ref: None,
        }
    }

    #[test]
    fn continuation_snapshot_binds_project_root_and_pin() {
        let source = "T-source";
        let mut record = thread_record("T-successor", "T-root");
        record.upstream_thread_id = Some(source.to_string());
        record.base_project_snapshot_hash = Some("a".repeat(64));
        let resume = continuation_resume_context(ProjectContext::LocalPath {
            path: PathBuf::from("/work/project"),
        });
        record.project_authority = resume.project_authority.clone();
        record.project_root = Some(PathBuf::from("/wrong/caller/path"));
        assert!(build_continuation_snapshot(&record, &resume).is_err());
        record.project_root = Some(PathBuf::from("/work/project"));
        let snapshot = build_continuation_snapshot(&record, &resume).unwrap();
        assert_eq!(snapshot.project_root, Some(PathBuf::from("/work/project")));
        assert_eq!(snapshot.base_project_snapshot_hash, Some("a".repeat(64)));
    }

    #[test]
    fn continuation_handoff_persists_created_successor_with_inherited_pin() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-root", "T-root"))
            .expect("root thread");

        let mut successor = thread_record("T-successor", "T-root");
        successor.project_root = Some(PathBuf::from("/work/project"));
        successor.base_project_snapshot_hash = Some("a".repeat(64));
        let resume = continuation_resume_context(ProjectContext::LocalPath {
            path: PathBuf::from("/work/project"),
        });
        successor.project_authority = resume.project_authority.clone();
        let outcome = store
            .create_or_get_continuation_for_test(
                &successor,
                "T-root",
                "T-root",
                Some("follow"),
                "request-fingerprint",
                Some(&resume),
            )
            .expect("create continuation successor");
        assert!(matches!(outcome, ContinuationOutcome::Created(_)));

        let projected = store
            .get_thread("T-successor")
            .expect("read successor")
            .expect("successor row");
        assert_eq!(projected.status, ThreadStatus::Created.as_str());
        assert_eq!(projected.upstream_thread_id.as_deref(), Some("T-root"));

        let inner = store.lock().expect("lock state store");
        let persisted = authoritative_snapshot_for_transition(&inner, "T-root", "T-successor")
            .expect("read authoritative successor snapshot");
        assert_eq!(persisted.base_project_snapshot_hash, Some("a".repeat(64)));
    }

    #[test]
    fn exact_child_link_replay_after_parent_continues_does_not_cancel_child() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-parent", "T-parent"))
            .expect("parent thread");
        store
            .create_thread_for_test(&thread_record("T-child", "T-child"))
            .expect("child thread");

        assert_eq!(
            store
                .record_child_link("T-parent", "T-child", "dispatch")
                .expect("initial child link"),
            None
        );
        store
            .finalize_thread(
                "T-parent",
                &FinalizeThreadRecord {
                    status: ThreadStatus::Continued.as_str().to_string(),
                    outcome_code: Some(ThreadStatus::Continued.as_str().to_string()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                },
            )
            .expect("continue parent");

        assert_eq!(
            store
                .record_child_link("T-parent", "T-child", "dispatch")
                .expect("exact link replay"),
            None
        );
        let child = store
            .get_thread("T-child")
            .expect("read child")
            .expect("child row");
        assert_eq!(child.runtime.stop_intent, None);
    }

    #[test]
    fn fresh_dispatch_link_after_parent_terminal_atomically_cancels_child() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-parent", "T-parent"))
            .expect("parent thread");
        store
            .create_thread_for_test(&thread_record("T-child", "T-child"))
            .expect("child thread");
        store
            .finalize_thread(
                "T-parent",
                &FinalizeThreadRecord {
                    status: ThreadStatus::Completed.as_str().to_string(),
                    outcome_code: Some(ThreadStatus::Completed.as_str().to_string()),
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                },
            )
            .expect("complete parent");

        assert_eq!(
            store
                .record_child_link("T-parent", "T-child", "dispatch")
                .expect("late child link"),
            Some(StopIntent::Cancel)
        );
        let child = store
            .get_thread("T-child")
            .expect("read child")
            .expect("child row");
        assert_eq!(child.runtime.stop_intent, Some(StopIntent::Cancel));
    }

    #[test]
    fn exact_child_link_replay_propagates_durable_parent_kill() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-parent", "T-parent"))
            .expect("parent thread");
        store
            .create_thread_for_test(&thread_record("T-child", "T-child"))
            .expect("child thread");
        store
            .record_child_link("T-parent", "T-child", "dispatch")
            .expect("initial child link");
        store
            .request_thread_stop("T-parent", StopIntent::Kill)
            .expect("kill parent");

        assert_eq!(
            store
                .record_child_link("T-parent", "T-child", "dispatch")
                .expect("exact link replay"),
            Some(StopIntent::Kill)
        );
        let child = store
            .get_thread("T-child")
            .expect("read child")
            .expect("child row");
        assert_eq!(child.runtime.stop_intent, Some(StopIntent::Kill));
    }

    #[test]
    fn trace_branch_does_not_project_ordinary_upstream_edge() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-root", "T-root"))
            .expect("root thread");

        let branch = thread_record("T-branch", "T-root");
        let persisted = store
            .create_trace_branch(
                &branch,
                json!({
                    "relation": "trace_branch",
                    "child_thread_id": "T-branch",
                    "parent_event_ref": {"chain_root_id": "T-root"},
                    "state_anchor_ref": {"chain_root_id": "T-root"}
                }),
            )
            .expect("trace branch");

        let child = store
            .get_thread("T-branch")
            .expect("get branch")
            .expect("branch thread");
        assert_eq!(child.upstream_thread_id, None);
        assert!(store.list_chain_edges("T-root").expect("edges").is_empty());
        assert_eq!(persisted.len(), 2);
        assert_eq!(
            persisted[1].event_type,
            ryeos_state::event_types::EDGE_RECORDED
        );
        assert_eq!(persisted[1].payload["relation"], json!("trace_branch"));
    }

    #[test]
    fn trace_branch_duplicate_explicit_child_id_does_not_append_events() {
        let store = test_store();
        store
            .create_thread_for_test(&thread_record("T-root", "T-root"))
            .expect("root thread");

        let branch = thread_record("T-branch", "T-root");
        store
            .create_trace_branch(
                &branch,
                json!({
                    "relation": "trace_branch",
                    "child_thread_id": "T-branch",
                    "parent_event_ref": {"chain_root_id": "T-root"},
                    "state_anchor_ref": {"chain_root_id": "T-root"}
                }),
            )
            .expect("trace branch");

        let head_after_first = {
            let g = store.lock().expect("lock");
            g.state_db
                .read_generic_head_ref("chains", "T-root")
                .expect("read chain head")
                .expect("chain head")
                .target_hash
        };

        let err = store
            .create_trace_branch(
                &branch,
                json!({
                    "relation": "trace_branch",
                    "child_thread_id": "T-branch",
                    "parent_event_ref": {"chain_root_id": "T-root"},
                    "state_anchor_ref": {"chain_root_id": "T-root"}
                }),
            )
            .expect_err("duplicate child id should fail");

        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err:#}"
        );

        let head_after_duplicate = {
            let g = store.lock().expect("lock");
            g.state_db
                .read_generic_head_ref("chains", "T-root")
                .expect("read chain head")
                .expect("chain head")
                .target_hash
        };
        assert_eq!(head_after_duplicate, head_after_first);

        let events = store
            .replay_events("T-root", Some("T-branch"), None, 10, 1024 * 1024)
            .expect("branch replay");
        assert_eq!(events.events.len(), 2);
        assert_eq!(
            events.events[0].event_type,
            ryeos_state::event_types::THREAD_CREATED
        );
        assert_eq!(
            events.events[1].event_type,
            ryeos_state::event_types::EDGE_RECORDED
        );
    }
}
