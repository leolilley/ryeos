use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Result};
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde_json::{json, Value};

use crate::cas::CasStore;

pub struct RegistryStore {
    cas_root: PathBuf,
    cas: CasStore,
}

impl RegistryStore {
    pub fn new(cas_root: PathBuf) -> Self {
        let registry_cas_root = cas_root.join("registry").join("objects");
        Self {
            cas_root,
            cas: CasStore::new(registry_cas_root),
        }
    }

    fn registry_dir(&self) -> PathBuf {
        self.cas_root.join("registry")
    }

    fn index_head_path(&self) -> PathBuf {
        self.registry_dir().join("index").join("head")
    }

    fn namespace_dir(&self) -> PathBuf {
        self.registry_dir().join("namespaces")
    }

    fn identities_dir(&self) -> PathBuf {
        self.registry_dir().join("identities")
    }

    fn read_head(&self) -> Result<Option<String>> {
        let path = self.index_head_path();
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(fs::read_to_string(&path)?.trim().to_string()))
    }

    fn write_head(&self, hash: &str) -> Result<()> {
        let path = self.index_head_path();
        crate::cas::atomic_write(&path, hash.as_bytes())?;
        Ok(())
    }

    fn empty_index() -> Value {
        let now = chrono::Utc::now().to_rfc3339();
        json!({
            "kind": "registry-index/v1",
            "schema": 1,
            "updated_at": now,
            "entries": {
                "tool": {},
                "directive": {},
                "knowledge": {},
                "bundle": {}
            }
        })
    }

    pub fn load_index(&self) -> Result<Value> {
        match self.read_head()? {
            None => Ok(Self::empty_index()),
            Some(head) => match self.cas.get_object(&head)? {
                Some(obj) => Ok(obj),
                None => Ok(Self::empty_index()),
            },
        }
    }

    pub fn search_items(
        &self,
        query: Option<&str>,
        kind: Option<&str>,
        namespace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let index = self.load_index()?;
        let entries = match index.get("entries").and_then(|e| e.as_object()) {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let valid_kinds = ["tool", "directive", "knowledge", "bundle"];
        let kinds_to_search: Vec<&str> = if let Some(k) = kind {
            if valid_kinds.contains(&k) {
                vec![k]
            } else {
                valid_kinds.to_vec()
            }
        } else {
            valid_kinds.to_vec()
        };

        let mut results = Vec::new();
        for t in &kinds_to_search {
            let type_entries = match entries.get(*t).and_then(|v| v.as_object()) {
                Some(e) => e,
                None => continue,
            };
            for (iid, entry) in type_entries {
                if let Some(ns) = namespace {
                    if entry.get("namespace").and_then(|v| v.as_str()) != Some(ns) {
                        continue;
                    }
                }
                if let Some(q) = query {
                    if !iid.to_lowercase().contains(&q.to_lowercase()) {
                        continue;
                    }
                }
                results.push(json!({
                    "kind": t,
                    "item_id": iid,
                    "namespace": entry.get("namespace"),
                    "latest_version": entry.get("latest_version"),
                    "owner": entry.get("owner"),
                }));
                if results.len() >= limit {
                    return Ok(results);
                }
            }
        }
        Ok(results)
    }

    pub fn get_item(&self, kind: &str, item_id: &str) -> Result<Option<Value>> {
        let index = self.load_index()?;
        Ok(index
            .get("entries")
            .and_then(|e| e.get(kind))
            .and_then(|k| k.get(item_id))
            .cloned())
    }

    pub fn get_version(&self, kind: &str, item_id: &str, version: &str) -> Result<Option<Value>> {
        let item = match self.get_item(kind, item_id)? {
            Some(i) => i,
            None => return Ok(None),
        };
        Ok(item.get("versions").and_then(|v| v.get(version)).cloned())
    }

    pub fn publish_item(
        &self,
        kind: &str,
        item_id: &str,
        version: &str,
        manifest_hash: &str,
        publisher_fp: &str,
    ) -> Result<Value> {
        let valid_kinds = ["tool", "directive", "knowledge", "bundle"];
        if !valid_kinds.contains(&kind) {
            return Ok(json!({ "ok": false, "error": format!("Invalid kind: {kind}") }));
        }
        if item_id.is_empty()
            || version.is_empty()
            || manifest_hash.is_empty()
            || publisher_fp.is_empty()
        {
            return Ok(json!({ "ok": false, "error": "Missing required field" }));
        }

        let namespace = if item_id.contains('/') {
            item_id.split('/').next().unwrap_or(item_id)
        } else {
            item_id
        };

        // Verify publisher owns or is authorized for the namespace
        if let Some(ns_owner) = self.namespace_owner(namespace)? {
            if ns_owner != publisher_fp {
                return Ok(json!({
                    "ok": false,
                    "error": format!("publisher {publisher_fp} is not authorized for namespace '{namespace}' (owned by {ns_owner})")
                }));
            }
        }

        let lock_path = self.registry_dir().join("index.lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock_file = fs::File::create(&lock_path)?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX);
            }
        }

        let result = (|| -> Result<Value> {
            let mut index = self.load_index()?;
            let entries = index
                .get_mut("entries")
                .and_then(|e| e.as_object_mut())
                .ok_or_else(|| anyhow::anyhow!("invalid index structure"))?;

            let type_entries = entries
                .entry(kind)
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid type entries"))?;

            if let Some(existing) = type_entries.get(item_id) {
                if let Some(ver) = existing.get("versions").and_then(|v| v.get(version)) {
                    if ver.get("manifest_hash").and_then(|v| v.as_str()) == Some(manifest_hash) {
                        let head = self.read_head()?;
                        return Ok(json!({ "ok": true, "head": head, "skipped": true }));
                    }
                    return Ok(
                        json!({ "ok": false, "error": format!("Version {version} already exists with different hash") }),
                    );
                }
            }

            if !type_entries.contains_key(item_id) {
                type_entries.insert(
                    item_id.to_string(),
                    json!({
                        "namespace": namespace,
                        "owner": publisher_fp,
                        "latest_version": version,
                        "versions": {},
                    }),
                );
            }

            let entry = type_entries
                .get_mut(item_id)
                .unwrap()
                .as_object_mut()
                .unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let versions = entry
                .entry("versions")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid versions"))?;

            versions.insert(
                version.to_string(),
                json!({
                    "manifest_hash": manifest_hash,
                    "published_at": now,
                    "publisher": publisher_fp,
                }),
            );
            entry.insert("latest_version".to_string(), json!(version));

            index
                .as_object_mut()
                .unwrap()
                .insert("updated_at".to_string(), json!(now));

            let new_head = self.cas.store_object(&index)?;
            self.write_head(&new_head)?;

            Ok(json!({ "ok": true, "head": new_head }))
        })();

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(lock_file.as_raw_fd(), libc::LOCK_UN);
            }
        }

        let _ = lock_file;
        result
    }

    pub fn claim_namespace(&self, namespace: &str, owner_fp: &str) -> Result<Value> {
        if namespace.is_empty() {
            return Ok(json!({ "ok": false, "error": "Missing namespace" }));
        }
        if namespace.contains('/')
            || namespace.contains('\\')
            || namespace.contains("..")
            || namespace.contains('\0')
        {
            return Ok(json!({ "ok": false, "error": "invalid namespace" }));
        }
        let ns_dir = self.namespace_dir();
        fs::create_dir_all(&ns_dir)?;
        let ns_file = ns_dir.join(namespace);

        if ns_file.exists() {
            let record: Value = serde_json::from_slice(&fs::read(&ns_file)?)?;
            if record.get("owner").and_then(|v| v.as_str()) != Some(owner_fp) {
                return Ok(
                    json!({ "ok": false, "error": format!("Namespace '{namespace}' already claimed") }),
                );
            }
            return Ok(json!({ "ok": true, "skipped": true }));
        }

        let record = json!({ "owner": owner_fp });
        crate::cas::atomic_write(&ns_file, &serde_json::to_vec(&record)?)?;

        Ok(json!({ "ok": true }))
    }

    pub fn register_identity(&self, identity_doc: &Value) -> Result<Value> {
        let principal_id = identity_doc
            .get("principal_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !principal_id.starts_with("fp:") {
            return Ok(json!({ "ok": false, "error": "Invalid principal_id format" }));
        }
        let fingerprint = &principal_id[3..];
        if fingerprint.is_empty()
            || fingerprint.contains('/')
            || fingerprint.contains('\\')
            || fingerprint.contains("..")
            || fingerprint.contains('\0')
        {
            return Ok(json!({ "ok": false, "error": "invalid fingerprint" }));
        }

        // Verify self-signature on identity document
        if let Err(e) = verify_identity_self_signature(identity_doc, fingerprint) {
            return Ok(json!({ "ok": false, "error": format!("identity signature invalid: {e}") }));
        }

        let identity_hash = self.cas.store_object(identity_doc)?;

        let id_dir = self.identities_dir();
        fs::create_dir_all(&id_dir)?;
        let id_file = id_dir.join(fingerprint);
        crate::cas::atomic_write(&id_file, identity_hash.as_bytes())?;

        Ok(json!({ "ok": true, "identity_hash": identity_hash }))
    }

    pub fn lookup_identity(&self, fingerprint: &str) -> Result<Option<Value>> {
        if fingerprint.is_empty()
            || fingerprint.contains('/')
            || fingerprint.contains('\\')
            || fingerprint.contains("..")
            || fingerprint.contains('\0')
        {
            return Ok(None);
        }
        let id_file = self.identities_dir().join(fingerprint);
        if !id_file.exists() {
            return Ok(None);
        }
        let identity_hash = fs::read_to_string(&id_file)?.trim().to_string();
        self.cas.get_object(&identity_hash)
    }

    /// Look up who owns a namespace. Returns None if unclaimed.
    fn namespace_owner(&self, namespace: &str) -> Result<Option<String>> {
        let ns_file = self.namespace_dir().join(namespace);
        if !ns_file.exists() {
            return Ok(None);
        }
        let record: Value = serde_json::from_slice(&fs::read(&ns_file)?)?;
        Ok(record
            .get("owner")
            .and_then(|v| v.as_str())
            .map(String::from))
    }
}

