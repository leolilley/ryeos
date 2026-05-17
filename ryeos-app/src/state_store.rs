use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use ryeos_state::objects::ThreadUsage;
use ryeos_state::StateDb;
use ryeos_state::chain::SnapshotUpdate;
use ryeos_state::signer::Signer;
use ryeos_state::objects::thread_snapshot::ThreadStatus;
use ryeos_state::objects::ThreadSnapshot;
use ryeos_state::queries;

use crate::runtime_db;
use crate::write_barrier::{WriteBarrier, WritePermit};
pub use runtime_db::{
    RuntimeInfo, CommandRecord, NewCommandRecord,
};

#[derive(Debug, Clone, Serialize)]
pub struct PersistedEventRecord {
    pub event_id: i64,
    pub chain_root_id: String,
    pub chain_seq: i64,
    pub thread_id: String,
    pub thread_seq: i64,
    pub event_type: String,
    pub storage_class: String,
    pub ts: String,
    pub payload: Value,
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
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, Serialize)]
pub struct FinalizeThreadRecord {
    pub status: String,
    pub outcome_code: Option<String>,
    pub result_json: Option<Value>,
    pub error_json: Option<Value>,
    pub artifacts: Vec<NewArtifactRecord>,
    pub final_cost: Option<ryeos_engine::contracts::FinalCost>,
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
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThreadResultRecord {
    pub outcome_code: Option<String>,
    pub result: Option<Value>,
    pub error: Option<Value>,
    pub metadata: Option<Value>,
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
    pub requested_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub runtime: RuntimeInfo,
}

struct Inner {
    state_db: StateDb,
    runtime_db: runtime_db::RuntimeDb,
    signer: Arc<dyn Signer>,
    write_barrier: WriteBarrier,
}

pub struct StateStore {
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for StateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateStore")
            .field("inner", &"<Mutex<Inner>>")
            .finish()
    }
}

fn build_snapshot(thread: &NewThreadRecord) -> ThreadSnapshot {
    let now = lillux::time::iso8601_now();
    ThreadSnapshot {
        schema: ryeos_state::objects::SCHEMA_VERSION,
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
        base_project_snapshot_hash: None,
        result_project_snapshot_hash: None,
        created_at: now.clone(),
        updated_at: now,
        started_at: None,
        finished_at: None,
        result: None,
        error: None,
        budget: None,
        artifacts: vec![],
        facets: Default::default(),
        last_event_hash: None,
        last_chain_seq: 0,
        last_thread_seq: 0,
    }
}

