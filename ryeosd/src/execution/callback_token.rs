use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

#[derive(Debug, Clone)]
pub struct CallbackCapability {
    pub token: String,
    pub invocation_id: String,
    pub thread_id: String,
    pub project_path: PathBuf,
    pub allowed_primaries: Vec<String>,
    pub expires_at: Instant,
}

pub struct CallbackCapabilityStore {
    capabilities: Mutex<HashMap<String, CallbackCapability>>,
}

impl CallbackCapabilityStore {
    pub fn new() -> Self {
        Self {
            capabilities: Mutex::new(HashMap::new()),
        }
    }

    pub fn generate(
        &self,
        thread_id: &str,
        project_path: PathBuf,
        allowed_primaries: Vec<String>,
        ttl: Duration,
    ) -> CallbackCapability {
        let random_bytes: [u8; 32] = rand::random();
        let hex = crate::cas::sha256_hex(&random_bytes);
        let token = format!("cbt-{hex}");

        let inv_bytes: [u8; 16] = rand::random();
        let inv_hex = crate::cas::sha256_hex(&inv_bytes);
        let invocation_id = format!("inv-{}", &inv_hex[..12]);

        let cap = CallbackCapability {
            token: token.clone(),
            invocation_id,
            thread_id: thread_id.to_string(),
            project_path,
            allowed_primaries,
            expires_at: Instant::now() + ttl,
        };

        self.capabilities
            .lock()
            .unwrap()
            .insert(token, cap.clone());
        cap
    }

    pub fn validate(
        &self,
        token: &str,
        thread_id: &str,
        project_path: &std::path::Path,
    ) -> Result<CallbackCapability> {
        let map = self.capabilities.lock().unwrap();
        let cap = map
            .get(token)
            .ok_or_else(|| anyhow::anyhow!("invalid callback capability"))?;

        if Instant::now() > cap.expires_at {
            bail!("callback capability expired");
        }

        if cap.thread_id != thread_id {
            bail!("callback capability does not match thread_id");
        }

        if cap.project_path != project_path {
            bail!("callback capability does not match project_path");
        }

        Ok(cap.clone())
    }

    pub fn validate_primary(
        &self,
        token: &str,
        thread_id: &str,
        project_path: &std::path::Path,
        primary: &str,
    ) -> Result<CallbackCapability> {
        let cap = self.validate(token, thread_id, project_path)?;
        if !cap.allowed_primaries.contains(&primary.to_string()) {
            bail!(
                "capability does not allow primary '{}' (allowed: {:?})",
                primary,
                cap.allowed_primaries
            );
        }
        Ok(cap)
    }

    pub fn invalidate(&self, token: &str) {
        self.capabilities.lock().unwrap().remove(token);
    }

    /// Validate callback token + thread_id without requiring project_path.
    /// Used by runtime.* UDS methods that don't carry project_path in params.
    pub fn validate_token_and_thread(
        &self,
        token: &str,
        thread_id: &str,
    ) -> Result<CallbackCapability> {
        let map = self.capabilities.lock().unwrap();
        let cap = map
            .get(token)
            .ok_or_else(|| anyhow::anyhow!("invalid callback capability"))?;

        if Instant::now() > cap.expires_at {
            bail!("callback capability expired");
        }

        if cap.thread_id != thread_id {
            bail!("callback capability does not match thread_id");
        }

        Ok(cap.clone())
    }

    pub fn invalidate_for_thread(&self, thread_id: &str) {
        let mut map = self.capabilities.lock().unwrap();
        map.retain(|_, cap| cap.thread_id != thread_id);
    }

    pub fn prune_expired(&self) -> usize {
        let mut map = self.capabilities.lock().unwrap();
        let now = Instant::now();
        let before = map.len();
        map.retain(|_, cap| cap.expires_at > now);
        before - map.len()
    }
}

pub fn compute_ttl(duration_seconds: Option<u64>) -> Duration {
    let secs = duration_seconds.unwrap_or(300);
    Duration::from_secs(secs.min(3600))
}

pub fn uds_allowed_primaries() -> Vec<String> {
    vec![
        "execute".to_string(),
        "fetch".to_string(),
        "sign".to_string(),
    ]
}

