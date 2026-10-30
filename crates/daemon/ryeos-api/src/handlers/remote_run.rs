//! `remote/run` — execute an item against a configured remote project.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::handler_error::{HandlerError, HandlerResult};
use crate::handlers::remote_push::OutboundPrincipal;
use crate::registry::ServiceDescriptor;
use crate::remote::client::RemoteClient;
use crate::remote::config;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Remote name (default: "default").
    #[serde(default = "default_remote")]
    pub remote: String,
    /// Item to execute (canonical ref).
    pub item_ref: String,
    pub ref_bindings: BTreeMap<String, String>,
    /// Explicit destination-local witnesses, resolved only by the selected
    /// remote. This does not forward source-admitted product authority or
    /// transfer witnesses, qualifications, or consumer bindings.
    #[serde(default)]
    pub product_selections:
        ryeos_state::external_content::products::composition::ProductSelectionInputs,
    /// Local project path used to resolve the configured remote binding.
    /// Omit only for an explicitly projectless execution policy.
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// Principal used to sign the destination request. Retained-current-HEAD
    /// launches always preserve the configured operator; projectless session
    /// control selects it explicitly.
    #[serde(default)]
    pub outbound_principal: OutboundPrincipal,
    /// Parameters for the item.
    #[serde(default)]
    pub parameters: Value,
    /// Explicit execution semantics for the destination project.
    pub execution_policy: ryeos_app::execution_policy::ExecutionPolicy,
    /// Caller-retained remote request coordinate. Required exactly when the
    /// destination policy returns `accepted`, so uncertain delivery can be
    /// queried without risking a second launch.
    #[serde(default)]
    pub launch_id: Option<String>,
}

// Proposed only: a contact ceiling for one accepted submission.
// All accepted contacts share this existing control-plane ceiling. A protected
// caller also retains its original earlier budget; preparation cannot renew it.
const ACCEPTED_CONTACT_MAX: lillux::time::Duration = lillux::time::Duration::from_secs(30);

fn default_remote() -> String {
    "default".to_string()
}

