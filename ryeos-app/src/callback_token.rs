use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::execution_provenance::ExecutionProvenance;

/// Default TTL for callback tokens when no explicit duration is requested.
const DEFAULT_CALLBACK_TTL_SECS: u64 = 300;

/// Maximum allowed TTL — tokens requesting longer lifetimes are capped.
const MAX_CALLBACK_TTL_SECS: u64 = 3600;

#[derive(Debug, Clone)]
pub struct CallbackCapability {
    pub token: String,
    pub invocation_id: String,
    pub thread_id: String,
    pub project_path: PathBuf,
    pub expires_at: Instant,
    /// V5.5 P2: composed effective capabilities the parent thread
    /// holds. Carried on the callback token so the daemon-side
    /// dispatcher can enforce caps at the trust boundary instead of
    /// trusting the runtime to self-police. Empty = deny-all.
    pub effective_caps: Vec<String>,
    /// Required provenance from the parent dispatch. Callback children
    /// are derived from this value with `clone_for_borrowed_child()`;
    /// there is no deploy-window fallback or daemon-engine fallback.
    pub provenance: ExecutionProvenance,
}

pub struct CallbackCapabilityStore {
    capabilities: Mutex<HashMap<String, CallbackCapability>>,
}

impl Default for CallbackCapabilityStore {
    fn default() -> Self {
        Self::new()
    }
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
        ttl: Duration,
        effective_caps: Vec<String>,
        provenance: ExecutionProvenance,
    ) -> CallbackCapability {
        let random_bytes: [u8; 32] = rand::random();
        let hex = lillux::cas::sha256_hex(&random_bytes);
        let token = format!("cbt-{hex}");

        let inv_bytes: [u8; 16] = rand::random();
        let inv_hex = lillux::cas::sha256_hex(&inv_bytes);
        let invocation_id = format!("inv-{}", &inv_hex[..12]);

        let cap = CallbackCapability {
            token: token.clone(),
            invocation_id,
            thread_id: thread_id.to_string(),
            project_path,
            expires_at: Instant::now() + ttl,
            effective_caps,
            provenance,
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
    let secs = duration_seconds.unwrap_or(DEFAULT_CALLBACK_TTL_SECS);
    Duration::from_secs(secs.min(MAX_CALLBACK_TTL_SECS))
}

#[derive(Debug, Clone)]
pub struct ThreadAuthState {
    pub token: String,
    pub thread_id: String,
    pub acting_principal: String,
    pub caller_scopes: Vec<String>,
    pub expires_at: Instant,
}

pub struct ThreadAuthStore {
    states: Mutex<HashMap<String, ThreadAuthState>>,
}

impl Default for ThreadAuthStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadAuthStore {
    pub fn new() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
        }
    }

    pub fn mint(
        &self,
        thread_id: &str,
        acting_principal: String,
        caller_scopes: Vec<String>,
        ttl: Duration,
    ) -> ThreadAuthState {
        let random_bytes: [u8; 32] = rand::random();
        let hex = lillux::cas::sha256_hex(&random_bytes);
        let token = format!("tat-{hex}");

        let state = ThreadAuthState {
            token: token.clone(),
            thread_id: thread_id.to_string(),
            acting_principal,
            caller_scopes,
            expires_at: Instant::now() + ttl,
        };

        self.states.lock().unwrap().insert(token, state.clone());
        state
    }

    pub fn validate(&self, token: &str, thread_id: &str) -> Result<ThreadAuthState> {
        let map = self.states.lock().unwrap();
        let state = map
            .get(token)
            .ok_or_else(|| anyhow::anyhow!("invalid thread auth token"))?;

        if Instant::now() > state.expires_at {
            bail!("thread auth token expired");
        }

        if state.thread_id != thread_id {
            bail!("thread auth token does not match thread_id");
        }

        Ok(state.clone())
    }

    pub fn invalidate(&self, token: &str) {
        self.states.lock().unwrap().remove(token);
    }

    pub fn invalidate_for_thread(&self, thread_id: &str) {
        let mut map = self.states.lock().unwrap();
        map.retain(|_, s| s.thread_id != thread_id);
    }

    pub fn prune_expired(&self) -> usize {
        let mut map = self.states.lock().unwrap();
        let now = Instant::now();
        let before = map.len();
        map.retain(|_, s| s.expires_at > now);
        before - map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::execution_provenance::{ExecutionProvenance, ProjectSourceKind};
    use crate::temp_dir_guard::TempDirGuard;
    use ryeos_engine::engine::Engine;

    fn minimal_engine() -> Arc<Engine> {
        Arc::new(Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
            None,
            vec![],
        ))
    }

    fn provenance(path: PathBuf) -> ExecutionProvenance {
        ExecutionProvenance::root_live_fs(path, minimal_engine())
    }

    #[test]
    fn generate_and_validate_round_trip() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-test123",
            PathBuf::from("/project"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/project")),
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
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
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
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
        );
        let cap2 = store.generate(
            "T-2",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
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
            Duration::from_secs(0),
            Vec::new(),
            provenance(PathBuf::from("/p")),
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
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
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
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/project-a")),
        );
        let err = store
            .validate(&cap.token, "T-1", PathBuf::from("/project-b").as_path())
            .unwrap_err();
        assert!(err.to_string().contains("project_path"));
    }

    #[test]
    fn prune_expired_removes_stale() {
        let store = CallbackCapabilityStore::new();
        store.generate(
            "T-1",
            PathBuf::from("/p"),
            Duration::from_secs(0),
            Vec::new(),
            provenance(PathBuf::from("/p")),
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.generate(
            "T-2",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
        );
        let pruned = store.prune_expired();
        assert_eq!(pruned, 1);
    }

    #[test]
    fn provenance_lifeline_arc_identity_preserved_across_generate_validate() {
        let store = CallbackCapabilityStore::new();
        let engine = minimal_engine();
        let tmp = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(tmp.path().to_path_buf()));
        let provenance = ExecutionProvenance::root_pushed_head(
            tmp.path().to_path_buf(),
            PathBuf::from("/original"),
            engine.clone(),
            lifeline.clone(),
            "snap".to_string(),
        );

        let cap = store.generate(
            "T-test",
            tmp.path().to_path_buf(),
            Duration::from_secs(300),
            vec!["ryeos.*".to_string()],
            provenance,
        );
        let validated = store.validate(&cap.token, "T-test", tmp.path()).unwrap();

        assert!(Arc::ptr_eq(&validated.provenance.request_engine, &engine));
        assert_eq!(validated.provenance.original_project_path, PathBuf::from("/original"));
        assert_eq!(validated.provenance.project_source, ProjectSourceKind::PushedHead);
        assert_eq!(validated.provenance.effective_path, tmp.path());
        assert!(Arc::ptr_eq(
            validated.provenance.workspace_lifeline.as_ref().unwrap(),
            &lifeline
        ));
    }

    #[test]
    fn provenance_engine_arc_identity_preserved_across_clone() {
        let engine = minimal_engine();
        let cap = CallbackCapability {
            token: "cbt-test".to_string(),
            invocation_id: "inv-test".to_string(),
            thread_id: "T-test".to_string(),
            project_path: PathBuf::from("/project"),
            expires_at: Instant::now() + Duration::from_secs(300),
            effective_caps: vec![],
            provenance: ExecutionProvenance::root_live_fs(PathBuf::from("/project"), engine.clone()),
        };

        let cloned = cap.clone();
        assert!(Arc::ptr_eq(
            &cloned.provenance.request_engine,
            &engine
        ));
    }

    #[test]
    fn provenance_required_round_trips_through_validate() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-test",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
        );
        let validated = store
            .validate(&cap.token, "T-test", PathBuf::from("/p").as_path())
            .unwrap();
        assert_eq!(validated.provenance.effective_path, PathBuf::from("/p"));
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

    // ── ThreadAuthStore ──────────────────────────────────────────────

    #[test]
    fn thread_auth_mint_and_validate_round_trip() {
        let store = ThreadAuthStore::new();
        let state = store.mint(
            "T-abc",
            "fp:user123".to_string(),
            vec!["execute".to_string()],
            Duration::from_secs(300),
        );
        assert!(state.token.starts_with("tat-"));
        assert_eq!(state.acting_principal, "fp:user123");

        let validated = store.validate(&state.token, "T-abc").unwrap();
        assert_eq!(validated.thread_id, "T-abc");
        assert_eq!(validated.acting_principal, "fp:user123");
        assert_eq!(validated.caller_scopes, vec!["execute"]);
    }

    #[test]
    fn thread_auth_rejects_unknown_token() {
        let store = ThreadAuthStore::new();
        let err = store.validate("tat-nonexistent", "T-x").unwrap_err();
        assert!(err.to_string().contains("invalid thread auth token"));
    }

    #[test]
    fn thread_auth_rejects_wrong_thread() {
        let store = ThreadAuthStore::new();
        let state = store.mint(
            "T-1",
            "fp:u".to_string(),
            vec![],
            Duration::from_secs(300),
        );
        let err = store.validate(&state.token, "T-2").unwrap_err();
        assert!(err.to_string().contains("thread_id"));
    }

    #[test]
    fn thread_auth_rejects_expired() {
        let store = ThreadAuthStore::new();
        let state = store.mint("T-1", "fp:u".to_string(), vec![], Duration::from_secs(0));
        std::thread::sleep(std::time::Duration::from_millis(10));
        let err = store.validate(&state.token, "T-1").unwrap_err();
        assert!(err.to_string().contains("expired"));
    }

    #[test]
    fn thread_auth_invalidate_removes_token() {
        let store = ThreadAuthStore::new();
        let state = store.mint("T-1", "fp:u".to_string(), vec![], Duration::from_secs(300));
        store.invalidate(&state.token);
        assert!(store.validate(&state.token, "T-1").is_err());
    }

    #[test]
    fn thread_auth_invalidate_for_thread() {
        let store = ThreadAuthStore::new();
        let s1 = store.mint("T-1", "fp:u".to_string(), vec![], Duration::from_secs(300));
        let s2 = store.mint("T-2", "fp:u".to_string(), vec![], Duration::from_secs(300));
        store.invalidate_for_thread("T-1");
        assert!(store.validate(&s1.token, "T-1").is_err());
        assert!(store.validate(&s2.token, "T-2").is_ok());
    }

    #[test]
    fn thread_auth_prune_expired() {
        let store = ThreadAuthStore::new();
        store.mint("T-1", "fp:u".to_string(), vec![], Duration::from_secs(0));
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.mint("T-2", "fp:u".to_string(), vec![], Duration::from_secs(300));
        let pruned = store.prune_expired();
        assert_eq!(pruned, 1);
    }
}
