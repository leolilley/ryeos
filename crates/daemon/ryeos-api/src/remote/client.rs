//! Outbound HTTP client for communicating with remote ryEOS nodes.
//!
//! Wraps `reqwest` with request signing and provides typed methods
//! for the remote API surface: CAS operations, push-head, execute,
//! public-key discovery, etc.

use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use serde_json::Value;

use ryeos_app::identity::NodeIdentity;
use ryeos_app::state::AppState;
use ryeos_state::ignore::IgnoreConfig;

const HASH_REQUEST_BODY_BUDGET_BYTES: usize = 900 * 1024;

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
            http: reqwest::Client::new(),
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
        let remotes =
            super::config::load_remotes_layered(&state.config.system_space_dir, project_path)?;
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
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .with_context(|| format!("failed to parse /public-key response from {}", url))?;
        if !status.is_success() {
            anyhow::bail!("GET {} returned {}: {}", url, status, body);
        }
        Ok(PublicKeyResponse {
            principal_id: body["principal_id"]
                .as_str()
                .context("missing principal_id in /public-key response")?
                .to_string(),
            fingerprint: body["fingerprint"]
                .as_str()
                .context("missing fingerprint in /public-key response")?
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
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("GET {} returned {}: {}", url, status, body);
        }
        Ok(body)
    }

    /// GET /ingest-ignore (no auth required).
    ///
    /// Returns the remote node's ingest ignore config as a typed struct.
    pub async fn get_ingest_ignore(&self) -> Result<IgnoreConfig> {
        let url = format!("{}/ingest-ignore", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .with_context(|| format!("failed to parse /ingest-ignore response from {}", url))?;
        if !status.is_success() {
            anyhow::bail!("GET {} returned {}: {}", url, status, body);
        }
        // Server returns the IgnoreConfig as JSON (with `patterns` field)
        let config: IgnoreConfig = serde_json::from_value(body)
            .context("failed to parse /ingest-ignore response as IgnoreConfig")?;
        Ok(config)
    }

    /// POST /objects/has (authenticated).
    pub async fn objects_has(&self, hashes: &[String]) -> Result<ObjectsHasResponse> {
        if hashes_request_body_size(hashes) > HASH_REQUEST_BODY_BUDGET_BYTES {
            let mut found = Vec::new();
            let mut missing = Vec::new();
            for chunk in chunk_hashes_for_body_budget(hashes, HASH_REQUEST_BODY_BUDGET_BYTES) {
                let resp = self.objects_has_once(&chunk).await?;
                found.extend(resp.found);
                missing.extend(resp.missing);
            }
            return Ok(ObjectsHasResponse { found, missing });
        }
        self.objects_has_once(hashes).await
    }

    async fn objects_has_once(&self, hashes: &[String]) -> Result<ObjectsHasResponse> {
        let body = serde_json::json!({ "hashes": hashes });
        let resp = self.signed_post("/objects/has", &body).await?;
        Ok(ObjectsHasResponse {
            found: resp["found"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            missing: resp["missing"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// POST /objects/put (authenticated).
    ///
    /// Blobs are base64-encoded raw bytes. Objects must be wrapped in
    /// `{ "value": ... }` per the server's `ObjectEntry` schema.
    pub async fn objects_put(
        &self,
        blobs: &[BlobUpload],
        objects: &[Value],
    ) -> Result<ObjectsPutResponse> {
        // Server expects objects as [{ "value": {...} }, ...]
        let wrapped_objects: Vec<Value> = objects
            .iter()
            .map(|obj| serde_json::json!({ "value": obj }))
            .collect();
        let body = serde_json::json!({
            "blobs": blobs,
            "objects": wrapped_objects,
        });
        let resp = self.signed_post("/objects/put", &body).await?;
        Ok(ObjectsPutResponse {
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
        if hashes_request_body_size(hashes) > HASH_REQUEST_BODY_BUDGET_BYTES {
            let mut entries = Vec::new();
            for chunk in chunk_hashes_for_body_budget(hashes, HASH_REQUEST_BODY_BUDGET_BYTES) {
                entries.extend(self.objects_get_once(&chunk).await?.entries);
            }
            return Ok(ObjectsGetResponse { entries });
        }
        self.objects_get_once(hashes).await
    }

    async fn objects_get_once(&self, hashes: &[String]) -> Result<ObjectsGetResponse> {
        let body = serde_json::json!({ "hashes": hashes });
        let resp = self.signed_post("/objects/get", &body).await?;
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
        Ok(ObjectsGetResponse { entries })
    }

    /// POST /push-head (authenticated).
    pub async fn push_head(&self, project_path: &str, snapshot_hash: &str) -> Result<Value> {
        let body = serde_json::json!({
            "project_path": project_path,
            "snapshot_hash": snapshot_hash,
        });
        self.signed_post("/push-head", &body).await
    }

    /// POST /project/apply-snapshot (authenticated).
    pub async fn apply_project_snapshot(
        &self,
        project_path: &str,
        snapshot_hash: &str,
        expected_deployed_hash: Option<&str>,
        force: bool,
    ) -> Result<Value> {
        let mut body = serde_json::json!({
            "project_path": project_path,
            "snapshot_hash": snapshot_hash,
            "force": force,
        });
        if let Some(expected) = expected_deployed_hash {
            body.as_object_mut()
                .unwrap()
                .insert("expected_deployed_hash".into(), serde_json::json!(expected));
        }
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
        project_path: &str,
        parameters: &Value,
        project_source: &str,
    ) -> Result<Value> {
        let body = serde_json::json!({
            "item_ref": item_ref,
            "project_path": project_path,
            "parameters": parameters,
            "project_source": { "kind": project_source },
        });
        self.signed_post("/execute", &body).await
    }

    /// POST /execute with full request options (operation, inputs).
    ///
    /// This is the extended variant used by target-site forwarding
    /// which needs to forward operation and inputs fields that the
    /// basic `execute()` method does not support.
    /// POST /execute with optional operation/inputs overrides.
    ///
    /// Used by the shared unary forward helper. Note: `target_site_id`
    /// is intentionally **not** forwarded to prevent forwarding loops —
    /// if site A forwards to site B, B should execute locally, not
    /// attempt to forward again.
    pub async fn execute_with_options(
        &self,
        item_ref: &str,
        project_path: &str,
        parameters: &Value,
        project_source: &str,
        operation: Option<&str>,
        inputs: Option<&Value>,
    ) -> Result<Value> {
        let mut body = serde_json::json!({
            "item_ref": item_ref,
            "project_path": project_path,
            "parameters": parameters,
            "project_source": { "kind": project_source },
        });
        if let Some(op) = operation {
            body["operation"] = Value::String(op.to_string());
        }
        if let Some(inputs) = inputs {
            body["inputs"] = inputs.clone();
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
        self.signed_post("/bundle/export", &body).await
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
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?;

        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .with_context(|| format!("failed to parse response from GET {}", url))?;

        if !status.is_success() {
            anyhow::bail!("GET {} returned {}: {}", url, status, resp_body);
        }

        Ok(resp_body)
    }

    async fn signed_post(&self, path: &str, body: &Value) -> Result<Value> {
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

        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .with_context(|| format!("failed to parse response from POST {}", url))?;

        if !status.is_success() {
            anyhow::bail!("POST {} returned {}: {}", url, status, resp_body);
        }

        Ok(resp_body)
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
                pair.split_once('=').or_else(|| {
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
    pub vault_fingerprint: String,
    /// Daemon site identity (e.g. `"site:my-hostname"`).
    /// Discovered from the remote's `/public-key` response.
    pub site_id: String,
}

#[derive(Debug, Clone)]
pub struct ObjectsHasResponse {
    pub found: Vec<String>,
    pub missing: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ObjectsPutResponse {
    pub blob_hashes: Vec<String>,
    pub object_hashes: Vec<String>,
}

/// A single entry from the `objects/get` response.
#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct AuthorizeKeyResponse {
    pub fingerprint: String,
    pub label: String,
    pub scopes: Vec<String>,
    pub granted_by: String,
    pub created_at: String,
}

/// A blob to upload, with base64-encoded data.
#[derive(serde::Serialize)]
pub struct BlobUpload {
    pub data: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
