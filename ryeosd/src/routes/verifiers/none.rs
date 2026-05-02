use std::sync::Arc;

use serde_json::Value;

use crate::dispatch_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    AuthVerifier, CompiledAuthVerifier, RoutePrincipal, VerifierRequestContext,
};

pub struct NoneVerifier;

impl AuthVerifier for NoneVerifier {
    fn key(&self) -> &'static str {
        "none"
    }

    fn validate_route_config(
        &self,
        _route_id: &str,
        _auth_config: Option<&Value>,
    ) -> Result<Arc<dyn CompiledAuthVerifier>, RouteConfigError> {
        Ok(Arc::new(CompiledNoneVerifier))
    }
}

struct CompiledNoneVerifier;

impl CompiledAuthVerifier for CompiledNoneVerifier {
    fn verify(
        &self,
        route_id: &str,
        _req: &VerifierRequestContext,
        _state: &crate::state::AppState,
    ) -> Result<RoutePrincipal, RouteDispatchError> {
        Ok(RoutePrincipal {
            id: format!("route:{}", route_id),
            scopes: vec![],
            verifier_key: "none",
            verified: false,
            metadata: std::collections::BTreeMap::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, Method};
    use tempfile::TempDir;

    fn setup_test_state() -> (TempDir, crate::state::AppState) {
        let tmpdir = TempDir::new().unwrap();
        let state_root = tmpdir.path().join(".ai").join("state");
        let runtime_db_path = tmpdir.path().join("runtime.sqlite3");
        let key_path = tmpdir.path().join("identity").join("node-key.pem");
        let config = crate::config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: runtime_db_path.clone(),
            uds_path: tmpdir.path().join("test.sock"),
            state_dir: tmpdir.path().to_path_buf(),
            node_signing_key_path: key_path.clone(),
            user_signing_key_path: tmpdir.path().join("user-key.pem"),
            system_data_dir: tmpdir.path().join("system"),
            require_auth: false,
            authorized_keys_dir: tmpdir.path().join("auth"),
        };

        let identity = crate::identity::NodeIdentity::create(&key_path).unwrap();
        let signer = Arc::new(
            crate::state_store::NodeIdentitySigner::from_identity(&identity),
        );
        let write_barrier = crate::write_barrier::WriteBarrier::new();
        let state_store = Arc::new(
            crate::state_store::StateStore::new(
                state_root,
                runtime_db_path,
                signer,
                write_barrier.clone(),
            )
            .unwrap(),
        );
        let kind_profiles = Arc::new(
            crate::kind_profiles::KindProfileRegistry::load_defaults(),
        );
        let events = Arc::new(
            crate::services::event_store::EventStoreService::new(state_store.clone()),
        );
        let threads = Arc::new(
            crate::services::thread_lifecycle::ThreadLifecycleService::new(
                state_store.clone(),
                kind_profiles.clone(),
                events.clone(),
            ),
        );
        let commands = Arc::new(
            crate::services::command_service::CommandService::new(
                state_store.clone(),
                kind_profiles,
                events.clone(),
            ),
        );

        let engine = ryeos_engine::engine::Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::ParserDispatcher::new(
                ryeos_engine::parsers::ParserRegistry::empty(),
                std::sync::Arc::new(ryeos_engine::handlers::HandlerRegistry::empty()),
            ),
            None,
            Vec::new(),
        );

        let snapshot = crate::node_config::NodeConfigSnapshot {
            bundles: vec![],
            routes: vec![],
        };

        let state = crate::state::AppState {
            config: Arc::new(config),
            state_store,
            engine: Arc::new(engine),
            identity: Arc::new(identity),
            threads,
            events,
            event_streams: Arc::new(
                crate::event_stream::ThreadEventHub::new(16),
            ),
            commands,
            callback_tokens: Arc::new(
                crate::execution::callback_token::CallbackCapabilityStore::new(),
            ),
            thread_auth: Arc::new(
                crate::execution::callback_token::ThreadAuthStore::new(),
            ),
            write_barrier: Arc::new(write_barrier),
            started_at: std::time::Instant::now(),
            started_at_iso: String::new(),
            catalog_health: crate::state::CatalogHealth {
                status: "ok".into(),
                missing_services: vec![],
            },
            services: Arc::new(crate::service_registry::build_service_registry()),
            node_config: Arc::new(snapshot.clone()),
            route_table: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::routes::build_route_table_or_bail(&snapshot).unwrap(),
            )),
            webhook_dedupe: Arc::new(crate::routes::webhook_dedupe::WebhookDedupeStore::new()),
            vault: Arc::new(crate::vault::EmptyVault),
        };

        (tmpdir, state)
    }

    #[test]
    fn none_verifier_always_succeeds() {
        let verifier = NoneVerifier;
        let compiled = verifier.validate_route_config("test-route", None).unwrap();
        let ctx = VerifierRequestContext {
            method: &Method::GET,
            path: "/test",
            headers: &HeaderMap::new(),
            body_raw: &[],
        };
        let (_tmp, state) = setup_test_state();
        let principal = compiled.verify("test-route", &ctx, &state).unwrap();
        assert_eq!(principal.verifier_key, "none");
        assert!(!principal.verified);
        assert_eq!(principal.id, "route:test-route");
    }
}
