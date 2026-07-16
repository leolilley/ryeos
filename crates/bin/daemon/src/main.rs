use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
use ryeos_node::lifecycle_marker;
use ryeosd::config::{self, Cli, Config};
use ryeosd::scheduler::db::SchedulerDb;
use ryeosd::{bootstrap, reconcile, scheduler, uds};

mod maintenance_schedule;
mod startup;

const STARTUP_FAILURE_REPORTING_GRACE: Duration = Duration::from_secs(30);

struct LifecycleExitGuard {
    state_dir: std::path::PathBuf,
    recorded: bool,
}

impl LifecycleExitGuard {
    fn new(state_dir: std::path::PathBuf) -> Self {
        Self {
            state_dir,
            recorded: false,
        }
    }

    fn record(&mut self, reason: &str) {
        lifecycle_marker::record_exit(&self.state_dir, reason);
        self.recorded = true;
    }
}

impl Drop for LifecycleExitGuard {
    fn drop(&mut self) {
        if !self.recorded {
            lifecycle_marker::record_exit(&self.state_dir, "startup_failed");
        }
    }
}

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

fn prospective_node_config_validator(
    ui: Arc<ryeos_ui::UiState>,
) -> Arc<ryeos_app::prospective_admission::ProspectiveNodeConfigValidator> {
    Arc::new(
        ryeos_app::prospective_admission::ProspectiveNodeConfigValidator::new(move |snapshot| {
            build_route_table(snapshot, ui.clone())
                .map(|_| ())
                .context("prospective route-table compilation failed")
        }),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    // Capture process start before any configuration, verification, or state
    // opening so every lifecycle surface reports the same wall/monotonic origin.
    let process_started = Instant::now();
    let process_started_at = lillux::time::iso8601_now();
    let cli = Cli::parse();

    if let Some(config::DaemonCommand::BuildInfo { revision, json }) = &cli.command {
        let build = ryeos_app::build_info::get_for_version(env!("CARGO_PKG_VERSION"));
        if *revision {
            println!("{}", build.revision);
        } else if *json {
            println!("{}", serde_json::to_string(&build)?);
        } else {
            println!("version: {}", build.version);
            println!("revision: {}", build.revision);
            println!("build_date: {}", build.build_date);
        }
        return Ok(());
    }

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
            config::DaemonCommand::BuildInfo { .. } => unreachable!("handled before config load"),
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
    let mut lifecycle_exit = LifecycleExitGuard::new(state_dir.clone());

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
    let daemon_json_path = config.app_root.join("daemon.json");
    let _ = std::fs::remove_file(&daemon_json_path);

    // Bind and start both stable outer transports before any projection work.
    // These listeners expose only lifecycle/liveness until the callback-capable
    // application is release-published by the startup coordinator.
    let tcp_listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;
    let actual_bind = tcp_listener
        .local_addr()
        .with_context(|| format!("failed to read local_addr after binding {}", config.bind))?;
    config.bind = actual_bind;
    let uds_listener = UnixListener::bind(&config.uds_path)
        .with_context(|| format!("failed to bind {}", config.uds_path.display()))?;
    let _discovery_cleanup =
        startup::DiscoveryCleanup::new(config.uds_path.clone(), daemon_json_path.clone())?;

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

    std::env::set_var("RYEOSD_SOCKET_PATH", &config.uds_path);
    std::env::set_var("RYEOSD_URL", format!("http://{}", actual_bind));

    // daemon.json is an early discovery hint.  Live lifecycle truth always
    // comes from the UDS, and started_at is process start (never ready time).
    let build = ryeos_app::build_info::get();
    let daemon_info = ryeos_node::DaemonMetadata {
        pid: Some(std::process::id()),
        uds_path: Some(config.uds_path.clone()),
        bind: Some(actual_bind.to_string()),
        started_at: Some(process_started_at.clone()),
        version: Some(build.version.to_string()),
        revision: Some(build.revision.to_string()),
        build_date: Some(build.build_date.to_string()),
        app_root: config.app_root.clone(),
    };
    daemon_info.write(&config.app_root).with_context(|| {
        format!(
            "failed to write daemon.json at {} — tools cannot discover the daemon without it",
            daemon_json_path.display()
        )
    })?;

    let lifecycle_identity = ryeos_node::LifecycleIdentity {
        pid: std::process::id(),
        bind: actual_bind.to_string(),
        uds_path: config.uds_path.clone(),
        app_root: config.app_root.clone(),
        started_at: process_started_at.clone(),
        version: build.version.to_string(),
        revision: Some(build.revision.to_string()),
        build_date: Some(build.build_date.to_string()),
    };
    let startup = startup::StartupCoordinator::bootstrap(lifecycle_identity, process_started)?;
    let recovery_execution_release = ryeos_app::recovery_execution_gate::arm();
    let http_state = startup.http_state();
    let mut uds_task = tokio::spawn(uds::server::serve_dynamic(
        uds_listener,
        startup.uds_state(),
    ));
    let shutdown = shutdown_signal();
    let mut http_task = tokio::spawn(async move {
        serve(tcp_listener, startup::build_outer_router(http_state))
            .with_graceful_shutdown(shutdown)
            .await
    });
    let progress_task = tokio::spawn(startup::progress_ticker(startup.clone()));
    let shutdown_drain_state = Arc::new(Mutex::new(None::<AppState>));
    // Give both spawned servers a scheduling turn before any synchronous
    // bootstrap work resumes; listener bind alone is not the accept-loop
    // publication boundary.
    tokio::task::yield_now().await;

    let daemon_result: (Result<()>, bool, bool) = {
        let daemon_work = async {
            startup.phase(
                ryeos_node::StartupPhase::Bootstrapping,
                "loading verified node configuration",
            )?;

            // Resolve every interrupted bundle tree/registration transaction before
            // the bootstrap loader consumes installed bundle registrations.
            let identity = NodeIdentity::load(&config.node_signing_key_path)?;
            let repaired_bundles =
                ryeos_app::bundle_transaction::reconcile_all_bundle_transactions(
                    &config.app_root,
                    identity.signing_key(),
                )?;
            if !repaired_bundles.is_empty() {
                tracing::warn!(
                    bundles = ?repaired_bundles,
                    "reconciled interrupted bundle transactions before registry loading"
                );
            }

            // Resolve the node sandbox exactly once. Engine-owned handlers and
            // ordinary execution share this immutable policy snapshot.
            let sandbox = Arc::new(
                ryeos_engine::sandbox::SandboxRuntime::load_for_daemon(
                    &config.app_root,
                    &config.uds_path,
                )
                .context("load node sandbox policy")?,
            );

            // ── Two-phase node-config bootstrap ──
            let (engine, node_config_snapshot, sandbox) =
                bootstrap::load_node_config_two_phase(&config, Arc::clone(&sandbox))?;
            let node_history_policy = {
                let roots = engine.resolution_roots(Some(config.app_root.clone()));
                let parsers = engine.effective_parser_dispatcher(Some(&config.app_root))?;
                let context = ryeos_engine::config_loading::ConfigLoadContext {
                    roots: &roots,
                    parsers: &parsers,
                    kinds: &engine.kinds,
                    trust_store: &engine.node_trust_store,
                };
                Arc::new(ryeos_engine::history_policy::load_node_thread_history_policy(&context)?)
            };

            // Build the service registry early — self-check needs it.
            let services = Arc::new(build_service_registry());

            // Self-check: verify every registered service resolves and is trusted.
            // Every service must resolve, verify, extract an endpoint, AND have a
            // registered handler. Any failure prevents daemon start (fail-closed).
            let catalog_health = {
                let operational_services = service_descriptors();
                let node_principal = identity.principal_id();

                let plan_ctx = ryeos_engine::contracts::PlanContext {
                    requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
                        ryeos_engine::contracts::Principal {
                            fingerprint: node_principal,
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
                    let canonical = match ryeos_engine::canonical_ref::CanonicalRef::parse(
                        desc.service_ref,
                    ) {
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
            let prospective_node_config_validator =
                prospective_node_config_validator(ui_state.clone());

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
            let runtime_state_dir = config.runtime_state_dir();
            let runtime_db_path = config.db_path.clone();
            let state_app_root = config.app_root.clone();
            let signer = Arc::new(state_store::NodeIdentitySigner::from_identity(&identity));
            let mut head_trust = ryeos_state::refs::TrustStore::new();
            head_trust.insert(
                identity.fingerprint().to_string(),
                *identity.verifying_key(),
            );
            let head_trust = Arc::new(head_trust);

            let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();

            // Execution admission limits must be armed before projection recovery
            // or any later runtime recovery action is classified or enqueued.
            let node_fanout = load_node_max_live_fanout(&engine, &config.app_root);
            ryeos_executor::execution::launch::arm_global_live_fanout_limit(node_fanout);
            if let Some(n) = node_fanout {
                tracing::info!(max_live_fanout = n, "node execution limits armed");
            }

            startup.phase(
                ryeos_node::StartupPhase::OpeningProjection,
                "opening thread projection",
            )?;
            let state_open_observer: Arc<dyn ryeos_state::ProjectionRecoveryObserver> =
                Arc::new(startup.clone());
            let state_store = tokio::task::spawn_blocking(move || {
                state_store::StateStore::new_with_head_trust_and_recovery_observer(
                    state_app_root,
                    runtime_state_dir,
                    runtime_db_path,
                    signer,
                    write_barrier.clone(),
                    head_trust,
                    state_open_observer,
                )
                .map(|store| (store, write_barrier))
            })
            .await
            .context("StateStore initialization task failed")?
            .context("StateStore initialization failed")?;
            let (state_store, write_barrier) = state_store;
            let state_store = Arc::new(state_store);
            tracing::info!("StateStore initialized successfully");

            // StateDb::open owns the one deterministic pending-transition replay
            // and reports its ReplayingHeadChanges progress through the observer.
            // A Remove whose head is already absent deliberately remains pending:
            // its runtime cleanup needs the scheduler pin index opened below.

            if ryeosd::shutdown_requested() {
                tracing::info!("startup shutdown requested after projection open");
                return Ok(());
            }

            // Scheduler fire projection recovery is a separate, potentially
            // long startup domain. It must complete before terminal-history
            // removal consults scheduler pins and before active-thread recovery
            // can observe scheduler rows.
            startup.phase(
                ryeos_node::StartupPhase::RecoveringSchedulerProjection,
                "recovering scheduler fire projection",
            )?;
            let scheduler_db_path = config
                .app_root
                .join(ryeos_engine::AI_DIR)
                .join("state")
                .join("scheduler.sqlite3");
            let scheduler_db = Arc::new(
                scheduler::db::SchedulerDb::open(&scheduler_db_path)
                    .context("SchedulerDb initialization failed")?,
            );
            let scheduler_runtime_gate = Arc::new(tokio::sync::RwLock::new(()));
            {
                // Scheduler fire rows are consulted by terminal-chain recovery
                // before the timer's later reconciliation phase. Recover the
                // durable outbox and any invalid fire projection now, under the
                // same exclusive gate used by every later journal mutation, so
                // pin inspection can never observe a partial full replay.
                let _scheduler_startup_guard = scheduler_runtime_gate.clone().write_owned().await;
                let runtime_state_dir = config.app_root.join(ryeos_engine::AI_DIR).join("state");
                scheduler_db
                    .drain_fire_outbox(&runtime_state_dir)
                    .context("recover scheduler fire outbox before runtime cleanup")?;
                if !scheduler_db.fire_projection_is_current()? {
                    scheduler::projection::rebuild_fires_from_dir(
                        &runtime_state_dir.join("schedules"),
                        &scheduler_db,
                    )
                    .context("rebuild scheduler fire projection before runtime cleanup")?;
                }
            }
            tracing::info!(path = %scheduler_db_path.display(), "SchedulerDb initialized");

            let events = Arc::new(EventStoreService::new(state_store.clone()));
            // The hub is shared with the lifecycle service so its create/start/
            // finalize/continuation writes publish live (persist-then-publish),
            // and stored as `event_streams` for the SSE endpoints + events.append.
            let event_streams = Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY));
            let threads = Arc::new(ThreadLifecycleService::new(
                state_store.clone(),
                engine.clone(),
                kind_profiles.clone(),
                events.clone(),
                event_streams.clone(),
            )?);
            threads.set_scheduler_db(
                scheduler_db.clone(),
                scheduler_runtime_gate.clone(),
                config.app_root.clone(),
            );
            // Operator live-input queue, shared between the `threads.input` enqueue
            // path (via AppState) and lifecycle finalization (closes a thread's entry).
            let live_input = Arc::new(ryeos_app::live_input_queue::LiveInputQueue::new());
            threads.set_live_input_queue(live_input.clone());
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
                ryeos_app::vault::SealedEnvelopeVault::load(&config.app_root).context(
                    "load sealed-envelope vault — did `ryeos init` (or daemon bootstrap) run?",
                )?,
            );

            let command_registry = Arc::new(
                ryeos_runtime::CommandRegistry::from_records(
                    &node_config_snapshot.commands,
                    &node_config_snapshot.command_registration_policy.policy,
                )
                .context("failed to build command registry from node-config records")?,
            );

            let authorizer = Arc::new(ryeos_runtime::authorizer::Authorizer::new());

            let mut app_state = AppState {
                config: Arc::new(config.clone()),
                sandbox,
                state_store,
                engine: engine.clone(),
                engine_cache: ryeos_app::engine_cache::EngineCache::new(
                    ryeos_app::engine_cache::EngineCacheConfig::default(),
                ),
                identity: Arc::new(identity),
                threads,
                live_input,
                events,
                event_streams,
                commands,
                callback_tokens,
                thread_auth,
                extensions: {
                    let mut ext = ryeos_app::extension_state::ExtensionState::new();
                    ext.insert(ui_state);
                    ext.insert(route_diagnostics);
                    ext.insert(prospective_node_config_validator);
                    Arc::new(ext)
                },
                write_barrier: Arc::new(write_barrier),
                started_at: process_started,
                started_at_iso: process_started_at.clone(),
                catalog_health,
                services,
                service_descriptors: service_descriptors(),
                node_config: node_config_snapshot,
                node_history_policy,
                vault,
                command_registry,
                authorizer,
                scheduler_db,
                scheduler_runtime_gate,
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
            let admission_store = app_state.state_store.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                if let Err(error) = admission_store.close_process_attachment_admission() {
                    tracing::error!(
                        error = %error,
                        "failed to close process attachment admission from shutdown signal"
                    );
                }
            });
            *shutdown_drain_state
                .lock()
                .expect("shutdown drain state mutex poisoned") = Some(app_state.clone());

            // A durable Remove journal record may survive after its signed head was
            // already unlinked. Finish its runtime/projection cleanup before any
            // execution reconciliation can recreate operational pins, and never
            // publish Ready while a head transition remains unresolved.
            startup.phase(
                ryeos_node::StartupPhase::ReplayingHeadChanges,
                "finishing interrupted terminal-history removals",
            )?;
            let _startup_retention_guard =
                app_state.scheduler_runtime_gate.clone().write_owned().await;
            let cleanup_store = app_state.state_store.clone();
            let cleanup_scheduler = app_state.scheduler_db.clone();
            let recovered_removals = tokio::task::spawn_blocking(move || {
                cleanup_store.recover_pending_terminal_chain_removals(
                    &lillux::time::iso8601_now(),
                    false,
                    |thread_ids| {
                        let mut pins = 0_u64;
                        for thread_id in thread_ids {
                            if cleanup_scheduler.find_fire_by_thread(thread_id)?.is_some() {
                                pins = pins.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("scheduler recovery pin count overflow")
                                })?;
                            }
                        }
                        Ok(pins)
                    },
                )
            })
            .await
            .context("interrupted terminal-history cleanup task failed")??;
            tracing::info!(
                recovered = recovered_removals.pending_retirements_recovered,
                "finished interrupted terminal-history removals"
            );
            let pending_heads = app_state.state_store.pending_head_transition_status()?;
            if pending_heads.pending != 0 {
                anyhow::bail!(
                    "startup recovery left {} unresolved chain-head transition(s)",
                    pending_heads.pending
                );
            }
            drop(_startup_retention_guard);
            let webhook_dedupe =
                Arc::new(ryeos_api::routes::webhook_dedupe::WebhookDedupeStore::new());

            // Reconcile active execution state while the stable listeners continue to
            // serve lifecycle status. Recovery work is collected before application
            // publication and dispatched only after callback state is available.
            // Node-scoped execution limits ride the SAME signed, layered config
            // family as every other execution limit: `config/execution/execution.yaml`,
            // `node:` section. Bundle layers carry defaults; the node's own tree
            // (`<app_root>/.ai/config/...`) is the top overlay — the operator's
            // surface, which no project layer can touch (per-launch policy reads use
            // the project as overlay instead and never read `node:`).
            startup.phase(
                ryeos_node::StartupPhase::ReconcilingThreads,
                "reconciling active thread execution state",
            )?;
            let active_reconcile = reconcile::reconcile_active_threads(&app_state).await?;
            startup.progress(|snapshot| {
                snapshot.recovery_threads = Some(active_reconcile.active_thread_ids.len() as u64);
            })?;
            let recovery_thread_targets = active_reconcile.active_thread_ids;
            let resume_intents = active_reconcile.resume_intents;
            if ryeosd::shutdown_requested() {
                tracing::info!("startup shutdown requested after thread reconciliation");
                return Ok(());
            }
            // Follow reconcile actions collected here, dispatched post-listener too: a
            // resumed parent's (or relaunched child's) first callback must not precede a
            // bound listener.
            // LOAD-BEARING ORDER: settle cancellation tombstones before follow
            // reconciliation can classify an admitted-but-unlaunched child for relaunch.
            ryeos_app::cascade::repair_cancelled_window_members(&app_state)?;
            startup.phase(
                ryeos_node::StartupPhase::ReconcilingFollow,
                "reconciling suspended follow execution state",
            )?;
            let follow_actions = reconcile::reconcile_follow(&app_state)?;
            if ryeosd::shutdown_requested() {
                tracing::info!("startup shutdown requested after follow reconciliation");
                return Ok(());
            }

            // Reconciliation writes are authoritative-first. If any projection apply
            // requested repair, drain that bounded pending journal before publishing
            // application readiness; Ready never fronts a known-stale projection.
            let projection_health = app_state.state_store.projection_health();
            if !projection_health.is_current() {
                startup.phase(
                    ryeos_node::StartupPhase::ReplayingHeadChanges,
                    "repairing projection changes produced during reconciliation",
                )?;
                if let Some(generation) = projection_health.begin_repair() {
                    let repair_store = app_state.state_store.clone();
                    let repaired = tokio::task::spawn_blocking(move || {
                        repair_store.repair_thread_projection()
                    })
                    .await
                    .context("startup projection repair task failed")?;
                    projection_health.finish_repair(generation, &repaired);
                    repaired.context("startup projection repair failed")?;
                }
                if !projection_health.is_current() {
                    anyhow::bail!("thread projection remained stale after startup repair");
                }
            }
            if ryeosd::shutdown_requested() {
                tracing::info!("startup shutdown requested after projection repair");
                return Ok(());
            }

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

            // Reconcile scheduler truth while holding the runtime gate through
            // application publication and recovery enqueue. Timer/recovery tasks may be
            // spawned below, but cannot claim a fire until Ready releases this guard.
            startup.phase(
                ryeos_node::StartupPhase::ReconcilingScheduler,
                "reconciling scheduler state",
            )?;
            maintenance_schedule::ensure_maintenance_schedule(&app_state)
                .context("reconcile bundle-authored maintenance schedules")?;
            let scheduler_ctx = Arc::new(ryeosd::scheduler_impl::AppSchedulerContext(Arc::new(
                app_state.clone(),
            )));
            let scheduler_reconcile_guard =
                app_state.scheduler_runtime_gate.clone().write_owned().await;
            let scheduler_intents = scheduler::reconcile::reconcile(&scheduler_ctx).await?;
            if ryeosd::shutdown_requested() {
                tracing::info!("startup shutdown requested after scheduler reconciliation");
                return Ok(());
            }

            // Publish callback-capable application state while external admission is
            // still closed. UDS runtime callbacks from recovered work can now succeed;
            // ordinary HTTP/content requests continue to receive node_starting.
            let callback_app = api_state.app.clone();
            startup.publish_application(callback_app, api_state)?;

            // Claim and enqueue every classified thread recovery action. The executor
            // preparation APIs persist SQLite launch ownership before detaching the
            // terminal runtime future; this loop therefore waits for durable admission,
            // never for recovered work to finish.
            let mut recovery_thread_targets = recovery_thread_targets;
            for action in &follow_actions {
                match action {
                    reconcile::FollowReconcileAction::Resume { follow_key } => {
                        let waiter = app_state
                            .state_store
                            .get_follow_waiter_by_key(follow_key)?
                            .ok_or_else(|| {
                                anyhow::anyhow!("follow recovery waiter disappeared: {follow_key}")
                            })?;
                        let successor_id = waiter.parent_successor_thread_id.ok_or_else(|| {
                            anyhow::anyhow!(
                                "follow recovery waiter {follow_key} has no parent successor"
                            )
                        })?;
                        recovery_thread_targets.insert(successor_id);
                    }
                    reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } => {
                        recovery_thread_targets.insert(child_thread_id.clone());
                    }
                }
            }
            dispatch_resume_intents(&app_state, resume_intents).await?;

            // Drive the follow reconcile actions collected above. Each launch is claim-
            // guarded, so a duplicate with a live path is a benign `Skipped`. `Resume`
            // wakes a suspended parent; `RelaunchChild` re-fires a child stranded in the
            // pre-launch window.
            // Launch-window recovery: release slots whose member chain settled while
            // no kick could land (crash window), then admit and launch queued
            // members. Post-listener for the same reason as the intents above —
            // launched runtimes call back immediately.
            for (child_thread_id, outcome) in
                ryeos_executor::execution::launch::prepare_launch_window_recovery(&app_state)
                    .context("durably classify launch-window recovery")?
            {
                recovery_thread_targets.insert(child_thread_id.clone());
                tracing::info!(
                    child_thread_id,
                    ?outcome,
                    "launch-window recovery classified"
                );
            }

            prepare_follow_recovery_actions(&app_state, follow_actions)?;

            if !projection_health.is_current() {
                if let Some(generation) = projection_health.begin_repair() {
                    let repair_store = app_state.state_store.clone();
                    let repaired = tokio::task::spawn_blocking(move || {
                        repair_store.repair_thread_projection()
                    })
                    .await
                    .context("recovery projection repair task failed")?;
                    projection_health.finish_repair(generation, &repaired);
                    repaired.context("recovery projection repair failed")?;
                }
            }
            let pre_scheduler_projection = projection_health.snapshot();
            if pre_scheduler_projection.status
                != ryeos_app::projection_health::ThreadProjectionState::Current
            {
                anyhow::bail!(
                    "thread projection became {:?} while startup recovery was enqueued",
                    pre_scheduler_projection.status
                );
            }

            // Persist/claim every recovered fire while the startup gate is still held.
            // `dispatch_fire` detaches execution after its durable scheduler record, so
            // this waits for enqueue—not for the scheduled work to finish.
            for intent in scheduler_intents {
                let fire_id = intent.fire_id.clone();
                let outcome = scheduler::timer::dispatch_recovery_fire_under_startup_guard(
                    scheduler_ctx.clone(),
                    intent,
                    &scheduler_reconcile_guard,
                )
                .await
                .with_context(|| format!("durably enqueue recovered scheduler fire {fire_id}"))?;
                tracing::info!(fire_id, ?outcome, "scheduler recovery fire classified");
            }
            // Detached recovery work may finish or fail quickly after preparation. Do
            // not publish Ready unless each original target is still owned, has attached
            // a live process, reached terminal, or remains in a durable follow/window
            // retry state. This turns task-side logging into an observable startup
            // classification boundary.
            ensure_recovery_targets_classified(&app_state, &recovery_thread_targets)?;

            // Scheduler recovery can itself create/project thread state. Take the
            // readiness snapshot only after every recovered fire has crossed its durable
            // dispatch boundary, repairing any journal work produced along the way.
            if !projection_health.is_current() {
                if let Some(generation) = projection_health.begin_repair() {
                    let repair_store = app_state.state_store.clone();
                    let repaired = tokio::task::spawn_blocking(move || {
                        repair_store.repair_thread_projection()
                    })
                    .await
                    .context("final recovery projection repair task failed")?;
                    projection_health.finish_repair(generation, &repaired);
                    repaired.context("final recovery projection repair failed")?;
                }
            }
            let projection_snapshot = projection_health.snapshot();
            if projection_snapshot.status
                != ryeos_app::projection_health::ThreadProjectionState::Current
            {
                anyhow::bail!(
                    "thread projection became {:?} after scheduler recovery enqueue",
                    projection_snapshot.status
                );
            }
            let projection = serde_json::to_value(projection_snapshot)
                .context("serialize final projection readiness snapshot")?;

            let recovery_release_for_ready = recovery_execution_release.clone();
            startup.ready(projection, move || {
                recovery_release_for_ready.open();
                drop(scheduler_reconcile_guard);
            })?;

            // The ordinary timer does not exist until Ready is published, so
            // no timer fire can race the final internal-gate release boundary.
            let scheduler_task = tokio::spawn(scheduler::timer::run(
                scheduler_ctx,
                scheduler_reload_rx,
                shutdown_signal(),
            ));
            tracing::info!("scheduler: timer loop started after readiness publication");

            supervise_background_tasks(app_state, ui_state_for_hints, scheduler_task).await
        };
        tokio::pin!(daemon_work);
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum FirstExit {
            DaemonWork,
            HttpListener,
            UdsListener,
        }

        let (first_exit, mut result) = tokio::select! {
            result = &mut daemon_work => (FirstExit::DaemonWork, result),
            result = &mut http_task => (FirstExit::HttpListener, classify_http_listener_exit(result)),
            result = &mut uds_task => (FirstExit::UdsListener, classify_uds_listener_exit(result)),
        };
        let shutdown_was_requested = ryeosd::shutdown_requested();
        let first_exit_was_clean = result.is_ok();

        // A listener is part of the daemon, not a detached convenience. Losing
        // either one closes admission and signals every producer before the
        // still-owned startup/runtime future is awaited to its boundary.
        if first_exit != FirstExit::DaemonWork {
            startup.begin_shutdown();
            ryeosd::request_shutdown();
            let work_result = (&mut daemon_work).await;
            if result.is_ok() {
                result = work_result;
            } else if let Err(error) = work_result {
                tracing::error!(%error, "daemon work also failed while handling listener loss");
            }
        }

        // No startup-owned task may remain parked behind a gate that can no
        // longer reach Ready. Waking it with cancellation drops transient
        // claims/tokens while preserving durable recovery truth for next boot.
        if startup.is_starting() {
            recovery_execution_release.cancel();
        }

        let startup_cancelled = result.is_err()
            && startup.is_starting()
            && shutdown_was_requested
            && (first_exit == FirstExit::DaemonWork || first_exit_was_clean);
        let startup_failed = result.is_err() && startup.is_starting() && !startup_cancelled;
        let uds_reporting_alive = first_exit == FirstExit::DaemonWork && !uds_task.is_finished();

        if startup_failed && uds_reporting_alive {
            let error = result.as_ref().expect_err("startup_failed implies error");
            if let Err(publish_error) = startup.failed(error) {
                tracing::error!(
                    error = %publish_error,
                    "failed to publish terminal startup failure"
                );
            }
            // The daemon-work future and all producers have already stopped.
            // Retain only the stable local diagnostics surface for the bounded
            // failure grace; never attempt this when UDS itself was lost.
            tokio::select! {
                _ = tokio::time::sleep(STARTUP_FAILURE_REPORTING_GRACE) => {}
                _ = shutdown_signal() => {}
            }
        }

        startup.begin_shutdown();
        ryeosd::request_shutdown();

        progress_task.abort();
        let _ = progress_task.await;

        if first_exit != FirstExit::HttpListener {
            if let Err(error) = settle_http_listener(&mut http_task).await {
                if result.is_ok() {
                    result = Err(error);
                } else {
                    tracing::error!(%error, "HTTP listener also failed during daemon shutdown");
                }
            }
        }
        if first_exit != FirstExit::UdsListener {
            if let Err(error) = settle_uds_listener(&mut uds_task).await {
                if result.is_ok() {
                    result = Err(error);
                } else {
                    tracing::error!(%error, "UDS listener also failed during daemon shutdown");
                }
            }
        }

        let shutdown_state = {
            let mut guard = shutdown_drain_state
                .lock()
                .expect("shutdown drain state mutex poisoned");
            guard.take()
        };
        if let Some(state) = shutdown_state {
            if !drain_running_threads(&state).await && result.is_ok() {
                result = Err(anyhow::anyhow!(
                    "shutdown could not prove every attached process terminated"
                ));
            }
        }

        (result, startup_cancelled, startup_failed)
    };
    let (daemon_result, startup_cancelled, startup_failed) = daemon_result;

    let exit_reason = if daemon_result.is_ok() || startup_cancelled {
        "signal"
    } else if startup_failed {
        "startup_failed"
    } else {
        "runtime_error"
    };
    lifecycle_exit.record(exit_reason);
    match daemon_result.as_ref() {
        Ok(()) => tracing::info!(reason = exit_reason, "daemon exiting"),
        Err(error) => tracing::error!(reason = exit_reason, error = %error, "daemon exiting"),
    }
    if startup_cancelled {
        Ok(())
    } else {
        daemon_result
    }
}