pub async fn handle(
    mut req: Request,
    ctx: crate::handler_context::HandlerContext,
    state: Arc<AppState>,
) -> HandlerResult<Value> {
    let contact_deadline = remote_contact_deadline(&req, &ctx);
    authorize_execution_refs(&req.item_ref, &req.ref_bindings, &ctx, &state)?;
    req.product_selections = ryeos_state::external_content::products::composition::canonicalize_product_selection_inputs(req.product_selections)
        .map_err(|error| HandlerError::BadRequest(format!("invalid destination-local product_selections: {error}")))?;
    let report =
        config::load_remotes_layered_report(&state.config.app_root, req.project.as_deref())
            .map_err(|e| HandlerError::Internal(format!("load remotes: {e:#}")))?;
    let loaded_remote = config::get_loaded_remote(&report.remotes, &req.remote)
        .map_err(|e| HandlerError::BadRequest(format!("remote '{}': {e:#}", req.remote)))?;
    let binding = req
        .project
        .as_ref()
        .map(|project| config::resolve_loaded_project_binding(&loaded_remote, project))
        .transpose()
        .map_err(|e| HandlerError::BadRequest(format!("project binding: {e:#}")))?;
    let remote_cfg = loaded_remote.config;

    req.execution_policy
        .validate()
        .map_err(|error| HandlerError::BadRequest(error.to_string()))?;
    if req.execution_policy.target != ryeos_app::execution_policy::ExecutionTarget::Here {
        return Err(HandlerError::BadRequest(
            "remote run receives a destination-local policy; target must be `here`".to_string(),
        ));
    }
    let live_direct = matches!(
        &req.execution_policy.project,
        ryeos_app::execution_policy::ProjectExecutionPolicy::LiveDirect { .. }
    );
    let retained_current_head = matches!(
        &req.execution_policy.project,
        ryeos_app::execution_policy::ProjectExecutionPolicy::Pinned {
            source: ryeos_app::execution_policy::PinnedSource::CurrentHead,
            realization: ryeos_app::execution_policy::PinnedRealization::Cow {
                terminal_publication:
                    ryeos_app::execution_policy::TerminalPublication::RetainCurrentHead,
            },
            ..
        }
    );
    let projectless = matches!(
        &req.execution_policy.project,
        ryeos_app::execution_policy::ProjectExecutionPolicy::Projectless
    );
    if !live_direct && !retained_current_head && !projectless {
        return Err(HandlerError::BadRequest(
            "remote run requires projectless authority, live_direct authority, or a retained current_head COW launch".to_string(),
        ));
    }
    if projectless && req.project.is_some() {
        return Err(HandlerError::BadRequest(
            "projectless remote run must omit project".to_string(),
        ));
    }
    if !projectless && binding.is_none() {
        return Err(HandlerError::BadRequest(
            "project-backed remote run requires a project and configured binding".to_string(),
        ));
    }
    if retained_current_head
        && req.execution_policy.response != ryeos_app::execution_policy::ExecutionResponse::Accepted
    {
        return Err(HandlerError::BadRequest(
            "retained current_head remote launches must return accepted so the caller can drive the durable session"
                .to_string(),
        ));
    }
    if retained_current_head
        && binding.as_ref().map(|value| value.sync_scope)
            != Some(config::ProjectSyncScope::FullProject)
    {
        let binding = binding
            .as_ref()
            .expect("project-backed binding checked above");
        return Err(HandlerError::BadRequest(format!(
            "retained current_head remote launches require a full_project binding; '{}' is {:?}",
            binding.local_project_path.display(),
            binding.sync_scope
        )));
    }
    let use_configured_operator =
        retained_current_head || req.outbound_principal == OutboundPrincipal::ConfiguredOperator;
    let client = if use_configured_operator {
        RemoteClient::from_remote_cfg_as_configured_operator(&state, &remote_cfg, &ctx)
            .map_err(|error| HandlerError::Forbidden(error.to_string()))?
    } else {
        RemoteClient::from_remote_cfg(&state, &remote_cfg)
    };
    let accepted =
        req.execution_policy.response == ryeos_app::execution_policy::ExecutionResponse::Accepted;
    match (accepted, req.launch_id.as_deref()) {
        (true, Some(launch_id)) if ryeos_app::state_store::is_canonical_launch_id(launch_id) => {}
        (true, Some(_)) => {
            return Err(HandlerError::BadRequest(
                "accepted remote launch_id must be L- followed by exactly 32 hexadecimal characters"
                    .to_string(),
            ));
        }
        (true, None) => {
            return Err(HandlerError::BadRequest(
                "accepted remote launches require a caller-retained launch_id".to_string(),
            ));
        }
        (false, Some(_)) => {
            return Err(HandlerError::BadRequest(
                "launch_id is valid only for an accepted remote launch".to_string(),
            ));
        }
        (false, None) => {}
    }
    let remote_result = submit_remote_request(
        &client,
        &req,
        binding
            .as_ref()
            .map(|binding| binding.remote_project_path.as_str()),
        contact_deadline,
    )
        .await
        .map_err(|error| {
            let coordinate = remote_launch_coordinate(&req)
                .map(|launch_id| {
                    format!(
                        "; retain remote launch coordinate {launch_id} and query that exact owner-bound launch before retrying"
                    )
                })
                .unwrap_or_default();
            crate::remote::client::map_remote_call_error(
                error,
                format!("remote run failed{coordinate}"),
            )
        })?;

    Ok(serde_json::json!({
        "remote": req.remote,
        "outbound_principal": if use_configured_operator { "configured_operator" } else { "node" },
        "local_project_path": binding.as_ref().map(|value| &value.local_project_path),
        "remote_project_path": binding.as_ref().map(|value| &value.remote_project_path),
        "sync_scope": binding.as_ref().map(|value| value.sync_scope),
        "result": remote_result,
    }))
}