pub fn http_allowed_primaries() -> Vec<String> {
    vec!["execute".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_validate_round_trip() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-test123",
            PathBuf::from("/project"),
            uds_allowed_primaries(),
            Duration::from_secs(300),
        );
        assert!(cap.token.starts_with("cbt-"));
        assert!(cap.invocation_id.starts_with("inv-"));
        assert_eq!(cap.invocation_id.len(), 16);

        let validated = store
            .validate(&cap.token, "T-test123", PathBuf::from("/project").as_path())
            .unwrap();
        assert_eq!(validated.thread_id, "T-test123");
    }

    #[test]
    fn validate_rejects_unknown_token() {
        let store = CallbackCapabilityStore::new();
        let err = store
            .validate("cbt-nonexistent", "T-x", PathBuf::from("/p").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("invalid callback capability"));
    }

    #[test]
    fn invalidate_removes_capability() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-test",
            PathBuf::from("/p"),
            uds_allowed_primaries(),
            Duration::from_secs(300),
        );
        store.invalidate(&cap.token);
        assert!(store
            .validate(&cap.token, "T-test", PathBuf::from("/p").as_path())
            .is_err());
    }

    #[test]
    fn invalidate_for_thread_removes_matching() {
        let store = CallbackCapabilityStore::new();
        let cap1 = store.generate(
            "T-1",
            PathBuf::from("/p"),
            uds_allowed_primaries(),
            Duration::from_secs(300),
        );
        let cap2 = store.generate(
            "T-2",
            PathBuf::from("/p"),
            uds_allowed_primaries(),
            Duration::from_secs(300),
        );
        store.invalidate_for_thread("T-1");
        assert!(store
            .validate(&cap1.token, "T-1", PathBuf::from("/p").as_path())
            .is_err());
        assert!(store
            .validate(&cap2.token, "T-2", PathBuf::from("/p").as_path())
            .is_ok());
    }

    #[test]
    fn expired_capability_is_rejected() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-test",
            PathBuf::from("/p"),
            uds_allowed_primaries(),
            Duration::from_secs(0),
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        let err = store
            .validate(&cap.token, "T-test", PathBuf::from("/p").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("expired"));
    }

    #[test]
    fn wrong_thread_id_is_rejected() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-1",
            PathBuf::from("/p"),
            uds_allowed_primaries(),
            Duration::from_secs(300),
        );
        let err = store
            .validate(&cap.token, "T-2", PathBuf::from("/p").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("thread_id"));
    }

    #[test]
    fn wrong_project_path_is_rejected() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-1",
            PathBuf::from("/project-a"),
            uds_allowed_primaries(),
            Duration::from_secs(300),
        );
        let err = store
            .validate(&cap.token, "T-1", PathBuf::from("/project-b").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("project_path"));
    }

    #[test]
    fn validate_primary_allows_permitted() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-1",
            PathBuf::from("/p"),
            uds_allowed_primaries(),
            Duration::from_secs(300),
        );
        store
            .validate_primary(&cap.token, "T-1", PathBuf::from("/p").as_path(), "execute")
            .unwrap();
        store
            .validate_primary(&cap.token, "T-1", PathBuf::from("/p").as_path(), "fetch")
            .unwrap();
        store
            .validate_primary(&cap.token, "T-1", PathBuf::from("/p").as_path(), "sign")
            .unwrap();
    }

    #[test]
    fn validate_primary_rejects_disallowed() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-1",
            PathBuf::from("/p"),
            http_allowed_primaries(),
            Duration::from_secs(300),
        );
        let err = store
            .validate_primary(&cap.token, "T-1", PathBuf::from("/p").as_path(), "fetch")
            .unwrap_err();
        assert!(err.to_string().contains("does not allow primary 'fetch'"));
    }

    #[test]
    fn prune_expired_removes_stale() {
        let store = CallbackCapabilityStore::new();
        store.generate(
            "T-1",
            PathBuf::from("/p"),
            uds_allowed_primaries(),
            Duration::from_secs(0),
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.generate(
            "T-2",
            PathBuf::from("/p"),
            uds_allowed_primaries(),
            Duration::from_secs(300),
        );
        let pruned = store.prune_expired();
        assert_eq!(pruned, 1);
    }

    #[test]
    fn compute_ttl_defaults_to_300() {
        assert_eq!(compute_ttl(None), Duration::from_secs(300));
    }

    #[test]
    fn compute_ttl_caps_at_3600() {
        assert_eq!(compute_ttl(Some(9999)), Duration::from_secs(3600));
    }

    #[test]
    fn compute_ttl_uses_provided_value() {
        assert_eq!(compute_ttl(Some(600)), Duration::from_secs(600));
    }
}
