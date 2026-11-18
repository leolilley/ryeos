//! The operator attention queue: one list of items with one shape, drawn from
//! a closed set of store queries that a signed config selects.
//!
//! The config (`config:ryeos/ui/attention`) names each source, the query that
//! feeds it, and the presentation words (title, tone) its items carry. It also
//! maps thread statuses to presentation categories and says which statuses
//! count as a failed run. Adding a source of an existing query is data; a new
//! query needs a store fact and so is code here.

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
use ryeos_app::state_store::FailedRunAttention;
use ryeos_engine::config_loading::{ConfigLoadContext, ConfigSpec, ResolveMode};

pub const CONFIG_REF: &str = "config:ryeos/ui/attention";
const CONFIG_PATH: &str = "ryeos/ui/attention.yaml";
const ITEM_SCHEMA: &str = "ryeos.ui.attention_item.v2";
/// Threads scanned to scope approvals and candidates to one project.
const MAX_PROJECT_THREADS: usize = 2_000;

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
    let value = load_verified_config(state, CONFIG_PATH, CONFIG_REF)?;
    Ok(AttentionConfig::from_value(value).map_err(HandlerError::Internal)?)
}

/// The verified body of the signed config item at `path` (named `config_ref`
/// in errors), from the first installed bundle that carries it.
pub(crate) fn load_verified_config(
    state: &AppState,
    path: &str,
    config_ref: &str,
) -> Result<Value> {
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
            path: path.to_string(),
            mode: ResolveMode::FirstMatch,
        },
        &load_context,
    )
    .map_err(|error| HandlerError::Internal(format!("resolve {config_ref}: {error}")))?;
    let Some(layer) = resolved.layers.first() else {
        return Err(HandlerError::Internal(format!("{config_ref} is not installed")).into());
    };
    let value = ryeos_engine::config_loading::load_and_verify_trusted_config_file(
        &layer.path,
        &load_context,
    )
    .map_err(|error| HandlerError::Internal(format!("{config_ref} is untrusted: {error}")))?;
    Ok(value)
}

/// Every attention item for `owner`, oldest first, at most `limit`.
/// `project_root` narrows the queue to one project.
pub fn collect(
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
    items.truncate(limit);
    Ok(items)
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
    actions: Value,
) -> Value {
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
            json!({"open": true, "accept": true, "decline": true})
        );

        let refused = approval_item(&source(&config, "approval"), &approval("pending", false));
        assert_eq!(refused["actions"]["accept"], false);
        assert_eq!(refused["actions"]["decline"], true);
        let settled = approval_item(&source(&config, "approval"), &approval("expired", true));
        assert_eq!(settled["actions"]["accept"], false);
        assert_eq!(settled["actions"]["decline"], false);
    }

    #[test]
    fn candidate_item_offers_only_open() {
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
        assert_eq!(value["actions"], json!({"open": true}));
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
        assert_eq!(value["actions"], json!({"open": true}));
    }
}
