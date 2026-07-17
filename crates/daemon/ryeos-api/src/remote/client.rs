//! Outbound HTTP client for communicating with remote ryEOS nodes.
//!
//! Wraps `reqwest` with request signing and provides typed methods
//! for the remote API surface: CAS operations, push-head, execute,
//! public-key discovery, etc.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine;
use lillux::crypto::Verifier;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

use ryeos_app::identity::NodeIdentity;
use ryeos_app::state::AppState;
use ryeos_state::ignore::IgnoreConfig;

const HASH_REQUEST_BODY_BUDGET_BYTES: usize = 900 * 1024;

/// HTTP error from a remote-node call, carrying the status code and the **full
/// request URL** so callers can react to a specific failure — and so an operator
/// can see *which host* answered. A bare path hides the most common cause of a
/// confusing 404: the remote URL in config points at the wrong node, so the
/// route truly isn't there at that URL even though it exists on the real node.
#[derive(Debug)]
pub struct RemoteHttpError {
    pub method: &'static str,
    /// Full URL hit (base_url + path), e.g. `https://node.example.com/project/apply-snapshot`.
    pub url: String,
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl std::fmt::Display for RemoteHttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {} returned {}: {}",
            self.method, self.url, self.status, self.body
        )
    }
}

impl std::error::Error for RemoteHttpError {}

/// Cap on the response-body excerpt carried in a [`RemoteHttpError`]. A
/// wrong-host / proxy / CDN error can return an arbitrarily large HTML page;
/// keep enough to diagnose without bloating logs and error chains.
const ERROR_BODY_EXCERPT_MAX: usize = 16 * 1024;
const DEFAULT_JSON_RESPONSE_MAX_BYTES: usize = 64 * 1024 * 1024;

fn truncate_error_body(body: &str) -> String {
    if body.len() <= ERROR_BODY_EXCERPT_MAX {
        return body.to_string();
    }
    let mut end = ERROR_BODY_EXCERPT_MAX;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated, {} bytes total]", &body[..end], body.len())
}

/// Read a remote response and map any non-success status to a
/// [`RemoteHttpError`] carrying the raw body excerpt. JSON is parsed only for
/// success responses: a wrong-host / proxy / CDN error commonly returns HTML
/// or plain text, and parsing that as JSON first would bury the real failure
/// under a generic "failed to parse response" error.
async fn read_json_checked(
    method: &'static str,
    url: &str,
    resp: reqwest::Response,
) -> Result<Value> {
    read_json_checked_with_limit(method, url, resp, DEFAULT_JSON_RESPONSE_MAX_BYTES).await
}

/// Stream a response into a bounded buffer. Successful JSON responses fail
/// closed at `max_response_bytes`; error responses retain only a diagnostic
/// excerpt so a remote cannot force an unbounded allocation on either path.
async fn read_json_checked_with_limit(
    method: &'static str,
    url: &str,
    mut resp: reqwest::Response,
    max_response_bytes: usize,
) -> Result<Value> {
    if max_response_bytes == 0 {
        bail!("{method} {url}: response byte limit must be greater than zero");
    }
    let status = resp.status();
    let success = status.is_success();
    let body_limit = if success {
        max_response_bytes
    } else {
        ERROR_BODY_EXCERPT_MAX
    };
    if success
        && resp
            .content_length()
            .is_some_and(|length| length > max_response_bytes as u64)
    {
        bail!(
            "{method} {url}: response Content-Length exceeds {} byte limit",
            max_response_bytes
        );
    }

    let mut body = Vec::with_capacity(
        resp.content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(body_limit),
    );
    let mut truncated = false;
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("{method} {url}: failed to read response body"))?
    {
        let remaining = body_limit.saturating_sub(body.len());
        if chunk.len() > remaining {
            if success {
                bail!(
                    "{method} {url}: response body exceeds {} byte limit",
                    max_response_bytes
                );
            }
            body.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        body.extend_from_slice(&chunk);
    }

    let body_text = String::from_utf8_lossy(&body);
    if !status.is_success() {
        let body = if truncated {
            format!(
                "{}… [truncated after {} bytes]",
                body_text, ERROR_BODY_EXCERPT_MAX
            )
        } else {
            truncate_error_body(&body_text)
        };
        return Err(RemoteHttpError {
            method,
            url: url.to_string(),
            status,
            body,
        }
        .into());
    }
    serde_json::from_slice(&body)
        .with_context(|| format!("{method} {url}: failed to parse response as JSON"))
}

