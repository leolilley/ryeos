//! Plain record, request and outcome types of the state store.
use super::*;

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersistedEventRef {
    pub chain_root_id: String,
    pub chain_seq: i64,
    pub event_hash: String,
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PersistedEventNavigation {
    pub previous: Option<PersistedEventRef>,
    pub current: Option<PersistedEventRef>,
    pub next: Option<PersistedEventRef>,
    pub live_head: Option<PersistedEventRef>,
}

pub struct NodeIdentitySigner {
    pub(super) identity: ryeos_node_runtime::identity::NodeIdentity,
}

impl NodeIdentitySigner {
    pub fn from_identity(identity: &ryeos_node_runtime::identity::NodeIdentity) -> Self {
        Self {
            identity: identity.clone(),
        }
    }
}

impl Signer for NodeIdentitySigner {
    fn sign(&self, data: &[u8]) -> Vec<u8> {
        use lillux::crypto::Signer as Ed25519Signer;
        self.identity.signing_key().sign(data).to_bytes().to_vec()
    }

    fn fingerprint(&self) -> &str {
        self.identity.fingerprint()
    }

    fn verifying_key(&self) -> lillux::crypto::VerifyingKey {
        *self.identity.verifying_key()
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateManifestInput {
    pub name: String,
    pub media_type: String,
    pub content_base64: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateAnchorDraft {
    pub label: String,
    pub runtime: Value,
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateAnchorPublishParams {
    pub thread_id: String,
    pub subject: ryeos_state::objects::StateAnchorSubject,
    pub contract: String,
    pub restore: Value,
    pub restore_digest: String,
    #[serde(default)]
    pub objects: Vec<StateManifestInput>,
    pub anchor: StateAnchorDraft,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateAnchorPublication {
    pub state_digest: String,
    pub manifest_ref: String,
    pub event: PersistedEventRecord,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectObservationPublication {
    pub event: PersistedEventRecord,
    pub inserted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCallObservationPublication {
    pub event: PersistedEventRecord,
    pub inserted: bool,
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
    pub result_workspace_output_capture_hash: Option<String>,
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

/// What a fenced-kind chain's retirement keeps, decided by the composition
/// that owns the kind's meaning after it has qualified the chain.
#[derive(Debug, Clone, Default)]
pub struct FencedChainRetirementPlan {
    pub counterpart_roots: BTreeSet<String>,
    pub evidence: Vec<ryeos_state::state_db::RetirementEvidenceRef>,
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

#[derive(Debug, Clone, Serialize)]
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
/// Projection-located structural edge offered for signed execution-tree proof.
/// No field in this candidate grants visibility or establishes lineage.
#[derive(Debug, Clone)]
pub struct ExecutionTreeEdgeCandidate {
    pub parent_thread_id: String,
    pub parent_chain_root_id: String,
    pub child_thread_id: String,
    pub child_chain_root_id: String,
    pub relation: String,
}

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

#[derive(Debug, Clone)]
pub struct AdmittedProgramEvidence {
    pub admitted_launch_capsule_hash: String,
    pub subject: crate::thread_lifecycle::AdmittedProgramSubject,
    pub effective_definition_digest: ryeos_engine::resolution::EffectiveDefinitionDigest,
    pub resolution: ryeos_engine::resolution::ResolutionOutput,
}

/// Exact retained operands needed for a mechanical execution comparison.
///
/// The state boundary verifies the capsule, its sealed program, and the
/// retained realization before exposing this value. UI code never receives a
/// CAS handle or a way to reinterpret operational projection rows as
/// execution authority.
#[derive(Debug, Clone)]
pub struct AdmittedExecutionComparisonEvidence {
    pub program: AdmittedProgramEvidence,
    pub execution_realization_hash: String,
    pub execution_realization: ryeos_state::objects::AdmittedExecutionRealization,
}

/// Minimum exact-thread authority read from the signed CAS snapshot.
///
/// UI authorization must not derive membership from the rebuildable thread
/// projection. These fields authorize the subject before any capsule, event,
/// result, artifact, or descendant is loaded.
#[derive(Debug, Clone)]
pub struct AuthoritativeThreadSubject {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub item_ref: String,
    pub executor_ref: String,
    pub launch_mode: String,
    pub current_site_id: String,
    pub origin_site_id: String,
    pub upstream_thread_id: Option<String>,
    pub project_root: Option<PathBuf>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub status: ryeos_state::objects::ThreadStatus,
    pub requested_by: Option<String>,
    pub project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    pub admitted_launch_capsule_hash: Option<String>,
    pub result_project_snapshot_hash: Option<String>,
    pub result_workspace_output_capture_hash: Option<String>,
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

/// The exact seat a conditional lease or publication action targets. The seat
/// is a chain root; its signed snapshot must be a running thread of a fenced
/// chain kind carrying this owner, item and executor.
#[derive(Debug, Clone, Copy)]
pub struct ExactSeat<'a> {
    pub root_id: &'a str,
    pub owner: &'a str,
    pub item_ref: &'a str,
    pub executor_ref: &'a str,
}

/// The completed historical cut a seat action depends on. `expected` is the
/// seat root's own anchor and must also appear in `anchors`; `retired` names
/// chains that must still be fenced with the recorded closure.
#[derive(Debug, Clone, Copy)]
pub struct SeatCut<'a> {
    pub expected: &'a ryeos_state::AuthoritativeThreadEventReadAnchor,
    pub anchors: &'a [ryeos_state::AuthoritativeThreadEventReadAnchor],
    pub retired: &'a [ryeos_state::RetiredChainDependency],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SeatAnchorState {
    Current,
    Moved,
    NotThisSeat,
}

/// Outcome of an append conditioned on an exact cut. `Conflict` means a
/// supplied cut or admission moved and nothing was written; `NotRunning`
/// means the target is not an admitted running target (or its seat lease is
/// already claimed for reaping) and nothing was written.
#[derive(Debug)]
pub enum CutAppendResult {
    Committed(Vec<PersistedEventRecord>),
    Conflict,
    NotRunning,
}

/// Signed data visible to the admission callback of a mixed-cut append, taken
/// while every chain lock of the cut is held. `counterpart` is present only
/// when the request named a live counterpart chain.
#[derive(Debug, Clone, Copy)]
pub struct MixedCutAdmission<'a> {
    pub target: &'a ryeos_state::objects::ThreadSnapshot,
    pub target_thread_ids: &'a [String],
    pub counterpart: Option<&'a ryeos_state::objects::ThreadSnapshot>,
}

/// One append to a running chain root that depends on its own frontier, an
/// optional second live chain, any further live chains, and any retired chains
/// staying exactly as the caller read them.
#[derive(Debug, Clone, Copy)]
pub struct MixedCutAppend<'a> {
    pub chain_root_id: &'a str,
    pub thread_id: &'a str,
    pub events: &'a [NewEventRecord],
    pub expected: &'a ryeos_state::AuthoritativeThreadEventReadAnchor,
    pub counterpart: Option<&'a ryeos_state::AuthoritativeThreadEventReadAnchor>,
    /// Kind the target thread's signed snapshot must carry.
    pub required_target_kind: &'a str,
    pub current_anchors: &'a [ryeos_state::AuthoritativeThreadEventReadAnchor],
    pub retired: &'a [ryeos_state::RetiredChainDependency],
}

/// Two exact seats published in order under one cut: the first seat takes
/// `first_events`, then the second takes the batch built from the first
/// commit. Both seats are chain roots whose signed snapshots must carry their
/// seat identity; the cut's `expected` anchor is the first seat's, and the
/// second seat's anchor must also appear in `cut.anchors`. When
/// `first_rests_on_second` is set, the first seat's batch is published only
/// after a durable obligation records that its evidence rests on the second
/// seat's retained prefix, so the second seat's closure cannot be released
/// before the first seat is itself fenced.
#[derive(Debug, Clone, Copy)]
pub struct SeatPairAppend<'a> {
    pub first: &'a ExactSeat<'a>,
    pub second: &'a ExactSeat<'a>,
    pub second_anchor: &'a ryeos_state::AuthoritativeThreadEventReadAnchor,
    pub first_events: &'a [NewEventRecord],
    pub first_rests_on_second: bool,
    pub cut: &'a SeatCut<'a>,
}

/// Signed data visible to the admission callback of a seat pair append, taken
/// while every chain lock of the cut is held.
#[derive(Debug, Clone, Copy)]
pub struct SeatPairAdmission<'a> {
    pub first: &'a ryeos_state::objects::ThreadSnapshot,
    pub second: &'a ryeos_state::objects::ThreadSnapshot,
}

/// Outcome of an ordered two-seat append. Two signed heads cannot move
/// atomically: after the first publication every outcome other than
/// `Committed` is partial or uncertain and must be reconciled from signed
/// history before either seat is retried.
#[derive(Debug)]
pub enum CutPairAppendResult {
    Committed {
        first: Vec<PersistedEventRecord>,
        second: Vec<PersistedEventRecord>,
    },
    Conflict,
    NotRunning,
    FirstUncertain(anyhow::Error),
    FirstCommittedSecondUncertain {
        first: Vec<PersistedEventRecord>,
        error: anyhow::Error,
    },
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
    /// Retained project generation coordinate, or null when none was retained.
    /// Exact `get_thread` resolves it from immutable testimony; list DTOs keep
    /// their existing projection-only display contract. Neither follows a
    /// continuation nor grants import/publication authority.
    pub result_project_snapshot_hash: Option<String>,
    pub result_workspace_output_capture_hash: Option<String>,
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
    pub(crate) remote_writer_grant_hash: Option<String>,
}

/// Result of an idempotent operator continuation create-or-get.
#[derive(Debug)]
pub enum ContinuationOutcome {
    /// A new successor was created with this request's fingerprint persisted on
    /// its edge. The caller should launch it. Carries best-effort live events
    /// and detail derived directly from the exact committed snapshot.
    Created {
        persisted: Vec<PersistedEventRecord>,
        successor: Box<ThreadDetail>,
    },
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