fn classify_http_listener_exit(
    result: std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match result {
        Err(error) => Err(anyhow::Error::new(error).context("http listener task stopped")),
        Ok(Err(error)) => Err(anyhow::Error::new(error).context("http listener failed")),
        Ok(Ok(())) if ryeosd::shutdown_requested() => Ok(()),
        Ok(Ok(())) => anyhow::bail!("http listener exited before shutdown"),
    }
}

fn classify_uds_listener_exit(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match result {
        Err(error) => Err(anyhow::Error::new(error).context("UDS listener task stopped")),
        Ok(Err(error)) => Err(error.context("UDS listener failed")),
        Ok(Ok(())) if ryeosd::shutdown_requested() => Ok(()),
        Ok(Ok(())) => anyhow::bail!("UDS listener exited before shutdown"),
    }
}

const LISTENER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

async fn settle_http_listener(
    task: &mut tokio::task::JoinHandle<std::io::Result<()>>,
) -> Result<()> {
    match tokio::time::timeout(LISTENER_SHUTDOWN_TIMEOUT, &mut *task).await {
        Ok(joined) => classify_http_listener_exit(joined),
        Err(_) => {
            tracing::warn!("HTTP listener exceeded shutdown grace; aborting remaining connections");
            task.abort();
            let _ = task.await;
            Ok(())
        }
    }
}

