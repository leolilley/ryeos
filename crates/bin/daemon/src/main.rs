use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::serve;
use clap::Parser;
use tokio::net::{TcpListener, UnixListener};

use ryeos_app::callback_token::CallbackCapabilityStore;
use ryeos_app::command_service::CommandService;
use ryeos_app::event_store_service::EventStoreService;
use ryeos_app::event_stream::{ThreadEventHub, DEFAULT_EVENT_STREAM_CAPACITY};
use ryeos_app::identity::NodeIdentity;
use ryeos_app::state::AppState;
use ryeos_app::thread_lifecycle::ThreadLifecycleService;
use ryeos_app::{command_service, event_store_service, thread_lifecycle};
use ryeos_app::{kind_profiles, process, state, state_lock, state_store};
use ryeos_executor::executor as service_executor;
use ryeosd::config::{self, Cli, Config};
use ryeosd::scheduler::db::SchedulerDb;
use ryeosd::{bootstrap, lifecycle_marker, reconcile, scheduler, uds};

fn service_descriptors() -> &'static [ryeos_app::service_registry::ServiceDescriptor] {
    static DESCRIPTORS: once_cell::sync::Lazy<Vec<ryeos_app::service_registry::ServiceDescriptor>> =
        once_cell::sync::Lazy::new(|| {
            ryeos_api::handlers::ALL
                .iter()
                .chain(ryeos_ui::handlers::ALL.iter())
                .copied()
                .collect()
        });
    &DESCRIPTORS
}

fn build_service_registry() -> ryeos_app::service_registry::ServiceRegistry {
    ryeos_api::registry::build_service_registry_from(service_descriptors())
}

