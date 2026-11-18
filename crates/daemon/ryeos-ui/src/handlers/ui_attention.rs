//! The operator attention queue: one list of items with one shape, drawn from
//! a closed set of store queries that a signed config selects.
//!
//! The config (`config:ryeos/ui/attention`) names each source, the query that
//! feeds it, and the presentation words (title, tone) its items carry. It also
//! maps thread statuses to presentation categories and says which statuses
//! count as a failed run. Adding a source of an existing query is data; a new
//! query needs a store fact and so is code here.
//!
//! An operator may acknowledge an item whose source allows it. The
//! acknowledgement is a durable `attention_acknowledged` event on the item's
//! chain, recording the item's activity time. It hides the item until the item
//! has later activity, at which point the item is raised again; it never decides the underlying
//! work.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use ryeos_app::handler_error::HandlerError;
use ryeos_app::runtime_db::{
    DedicatedSessionApprovalProjection, DedicatedSessionCandidateAttention,
};
use ryeos_app::state::AppState;
use ryeos_app::state_store::{FailedRunAttention, NewEventRecord};
use ryeos_engine::config_loading::{ConfigLoadContext, ConfigSpec, ResolveMode};

pub const CONFIG_REF: &str = "config:ryeos/ui/attention";
const CONFIG_PATH: &str = "ryeos/ui/attention.yaml";
const ITEM_SCHEMA: &str = "ryeos.ui.attention_item.v2";
const ACK_SCHEMA: &str = "ryeos.ui.attention_acknowledged.v1";
/// Acknowledgement events read per page of one chain.
const ACK_PAGE_EVENTS: usize = 500;
const ACK_PAGE_BYTES: usize = 512 * 1024;
/// Pages read per chain; acknowledgements beyond this are not seen.
const ACK_MAX_PAGES: usize = 8;
/// Threads scanned to scope approvals and candidates to one project.
const MAX_PROJECT_THREADS: usize = 2_000;
/// Items scanned per source to find the one being acknowledged.
const ACK_SCAN_LIMIT: usize = 500;

/// The store queries a source may name. The set is closed because each one is
/// an implementation over a distinct store fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Query {
    ApprovalsPending,
    CandidatesReady,
    RunsFailed,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// The item kind this source raises; unique within the config.
    pub kind: String,
    pub query: Query,
    pub title: String,
    pub tone: String,
    /// Short noun the bars show next to this kind's count.
    #[serde(default)]
    pub count_label: Option<String>,
    /// Whether an operator may acknowledge this kind's items. An item whose
    /// activity time never advances must not be acknowledgeable, because an
    /// acknowledgement could never be cleared.
    #[serde(default)]
    pub acknowledgeable: bool,
    /// Thread statuses the `runs_failed` query treats as failed.
    #[serde(default)]
    pub statuses: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionConfig {
    pub sources: Vec<Source>,
    /// Thread status to presentation category; an unlisted status is
    /// `unknown`.
    pub phase_categories: BTreeMap<String, String>,
}

impl AttentionConfig {
    /// Parse a verified config body. The envelope fields (`category`,
    /// `version`, `description`) are dropped first; any other unknown field is
    /// refused.
    pub fn from_value(mut value: Value) -> Result<Self, String> {
        if let Some(object) = value.as_object_mut() {
            for envelope in ["category", "version", "description"] {
                object.remove(envelope);
            }
        }
        let config: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        let mut kinds = BTreeSet::new();
        for source in &config.sources {
            if source.kind.is_empty() || source.title.is_empty() || source.tone.is_empty() {
                return Err("attention source needs a kind, title and tone".into());
            }
            if !kinds.insert(source.kind.as_str()) {
                return Err(format!(
                    "attention kind `{}` is declared twice",
                    source.kind
                ));
            }
            match (source.query, source.statuses.is_empty()) {
                (Query::RunsFailed, true) => {
                    return Err(format!("source `{}` names no failed statuses", source.kind));
                }
                (Query::ApprovalsPending | Query::CandidatesReady, false) => {
                    return Err(format!("source `{}` takes no statuses", source.kind));
                }
                _ => {}
            }
        }
        Ok(config)
    }

    pub fn phase_category(&self, status: &str) -> &str {
        self.phase_categories
            .get(status)
            .map_or("unknown", String::as_str)
    }
}