async fn settle_uds_listener(task: &mut tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    match tokio::time::timeout(LISTENER_SHUTDOWN_TIMEOUT, &mut *task).await {
        Ok(joined) => classify_uds_listener_exit(joined),
        Err(_) => {
            tracing::warn!("UDS listener exceeded shutdown grace; aborting remaining connections");
            task.abort();
            let _ = task.await;
            Ok(())
        }
    }
}

/// Read `node.max_live_fanout` from the layered signed execution config,
/// bundle defaults first and the node's own `.ai` tree last (last layer
/// wins, matching execution-policy layering). A layer that fails signature
/// verification is skipped loudly rather than trusted.
fn load_node_max_live_fanout(
    engine: &ryeos_engine::engine::Engine,
    app_root: &std::path::Path,
) -> Option<u32> {
    let roots = engine.resolution_roots(Some(app_root.to_path_buf()));
    let parsers = match engine.effective_parser_dispatcher(Some(app_root)) {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "node execution limits: parser dispatcher unavailable");
            return None;
        }
    };
    let ctx = ryeos_engine::config_loading::ConfigLoadContext {
        roots: &roots,
        parsers: &parsers,
        kinds: &engine.kinds,
        trust_store: &engine.node_trust_store,
    };
    let mut limit: Option<u32> = None;
    for root in &roots.ordered {
        let candidate = root
            .ai_root
            .join("config")
            .join("execution")
            .join("execution.yaml");
        if !candidate.exists() {
            continue;
        }
        match ryeos_engine::config_loading::load_and_verify_config_file(&candidate, &ctx) {
            Ok(value) => {
                if let Some(n) = value
                    .get("node")
                    .and_then(|n| n.get("max_live_fanout"))
                    .and_then(|v| v.as_u64())
                {
                    limit = Some(n as u32);
                }
            }
            Err(err) => {
                tracing::warn!(
                    path = %candidate.display(),
                    error = %err,
                    "node execution limits: config layer failed verification — ignoring"
                );
            }
        }
    }
    limit.filter(|n| *n > 0)
}