fn convert_events(events: &[NewEventRecord], chain_root_id: &str, thread_id: &str) -> Vec<ryeos_state::objects::ThreadEvent> {
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

fn persisted_from_append(result: &ryeos_state::chain::AppendResult, events: &[NewEventRecord]) -> Vec<PersistedEventRecord> {
    result
        .events
        .iter()
        .zip(events.iter())
        .map(|(stored, input)| PersistedEventRecord {
            event_id: stored.chain_seq as i64,
            chain_root_id: stored.chain_root_id.clone(),
            chain_seq: stored.chain_seq as i64,
            thread_id: stored.thread_id.clone(),
            thread_seq: stored.thread_seq as i64,
            event_type: input.event_type.clone(),
            storage_class: input.storage_class.clone(),
            ts: stored.ts.clone(),
            payload: input.payload.clone(),
        })
        .collect()
}

impl StateStore {
    pub fn new(
        state_root: PathBuf,
        runtime_db_path: PathBuf,
        signer: Arc<dyn Signer>,
        write_barrier: WriteBarrier,
    ) -> Result<Self> {
        std::fs::create_dir_all(&state_root)
            .context("failed to create state_root directory")?;

        let state_db = StateDb::open(&state_root)?;
        let runtime_db = runtime_db::RuntimeDb::open(&runtime_db_path)?;

        Ok(Self {
            inner: Mutex::new(Inner {
                state_db,
                runtime_db,
                signer,
                write_barrier,
            }),
        })
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

    /// Run a closure with access to the underlying StateDb.
    pub fn with_state_db<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&StateDb) -> Result<T>,
    {
        let g = self.lock()?;
        f(&g.state_db)
    }

    /// Run a closure with access to the projection database.
    pub fn with_projection<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&ryeos_state::ProjectionDb) -> Result<T>,
    {
        let g = self.lock()?;
        f(g.state_db.projection())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.inner.lock().map_err(|e| anyhow!("StateStore lock poisoned: {e}"))
    }

    /// Acquire a write permit from the write barrier.
    /// Fails if the daemon is quiescing for GC.
    fn acquire_write_permit(&self) -> Result<WritePermit> {
        let g = self.lock()?;
        g.write_barrier.try_acquire()
            .map_err(|e| anyhow!("cannot acquire write permit: {e}"))
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
    pub fn create_thread(
        &self,
        thread: &NewThreadRecord,
    ) -> Result<Vec<PersistedEventRecord>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let snapshot = build_snapshot(thread);

        if thread.thread_id == thread.chain_root_id {
            g.state_db
                .create_chain(&thread.thread_id, snapshot, g.signer.as_ref())?;
        } else {
            g.state_db
                .add_thread(&thread.chain_root_id, snapshot, g.signer.as_ref())?;
        }

        g.runtime_db
            .insert_thread_runtime(&thread.thread_id, &thread.chain_root_id)?;

        // Edge is derived from snapshot's upstream_thread_id during
        // project_thread_snapshot (see projection.rs). No direct write needed.

        let create_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &thread.kind,
                "item_ref": &thread.item_ref,
                "executor_ref": &thread.executor_ref,
                "launch_mode": &thread.launch_mode,
            }),
        };

        let te = convert_events(std::slice::from_ref(&create_event), &thread.chain_root_id, &thread.thread_id);
        let result = g.state_db.append_events(
            &thread.chain_root_id,
            &thread.thread_id,
            te,
            vec![],
            g.signer.as_ref(),
        )?;

        Ok(persisted_from_append(&result, &[create_event]))
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
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;

        if thread_row.status != "created" {
            bail!(
                "invalid status transition: {} -> running",
                thread_row.status
            );
        }

        let now = lillux::time::iso8601_now();
        let updated_snapshot = ThreadSnapshot {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_snapshot".to_string(),
            thread_id: thread_row.thread_id.clone(),
            chain_root_id: thread_row.chain_root_id.clone(),
            status: ThreadStatus::Running,
            kind_name: thread_row.kind.clone(),
            item_ref: thread_row.item_ref.clone(),
            executor_ref: thread_row.executor_ref.clone(),
            launch_mode: thread_row.launch_mode.clone(),
            current_site_id: thread_row.current_site_id.clone(),
            origin_site_id: thread_row.origin_site_id.clone(),
            upstream_thread_id: thread_row.upstream_thread_id.clone(),
            requested_by: thread_row.requested_by.clone(),
            base_project_snapshot_hash: base_project_snapshot_hash.map(String::from),
            result_project_snapshot_hash: None,
            created_at: thread_row.created_at.clone(),
            updated_at: now.clone(),
            started_at: Some(now.clone()),
            finished_at: None,
            result: None,
            error: None,
            budget: None,
            artifacts: vec![],
            facets: Default::default(),
            last_event_hash: None,
            last_chain_seq: 0,
            last_thread_seq: 0,
        };

        let snapshot_update = SnapshotUpdate {
            thread_id: thread_id.to_string(),
            new_snapshot: updated_snapshot,
        };

        let event = NewEventRecord {
            event_type: "thread_started".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({}),
        };

        let te = convert_events(std::slice::from_ref(&event), &thread_row.chain_root_id, thread_id);
        let result = g.state_db.append_events(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![snapshot_update],
            g.signer.as_ref(),
        )?;

        Ok(persisted_from_append(&result, &[event]))
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
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let thread_row = g
            .state_db
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow!("thread not found: {thread_id}"))?;

        if is_terminal_status(&thread_row.status) {
            bail!(
                "invalid status transition: {} -> {}",
                thread_row.status,
                update.status
            );
        }

        let now = lillux::time::iso8601_now();
        let terminal_status = ThreadStatus::from_str_lossy(&update.status)
            .ok_or_else(|| anyhow!("invalid terminal status: {}", update.status))?;

        let mut facets = BTreeMap::new();
        if let Some(ref cost) = update.final_cost {
            facets.insert("cost.turns".to_string(), cost.turns.to_string());
            facets.insert("cost.input_tokens".to_string(), cost.input_tokens.to_string());
            facets.insert("cost.output_tokens".to_string(), cost.output_tokens.to_string());
            facets.insert("cost.spend".to_string(), cost.spend.to_string());
            if let Some(ref provider) = cost.provider {
                facets.insert("cost.provider".to_string(), provider.clone());
            }
            if let Some(ref metadata) = cost.metadata {
                if let Ok(s) = serde_json::to_string(metadata) {
                    facets.insert("cost.metadata_json".to_string(), s);
                }
            }
        }

        let artifacts_json: Vec<Value> = update.artifacts.iter().map(|a| serde_json::to_value(a).unwrap()).collect();

        let updated_snapshot = ThreadSnapshot {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_snapshot".to_string(),
            thread_id: thread_row.thread_id.clone(),
            chain_root_id: thread_row.chain_root_id.clone(),
            status: terminal_status,
            kind_name: thread_row.kind.clone(),
            item_ref: thread_row.item_ref.clone(),
            executor_ref: thread_row.executor_ref.clone(),
            launch_mode: thread_row.launch_mode.clone(),
            current_site_id: thread_row.current_site_id.clone(),
            origin_site_id: thread_row.origin_site_id.clone(),
            upstream_thread_id: thread_row.upstream_thread_id.clone(),
            requested_by: thread_row.requested_by.clone(),
            base_project_snapshot_hash: None,
            result_project_snapshot_hash: None,
            created_at: thread_row.created_at.clone(),
            updated_at: now.clone(),
            started_at: thread_row.started_at.clone(),
            finished_at: Some(now.clone()),
            result: update.result_json.clone(),
            error: update.error_json.clone(),
            budget: update.final_cost.as_ref().map(|cost| {
                ThreadUsage {
                    completed_turns: cost.turns as u32,
                    input_tokens: cost.input_tokens as u64,
                    output_tokens: cost.output_tokens as u64,
                    spend_usd: cost.spend,
                    spawns_used: 0, // not tracked in FinalCost
                    started_at: thread_row.started_at.clone()
                        .unwrap_or_else(|| thread_row.created_at.clone()),
                    settled_at: now.clone(),
                    last_settled_turn_seq: cost.turns as u64,
                    elapsed_ms: 0, // daemon doesn't track wall-clock time
                }
            }),
            artifacts: artifacts_json,
            facets,
            last_event_hash: None,
            last_chain_seq: 0,
            last_thread_seq: 0,
        };

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
                }),
            });
        }

        let mut terminal_payload = json!({
            "outcome_code": update.outcome_code,
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
        let result = g.state_db.append_events(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![snapshot_update],
            g.signer.as_ref(),
        )?;

        Ok(persisted_from_append(&result, &events_to_append))
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
    pub fn create_continuation(
        &self,
        successor: &NewThreadRecord,
        source_thread_id: &str,
        chain_root_id: &str,
        reason: Option<&str>,
    ) -> Result<Vec<PersistedEventRecord>> {
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let source_row = g
            .state_db
            .get_thread(source_thread_id)?
            .ok_or_else(|| anyhow!("source thread not found: {source_thread_id}"))?;

        if is_terminal_status(&source_row.status)
            && source_row.status != "failed"
            && source_row.status != "completed"
        {
            bail!("cannot continue thread in terminal status '{}'", source_row.status);
        }

        let now = lillux::time::iso8601_now();
        let source_snapshot = ThreadSnapshot {
            schema: ryeos_state::objects::SCHEMA_VERSION,
            kind: "thread_snapshot".to_string(),
            thread_id: source_row.thread_id.clone(),
            chain_root_id: source_row.chain_root_id.clone(),
            status: ThreadStatus::Continued,
            kind_name: source_row.kind.clone(),
            item_ref: source_row.item_ref.clone(),
            executor_ref: source_row.executor_ref.clone(),
            launch_mode: source_row.launch_mode.clone(),
            current_site_id: source_row.current_site_id.clone(),
            origin_site_id: source_row.origin_site_id.clone(),
            upstream_thread_id: source_row.upstream_thread_id.clone(),
            requested_by: source_row.requested_by.clone(),
            base_project_snapshot_hash: None,
            result_project_snapshot_hash: None,
            created_at: source_row.created_at.clone(),
            updated_at: now.clone(),
            started_at: source_row.started_at.clone(),
            finished_at: Some(now),
            result: None,
            error: None,
            budget: None,
            artifacts: vec![],
            facets: Default::default(),
            last_event_hash: None,
            last_chain_seq: 0,
            last_thread_seq: 0,
        };

        let source_snapshot_update = SnapshotUpdate {
            thread_id: source_thread_id.to_string(),
            new_snapshot: source_snapshot,
        };

        // Ensure successor has upstream_thread_id set to source for edge derivation
        let mut successor_with_upstream = successor.clone();
        if successor_with_upstream.upstream_thread_id.is_none() {
            successor_with_upstream.upstream_thread_id = Some(source_thread_id.to_string());
        }
        let successor_snapshot = build_snapshot(&successor_with_upstream);
        g.state_db
            .add_thread(chain_root_id, successor_snapshot, g.signer.as_ref())?;

        g.runtime_db
            .insert_thread_runtime(&successor.thread_id, chain_root_id)?;

        // Edge is derived from successor snapshot's upstream_thread_id during
        // projection (see project_thread_snapshot in rye-state). No direct write needed.

        let source_event = NewEventRecord {
            event_type: "thread_continued".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "successor_thread_id": &successor.thread_id,
                "reason": reason,
            }),
        };

        let ste = convert_events(std::slice::from_ref(&source_event), chain_root_id, source_thread_id);
        let source_result = g.state_db.append_events(
            chain_root_id,
            source_thread_id,
            ste,
            vec![source_snapshot_update],
            g.signer.as_ref(),
        )?;

        let successor_event = NewEventRecord {
            event_type: "thread_created".to_string(),
            storage_class: "indexed".to_string(),
            payload: json!({
                "kind": &successor.kind,
                "item_ref": &successor.item_ref,
                "continuation_from": source_thread_id,
            }),
        };

        let sste = convert_events(std::slice::from_ref(&successor_event), chain_root_id, &successor.thread_id);
        let successor_result = g.state_db.append_events(
            chain_root_id,
            &successor.thread_id,
            sste,
            vec![],
            g.signer.as_ref(),
        )?;

        let mut all_events = persisted_from_append(&source_result, &[source_event]);
        all_events.extend(persisted_from_append(&successor_result, &[successor_event]));
        Ok(all_events)
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
            requested_by: thread_row.requested_by,
            created_at: thread_row.created_at,
            updated_at: thread_row.updated_at,
            started_at: thread_row.started_at,
            finished_at: thread_row.finished_at,
            runtime,
        }))
    }

    pub fn get_thread_result(&self, thread_id: &str) -> Result<Option<ThreadResultRecord>> {
        let g = self.lock()?;
        let result_row = queries::get_thread_result(g.state_db.projection(), thread_id)?;
        let result = match result_row {
            Some(row) => {
                let result_val = match row.result {
                    Some(bytes) => Some(serde_json::from_slice::<Value>(&bytes)
                        .with_context(|| {
                            format!(
                                "malformed JSON in thread_results.result for thread_id {}",
                                thread_id
                            )
                        })?),
                    None => None,
                };
                Some(ThreadResultRecord {
                    outcome_code: None,
                    result: result_val,
                    error: row.error.map(|e| json!(e)),
                    metadata: None,
                })
            }
            None => None,
        };
        Ok(result)
    }

    pub fn list_thread_artifacts(&self, thread_id: &str) -> Result<Vec<ThreadArtifactRecord>> {
        let g = self.lock()?;
        let artifact_rows = queries::list_thread_artifacts(g.state_db.projection(), thread_id)?;
        let mut records = Vec::with_capacity(artifact_rows.len());
        for (idx, row) in artifact_rows.into_iter().enumerate() {
            let metadata = match row.metadata {
                Some(bytes) => Some(serde_json::from_slice::<Value>(&bytes)
                    .with_context(|| {
                        format!(
                            "malformed JSON in thread_artifacts.metadata \
                             for artifact at index {idx} of thread_id {}",
                            thread_id
                        )
                    })?),
                None => None,
            };
            records.push(ThreadArtifactRecord {
                artifact_id: idx as i64 + 1,
                artifact_type: row.kind,
                uri: String::new(),
                content_hash: None,
                metadata,
            });
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
        let _permit = self.acquire_write_permit()?;
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

        let te = convert_events(std::slice::from_ref(&event), &thread_row.chain_root_id, thread_id);
        let result = g.state_db.append_events(
            &thread_row.chain_root_id,
            thread_id,
            te,
            vec![],
            g.signer.as_ref(),
        )?;

        let persisted = persisted_from_append(&result, &[event]);

        let persisted_event = persisted.into_iter().next()
            .ok_or_else(|| anyhow!("artifact_published event was not persisted for thread {thread_id}"))?;

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
        let g = self.lock()?;
        let thread_rows = queries::list_threads(g.state_db.projection(), limit)?;
        Ok(thread_rows
            .into_iter()
            .map(|row| ThreadListItem {
                thread_id: row.thread_id,
                chain_root_id: row.chain_root_id,
                kind: row.kind,
                status: row.status,
                item_ref: row.item_ref,
                launch_mode: row.launch_mode,
                current_site_id: row.current_site_id,
                origin_site_id: row.origin_site_id,
                created_at: row.created_at,
                updated_at: row.updated_at,
            })
            .collect())
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
                requested_by: row.requested_by,
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
                requested_by: row.requested_by,
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
                requested_by: row.requested_by,
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
        queries::active_thread_count(g.state_db.projection())}

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
        launch_metadata: &crate::launch_metadata::RuntimeLaunchMetadata,
    ) -> Result<()> {
        let g = self.lock()?;
        // Defensive: skip attach if the thread was already finalized
        // (e.g. cancelled while the runner was between spawn and attach).
        if let Some(thread) = g.state_db.get_thread(thread_id)? {
            if is_terminal_status(&thread.status) {
                tracing::warn!(
                    thread_id,
                    status = %thread.status,
                    pid,
                    pgid,
                    "skipping attach_process — thread already terminal"
                );
                return Ok(());
            }
        }
        g.runtime_db
            .attach_process(thread_id, pid, pgid, launch_metadata)
    }

    /// Read the auto-resume attempt counter for a thread.
    pub fn get_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let g = self.lock()?;
        g.runtime_db.get_resume_attempts(thread_id)
    }

    /// Atomically bump the auto-resume counter and return the
    /// post-increment value.
    pub fn bump_resume_attempts(&self, thread_id: &str) -> Result<u32> {
        let g = self.lock()?;
        g.runtime_db.bump_resume_attempts(thread_id)
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
        let _permit = self.acquire_write_permit()?;
        let g = self.lock()?;
        let te = convert_events(events, chain_root_id, thread_id);
        let result = g.state_db.append_events(
            chain_root_id,
            thread_id,
            te,
            vec![],
            g.signer.as_ref(),
        )?;
        Ok(persisted_from_append(&result, events))
    }

    pub fn replay_events(
        &self,
        chain_root_id: &str,
        thread_id: Option<&str>,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<PersistedEventRecord>> {
        let g = self.lock()?;
        let event_rows = queries::replay_events(g.state_db.projection(), chain_root_id, thread_id, after_seq, limit)?;
        event_rows
            .into_iter()
            .map(|row| {
                let payload: Value = serde_json::from_slice(&row.payload)
                    .with_context(|| {
                        format!(
                            "malformed JSON payload for event {} (chain_seq {})",
                            row.event_id, row.chain_seq
                        )
                    })?;
                Ok(PersistedEventRecord {
                    event_id: row.event_id,
                    chain_root_id: row.chain_root_id,
                    chain_seq: row.chain_seq,
                    thread_id: row.thread_id,
                    thread_seq: row.thread_seq,
                    event_type: row.event_type,
                    storage_class: row.durability,
                    ts: row.ts,
                    payload,
                })
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn submit_command(
        &self,
        cmd: &NewCommandRecord,
    ) -> Result<CommandRecord> {
        let g = self.lock()?;
        g.runtime_db.submit_command(cmd)
    }

    pub fn claim_commands(
        &self,
        thread_id: &str,
    ) -> Result<Vec<CommandRecord>> {
        let g = self.lock()?;
        g.runtime_db.claim_commands(thread_id)
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

    pub fn get_facets(&self, thread_id: &str) -> Result<Vec<(String, String)>> {
        let g = self.lock()?;
        let facet_rows = queries::get_facets(g.state_db.projection(), thread_id)?;
        Ok(facet_rows
            .into_iter()
            .map(|row| (row.key, String::from_utf8_lossy(&row.value).to_string()))
            .collect())
    }
}

fn is_terminal_status(status: &str) -> bool {
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