fn remote_contact_deadline(
    req: &Request,
    ctx: &crate::handler_context::HandlerContext,
) -> Option<lillux::time::MonotonicDeadline> {
    let original = ctx
        .original_execution_budget()
        .map(|budget| budget.contact_expiry());
    let contact = (req.execution_policy.response
        == ryeos_app::execution_policy::ExecutionResponse::Accepted)
        .then(|| lillux::time::MonotonicDeadline::after(ACCEPTED_CONTACT_MAX));
    match (original, contact) {
        (Some(original), Some(contact)) => Some(original.min(contact)),
        (original, contact) => original.or(contact),
    }
}

// Diagnostic retention only. The target's existing owner-bound service
// validates this coordinate; it is never local admission authority.
fn remote_launch_coordinate(req: &Request) -> Option<&str> {
    req.launch_id.as_deref().or_else(|| {
        matches!(
            req.item_ref.as_str(),
            "service:launch/status" | "service:launch/cancel"
        )
        .then(|| req.parameters.get("launch_id").and_then(Value::as_str))
        .flatten()
        .filter(|id| ryeos_app::state_store::is_canonical_launch_id(id))
    })
}

/// Use the normal signed request and its full result envelope. Timeouts and
/// late acknowledgements retain uncertainty at the same launch coordinate;
/// neither repeats contact nor settles the target's process or financial owner.
async fn submit_remote_request(
    client: &RemoteClient,
    req: &Request,
    project_path: Option<&str>,
    contact_deadline: Option<lillux::time::MonotonicDeadline>,
) -> anyhow::Result<Value> {
    let remaining = contact_deadline.map(|deadline| deadline.remaining());
    if remaining.is_some_and(|remaining| remaining.is_zero()) {
        anyhow::bail!("remote contact deadline expired before submission");
    }
    let result = if req.execution_policy.response
        == ryeos_app::execution_policy::ExecutionResponse::Accepted
    {
        let launch_id = req
            .launch_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("accepted remote submission requires launch_id"))?;
        let remaining = remaining
            .ok_or_else(|| anyhow::anyhow!("accepted remote submission lost contact ceiling"))?;
        client
            .execute_accepted_with_total_timeout(
                &req.item_ref,
                &req.ref_bindings,
                &req.product_selections,
                project_path,
                &req.parameters,
                &req.execution_policy,
                launch_id,
                remaining,
            )
            .await?
    } else {
        let call = client.execute(
            &req.item_ref,
            &req.ref_bindings,
            &req.product_selections,
            project_path,
            &req.parameters,
            &req.execution_policy,
            req.launch_id.as_deref(),
        );
        if let Some(remaining) = remaining {
            tokio::time::timeout(remaining, call).await.map_err(|_| {
                anyhow::anyhow!("protected remote wait exceeded original contact deadline")
            })??
        } else {
            call.await?
        }
    };
    if contact_deadline.is_some_and(|deadline| deadline.has_elapsed()) {
        anyhow::bail!(
            "remote response arrived after original contact deadline; retain launch uncertainty"
        );
    }
    Ok(result)
}

fn authorize_execution_refs(
    item_ref: &str,
    ref_bindings: &BTreeMap<String, String>,
    ctx: &crate::handler_context::HandlerContext,
    state: &AppState,
) -> HandlerResult<()> {
    ctx.require_verified()?;
    ryeos_executor::execution::launch_preparation::validate_ref_bindings(ref_bindings)
        .map_err(|error| HandlerError::BadRequest(format!("invalid ref_bindings: {error}")))?;
    for (label, value) in std::iter::once(("item_ref", item_ref)).chain(
        ref_bindings
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str())),
    ) {
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(value)
            .map_err(|error| HandlerError::BadRequest(format!("invalid {label}: {error}")))?;
        let required = ryeos_runtime::authorizer::canonical_cap(
            &canonical.kind,
            &canonical.bare_id,
            "execute",
        );
        let policy = ryeos_runtime::authorizer::AuthorizationPolicy::require(&required);
        state
            .authorizer
            .authorize(&ctx.scopes, &policy)
            .map_err(|_| {
                HandlerError::Forbidden(format!("missing required capability: {required}"))
            })?;
    }
    Ok(())
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:remote/run",
    endpoint: "remote.run",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.remote/admin"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await.map_err(Into::into)
        })
    },
};