fn build_route_table(
    snapshot: &ryeos_app::node_config::NodeConfigSnapshot,
    ui: std::sync::Arc<ryeos_ui::UiState>,
) -> anyhow::Result<ryeos_api::routes::RouteTable> {
    let mut mode_registry =
        ryeos_api::routes::response_modes::ResponseModeRegistry::with_api_builtins_from(
            service_descriptors(),
        );
    let mut extensions = ryeos_api::routes::RouteExtensionRegistry {
        auth: ryeos_api::routes::invokers::AuthInvokerRegistry::with_api_builtins(),
    };

    ryeos_ui::register_extensions(&mut extensions, &mut mode_registry, ui);

    ryeos_api::routes::build_route_table_from_snapshot_with_extensions(
        snapshot,
        &mode_registry,
        &extensions,
    )
    .map_err(|errors| {
        let msgs: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        anyhow::anyhow!(
            "route table build failed at startup ({} error(s)): {}",
            errors.len(),
            msgs.join("; ")
        )
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut config = Config::load(&cli.to_sources())?;
    ryeosd::init_shutdown_channel();

    // Verify operator-owned node initialization before any local repairs
    // or runtime-state writes. `ryeos init` is authoritative for bundle
    // registrations and operator identity/trust artifacts.
    bootstrap::verify_initialized(&config)?;

    // Handle subcommands BEFORE acquiring the daemon state lock or
    // initializing tracing. Subcommands (e.g. `run-service`) manage
    // their own state lock and must not conflict with the daemon's.
    if let Some(ref cmd) = cli.command {
        match cmd {
            config::DaemonCommand::RunService {
                service_ref,
                params,
            } => {
                return run_service_standalone(&config, service_ref, params.as_deref()).await;
            }
        }
    }

    // Acquire the daemon state lock BEFORE unlinking any sockets or
    // writing runtime state. This prevents a second `ryeosd` (or
    // standalone service) from racing in and removing the first
    // daemon's live socket. The lock is automatically released when
    // the process exits (Drop on the file descriptor).
    let state_lock_path = state_lock::default_lock_path(&config.app_root);
    let _state_lock = state_lock::StateLock::acquire(&state_lock_path).context(
        "failed to acquire state lock — is another ryeosd instance or standalone service running?",
    )?;

    // Initialize tracing with file sink only after init-state passes so direct
    // `ryeosd` startup on a fresh system cannot create runtime state.
    ryeos_tracing::init_subscriber(ryeos_tracing::SubscriberConfig::for_daemon_with_file_sink(
        &config.app_root,
    ));

    // Surface how the previous run ended (clean, or an inferred crash from a
    // stale `running` marker), warn on low disk, then mark this run as running.
    let state_dir = config.runtime_state_dir();
    lifecycle_marker::report_previous_exit(&state_dir);
    lifecycle_marker::check_disk_space(&state_dir);
    lifecycle_marker::record_running(&state_dir);

    // Repair only daemon-local artifacts. Missing operator artifacts
    // (user signing key, trust docs) fail with guidance to run
    // `ryeos init` — daemon never substitutes for operator init.
    bootstrap::repair_daemon_local(&config)?;

    // Startup auto-signing is intentionally NOT called here.
    // The daemon start path is fail-closed on unsigned trust-sensitive items.
    // Use `ryeos init` to install bundle registrations before daemon startup.

    tracing::info!("State lock acquired");

    process::remove_stale_socket(&config.uds_path)?;
    ensure_runtime_paths(&config)?;

    // ── Two-phase node-config bootstrap ──
    let (engine, node_config_snapshot) = bootstrap::load_node_config_two_phase(&config)?;

    // Build the service registry early — self-check needs it.
    let services = Arc::new(build_service_registry());

    // Self-check: verify every registered service resolves and is trusted.
    // Every service must resolve, verify, extract an endpoint, AND have a
    // registered handler. Any failure prevents daemon start (fail-closed).
    let catalog_health = {
        let operational_services = service_descriptors();

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
            let canonical = match ryeos_engine::canonical_ref::CanonicalRef::parse(desc.service_ref)
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(service = desc.service_ref, error = %e, "operational service ref parse failed");
                    continue;
                }
            };

            match engine.resolve(&plan_ctx, &canonical) {
                Ok(resolved) => match engine.verify(&plan_ctx, resolved) {
                    Ok(verified) => {
                        match ryeos_api::registry::extract_endpoint(
                            &verified.resolved.metadata.extra,
                        ) {
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
                },
                Err(_) => {
                    tracing::warn!(
                        service = desc.service_ref,
                        "operational service not found in bundle"
                    );
                    missing.push(desc.service_ref);
                }
            }
        }

        if !failed.is_empty() {
            for (svc, error) in &failed {
                tracing::error!(
                    svc,
                    error,
                    "refusing to start: operational service failed verification"
                );
            }
            anyhow::bail!(
                "operational service catalog self-check failed: {} service(s) failed verification",
                failed.len()
            );
        }

        // Missing items are NOT a boot failure: a lean node (e.g. the
        // hosted-node image) deliberately installs a subset of bundles,
        // so descriptors whose service YAML ships in an absent bundle
        // simply don't resolve. The node boots degraded:
        // `service:health/status` reports `missing_services`, and
        // executing one returns the structured `service_not_installed`
        // error. Only verification failures (tampered/unsigned items)
        // remain fail-closed above.
        if missing.is_empty() {
            ryeos_app::state::CatalogHealth {
                status: "ok".into(),
                missing_services: vec![],
            }
        } else {
            for svc in &missing {
                tracing::warn!(
                    svc,
                    "operational service not found in installed bundles; \
                     starting degraded — executing it will return service_not_installed"
                );
            }
            ryeos_app::state::CatalogHealth {
                status: "degraded".into(),
                missing_services: missing.iter().map(|s| s.to_string()).collect(),
            }
        }
    };

    // Build UI state (browser sessions + session bus).
    let ui_state = std::sync::Arc::new(ryeos_ui::UiState::new());
    let ui_state_for_hints = ui_state.clone();

    // Build the route table from the node-config snapshot.
    let route_table = {
        let table = build_route_table(&node_config_snapshot, ui_state.clone())
            .context("route table build failed at startup — check route YAML files")?;
        Arc::new(arc_swap::ArcSwap::from_pointee(table))
    };
    tracing::info!(routes = route_table.load().all.len(), "route table built");

    // Publish the route diagnostics snapshot consumed by
    // `service:system/routes`.
    let route_diagnostics = Arc::new(ryeos_app::route_diagnostics::RouteDiagnostics::new());
    {
        let table = route_table.load();
        route_diagnostics.publish(
            table.fingerprint.clone(),
            ryeos_api::routes::route_diagnostic_entries(&table),
        );
    }

    // Build thread-kind profiles from the loaded kind schemas.
    let kind_profiles = Arc::new(kind_profiles::KindProfileRegistry::build(Some(
        &engine.kinds,
    )));
    let identity = NodeIdentity::load(&config.node_signing_key_path)?;

    let runtime_state_dir = config.runtime_state_dir();
    let runtime_db_path = config.db_path.clone();
    let signer = Arc::new(state_store::NodeIdentitySigner::from_identity(&identity));

    let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();

    let state_store = Arc::new(
        state_store::StateStore::new(
            runtime_state_dir,
            runtime_db_path,
            signer,
            write_barrier.clone(),
        )
        .context("StateStore initialization failed")?,
    );
    tracing::info!("StateStore initialized successfully");

    // Open scheduler DB
    let scheduler_db_path = config
        .app_root
        .join(ryeos_engine::AI_DIR)
        .join("state")
        .join("scheduler.sqlite3");
    let scheduler_db = Arc::new(
        scheduler::db::SchedulerDb::open(&scheduler_db_path)
            .context("SchedulerDb initialization failed")?,
    );
    tracing::info!(path = %scheduler_db_path.display(), "SchedulerDb initialized");

    let events = Arc::new(EventStoreService::new(state_store.clone()));
    // The hub is shared with the lifecycle service so its create/start/
    // finalize/continuation writes publish live (persist-then-publish),
    // and stored as `event_streams` for the SSE endpoints + events.append.
    let event_streams = Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY));
    let threads = Arc::new(ThreadLifecycleService::new(
        state_store.clone(),
        kind_profiles.clone(),
        events.clone(),
        event_streams.clone(),
    )?);
    threads.set_scheduler_db(scheduler_db.clone(), config.app_root.clone());
    let commands = Arc::new(CommandService::new(
        state_store.clone(),
        kind_profiles.clone(),
        events.clone(),
    ));
    let callback_tokens = Arc::new(CallbackCapabilityStore::new());
    let thread_auth = Arc::new(ryeos_app::callback_token::ThreadAuthStore::new());
    // `services` already built above for self-check

    // Operator-secret store. Sealed-envelope (X25519 + XChaCha20-Poly1305)
    // store at `<state>/.ai/state/secrets/store.enc`, decrypted with
    // the vault X25519 secret key generated by bootstrap. Subprocesses
    // inherit only the secrets they declare via `required_secrets`,
    // through the existing `vault_bindings` plumbing in
    // `thread_lifecycle::spawn_item`. Daemon stays vendor-
    // agnostic — `vault.rs` only moves opaque `String -> String` pairs.
    let vault: Arc<dyn ryeos_app::vault::NodeVault> = Arc::new(
        ryeos_app::vault::SealedEnvelopeVault::load(&config.app_root)
            .context("load sealed-envelope vault — did `ryeos init` (or daemon bootstrap) run?")?,
    );

    let command_registry = Arc::new(
        ryeos_runtime::CommandRegistry::from_records(
            &node_config_snapshot.commands,
            &node_config_snapshot.command_registration_policy.policy,
        )
        .context("failed to build command registry from node-config records")?,
    );

    let authorizer = Arc::new(ryeos_runtime::authorizer::Authorizer::new());

    // Bind the TCP listener BEFORE constructing AppState so the
    // status endpoint reports the actual bound address (when the
    // operator passes `:0`, the kernel assigns an ephemeral port).
    // Without this hoist, `ryeos status` would echo back the
    // requested `127.0.0.1:0` instead of the real listener address.
    let tcp_listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;
    let actual_bind = tcp_listener
        .local_addr()
        .with_context(|| format!("failed to read local_addr after binding {}", config.bind))?;
    config.bind = actual_bind;

    let mut app_state = AppState {
        config: Arc::new(config.clone()),
        state_store,
        engine: engine.clone(),
        engine_cache: ryeos_app::engine_cache::EngineCache::new(
            ryeos_app::engine_cache::EngineCacheConfig::default(),
        ),
        identity: Arc::new(identity),
        threads,
        events,
        event_streams,
        commands,
        callback_tokens,
        thread_auth,
        extensions: {
            let mut ext = ryeos_app::extension_state::ExtensionState::new();
            ext.insert(ui_state);
            ext.insert(route_diagnostics);
            Arc::new(ext)
        },
        write_barrier: Arc::new(write_barrier),
        started_at: Instant::now(),
        started_at_iso: lillux::time::iso8601_now(),
        catalog_health,
        services,
        service_descriptors: service_descriptors(),
        node_config: node_config_snapshot,
        vault,
        command_registry,
        authorizer,
        scheduler_db,
        scheduler_runtime_gate: Arc::new(tokio::sync::RwLock::new(())),
        scheduler_reload_tx: None,
        ignore_matcher: Arc::new(
            ryeos_app::ignore::load_from_app_root(&config.app_root)
                .context("load ingest ignore config — did `ryeos init` run?")?,
        ),
        vault_fingerprint: {
            let vault_pk_path = config
                .app_root
                .join(ryeos_engine::AI_DIR)
                .join("node/vault/public_key.pem");
            if vault_pk_path.exists() {
                lillux::vault::read_public_key(&vault_pk_path)
                    .ok()
                    .map(|pk| pk.fingerprint())
            } else {
                None
            }
        },
    };
    let webhook_dedupe = Arc::new(ryeos_api::routes::webhook_dedupe::WebhookDedupeStore::new());

    // Session hints: thread lifecycle events fan out to every live UI
    // session as transient `thread.hint` notices (never persisted —
    // hints say "look", the braid says what happened).
    {
        let hub = app_state.event_streams.clone();
        let ui = ui_state_for_hints.clone();
        tokio::spawn(async move {
            let mut rx = hub.subscribe_all();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if !ryeos_api::routes::invokers::stream_helpers::is_lifecycle_hint(
                            &event.event_type,
                        ) {
                            continue;
                        }
                        for session_id in ui.browser_sessions.session_ids() {
                            ui.session_bus.publish(
                                &session_id,
                                "thread.hint",
                                serde_json::json!({
                                    "kind": "thread",
                                    "thread_id": event.thread_id,
                                    "chain_root_id": event.chain_root_id,
                                    "event_type": event.event_type,
                                }),
                            );
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Reconcile threads from the previous run BEFORE binding listeners,
    // but DO NOT dispatch the resume intents yet — a resumed subprocess
    // making its first daemon callback before the UDS / HTTP server is
    // bound would fail. We collect intents here and dispatch them
    // below, after the listeners are accepting connections.
    let resume_intents = reconcile::reconcile(&app_state).await?;

    // Scheduler reload channel — must be created BEFORE the router is built
    // so that HTTP handler clones of AppState carry the sender.
    let (scheduler_reload_tx, scheduler_reload_rx) =
        tokio::sync::mpsc::channel::<scheduler::ReloadSignal>(16);
    app_state.scheduler_reload_tx = Some(scheduler_reload_tx);

    // Auth is per-route via the dispatcher's auth_invoker chain.
    // No global middleware layer — each route declares its own auth
    // policy (auth: "none" for public, auth: "ryeos_signed" for
    // authenticated). The fallback dispatcher handles everything.
    let api_state = ryeos_api::ApiState {
        app: Arc::new(app_state.clone()),
        route_table,
        webhook_dedupe,
    };
    let app = ryeos_api::build_router(api_state);
    // `tcp_listener` and `actual_bind` were created above before
    // AppState construction so the status endpoint reports the real
    // bound address.
    let uds_listener = UnixListener::bind(&config.uds_path)
        .with_context(|| format!("failed to bind {}", config.uds_path.display()))?;

    std::env::set_var("RYEOSD_SOCKET_PATH", &config.uds_path);
    std::env::set_var("RYEOSD_URL", format!("http://{}", actual_bind));

    // Write daemon.json so tools can discover the daemon.
    // This is the discovery contract — fail if we can't write it.
    let daemon_info = ryeos_node::DaemonMetadata {
        pid: Some(std::process::id()),
        uds_path: Some(config.uds_path.clone()),
        bind: Some(actual_bind.to_string()),
        started_at: Some(lillux::time::iso8601_now()),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        app_root: config.app_root.clone(),
    };
    let daemon_json_path = config.app_root.join("daemon.json");
    daemon_info.write(&config.app_root).with_context(|| {
        format!(
            "failed to write daemon.json at {} — tools cannot discover the daemon without it",
            daemon_json_path.display()
        )
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.uds_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to set socket permissions on {}",
                    config.uds_path.display()
                )
            })?;
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
            let params =
                match ryeos_executor::execution::runner::execution_params_from_resume_context(
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
                            &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
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
            if let Err(err) = ryeos_executor::execution::runner::run_existing_detached(
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

    // ── Scheduler reconciliation + timer start ──
    let scheduler_ctx = Arc::new(ryeosd::scheduler_impl::AppSchedulerContext(Arc::new(
        app_state.clone(),
    )));
    let scheduler_intents = {
        let _scheduler_reconcile_guard =
            app_state.scheduler_runtime_gate.clone().write_owned().await;
        scheduler::reconcile::reconcile(&scheduler_ctx).await?
    };
    for intent in scheduler_intents {
        let st = scheduler_ctx.clone();
        tokio::spawn(async move {
            scheduler::timer::dispatch_recovery_fire(st, intent).await;
        });
    }

    tokio::spawn(scheduler::timer::run(scheduler_ctx, scheduler_reload_rx));
    tracing::info!("scheduler: timer loop started");

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

    // Record a clean, handled shutdown so the next startup can distinguish it
    // from a crash/SIGKILL (which leaves the `running` marker behind).
    lifecycle_marker::record_exit(&state_dir, "signal");

    // Drain running threads on shutdown
    drain_running_threads(&app_state);

    // Cleanup daemon.json
    let _ = std::fs::remove_file(&daemon_json_path);

    if config.uds_path.exists() {
        let _ = std::fs::remove_file(&config.uds_path);
    }

    Ok(())
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

    for thread in &threads {
        if let Some(pgid) = thread.runtime.pgid {
            let action = process::resolve_shutdown_action(
                thread
                    .runtime
                    .launch_metadata
                    .as_ref()
                    .and_then(|lm| lm.cancellation_mode),
            );
            tracing::info!(
                pgid,
                thread_id = %thread.thread_id,
                action = ?action,
                "killing process group"
            );
            let result = process::kill_by_action(pgid, action);
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

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let lifecycle_shutdown = async {
        if let Some(mut rx) = ryeosd::subscribe_shutdown() {
            let _ = rx.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
        _ = lifecycle_shutdown => {}
    }
}

fn ensure_runtime_paths(config: &Config) -> Result<()> {
    if let Some(parent) = config.db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db parent {}", parent.display()))?;
    }
    if let Some(parent) = config.uds_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create uds parent {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).with_context(
                || {
                    format!(
                        "failed to set runtime dir permissions on {}",
                        parent.display()
                    )
                },
            )?;
        }
    }
    Ok(())
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
    use ryeos_executor::executor::{ExecutionContext, ExecutionMode};

    // Verify initialization
    bootstrap::verify_initialized(config)?;

    // Acquire the state lock immediately after init verification, BEFORE
    // any expensive bootstrap work. Otherwise standalone mode would
    // perform identity/engine/node-config loads while a competing
    // daemon held the lock, only to fail late.
    let _state_lock =
        state_lock::StateLock::acquire(&state_lock::default_lock_path(&config.app_root))
            .context("failed to acquire state lock — is the daemon running?")?;

    // Two-phase node-config bootstrap (same as daemon-start path)
    let (engine, node_config_snapshot) = bootstrap::load_node_config_two_phase(config)?;

    let kind_profiles = Arc::new(kind_profiles::KindProfileRegistry::build(Some(
        &engine.kinds,
    )));
    let identity = NodeIdentity::load(&config.node_signing_key_path)?;

    let services = Arc::new(build_service_registry());

    let runtime_state_dir = config.runtime_state_dir();
    let runtime_db_path = config.db_path.clone();
    let signer = Arc::new(state_store::NodeIdentitySigner::from_identity(&identity));
    let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
    let state_store = Arc::new(state_store::StateStore::new(
        runtime_state_dir,
        runtime_db_path,
        signer,
        write_barrier.clone(),
    )?);

    let events = Arc::new(event_store_service::EventStoreService::new(
        state_store.clone(),
    ));
    let event_streams = Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY));
    let threads = Arc::new(thread_lifecycle::ThreadLifecycleService::new(
        state_store.clone(),
        kind_profiles.clone(),
        events.clone(),
        event_streams.clone(),
    )?);
    let commands = Arc::new(command_service::CommandService::new(
        state_store.clone(),
        kind_profiles,
        events.clone(),
    ));

    let standalone_command_registry = Arc::new(
        ryeos_runtime::CommandRegistry::from_records(
            &node_config_snapshot.commands,
            &node_config_snapshot.command_registration_policy.policy,
        )
        .context("failed to build command registry from node-config records")?,
    );

    let standalone_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new());

    let app_state = state::AppState {
        config: Arc::new(config.clone()),
        state_store,
        engine: engine.clone(),
        engine_cache: ryeos_app::engine_cache::EngineCache::new(
            ryeos_app::engine_cache::EngineCacheConfig::default(),
        ),
        identity: Arc::new(identity),
        threads,
        events,
        event_streams,
        commands,
        callback_tokens: Arc::new(ryeos_app::callback_token::CallbackCapabilityStore::new()),
        thread_auth: Arc::new(ryeos_app::callback_token::ThreadAuthStore::new()),
        extensions: Arc::new(ryeos_app::extension_state::ExtensionState::new()),
        write_barrier: Arc::new(write_barrier),
        started_at: Instant::now(),
        started_at_iso: lillux::time::iso8601_now(),
        catalog_health: state::CatalogHealth {
            status: "standalone".into(),
            missing_services: vec![],
        },
        services,
        service_descriptors: service_descriptors(),
        node_config: node_config_snapshot.clone(),
        vault: Arc::new(
            ryeos_app::vault::SealedEnvelopeVault::load(&config.app_root)
                .context("load sealed-envelope vault — did `ryeos init` run?")?,
        ),
        command_registry: standalone_command_registry,
        authorizer: standalone_auth,
        scheduler_db: Arc::new(SchedulerDb::new_in_memory().context("scheduler in-memory db")?),
        scheduler_runtime_gate: Arc::new(tokio::sync::RwLock::new(())),
        scheduler_reload_tx: None,
        ignore_matcher: Arc::new(ryeos_app::ignore::matcher_from_builtins()),
        vault_fingerprint: None,
    };

    let params: serde_json::Value = match params_json {
        Some(json_str) => {
            serde_json::from_str(json_str).with_context(|| "parse --params as JSON")?
        }
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
        requested_call: None,
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
    use ryeos_app::process::{resolve_shutdown_action, ShutdownAction};
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