/// Cross the durable ownership boundary for every active-thread resume intent.
/// The same path is used at startup (while the execution gate is armed) and by
/// the periodic live sweep (after the gate opens), so recovery semantics cannot
/// drift between boot and mid-life repair.
async fn dispatch_resume_intents(
    state: &AppState,
    intents: Vec<reconcile::ResumeIntent>,
) -> Result<()> {
    for intent in intents {
        let thread_id = intent.thread_id.clone();
        if intent.kind != reconcile::ResumeKind::NativeResume {
            let outcome = match intent.kind {
                reconcile::ResumeKind::OperatorContinuation => {
                    ryeos_executor::execution::launch::prepare_and_spawn_operator_successor_recovery(
                        state.clone(),
                        &thread_id,
                    )
                }
                reconcile::ResumeKind::Continuation => {
                    ryeos_executor::execution::launch::prepare_and_spawn_successor_recovery(
                        state.clone(),
                        &thread_id,
                    )
                }
                reconcile::ResumeKind::NativeResume => unreachable!("handled above"),
            }
            .with_context(|| format!("durably enqueue successor recovery {thread_id}"))?;
            tracing::info!(thread_id, ?outcome, "successor recovery classified");
            continue;
        }

        // Runtime-registry resumes require the managed envelope path. Generic
        // tool-subprocess resumes use the runner path; it transfers the same
        // durable launch claim to its detached background task.
        if intent.resume_context.runtime_ref.is_some() {
            let outcome = ryeos_executor::execution::launch::prepare_and_spawn_existing_native_resume_recovery(
                state.clone(),
                &thread_id,
            )
            .with_context(|| format!("durably enqueue managed native resume {thread_id}"))?;
            tracing::info!(thread_id, ?outcome, "managed native resume classified");
            continue;
        }

        let params = match ryeos_executor::execution::runner::execution_params_from_resume_context(
            state,
            &intent.resume_context,
        ) {
            Ok(params) => params,
            Err(error) => {
                tracing::error!(
                    thread_id,
                    error = %error,
                    "resume parameters could not be reconstructed; classifying failed"
                );
                state.threads.finalize_thread(
                    &ryeos_app::thread_lifecycle::ThreadFinalizeParams {
                        thread_id,
                        status: "failed".to_string(),
                        outcome_code: Some("resume_rebuild_failed".to_string()),
                        result: None,
                        error: Some(serde_json::json!({
                            "code": "resume_rebuild_failed",
                            "message": error.to_string(),
                        })),
                        metadata: None,
                        artifacts: Vec::new(),
                        final_cost: None,
                        summary_json: None,
                    },
                )?;
                continue;
            }
        };
        match ryeos_executor::execution::runner::run_existing_detached(
            state.clone(),
            thread_id.clone(),
            intent.chain_root_id,
            params,
            intent.prior_status,
        )
        .await
        {
            Ok(outcome) => {
                tracing::info!(thread_id, ?outcome, "tool native resume classified");
            }
            Err(error) => {
                let terminal = state
                    .state_store
                    .get_thread(&thread_id)?
                    .and_then(|thread| {
                        ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
                    })
                    .is_some_and(|status| status.is_terminal());
                if !terminal {
                    return Err(anyhow::anyhow!(error)).with_context(|| {
                        format!(
                            "tool native resume {thread_id} failed before durable classification"
                        )
                    });
                }
                tracing::warn!(
                    thread_id,
                    error = %error,
                    "tool native resume classified as terminal failure"
                );
            }
        }
    }
    Ok(())
}

