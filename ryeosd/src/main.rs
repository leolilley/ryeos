mod api;
mod auth;
mod bootstrap;
mod config;
mod engine_init;
mod execution;
mod identity;
mod kind_profiles;
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
    
    // Initialize StateStore with CAS backing (Phase 0.5H.3)
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

    reconcile::reconcile(&app_state).await?;

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

    let shutdown = shutdown_signal();
    let http = serve(tcp_listener, app).with_graceful_shutdown(shutdown);

    tokio::select! {
        result = http => {
            result.context("http server exited unexpectedly")?;
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
            tracing::info!(
                pgid,
                thread_id = %thread.thread_id,
                "killing process group"
            );
            let result = process::kill_process_group(pgid, std::time::Duration::from_secs(3));
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
        .route("/status", get(api::health::status))
        .route("/public-key", get(api::health::public_key))
        .route("/threads", get(api::threads::list_threads))
        .route("/threads/:thread_id", get(api::threads::get_thread))
        .route(
            "/threads/:thread_id/children",
            get(api::threads::list_children),
        )
        .route("/threads/:thread_id/chain", get(api::threads::get_chain))
        .route(
            "/threads/:thread_id/commands",
            post(api::commands::submit_command),
        )
        .route(
            "/threads/:thread_id/events",
            get(api::events::get_thread_events),
        )
        .route(
            "/threads/:thread_id/events/stream",
            get(api::events::stream_thread_events),
        )
        .route(
            "/chains/:chain_root_id/events",
            get(api::events::get_chain_events),
        )
        .route(
            "/chains/:chain_root_id/events/stream",
            get(api::events::stream_chain_events),
        )
        .route("/execute", post(api::execute::execute))
        .route(
            "/runtime/{method}",
            post(api::runtime_callback::runtime_callback),
        )
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