/// Cap on establishing a TCP/TLS connection to a remote. Applies to
/// every request; without it a dead or filtered remote hangs callers
/// for the OS connect timeout (minutes).
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Total-request cap for control-plane calls (health, discovery, listings,
/// signed GETs). Deliberately not applied to generic `signed_post`: execution
/// and general CAS APIs may legitimately run longer. Bounded bundle transfers
/// use their own explicit timeout below.
const CONTROL_PLANE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Total wall-clock cap for one bundle export or bounded bundle-object batch.
///
/// General signed POSTs include long-running execution calls and intentionally
/// remain uncapped here. Bundle transfer calls have independently bounded
/// payloads, so they can also carry a finite liveness bound without changing
/// the execution API's semantics.
const BUNDLE_TRANSFER_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Typed client for a remote ryEOS node.
pub struct RemoteClient {
    /// Base URL (e.g. `https://ryeos.example.com`).
    base_url: String,
    /// Audience for request signing (remote's principal_id).
    audience: String,
    /// HTTP client.
    http: reqwest::Client,
    /// This node's identity for signing outbound requests.
    identity: Arc<NodeIdentity>,
}

impl RemoteClient {
    pub fn new(base_url: &str, audience: &str, identity: Arc<NodeIdentity>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            audience: audience.to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                // Signed requests must never auto-follow redirects: a
                // 301/302/303 can downgrade a POST to GET, but the signature is
                // bound to method/path/body. With redirects off a 3xx surfaces
                // as an error instead. Same invariant as the CLI/terminal
                // `signed_client`.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("failed to build remote signed HTTP client"),
            identity,
        }
    }

    /// The base URL this client talks to.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Build a client from a named remote config + app state.
    ///
    /// Looks up `remote_name` in the layered remotes config
    /// (project overrides user). Pass `Some(project_path)` when
    /// the caller is acting in the context of a local project so
    /// project-level remote definitions are honored.
    pub fn from_named_remote(
        state: &AppState,
        remote_name: &str,
        project_path: Option<&std::path::Path>,
    ) -> Result<Self> {
        let remotes = super::config::load_remotes_layered(&state.config.app_root, project_path)?;
        let remote = super::config::get_remote(&remotes, remote_name)?;
        Ok(Self::from_remote_cfg(state, &remote))
    }

    /// Build a client from an already-resolved [`RemoteConfig`].
    /// Callers that need project layering should resolve via
    /// [`super::config::load_remotes_layered`] first.
    pub fn from_remote_cfg(state: &AppState, remote: &super::config::RemoteConfig) -> Self {
        Self::new(&remote.url, &remote.principal_id, state.identity.clone())
    }

    /// GET /public-key (no auth required).
    pub async fn get_public_key(&self) -> Result<PublicKeyResponse> {
        let url = format!("{}/public-key", self.base_url);
        let resp = self
            .http
            .get(&url)
            .timeout(CONTROL_PLANE_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        let body = read_json_checked("GET", &url, resp).await?;
        Ok(PublicKeyResponse {
            principal_id: body["principal_id"]
                .as_str()
                .context("missing principal_id in /public-key response")?
                .to_string(),
            fingerprint: body["fingerprint"]
                .as_str()
                .context("missing fingerprint in /public-key response")?
                .to_string(),
            signing_key: body["signing_key"]
                .as_str()
                .context("missing signing_key in /public-key response; re-run remote configure against an updated remote daemon")?
                .to_string(),
            vault_fingerprint: body["vault_fingerprint"]
                .as_str()
                .context("missing vault_fingerprint in /public-key response")?
                .to_string(),
            site_id: body["site_id"]
                .as_str()
                .context("missing site_id in /public-key response; re-run remote configure against an updated remote daemon")?
                .to_string(),
        })
    }

    /// GET /health (no auth required).
    pub async fn get_health(&self) -> Result<Value> {
        let url = format!("{}/health", self.base_url);
        let resp = self
            .http
            .get(&url)
            .timeout(CONTROL_PLANE_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        read_json_checked("GET", &url, resp).await
    }

    /// GET /ingest-ignore (no auth required).
    ///
    /// Returns the remote node's ingest ignore config as a typed struct.
    pub async fn get_ingest_ignore(&self) -> Result<IgnoreConfig> {
        let url = format!("{}/ingest-ignore", self.base_url);
        let resp = self
            .http
            .get(&url)
            .timeout(CONTROL_PLANE_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        let body = read_json_checked("GET", &url, resp).await?;
        // Server returns the IgnoreConfig as JSON (with `patterns` field)
        let config: IgnoreConfig = serde_json::from_value(body)
            .context("failed to parse /ingest-ignore response as IgnoreConfig")?;
        Ok(config)
    }

    /// POST /objects/has (authenticated).
    ///
    /// Object and blob digests stay typed throughout the protocol. A digest in
    /// one namespace never satisfies a request for the other namespace.
    pub async fn objects_has(
        &self,
        object_hashes: &[String],
        blob_hashes: &[String],
    ) -> Result<ObjectsHasResponse> {
        if typed_hashes_request_body_size(object_hashes, blob_hashes)
            <= HASH_REQUEST_BODY_BUDGET_BYTES
        {
            return self.objects_has_once(object_hashes, blob_hashes).await;
        }

        let mut response = ObjectsHasResponse::default();
        for chunk in
            chunk_typed_hashes_for_body_budget(object_hashes, HASH_REQUEST_BODY_BUDGET_BYTES)
        {
            let part = self.objects_has_once(&chunk, &[]).await?;
            response
                .found_object_hashes
                .extend(part.found_object_hashes);
            response
                .missing_object_hashes
                .extend(part.missing_object_hashes);
        }
        for chunk in chunk_typed_hashes_for_body_budget(blob_hashes, HASH_REQUEST_BODY_BUDGET_BYTES)
        {
            let part = self.objects_has_once(&[], &chunk).await?;
            response.found_blob_hashes.extend(part.found_blob_hashes);
            response
                .missing_blob_hashes
                .extend(part.missing_blob_hashes);
        }
        response.validate_against_request(object_hashes, blob_hashes)?;
        Ok(response)
    }

    async fn objects_has_once(
        &self,
        object_hashes: &[String],
        blob_hashes: &[String],
    ) -> Result<ObjectsHasResponse> {
        let body = serde_json::json!({
            "object_hashes": object_hashes,
            "blob_hashes": blob_hashes,
        });
        let resp = self.signed_post("/objects/has", &body).await?;
        let response: ObjectsHasResponse =
            serde_json::from_value(resp).context("failed to decode typed objects/has response")?;
        response.validate_against_request(object_hashes, blob_hashes)?;
        Ok(response)
    }

    /// POST /objects/put (authenticated).
    ///
    /// Blobs are base64-encoded raw bytes. Objects must be wrapped in
    /// `{ "value": ... }` per the server's `ObjectEntry` schema.
    pub async fn objects_put(
        &self,
        staging_id: Option<&str>,
        project_path: &str,
        blobs: &[BlobUpload],
        objects: &[Value],
    ) -> Result<ObjectsPutResponse> {
        // Server expects objects as [{ "value": {...} }, ...]
        let wrapped_objects: Vec<Value> = objects
            .iter()
            .map(|obj| serde_json::json!({ "value": obj }))
            .collect();
        let body = serde_json::json!({
            "staging_id": staging_id,
            "project_path": project_path,
            "blobs": blobs,
            "objects": wrapped_objects,
        });
        let resp = self.signed_post("/objects/put", &body).await?;
        Ok(ObjectsPutResponse {
            staging_id: resp["staging_id"]
                .as_str()
                .context("objects/put response missing staging_id")?
                .to_string(),
            expected_previous_hash: match resp.get("expected_previous_hash") {
                Some(Value::String(hash)) => Some(hash.clone()),
                Some(Value::Null) => None,
                _ => anyhow::bail!("objects/put response missing nullable expected_previous_hash"),
            },
            blob_hashes: resp["blob_hashes"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            object_hashes: resp["object_hashes"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// POST /objects/get (authenticated).
    ///
    /// Server returns `{ "entries": [{ "hash": "...", "kind": "blob"|"object"|"missing", ... }] }`.
    pub async fn objects_get(&self, hashes: &[String]) -> Result<ObjectsGetResponse> {
        self.objects_get_with_response_limit(hashes, DEFAULT_JSON_RESPONSE_MAX_BYTES)
            .await
    }

    /// Fetch CAS entries while bounding each HTTP response body. Callers that
    /// know the declared object sizes should batch requests and choose a limit
    /// that includes base64/JSON overhead without allowing unbounded buffering.
    pub async fn objects_get_with_response_limit(
        &self,
        hashes: &[String],
        max_response_bytes: usize,
    ) -> Result<ObjectsGetResponse> {
        self.objects_get_with_response_limit_inner(hashes, max_response_bytes, None)
            .await
    }

    /// Fetch one or more bounded object batches for bundle transfer. Every
    /// underlying HTTP request has both a response-byte limit and a total
    /// wall-clock timeout; generic CAS callers retain their existing behavior.
    pub async fn objects_get_bundle_batch_with_response_limit(
        &self,
        hashes: &[String],
        max_response_bytes: usize,
    ) -> Result<ObjectsGetResponse> {
        self.objects_get_with_response_limit_inner(
            hashes,
            max_response_bytes,
            Some(BUNDLE_TRANSFER_REQUEST_TIMEOUT),
        )
        .await
    }

    async fn objects_get_with_response_limit_inner(
        &self,
        hashes: &[String],
        max_response_bytes: usize,
        total_timeout: Option<std::time::Duration>,
    ) -> Result<ObjectsGetResponse> {
        if hashes_request_body_size(hashes) > HASH_REQUEST_BODY_BUDGET_BYTES {
            let mut entries = Vec::new();
            for chunk in chunk_hashes_for_body_budget(hashes, HASH_REQUEST_BODY_BUDGET_BYTES) {
                entries.extend(
                    self.objects_get_once(&chunk, max_response_bytes, total_timeout)
                        .await?
                        .entries,
                );
            }
            return Ok(ObjectsGetResponse { entries });
        }
        self.objects_get_once(hashes, max_response_bytes, total_timeout)
            .await
    }

    async fn objects_get_once(
        &self,
        hashes: &[String],
        max_response_bytes: usize,
        total_timeout: Option<std::time::Duration>,
    ) -> Result<ObjectsGetResponse> {
        let body = serde_json::json!({ "hashes": hashes });
        let resp = match total_timeout {
            Some(timeout) => {
                self.signed_post_with_response_limit_and_timeout(
                    "/objects/get",
                    &body,
                    max_response_bytes,
                    timeout,
                )
                .await?
            }
            None => {
                self.signed_post_with_response_limit("/objects/get", &body, max_response_bytes)
                    .await?
            }
        };
        let entries = resp
            .get("entries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                let hash = entry.get("hash")?.as_str()?.to_string();
                let kind = entry.get("kind")?.as_str()?.to_string();
                Some(CasEntry {
                    hash,
                    kind,
                    data: entry.get("data").and_then(|v| v.as_str()).map(String::from),
                    value: entry.get("value").cloned(),
                })
            })
            .collect();
        let response = ObjectsGetResponse { entries };
        response.validate_against_request(hashes)?;
        Ok(response)
    }

    /// POST /objects/closure/describe (authenticated).
    pub async fn objects_closure_describe(
        &self,
        roots: &[String],
        options: ObjectsClosureRequestOptions,
    ) -> Result<ObjectsClosureDescribeResponse> {
        let body = closure_request_body(roots, &options);
        let resp = self.signed_post("/objects/closure/describe", &body).await?;
        let response: ObjectsClosureDescribeResponse = serde_json::from_value(resp)
            .context("failed to parse objects/closure/describe response")?;
        response.validate_against_request(roots)?;
        Ok(response)
    }

    /// POST /objects/closure/get (authenticated).
    pub async fn objects_closure_get(
        &self,
        roots: &[String],
        options: ObjectsClosureRequestOptions,
    ) -> Result<ObjectsClosureGetResponse> {
        let body = closure_request_body(roots, &options);
        let resp = self.signed_post("/objects/closure/get", &body).await?;
        let response: ObjectsClosureGetResponse =
            serde_json::from_value(resp).context("failed to parse objects/closure/get response")?;
        response.validate_against_request(roots)?;
        Ok(response)
    }

    /// POST /admission/submit (authenticated).
    pub async fn admission_submit(
        &self,
        subject_hash: &str,
        policy: &str,
        options: AdmissionSubmitOptions,
    ) -> Result<AdmissionSubmitResponse> {
        let mut body = serde_json::json!({
            "subject_hash": subject_hash,
            "policy": policy,
        });
        if let Some(claim) = options.claim {
            body["claim"] = serde_json::json!(claim);
        }
        if let Some(limit) = options.max_objects {
            body["max_objects"] = serde_json::json!(limit);
        }
        if let Some(limit) = options.max_blobs {
            body["max_blobs"] = serde_json::json!(limit);
        }
        if let Some(limit) = options.max_object_bytes {
            body["max_object_bytes"] = serde_json::json!(limit);
        }
        if let Some(limit) = options.max_blob_bytes {
            body["max_blob_bytes"] = serde_json::json!(limit);
        }
        if let Some(limit) = options.max_total_blob_bytes {
            body["max_total_blob_bytes"] = serde_json::json!(limit);
        }
        if let Some(limit) = options.max_links_per_object {
            body["max_links_per_object"] = serde_json::json!(limit);
        }
        let resp = self.signed_post("/admission/submit", &body).await?;
        let response: AdmissionSubmitResponse =
            serde_json::from_value(resp).context("failed to parse admission/submit response")?;
        response.validate_against_request(subject_hash, policy)?;
        Ok(response)
    }

    /// POST /admission/status (authenticated).
    pub async fn admission_status(
        &self,
        subject_hash: &str,
        policy: &str,
    ) -> Result<AdmissionStatusResponse> {
        let body = serde_json::json!({
            "subject_hash": subject_hash,
            "policy": policy,
        });
        let resp = self.signed_post("/admission/status", &body).await?;
        let response: AdmissionStatusResponse =
            serde_json::from_value(resp).context("failed to parse admission/status response")?;
        response.validate_against_request(subject_hash, policy)?;
        Ok(response)
    }

    /// POST /admission/attestations-for-subject (authenticated).
    pub async fn admission_attestations_for_subject(
        &self,
        subject_hash: &str,
        policy: Option<&str>,
    ) -> Result<AdmissionAttestationsForSubjectResponse> {
        let mut body = serde_json::json!({ "subject_hash": subject_hash });
        if let Some(policy) = policy {
            body["policy"] = serde_json::json!(policy);
        }
        let resp = self
            .signed_post("/admission/attestations-for-subject", &body)
            .await?;
        let response: AdmissionAttestationsForSubjectResponse = serde_json::from_value(resp)
            .context("failed to parse admission/attestations-for-subject response")?;
        response.validate_against_request(subject_hash, policy)?;
        Ok(response)
    }

    /// POST /federation/capabilities (authenticated identity handshake).
    pub async fn federation_capabilities(&self) -> Result<FederationCapabilitiesResponse> {
        let resp = self
            .signed_post("/federation/capabilities", &serde_json::json!({}))
            .await?;
        let response: FederationCapabilitiesResponse = serde_json::from_value(resp)
            .context("failed to parse federation/capabilities response")?;
        response.validate()?;
        Ok(response)
    }

    /// POST /federation/heads/list (authenticated).
    pub async fn federation_heads_list(
        &self,
        prefix: &str,
        limit: Option<usize>,
        expected_signer: &str,
        verifying_key: &lillux::crypto::VerifyingKey,
    ) -> Result<FederationHeadsListResponse> {
        let mut body = serde_json::json!({ "prefix": prefix });
        if let Some(limit) = limit {
            body["limit"] = serde_json::json!(limit);
        }
        let resp = self.signed_post("/federation/heads/list", &body).await?;
        let response: FederationHeadsListResponse = serde_json::from_value(resp)
            .context("failed to parse federation/heads/list response")?;
        response.validate(prefix)?;
        response.verify_signed_refs(expected_signer, verifying_key)?;
        Ok(response)
    }

    /// POST /sync/jobs/list (authenticated).
    pub async fn sync_jobs_list(
        &self,
        state: Option<&str>,
        limit: Option<usize>,
    ) -> Result<SyncJobsListResponse> {
        let mut body = serde_json::json!({});
        if let Some(state) = state {
            body["state"] = serde_json::json!(state);
        }
        if let Some(limit) = limit {
            body["limit"] = serde_json::json!(limit);
        }
        let resp = self.signed_post("/sync/jobs/list", &body).await?;
        let response: SyncJobsListResponse =
            serde_json::from_value(resp).context("failed to parse sync/jobs/list response")?;
        response.validate(state)?;
        Ok(response)
    }

    /// POST /sync/jobs/inspect (authenticated).
    pub async fn sync_jobs_inspect(&self, job_id: &str) -> Result<SyncJobsInspectResponse> {
        let body = serde_json::json!({ "job_id": job_id });
        let resp = self.signed_post("/sync/jobs/inspect", &body).await?;
        let response: SyncJobsInspectResponse =
            serde_json::from_value(resp).context("failed to parse sync/jobs/inspect response")?;
        response.validate_against_request(job_id)?;
        Ok(response)
    }

    /// POST /push-head (authenticated).
    pub async fn push_head(
        &self,
        project_path: &str,
        snapshot_hash: &str,
        staging_id: &str,
        expected_previous_hash: Option<&str>,
    ) -> Result<Value> {
        let body = serde_json::json!({
            "project_path": project_path,
            "snapshot_hash": snapshot_hash,
            "staging_id": staging_id,
            "expected_previous_hash": expected_previous_hash,
        });
        self.signed_post("/push-head", &body).await
    }

    /// POST /project/apply-snapshot (authenticated).
    pub async fn apply_project_snapshot(
        &self,
        project_path: &str,
        snapshot_hash: &str,
        expected_deployed_hash: Option<&str>,
    ) -> Result<Value> {
        let body = serde_json::json!({
            "project_path": project_path,
            "snapshot_hash": snapshot_hash,
            "expected_deployed_hash": expected_deployed_hash,
        });
        self.signed_post("/project/apply-snapshot", &body).await
    }

    /// POST /project/status (authenticated).
    pub async fn project_status(&self, project_path: &str) -> Result<Value> {
        let body = serde_json::json!({ "project_path": project_path });
        self.signed_post("/project/status", &body).await
    }

    /// POST /execute (authenticated).
    pub async fn execute(
        &self,
        item_ref: &str,
        ref_bindings: &BTreeMap<String, String>,
        project_path: &str,
        parameters: &Value,
        project_source: &str,
    ) -> Result<Value> {
        let body = serde_json::json!({
            "item_ref": item_ref,
            "ref_bindings": ref_bindings,
            "project_path": project_path,
            "parameters": parameters,
            "project_source": { "kind": project_source },
        });
        self.signed_post("/execute", &body).await
    }

    /// POST /execute with a full method call (`call.method`, `call.args`).
    ///
    /// This is the extended variant used by remote method forwarding,
    /// which needs to forward the `call` block that the basic `execute()`
    /// method does not support.
    ///
    /// Used by the shared unary forward helper. Note: `target_site_id`
    /// is intentionally **not** forwarded to prevent forwarding loops —
    /// if site A forwards to site B, B should execute locally, not
    /// attempt to forward again.
    pub async fn execute_with_options(
        &self,
        item_ref: &str,
        ref_bindings: &BTreeMap<String, String>,
        project_path: &str,
        parameters: &Value,
        project_source: &str,
        call: Option<&ryeos_engine::method_call::MethodCall>,
    ) -> Result<Value> {
        let mut body = serde_json::json!({
            "item_ref": item_ref,
            "ref_bindings": ref_bindings,
            "project_path": project_path,
            "parameters": parameters,
            "project_source": { "kind": project_source },
        });
        // Forward the `call` block only when it carries intent.
        if let Some(call) = call.filter(|c| !c.is_empty()) {
            body["call"] = serde_json::to_value(call)
                .map_err(|e| anyhow::anyhow!("failed to serialize call block: {e}"))?;
        }
        self.signed_post("/execute", &body).await
    }

    /// POST /authorize-key (authenticated).
    ///
    /// Authorize a public key on the remote node with the given
    /// capabilities. The remote validates scope restrictions and
    /// creates a node-signed authorized-key TOML.
    pub async fn authorize_key(
        &self,
        public_key: &str,
        label: &str,
        scopes: &[String],
    ) -> Result<AuthorizeKeyResponse> {
        let body = serde_json::json!({
            "public_key": public_key,
            "label": label,
            "scopes": scopes,
        });
        let resp = self.signed_post("/authorize-key", &body).await?;
        Ok(AuthorizeKeyResponse {
            fingerprint: resp["fingerprint"]
                .as_str()
                .context("missing fingerprint in authorize-key response")?
                .to_string(),
            label: resp["label"]
                .as_str()
                .context("missing label in authorize-key response")?
                .to_string(),
            scopes: resp["scopes"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            granted_by: resp["granted_by"].as_str().unwrap_or("").to_string(),
            created_at: resp["created_at"].as_str().unwrap_or("").to_string(),
        })
    }

    /// POST /admission/claim (unauthenticated route; claim is self-signed).
    pub async fn admission_claim(
        &self,
        token: &str,
        public_key: &str,
        label: Option<&str>,
        scopes: &[String],
    ) -> Result<Value> {
        let scopes = normalize_admission_scopes(scopes)?;
        let signed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let nonce_bytes = rand::Rng::gen::<[u8; 16]>(&mut rand::thread_rng());
        let nonce = base64::engine::general_purpose::STANDARD.encode(nonce_bytes);
        let token_hash = admission_token_hash(token);
        let claim = admission_claim_string(
            &self.audience,
            &token_hash,
            public_key,
            &scopes,
            signed_at,
            &nonce,
        );
        let content_hash = lillux::cas::sha256_hex(claim.as_bytes());
        let signature =
            lillux::crypto::Signer::sign(self.identity.signing_key(), content_hash.as_bytes());
        let mut body = serde_json::json!({
            "token": token,
            "public_key": public_key,
            "scopes": scopes,
            "signed_at": signed_at,
            "nonce": nonce,
            "signature": base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        });
        if let Some(label) = label {
            body["label"] = Value::String(label.to_string());
        }
        self.unsigned_post("/admission/claim", &body).await
    }

    /// POST /vault/set (authenticated).
    pub async fn vault_set(&self, name: &str, value: &str) -> Result<Value> {
        let body = serde_json::json!({ "name": name, "value": value });
        self.signed_post("/vault/set", &body).await
    }

    /// GET /vault/list (authenticated).
    pub async fn vault_list(&self) -> Result<Value> {
        self.signed_get("/vault/list").await
    }

    /// POST /vault/delete (authenticated).
    pub async fn vault_delete(&self, name: &str) -> Result<Value> {
        let body = serde_json::json!({ "name": name });
        self.signed_post("/vault/delete", &body).await
    }

    /// GET /threads (authenticated) — list own threads on remote.
    pub async fn threads_list(&self, limit: usize) -> Result<Value> {
        self.signed_get(&format!("/threads?limit={}", limit)).await
    }

    /// GET /threads/{id} (authenticated) — get thread detail on remote.
    pub async fn threads_get(&self, thread_id: &str) -> Result<Value> {
        self.signed_get(&format!("/threads/{}", thread_id)).await
    }

    /// POST /bundle/export (authenticated) — export a bundle as CAS objects.
    pub async fn bundle_export(&self, bundle_name: &str) -> Result<Value> {
        let body = serde_json::json!({ "bundle_name": bundle_name });
        self.signed_post_with_response_limit_and_timeout(
            "/bundle/export",
            &body,
            DEFAULT_JSON_RESPONSE_MAX_BYTES,
            BUNDLE_TRANSFER_REQUEST_TIMEOUT,
        )
        .await
    }

    // ── Internal ──────────────────────────────────────────────────

    async fn signed_get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        // Sign with the full path+query — canonicalize_path inside
        // sign_request will sort query params to match server-side.
        let headers = self.sign_request("GET", path, &[])?;

        let resp = self
            .http
            .get(&url)
            .timeout(CONTROL_PLANE_TIMEOUT)
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?;

        read_json_checked("GET", &url, resp).await
    }

    async fn signed_post(&self, path: &str, body: &Value) -> Result<Value> {
        self.signed_post_with_response_limit(path, body, DEFAULT_JSON_RESPONSE_MAX_BYTES)
            .await
    }

    async fn signed_post_with_response_limit(
        &self,
        path: &str,
        body: &Value,
        max_response_bytes: usize,
    ) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let body_bytes = serde_json::to_vec(body)?;
        let headers = self.sign_request("POST", path, &body_bytes)?;

        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .body(body_bytes)
            .send()
            .await
            .with_context(|| format!("POST {} failed", url))?;

        read_json_checked_with_limit("POST", &url, resp, max_response_bytes).await
    }

    /// Apply a total timeout around request signing, connect/send, and the
    /// complete bounded response-body read. This is deliberately opt-in so
    /// long-running signed POST endpoints such as `/execute` are unchanged.
    async fn signed_post_with_response_limit_and_timeout(
        &self,
        path: &str,
        body: &Value,
        max_response_bytes: usize,
        total_timeout: std::time::Duration,
    ) -> Result<Value> {
        tokio::time::timeout(
            total_timeout,
            self.signed_post_with_response_limit(path, body, max_response_bytes),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "POST {}{} exceeded bundle-transfer timeout of {} seconds",
                self.base_url,
                path,
                total_timeout.as_secs()
            )
        })?
    }

    async fn unsigned_post(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let body_bytes = serde_json::to_vec(body)?;

        let resp = self
            .http
            .post(&url)
            .timeout(CONTROL_PLANE_TIMEOUT)
            .header("content-type", "application/json")
            .body(body_bytes)
            .send()
            .await
            .with_context(|| format!("POST {} failed", url))?;

        read_json_checked("POST", &url, resp).await
    }

    /// Build canonical request and produce the four `x-ryeos-*` headers.
    ///
    /// Canonical string:
    ///   "ryeos-request-v1\n{METHOD}\n{canonical_path}\n{sha256(body)}\n{timestamp}\n{nonce}\n{audience}"
    ///
    /// Signature = Ed25519(sk, sha256(canonical_string).as_bytes())
    fn sign_request(
        &self,
        method: &str,
        path_and_query: &str,
        body: &[u8],
    ) -> Result<SignedHeaders> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let nonce_bytes = rand::Rng::gen::<[u8; 16]>(&mut rand::thread_rng());
        let nonce = base64::engine::general_purpose::STANDARD.encode(nonce_bytes);

        let body_hash = lillux::cas::sha256_hex(body);
        let canon_path = canonicalize_path(path_and_query);

        let string_to_sign = format!(
            "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
            method.to_uppercase(),
            canon_path,
            body_hash,
            timestamp,
            nonce,
            self.audience,
        );

        let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());
        let sig =
            lillux::crypto::Signer::sign(self.identity.signing_key(), content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        Ok(SignedHeaders {
            key_id: self.identity.principal_id(),
            timestamp: timestamp.to_string(),
            nonce,
            signature: sig_b64,
        })
    }
}

fn hashes_request_body_size(hashes: &[String]) -> usize {
    serde_json::json!({ "hashes": hashes }).to_string().len()
}

fn typed_hashes_request_body_size(object_hashes: &[String], blob_hashes: &[String]) -> usize {
    serde_json::json!({
        "object_hashes": object_hashes,
        "blob_hashes": blob_hashes,
    })
    .to_string()
    .len()
}

fn chunk_typed_hashes_for_body_budget(hashes: &[String], budget_bytes: usize) -> Vec<Vec<String>> {
    if hashes.is_empty() {
        return Vec::new();
    }

    let empty_body_size = typed_hashes_request_body_size(&[], &[]);
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_size = empty_body_size;
    for hash in hashes {
        let encoded_hash_size = serde_json::to_string(hash)
            .expect("serializing a string as JSON cannot fail")
            .len();
        let separator_size = usize::from(!current.is_empty());
        if !current.is_empty()
            && current_size
                .saturating_add(separator_size)
                .saturating_add(encoded_hash_size)
                > budget_bytes
        {
            chunks.push(std::mem::take(&mut current));
            current_size = empty_body_size;
        }
        current_size = current_size
            .saturating_add(usize::from(!current.is_empty()))
            .saturating_add(encoded_hash_size);
        current.push(hash.clone());
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn chunk_hashes_for_body_budget(hashes: &[String], budget_bytes: usize) -> Vec<Vec<String>> {
    if hashes.is_empty() {
        return Vec::new();
    }

    let empty_body_size = hashes_request_body_size(&[]);
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_size = empty_body_size;

    for hash in hashes {
        let entry_size = hash.len() + 2 + usize::from(!current.is_empty()); // quotes + comma
        if !current.is_empty() && current_size + entry_size > budget_bytes {
            chunks.push(std::mem::take(&mut current));
            current_size = empty_body_size;
        }
        current_size += hash.len() + 2 + usize::from(!current.is_empty());
        current.push(hash.clone());
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

fn admission_token_hash(token: &str) -> String {
    lillux::cas::sha256_hex(token.as_bytes())
}

fn normalize_admission_scopes(scopes: &[String]) -> Result<Vec<String>> {
    let mut normalized: Vec<String> = scopes
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    normalized.sort();
    normalized.dedup();

    if normalized.is_empty() {
        anyhow::bail!("scopes must not be empty");
    }
    if normalized.len() != scopes.len() {
        anyhow::bail!("duplicate or empty scopes after normalization");
    }
    for scope in &normalized {
        ryeos_runtime::authorizer::validate_scope_pattern(scope).map_err(anyhow::Error::msg)?;
    }
    if normalized.iter().any(|s| s.contains('*')) {
        anyhow::bail!("wildcard scopes are forbidden for admission claim requests");
    }

    Ok(normalized)
}

fn admission_claim_string(
    audience: &str,
    token_hash: &str,
    public_key: &str,
    scopes: &[String],
    signed_at: u64,
    nonce: &str,
) -> String {
    format!(
        "ryeos-admission-claim-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        audience,
        token_hash,
        public_key,
        scopes.join(","),
        signed_at,
        nonce,
    )
}

struct SignedHeaders {
    key_id: String,
    timestamp: String,
    nonce: String,
    signature: String,
}

/// Sort query parameters alphabetically and normalise the path.
/// Matches `crates/bin/daemon/src/auth.rs::canonical_path` — uses a Vec to
/// preserve repeated keys, matching server canonicalization exactly.
fn canonicalize_path(path_and_query: &str) -> String {
    if let Some((path, query)) = path_and_query.split_once('?') {
        let mut params: Vec<(&str, &str)> = query
            .split('&')
            .filter_map(|pair| {
                pair.split_once('=').or({
                    if pair.is_empty() {
                        None
                    } else {
                        Some((pair, ""))
                    }
                })
            })
            .collect();
        params.sort();
        let sorted: Vec<String> = params
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    k.to_string()
                } else {
                    format!("{k}={v}")
                }
            })
            .collect();
        format!("{}?{}", path, sorted.join("&"))
    } else {
        path_and_query.to_string()
    }
}

// ── Response types ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PublicKeyResponse {
    pub principal_id: String,
    pub fingerprint: String,
    /// Remote daemon Ed25519 verifying key in `ed25519:<base64>` form.
    pub signing_key: String,
    pub vault_fingerprint: String,
    /// Daemon site identity (e.g. `"site:my-hostname"`).
    /// Discovered from the remote's `/public-key` response.
    pub site_id: String,
}

impl PublicKeyResponse {
    pub fn validate_identity_binding(&self) -> Result<lillux::crypto::VerifyingKey> {
        let key = super::config::decode_signing_key(&self.signing_key)
            .context("invalid remote signing_key from /public-key")?;
        let actual_fingerprint = lillux::crypto::fingerprint(&key);
        if self.fingerprint != actual_fingerprint {
            anyhow::bail!(
                "remote /public-key fingerprint mismatch: response fingerprint {}, signing_key fingerprint {}",
                self.fingerprint,
                actual_fingerprint,
            );
        }
        let expected_principal = format!("fp:{actual_fingerprint}");
        if self.principal_id != expected_principal {
            anyhow::bail!(
                "remote /public-key principal_id mismatch: expected {}, got {}",
                expected_principal,
                self.principal_id,
            );
        }
        if self.site_id.trim().is_empty() {
            anyhow::bail!("remote /public-key site_id must not be empty");
        }
        if self.vault_fingerprint.trim().is_empty() {
            anyhow::bail!("remote /public-key vault_fingerprint must not be empty");
        }
        Ok(key)
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectsHasResponse {
    pub found_object_hashes: Vec<String>,
    pub missing_object_hashes: Vec<String>,
    pub found_blob_hashes: Vec<String>,
    pub missing_blob_hashes: Vec<String>,
}

impl ObjectsHasResponse {
    pub fn validate_against_request(
        &self,
        requested_objects: &[String],
        requested_blobs: &[String],
    ) -> Result<()> {
        validate_hash_partition(
            requested_objects,
            &self.found_object_hashes,
            &self.missing_object_hashes,
            "object",
        )?;
        validate_hash_partition(
            requested_blobs,
            &self.found_blob_hashes,
            &self.missing_blob_hashes,
            "blob",
        )
    }
}

fn validate_hash_partition(
    requested: &[String],
    found: &[String],
    missing: &[String],
    namespace: &str,
) -> Result<()> {
    let requested_set: BTreeSet<&str> = requested.iter().map(String::as_str).collect();
    if requested_set.len() != requested.len() {
        anyhow::bail!("objects/has request contains duplicate {namespace} hashes");
    }

    let mut seen = BTreeSet::new();
    for hash in found.iter().chain(missing.iter()) {
        if !is_canonical_hash(hash) {
            anyhow::bail!("objects/has returned invalid {namespace} hash: {hash}");
        }
        if !requested_set.contains(hash.as_str()) {
            anyhow::bail!("objects/has returned unexpected {namespace} hash: {hash}");
        }
        if !seen.insert(hash.as_str()) {
            anyhow::bail!("objects/has returned duplicate {namespace} hash: {hash}");
        }
    }

    for hash in requested_set {
        if !seen.contains(hash) {
            anyhow::bail!("objects/has omitted requested {namespace} hash: {hash}");
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
pub struct ObjectsPutResponse {
    pub staging_id: String,
    pub expected_previous_hash: Option<String>,
    pub blob_hashes: Vec<String>,
    pub object_hashes: Vec<String>,
}

/// A single entry from the `objects/get` response.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CasEntry {
    pub hash: String,
    /// "blob", "object", or "missing".
    pub kind: String,
    /// Base64-encoded data (present when kind == "blob").
    pub data: Option<String>,
    /// JSON value (present when kind == "object").
    pub value: Option<Value>,
}

/// Response from `objects/get`.
#[derive(Debug, Clone)]
pub struct ObjectsGetResponse {
    pub entries: Vec<CasEntry>,
}

impl ObjectsGetResponse {
    pub fn validate_against_request(&self, requested: &[String]) -> Result<()> {
        let requested: std::collections::BTreeSet<&str> =
            requested.iter().map(String::as_str).collect();
        let mut seen = std::collections::BTreeSet::new();
        for entry in &self.entries {
            if !is_canonical_hash(&entry.hash) {
                anyhow::bail!("invalid objects/get entry hash {}", entry.hash);
            }
            if !requested.contains(entry.hash.as_str()) {
                anyhow::bail!("objects/get returned unexpected hash {}", entry.hash);
            }
            if !seen.insert(entry.hash.as_str()) {
                anyhow::bail!("objects/get returned duplicate hash {}", entry.hash);
            }
            match entry.kind.as_str() {
                "object" if entry.value.is_some() && entry.data.is_none() => {
                    let canonical =
                        lillux::canonical_json(entry.value.as_ref().expect("value checked above"))?;
                    let actual = lillux::sha256_hex(canonical.as_bytes());
                    if actual != entry.hash {
                        anyhow::bail!(
                            "objects/get object hash mismatch: expected {}, got {}",
                            entry.hash,
                            actual
                        );
                    }
                }
                "blob" if entry.data.is_some() && entry.value.is_none() => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(entry.data.as_ref().expect("data checked above"))
                        .context("invalid base64 in objects/get blob entry")?;
                    let actual = lillux::sha256_hex(&bytes);
                    if actual != entry.hash {
                        anyhow::bail!(
                            "objects/get blob hash mismatch: expected {}, got {}",
                            entry.hash,
                            actual
                        );
                    }
                }
                "missing" if entry.value.is_none() && entry.data.is_none() => {}
                other => anyhow::bail!("invalid objects/get entry kind/shape: {other}"),
            }
        }
        for hash in requested {
            if !seen.contains(hash) {
                anyhow::bail!("objects/get response missing requested hash {hash}");
            }
        }
        Ok(())
    }

    /// Find an entry by hash, returning its value (for objects) or
    /// decoded blob data.
    pub fn find_object(&self, hash: &str) -> Option<Value> {
        self.entries
            .iter()
            .find(|e| e.hash == hash && e.kind == "object")
            .and_then(|e| e.value.clone())
    }

    /// Find a blob entry by hash, returning decoded bytes.
    pub fn find_blob(&self, hash: &str) -> Option<Vec<u8>> {
        use base64::Engine;
        self.entries
            .iter()
            .find(|e| e.hash == hash && e.kind == "blob")
            .and_then(|e| e.data.as_ref())
            .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ObjectsClosureDescribeResponse {
    #[serde(flatten)]
    pub closure: ObjectsClosureSummary,
}

impl ObjectsClosureDescribeResponse {
    pub fn validate_against_request(&self, requested_roots: &[String]) -> Result<()> {
        validate_closure_summary_against_request(&self.closure, requested_roots)
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ObjectsClosureGetResponse {
    pub closure: ObjectsClosureSummary,
    #[serde(default)]
    pub object_bytes: u64,
    #[serde(default)]
    pub blob_bytes: u64,
    pub entries: Vec<CasEntry>,
}

impl ObjectsClosureGetResponse {
    pub fn validate_against_request(&self, requested_roots: &[String]) -> Result<()> {
        validate_closure_summary_against_request(&self.closure, requested_roots)?;
        if !self.closure.complete {
            anyhow::bail!("remote returned incomplete object closure");
        }
        if !self.closure.missing_objects.is_empty()
            || !self.closure.missing_blobs.is_empty()
            || !self.closure.malformed_objects.is_empty()
            || !self.closure.unsupported_objects.is_empty()
        {
            anyhow::bail!(
                "remote returned complete closure with missing/malformed/unsupported entries"
            );
        }
        let object_hashes: std::collections::BTreeSet<&str> = self
            .closure
            .object_hashes
            .iter()
            .map(String::as_str)
            .collect();
        let blob_hashes: std::collections::BTreeSet<&str> = self
            .closure
            .blob_hashes
            .iter()
            .map(String::as_str)
            .collect();
        for hash in self
            .closure
            .object_hashes
            .iter()
            .chain(self.closure.blob_hashes.iter())
        {
            if !is_canonical_hash(hash) {
                anyhow::bail!("invalid closure hash {hash}");
            }
        }
        for entry in &self.entries {
            if !is_canonical_hash(&entry.hash) {
                anyhow::bail!("invalid closure entry hash {}", entry.hash);
            }
            match entry.kind.as_str() {
                "object" if entry.value.is_some() && entry.data.is_none() => {
                    if !object_hashes.contains(entry.hash.as_str()) {
                        anyhow::bail!("object entry {} is not in advertised closure", entry.hash);
                    }
                    let canonical =
                        lillux::canonical_json(entry.value.as_ref().expect("value checked above"))?;
                    let actual = lillux::sha256_hex(canonical.as_bytes());
                    if actual != entry.hash {
                        anyhow::bail!(
                            "object closure entry hash mismatch: expected {}, got {}",
                            entry.hash,
                            actual
                        );
                    }
                }
                "blob" if entry.data.is_some() && entry.value.is_none() => {
                    if !blob_hashes.contains(entry.hash.as_str()) {
                        anyhow::bail!("blob entry {} is not in advertised closure", entry.hash);
                    }
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(entry.data.as_ref().expect("data checked above"))
                        .context("invalid base64 in closure blob entry")?;
                    let actual = lillux::sha256_hex(&decoded);
                    if actual != entry.hash {
                        anyhow::bail!(
                            "blob closure entry hash mismatch: expected {}, got {}",
                            entry.hash,
                            actual
                        );
                    }
                }
                "missing" if entry.value.is_none() && entry.data.is_none() => {}
                other => anyhow::bail!("invalid closure entry kind/shape: {other}"),
            }
        }
        for hash in object_hashes {
            if !self
                .entries
                .iter()
                .any(|entry| entry.kind == "object" && entry.hash == hash)
            {
                anyhow::bail!("missing object entry for advertised closure hash {hash}");
            }
        }
        for hash in blob_hashes {
            if !self
                .entries
                .iter()
                .any(|entry| entry.kind == "blob" && entry.hash == hash)
            {
                anyhow::bail!("missing blob entry for advertised closure hash {hash}");
            }
        }
        Ok(())
    }
}

fn validate_closure_summary_against_request(
    closure: &ObjectsClosureSummary,
    requested_roots: &[String],
) -> Result<()> {
    let requested: std::collections::BTreeSet<&str> =
        requested_roots.iter().map(String::as_str).collect();
    let returned: std::collections::BTreeSet<&str> =
        closure.roots.iter().map(String::as_str).collect();
    if requested != returned {
        anyhow::bail!("closure response roots do not match request");
    }
    for hash in closure
        .roots
        .iter()
        .chain(closure.object_hashes.iter())
        .chain(closure.blob_hashes.iter())
        .chain(closure.missing_objects.iter().map(|item| &item.hash))
        .chain(closure.missing_blobs.iter().map(|item| &item.hash))
        .chain(closure.malformed_objects.iter().map(|item| &item.hash))
        .chain(closure.unsupported_objects.iter().map(|item| &item.hash))
    {
        if !is_canonical_hash(hash) {
            anyhow::bail!("invalid closure summary hash {hash}");
        }
    }
    for root in requested_roots {
        let accounted = closure.object_hashes.iter().any(|hash| hash == root)
            || closure
                .missing_objects
                .iter()
                .any(|item| item.hash == *root)
            || closure
                .malformed_objects
                .iter()
                .any(|item| item.hash == *root)
            || closure
                .unsupported_objects
                .iter()
                .any(|item| item.hash == *root);
        if !accounted {
            anyhow::bail!("closure response did not account for requested root {root}");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct ObjectsClosureSummary {
    pub roots: Vec<String>,
    pub complete: bool,
    pub object_hashes: Vec<String>,
    pub blob_hashes: Vec<String>,
    pub missing_objects: Vec<ClosureMissingObject>,
    #[serde(default)]
    pub missing_blobs: Vec<ClosureMissingObject>,
    pub malformed_objects: Vec<ClosureMalformedObject>,
    pub unsupported_objects: Vec<ClosureUnsupportedObject>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ClosureMissingObject {
    pub hash: String,
    pub referenced_by: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ClosureMalformedObject {
    pub hash: String,
    pub reason: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ClosureUnsupportedObject {
    pub hash: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default)]
pub struct ObjectsClosureRequestOptions {
    pub max_objects: Option<usize>,
    pub max_blobs: Option<usize>,
    pub max_object_bytes: Option<u64>,
    pub max_total_object_bytes: Option<u64>,
    pub max_blob_bytes: Option<u64>,
    pub max_total_blob_bytes: Option<u64>,
    pub max_response_bytes: Option<u64>,
    pub max_links_per_object: Option<usize>,
    pub allow_incomplete: bool,
}

fn closure_request_body(roots: &[String], options: &ObjectsClosureRequestOptions) -> Value {
    let mut body = serde_json::json!({ "roots": roots });
    if let Some(limit) = options.max_objects {
        body["max_objects"] = serde_json::json!(limit);
    }
    if let Some(limit) = options.max_blobs {
        body["max_blobs"] = serde_json::json!(limit);
    }
    if let Some(limit) = options.max_object_bytes {
        body["max_object_bytes"] = serde_json::json!(limit);
    }
    if let Some(limit) = options.max_total_object_bytes {
        body["max_total_object_bytes"] = serde_json::json!(limit);
    }
    if let Some(limit) = options.max_blob_bytes {
        body["max_blob_bytes"] = serde_json::json!(limit);
    }
    if let Some(limit) = options.max_total_blob_bytes {
        body["max_total_blob_bytes"] = serde_json::json!(limit);
    }
    if let Some(limit) = options.max_response_bytes {
        body["max_response_bytes"] = serde_json::json!(limit);
    }
    if let Some(limit) = options.max_links_per_object {
        body["max_links_per_object"] = serde_json::json!(limit);
    }
    if options.allow_incomplete {
        body["allow_incomplete"] = serde_json::json!(true);
    }
    body
}

#[derive(Debug, Clone, Default)]
pub struct AdmissionSubmitOptions {
    pub claim: Option<String>,
    pub max_objects: Option<usize>,
    pub max_blobs: Option<usize>,
    pub max_object_bytes: Option<u64>,
    pub max_blob_bytes: Option<u64>,
    pub max_total_blob_bytes: Option<u64>,
    pub max_links_per_object: Option<usize>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AdmissionSubmitResponse {
    pub subject_hash: String,
    pub policy: String,
    pub claim: String,
    pub attestation_hash: String,
    pub reused_existing: bool,
}

impl AdmissionSubmitResponse {
    pub fn validate_against_request(&self, subject_hash: &str, policy: &str) -> Result<()> {
        if self.subject_hash != subject_hash {
            anyhow::bail!(
                "admission submit subject mismatch: expected {}, got {}",
                subject_hash,
                self.subject_hash
            );
        }
        if self.policy != policy {
            anyhow::bail!(
                "admission submit policy mismatch: expected {}, got {}",
                policy,
                self.policy
            );
        }
        if !is_canonical_hash(&self.attestation_hash) {
            anyhow::bail!(
                "invalid admission attestation hash: {}",
                self.attestation_hash
            );
        }
        if self.claim != "accepted" {
            anyhow::bail!("unsupported admission claim in response: {}", self.claim);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AdmissionStatusResponse {
    pub subject_hash: String,
    pub policy: String,
    pub status: String,
    pub attestation_hash: Option<String>,
    pub head: Option<Value>,
    pub attestation: Option<Value>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AdmissionAttestationsForSubjectResponse {
    pub subject_hash: String,
    pub policy: Option<String>,
    #[serde(default)]
    pub attestations: Vec<AdmissionAttestationRemoteRecord>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AdmissionAttestationRemoteRecord {
    pub attestation_hash: String,
    pub subject_hash: String,
    pub policy: String,
    pub claim: String,
    pub issuer: String,
    pub issued_at: String,
    pub expires_at: Option<String>,
    pub head_ref_path: Option<String>,
    pub indexed_at: String,
    pub state: String,
    pub attestation: Value,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SyncJobsListResponse {
    #[serde(default)]
    pub jobs: Vec<SyncJobRemoteRecord>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct FederationCapabilitiesResponse {
    pub protocol: Value,
    pub identity: Value,
    #[serde(default)]
    pub object_kinds: Vec<String>,
    pub services: Value,
    pub limits: Value,
}

impl FederationCapabilitiesResponse {
    pub fn validate(&self) -> Result<()> {
        let protocol_name = self
            .protocol
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("federation capabilities missing protocol.name"))?;
        if protocol_name != "ryeos-distributed-substrate" {
            anyhow::bail!("unsupported federation protocol: {protocol_name}");
        }
        let versions = self
            .protocol
            .get("versions")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("federation capabilities missing protocol.versions"))?;
        if !versions.iter().any(|version| version.as_u64() == Some(1)) {
            anyhow::bail!("federation capabilities missing supported protocol version 1");
        }
        for field in ["principal_id", "fingerprint", "site_id"] {
            if self.identity.get(field).and_then(Value::as_str).is_none() {
                anyhow::bail!("federation capabilities missing identity.{field}");
            }
        }
        if !self.object_kinds.iter().any(|kind| kind == "attestation") {
            anyhow::bail!("federation capabilities missing attestation object support");
        }
        if self.services.get("objects").is_none() || self.services.get("admission").is_none() {
            anyhow::bail!("federation capabilities missing core services");
        }
        if self.limits.get("default_max_objects_per_closure").is_none() {
            anyhow::bail!("federation capabilities missing closure limits");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct FederationHeadsListResponse {
    pub prefix: String,
    pub limit: usize,
    pub truncated: bool,
    #[serde(default)]
    pub heads: Vec<FederationHeadRemoteRecord>,
}

impl FederationHeadsListResponse {
    pub fn validate(&self, requested_prefix: &str) -> Result<()> {
        if self.prefix != requested_prefix {
            anyhow::bail!(
                "federation heads prefix mismatch: expected {}, got {}",
                requested_prefix,
                self.prefix
            );
        }
        if self.limit > 500 {
            anyhow::bail!("federation heads response limit exceeds protocol maximum");
        }
        if self.heads.len() > self.limit {
            anyhow::bail!("federation heads response exceeds declared limit");
        }
        for head in &self.heads {
            head.validate()?;
            if !requested_prefix.is_empty()
                && !head.ref_path.starts_with(&format!("{requested_prefix}/"))
            {
                anyhow::bail!(
                    "federation head {} escaped requested prefix {}",
                    head.ref_path,
                    requested_prefix
                );
            }
        }
        Ok(())
    }

    pub fn verify_signed_refs(
        &self,
        expected_signer: &str,
        verifying_key: &lillux::crypto::VerifyingKey,
    ) -> Result<()> {
        for head in &self.heads {
            head.signed_ref
                .verify_with_key(expected_signer, verifying_key)
                .with_context(|| format!("failed to verify federated head {}", head.ref_path))?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct FederationHeadRemoteRecord {
    pub namespace: String,
    pub name: String,
    pub ref_path: String,
    pub target_hash: String,
    pub signer: String,
    pub updated_at: String,
    pub signed_ref: RemoteSignedRef,
}

impl FederationHeadRemoteRecord {
    fn validate(&self) -> Result<()> {
        if self.namespace.is_empty()
            || self.name.is_empty()
            || self.ref_path.is_empty()
            || self.signer.is_empty()
            || self.updated_at.is_empty()
        {
            anyhow::bail!("federation head response contains empty required field");
        }
        if !self.ref_path.ends_with("/head") {
            anyhow::bail!("federation head response ref_path missing /head suffix");
        }
        if self.ref_path.contains("..") || self.ref_path.starts_with('/') {
            anyhow::bail!("federation head response has unsafe ref_path");
        }
        if !is_canonical_hash(&self.target_hash) {
            anyhow::bail!("invalid federation head target hash: {}", self.target_hash);
        }
        self.signed_ref.validate()?;
        if self.signed_ref.ref_path != self.ref_path
            || self.signed_ref.target_hash != self.target_hash
            || self.signed_ref.signer != self.signer
            || self.signed_ref.updated_at != self.updated_at
        {
            anyhow::bail!("federation head signed_ref does not match response metadata");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RemoteSignedRef {
    pub schema: u32,
    pub kind: String,
    pub ref_path: String,
    pub target_hash: String,
    pub updated_at: String,
    pub signer: String,
    pub signature: String,
}

impl RemoteSignedRef {
    pub fn validate(&self) -> Result<()> {
        if self.schema != 1 {
            anyhow::bail!("unsupported signed ref schema: {}", self.schema);
        }
        if self.kind != "signed_ref" {
            anyhow::bail!("unexpected signed ref kind: {}", self.kind);
        }
        if self.ref_path.is_empty()
            || self.updated_at.is_empty()
            || self.signer.is_empty()
            || self.signature.is_empty()
        {
            anyhow::bail!("signed ref contains empty required field");
        }
        if !self.ref_path.ends_with("/head")
            || self.ref_path.contains("..")
            || self.ref_path.starts_with('/')
        {
            anyhow::bail!("signed ref has unsafe ref_path");
        }
        if !is_canonical_hash(&self.target_hash) {
            anyhow::bail!("invalid signed ref target hash: {}", self.target_hash);
        }
        ryeos_state::parse_canonical_timestamp(&self.updated_at)
            .map_err(|error| anyhow::anyhow!("invalid signed ref updated_at: {error}"))?;
        if !is_canonical_hash(&self.signer) {
            anyhow::bail!("invalid signed ref signer fingerprint: {}", self.signer);
        }
        Ok(())
    }

    /// Verify this remote signed ref against an expected signer key.
    ///
    /// Structural validation alone is only discovery; callers must use this
    /// before importing or treating a federated head as authoritative.
    pub fn verify_with_key(
        &self,
        expected_signer: &str,
        verifying_key: &lillux::crypto::VerifyingKey,
    ) -> Result<()> {
        self.validate()?;
        if self.signer != expected_signer {
            anyhow::bail!(
                "signed ref signer mismatch: expected {}, got {}",
                expected_signer,
                self.signer
            );
        }
        let unsigned = serde_json::json!({
            "schema": self.schema,
            "kind": self.kind,
            "ref_path": self.ref_path,
            "target_hash": self.target_hash,
            "updated_at": self.updated_at,
            "signer": self.signer,
        });
        let canonical = lillux::canonical_json(&unsigned)?;
        let sig_bytes = base64::engine::general_purpose::STANDARD
            .decode(&self.signature)
            .context("failed to decode signed ref signature")?;
        let signature = lillux::crypto::Signature::from_slice(&sig_bytes)
            .map_err(|e| anyhow::anyhow!("failed to parse signed ref signature: {e}"))?;
        verifying_key
            .verify(canonical.as_bytes(), &signature)
            .map_err(|e| anyhow::anyhow!("signed ref signature verification failed: {e}"))
    }
}

impl SyncJobsListResponse {
    pub fn validate(&self, requested_state: Option<&str>) -> Result<()> {
        if let Some(state) = requested_state {
            parse_remote_sync_job_state(state)?;
            for job in &self.jobs {
                if job.state != state {
                    anyhow::bail!(
                        "sync jobs list state mismatch: expected {}, got {} for {}",
                        state,
                        job.state,
                        job.job_id
                    );
                }
            }
        }
        for job in &self.jobs {
            job.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SyncJobsInspectResponse {
    pub status: String,
    pub job_id: Option<String>,
    pub job: Option<SyncJobRemoteRecord>,
    #[serde(default)]
    pub attempts: Vec<SyncJobAttemptRemoteRecord>,
}

impl SyncJobsInspectResponse {
    pub fn validate_against_request(&self, job_id: &str) -> Result<()> {
        match self.status.as_str() {
            "missing" => {
                if self.job.is_some() {
                    anyhow::bail!("missing sync job response must not include job data");
                }
                if !self.attempts.is_empty() {
                    anyhow::bail!("missing sync job response must not include attempts");
                }
                if self.job_id.as_deref() != Some(job_id) {
                    anyhow::bail!("sync job inspect missing response id mismatch");
                }
            }
            "found" => {
                let Some(job) = self.job.as_ref() else {
                    anyhow::bail!("found sync job response missing job data");
                };
                job.validate()?;
                if job.job_id != job_id {
                    anyhow::bail!(
                        "sync job inspect id mismatch: expected {}, got {}",
                        job_id,
                        job.job_id
                    );
                }
                for attempt in &self.attempts {
                    attempt.validate()?;
                    if attempt.job_id != job_id {
                        anyhow::bail!(
                            "sync job inspect attempt id mismatch: expected {}, got {} for {}",
                            job_id,
                            attempt.job_id,
                            attempt.attempt_id
                        );
                    }
                }
            }
            other => anyhow::bail!("unknown sync job inspect status: {other}"),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SyncJobRemoteRecord {
    pub job_id: String,
    pub operation_type: String,
    pub peer: Option<String>,
    pub state: String,
    pub phase: String,
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub heads: Vec<String>,
    #[serde(default)]
    pub uploaded_hashes: Vec<String>,
    #[serde(default)]
    pub fetched_hashes: Vec<String>,
    pub attempt_count: u64,
    pub max_attempts: u64,
    pub last_error: Option<String>,
    pub result: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

impl SyncJobRemoteRecord {
    pub fn validate(&self) -> Result<()> {
        if self.job_id.is_empty() {
            anyhow::bail!("sync job response missing job_id");
        }
        if self.operation_type.is_empty() {
            anyhow::bail!("sync job response missing operation_type");
        }
        parse_remote_sync_job_state(&self.state)?;
        if self.phase.is_empty() {
            anyhow::bail!("sync job response missing phase");
        }
        for hash in self
            .roots
            .iter()
            .chain(self.heads.iter())
            .chain(self.uploaded_hashes.iter())
            .chain(self.fetched_hashes.iter())
        {
            if !is_canonical_hash(hash) {
                anyhow::bail!("invalid sync job response hash: {hash}");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SyncJobAttemptRemoteRecord {
    pub attempt_id: String,
    pub job_id: String,
    pub attempt_number: u64,
    pub worker_id: Option<String>,
    pub state: String,
    pub phase: String,
    pub started_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub result: Option<Value>,
}

impl SyncJobAttemptRemoteRecord {
    pub fn validate(&self) -> Result<()> {
        if self.attempt_id.is_empty() {
            anyhow::bail!("sync job attempt response missing attempt_id");
        }
        if self.job_id.is_empty() {
            anyhow::bail!("sync job attempt response missing job_id");
        }
        if self.attempt_number == 0 {
            anyhow::bail!("sync job attempt response has zero attempt_number");
        }
        parse_remote_sync_job_attempt_state(&self.state)?;
        if self.phase.is_empty() {
            anyhow::bail!("sync job attempt response missing phase");
        }
        if self.started_at.is_empty() || self.updated_at.is_empty() {
            anyhow::bail!("sync job attempt response missing timestamps");
        }
        Ok(())
    }
}

fn parse_remote_sync_job_state(value: &str) -> Result<()> {
    match value {
        "planned" | "running" | "completed" | "failed" | "retryable" | "cancelled" => Ok(()),
        other => anyhow::bail!("unknown sync job state: {other}"),
    }
}

fn parse_remote_sync_job_attempt_state(value: &str) -> Result<()> {
    match value {
        "running" | "completed" | "failed" | "cancelled" => Ok(()),
        other => anyhow::bail!("unknown sync job attempt state: {other}"),
    }
}

impl AdmissionStatusResponse {
    pub fn validate_against_request(&self, subject_hash: &str, policy: &str) -> Result<()> {
        if self.subject_hash != subject_hash {
            anyhow::bail!(
                "admission status subject mismatch: expected {}, got {}",
                subject_hash,
                self.subject_hash
            );
        }
        if self.policy != policy {
            anyhow::bail!(
                "admission status policy mismatch: expected {}, got {}",
                policy,
                self.policy
            );
        }
        match self.status.as_str() {
            "missing" => {
                if self.attestation_hash.is_some()
                    || self.head.is_some()
                    || self.attestation.is_some()
                {
                    anyhow::bail!("missing admission status must not include attestation data");
                }
            }
            "accepted" => {
                let Some(head) = self.head.as_ref() else {
                    anyhow::bail!("accepted admission status missing head");
                };
                let Some(hash) = self.attestation_hash.as_deref() else {
                    anyhow::bail!("accepted admission status missing attestation_hash");
                };
                if !is_canonical_hash(hash) {
                    anyhow::bail!("invalid admission attestation hash: {hash}");
                }
                if let Some(signed_ref_value) = head.get("signed_ref") {
                    let signed_ref: RemoteSignedRef =
                        serde_json::from_value(signed_ref_value.clone())
                            .context("failed to parse admission head signed_ref")?;
                    signed_ref.validate()?;
                    if signed_ref.target_hash != hash {
                        anyhow::bail!("admission head signed_ref does not point at attestation");
                    }
                }
                let Some(attestation) = self.attestation.as_ref() else {
                    anyhow::bail!("accepted admission status missing attestation object");
                };
                let canonical = lillux::canonical_json(attestation)?;
                let actual = lillux::sha256_hex(canonical.as_bytes());
                if actual != hash {
                    anyhow::bail!(
                        "admission status attestation hash mismatch: expected {}, got {}",
                        hash,
                        actual
                    );
                }
                let attestation = ryeos_state::Attestation::from_value(attestation)?;
                if attestation.subject_hash != subject_hash
                    || attestation.policy != policy
                    || attestation.claim != "accepted"
                {
                    anyhow::bail!("accepted admission attestation does not match request");
                }
            }
            "invalid" | "missing_attestation" => {}
            other => anyhow::bail!("unknown admission status: {other}"),
        }
        Ok(())
    }
}

impl AdmissionAttestationsForSubjectResponse {
    pub fn validate_against_request(&self, subject_hash: &str, policy: Option<&str>) -> Result<()> {
        if self.subject_hash != subject_hash {
            anyhow::bail!(
                "admission attestations subject mismatch: expected {}, got {}",
                subject_hash,
                self.subject_hash
            );
        }
        if self.policy.as_deref() != policy {
            anyhow::bail!("admission attestations policy mismatch");
        }
        for attestation in &self.attestations {
            attestation.validate()?;
            if attestation.subject_hash != subject_hash {
                anyhow::bail!(
                    "admission attestation subject mismatch: expected {}, got {}",
                    subject_hash,
                    attestation.subject_hash
                );
            }
            if let Some(policy) = policy {
                if attestation.policy != policy {
                    anyhow::bail!(
                        "admission attestation policy mismatch: expected {}, got {}",
                        policy,
                        attestation.policy
                    );
                }
            }
        }
        Ok(())
    }
}

impl AdmissionAttestationRemoteRecord {
    fn validate(&self) -> Result<()> {
        if !is_canonical_hash(&self.attestation_hash) {
            anyhow::bail!(
                "invalid admission attestation hash: {}",
                self.attestation_hash
            );
        }
        if !is_canonical_hash(&self.subject_hash) {
            anyhow::bail!("invalid admission subject hash: {}", self.subject_hash);
        }
        if self.policy.is_empty()
            || self.claim.is_empty()
            || self.issuer.is_empty()
            || self.issued_at.is_empty()
            || self.indexed_at.is_empty()
        {
            anyhow::bail!("admission attestation response contains empty required field");
        }
        if !matches!(self.state.as_str(), "accepted" | "rejected") {
            anyhow::bail!("unknown admission attestation state: {}", self.state);
        }
        let canonical = lillux::canonical_json(&self.attestation)?;
        let actual = lillux::sha256_hex(canonical.as_bytes());
        if actual != self.attestation_hash {
            anyhow::bail!(
                "admission attestation object hash mismatch: expected {}, got {}",
                self.attestation_hash,
                actual
            );
        }
        let attestation = ryeos_state::Attestation::from_value(&self.attestation)?;
        if attestation.subject_hash != self.subject_hash
            || attestation.policy != self.policy
            || attestation.claim != self.claim
            || attestation.issuer != self.issuer
            || attestation.issued_at != self.issued_at
            || attestation.expires_at != self.expires_at
        {
            anyhow::bail!("admission attestation object does not match index row");
        }
        Ok(())
    }
}

fn is_canonical_hash(hash: &str) -> bool {
    lillux::valid_hash(hash) && !hash.bytes().any(|b| b.is_ascii_uppercase())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthorizeKeyResponse {
    pub fingerprint: String,
    pub label: String,
    pub scopes: Vec<String>,
    pub granted_by: String,
    pub created_at: String,
}

/// A blob to upload, with base64-encoded data.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BlobUpload {
    pub data: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public_key_response(seed: u8) -> PublicKeyResponse {
        let signing_key = lillux::crypto::SigningKey::from_bytes(&[seed; 32]);
        let verifying_key = signing_key.verifying_key();
        let fingerprint = lillux::crypto::fingerprint(&verifying_key);
        PublicKeyResponse {
            principal_id: format!("fp:{fingerprint}"),
            fingerprint,
            signing_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(verifying_key.as_bytes())
            ),
            vault_fingerprint: "sha256:vault".into(),
            site_id: "site:test".into(),
        }
    }

    #[test]
    fn public_key_response_validates_identity_binding() {
        let response = public_key_response(7);
        let key = response.validate_identity_binding().unwrap();
        assert_eq!(lillux::crypto::fingerprint(&key), response.fingerprint);
    }

    #[test]
    fn public_key_response_rejects_mismatched_fingerprint() {
        let mut response = public_key_response(7);
        response.fingerprint = "00".repeat(32);
        assert!(response.validate_identity_binding().is_err());
    }

    #[test]
    fn public_key_response_rejects_mismatched_principal() {
        let mut response = public_key_response(7);
        response.principal_id = format!("fp:{}", "00".repeat(32));
        assert!(response.validate_identity_binding().is_err());
    }

    #[test]
    fn canonicalize_path_no_query() {
        assert_eq!(canonicalize_path("/execute"), "/execute");
    }

    #[test]
    fn canonicalize_path_sorted_query() {
        assert_eq!(
            canonicalize_path("/threads/abc/events/stream?after=42&limit=10"),
            "/threads/abc/events/stream?after=42&limit=10"
        );
    }

    #[test]
    fn canonicalize_path_unsorted_query() {
        assert_eq!(
            canonicalize_path("/threads/abc?limit=10&after=42"),
            "/threads/abc?after=42&limit=10"
        );
    }

    #[test]
    fn canonicalize_path_repeated_keys() {
        // Repeated query keys must be preserved and sorted together.
        // Key "tag" appears twice: tag=b sorts before tag=a only if
        // the full pair (tag, b) < (tag, a), but since k is the same,
        // the value is the tiebreaker.
        assert_eq!(
            canonicalize_path("/objects/get?hashes=abc&hashes=def"),
            "/objects/get?hashes=abc&hashes=def"
        );
        assert_eq!(
            canonicalize_path("/search?tag=b&limit=5&tag=a"),
            "/search?limit=5&tag=a&tag=b"
        );
    }

    #[test]
    fn canonicalize_path_empty_value() {
        assert_eq!(
            canonicalize_path("/path?flag&other=val"),
            "/path?flag&other=val"
        );
    }

    #[test]
    fn signed_get_canonicalization_matches_server_repeated_keys() {
        // The canonicalization for signing must match the server's
        // auth.rs canonicalization. This test verifies that repeated
        // keys (like hashes=abc&hashes=def) are preserved in order
        // but sorted as pairs, and the full canonical string is
        // deterministic.
        let path_with_repeats = "/objects/get?hashes=def&hashes=abc&hashes=ghi";
        let canonical = canonicalize_path(path_with_repeats);
        // Sorted by key, then value:
        assert_eq!(canonical, "/objects/get?hashes=abc&hashes=def&hashes=ghi");
    }

    #[test]
    fn chunk_hashes_respects_body_budget() {
        let hashes: Vec<String> = (0..100).map(|i| format!("{:064x}", i)).collect();
        let chunks = chunk_hashes_for_body_budget(&hashes, 512);

        assert!(chunks.len() > 1);
        assert_eq!(chunks.iter().map(Vec::len).sum::<usize>(), hashes.len());
        for chunk in chunks {
            assert!(hashes_request_body_size(&chunk) <= 512);
        }
    }

    #[test]
    fn objects_has_response_must_partition_request() {
        let a = "11".repeat(32);
        let b = "22".repeat(32);
        ObjectsHasResponse {
            found_object_hashes: vec![a.clone()],
            missing_object_hashes: vec![b.clone()],
            ..Default::default()
        }
        .validate_against_request(&[a.clone(), b.clone()], &[])
        .unwrap();

        assert!(ObjectsHasResponse {
            found_object_hashes: vec![a.clone()],
            ..Default::default()
        }
        .validate_against_request(&[a.clone(), b.clone()], &[])
        .is_err());

        assert!(ObjectsHasResponse {
            found_blob_hashes: vec![a.clone()],
            missing_blob_hashes: vec![a.clone()],
            ..Default::default()
        }
        .validate_against_request(&[], &[a, b])
        .is_err());
    }

    #[test]
    fn closure_describe_response_validates_requested_roots() {
        let root = "11".repeat(32);
        let other = "22".repeat(32);
        let response: ObjectsClosureDescribeResponse = serde_json::from_value(serde_json::json!({
            "roots": [root.clone()],
            "complete": true,
            "object_hashes": [root.clone()],
            "blob_hashes": [],
            "missing_objects": [],
            "missing_blobs": [],
            "malformed_objects": [],
            "unsupported_objects": []
        }))
        .unwrap();
        response
            .validate_against_request(std::slice::from_ref(&root))
            .unwrap();
        assert!(response.validate_against_request(&[other]).is_err());

        let response: ObjectsClosureDescribeResponse = serde_json::from_value(serde_json::json!({
            "roots": [root.clone()],
            "complete": true,
            "object_hashes": [],
            "blob_hashes": [],
            "missing_objects": [],
            "missing_blobs": [],
            "malformed_objects": [],
            "unsupported_objects": []
        }))
        .unwrap();
        assert!(response.validate_against_request(&[root]).is_err());
    }

    #[test]
    fn admission_attestations_response_validates_subject_and_policy() {
        let subject = "11".repeat(32);
        let signature = base64::engine::general_purpose::STANDARD.encode([0_u8; 64]);
        let attestation_value = serde_json::json!({
            "schema": 1,
            "kind": "attestation",
            "subject_hash": subject,
            "policy": "local-node-v1",
            "claim": "accepted",
            "issuer": format!("fp:{}", "44".repeat(32)),
            "issued_at": "2026-05-30T00:00:00Z",
            "expires_at": null,
            "evidence": {},
            "signature": signature,
        });
        let attestation = lillux::sha256_hex(
            lillux::canonical_json(&attestation_value)
                .unwrap()
                .as_bytes(),
        );
        let response: AdmissionAttestationsForSubjectResponse =
            serde_json::from_value(serde_json::json!({
                "subject_hash": subject,
                "policy": "local-node-v1",
                "attestations": [{
                    "attestation_hash": attestation,
                    "subject_hash": "11".repeat(32),
                    "policy": "local-node-v1",
                    "claim": "accepted",
                    "issuer": format!("fp:{}", "44".repeat(32)),
                    "issued_at": "2026-05-30T00:00:00Z",
                    "expires_at": null,
                    "head_ref_path": "admissions/local-node-v1/subject/head",
                    "indexed_at": "2026-05-30T00:00:01Z",
                    "state": "accepted",
                    "attestation": attestation_value,
                }]
            }))
            .unwrap();
        response
            .validate_against_request(&"11".repeat(32), Some("local-node-v1"))
            .unwrap();
        assert!(response
            .validate_against_request(&"33".repeat(32), Some("local-node-v1"))
            .is_err());
        assert!(response
            .validate_against_request(&"11".repeat(32), Some("other"))
            .is_err());
    }

    #[test]
    fn sync_jobs_list_response_validates_state_filter() {
        let response: SyncJobsListResponse = serde_json::from_value(serde_json::json!({
            "jobs": [{
                "job_id": "job-a",
                "operation_type": "mirror_pull",
                "peer": "node-a",
                "state": "running",
                "phase": "fetching",
                "roots": ["11".repeat(32)],
                "heads": [],
                "uploaded_hashes": [],
                "fetched_hashes": ["22".repeat(32)],
                "attempt_count": 1,
                "max_attempts": 3,
                "last_error": null,
                "result": null,
                "created_at": "2026-05-30T00:00:00Z",
                "updated_at": "2026-05-30T00:00:01Z",
                "finished_at": null
            }]
        }))
        .unwrap();

        response.validate(Some("running")).unwrap();
        assert!(response.validate(Some("completed")).is_err());
    }

    #[test]
    fn federation_capabilities_response_validates_core_contract() {
        let response: FederationCapabilitiesResponse = serde_json::from_value(serde_json::json!({
            "protocol": {
                "name": "ryeos-distributed-substrate",
                "versions": [1],
                "preferred_version": 1
            },
            "identity": {
                "principal_id": "principal:node",
                "fingerprint": "fp:node",
                "site_id": "site:node"
            },
            "object_kinds": ["chain_state", "attestation"],
            "services": {
                "objects": {"closure_describe": true},
                "admission": {"submit": true}
            },
            "limits": {
                "default_max_objects_per_closure": 4096
            }
        }))
        .unwrap();
        response.validate().unwrap();

        let response: FederationCapabilitiesResponse = serde_json::from_value(serde_json::json!({
            "protocol": {
                "name": "ryeos-distributed-substrate",
                "versions": [2],
                "preferred_version": 2
            },
            "identity": {
                "principal_id": "principal:node",
                "fingerprint": "fp:node",
                "site_id": "site:node"
            },
            "object_kinds": ["attestation"],
            "services": {"objects": {}, "admission": {}},
            "limits": {"default_max_objects_per_closure": 4096}
        }))
        .unwrap();
        assert!(response.validate().is_err());
    }

    #[test]
    fn federation_heads_list_response_validates_prefix_and_hashes() {
        let response: FederationHeadsListResponse = serde_json::from_value(serde_json::json!({
            "prefix": "admissions",
            "limit": 10,
            "truncated": false,
            "heads": [{
                "namespace": "admissions/policy-a",
                "name": "subject-a",
                "ref_path": "admissions/policy-a/subject-a/head",
                "target_hash": "11".repeat(32),
                "signer": "aa".repeat(32),
                "updated_at": "2026-05-30T00:00:01Z",
                "signed_ref": {
                    "schema": 1,
                    "kind": "signed_ref",
                    "ref_path": "admissions/policy-a/subject-a/head",
                    "target_hash": "11".repeat(32),
                    "updated_at": "2026-05-30T00:00:01Z",
                    "signer": "aa".repeat(32),
                    "signature": "sig"
                }
            }]
        }))
        .unwrap();
        response.validate("admissions").unwrap();
        assert!(response.validate("collections").is_err());

        let response: FederationHeadsListResponse = serde_json::from_value(serde_json::json!({
            "prefix": "admissions",
            "limit": 10,
            "truncated": false,
            "heads": [{
                "namespace": "admissions/policy-a",
                "name": "subject-a",
                "ref_path": "admissions/policy-a/subject-a/head",
                "target_hash": "AA".repeat(32),
                "signer": "aa".repeat(32),
                "updated_at": "2026-05-30T00:00:01Z",
                "signed_ref": {
                    "schema": 1,
                    "kind": "signed_ref",
                    "ref_path": "admissions/policy-a/subject-a/head",
                    "target_hash": "AA".repeat(32),
                    "updated_at": "2026-05-30T00:00:01Z",
                    "signer": "aa".repeat(32),
                    "signature": "sig"
                }
            }]
        }))
        .unwrap();
        assert!(response.validate("admissions").is_err());
    }

    #[test]
    fn sync_jobs_inspect_response_validates_requested_id() {
        let response: SyncJobsInspectResponse = serde_json::from_value(serde_json::json!({
            "status": "found",
            "job": {
                "job_id": "job-a",
                "operation_type": "mirror_pull",
                "peer": null,
                "state": "completed",
                "phase": "done",
                "roots": [],
                "heads": ["33".repeat(32)],
                "uploaded_hashes": [],
                "fetched_hashes": [],
                "attempt_count": 1,
                "max_attempts": 3,
                "last_error": null,
                "result": {"ok": true},
                "created_at": "2026-05-30T00:00:00Z",
                "updated_at": "2026-05-30T00:00:01Z",
                "finished_at": "2026-05-30T00:00:01Z"
            },
            "attempts": [{
                "attempt_id": "attempt-a",
                "job_id": "job-a",
                "attempt_number": 1,
                "worker_id": "worker-a",
                "state": "completed",
                "phase": "done",
                "started_at": "2026-05-30T00:00:00Z",
                "updated_at": "2026-05-30T00:00:01Z",
                "finished_at": "2026-05-30T00:00:01Z",
                "error": null,
                "result": {"ok": true}
            }]
        }))
        .unwrap();

        response.validate_against_request("job-a").unwrap();
        assert!(response.validate_against_request("job-b").is_err());

        let mismatched_attempt: SyncJobsInspectResponse =
            serde_json::from_value(serde_json::json!({
                "status": "found",
                "job": {
                    "job_id": "job-a",
                    "operation_type": "mirror_pull",
                    "peer": null,
                    "state": "completed",
                    "phase": "done",
                    "roots": [],
                    "heads": [],
                    "uploaded_hashes": [],
                    "fetched_hashes": [],
                    "attempt_count": 1,
                    "max_attempts": 3,
                    "last_error": null,
                    "result": null,
                    "created_at": "2026-05-30T00:00:00Z",
                    "updated_at": "2026-05-30T00:00:01Z",
                    "finished_at": "2026-05-30T00:00:01Z"
                },
                "attempts": [{
                    "attempt_id": "attempt-b",
                    "job_id": "job-b",
                    "attempt_number": 1,
                    "worker_id": null,
                    "state": "completed",
                    "phase": "done",
                    "started_at": "2026-05-30T00:00:00Z",
                    "updated_at": "2026-05-30T00:00:01Z",
                    "finished_at": "2026-05-30T00:00:01Z",
                    "error": null,
                    "result": null
                }]
            }))
            .unwrap();
        assert!(mismatched_attempt
            .validate_against_request("job-a")
            .is_err());
    }

    #[test]
    fn sync_job_remote_record_rejects_uppercase_hashes() {
        let response: SyncJobsListResponse = serde_json::from_value(serde_json::json!({
            "jobs": [{
                "job_id": "job-a",
                "operation_type": "mirror_pull",
                "peer": null,
                "state": "running",
                "phase": "fetching",
                "roots": ["AA".repeat(32)],
                "heads": [],
                "uploaded_hashes": [],
                "fetched_hashes": [],
                "attempt_count": 1,
                "max_attempts": 3,
                "last_error": null,
                "result": null,
                "created_at": "2026-05-30T00:00:00Z",
                "updated_at": "2026-05-30T00:00:01Z",
                "finished_at": null
            }]
        }))
        .unwrap();

        assert!(response.validate(Some("running")).is_err());
    }
}