/// Resolve and verify the signed attention config.
pub fn load_config(state: &AppState) -> Result<AttentionConfig> {
    let engine = &state.engine;
    let roots = engine.resolution_roots(None);
    let load_context = ConfigLoadContext {
        roots: &roots,
        parsers: &engine.parser_dispatcher,
        kinds: &engine.kinds,
        trust_store: &engine.trust_store,
        project_authority: None,
    };
    let resolved = ryeos_engine::config_loading::resolve_config_spec(
        &ConfigSpec {
            path: CONFIG_PATH.to_string(),
            mode: ResolveMode::FirstMatch,
        },
        &load_context,
    )
    .map_err(|error| HandlerError::Internal(format!("resolve {CONFIG_REF}: {error}")))?;
    let Some(layer) = resolved.layers.first() else {
        return Err(HandlerError::Internal(format!("{CONFIG_REF} is not installed")).into());
    };
    let value = ryeos_engine::config_loading::load_and_verify_trusted_config_file(
        &layer.path,
        &load_context,
    )
    .map_err(|error| HandlerError::Internal(format!("{CONFIG_REF} is untrusted: {error}")))?;
    Ok(AttentionConfig::from_value(value).map_err(HandlerError::Internal)?)
}

/// Every attention item for `owner` that is not acknowledged, oldest first, at
/// most `limit`. `project_root` narrows the queue to one project.
pub fn collect(
    state: &AppState,
    config: &AttentionConfig,
    owner: &str,
    project_root: Option<&Path>,
    limit: usize,
) -> Result<Vec<Value>> {
    let mut items = collect_unacknowledged(state, config, owner, project_root, limit)?;
    items.truncate(limit);
    Ok(items)
}

/// The same queue as [`collect`] without the final truncation, so a caller
/// can count the whole queue and still return only a page of it. Each source
/// query is bounded by `source_limit`.
pub fn collect_unacknowledged(
    state: &AppState,
    config: &AttentionConfig,
    owner: &str,
    project_root: Option<&Path>,
    source_limit: usize,
) -> Result<Vec<Value>> {
    let mut items = collect_raised(state, config, owner, project_root, source_limit)?;
    let acknowledgements = acknowledgements(state, owner, &items)?;
    items.retain(|item| !is_acknowledged(item, &acknowledgements));
    Ok(items)
}

/// Counts of a queue by kind, in the config's source order, for the bars.
/// The counts come from the items the daemon returns; a client adds nothing.
pub fn counts(config: &AttentionConfig, items: &[Value]) -> Value {
    let by_kind = config
        .sources
        .iter()
        .filter_map(|source| {
            let count = items
                .iter()
                .filter(|item| item["kind"] == source.kind.as_str())
                .count();
            (count > 0).then(|| {
                json!({
                    "kind": source.kind,
                    "label": source.count_label.as_deref().unwrap_or(&source.kind),
                    "tone": source.tone,
                    "count": count,
                })
            })
        })
        .collect::<Vec<_>>();
    json!({"total": items.len(), "by_kind": by_kind})
}

/// Every item the sources raise for `owner`, acknowledged or not, oldest first.
fn collect_raised(
    state: &AppState,
    config: &AttentionConfig,
    owner: &str,
    project_root: Option<&Path>,
    limit: usize,
) -> Result<Vec<Value>> {
    let placements = match project_root {
        None => None,
        Some(root) => Some(
            state
                .threads
                .list_thread_views_query(
                    MAX_PROJECT_THREADS,
                    &ryeos_app::thread_lifecycle::ThreadListFilter {
                        principal: Some(owner.to_owned()),
                        project_root: Some(root.to_path_buf()),
                        ..Default::default()
                    },
                    ryeos_app::thread_lifecycle::ThreadSort::Newest,
                )?
                .into_iter()
                .map(|thread| thread.item.thread_id)
                .collect::<BTreeSet<_>>(),
        ),
    };
    let mut items = Vec::new();
    for source in &config.sources {
        match source.query {
            Query::ApprovalsPending => {
                for entry in state
                    .state_store
                    .pending_dedicated_session_approval_attention(
                        owner,
                        placements.as_ref(),
                        limit,
                    )?
                {
                    items.push(approval_item(source, &entry));
                }
            }
            Query::CandidatesReady => {
                for entry in state.state_store.dedicated_session_candidate_attention(
                    owner,
                    placements.as_ref(),
                    limit,
                )? {
                    items.push(candidate_item(source, &entry));
                }
            }
            Query::RunsFailed => {
                for entry in state.state_store.failed_run_attention(
                    owner,
                    project_root,
                    &source.statuses,
                    limit,
                )? {
                    items.push(failed_run_item(source, &entry));
                }
            }
        }
    }
    items.sort_by(|left, right| {
        left["since_ms"]
            .as_i64()
            .cmp(&right["since_ms"].as_i64())
            .then_with(|| left["id"].as_str().cmp(&right["id"].as_str()))
    });
    Ok(items)
}