// ── Identity signature verification ─────────────────────────────────

/// Verify the self-signature on a public identity document.
///
/// The identity doc structure (from `ryeosd/src/identity.rs`):
/// ```json
/// {
///   "kind": "identity/v1",
///   "principal_id": "fp:<fingerprint>",
///   "signing_key": "ed25519:<base64>",
///   "created_at": "...",
///   "_signature": {
///     "signer": "fp:<fingerprint>",
///     "sig": "<base64>",
///     "signed_at": "..."
///   }
/// }
/// ```
///
/// The signature is computed over the canonical JSON of the unsigned
/// document (all fields except `_signature`).
pub(crate) fn verify_identity_self_signature(doc: &Value, expected_fp: &str) -> Result<()> {
    // Extract signing key
    let signing_key_str = doc
        .get("signing_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing signing_key"))?;

    if !signing_key_str.starts_with("ed25519:") {
        bail!("unsupported signing key format: {signing_key_str}");
    }
    let key_b64 = &signing_key_str["ed25519:".len()..];
    let key_bytes = base64::engine::general_purpose::STANDARD.decode(key_b64)?;
    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("signing key must be 32 bytes"))?;
    let verifying_key = VerifyingKey::from_bytes(&key_array)?;

    // Verify fingerprint matches the public key
    let actual_fp = crate::cas::sha256_hex(verifying_key.as_bytes());
    if actual_fp != expected_fp {
        bail!("signing key fingerprint mismatch: expected {expected_fp}, got {actual_fp}");
    }

    // Extract signature
    let sig_section = doc
        .get("_signature")
        .ok_or_else(|| anyhow::anyhow!("missing _signature"))?;
    let sig_b64 = sig_section
        .get("sig")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing _signature.sig"))?;

    // Verify signer matches principal
    let signer = sig_section
        .get("signer")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let expected_signer = format!("fp:{expected_fp}");
    if signer != expected_signer {
        bail!("signer mismatch: expected {expected_signer}, got {signer}");
    }

    // Reconstruct the unsigned payload (same as identity.rs build_public_identity)
    let unsigned = json!({
        "kind": doc.get("kind").and_then(|v| v.as_str()).unwrap_or("identity/v1"),
        "principal_id": doc.get("principal_id").and_then(|v| v.as_str()).unwrap_or(""),
        "signing_key": signing_key_str,
        "created_at": doc.get("created_at").and_then(|v| v.as_str()).unwrap_or(""),
    });
    let payload = serde_json::to_vec(&unsigned)?;

    // Decode and verify signature
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64)?;
    let signature = Signature::from_slice(&sig_bytes)?;
    verifying_key
        .verify(&payload, &signature)
        .map_err(|_| anyhow::anyhow!("Ed25519 signature verification failed"))?;

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn make_identity(sk: &SigningKey) -> (Value, String) {
        let vk = sk.verifying_key();
        let fp = crate::cas::sha256_hex(vk.as_bytes());
        let principal_id = format!("fp:{fp}");
        let signing_key_str = format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(vk.as_bytes())
        );
        let created_at = "2026-04-10T00:00:00Z";

        let unsigned = json!({
            "kind": "identity/v1",
            "principal_id": principal_id,
            "signing_key": signing_key_str,
            "created_at": created_at,
        });
        let payload = serde_json::to_vec(&unsigned).unwrap();
        let signature: Signature = sk.sign(&payload);

        let doc = json!({
            "kind": "identity/v1",
            "principal_id": principal_id,
            "signing_key": signing_key_str,
            "created_at": created_at,
            "_signature": {
                "signer": principal_id,
                "sig": base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
                "signed_at": created_at,
            }
        });
        (doc, fp)
    }

    #[test]
    fn verify_valid_identity_signature() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let (doc, fp) = make_identity(&sk);
        assert!(verify_identity_self_signature(&doc, &fp).is_ok());
    }

    #[test]
    fn verify_identity_wrong_fingerprint() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let (doc, _) = make_identity(&sk);
        let err = verify_identity_self_signature(&doc, "wrong_fp").unwrap_err();
        assert!(err.to_string().contains("fingerprint mismatch"));
    }

    #[test]
    fn verify_identity_tampered_doc() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let (mut doc, fp) = make_identity(&sk);
        // Tamper with created_at — signature should fail
        doc.as_object_mut()
            .unwrap()
            .insert("created_at".to_string(), json!("2099-01-01T00:00:00Z"));
        let err = verify_identity_self_signature(&doc, &fp).unwrap_err();
        assert!(err.to_string().contains("signature verification failed"));
    }

    #[test]
    fn verify_identity_wrong_signer() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let (mut doc, fp) = make_identity(&sk);
        // Tamper with signer field
        doc.get_mut("_signature")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("signer".to_string(), json!("fp:wrong"));
        let err = verify_identity_self_signature(&doc, &fp).unwrap_err();
        assert!(err.to_string().contains("signer mismatch"));
    }

    #[test]
    fn verify_identity_missing_signature() {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let (doc_with_sig, fp) = make_identity(&sk);
        // Remove _signature from a valid doc
        let mut doc = doc_with_sig.clone();
        doc.as_object_mut().unwrap().remove("_signature");
        let err = verify_identity_self_signature(&doc, &fp).unwrap_err();
        assert!(
            err.to_string().contains("missing _signature"),
            "expected 'missing _signature', got: {err}"
        );
    }

    #[test]
    fn register_identity_rejects_bad_signature() {
        let dir = std::env::temp_dir().join(format!(
            "rye_registry_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let store = RegistryStore::new(dir.clone());

        // Doc with wrong signature
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let (mut doc, _fp) = make_identity(&sk);
        doc.as_object_mut()
            .unwrap()
            .insert("created_at".to_string(), json!("tampered"));

        let result = store.register_identity(&doc).unwrap();
        assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert!(result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("identity signature invalid"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn publish_rejects_wrong_namespace_owner() {
        let dir = std::env::temp_dir().join(format!(
            "rye_registry_test2_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let store = RegistryStore::new(dir.clone());

        // Claim namespace as "owner_a"
        store.claim_namespace("myns", "owner_a").unwrap();

        // Try to publish as "owner_b"
        let result = store
            .publish_item("tool", "myns/mytool", "1.0.0", "hash123", "owner_b")
            .unwrap();
        assert_eq!(result.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert!(result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("not authorized"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
