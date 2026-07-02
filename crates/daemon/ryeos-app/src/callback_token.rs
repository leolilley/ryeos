use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde_json::Value;

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
    /// Chain root of the minting thread. Carried so the daemon can key
    /// cross-chain wiring from a callback without re-deriving it. It is NOT an
    /// authority source by itself — callers that act on it MUST confirm it
    /// against the authoritative thread row via
    /// [`CallbackCapability::assert_chain_root`].
    pub chain_root_id: String,
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
    /// Bundle identity derived by the launcher from the verified root item.
    /// Runtime bundle-event APIs use this instead of trusting caller-supplied
    /// bundle IDs.
    pub effective_bundle_id: Option<String>,
    /// Root item ref that minted this callback token, used for attribution.
    pub item_ref: Option<String>,
    /// Parent thread's resolved hard limits, serialized by the launcher. The
    /// daemon passes this through out-of-band on callback-dispatched child
    /// launches so runtimes cannot spoof parent budget inheritance.
    pub hard_limits: Value,
    /// Parent thread's current spawn-tree depth. Children launch at `depth + 1`.
    pub depth: u32,
}

impl CallbackCapability {
    /// Confirm this cap's carried `chain_root_id` against the authoritative
    /// chain root from state. The cap value is a convenience carrier, never
    /// trusted on its own — cross-chain wiring keys on the validated result of
    /// this check, not the raw token value.
    pub fn assert_chain_root(&self, authoritative_chain_root_id: &str) -> Result<()> {
        if self.chain_root_id != authoritative_chain_root_id {
            bail!(
                "callback capability chain_root_id mismatch: cap={}, state={}",
                self.chain_root_id,
                authoritative_chain_root_id
            );
        }
        Ok(())
    }
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
        self.generate_with_context(
            thread_id,
            project_path,
            ttl,
            effective_caps,
            provenance,
            None,
            None,
            Value::Null,
            0,
        )
    }

    pub fn generate_with_context(
        &self,
        thread_id: &str,
        project_path: PathBuf,
        ttl: Duration,
        effective_caps: Vec<String>,
        provenance: ExecutionProvenance,
        effective_bundle_id: Option<String>,
        item_ref: Option<String>,
        hard_limits: Value,
        depth: u32,
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
            // Defaults to root (chain_root == thread_id). The managed launch
            // path overrides this via `set_chain_root` with the thread's
            // authoritative chain root from state.
            chain_root_id: thread_id.to_string(),
            project_path,
            expires_at: Instant::now() + ttl,
            effective_caps,
            provenance,
            effective_bundle_id,
            item_ref,
            hard_limits,
            depth,
        };

        self.capabilities.lock().unwrap().insert(token, cap.clone());
        cap
    }

    /// Override the carried chain root for a freshly-minted cap. Returns whether
    /// the token was found. Root mints default `chain_root == thread_id`; the
    /// managed launch path sets the thread's authoritative chain root (from
    /// state) here so the cap reflects real chain lineage.
    pub fn set_chain_root(&self, token: &str, chain_root_id: &str) -> bool {
        match self.capabilities.lock().unwrap().get_mut(token) {
            Some(cap) => {
                cap.chain_root_id = chain_root_id.to_string();
                true
            }
            None => false,
        }
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

    /// Validate a callback token without binding it to a thread. Returns the
    /// capability if the token exists and has not expired. The caller is
    /// responsible for any access-scope check (e.g. chain membership for a
    /// read). Never use this for a write or lifecycle method — those require an
    /// exact-thread match via [`Self::validate_token_and_thread`].
    pub fn validate_token_only(&self, token: &str) -> Result<CallbackCapability> {
        let map = self.capabilities.lock().unwrap();
        let cap = map
            .get(token)
            .ok_or_else(|| anyhow::anyhow!("invalid callback capability"))?;

        if Instant::now() > cap.expires_at {
            bail!("callback capability expired");
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

/// Margin added to a run's hard timeout so the run-scoped token outlives the
/// finalization callback that fires at/just after the deadline.
const LAUNCH_TTL_MARGIN_SECS: u64 = 300;

/// Absolute backstop for a run-scoped token — far above any realistic run — so a
/// zombie run cannot hold its credential indefinitely, without re-introducing
/// the sub-run cap that caused tokens to expire mid-run.
const MAX_LAUNCH_TTL_SECS: u64 = 7 * 24 * 3600;

/// TTL for a **run-scoped** launch token (the callback + thread-auth tokens a
/// launched runtime holds for its whole life).
///
/// Unlike [`compute_ttl`], it is deliberately NOT capped at
/// [`MAX_CALLBACK_TTL_SECS`] (3600s): the token must outlive the run's hard
/// timeout (`duration_seconds`) plus the finalization window, or a run allowed
/// to exceed 3600s loses callback/auth authority before it can finalize — a
/// silent mid-run failure. The token is thread-scoped and invalidated at run
/// end, so a TTL that tracks the run's duration is the correct lifetime; a
/// generous absolute backstop bounds the pathological zombie case.
///
/// A `duration_seconds` value of 0 is the launch hard-limit sentinel for
/// "unlimited". The token still needs an explicit authority lifetime, so it gets
/// the absolute launch-token backstop rather than the short default TTL.
///
/// CAVEAT: a run whose effective finite `duration_seconds` exceeds
/// [`MAX_LAUNCH_TTL_SECS`] (7 days), or an unlimited run that actually lives
/// that long, can outlive callback authority. Longer runs need renewal rather
/// than a silent larger constant here.
pub fn launch_token_ttl(duration_seconds: Option<u64>) -> Duration {
    let Some(secs) = duration_seconds else {
        return Duration::from_secs(DEFAULT_CALLBACK_TTL_SECS + LAUNCH_TTL_MARGIN_SECS);
    };
    if secs == 0 {
        return Duration::from_secs(MAX_LAUNCH_TTL_SECS);
    }
    Duration::from_secs(
        secs.saturating_add(LAUNCH_TTL_MARGIN_SECS)
            .min(MAX_LAUNCH_TTL_SECS),
    )
}

pub fn effective_bundle_id_from_item_ref(item_ref: &str) -> Option<String> {
    let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(item_ref).ok()?;
    canonical
        .bare_id
        .split('/')
        .next()
        .filter(|segment| !segment.is_empty())
        .map(str::to_string)
}

/// Single source of truth for an execution request's effective bundle id.
///
/// Derived from the **resolved** canonical ref (the post-resolution identity of
/// the item that will actually run), not the requested `item_ref` which may be
/// an alias or non-canonical form. The runtime-cap minter, the manifest
/// namespace check, and the callback token's `effective_bundle_id` MUST all use
/// this one value so the minted caps and the token that carries them claim the
/// same bundle identity.
pub fn effective_bundle_id_for_request(
    resolved: &crate::thread_lifecycle::ResolvedExecutionRequest,
) -> Option<String> {
    effective_bundle_id_from_item_ref(&resolved.resolved_item.canonical_ref.to_string())
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
    use std::path::Path;
    use std::sync::Arc;

    use crate::execution_provenance::{ExecutionProvenance, ProjectSourceKind};
    use crate::temp_dir_guard::TempDirGuard;
    use ryeos_engine::engine::Engine;

    type TestProvenance = ExecutionProvenance;

    fn minimal_engine() -> Arc<Engine> {
        Arc::new(Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
            vec![],
        ))
    }

    fn provenance(path: PathBuf) -> TestProvenance {
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
    fn chain_root_defaults_to_thread_id_then_set_chain_root_overrides() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate_with_context(
            "T-succ",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
            None,
            None,
            serde_json::Value::Null,
            0,
        );
        // Defaults to root (chain_root == thread_id).
        assert_eq!(cap.chain_root_id, "T-succ");
        // The managed launch path overrides with the authoritative chain root.
        store.set_chain_root(&cap.token, "T-root");
        let v = store
            .validate(&cap.token, "T-succ", PathBuf::from("/p").as_path())
            .unwrap();
        assert_eq!(v.chain_root_id, "T-root");
    }

    #[test]
    fn generate_uses_thread_id_as_chain_root() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-root",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
        );
        assert_eq!(cap.chain_root_id, "T-root");
    }

    #[test]
    fn assert_chain_root_rejects_mismatch() {
        let store = CallbackCapabilityStore::new();
        let cap = store.generate(
            "T-succ",
            PathBuf::from("/p"),
            Duration::from_secs(300),
            Vec::new(),
            provenance(PathBuf::from("/p")),
        );
        store.set_chain_root(&cap.token, "T-root");
        let cap = store
            .validate(&cap.token, "T-succ", PathBuf::from("/p").as_path())
            .unwrap();
        assert!(cap.assert_chain_root("T-root").is_ok());
        assert!(cap.assert_chain_root("T-other").is_err());
    }

    #[test]
    fn generate_with_context_round_trips_parent_limits_and_depth() {
        let store = CallbackCapabilityStore::new();
        let hard_limits = serde_json::json!({
            "turns": 6,
            "tokens": 1000,
            "spend_usd": 0.25,
            "spawns": 2,
            "depth": 3,
            "duration_seconds": 45,
        });
        let cap = store.generate_with_context(
            "T-parent",
            PathBuf::from("/project"),
            Duration::from_secs(300),
            vec!["ryeos.*".to_string()],
            provenance(PathBuf::from("/project")),
            Some("bundle-123".to_string()),
            Some("directive:team/parent".to_string()),
            hard_limits.clone(),
            4,
        );

        let validated = store
            .validate(&cap.token, "T-parent", PathBuf::from("/project").as_path())
            .unwrap();
        assert_eq!(validated.thread_id, "T-parent");
        assert_eq!(validated.hard_limits, hard_limits);
        assert_eq!(validated.depth, 4);
        assert_eq!(validated.effective_bundle_id.as_deref(), Some("bundle-123"));
        assert_eq!(validated.item_ref.as_deref(), Some("directive:team/parent"));
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

        assert!(Arc::ptr_eq(validated.provenance.request_engine(), &engine));
        assert_eq!(
            validated.provenance.original_project_path(),
            Path::new("/original")
        );
        assert_eq!(
            validated.provenance.project_source(),
            ProjectSourceKind::PushedHead
        );
        assert_eq!(validated.provenance.effective_path(), tmp.path());
        match &validated.provenance {
            ExecutionProvenance::RootPushedHead {
                workspace_lifeline, ..
            } => assert!(Arc::ptr_eq(workspace_lifeline, &lifeline)),
            other => panic!("expected RootPushedHead, got {other:?}"),
        }
    }

    #[test]
    fn provenance_engine_arc_identity_preserved_across_clone() {
        let engine = minimal_engine();
        let cap = CallbackCapability {
            token: "cbt-test".to_string(),
            invocation_id: "inv-test".to_string(),
            thread_id: "T-test".to_string(),
            chain_root_id: "T-test".to_string(),
            project_path: PathBuf::from("/project"),
            expires_at: Instant::now() + Duration::from_secs(300),
            effective_caps: vec![],
            provenance: ExecutionProvenance::root_live_fs(
                PathBuf::from("/project"),
                engine.clone(),
            ),
            effective_bundle_id: None,
            item_ref: None,
            hard_limits: serde_json::Value::Null,
            depth: 0,
        };

        let cloned = cap.clone();
        assert!(Arc::ptr_eq(cloned.provenance.request_engine(), &engine));
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
        assert_eq!(validated.provenance.effective_path(), Path::new("/p"));
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
    fn launch_token_ttl_outlives_a_run_past_3600() {
        // The bug: a run allowed to exceed 3600s must NOT lose its callback
        // token before it finalizes. The run-scoped TTL covers the run duration
        // plus a finalization margin, not the sub-run 3600 cap.
        let run = 7200u64;
        let ttl = launch_token_ttl(Some(run));
        assert!(
            ttl >= Duration::from_secs(run),
            "run-scoped token must outlive the run's hard timeout: {ttl:?}"
        );
        assert_eq!(ttl, Duration::from_secs(run + LAUNCH_TTL_MARGIN_SECS));
    }

    #[test]
    fn launch_token_ttl_has_absolute_backstop() {
        // A pathological duration is bounded, but the backstop is far above any
        // realistic run (so it never clips a real run the way 3600 did).
        assert_eq!(
            launch_token_ttl(Some(u64::MAX)),
            Duration::from_secs(MAX_LAUNCH_TTL_SECS)
        );
        assert!(MAX_LAUNCH_TTL_SECS > 3600);
    }

    #[test]
    fn launch_token_ttl_zero_duration_uses_backstop() {
        // Launch hard-limits use 0 as the unlimited sentinel. The run token must
        // not collapse unlimited runtime authority to only the finalization
        // margin.
        assert_eq!(
            launch_token_ttl(Some(0)),
            Duration::from_secs(MAX_LAUNCH_TTL_SECS)
        );
    }

    #[test]
    fn launch_token_ttl_defaults_when_unset() {
        assert_eq!(
            launch_token_ttl(None),
            Duration::from_secs(DEFAULT_CALLBACK_TTL_SECS + LAUNCH_TTL_MARGIN_SECS)
        );
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
        let state = store.mint("T-1", "fp:u".to_string(), vec![], Duration::from_secs(300));
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
