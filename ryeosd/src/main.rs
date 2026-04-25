mod api;
mod auth;
mod bootstrap;
mod config;
mod engine_init;
mod execution;
mod identity;
mod kind_profiles;
mod launch_metadata;
mod maintenance;
mod policy;
mod process;
mod reconcile;
mod runtime_db;
mod services;
mod state;
mod state_store;
mod uds;
mod write_barrier;

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::{serve, Router};
use clap::Parser;
use config::{Cli, Config};
use tokio::net::{TcpListener, UnixListener};
use tracing_subscriber::EnvFilter;

use crate::execution::callback_token::CallbackCapabilityStore;
use crate::identity::NodeIdentity;
use crate::services::command_service::CommandService;
use crate::services::event_store::EventStoreService;
use crate::services::thread_lifecycle::ThreadLifecycleService;
use crate::state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ryeosd=info,rye_engine=info")),
        )
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli)?;

    // --init-only: run bootstrap + sign, then exit
    if cli.init_only {
        let force = cli.force;
        bootstrap::init(&config, &bootstrap::InitOptions { force })?;
        bootstrap::sign_unsigned_items(&config);
        tracing::info!("init-only complete, exiting");
        return Ok(());
    }

    // Init-if-missing convenience (creates runtime dirs + default config)
    if cli.init_if_missing && !config.state_dir.exists() {
        bootstrap::init(&config, &bootstrap::InitOptions { force: false })?;
    }

    // Verify initialization
    bootstrap::verify_initialized(&config)?;

    // Sign any unsigned bundle items using the user's signing key.
    // Runs on every start so that newly-added bundles get signed on first use.
    bootstrap::sign_unsigned_items(&config);

    process::remove_stale_socket(&config.uds_path)?;

    let kind_profiles = Arc::new(kind_profiles::KindProfileRegistry::load_from_config(&config));
    let identity = NodeIdentity::load(&config.signing_key_path)?;
    let engine = Arc::new(engine_init::build_engine(&config)?);
    
    let state_root = config.state_dir.join(".state");
    let runtime_db_path = config.db_path.clone();
    let signer = Arc::new(state_store::NodeIdentitySigner::from_identity(&identity));
    
    let write_barrier = crate::write_barrier::WriteBarrier::new();
    
    let state_store = Arc::new(state_store::StateStore::new(state_root, runtime_db_path, signer, write_barrier.clone())
        .context("StateStore initialization failed")?);
    tracing::info!("StateStore initialized successfully");
    
    let events = Arc::new(EventStoreService::new(
        state_store.clone(),
    ));
    let threads = Arc::new(ThreadLifecycleService::new(
        state_store.clone(),
        kind_profiles.clone(),
        events.clone(),
    ));
    let commands = Arc::new(CommandService::new(state_store.clone(), kind_profiles.clone(), events.clone()));
    let callback_tokens = Arc::new(CallbackCapabilityStore::new());

    let app_state = AppState {
        config: Arc::new(config.clone()),
        state_store,
        engine: engine.clone(),
        identity: Arc::new(identity),
        threads,
        events,
        commands,
        callback_tokens,
        write_barrier: Arc::new(write_barrier),
        started_at: Instant::now(),
        started_at_iso: lillux::time::iso8601_now(),
    };

    // Reconcile threads from the previous run BEFORE binding listeners,
    // but DO NOT dispatch the resume intents yet — a resumed subprocess
    // making its first daemon callback before the UDS / HTTP server is
    // bound would fail. We collect intents here and dispatch them
    // below, after the listeners are accepting connections.
    let resume_intents = reconcile::reconcile(&app_state).await?;

    let app = build_router(app_state.clone())
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            auth::auth_middleware,
        ));
    let tcp_listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;
    let uds_listener = UnixListener::bind(&config.uds_path)
        .with_context(|| format!("failed to bind {}", config.uds_path.display()))?;

    std::env::set_var("RYEOSD_SOCKET_PATH", &config.uds_path);
    std::env::set_var("RYEOSD_URL", format!("http://{}", config.bind));

    // Write daemon.json so tools can discover the daemon.
    // This is the discovery contract — fail if we can't write it.
    let daemon_info = serde_json::json!({
        "pid": std::process::id(),
        "socket": config.uds_path.display().to_string(),
        "started_at": lillux::time::iso8601_now(),
    });
    let daemon_json_path = config.state_dir.join("daemon.json");
    std::fs::write(
        &daemon_json_path,
        serde_json::to_string_pretty(&daemon_info)?,
    )
    .with_context(|| format!(
        "failed to write daemon.json at {} — tools cannot discover the daemon without it",
        daemon_json_path.display()
    ))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.uds_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set socket permissions on {}", config.uds_path.display()))?;
    }

    let uds_state = Arc::new(app_state.clone());
    let uds_task = tokio::spawn(async move { uds::server::serve(uds_listener, uds_state).await });

    // HTTP server is started BEFORE dispatching resume intents.
    // A resumed subprocess that prefers RYEOSD_URL over
    // RYEOSD_SOCKET_PATH would otherwise hit a cold server when it
    // makes its first callback. Spawning the serve future here, then
    // selecting on it below, ensures the accept loop is live before
    // any reconciler-spawned child can start.
    let shutdown = shutdown_signal();
    let http_task = tokio::spawn(async move {
        serve(tcp_listener, app)
            .with_graceful_shutdown(shutdown)
            .await
    });

    // Listeners are bound and BOTH the UDS accept loop and HTTP
    // accept loop are running. Now dispatch the resume intents
    // collected by `reconcile`. These run as in-runtime tokio tasks
    // (NOT daemon-detached) so that shutdown's
    // `drain_running_threads` covers them. Failures finalize the
    // thread immediately so it never sits in a non-terminal state
    // until the next daemon restart.
    for intent in resume_intents {
        let st = app_state.clone();
        let threads = app_state.threads.clone();
        tokio::spawn(async move {
            let params = match crate::execution::runner::execution_params_from_resume_context(
                &st,
                &intent.resume_context,
            ) {
                Ok(p) => p,
                Err(err) => {
                    tracing::error!(
                        thread_id = %intent.thread_id,
                        error = %err,
                        "resume: failed to build ExecutionParams from ResumeContext — finalizing"
                    );
                    if let Err(fin_err) = threads.finalize_thread(
                        &crate::services::thread_lifecycle::ThreadFinalizeParams {
                            thread_id: intent.thread_id.clone(),
                            status: "failed".to_string(),
                            outcome_code: Some("resume_rebuild_failed".to_string()),
                            result: None,
                            error: Some(serde_json::json!({
                                "code": "resume_rebuild_failed",
                                "message": err.to_string(),
                            })),
                            metadata: None,
                            artifacts: Vec::new(),
                            final_cost: None,
                            summary_json: None,
                        },
                    ) {
                        tracing::warn!(
                            thread_id = %intent.thread_id,
                            error = %fin_err,
                            "resume: finalize after rebuild failure also failed"
                        );
                    }
                    return;
                }
            };
            if let Err(err) = crate::execution::runner::run_existing_detached(
                st,
                intent.thread_id.clone(),
                intent.chain_root_id,
                params,
                intent.prior_status,
            )
            .await
            {
                tracing::error!(
                    thread_id = %intent.thread_id,
                    error = %err,
                    "resume: dispatch failed"
                );
            }
        });
    }

    tokio::select! {
        result = http_task => {
            result
                .context("http task join failed")?
                .context("http server exited unexpectedly")?;
        }
        result = uds_task => {
            result.context("uds task join failed")??;
        }
    }

    // Drain running threads on shutdown
    drain_running_threads(&app_state);

    // Cleanup daemon.json
    let _ = std::fs::remove_file(&daemon_json_path);

    if config.uds_path.exists() {
        let _ = std::fs::remove_file(&config.uds_path);
    }

    Ok(())
}