/// The latest `item_since_ms` each acknowledgeable item of `items` was
/// acknowledged at by `owner`. Read from the item's chain.
fn acknowledgements(
    state: &AppState,
    owner: &str,
    items: &[Value],
) -> Result<BTreeMap<String, i64>> {
    let chains = items
        .iter()
        .filter(|item| item["actions"]["acknowledge"] == true)
        .filter_map(|item| item["subject"]["chain_root_id"].as_str())
        .collect::<BTreeSet<_>>();
    let wanted = [ryeos_state::event_types::ATTENTION_ACKNOWLEDGED.to_owned()];
    let mut acknowledged = BTreeMap::<String, i64>::new();
    for chain_root_id in chains {
        let mut after = None;
        for _ in 0..ACK_MAX_PAGES {
            let page = state.state_store.replay_events_of_types(
                chain_root_id,
                None,
                after,
                ACK_PAGE_EVENTS,
                ACK_PAGE_BYTES,
                Some(&wanted),
            )?;
            for event in &page.events {
                after = Some(event.chain_seq);
                if event.payload["principal"] != owner {
                    continue;
                }
                let (Some(item_id), Some(since_ms)) = (
                    event.payload["item_id"].as_str(),
                    event.payload["item_since_ms"].as_i64(),
                ) else {
                    continue;
                };
                let latest = acknowledged.entry(item_id.to_owned()).or_insert(since_ms);
                *latest = (*latest).max(since_ms);
            }
            if !page.has_more {
                break;
            }
        }
    }
    Ok(acknowledged)
}

/// An acknowledged item stays hidden while its current activity time is no
/// later than the activity time it was acknowledged at. Any later activity
/// raises it again.
fn is_acknowledged(item: &Value, acknowledgements: &BTreeMap<String, i64>) -> bool {
    let (Some(id), Some(since_ms)) = (item["id"].as_str(), item["since_ms"].as_i64()) else {
        return false;
    };
    item["actions"]["acknowledge"] == true
        && acknowledgements
            .get(id)
            .is_some_and(|acknowledged_since_ms| since_ms <= *acknowledged_since_ms)
}

/// Acknowledge the item `item_id` for `owner`: record it on the item's chain.
/// An item that is not in the owner's queue, or whose source does not allow
/// acknowledgement, is refused. Acknowledging an item that is already hidden
/// records nothing and reports it.
pub fn acknowledge(
    state: &AppState,
    config: &AttentionConfig,
    owner: &str,
    item_id: &str,
) -> Result<Value, HandlerError> {
    let internal = |error: anyhow::Error| HandlerError::Internal(format!("{error:#}"));
    let items = collect_raised(state, config, owner, None, ACK_SCAN_LIMIT).map_err(internal)?;
    let item = items
        .iter()
        .find(|item| item["id"] == item_id)
        .ok_or(HandlerError::NotFound)?;
    if item["actions"]["acknowledge"] != true {
        return Err(HandlerError::BadRequest(format!(
            "attention item `{item_id}` cannot be acknowledged"
        )));
    }
    let existing = acknowledgements(state, owner, std::slice::from_ref(item)).map_err(internal)?;
    if is_acknowledged(item, &existing) {
        return Ok(json!({"schema_version": ACK_SCHEMA, "item_id": item_id, "already": true}));
    }
    let chain_root_id = item["subject"]["chain_root_id"]
        .as_str()
        .ok_or_else(|| HandlerError::Internal("attention item has no chain".into()))?;
    let thread_id = item["subject"]["thread_id"]
        .as_str()
        .ok_or_else(|| HandlerError::Internal("attention item has no thread".into()))?;
    let persisted = state
        .state_store
        .append_events(
            chain_root_id,
            thread_id,
            &[NewEventRecord {
                event_type: ryeos_state::event_types::ATTENTION_ACKNOWLEDGED.to_owned(),
                storage_class: "indexed".to_owned(),
                payload: json!({
                    "schema_version": ACK_SCHEMA,
                    "item_id": item_id,
                    "kind": item["kind"],
                    "principal": owner,
                    "item_since_ms": item["since_ms"],
                }),
            }],
        )
        .map_err(internal)?;
    Ok(json!({
        "schema_version": ACK_SCHEMA,
        "item_id": item_id,
        "already": false,
        "chain_seq": persisted.first().map(|event| event.chain_seq),
    }))
}

