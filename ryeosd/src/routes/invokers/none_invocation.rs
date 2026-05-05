//! Auth invoker for the `none` verifier.
//!
//! Produces an anonymous principal with no scopes. Used for public routes.

use std::collections::BTreeMap;

use crate::dispatch_error::RouteDispatchError;
use crate::routes::invocation::{
    CompiledRouteInvocation, PrincipalPolicy, RouteInvocationContract, RouteInvocationContext,
    RouteInvocationOutput, RouteInvocationResult, RoutePrincipal,
};

pub struct CompiledNoneVerifier;

static NONE_CONTRACT: RouteInvocationContract = RouteInvocationContract {
    output: RouteInvocationOutput::Principal,
    principal: PrincipalPolicy::Forbidden,
};

#[axum::async_trait]
impl CompiledRouteInvocation for CompiledNoneVerifier {
    fn contract(&self) -> &'static RouteInvocationContract {
        &NONE_CONTRACT
    }

    async fn invoke(
        &self,
        ctx: RouteInvocationContext,
    ) -> Result<RouteInvocationResult, RouteDispatchError> {
        Ok(RouteInvocationResult::Principal(RoutePrincipal {
            id: format!("route:{}", ctx.route_id),
            scopes: vec![],
            verifier_key: "none",
            verified: false,
            metadata: BTreeMap::new(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn build_test_state() -> (tempfile::TempDir, crate::state::AppState) {
        std::env::set_var("HOSTNAME", "testhost");
        let tmpdir = tempfile::TempDir::new().unwrap();
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
            )
            .expect("HOSTNAME not set in test environment"),
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
        let test_vr = Arc::new(ryeos_runtime::verb_registry::VerbRegistry::with_builtins());
        let test_auth = Arc::new(ryeos_runtime::authorizer::Authorizer::new(test_vr.clone()));
        let state = crate::state::AppState {
            config: Arc::new(config),
            state_store,
            engine: Arc::new(engine),
            identity: Arc::new(identity),
            threads,
            events,
            event_streams: Arc::new(crate::event_stream::ThreadEventHub::new(16)),
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
            verb_registry: test_vr,
            authorizer: test_auth,
        };
        (tmpdir, state)
    }

    #[tokio::test]
    async fn none_verifier_always_succeeds() {
        let compiled = CompiledNoneVerifier;
        let (_tmp, state) = build_test_state();
        let ctx = crate::routes::invocation::RouteInvocationContext {
            route_id: "test-route".into(),
            method: axum::http::Method::GET,
            uri: "/test".parse().unwrap(),
            captures: BTreeMap::new(),
            headers: axum::http::HeaderMap::new(),
            body_raw: vec![],
            input: serde_json::Value::Null,
            principal: None,
            state,
        };
        let result = compiled.invoke(ctx).await.unwrap();
        match result {
            RouteInvocationResult::Principal(p) => {
                assert_eq!(p.verifier_key, "none");
                assert!(!p.verified);
                assert_eq!(p.id, "route:test-route");
            }
            other => panic!("expected Principal, got {:?}", other.variant_name()),
        }
    }
}