#[cfg(test)]
mod tests {
    use super::Request;
    use super::{remote_contact_deadline, remote_launch_coordinate, submit_remote_request};
    use lillux::time::Duration;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy)]
    enum PeerMode {
        HeaderStall,
        BodyTrickle,
        EchoAfter(Duration),
    }

    #[derive(Clone)]
    struct PeerState {
        mode: PeerMode,
        requests: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
        stop: tokio::sync::watch::Receiver<bool>,
        producers: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    }

    struct Peer {
        address: std::net::SocketAddr,
        state: PeerState,
        stop: tokio::sync::watch::Sender<bool>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Peer {
        async fn start(mode: PeerMode) -> Self {
            async fn receive(
                axum::extract::State(state): axum::extract::State<PeerState>,
                headers: axum::http::HeaderMap,
                axum::Json(body): axum::Json<serde_json::Value>,
            ) -> axum::response::Response {
                use axum::response::IntoResponse;
                let key = headers
                    .get("x-ryeos-key-id")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                state.requests.lock().unwrap().push((key, body.clone()));
                let mut stop = state.stop.clone();
                match state.mode {
                    PeerMode::HeaderStall => {
                        if !*stop.borrow() {
                            let _ = stop.changed().await;
                        }
                    }
                    PeerMode::EchoAfter(delay) => {
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {},
                            _ = stop.changed() => {},
                        }
                    }
                    PeerMode::BodyTrickle => {
                        let (sender, receiver) =
                            tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(2);
                        let producer = tokio::spawn(async move {
                            if sender.send(Ok(b"{".to_vec())).await.is_err() {
                                return;
                            }
                            loop {
                                if *stop.borrow() {
                                    break;
                                }
                                tokio::select! {
                                    _ = stop.changed() => break,
                                    _ = tokio::time::sleep(Duration::from_millis(10)) => {
                                        if sender.send(Ok(b" ".to_vec())).await.is_err() { break; }
                                    }
                                }
                            }
                        });
                        state.producers.lock().unwrap().push(producer);
                        return axum::response::Response::builder()
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from_stream(
                                tokio_stream::wrappers::ReceiverStream::new(receiver),
                            ))
                            .unwrap();
                    }
                }
                axum::Json(serde_json::json!({
                    "status":"accepted", "launch_id":body["launch_id"],
                    "thread_id":"T-peer-fixture", "thread":{"thread_id":"T-peer-fixture"},
                    "result":{"request":body}, "accounting":{"fixture":true}
                }))
                .into_response()
            }
            let (stop, mut shutdown) = tokio::sync::watch::channel(false);
            let state = PeerState {
                mode,
                requests: Arc::new(Mutex::new(Vec::new())),
                stop: shutdown.clone(),
                producers: Arc::new(Mutex::new(Vec::new())),
            };
            let app = axum::Router::new()
                .route("/execute", axum::routing::post(receive))
                .route("/execute/launch", axum::routing::post(receive))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        if !*shutdown.borrow() {
                            let _ = shutdown.changed().await;
                        }
                    })
                    .await
                    .unwrap();
            });
            Self {
                address,
                state,
                stop,
                server,
            }
        }

        fn operator_client(
            &self,
            fixture: &ryeos_app::handler_context::budget_test_support::Fixture,
        ) -> crate::remote::client::RemoteClient {
            use base64::Engine as _;
            let remote = crate::remote::config::RemoteConfig {
                name: "fixture".into(),
                url: format!("http://{}", self.address),
                principal_id: fixture.state.identity.principal_id(),
                signing_key: format!(
                    "ed25519:{}",
                    base64::engine::general_purpose::STANDARD
                        .encode(fixture.state.identity.verifying_key().as_bytes())
                ),
                site_id: "site:fixture-peer".into(),
                vault_fingerprint: "fixture-vault".into(),
                ingest_ignore: ryeos_app::ignore::IgnoreConfig {
                    patterns: Vec::new(),
                },
                project_bindings: std::collections::HashMap::new(),
            };
            crate::remote::client::RemoteClient::from_remote_cfg_as_configured_operator(
                &fixture.state,
                &remote,
                &fixture.context,
            )
            .unwrap()
        }

        async fn settle(mut self) {
            self.stop.send(true).unwrap();
            if tokio::time::timeout(Duration::from_secs(1), &mut self.server)
                .await
                .is_err()
            {
                self.server.abort();
                let _ = self.server.await;
                panic!("finite mock peer failed to settle");
            }
            let producers = std::mem::take(&mut *self.state.producers.lock().unwrap());
            for producer in producers {
                producer.await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn all_accepted_contacts_share_original_expiry_and_one_launch() {
        for projectless in [true, false] {
            for mode in [PeerMode::HeaderStall, PeerMode::BodyTrickle] {
                let directory = tempfile::tempdir().unwrap();
                let fixture = ryeos_app::handler_context::budget_test_support::build(
                    directory.path(),
                    Duration::from_millis(250),
                )
                .unwrap();
                let peer = Peer::start(mode).await;
                let client = peer.operator_client(&fixture);
                let mut payload = retained_request();
                if projectless {
                    payload.as_object_mut().unwrap().remove("project");
                    payload["execution_policy"]["project"] =
                        serde_json::json!({"kind":"projectless"});
                }
                payload["product_selections"] = serde_json::json!([{
                    "target":{"kind":"content_dependency","binding":"model"},
                    "selection":{"declaration_id":"fixture-model","witness_hash":"ab".repeat(32),
                        "witness_source":{"kind":"local_capture"},"qualification_hash":null, "remote_testimony": null}
                }]);
                payload["outbound_principal"] = serde_json::json!("configured_operator");
                let request: Request = serde_json::from_value(payload).unwrap();
                let unscoped = crate::handler_context::HandlerContext::new(
                    "session:test".into(),
                    Vec::new(),
                    true,
                );
                assert!(
                    remote_contact_deadline(&request, &unscoped)
                        .unwrap()
                        .remaining()
                        <= Duration::from_secs(30)
                );
                let deadline = remote_contact_deadline(&request, &fixture.context).unwrap();
                tokio::time::sleep(Duration::from_millis(20)).await;
                assert!(deadline.remaining() < Duration::from_millis(250));
                let project = (!projectless).then_some("/target-project");
                let error = tokio::time::timeout(
                    Duration::from_secs(1),
                    submit_remote_request(&client, &request, project, Some(deadline)),
                )
                .await
                .unwrap()
                .unwrap_err();
                assert!(error.to_string().contains("timeout"));
                let sent = peer.state.requests.lock().unwrap().clone();
                assert_eq!(sent.len(), 1);
                assert_eq!(sent[0].0, fixture.context.fingerprint);
                assert_eq!(
                    sent[0].1["required_origin_site_id"],
                    fixture.state.threads.site_id()
                );
                assert_eq!(
                    sent[0].1["launch_id"],
                    request.launch_id.as_deref().unwrap()
                );
                assert_eq!(sent[0].1["parameters"], request.parameters);
                assert_eq!(
                    sent[0].1["product_selections"],
                    serde_json::to_value(&request.product_selections).unwrap()
                );
                assert_eq!(
                    sent[0].1["project_path"],
                    serde_json::to_value(project).unwrap()
                );
                assert_eq!(
                    sent[0].1["execution_policy"],
                    serde_json::to_value(&request.execution_policy).unwrap()
                );
                assert!(
                    submit_remote_request(&client, &request, project, Some(deadline))
                        .await
                        .is_err()
                );
                assert_eq!(
                    peer.state.requests.lock().unwrap().len(),
                    1,
                    "expired owner permits no second POST"
                );
                peer.settle().await;
            }
        }
    }

    #[tokio::test]
    async fn protected_status_cancel_wait_preserves_original_owner_and_envelope() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = ryeos_app::handler_context::budget_test_support::build(
            directory.path(),
            Duration::from_millis(500),
        )
        .unwrap();
        let mut payload = retained_request();
        payload.as_object_mut().unwrap().remove("project");
        let original_launch = payload
            .as_object_mut()
            .unwrap()
            .remove("launch_id")
            .unwrap();
        payload["outbound_principal"] = serde_json::json!("configured_operator");
        payload["execution_policy"]["response"] = serde_json::json!("wait");
        payload["execution_policy"]["project"] = serde_json::json!({"kind":"projectless"});
        payload["item_ref"] = serde_json::json!("service:launch/status");
        payload["parameters"] = serde_json::json!({"launch_id":original_launch});
        let mut request: Request = serde_json::from_value(payload.clone()).unwrap();
        let peer = Peer::start(PeerMode::EchoAfter(Duration::from_millis(60))).await;
        let internal = crate::handler_context::HandlerContext::new(
            fixture.context.fingerprint.clone(),
            Vec::new(),
            false,
        );
        assert!(
            super::handle(
                serde_json::from_value(payload).unwrap(),
                internal,
                Arc::new(fixture.state.clone())
            )
            .await
            .is_err()
        );
        assert_eq!(
            peer.state.requests.lock().unwrap().len(),
            0,
            "internal unverified service authority permits no target contact"
        );
        let client = peer.operator_client(&fixture);
        let original = fixture.context.original_execution_budget().unwrap().clone();
        let deadline = remote_contact_deadline(&request, &fixture.context).unwrap();
        let envelope = submit_remote_request(&client, &request, None, Some(deadline))
            .await
            .unwrap();
        assert_eq!(envelope["thread"]["thread_id"], "T-peer-fixture");
        assert_eq!(envelope["accounting"]["fixture"], true);
        assert_eq!(
            envelope["result"]["request"]["parameters"],
            request.parameters
        );
        assert_eq!(remote_launch_coordinate(&request), original_launch.as_str());
        let foreign =
            crate::handler_context::HandlerContext::new("session:other".into(), Vec::new(), true);
        assert!(
            foreign
                .with_original_execution_budget(Some(original.clone()))
                .is_err()
        );
        assert_eq!(peer.state.requests.lock().unwrap().len(), 1);
        peer.settle().await;

        request.item_ref = "service:launch/cancel".into();
        let stalled = Peer::start(PeerMode::HeaderStall).await;
        let client = stalled.operator_client(&fixture);
        let before_cancel = deadline.remaining();
        let cancel_deadline = remote_contact_deadline(&request, &fixture.context).unwrap();
        assert!(cancel_deadline.remaining() <= before_cancel);
        assert!(
            tokio::time::timeout(
                Duration::from_secs(1),
                submit_remote_request(&client, &request, None, Some(cancel_deadline))
            )
            .await
            .unwrap()
            .is_err()
        );
        assert_eq!(fixture.context.original_execution_budget(), Some(&original));
        assert!(original.remaining().is_zero());
        assert_eq!(stalled.state.requests.lock().unwrap().len(), 1);
        assert!(
            submit_remote_request(&client, &request, None, Some(cancel_deadline))
                .await
                .is_err()
        );
        assert_eq!(stalled.state.requests.lock().unwrap().len(), 1);
        stalled.settle().await;

        // An independently authorized ordinary Wait keeps its existing
        // contract and can inspect the same launch; it cannot renew old work.
        let unscoped =
            crate::handler_context::HandlerContext::new("session:test".into(), Vec::new(), true);
        assert!(remote_contact_deadline(&request, &unscoped).is_none());
        let peer = Peer::start(PeerMode::EchoAfter(Duration::from_millis(60))).await;
        let result = submit_remote_request(&peer.operator_client(&fixture), &request, None, None)
            .await
            .unwrap();
        assert_eq!(
            result["result"]["request"]["parameters"],
            request.parameters
        );
        assert_eq!(peer.state.requests.lock().unwrap().len(), 1);
        peer.settle().await;
        assert!(original.remaining().is_zero());
    }

    fn signed_remote_run_command() -> ryeos_runtime::CommandDef {
        let path = ryeos_engine::test_support::workspace_root()
            .join("bundles/core/.ai/node/commands/remote-run.yaml");
        let mut command: ryeos_runtime::CommandDef =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        command.name = "remote run".into();
        command.source_file = path;
        command
    }

    fn retained_request() -> serde_json::Value {
        serde_json::json!({
            "remote": "hosted",
            "item_ref": "worker_execution:fixture/session",
            "ref_bindings": {},
            "project": "/project",
            "parameters": {"credential_profile_id": "personal"},
            "launch_id": "L-0123456789abcdef0123456789abcdef",
            "execution_policy": {
                "schema_version": 2,
                "ownership": "daemon_owned",
                "recovery": "restart_recoverable",
                "response": "accepted",
                "target": {"kind": "here"},
                "environment": {"kind": "none"},
                "project": {
                    "kind": "pinned",
                    "source": {"kind": "current_head"},
                    "realization": {
                        "kind": "cow",
                        "terminal_publication": {"kind": "retain_current_head"}
                    },
                    "child_policy": {"kind": "inherit"}
                }
            }
        })
    }

    #[test]
    fn signed_service_payload_requires_policy_and_carries_launch_coordinate() {
        let request: Request = serde_json::from_value(retained_request()).unwrap();
        assert!(request.product_selections.is_empty());
        assert_eq!(
            request.launch_id.as_deref(),
            Some("L-0123456789abcdef0123456789abcdef")
        );
        assert_eq!(
            request.execution_policy.response,
            ryeos_app::execution_policy::ExecutionResponse::Accepted
        );

        let mut missing = retained_request();
        missing.as_object_mut().unwrap().remove("execution_policy");
        assert!(serde_json::from_value::<Request>(missing).is_err());
    }

    #[test]
    fn destination_product_selectors_use_the_existing_bounded_contract() {
        use ryeos_state::external_content::products::composition::{
            MAX_PRODUCT_SELECTIONS, canonicalize_product_selection_inputs,
        };
        let selection = |id: &str| {
            serde_json::json!({
                "target": {"kind": "content_dependency", "binding": "environment"},
                "selection": {
                    "declaration_id": id,
                    "witness_hash": "ab".repeat(32),
                    "witness_source": {"kind": "local_capture"},
                    "qualification_hash": null, "remote_testimony": null
                }
            })
        };
        let mut payload = retained_request();
        payload["product_selections"] = serde_json::json!([selection("z"), selection("a")]);
        let request: Request = serde_json::from_value(payload.clone()).unwrap();
        let canonical = canonicalize_product_selection_inputs(request.product_selections).unwrap();
        assert_eq!(canonical[0].selection.declaration_id, "a");

        payload["product_selections"] = serde_json::json!([selection("a"), selection("a")]);
        let request: Request = serde_json::from_value(payload.clone()).unwrap();
        assert!(canonicalize_product_selection_inputs(request.product_selections).is_err());

        payload["product_selections"] = serde_json::json!(
            (0..=MAX_PRODUCT_SELECTIONS)
                .map(|i| selection(&format!("input-{i}")))
                .collect::<Vec<_>>()
        );
        let request: Request = serde_json::from_value(payload.clone()).unwrap();
        assert!(canonicalize_product_selection_inputs(request.product_selections).is_err());

        payload["product_selections"] = serde_json::json!([selection("a")]);
        payload["product_selections"][0]["selection"]
            .as_object_mut()
            .unwrap()
            .remove("qualification_hash");
        assert!(serde_json::from_value::<Request>(payload).is_err());
    }

    #[test]
    fn signed_remote_run_service_exposes_destination_selectors() {
        let path = ryeos_engine::test_support::workspace_root()
            .join("bundles/core/.ai/services/remote/run.yaml");
        let service: serde_json::Value =
            serde_yaml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(service["schema"]["product_selections"], "array?");
        assert_eq!(service["result_retention"], "digest_only");
    }

    #[test]
    fn signed_remote_run_default_is_a_valid_typed_live_policy() {
        let project = tempfile::tempdir().unwrap();
        let command = signed_remote_run_command();
        let tail = vec!["hosted".to_string(), "service:node/status".to_string()];
        let mut parameters =
            ryeos_runtime::arg_binder::bind_argv_with_command(&tail, Some(&command)).unwrap();
        let outer = ryeos_app::command_invocation::apply_project_policy(
            &command,
            &mut parameters,
            Some(project.path()),
            project.path(),
        )
        .unwrap();
        assert!(outer.is_none());
        let request: Request = serde_json::from_value(parameters).unwrap();
        request.execution_policy.validate().unwrap();
        assert!(matches!(
            request.execution_policy.project,
            ryeos_app::execution_policy::ProjectExecutionPolicy::LiveDirect { .. }
        ));
        assert_eq!(request.project.as_deref(), Some(project.path()));
    }

    #[test]
    fn signed_remote_run_preserves_explicit_destination_policies() {
        let command = signed_remote_run_command();
        for policy in [
            serde_json::json!({
                "schema_version": 2,
                "ownership": "daemon_owned",
                "recovery": "restart_recoverable",
                "response": "wait",
                "target": {"kind": "here"},
                "environment": {"kind": "none"},
                "project": {"kind": "projectless"}
            }),
            serde_json::json!({
                "schema_version": 2,
                "ownership": "daemon_owned",
                "recovery": "restart_recoverable",
                "response": "wait",
                "target": {"kind": "here"},
                "environment": {
                    "kind": "project_overlay",
                    "include_operator_vault": false,
                    "name_policy": {"kind": "declared_required"}
                },
                "project": {
                    "kind": "live_direct",
                    "access": "read_only",
                    "child_policy": {"kind": "inherit"}
                }
            }),
            retained_request()["execution_policy"].clone(),
        ] {
            let expected = policy.clone();
            let mut parameters = ryeos_runtime::arg_binder::bind_argv_with_command_and_overlay(
                &["service:node/status".into(), "--no-project".into()],
                Some(&command),
                &serde_json::json!({"execution_policy": policy}),
            )
            .unwrap();
            ryeos_app::command_invocation::apply_project_policy(
                &command,
                &mut parameters,
                None,
                ryeos_engine::test_support::workspace_root().as_path(),
            )
            .unwrap();
            assert_eq!(parameters["execution_policy"], expected);
            let request: Request = serde_json::from_value(parameters).unwrap();
            request.execution_policy.validate().unwrap();
        }
    }

    #[test]
    fn projectless_control_request_selects_configured_operator_without_project() {
        let request: Request = serde_json::from_value(serde_json::json!({
            "remote": "hosted",
            "item_ref": "service:worker-executions/status",
            "ref_bindings": {},
            "outbound_principal": "configured_operator",
            "parameters": {"session_id": "T-session"},
            "execution_policy": {
                "schema_version": 2,
                "ownership": "daemon_owned",
                "recovery": "restart_recoverable",
                "response": "wait",
                "target": {"kind": "here"},
                "environment": {"kind": "none"},
                "project": {"kind": "projectless"}
            }
        }))
        .unwrap();
        assert!(request.project.is_none());
        assert_eq!(
            request.outbound_principal,
            crate::handlers::remote_push::OutboundPrincipal::ConfiguredOperator
        );
        assert!(matches!(
            request.execution_policy.project,
            ryeos_app::execution_policy::ProjectExecutionPolicy::Projectless
        ));
    }
}