/// Persist and enqueue every startup follow recovery action. Each preparation
/// call returns only after its launch claim has been transferred to the owned
/// detached task, which is the pre-Ready durability boundary.
fn prepare_follow_recovery_actions(
    state: &AppState,
    actions: Vec<reconcile::FollowReconcileAction>,
) -> Result<()> {
    for action in actions {
        let (label, outcome) = match action {
            reconcile::FollowReconcileAction::Resume { follow_key } => {
                let outcome =
                    ryeos_executor::execution::launch::prepare_and_spawn_follow_resume_recovery(
                        state.clone(),
                        &follow_key,
                    )
                    .with_context(|| format!("durably enqueue follow resume {follow_key}"))?;
                (format!("parent-resume {follow_key}"), outcome)
            }
            reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } => {
                let outcome =
                    ryeos_executor::execution::launch::prepare_and_spawn_follow_child_recovery(
                        state.clone(),
                        &child_thread_id,
                    )
                    .with_context(|| format!("durably enqueue follow child {child_thread_id}"))?;
                (format!("child-relaunch {child_thread_id}"), outcome)
            }
        };
        tracing::info!(action = %label, ?outcome, "follow recovery classified");
    }
    Ok(())
}

/// Verify the pre-Ready recovery ownership boundary for the complete initial
/// `created|running` set. The startup execution gate keeps every detached
/// recovery task inert here, so each row must be terminal, attached to a
/// verified RyeOS process, protected by a launch claim, or owned by a durable
/// follow/launch-window state machine.
fn ensure_recovery_targets_classified(state: &AppState, targets: &BTreeSet<String>) -> Result<()> {
    for thread_id in targets {
        let thread = state
            .state_store
            .get_thread(thread_id)?
            .ok_or_else(|| anyhow::anyhow!("recovery target disappeared: {thread_id}"))?;
        let status = ryeos_state::objects::ThreadStatus::from_str_lossy(&thread.status)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "recovery target {thread_id} has unknown status {:?}",
                    thread.status
                )
            })?;
        let live_owned_process = thread
            .runtime
            .process_identity
            .as_ref()
            .is_some_and(|identity| {
                ryeos_app::process::execution_liveness(identity)
                    == ryeos_app::process::IdentityLiveness::Alive
                    && ryeos_app::process::execution_group_liveness(identity)
                        == ryeos_app::process::IdentityLiveness::Alive
            });
        let durable_follow_owner = state
            .state_store
            .get_follow_waiter_by_child_chain(&thread.chain_root_id)?
            .is_some()
            || state
                .state_store
                .get_follow_waiter_by_parent_thread(thread_id)?
                .is_some()
            || state
                .state_store
                .get_follow_waiter_by_successor(thread_id)?
                .is_some();
        let durable_window_owner = state
            .state_store
            .launch_window_is_member(&thread.chain_root_id)?;
        if status.is_terminal()
            || live_owned_process
            || state.state_store.get_launch_claim(thread_id)?.is_some()
            || durable_follow_owner
            || durable_window_owner
        {
            continue;
        }
        anyhow::bail!(
            "recovery target {thread_id} reached readiness without terminal state, a verified live process, or durable claim/follow/window ownership"
        );
    }
    Ok(())
}

