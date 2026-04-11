use std::fs;
use std::path::PathBuf;

use anyhow::Result;
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
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, hash.as_bytes())?;
        fs::rename(&tmp, &path)?;
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
                    return Ok(json!({ "ok": false, "error": format!("Version {version} already exists with different hash") }));
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
        let tmp = ns_file.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(&record)?)?;
        fs::rename(&tmp, &ns_file)?;

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

        let identity_hash = self.cas.store_object(identity_doc)?;

        let id_dir = self.identities_dir();
        fs::create_dir_all(&id_dir)?;
        let id_file = id_dir.join(fingerprint);
        let tmp = id_file.with_extension("tmp");
        fs::write(&tmp, identity_hash.as_bytes())?;
        fs::rename(&tmp, &id_file)?;

        Ok(json!({ "ok": true, "identity_hash": identity_hash }))
    }

    pub fn lookup_identity(&self, fingerprint: &str) -> Result<Option<Value>> {
        let id_file = self.identities_dir().join(fingerprint);
        if !id_file.exists() {
            return Ok(None);
        }
        let identity_hash = fs::read_to_string(&id_file)?.trim().to_string();
        self.cas.get_object(&identity_hash)
    }
}
