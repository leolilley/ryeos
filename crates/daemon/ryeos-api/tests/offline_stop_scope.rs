//! Queueing an offline operation and stopping the node are separate scopes.
//! A delivery that asks for `stop: now` is refused before anything is queued
//! unless the caller is the node's local configured operator or holds the
//! exact stop scope; no wildcard in a grant satisfies it.

use ryeos_api::test_support as test_state;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_api::handler_error::HandlerError;
use ryeos_api::handlers::offline_operations::{
    OFFLINE_STOP_SCOPE, OFFLINE_STOP_SCOPE_REQUIRED, StopWhen, SubmitRequest, submit,
};
use ryeos_app::handler_context::HandlerContext;
use ryeos_app::state::AppState;
use ryeos_node::offline_operation::contract::{REQUEST_SCHEMA, RequestBody, SignedRequest};
use ryeos_node::offline_operation::deliver::{StopForOfflineOperation, status_report};
use ryeos_node::offline_operation::facts::MANDATORY_FACTS;
use ryeos_node::offline_operation::table::lookup;
use ryeos_node::offline_operation::{authorized_keys_dir, read_facts};
use ryeos_node_runtime::identity::{
    AuthorizedKeyPrincipalClass, GrantTerms, WildcardPolicy, reconcile_authorized_key_toml_scopes,
};
use serde_json::{Value, json};

const SITE: &str = "site:origin-a";
const QUEUE_SCOPE: &str = "ryeos.execute.service.offline-operations/submit";
const RESTART_SCOPE: &str = "ryeos.offline.node.allow-restart";

// Fields drop in declaration order: the state goes before the store it
// holds, and the store before the directory it lives in.
struct Rig {
    state: Arc<AppState>,
    operator: SigningKey,
    stopped: Arc<AtomicBool>,
    _store_owner: ryeos_app::test_support::TestStateStore,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let (dir, store_owner, state) = test_state::build_test_state();
    // The shared fixture keeps the node key outside the default runtime
    // paths; reading the node's facts to build a request looks for it there,
    // so the key is copied into place inside this test's own directory.
    let default_key = ryeos_engine::roots::RuntimeRoot::new(state.config.app_root.clone())
        .node_signing_key_path();
    std::fs::create_dir_all(default_key.parent().unwrap()).unwrap();
    std::fs::copy(&state.config.node_signing_key_path, &default_key).unwrap();
    let operator =
        ryeos_node_runtime::operator_identity::test_support::load_operator(&state.config.app_root)
            .unwrap()
            .signing_key()
            .clone();
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stopped);
    state
        .extensions
        .insert(Arc::new(StopForOfflineOperation::new(move || {
            flag.store(true, Ordering::SeqCst);
        })));
    Rig {
        _dir: dir,
        _store_owner: store_owner,
        state: Arc::new(state),
        operator,
        stopped,
    }
}

fn fingerprint(key: &SigningKey) -> String {
    lillux::signature::compute_fingerprint(&key.verifying_key())
}

fn stamp(offset_secs: i64) -> String {
    lillux::time::iso8601_from_unix_secs(
        lillux::time::unix_secs_now()
            .checked_add_signed(offset_secs)
            .unwrap(),
    )
}

impl Rig {
    fn root(&self) -> &std::path::Path {
        &self.state.config.app_root
    }