/// Drive follow reconcile actions during the periodic live sweep. Every launch
/// is claim-guarded, so concurrent drives are benign skips; real failures are
/// propagated to the supervised recovery task instead of becoming log-only
/// stranded work.
async fn dispatch_follow_actions(
    state: &AppState,
    actions: Vec<reconcile::FollowReconcileAction>,
) -> Result<()> {
    for action in actions {
        use ryeos_executor::execution::launch::{launch_follow_child, SuccessorLaunchOutcome};
        let (label, outcome) = match action {
            reconcile::FollowReconcileAction::Resume { follow_key } => {
                let outcome = ryeos_executor::execution::launch::launch_follow_resume_successor(
                    state.clone(),
                    &follow_key,
                )
                .await;
                (format!("parent-resume {follow_key}"), outcome)
            }
            reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } => {
                // Reconcile parity: a fresh relaunch, no parent clamp/depth.
                let outcome =
                    launch_follow_child(state.clone(), &child_thread_id, None, None).await;
                (format!("child-relaunch {child_thread_id}"), outcome)
            }
        };
        match outcome {
            Ok(SuccessorLaunchOutcome::Launched(_)) => {}
            Ok(SuccessorLaunchOutcome::Skipped(reason)) => {
                tracing::debug!(action = %label, reason, "reconcile: follow action skipped");
            }
            Err(error) => {
                return Err(anyhow::anyhow!(error))
                    .with_context(|| format!("reconcile follow action {label}"));
            }
        }
    }
    Ok(())
}

async fn run_periodic_recovery(state: AppState) -> Result<()> {
    if !ryeos_app::recovery_execution_gate::wait_if_armed().await {
        return Ok(());
    }
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let period = Duration::from_secs(120);
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            _ = tick.tick() => {}
        }

        ryeos_app::cascade::repair_cancelled_window_members(&state)
            .context("periodic cancelled launch-window repair")?;

        let active = reconcile::reconcile_live_threads(&state)
            .await
            .context("periodic active-thread reconcile")?;
        let mut targets = active.active_thread_ids;
        dispatch_resume_intents(&state, active.resume_intents)
            .await
            .context("periodic active-thread recovery dispatch")?;

        let follow_actions =
            reconcile::reconcile_follow(&state).context("periodic follow reconcile")?;
        for action in &follow_actions {
            match action {
                reconcile::FollowReconcileAction::Resume { follow_key } => {
                    let waiter = state
                        .state_store
                        .get_follow_waiter_by_key(follow_key)?
                        .ok_or_else(|| {
                            anyhow::anyhow!("periodic follow waiter disappeared: {follow_key}")
                        })?;
                    if let Some(successor) = waiter.parent_successor_thread_id {
                        targets.insert(successor);
                    }
                }
                reconcile::FollowReconcileAction::RelaunchChild { child_thread_id } => {
                    targets.insert(child_thread_id.clone());
                }
            }
        }
        dispatch_follow_actions(&state, follow_actions).await?;

        for (child_thread_id, outcome) in
            ryeos_executor::execution::launch::prepare_launch_window_recovery(&state)
                .context("periodic launch-window recovery")?
        {
            targets.insert(child_thread_id.clone());
            tracing::info!(
                child_thread_id,
                ?outcome,
                "periodic launch-window recovery classified"
            );
        }

        ensure_recovery_targets_classified(&state, &targets)
            .context("periodic recovery ownership boundary")?;
    }
}

