use anyhow::Result;
use serde_json::json;

use crate::launch_metadata::{ResumeContext, RuntimeLaunchMetadata};
use crate::process::pgid_alive;
use crate::services::thread_lifecycle::ThreadFinalizeParams;
use crate::state::AppState;

/// Decision the reconciler makes for a single dead thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeDecision {
    /// No `native_resume` policy on the persisted spec — finalize.
    NoResumePolicy,
    /// Spec declared `native_resume` but no `ResumeContext` was
    /// captured at spawn (corruption, partial spawn, schema drift).
    /// Cannot rebuild `ExecutionParams` — finalize.
    MissingResumeContext,
    /// Thread is eligible for auto-resume; the reconciler should
    /// increment the counter and re-spawn with `RYE_RESUME=1`.
    Resume { next_attempt: u32, max: u32 },
    /// Spec declared `native_resume` and a `ResumeContext` is present,
    /// but the retry budget is exhausted — finalize.
    Exhausted { attempts: u32, max: u32 },
}

/// Pure mapping from `(launch_metadata, current_attempts)` to a
/// reconciler action. Extracted so it can be unit-tested without
/// touching the daemon state.
pub fn decide_resume(
    metadata: Option<&RuntimeLaunchMetadata>,
    current_attempts: u32,
) -> ResumeDecision {
    let m = match metadata {
        Some(m) if m.declares_native_resume() => m,
        _ => return ResumeDecision::NoResumePolicy,
    };
    let policy = m
        .native_resume
        .as_ref()
        .expect("declares_native_resume implies native_resume.is_some()");
    // resume_context is required to actually re-spawn; without it we
    // can't reconstruct ExecutionParams.
    if metadata.and_then(|m| m.resume_context.as_ref()).is_none() {
        return ResumeDecision::MissingResumeContext;
    }
    if current_attempts >= policy.max_auto_resume_attempts {
        return ResumeDecision::Exhausted {
            attempts: current_attempts,
            max: policy.max_auto_resume_attempts,
        };
    }
    ResumeDecision::Resume {
        next_attempt: current_attempts + 1,
        max: policy.max_auto_resume_attempts,
    }
}

/// A thread that the reconciler decided to auto-resume. The dispatcher
/// (in `main.rs`, AFTER the daemon's listeners are bound) consumes
/// this and calls `runner::run_existing_detached`.
#[derive(Debug, Clone)]
pub struct ResumeIntent {
    pub thread_id: String,
    pub chain_root_id: String,
    pub resume_context: ResumeContext,
    /// Status of the thread row at reconcile time. The dispatcher uses
    /// this to decide whether to call `mark_running` after attaching
    /// the new spawn — `created` rows have never been transitioned,
    /// and `drain_running_threads` only sees `running` rows.
    pub prior_status: String,
}