fn actions(entries: &[(&str, bool)]) -> Value {
    Value::Object(
        entries
            .iter()
            .map(|(name, allowed)| ((*name).to_owned(), Value::Bool(*allowed)))
            .collect::<Map<_, _>>(),
    )
}

#[allow(clippy::too_many_arguments)]
fn item(
    source: &Source,
    subject_key: &str,
    chain_root_id: &str,
    thread_id: &str,
    state: &str,
    since_ms: i64,
    expires_at_ms: Option<i64>,
    detail: Value,
    mut actions: Value,
) -> Value {
    actions["acknowledge"] = Value::Bool(source.acknowledgeable);
    json!({
        "schema_version": ITEM_SCHEMA,
        "id": format!("{}:{subject_key}", source.kind),
        "kind": source.kind,
        "subject": {"chain_root_id": chain_root_id, "thread_id": thread_id},
        "title": source.title,
        "tone": source.tone,
        "state": state,
        "since_ms": since_ms,
        "expires_at_ms": expires_at_ms,
        "detail": detail,
        "actions": actions,
    })
}

fn approval_item(source: &Source, entry: &DedicatedSessionApprovalProjection) -> Value {
    let approval = &entry.approval;
    let authority = ryeos_app::dedicated_session_service::public_approval_authority(
        &approval.requested_authority,
    );
    let pending = approval.state == "pending";
    let accept = pending && authority["accept_allowed"].as_bool().unwrap_or(false);
    item(
        source,
        &approval.approval_id,
        &entry.chain_root_id,
        &approval.placement_thread_id,
        &approval.state,
        approval.created_at_ms,
        Some(approval.expires_at_ms),
        json!({
            "summary": approval.operation_class,
            "approval_id": approval.approval_id,
            "worker_boot_epoch": approval.worker_boot_epoch,
            "request_digest": approval.request_digest,
            "operation_class": approval.operation_class,
            "requested_authority": authority,
        }),
        actions(&[("open", true), ("accept", accept), ("decline", pending)]),
    )
}

fn candidate_item(source: &Source, entry: &DedicatedSessionCandidateAttention) -> Value {
    item(
        source,
        &entry.placement_thread_id,
        &entry.chain_root_id,
        &entry.placement_thread_id,
        &entry.state,
        entry.updated_at_ms,
        None,
        json!({
            "summary": entry.publication_result,
            "candidate_snapshot_hash": entry.candidate_snapshot_hash,
            "candidate_validation_hash": entry.candidate_validation_hash,
            "candidate_evaluation_hash": entry.candidate_evaluation_hash,
            "publication_result": entry.publication_result,
        }),
        actions(&[("open", true)]),
    )
}

