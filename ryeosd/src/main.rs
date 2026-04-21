mod api;
mod auth;
mod bootstrap;
mod broker;
mod cas;
mod config;
mod db;
mod engine_init;
mod execution;
mod gc;
mod identity;
mod kind_profiles;
mod policy;
mod process;
mod reconcile;
mod refs;
mod registry;
mod services;
mod state;
mod uds;
mod vault;
mod webhooks;

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::routing::{get, post};
use axum::{serve, Router};
use chrono::Utc;
use clap::Parser;
use config::{Cli, Config};
use tokio::net::{TcpListener, UnixListener};
use tracing_subscriber::EnvFilter;

use crate::broker::{LiveBroker, DEFAULT_BROKER_CAPACITY};
use crate::cas::CasStore;
use crate::db::Database;
use crate::execution::callback_token::CallbackCapabilityStore;
use crate::identity::NodeIdentity;
use crate::refs::RefStore;
use crate::registry::RegistryStore;
use crate::services::budget_service::BudgetService;
use crate::services::command_service::CommandService;
use crate::services::event_store::EventStoreService;
use crate::services::thread_lifecycle::ThreadLifecycleService;
use crate::state::AppState;
use crate::vault::VaultStore;
use crate::webhooks::WebhookStore;

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
    let db = Database::new(&config.db_path, kind_profiles)?;
    let identity = NodeIdentity::load(&config.signing_key_path)?;
    let engine = Arc::new(engine_init::build_engine(&config)?);
    let db = Arc::new(db);
    let broker = Arc::new(LiveBroker::new(DEFAULT_BROKER_CAPACITY));
    let events = Arc::new(EventStoreService::new(db.clone(), broker.clone()));
    let threads = Arc::new(ThreadLifecycleService::new(db.clone(), events.clone()));
    let commands = Arc::new(CommandService::new(db.clone(), events.clone()));
    let budgets = Arc::new(BudgetService::new(db.clone(), events.clone()));
    let cas = Arc::new(CasStore::new(config.cas_root.clone()));
    let refs = Arc::new(RefStore::new(config.cas_root.clone()));
    let registry = Arc::new(RegistryStore::new(config.cas_root.clone()));
    let vault = Arc::new(VaultStore::new(config.cas_root.clone()));
    let webhooks = Arc::new(WebhookStore::new(config.cas_root.clone()));
    let callback_tokens = Arc::new(CallbackCapabilityStore::new());

    let state = AppState {
        config: Arc::new(config.clone()),
        db,
        engine: engine.clone(),
        identity: Arc::new(identity),
        threads,
        events,
        broker,
        commands,
        budgets,
        cas,
        refs,
        registry,
        vault,
        webhooks,
        callback_tokens,
        started_at: Instant::now(),
        started_at_iso: Utc::now().to_rfc3339(),
    };

    reconcile::reconcile(&state).await?;

    let app = build_router(state.clone())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ));
    let tcp_listener = TcpListener::bind(config.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.bind))?;
    let uds_listener = UnixListener::bind(&config.uds_path)
        .with_context(|| format!("failed to bind {}", config.uds_path.display()))?;

    std::env::set_var("RYEOSD_SOCKET_PATH", &config.uds_path);
    std::env::set_var("RYEOSD_URL", format!("http://{}", config.bind));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config.uds_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set socket permissions on {}", config.uds_path.display()))?;
    }

    let uds_state = Arc::new(state.clone());
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
    drain_running_threads(&state);

    if config.uds_path.exists() {
        let _ = std::fs::remove_file(&config.uds_path);
    }

    Ok(())
}

fn drain_running_threads(state: &AppState) {
    let threads = match state.db.list_threads_by_status(&["running"]) {
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
        .route("/objects/has", post(api::objects::has_objects))
        .route("/objects/put", post(api::objects::put_objects))
        .route("/objects/get", post(api::objects::get_objects))
        .route("/gc", post(api::gc::run_gc))
        .route("/push", post(api::push::push))
        .route("/project-head", get(api::push::get_project_head))
        .route("/push/user-space", post(api::push::push_user_space))
        .route("/user-space", get(api::push::get_user_space))
        .route("/registry/publish", post(api::registry::publish))
        .route("/registry/search", get(api::registry::search))
        .route("/registry/items/:kind/:item_id", get(api::registry::get_item))
        .route(
            "/registry/items/:kind/:item_id/versions/:version",
            get(api::registry::get_version),
        )
        .route(
            "/registry/namespaces/claim",
            post(api::registry::claim_namespace),
        )
        .route(
            "/registry/identity",
            post(api::registry::register_identity),
        )
        .route(
            "/registry/identity/:fingerprint",
            get(api::registry::lookup_identity),
        )
        .route("/vault/set", post(api::vault::vault_set))
        .route("/vault/get", post(api::vault::vault_get))
        .route("/vault/list", get(api::vault::vault_list))
        .route("/vault/delete", post(api::vault::vault_delete))
        .route(
            "/webhook-bindings",
            get(api::webhooks::list_webhooks).post(api::webhooks::create_webhook),
        )
        .route(
            "/webhook-bindings/:hook_id",
            axum::routing::delete(api::webhooks::revoke_webhook),
        )
        .route(
            "/webhooks/inbound/:hook_id",
            post(api::webhooks::inbound_webhook),
        )
        .route("/refs/pins", get(api::pins::list_pins).post(api::pins::write_pin))
        .route("/refs/pins/:name", axum::routing::delete(api::pins::delete_pin))
        .route("/refs/generic/*ref_path", get(api::refs::get_ref).put(api::refs::put_ref))
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