/// Finalize a dead thread as failed. Centralized so the `Finalize` /
/// `FinalizeExhausted` / orphan branches share identical bookkeeping.
fn finalize_dead(
    state: &AppState,
    thread_id: &str,
    pgid: Option<i64>,
    prior_status: &str,
    extra: Option<(&'static str, serde_json::Value)>,
    reconciled: &mut usize,
) {
    let (outcome_code, error_extra) = match extra {
        Some((code, val)) => (code, Some(val)),
        None => ("daemon_reconciled", None),
    };
    let mut error_obj = serde_json::json!({
        "message": "thread in non-terminal state after daemon restart; process is dead",
        "reconciled_from_status": prior_status,
        "pgid": pgid,
    });
    if let Some(extra) = error_extra {
        error_obj["details"] = extra;
    }
    if let Err(err) = state.threads.finalize_thread(&ThreadFinalizeParams {
        thread_id: thread_id.to_string(),
        status: "failed".to_string(),
        outcome_code: Some(outcome_code.to_string()),
        result: None,
        error: Some(error_obj),
        metadata: None,
        artifacts: Vec::new(),
        final_cost: None,
        summary_json: None,
    }) {
        tracing::warn!(
            thread_id,
            error = %err,
            "failed to finalize thread"
        );
    } else {
        *reconciled += 1;
    }
}

/// Reconcile daemon state after restart.
///
/// Three phases:
/// 1. Catch up projection from CAS (repair any projection drift)
/// 2. Find threads left in non-terminal status whose processes are dead
///    (or whose spawn was interrupted before pid/pgid attach)
/// 3. For each: either finalize-as-failed, or collect a `ResumeIntent`
///    that the caller dispatches AFTER the daemon's listeners are bound
///
/// Returns the `ResumeIntent`s the caller must dispatch. The reconciler
/// itself never calls `run_existing_detached` — it would race the
/// daemon's listener startup, and a resumed subprocess making its first
/// callback before the UDS / HTTP server is bound would fail.
#[tracing::instrument(name = "state:reconcile", skip(state))]
pub async fn reconcile(state: &AppState) -> Result<Vec<ResumeIntent>> {
    // Catch up projection from CAS.
    let cas_root = state.state_store.cas_root()?;
    let refs_root = state.state_store.refs_root()?;

    state.state_store.with_projection(|projection| {
        let catch_up = ryeos_state::rebuild::catch_up_projection(
            projection,
            &cas_root,
            &refs_root,
        )?;

        if catch_up.chains_updated > 0 {
            tracing::info!(
                chains_checked = catch_up.chains_checked,
                chains_updated = catch_up.chains_updated,
                threads_restored = catch_up.threads_restored,
                events_projected = catch_up.events_projected,
                "projection caught up from CAS"
            );
        }

        Ok::<_, anyhow::Error>(())
    })?;

    // Orphan thread cleanup.
    let running_threads = state
        .state_store
        .list_threads_by_status(&["created", "running"])?;

    if running_threads.is_empty() {
        tracing::debug!("no orphaned threads");
        return Ok(Vec::new());
    }

    tracing::info!(count = running_threads.len(), "checking non-terminal threads");

    let mut reconciled = 0usize;
    let mut intents: Vec<ResumeIntent> = Vec::new();

    for thread in &running_threads {
        let pgid = thread.runtime.pgid;

        // A thread is "dead" when:
        //  - it has a pgid and that pgid is no longer alive, OR
        //  - it has NO pid/pgid: spawn was interrupted before attach
        //    (created/running but never made it to attach_process).
        // Both cases route through the same decide_resume path.
        let process_dead = match pgid {
            Some(p) => !pgid_alive(p),
            None => true,
        };

        if !process_dead {
            continue;
        }

        let interrupted_spawn = pgid.is_none();

        let attempts = match state.state_store.get_resume_attempts(&thread.thread_id) {
            Ok(n) => n,
            Err(err) => {
                tracing::error!(
                    thread_id = %thread.thread_id,
                    error = %err,
                    "failed to read resume_attempts; finalizing as failed instead of resuming"
                );
                finalize_dead(
                    state,
                    &thread.thread_id,
                    pgid,
                    &thread.status,
                    Some((
                        "resume_counter_io_error",
                        json!({"error": err.to_string()}),
                    )),
                    &mut reconciled,
                );
                continue;
            }
        };
        let decision = decide_resume(thread.runtime.launch_metadata.as_ref(), attempts);

        match decision {
            ResumeDecision::NoResumePolicy => {
                let extra = if interrupted_spawn {
                    Some(("interrupted_spawn", json!({"reason": "no pid/pgid attached"})))
                } else {
                    None
                };
                tracing::info!(
                    thread_id = %thread.thread_id,
                    kind = %thread.kind,
                    status = %thread.status,
                    pgid = ?pgid,
                    interrupted_spawn,
                    "dead process, no native_resume policy — finalizing failed"
                );
                finalize_dead(
                    state,
                    &thread.thread_id,
                    pgid,
                    &thread.status,
                    extra,
                    &mut reconciled,
                );
            }
            ResumeDecision::MissingResumeContext => {
                tracing::warn!(
                    thread_id = %thread.thread_id,
                    kind = %thread.kind,
                    status = %thread.status,
                    pgid = ?pgid,
                    "native_resume declared but no ResumeContext captured — \
                     cannot rebuild ExecutionParams; finalizing failed"
                );
                finalize_dead(
                    state,
                    &thread.thread_id,
                    pgid,
                    &thread.status,
                    Some((
                        "missing_resume_context",
                        json!({"reason": "no resume_context in launch_metadata"}),
                    )),
                    &mut reconciled,
                );
            }
            ResumeDecision::Exhausted { attempts, max } => {
                tracing::warn!(
                    thread_id = %thread.thread_id,
                    attempts,
                    max,
                    "native_resume retry budget exhausted — finalizing failed"
                );
                finalize_dead(
                    state,
                    &thread.thread_id,
                    pgid,
                    &thread.status,
                    Some((
                        "resume_exhausted",
                        json!({"attempts": attempts, "max": max}),
                    )),
                    &mut reconciled,
                );
            }
            ResumeDecision::Resume { next_attempt, max } => {
                // Persist the bumped counter BEFORE scheduling the
                // re-spawn so a crash mid-resume does not grant an
                // infinite retry loop. Source of truth lives in
                // runtime_db.thread_runtime.resume_attempts.
                if let Err(err) = state.state_store.bump_resume_attempts(&thread.thread_id) {
                    tracing::warn!(
                        thread_id = %thread.thread_id,
                        error = %err,
                        "failed to persist resume_attempts; finalizing instead of resuming"
                    );
                    finalize_dead(
                        state,
                        &thread.thread_id,
                        pgid,
                        &thread.status,
                        Some((
                            "resume_counter_io_error",
                            json!({"error": err.to_string()}),
                        )),
                        &mut reconciled,
                    );
                    continue;
                }
                // Pull the resume_context for the dispatcher.
                let resume_context = thread
                    .runtime
                    .launch_metadata
                    .as_ref()
                    .and_then(|m| m.resume_context.clone())
                    .expect("decide_resume==Resume implies resume_context is Some");
                tracing::info!(
                    thread_id = %thread.thread_id,
                    attempt = next_attempt,
                    max,
                    "native_resume eligible — collecting ResumeIntent for post-listener dispatch"
                );
                intents.push(ResumeIntent {
                    thread_id: thread.thread_id.clone(),
                    chain_root_id: thread.chain_root_id.clone(),
                    resume_context,
                    prior_status: thread.status.clone(),
                });
                reconciled += 1;
            }
        }
    }

    tracing::info!(
        count = reconciled,
        resume_intents = intents.len(),
        "reconciled orphaned threads"
    );
    Ok(intents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch_metadata::ResumeContext;
    use ryeos_engine::contracts::{
        EffectivePrincipal, ExecutionHints, NativeResumeSpec, Principal, ProjectContext,
    };
    use std::path::PathBuf;

    fn principal() -> EffectivePrincipal {
        EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test".into(),
            scopes: vec!["execute".into()],
        })
    }

    fn ctx() -> ResumeContext {
        ResumeContext {
            kind: "tool_run".into(),
            item_ref: "ns/foo".into(),
            launch_mode: "detached".into(),
            parameters: serde_json::json!({}),
            project_context: ProjectContext::LocalPath {
                path: PathBuf::from("/tmp/p"),
            },
            original_snapshot_hash: None,
            current_site_id: "site:a".into(),
            origin_site_id: "site:a".into(),
            requested_by: principal(),
            execution_hints: ExecutionHints::default(),
            effective_caps: Vec::new(),
        }
    }

    #[test]
    fn no_metadata_finalizes() {
        assert_eq!(decide_resume(None, 0), ResumeDecision::NoResumePolicy);
    }

    #[test]
    fn no_native_resume_finalizes() {
        let m = RuntimeLaunchMetadata::default();
        assert_eq!(decide_resume(Some(&m), 0), ResumeDecision::NoResumePolicy);
    }

    #[test]
    fn missing_resume_context_distinct_variant() {
        let m = RuntimeLaunchMetadata {
            native_resume: Some(NativeResumeSpec {
                checkpoint_interval_secs: 30,
                max_auto_resume_attempts: 1,
            }),
            ..Default::default()
        };
        assert_eq!(
            decide_resume(Some(&m), 0),
            ResumeDecision::MissingResumeContext
        );
    }

    #[test]
    fn within_budget_resumes() {
        let m = RuntimeLaunchMetadata {
            native_resume: Some(NativeResumeSpec {
                checkpoint_interval_secs: 30,
                max_auto_resume_attempts: 3,
            }),
            resume_context: Some(ctx()),
            ..Default::default()
        };
        assert_eq!(
            decide_resume(Some(&m), 0),
            ResumeDecision::Resume {
                next_attempt: 1,
                max: 3
            }
        );
        assert_eq!(
            decide_resume(Some(&m), 2),
            ResumeDecision::Resume {
                next_attempt: 3,
                max: 3
            }
        );
    }

    #[test]
    fn over_budget_exhausts() {
        let m = RuntimeLaunchMetadata {
            native_resume: Some(NativeResumeSpec {
                checkpoint_interval_secs: 30,
                max_auto_resume_attempts: 2,
            }),
            resume_context: Some(ctx()),
            ..Default::default()
        };
        assert_eq!(
            decide_resume(Some(&m), 2),
            ResumeDecision::Exhausted {
                attempts: 2,
                max: 2
            }
        );
        assert_eq!(
            decide_resume(Some(&m), 5),
            ResumeDecision::Exhausted {
                attempts: 5,
                max: 2
            }
        );
    }
}