    /// A remote principal's grant that lets it sign the restart operation.
    fn grant(&self, key: &SigningKey) -> String {
        let fp = fingerprint(key);
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().as_bytes());
        let directory = authorized_keys_dir(self.root());
        reconcile_authorized_key_toml_scopes(
            &directory,
            &fp,
            &encoded,
            &[RESTART_SCOPE.to_owned()],
            "remote operator",
            "test",
            "2026-01-01T00:00:00Z",
            &GrantTerms::for_tests(),
            &self.state.identity,
            WildcardPolicy::Reject,
            false,
            Some(SITE),
            true,
        )
        .unwrap();
        lillux::sha256_hex(&std::fs::read(directory.join(format!("{fp}.toml"))).unwrap())
    }

    fn body(&self, signer: &SigningKey, grant: Option<String>) -> RequestBody {
        let facts = read_facts(self.root()).unwrap();
        let spec = lookup("node.allow-restart").unwrap();
        RequestBody {
            schema: REQUEST_SCHEMA.to_owned(),
            node_fingerprint: self.state.identity.fingerprint().to_owned(),
            signer_fingerprint: fingerprint(signer),
            signer_origin_site_id: grant.as_ref().map(|_| SITE.to_owned()),
            signer_grant_digest: grant,
            operation: "node.allow-restart".to_owned(),
            arguments: json!({}),
            inputs: BTreeMap::new(),
            expected_result: BTreeMap::new(),
            precondition: MANDATORY_FACTS
                .iter()
                .chain(spec.preconditions)
                .map(|fact| ((*fact).to_owned(), facts[*fact].clone()))
                .collect(),
            issued_at: stamp(-30),
            not_after: stamp(3600),
        }
    }

    /// The node's local configured operator, with a request it signed itself.
    fn operator_delivery(&self) -> (HandlerContext, SignedRequest) {
        let context = HandlerContext::new_with_authority(
            format!("fp:{}", fingerprint(&self.operator)),
            vec!["*".to_owned()],
            true,
            Some(AuthorizedKeyPrincipalClass::LocalClient),
            None,
        );
        let request = SignedRequest::sign(self.body(&self.operator, None), &self.operator).unwrap();
        (context, request)
    }

    /// A remote principal holding `scopes`, with a request it signed.
    fn principal_delivery(&self, scopes: &[&str]) -> (HandlerContext, SignedRequest) {
        let key = lillux::crypto::generate_signing_key();
        let grant = self.grant(&key);
        let context = HandlerContext::new_with_authority(
            format!("fp:{}", fingerprint(&key)),
            scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            true,
            Some(AuthorizedKeyPrincipalClass::RemotePrincipal),
            Some(SITE.to_owned()),
        );
        let request = SignedRequest::sign(self.body(&key, Some(grant)), &key).unwrap();
        (context, request)
    }

    async fn submit(
        &self,
        context: &HandlerContext,
        request: &SignedRequest,
        stop: Option<StopWhen>,
    ) -> Result<Value, HandlerError> {
        submit(
            SubmitRequest {
                request: request.clone(),
                inputs: BTreeMap::new(),
                stop,
            },
            context.clone(),
            Arc::clone(&self.state),
        )
        .await
    }

    fn queued(&self) -> usize {
        status_report(self.root(), &self.state.identity).unwrap()["queued"]
            .as_array()
            .unwrap()
            .len()
    }

    /// Whether the stop ran, once its delay has passed.
    async fn stop_ran(&self) -> bool {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        self.stopped.load(Ordering::SeqCst)
    }
}

fn assert_stop_scope_required(result: Result<Value, HandlerError>) {
    match result {
        Err(HandlerError::Structured { code, status, body }) => {
            assert_eq!(code, OFFLINE_STOP_SCOPE_REQUIRED);
            assert_eq!(status, 403);
            assert_eq!(body["code"], OFFLINE_STOP_SCOPE_REQUIRED);
            assert_eq!(body["required_scope"], OFFLINE_STOP_SCOPE);
        }
        other => panic!("expected offline_stop_scope_required, got {other:?}"),
    }
}

#[tokio::test]
async fn a_queue_only_key_cannot_stop_the_node_and_nothing_is_queued() {
    let rig = rig();
    let (context, request) = rig.principal_delivery(&[QUEUE_SCOPE]);
    assert_stop_scope_required(rig.submit(&context, &request, Some(StopWhen::Now)).await);
    assert_eq!(rig.queued(), 0);
    assert!(!rig.stop_ran().await);
}

#[tokio::test]
async fn a_wildcard_grant_does_not_carry_the_stop_scope() {
    let rig = rig();
    for wildcard in ["ryeos.execute.service.*", "ryeos.execute.*", "ryeos.*", "*"] {
        let (context, request) = rig.principal_delivery(&[wildcard, QUEUE_SCOPE]);
        assert_stop_scope_required(rig.submit(&context, &request, Some(StopWhen::Now)).await);
    }
    assert_eq!(rig.queued(), 0);
    assert!(!rig.stop_ran().await);
}

#[tokio::test]
async fn the_exact_stop_scope_queues_the_request_and_stops_the_node() {
    let rig = rig();
    let (context, request) = rig.principal_delivery(&[QUEUE_SCOPE, OFFLINE_STOP_SCOPE]);
    let result = rig
        .submit(&context, &request, Some(StopWhen::Now))
        .await
        .unwrap();
    assert_eq!(result["stop"], "requested");
    assert_eq!(rig.queued(), 1);
    assert!(rig.stop_ran().await);
}

#[tokio::test]
async fn a_queue_only_key_still_queues_without_a_stop() {
    let rig = rig();
    let (context, request) = rig.principal_delivery(&[QUEUE_SCOPE]);
    let result = rig.submit(&context, &request, None).await.unwrap();
    assert_eq!(result["stop"], "not_requested");
    assert_eq!(rig.queued(), 1);
    assert!(!rig.stop_ran().await);
}