async fn run_ui_hint_loop(hub: Arc<ThreadEventHub>, ui: Arc<ryeos_ui::UiState>) -> Result<()> {
    let mut rx = hub.subscribe_all();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut activity_tick = tokio::time::interval(Duration::from_millis(750));
    activity_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending_activity: HashMap<String, u64> = HashMap::new();

    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            recv = rx.recv() => {
                match recv {
                    Ok(event) => {
                        if ryeos_api::routes::invokers::stream_helpers::is_lifecycle_hint(
                            &event.event_type,
                        ) {
                            let status = ryeos_api::routes::invokers::stream_helpers::lifecycle_status(
                                &event.event_type,
                            );
                            let payload = serde_json::json!({
                                "kind": "thread",
                                "thread_id": &event.thread_id,
                                "chain_root_id": &event.chain_root_id,
                                "event_type": &event.event_type,
                                "status": status,
                                "updated_at": &event.ts,
                            });
                            for session_id in ui.browser_sessions.session_ids() {
                                ui.session_bus.publish(&session_id, "thread.hint", payload.clone());
                            }
                        } else if !event.event_type.starts_with("seat.") {
                            *pending_activity.entry(event.thread_id).or_insert(0) += 1;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        anyhow::bail!("thread event hub closed before daemon shutdown")
                    }
                }
            }
            _ = activity_tick.tick() => {
                if pending_activity.is_empty() {
                    continue;
                }
                let thread_ids = pending_activity.keys().cloned().collect::<Vec<_>>();
                let event_count = pending_activity.values().sum::<u64>();
                pending_activity.clear();
                let payload = serde_json::json!({
                    "kind": "activity",
                    "thread_ids": thread_ids,
                    "event_count": event_count,
                });
                for session_id in ui.browser_sessions.session_ids() {
                    ui.session_bus.publish(&session_id, "thread.hint", payload.clone());
                }
            }
        }
    }
}

async fn supervise_background_tasks(
    state: AppState,
    ui: Arc<ryeos_ui::UiState>,
    scheduler_task: tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let mut tasks = tokio::task::JoinSet::<(&'static str, Result<()>)>::new();

    tasks.spawn(async move {
        let result = scheduler_task
            .await
            .context("scheduler timer task join failed")
            .and_then(|result| result);
        ("scheduler timer", result)
    });

    let repair_store = state.state_store.clone();
    tasks.spawn(async move {
        (
            "projection repair",
            ryeos_app::projection_repair::run(repair_store, shutdown_signal()).await,
        )
    });

    let periodic_state = state.clone();
    tasks.spawn(async move {
        (
            "periodic recovery",
            run_periodic_recovery(periodic_state).await,
        )
    });

    let hint_hub = state.event_streams.clone();
    tasks.spawn(async move { ("UI hint fanout", run_ui_hint_loop(hint_hub, ui).await) });

    let first_result = tokio::select! {
        _ = shutdown_signal() => Ok(()),
        joined = tasks.join_next() => {
            match joined {
                Some(Ok((name, result))) if ryeosd::shutdown_requested() => {
                    result.with_context(|| format!("{name} failed during shutdown"))
                }
                Some(Ok((name, Ok(())))) => {
                    anyhow::bail!("supervised {name} task exited before shutdown")
                }
                Some(Ok((name, Err(error)))) => {
                    Err(error).with_context(|| format!("supervised {name} task failed"))
                }
                Some(Err(error)) => {
                    Err(anyhow::Error::new(error).context("supervised daemon task panicked"))
                }
                None => anyhow::bail!("all supervised daemon tasks disappeared"),
            }
        }
    };

    // One process-wide signal closes every task and both listeners. Projection
    // repair is not aborted: if it is inside spawn_blocking, supervision waits
    // for that bounded journal-replay unit to finish before state is drained.
    ryeosd::request_shutdown();
    let mut drain_error = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((_, Ok(()))) => {}
            Ok((name, Err(error))) => {
                drain_error.get_or_insert_with(|| error.context(format!("{name} shutdown")));
            }
            Err(error) => {
                drain_error.get_or_insert_with(|| {
                    anyhow::Error::new(error).context("supervised task shutdown join")
                });
            }
        }
    }

    match first_result {
        Err(error) => Err(error),
        Ok(()) => match drain_error {
            Some(error) => Err(error),
            None => Ok(()),
        },
    }
}

async fn drain_running_threads(state: &AppState) -> bool {
    // Serialize shutdown against every future UDS/internal attachment. An
    // attach that committed first appears below; one that arrives later is
    // rejected and its spawn owner aborts the exact process.
    if let Err(error) = state.state_store.close_process_attachment_admission() {
        tracing::error!(error = %error, "failed to close process attachment admission");
        return false;
    }

    let drain_deadline = Instant::now()
        .checked_add(Duration::from_secs(
            process::MAX_GRACEFUL_SHUTDOWN_GRACE_SECS,
        ))
        .unwrap_or_else(Instant::now);
    let attached_ids = match state.state_store.list_attached_thread_ids() {
        Ok(ids) => ids,
        Err(error) => {
            tracing::error!(error = %error, "failed to list attached threads during shutdown");
            return false;
        }
    };
    if attached_ids.is_empty() {
        return true;
    }
    tracing::info!(
        count = attached_ids.len(),
        "draining attached threads concurrently"
    );

    const HARD_KILL_PROOF_RESERVE: Duration = Duration::from_millis(250);
    let mut pending = Vec::with_capacity(attached_ids.len());
    for thread_id in attached_ids {
        let thread = match state.state_store.get_thread(&thread_id) {
            Ok(Some(thread)) => thread,
            Ok(None) => {
                tracing::warn!(thread_id, "attached runtime row has no thread");
                continue;
            }
            Err(error) => {
                tracing::warn!(thread_id, error = %error, "failed to load attached thread");
                continue;
            }
        };
        let Some(identity) = thread.runtime.process_identity.clone() else {
            tracing::warn!(thread_id, "attached row lost its durable process identity");
            continue;
        };
        let action = process::resolve_shutdown_action(
            thread
                .runtime
                .launch_metadata
                .as_ref()
                .and_then(|metadata| metadata.cancellation_mode),
        );
        let kill_identity = identity.clone();
        let kill_task = tokio::task::spawn_blocking(move || {
            let bounded_action = match action {
                process::ShutdownAction::Hard => process::ShutdownAction::Hard,
                process::ShutdownAction::Graceful(grace) => process::ShutdownAction::Graceful(
                    grace.min(
                        drain_deadline
                            .saturating_duration_since(Instant::now())
                            .saturating_sub(HARD_KILL_PROOF_RESERVE),
                    ),
                ),
            };
            process::kill_by_action(&kill_identity, bounded_action)
        });
        pending.push((thread_id, thread.status, identity, kill_task));
    }

    for (thread_id, status, identity, kill_task) in pending {
        let result = match kill_task.await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(thread_id, error = %error, "shutdown process-kill worker failed");
                continue;
            }
        };
        if !result.success {
            tracing::warn!(
                thread_id,
                method = result.method,
                "failed to prove attached process termination during shutdown"
            );
            continue;
        }
        match state
            .state_store
            .clear_thread_process_if_matches(&thread_id, &identity)
        {
            Ok(true) => {}
            Ok(false) => tracing::warn!(thread_id, "shutdown identity changed before clear"),
            Err(error) => tracing::warn!(
                thread_id,
                error = %error,
                "failed to clear shutdown process identity"
            ),
        }
        if !ryeos_app::state_store::is_terminal_status(&status) {
            if let Err(error) = state.state_store.reset_resume_attempts(&thread_id) {
                tracing::warn!(
                    thread_id,
                    error = %error,
                    "failed to re-arm resume budget during drain"
                );
            }
        }
    }

    match state.state_store.list_attached_thread_ids() {
        Ok(remaining) if !remaining.is_empty() => {
            tracing::error!(
                remaining = ?remaining,
                "shutdown drain exhausted with attached identities still present"
            );
            false
        }
        Ok(_) => true,
        Err(error) => {
            tracing::error!(error = %error, "failed final shutdown drain audit");
            false
        }
    }
}