/// Resolve the cancellation policy for a thread to a concrete daemon action.
///
/// - `Some(CancellationMode::Hard)` → SIGKILL only (no SIGTERM).
/// - `Some(CancellationMode::Graceful { grace_secs })` → SIGTERM, wait
///   `grace_secs`, then SIGKILL.
/// - `None` → default 3s graceful (used when a tool did not declare
///   `native_async`).
fn resolve_shutdown_action(
    mode: Option<ryeos_engine::contracts::CancellationMode>,
) -> ShutdownAction {
    use ryeos_engine::contracts::CancellationMode;
    match mode {
        Some(CancellationMode::Hard) => ShutdownAction::Hard,
        Some(CancellationMode::Graceful { grace_secs }) => {
            ShutdownAction::Graceful(std::time::Duration::from_secs(grace_secs))
        }
        None => ShutdownAction::Graceful(std::time::Duration::from_secs(3)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownAction {
    Hard,
    Graceful(std::time::Duration),
}

fn drain_running_threads(state: &AppState) {
    let threads = match state.state_store.list_threads_by_status(&["running"]) {
        Ok(threads) => threads,
        Err(err) => {
            tracing::warn!(error = %err, "failed to list running threads during shutdown");
            return;
        }
    };

    if threads.is_empty() {
        return;
    }

    tracing::info!(count = threads.len(), "draining running threads");

    let daemon_pgid = process::daemon_pgid();

    for thread in &threads {
        if let Some(pgid) = thread.runtime.pgid {
            if pgid == daemon_pgid {
                tracing::debug!(
                    thread_id = %thread.thread_id,
                    pgid,
                    "skipping thread — PGID matches daemon"
                );
                continue;
            }
            let action = resolve_shutdown_action(
                thread.runtime.launch_metadata.as_ref().and_then(
                    |lm| lm.cancellation_mode,
                ),
            );
            tracing::info!(
                pgid,
                thread_id = %thread.thread_id,
                action = ?action,
                "killing process group"
            );
            let result = match action {
                ShutdownAction::Hard => process::hard_kill_process_group(pgid),
                ShutdownAction::Graceful(grace) => {
                    process::kill_process_group(pgid, grace)
                }
            };
            if !result.success {
                tracing::warn!(
                    pgid,
                    method = %result.method,
                    "failed to kill process group"
                );
            }
        }
    }
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(api::health::health))
        .route("/execute", post(api::execute::execute))
        .with_state(state)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => { sigterm.recv().await; }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod shutdown_mapping_tests {
    use super::{resolve_shutdown_action, ShutdownAction};
    use ryeos_engine::contracts::CancellationMode;
    use std::time::Duration;

    #[test]
    fn hard_mode_maps_to_hard_kill() {
        assert_eq!(
            resolve_shutdown_action(Some(CancellationMode::Hard)),
            ShutdownAction::Hard
        );
    }

    #[test]
    fn graceful_mode_uses_declared_grace() {
        assert_eq!(
            resolve_shutdown_action(Some(CancellationMode::Graceful { grace_secs: 11 })),
            ShutdownAction::Graceful(Duration::from_secs(11))
        );
    }

    #[test]
    fn no_mode_falls_back_to_three_second_default() {
        assert_eq!(
            resolve_shutdown_action(None),
            ShutdownAction::Graceful(Duration::from_secs(3))
        );
    }

    #[test]
    fn graceful_zero_is_preserved() {
        // Graceful{0} is distinct from Hard — we still attempt SIGTERM
        // first via the poll loop, just with a zero-length grace.
        assert_eq!(
            resolve_shutdown_action(Some(CancellationMode::Graceful { grace_secs: 0 })),
            ShutdownAction::Graceful(Duration::from_secs(0))
        );
    }
}