#[tokio::test]
async fn the_local_configured_operator_stops_without_the_scope() {
    let rig = rig();
    let (context, request) = rig.operator_delivery();
    let result = rig
        .submit(&context, &request, Some(StopWhen::Now))
        .await
        .unwrap();
    assert_eq!(result["stop"], "requested");
    assert_eq!(rig.queued(), 1);
    assert!(rig.stop_ran().await);
}

#[tokio::test]
async fn resubmitting_a_queued_request_with_a_stop_is_refused_for_a_queue_only_key() {
    let rig = rig();
    let (context, request) = rig.principal_delivery(&[QUEUE_SCOPE]);
    rig.submit(&context, &request, None).await.unwrap();
    assert_eq!(rig.queued(), 1);
    assert_stop_scope_required(rig.submit(&context, &request, Some(StopWhen::Now)).await);
    assert_eq!(rig.queued(), 1);
    assert!(!rig.stop_ran().await);
}

#[tokio::test]
async fn a_forwarded_delivery_needs_the_stop_scope_in_both_grants() {
    let rig = rig();
    // The delegated scopes lack the stop scope.
    let (context, request) = rig.principal_delivery(&[QUEUE_SCOPE]);
    let forwarded = context
        .with_delegated_via(Some("site:forwarder".to_owned()))
        .with_forwarding_scopes(Some(vec![
            QUEUE_SCOPE.to_owned(),
            OFFLINE_STOP_SCOPE.to_owned(),
        ]));
    assert_stop_scope_required(rig.submit(&forwarded, &request, Some(StopWhen::Now)).await);

    // The delegated scopes hold it, the forwarding node's grant does not, or
    // its scopes are unknown.
    let (context, request) = rig.principal_delivery(&[QUEUE_SCOPE, OFFLINE_STOP_SCOPE]);
    let forwarded = context.with_delegated_via(Some("site:forwarder".to_owned()));
    for forwarder in [
        None,
        Some(vec![QUEUE_SCOPE.to_owned()]),
        Some(vec!["ryeos.execute.service.*".to_owned(), "*".to_owned()]),
    ] {
        let forwarded = forwarded.clone().with_forwarding_scopes(forwarder);
        assert_stop_scope_required(rig.submit(&forwarded, &request, Some(StopWhen::Now)).await);
    }
    assert_eq!(rig.queued(), 0);
    assert!(!rig.stop_ran().await);

    // Both hold it.
    let forwarded = forwarded.with_forwarding_scopes(Some(vec![
        QUEUE_SCOPE.to_owned(),
        OFFLINE_STOP_SCOPE.to_owned(),
    ]));
    let result = rig
        .submit(&forwarded, &request, Some(StopWhen::Now))
        .await
        .unwrap();
    assert_eq!(result["stop"], "requested");
    assert_eq!(rig.queued(), 1);
    assert!(rig.stop_ran().await);
}

#[test]
fn only_the_local_operator_can_write_the_stop_scope_into_a_grant() {
    assert!(
        ryeos_node_runtime::identity::reject_offline_grant_scopes(&[OFFLINE_STOP_SCOPE.to_owned()])
            .is_err()
    );
    // The queue scope and wildcards stay ordinary scopes for any writer.
    ryeos_node_runtime::identity::reject_offline_grant_scopes(&[
        QUEUE_SCOPE.to_owned(),
        "ryeos.execute.service.*".to_owned(),
    ])
    .unwrap();
    let key = lillux::crypto::generate_signing_key();
    let request = |scopes: &str| -> ryeos_api::handlers::authorize_client::Request {
        serde_json::from_value(json!({
            "public_key": base64::engine::general_purpose::STANDARD
                .encode(key.verifying_key().as_bytes()),
            "scopes": scopes,
            "origin_site_id": SITE,
        }))
        .unwrap()
    };
    let root = std::path::PathBuf::from("/nonexistent");
    assert!(
        request(OFFLINE_STOP_SCOPE)
            .into_params(root.clone())
            .is_err()
    );
    request(OFFLINE_STOP_SCOPE)
        .into_params_as_local_operator(root)
        .unwrap();
}

#[test]
fn a_delegation_may_lend_the_stop_scope() {
    // A delegation only lends part of its signer's own grant, so the scope
    // that cannot be granted to a non-operator can still be lent.
    ryeos_node_runtime::identity::reject_offline_scopes(&[OFFLINE_STOP_SCOPE.to_owned()]).unwrap();
}