async fn shutdown_signal() {
    if ryeosd::shutdown_requested() {
        return;
    }

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
    ryeosd::request_shutdown();
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

    let identity = NodeIdentity::load(&config.node_signing_key_path)?;
    ryeos_app::bundle_transaction::reconcile_all_bundle_transactions(
        &config.app_root,
        identity.signing_key(),
    )
    .context("reconcile interrupted bundle transactions")?;

    let sandbox = Arc::new(
        ryeos_engine::sandbox::SandboxRuntime::load(&config.app_root)
            .context("load node sandbox policy")?,
    );

    // Two-phase node-config bootstrap (same as daemon-start path)
    let (engine, node_config_snapshot, sandbox) =
        bootstrap::load_node_config_two_phase(config, Arc::clone(&sandbox))?;
    let node_history_policy = {
        let roots = engine.resolution_roots(Some(config.app_root.clone()));
        let parsers = engine.effective_parser_dispatcher(Some(&config.app_root))?;
        let context = ryeos_engine::config_loading::ConfigLoadContext {
            roots: &roots,
            parsers: &parsers,
            kinds: &engine.kinds,
            trust_store: &engine.node_trust_store,
        };
        Arc::new(ryeos_engine::history_policy::load_node_thread_history_policy(&context)?)
    };

    let params: serde_json::Value = match params_json {
        Some(json_str) => {
            serde_json::from_str(json_str).with_context(|| "parse --params as JSON")?
        }
        None => serde_json::json!({}),
    };
    let standalone_principal = identity.principal_id();
    let standalone_plan_ctx = ryeos_engine::contracts::PlanContext {
        requested_by: ryeos_engine::contracts::EffectivePrincipal::Local(
            ryeos_engine::contracts::Principal {
                fingerprint: standalone_principal.clone(),
                scopes: vec![],
            },
        ),
        project_context: ryeos_engine::contracts::ProjectContext::None,
        current_site_id: "site:local".into(),
        origin_site_id: "site:local".into(),
        execution_hints: ryeos_engine::contracts::ExecutionHints::default(),
        validate_only: false,
    };
    let service_canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(service_ref)
        .with_context(|| format!("invalid standalone service ref {service_ref}"))?;
    let service_resolved = engine
        .resolve(&standalone_plan_ctx, &service_canonical)
        .with_context(|| format!("resolve standalone service {service_ref}"))?;
    let service_verified = engine
        .verify(&standalone_plan_ctx, service_resolved)
        .with_context(|| format!("verify standalone service {service_ref}"))?;
    let standalone_state_access = ryeos_app::service_registry::extract_standalone_state_access(
        &service_verified.resolved.metadata.extra,
    )?;

    let kind_profiles = Arc::new(kind_profiles::KindProfileRegistry::build(Some(
        &engine.kinds,
    )));
    let services = Arc::new(build_service_registry());

    let runtime_state_dir = config.runtime_state_dir();
    let runtime_db_path = config.db_path.clone();
    let signer = Arc::new(state_store::NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(
        identity.fingerprint().to_string(),
        *identity.verifying_key(),
    );
    let write_barrier = ryeos_app::write_barrier::WriteBarrier::new();
    let head_trust = Arc::new(head_trust);
    let state_store = Arc::new(match standalone_state_access {
        ryeos_app::service_registry::StandaloneStateAccess::ReadWrite => {
            state_store::StateStore::new_with_head_trust(
                config.app_root.clone(),
                runtime_state_dir,
                runtime_db_path,
                signer,
                write_barrier.clone(),
                head_trust,
            )?
        }
        ryeos_app::service_registry::StandaloneStateAccess::ReadOnlyExisting => {
            state_store::StateStore::new_for_projection_verification(
                runtime_state_dir,
                signer,
                write_barrier.clone(),
                head_trust,
            )?
        }
        ryeos_app::service_registry::StandaloneStateAccess::ProjectionRebuild => {
            state_store::StateStore::new_for_projection_rebuild(
                config.app_root.clone(),
                runtime_state_dir,
                runtime_db_path,
                signer,
                write_barrier.clone(),
                head_trust,
            )?
        }
    });

    let events = Arc::new(event_store_service::EventStoreService::new(
        state_store.clone(),
    ));
    let event_streams = Arc::new(ThreadEventHub::new(DEFAULT_EVENT_STREAM_CAPACITY));
    let threads = Arc::new(thread_lifecycle::ThreadLifecycleService::new(
        state_store.clone(),
        engine.clone(),
        kind_profiles.clone(),
        events.clone(),
        event_streams.clone(),
    )?);
    let live_input = Arc::new(ryeos_app::live_input_queue::LiveInputQueue::new());
    threads.set_live_input_queue(live_input.clone());
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
    let standalone_ui_state = Arc::new(ryeos_ui::UiState::new());
    let standalone_node_config_validator =
        prospective_node_config_validator(standalone_ui_state.clone());
    let standalone_scheduler_db = match standalone_state_access {
        ryeos_app::service_registry::StandaloneStateAccess::ProjectionRebuild => {
            let scheduler_path = config
                .app_root
                .join(ryeos_engine::AI_DIR)
                .join("state")
                .join("scheduler.sqlite3");
            Arc::new(
                SchedulerDb::open_existing_current(&scheduler_path).with_context(|| {
                    format!(
                        "projection rebuild requires current persisted scheduler state at {}",
                        scheduler_path.display()
                    )
                })?,
            )
        }
        ryeos_app::service_registry::StandaloneStateAccess::ReadWrite
        | ryeos_app::service_registry::StandaloneStateAccess::ReadOnlyExisting => {
            Arc::new(SchedulerDb::new_in_memory().context("scheduler in-memory db")?)
        }
    };

    let app_state = state::AppState {
        config: Arc::new(config.clone()),
        sandbox,
        state_store,
        engine: engine.clone(),
        engine_cache: ryeos_app::engine_cache::EngineCache::new(
            ryeos_app::engine_cache::EngineCacheConfig::default(),
        ),
        identity: Arc::new(identity),
        threads,
        live_input,
        events,
        event_streams,
        commands,
        callback_tokens: Arc::new(ryeos_app::callback_token::CallbackCapabilityStore::new()),
        thread_auth: Arc::new(ryeos_app::callback_token::ThreadAuthStore::new()),
        extensions: {
            let mut extensions = ryeos_app::extension_state::ExtensionState::new();
            extensions.insert(standalone_ui_state);
            extensions.insert(standalone_node_config_validator);
            Arc::new(extensions)
        },
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
        node_history_policy,
        vault: Arc::new(
            ryeos_app::vault::SealedEnvelopeVault::load(&config.app_root)
                .context("load sealed-envelope vault — did `ryeos init` run?")?,
        ),
        command_registry: standalone_command_registry,
        authorizer: standalone_auth,
        scheduler_db: standalone_scheduler_db,
        scheduler_runtime_gate: Arc::new(tokio::sync::RwLock::new(())),
        scheduler_reload_tx: None,
        ignore_matcher: Arc::new(ryeos_app::ignore::matcher_from_builtins()),
        vault_fingerprint: None,
    };

    let ctx = ExecutionContext {
        principal_fingerprint: standalone_principal,
        caller_scopes: vec![], // standalone: operator authority, no caps
        engine,
        plan_ctx: standalone_plan_ctx,
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
        invocation_id = %result.invocation_id,
        recorded = result.recorded,
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