fn failed_run_item(source: &Source, entry: &FailedRunAttention) -> Value {
    let since_ms = lillux::time::unix_secs_from_iso8601(&entry.updated_at).map_or(0, |secs| {
        i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)
    });
    item(
        source,
        &entry.thread_id,
        &entry.chain_root_id,
        &entry.thread_id,
        &entry.status,
        since_ms,
        None,
        json!({
            "summary": entry.item_ref,
            "item_ref": entry.item_ref,
            "status": entry.status,
            "project_root": entry.project_root,
        }),
        actions(&[("open", true)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::runtime_db::DedicatedSessionApprovalRecord;
    use ryeos_app::state_store::NewThreadRecord;
    use ryeos_state::objects::{
        EnvironmentAuthority, ExecutionProjectAuthority, PinnedProjectRealization,
        PinnedTerminalPublication,
    };

    const OWNER: &str = "fp:owner";
    const OTHER: &str = "fp:other";

    fn shipped_config() -> AttentionConfig {
        let path = ryeos_engine::test_support::workspace_root()
            .join("bundles/ryeos-ui/.ai/config/ryeos/ui/attention.yaml");
        let text = std::fs::read_to_string(path).unwrap();
        AttentionConfig::from_value(serde_yaml::from_str(&text).unwrap()).unwrap()
    }

    fn source(config: &AttentionConfig, kind: &str) -> Source {
        config
            .sources
            .iter()
            .find(|source| source.kind == kind)
            .unwrap()
            .clone()
    }

    #[test]
    fn shipped_config_parses_and_names_the_three_sources() {
        let config = shipped_config();
        let queries = config
            .sources
            .iter()
            .map(|source| (source.kind.as_str(), source.query))
            .collect::<Vec<_>>();
        assert_eq!(
            queries,
            vec![
                ("approval", Query::ApprovalsPending),
                ("candidate", Query::CandidatesReady),
                ("failed_run", Query::RunsFailed),
            ]
        );
        assert_eq!(config.phase_category("running"), "running");
        assert_eq!(config.phase_category("continued"), "blocked");
        assert_eq!(config.phase_category("timed_out"), "failed");
        assert_eq!(config.phase_category("something_new"), "unknown");
    }

    #[test]
    fn malformed_config_is_refused() {
        let parse = |text: &str| AttentionConfig::from_value(serde_yaml::from_str(text).unwrap());
        let ok = "sources:\n  - {kind: a, query: runs_failed, title: T, tone: warn, statuses: [failed]}\nphase_categories: {}\n";
        assert!(parse(ok).is_ok());
        for bad in [
            // runs_failed without statuses
            "sources:\n  - {kind: a, query: runs_failed, title: T, tone: warn}\nphase_categories: {}\n",
            // statuses on a query that takes none
            "sources:\n  - {kind: a, query: approvals_pending, title: T, tone: warn, statuses: [x]}\nphase_categories: {}\n",
            // duplicate kind
            "sources:\n  - {kind: a, query: approvals_pending, title: T, tone: warn}\n  - {kind: a, query: candidates_ready, title: T, tone: warn}\nphase_categories: {}\n",
            // unknown query
            "sources:\n  - {kind: a, query: asks_pending, title: T, tone: warn}\nphase_categories: {}\n",
            // unknown field
            "sources:\n  - {kind: a, query: approvals_pending, title: T, tone: warn, extra: 1}\nphase_categories: {}\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    fn approval(state: &str, accept_allowed: bool) -> DedicatedSessionApprovalProjection {
        DedicatedSessionApprovalProjection {
            chain_root_id: "T-chain".into(),
            approval: DedicatedSessionApprovalRecord {
                placement_thread_id: "T-place".into(),
                approval_id: "A-1".into(),
                worker_instance_id: "W-1".into(),
                worker_boot_epoch: 3,
                request_digest: "d".repeat(64),
                operation_class: "command_execution".into(),
                requested_authority: json!({
                    "accept_allowed": accept_allowed,
                    "display": {"title": "run"},
                    "request_id": "secret-upstream-id",
                }),
                state: state.into(),
                decision_principal: None,
                decision: None,
                decision_digest: None,
                reservation_token: None,
                expires_at_ms: 9_000,
                created_at_ms: 1_000,
                resolved_at_ms: None,
                delivery_contacted_at_ms: None,
                delivery_settled_at_ms: None,
            },
        }
    }

    #[test]
    fn approval_item_carries_the_common_shape_and_eligibility() {
        let config = shipped_config();
        let value = approval_item(&source(&config, "approval"), &approval("pending", true));
        assert_eq!(value["schema_version"], ITEM_SCHEMA);
        assert_eq!(value["id"], "approval:A-1");
        assert_eq!(value["kind"], "approval");
        assert_eq!(value["subject"]["chain_root_id"], "T-chain");
        assert_eq!(value["subject"]["thread_id"], "T-place");
        assert_eq!(value["since_ms"], 1_000);
        assert_eq!(value["expires_at_ms"], 9_000);
        assert_eq!(value["detail"]["approval_id"], "A-1");
        assert_eq!(
            value["detail"]["requested_authority"]["accept_allowed"],
            true
        );
        // Upstream delivery coordinates are never published.
        assert!(!value.to_string().contains("secret-upstream-id"));
        assert_eq!(
            value["actions"],
            json!({"open": true, "accept": true, "decline": true, "acknowledge": false})
        );

        let refused = approval_item(&source(&config, "approval"), &approval("pending", false));
        assert_eq!(refused["actions"]["accept"], false);
        assert_eq!(refused["actions"]["decline"], true);
        let settled = approval_item(&source(&config, "approval"), &approval("expired", true));
        assert_eq!(settled["actions"]["accept"], false);
        assert_eq!(settled["actions"]["decline"], false);
    }

    #[test]
    fn candidate_item_offers_open_and_acknowledge() {
        let config = shipped_config();
        let value = candidate_item(
            &source(&config, "candidate"),
            &DedicatedSessionCandidateAttention {
                chain_root_id: "T-chain".into(),
                placement_thread_id: "T-place".into(),
                state: "completed".into(),
                candidate_snapshot_hash: "a".repeat(64),
                candidate_validation_hash: None,
                candidate_evaluation_hash: Some("b".repeat(64)),
                publication_result: "retained_for_review".into(),
                updated_at_ms: 5_000,
            },
        );
        assert_eq!(value["id"], "candidate:T-place");
        assert_eq!(value["since_ms"], 5_000);
        assert_eq!(value["expires_at_ms"], Value::Null);
        assert_eq!(value["detail"]["publication_result"], "retained_for_review");
        assert_eq!(value["actions"], json!({"open": true, "acknowledge": true}));
    }

    fn thread(id: &str, chain_root: &str, owner: &str) -> NewThreadRecord {
        let hash = "a".repeat(64);
        let base = "b".repeat(64);
        NewThreadRecord {
            thread_id: id.to_owned(),
            chain_root_id: chain_root.to_owned(),
            kind: "tool".to_owned(),
            item_ref: format!("tool:test/{id}"),
            executor_ref: "native:test".to_owned(),
            launch_mode: "wait".to_owned(),
            current_site_id: "site:test".to_owned(),
            origin_site_id: "site:test".to_owned(),
            upstream_thread_id: None,
            requested_by: Some(owner.to_owned()),
            project_root: None,
            project_authority: ExecutionProjectAuthority::pinned(
                "d".repeat(64),
                None,
                base.clone(),
                PinnedProjectRealization::Cow {
                    terminal_publication: PinnedTerminalPublication::RetainResult,
                },
                EnvironmentAuthority::None,
                Vec::new(),
            )
            .unwrap(),
            base_project_snapshot_hash: Some(base),
            usage_subject: None,
            usage_subject_asserted_by: None,
            captured_history_policy: Some(ryeos_state::objects::CapturedThreadHistoryPolicy {
                retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
                canonical_item_ref: format!("tool:test/{id}"),
                item_content_hash: hash.clone(),
                item_signer_fingerprint: Some(hash.clone()),
                item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
                kind_schema_content_hash: hash,
                resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
                    node_policy:
                        ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
                },
            }),
        }
    }

    fn finish(state: &AppState, id: &str, status: &str) {
        state
            .state_store
            .finalize_thread(
                id,
                &ryeos_app::state_store::FinalizeThreadRecord {
                    status: status.to_owned(),
                    outcome_code: None,
                    result_json: None,
                    error_json: None,
                    artifacts: Vec::new(),
                    final_cost: None,
                    managed_envelope: None,
                    result_project_snapshot_hash: None,
                    result_workspace_output_capture_hash: None,
                },
            )
            .unwrap();
    }

    fn failed_ids(state: &AppState, owner: &str) -> Vec<String> {
        state
            .state_store
            .failed_run_attention(owner, None, &["failed".into(), "timed_out".into()], 50)
            .unwrap()
            .into_iter()
            .map(|entry| entry.thread_id)
            .collect()
    }

    #[test]
    fn failed_runs_are_scoped_to_what_the_owner_launched() {
        let (_dir, _owner, state) = crate::test_support::build_test_state();
        for (id, owner, status) in [
            ("T-mine-failed", OWNER, "failed"),
            ("T-mine-timeout", OWNER, "timed_out"),
            ("T-mine-done", OWNER, "completed"),
            ("T-mine-cancelled", OWNER, "cancelled"),
            ("T-theirs-failed", OTHER, "failed"),
        ] {
            state
                .state_store
                .create_thread_for_test(&thread(id, id, owner))
                .unwrap();
            finish(&state, id, status);
        }
        let mut ids = failed_ids(&state, OWNER);
        ids.sort();
        assert_eq!(ids, ["T-mine-failed", "T-mine-timeout"]);
        assert_eq!(failed_ids(&state, OTHER), ["T-theirs-failed"]);
        // No configured statuses means nothing is failed.
        assert!(
            state
                .state_store
                .failed_run_attention(OWNER, None, &[], 50)
                .unwrap()
                .is_empty()
        );
        // The limit bounds the result.
        assert_eq!(
            state
                .state_store
                .failed_run_attention(OWNER, None, &["failed".into(), "timed_out".into()], 1)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_failed_child_run_raises_nothing_unless_it_is_followed() {
        let (_dir, _owner, state) = crate::test_support::build_test_state();
        state
            .state_store
            .create_thread_for_test(&thread("T-parent", "T-parent", OWNER))
            .unwrap();
        state
            .state_store
            .create_thread_for_test(&thread("T-child", "T-child", OWNER))
            .unwrap();
        state
            .state_store
            .record_child_link("T-parent", "T-child", "graph_step")
            .unwrap();
        finish(&state, "T-child", "failed");
        assert!(failed_ids(&state, OWNER).is_empty());
    }

    #[test]
    fn a_failed_continuation_head_reports_once_under_its_chain_root() {
        let (_dir, _owner, state) = crate::test_support::build_test_state();
        state
            .state_store
            .create_thread_for_test(&thread("T-root", "T-root", OWNER))
            .unwrap();
        let mut head = thread("T-head", "T-root", OWNER);
        head.upstream_thread_id = Some("T-root".into());
        head.captured_history_policy = None;
        state.state_store.create_thread_for_test(&head).unwrap();
        finish(&state, "T-head", "failed");
        let rows = state
            .state_store
            .failed_run_attention(OWNER, None, &["failed".into()], 10)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].thread_id, "T-head");
        assert_eq!(rows[0].chain_root_id, "T-root");
    }

    #[test]
    fn failed_run_item_has_the_common_shape() {
        let config = shipped_config();
        let value = failed_run_item(
            &source(&config, "failed_run"),
            &FailedRunAttention {
                chain_root_id: "T-chain".into(),
                thread_id: "T-head".into(),
                status: "failed".into(),
                item_ref: "directive:x".into(),
                project_root: Some("/work".into()),
                updated_at: "2026-11-17T01:00:00Z".into(),
            },
        );
        assert_eq!(value["id"], "failed_run:T-head");
        assert_eq!(value["state"], "failed");
        assert_eq!(value["tone"], "danger");
        assert!(value["since_ms"].as_i64().unwrap() > 0);
        assert_eq!(value["actions"], json!({"open": true, "acknowledge": true}));
    }
    fn failed_run_config(acknowledgeable: bool) -> AttentionConfig {
        AttentionConfig::from_value(json!({
            "sources": [{
                "kind": "failed_run", "query": "runs_failed", "title": "Run failed",
                "tone": "danger", "count_label": "failed",
                "acknowledgeable": acknowledgeable, "statuses": ["failed"],
            }],
            "phase_categories": {},
        }))
        .unwrap()
    }

    fn fail_run(state: &AppState, id: &str, owner: &str) {
        state
            .state_store
            .create_thread_for_test(&thread(id, id, owner))
            .unwrap();
        finish(state, id, "failed");
    }

    fn raised_ids(state: &AppState, config: &AttentionConfig, owner: &str) -> Vec<String> {
        collect(state, config, owner, None, 50)
            .unwrap()
            .into_iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn acknowledging_hides_the_item_and_records_it_once() {
        let (_dir, _owner, state) = crate::test_support::build_test_state();
        let config = failed_run_config(true);
        fail_run(&state, "T-a", OWNER);
        fail_run(&state, "T-b", OWNER);
        assert_eq!(
            raised_ids(&state, &config, OWNER),
            ["failed_run:T-a", "failed_run:T-b"]
        );

        let first = acknowledge(&state, &config, OWNER, "failed_run:T-a").unwrap();
        assert_eq!(first["already"], false);
        // Only the acknowledged item is hidden; the other stays raised.
        assert_eq!(raised_ids(&state, &config, OWNER), ["failed_run:T-b"]);

        // A second acknowledgement of a hidden item records nothing.
        let second = acknowledge(&state, &config, OWNER, "failed_run:T-a").unwrap();
        assert_eq!(second["already"], true);
        let recorded = state
            .state_store
            .replay_events_of_types(
                "T-a",
                None,
                None,
                10,
                1 << 20,
                Some(&[ryeos_state::event_types::ATTENTION_ACKNOWLEDGED.to_owned()]),
            )
            .unwrap();
        assert_eq!(recorded.events.len(), 1);
        let payload = &recorded.events[0].payload;
        assert_eq!(payload["item_id"], "failed_run:T-a");
        assert_eq!(payload["principal"], OWNER);
        assert_eq!(payload["kind"], "failed_run");
    }

    #[test]
    fn new_activity_raises_an_acknowledged_item_again() {
        let (_dir, _owner, state) = crate::test_support::build_test_state();
        let config = failed_run_config(true);
        fail_run(&state, "T-a", OWNER);
        acknowledge(&state, &config, OWNER, "failed_run:T-a").unwrap();
        assert!(raised_ids(&state, &config, OWNER).is_empty());

        // The same item, read again after its activity moved even one
        // millisecond past the acknowledged time, is raised again; the recorded acknowledgement is
        // untouched.
        let mut raised = collect_raised(&state, &config, OWNER, None, 50).unwrap();
        let acknowledgements = acknowledgements(&state, OWNER, &raised).unwrap();
        assert!(is_acknowledged(&raised[0], &acknowledgements));
        let since_ms = raised[0]["since_ms"].as_i64().unwrap();
        raised[0]["since_ms"] = json!(since_ms + 1);
        assert!(!is_acknowledged(&raised[0], &acknowledgements));
    }

    #[test]
    fn acknowledgement_compares_exact_milliseconds() {
        let item = |since_ms: i64| json!({"id": "failed_run:T", "since_ms": since_ms, "actions": {"acknowledge": true}});
        let acknowledged = BTreeMap::from([("failed_run:T".to_owned(), 100_500)]);
        assert!(is_acknowledged(&item(99_000), &acknowledged));
        assert!(is_acknowledged(&item(100_500), &acknowledged));
        // Activity one millisecond later, in the same second, raises it again.
        assert!(!is_acknowledged(&item(100_501), &acknowledged));
        // An item the source does not allow to be acknowledged is never hidden.
        let mut blocked = item(1);
        blocked["actions"]["acknowledge"] = json!(false);
        assert!(!is_acknowledged(&blocked, &acknowledged));
        // Another item's acknowledgement does not apply.
        let other = BTreeMap::from([("failed_run:U".to_owned(), 100_500)]);
        assert!(!is_acknowledged(&item(1), &other));
    }

    #[test]
    fn acknowledging_is_scoped_to_the_principals_own_queue() {
        let (_dir, _owner, state) = crate::test_support::build_test_state();
        let config = failed_run_config(true);
        fail_run(&state, "T-mine", OWNER);
        fail_run(&state, "T-theirs", OTHER);
        // Another principal's item and an unknown item are both absent.
        for id in ["failed_run:T-theirs", "failed_run:T-nobody"] {
            assert!(matches!(
                acknowledge(&state, &config, OWNER, id),
                Err(HandlerError::NotFound)
            ));
        }
        acknowledge(&state, &config, OWNER, "failed_run:T-mine").unwrap();
        // Acknowledging as the owner hides nothing in the other principal's queue.
        assert_eq!(raised_ids(&state, &config, OTHER), ["failed_run:T-theirs"]);
    }

    #[test]
    fn a_source_that_forbids_acknowledgement_refuses_it() {
        let (_dir, _owner, state) = crate::test_support::build_test_state();
        let config = failed_run_config(false);
        fail_run(&state, "T-a", OWNER);
        assert!(matches!(
            acknowledge(&state, &config, OWNER, "failed_run:T-a"),
            Err(HandlerError::BadRequest(_))
        ));
        assert_eq!(raised_ids(&state, &config, OWNER), ["failed_run:T-a"]);
    }

    #[test]
    fn counts_follow_the_queue_in_source_order() {
        let config = shipped_config();
        let item = |kind: &str| json!({"kind": kind});
        let counts = counts(
            &config,
            &[item("failed_run"), item("approval"), item("failed_run")],
        );
        assert_eq!(counts["total"], 3);
        assert_eq!(
            counts["by_kind"],
            json!([
                {"kind": "approval", "label": "approvals", "tone": "warn", "count": 1},
                {"kind": "failed_run", "label": "failed", "tone": "danger", "count": 2},
            ])
        );
        assert_eq!(super::counts(&config, &[])["by_kind"], json!([]));
    }
}
