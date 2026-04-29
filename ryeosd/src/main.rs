use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::{serve, Router};
use clap::Parser;
use tokio::net::{TcpListener, UnixListener};

use ryeosd::config::{self, Cli, Config};
use ryeosd::event_stream::{ThreadEventHub, DEFAULT_EVENT_STREAM_CAPACITY};
use ryeosd::execution::callback_token::CallbackCapabilityStore;
use ryeosd::identity::NodeIdentity;
use ryeosd::services::command_service::CommandService;
use ryeosd::services::event_store::EventStoreService;
use ryeosd::services::thread_lifecycle::ThreadLifecycleService;
use ryeosd::state::AppState;
use ryeosd::{
    api, auth, bootstrap, execution, kind_profiles, process, reconcile, routes, service_executor,
    service_registry, services, state, state_lock, state_store, uds,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli)?;

    // Initialize tracing with file sink (writes ndjson to <state_dir>/.ai/state/trace-events.ndjson).
    // Must come after config load so state_dir is known.
    // The init_if_missing / init_only paths below may create .ai/state/ if it doesn't exist yet,
    // but for_daemon_with_file_sink already creates .ai/state/ on its own.
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_daemon_with_file_sink(&config.state_dir));

    // --init-only: bootstrap node-local state (identity, trust, dirs) and exit.
    //
    // This intentionally does NOT walk or sign items in `system_data_dir`.
    // System-tier bundle items are operator/publisher-managed and out of
    // scope for daemon init. To sign bundle items, use the explicit signer
    // tool (`cargo run --example resign_yaml -p ryeos-engine -- <path>`).
    if cli.init_only {
        let force = cli.force;
        bootstrap::init(&config, &bootstrap::InitOptions { force })?;
        tracing::info!("init-only complete, exiting");
        return Ok(());
    }

    // Init-if-missing convenience: applies to BOTH daemon-start and the
    // standalone `run-service` subcommand. Done before subcommand dispatch
    // so standalone callers don't need a separate init step.
    //
    // We check for both node identity and vault key files rather than
    // `state_dir.exists()` because the tracing subscriber pre-creates
    // `<state_dir>/.ai/state/` before this code runs, which would
    // otherwise defeat the predicate. Either key missing triggers a
    // (load-or-create-per-key idempotent) init; this matters when a
    // test or operator has pre-placed one key but not the other.
    let vault_secret_path = config
        .state_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault")
        .join("private_key.pem");
    if cli.init_if_missing
        && (!config.node_signing_key_path.exists() || !vault_secret_path.exists())
    {
        bootstrap::init(&config, &bootstrap::InitOptions { force: false })?;
    }

    // Handle subcommands before daemon startup
    if let Some(ref cmd) = cli.command {
        match cmd {
            config::DaemonCommand::RunService { service_ref, params } => {
                return run_service_standalone(&config, service_ref, params.as_deref()).await;
            }
        }
    }

    // Verify initialization
    bootstrap::verify_initialized(&config)?;

    // Startup auto-signing is intentionally NOT called here.
    // The daemon start path is fail-closed on unsigned trust-sensitive items.
    // Use `ryeosd --init-only` (or `--init-only --force`) to sign items.

    process::remove_stale_socket(&config.uds_path)?;

    // ── Two-phase node-config bootstrap ──
    let (engine, node_config_snapshot) = bootstrap::load_node_config_two_phase(&config)?;

    // Build the service registry early — self-check needs it.
    let services = Arc::new(service_registry::build_service_registry());

    // Self-check: verify every registered service resolves and is trusted.
    // Every service must resolve, verify, extract an endpoint, AND have a
    // registered handler. Any failure prevents daemon start (fail-closed).
    let catalog_health = {
        let operational_services = ryeosd::service_handlers::ALL;

        let plan_ctx = ryeos_engine::contracts::PlanContext {
            requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: "fp:daemon-self-check".into(),
                    scopes: vec![],
                },
            ),
            project_context: ryeos_engine::contracts::ProjectContext::None,
            current_site_id: "site:local".into(),
            origin_site_id: "site:local".into(),
            execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
            validate_only: true,
        };

        let mut failed: Vec<(&str, String)> = Vec::new();
        let mut missing: Vec<&str> = Vec::new();

        for desc in operational_services {
            let canonical = match ryeos_engine::canonical_ref::CanonicalRef::parse(desc.service_ref) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(service = desc.service_ref, error = %e, "operational service ref parse failed");
                    continue;
                }
            };

            match engine.resolve(&plan_ctx, &canonical) {
                Ok(resolved) => {
                    match engine.verify(&plan_ctx, resolved) {
                        Ok(verified) => {
                            match ryeosd::service_registry::extract_endpoint(&verified.resolved.metadata.extra) {
                                Ok(endpoint) => {
                                    if !services.has(&endpoint) {
                                        let msg = format!(
                                            "service resolves to endpoint '{}' but no handler registered",
                                            endpoint
                                        );
                                        tracing::error!(service = desc.service_ref, %endpoint, "{}", msg);
                                        failed.push((desc.service_ref, msg));
                                    } else {
                                        tracing::debug!(
                                            service = desc.service_ref,
                                            trust_class = ?verified.trust_class,
                                            %endpoint,
                                            "operational service verified"
                                        );
                                    }
                                }
                                Err(e) => {
                                    let msg = format!("endpoint extraction failed: {e}");
                                    tracing::error!(service = desc.service_ref, error = %e, "{}", msg);
                                    failed.push((desc.service_ref, msg));
                                }
                            }
                        }
                        Err(e) => {
                            let msg = format!("{e}");
                            tracing::error!(service = desc.service_ref, error = %msg, "operational service verification FAILED");
                            failed.push((desc.service_ref, msg));
                        }
                    }
                }
                Err(_) => {
                    tracing::warn!(service = desc.service_ref, "operational service not found in bundle");
                    missing.push(desc.service_ref);
                }
            }
        }

        if !failed.is_empty() {
            for (svc, error) in &failed {
                tracing::error!(svc, error, "refusing to start: operational service failed verification");
            }
            anyhow::bail!(
                "operational service catalog self-check failed: {} service(s) failed verification",
                failed.len()
            );
        }

        if !missing.is_empty() {
            for svc in &missing {
                tracing::error!(svc, "refusing to start: operational service not found in bundle");
            }
            anyhow::bail!(
                "operational service catalog self-check failed: {} service(s) missing",
                missing.len()
            );
        }

        ryeosd::state::CatalogHealth {
            status: "ok".into(),
            missing_services: vec![],
        }
    };

    // Build the route table from the node-config snapshot.
    let route_table = {
        let table = routes::build_route_table_or_bail(&node_config_snapshot)
            .context("route table build failed at startup — check route YAML files")?;
        Arc::new(arc_swap::ArcSwap::from_pointee(table))
    };
    tracing::info!(routes = route_table.load().all.len(), "route table built");

    // These must be initialized after the self-check (which only needs engine + services).
    let kind_profiles = Arc::new(kind_profiles::KindProfileRegistry::load_from_config(&config));
    let identity = NodeIdentity::load(&config.node_signing_key_path)?;
    
    let state_root = config.state_dir.join(".ai").join("state");
    let runtime_db_path = config.db_path.clone();
    let signer = Arc::new(state_store::NodeIdentitySigner::from_identity(&identity));
    
    let write_barrier = ryeosd::write_barrier::WriteBarrier::new();
    
    let state_store = Arc::new(state_store::StateStore::new(state_root, runtime_db_path, signer, write_barrier.clone())
        .context("StateStore initialization failed")?);
    tracing::info!("StateStore initialized successfully");

    // Acquire operator state lock — prevents standalone services from
    // running while the daemon is up. Released on process exit (Drop).
    let _state_lock = state_lock::StateLock::acquire(
        &state_lock::default_lock_path(&config.state_dir),
    ).context("failed to acquire state lock — is another ryeosd instance or standalone service running?")?;
    tracing::info!("State lock acquired");
    
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
    // `services` already built above for self-check

    // Operator-secret store. Sealed-envelope (X25519 + XChaCha20-Poly1305)
    // store at `<state>/.ai/state/secrets/store.enc`, decrypted with
    // the vault X25519 secret key generated by bootstrap. Subprocesses
    // inherit only the secrets they declare via `required_secrets`,
    // through the existing `vault_bindings` plumbing in
    // `services::thread_lifecycle::spawn_item`. Daemon stays vendor-
    // agnostic — `vault.rs` only moves opaque `String -> String` pairs.
    let vault: Arc<dyn ryeosd::vault::NodeVault> = Arc::new(
        ryeosd::vault::SealedEnvelopeVault::load(&config.state_dir)
            .context("load sealed-envelope vault — did `rye init` (or daemon bootstrap) run?")?,
    );

    let app_state = AppState {
        config: Arc::new(config.clone()),
        state_store,
        engine: engine.clone(),
        identity: Arc::new(identity),
        threads,
        events,
        event_streams: Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY)),
        commands,
        callback_tokens,
        write_barrier: Arc::new(write_barrier),
        started_at: Instant::now(),
        started_at_iso: lillux::time::iso8601_now(),
        catalog_health,
        services,
        node_config: node_config_snapshot,
        route_table,
        webhook_dedupe: Arc::new(crate::routes::webhook_dedupe::WebhookDedupeStore::new()),
        vault,
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
        "bind": config.bind.to_string(),
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
            let params = match ryeosd::execution::runner::execution_params_from_resume_context(
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
                        &ryeosd::services::thread_lifecycle::ThreadFinalizeParams {
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
            if let Err(err) = ryeosd::execution::runner::run_existing_detached(
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
        .fallback(routes::dispatcher::custom_route_dispatcher)
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

/// Run a service in standalone mode (daemon is not running).
///
/// Performs bootstrap: load config, build engine + trust store,
/// load full node-config, build service registry, acquire state lock, dispatch via shared executor.
async fn run_service_standalone(
    config: &config::Config,
    service_ref: &str,
    params_json: Option<&str>,
) -> Result<()> {
    use ryeosd::service_executor::{ExecutionContext, ExecutionMode};

    // Verify initialization
    bootstrap::verify_initialized(config)?;

    // Minimal bootstrap (subset of daemon startup)
    let kind_profiles = Arc::new(kind_profiles::KindProfileRegistry::load_from_config(config));
    let identity = NodeIdentity::load(&config.node_signing_key_path)?;

    // Two-phase node-config bootstrap (same as daemon-start path)
    let (engine, node_config_snapshot) = bootstrap::load_node_config_two_phase(config)?;

    let services = Arc::new(service_registry::build_service_registry());

    // Acquire state lock (prevents concurrent daemon)
    let _state_lock = state_lock::StateLock::acquire(
        &state_lock::default_lock_path(&config.state_dir),
    ).context("failed to acquire state lock — is the daemon running?")?;

    let state_root = config.state_dir.join(".ai").join("state");
    let runtime_db_path = config.db_path.clone();
    let signer = Arc::new(state_store::NodeIdentitySigner::from_identity(&identity));
    let write_barrier = ryeosd::write_barrier::WriteBarrier::new();
    let state_store = Arc::new(state_store::StateStore::new(
        state_root, runtime_db_path, signer, write_barrier.clone(),
    )?);

    let events = Arc::new(services::event_store::EventStoreService::new(state_store.clone()));
    let threads = Arc::new(services::thread_lifecycle::ThreadLifecycleService::new(
        state_store.clone(),
        kind_profiles.clone(),
        events.clone(),
    ));
    let commands = Arc::new(services::command_service::CommandService::new(
        state_store.clone(),
        kind_profiles,
        events.clone(),
    ));

    let app_state = state::AppState {
        config: Arc::new(config.clone()),
        state_store,
        engine: engine.clone(),
        identity: Arc::new(identity),
        threads,
        events,
        event_streams: Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY)),
        commands,
        callback_tokens: Arc::new(execution::callback_token::CallbackCapabilityStore::new()),
        write_barrier: Arc::new(write_barrier),
        started_at: Instant::now(),
        started_at_iso: lillux::time::iso8601_now(),
        catalog_health: state::CatalogHealth {
            status: "standalone".into(),
            missing_services: vec![],
        },
        services,
        node_config: node_config_snapshot.clone(),
        route_table: Arc::new(arc_swap::ArcSwap::from_pointee(
            routes::build_route_table_or_bail(&node_config_snapshot)?,
        )),
        webhook_dedupe: Arc::new(routes::webhook_dedupe::WebhookDedupeStore::new()),
        vault: Arc::new(
            ryeosd::vault::SealedEnvelopeVault::load(&config.state_dir)
                .context("load sealed-envelope vault — did `rye init` run?")?,
        ),
    };

    let params: serde_json::Value = match params_json {
        Some(json_str) => serde_json::from_str(json_str)
            .with_context(|| "parse --params as JSON")?,
        None => serde_json::json!({}),
    };

    let ctx = ExecutionContext {
        principal_fingerprint: "fp:standalone-operator".into(),
        caller_scopes: vec![], // standalone: operator authority, no caps
        engine,
        plan_ctx: ryeos_engine::contracts::PlanContext {
            requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                ryeos_engine::contracts::Principal {
                    fingerprint: "fp:standalone-operator".into(),
                    scopes: vec![],
                },
            ),
            project_context: ryeos_engine::contracts::ProjectContext::None,
            current_site_id: "site:local".into(),
            origin_site_id: "site:local".into(),
            execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
            validate_only: false,
        },
    };

    let result = service_executor::execute_service(
        service_ref,
        params,
        ExecutionMode::Standalone,
        &ctx,
        &app_state,
    )
    .await?;

    tracing::info!(
        endpoint = %result.endpoint,
        trust_class = ?result.trust_class,
        effective_caps = ?result.effective_caps,
        audit_thread_id = %result.audit_thread_id,
        "standalone service completed"
    );

    // Print result to stdout
    println!("{}", serde_json::to_string_pretty(&result.value)?);

    Ok(())
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
