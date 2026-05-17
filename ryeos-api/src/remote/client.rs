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

    /// Build a client from a named remote config + app state.
    pub fn from_named_remote(state: &AppState, remote_name: &str) -> Result<Self> {
        let remotes = super::config::load_remotes(&state.config.system_space_dir)?;
        let remote = super::config::get_remote(&remotes, remote_name)?;
        Ok(Self::new(&remote.url, &remote.principal_id, state.identity.clone()))
    }

    /// GET /public-key (no auth required).
    pub async fn get_public_key(&self) -> Result<PublicKeyResponse> {
        let url = format!("{}/public-key", self.base_url);
        let resp = self.http.get(&url)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        let status = resp.status();
        let body: Value = resp.json().await
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
            vault_fingerprint: body["vault_fingerprint"].as_str().map(String::from),
        })
    }

    /// GET /health (no auth required).
    pub async fn get_health(&self) -> Result<Value> {
        let url = format!("{}/health", self.base_url);
        let resp = self.http.get(&url)
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
        let resp = self.http.get(&url)
            .send()
            .await
            .with_context(|| format!("failed to connect to {}", url))?;
        let status = resp.status();
        let body: Value = resp.json().await
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
        let body = serde_json::json!({ "hashes": hashes });
        let resp = self.signed_post("/objects/has", &body).await?;
        Ok(ObjectsHasResponse {
            found: resp["found"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            missing: resp["missing"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
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
            blob_hashes: resp["blob_hashes"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            object_hashes: resp["object_hashes"].as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default(),
        })
    }

    /// POST /objects/get (authenticated).
    ///
    /// Server returns `{ "entries": [{ "hash": "...", "kind": "blob"|"object"|"missing", ... }] }`.
    pub async fn objects_get(&self, hashes: &[String]) -> Result<ObjectsGetResponse> {
        let body = serde_json::json!({ "hashes": hashes });
        let resp = self.signed_post("/objects/get", &body).await?;
        let entries = resp.get("entries")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                let hash = entry.get("hash")?.as_str()?.to_string();
                let kind = entry.get("kind")?.as_str()?.to_string();
                Some(CasEntry { hash, kind, data: entry.get("data").and_then(|v| v.as_str()).map(String::from), value: entry.get("value").cloned() })
            })
            .collect();
        Ok(ObjectsGetResponse { entries })
    }

    /// POST /push-head (authenticated).
    pub async fn push_head(
        &self,
        project_path: &str,
        snapshot_hash: &str,
    ) -> Result<Value> {
        let body = serde_json::json!({
            "project_path": project_path,
            "snapshot_hash": snapshot_hash,
        });
        self.signed_post("/push-head", &body).await
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
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            granted_by: resp["granted_by"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            created_at: resp["created_at"]
                .as_str()
                .unwrap_or("")
                .to_string(),
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

    // ── Internal ──────────────────────────────────────────────────

    async fn signed_get(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        // Sign with the full path+query — canonicalize_path inside
        // sign_request will sort query params to match server-side.
        let headers = self.sign_request("GET", path, &[])?;

        let resp = self.http.get(&url)
            .header("x-ryeos-key-id", &headers.key_id)
            .header("x-ryeos-timestamp", &headers.timestamp)
            .header("x-ryeos-nonce", &headers.nonce)
            .header("x-ryeos-signature", &headers.signature)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?;

        let status = resp.status();
        let resp_body: Value = resp.json().await
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

        let resp = self.http.post(&url)
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
        let resp_body: Value = resp.json().await
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
        let sig = lillux::crypto::Signer::sign(self.identity.signing_key(), content_hash.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

        Ok(SignedHeaders {
            key_id: self.identity.principal_id(),
            timestamp: timestamp.to_string(),
            nonce,
            signature: sig_b64,
        })
    }
}

struct SignedHeaders {
    key_id: String,
    timestamp: String,
    nonce: String,
    signature: String,
}

/// Sort query parameters alphabetically and normalise the path.
/// Matches `ryeosd/src/auth.rs::canonical_path` — uses a Vec to
/// preserve repeated keys, matching server canonicalization exactly.
fn canonicalize_path(path_and_query: &str) -> String {
    if let Some((path, query)) = path_and_query.split_once('?') {
        let mut params: Vec<(&str, &str)> = query
            .split('&')
            .filter_map(|pair| pair.split_once('=').or_else(|| {
                if pair.is_empty() { None } else { Some((pair, "")) }
            }))
            .collect();
        params.sort();
        let sorted: Vec<String> = params
            .iter()
            .map(|(k, v)| {
                if v.is_empty() { k.to_string() } else { format!("{k}={v}") }
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
    pub vault_fingerprint: Option<String>,
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
        self.entries.iter()
            .find(|e| e.hash == hash && e.kind == "object")
            .and_then(|e| e.value.clone())
    }

    /// Find a blob entry by hash, returning decoded bytes.
    pub fn find_blob(&self, hash: &str) -> Option<Vec<u8>> {
        use base64::Engine;
        self.entries.iter()
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
}
