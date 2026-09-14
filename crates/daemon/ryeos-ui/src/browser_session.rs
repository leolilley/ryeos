//! Browser session store for `/ui` routes.
//!
//! In-memory store with TTL eviction. Sessions are created by
//! `ui.launch.mint` and consumed by the configured `service:ui/launch` route, which
//! sets a session cookie. Session transport routes (current binding,
//! binding-coordinate dispatch, seats, and event streams) validate the cookie
//! against this store. Ordinary UI data routes remain RyeOS-signed and cannot
//! be reached with cookie authority alone.
//!
//! ## Lifecycle
//!
//! 1. `client:ryeos/web` launcher calls `ui.launch.mint` on the daemon.
//! 2. Daemon retains a pending session with the exact compiled surface/view
//!    binding behind a short-lived activation token.
//! 3. Browser hits the daemon-returned launch URL, activation is committed,
//!    session cookie is set, browser is redirected to `/ui`.
//! 4. Session-authed routes validate the cookie against this store.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default session TTL: 8 hours.
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(8 * 3600);

/// Default launch token TTL: 60 seconds.
const DEFAULT_LAUNCH_TOKEN_TTL: Duration = Duration::from_secs(60);

/// Context provided by a client launcher when minting a browser session.
#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub compiled_binding: Arc<crate::compiled_binding::SessionCompiledUiBinding>,
    pub effective_surface: serde_json::Value,
    /// Authenticated caller authority retained only for exact binding-entry
    /// revalidation. It is never returned to the browser as its grant.
    pub granted_caps: Vec<String>,
    pub user_principal_id: Option<String>,
    /// Exact live project directory selected while the signed caller's
    /// project authority was checked. The browser retains no filesystem
    /// coordinate capable of replacing this descriptor authority.
    pub project_authority: Option<Arc<lillux::PinnedDirectory>>,
}

/// Server-side browser session record exposed to handlers and verifiers.
#[derive(Debug, Clone)]
pub struct BrowserSession {
    pub session_id: String,
    pub created_at: Instant,
    pub expires_at: Instant,
    pub compiled_binding: Arc<crate::compiled_binding::SessionCompiledUiBinding>,
    pub effective_surface: serde_json::Value,
    pub granted_caps: Vec<String>,
    /// Cached projections of `compiled_binding`, retained for existing seat
    /// and project-resolution callers. They are immutable and are not
    /// independent authority.
    pub project_root: Option<String>,
    pub surface_ref: String,
    pub user_principal_id: Option<String>,
    pub project_authority: Option<Arc<lillux::PinnedDirectory>>,
}

/// Short-lived, idempotent activation token for a pending session.
#[derive(Debug)]
struct LaunchToken {
    session: BrowserSession,
    predecessor_session_id: Option<String>,
    activated: bool,
    #[allow(dead_code)]
    created_at: Instant,
    expires_at: Instant,
}

/// In-memory browser session store.
pub struct BrowserSessionStore {
    sessions: Mutex<HashMap<String, BrowserSession>>,
    launch_tokens: Mutex<HashMap<String, LaunchToken>>,
    session_ttl: Duration,
    launch_token_ttl: Duration,
}

/// Exact result of one launch-token activation. The predecessor coordinate is
/// retained on replay so daemon-owned dependent cleanup is itself retryable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchActivation {
    pub session_id: String,
    pub predecessor_session_id: Option<String>,
}

impl Default for BrowserSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserSessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            launch_tokens: Mutex::new(HashMap::new()),
            session_ttl: DEFAULT_SESSION_TTL,
            launch_token_ttl: DEFAULT_LAUNCH_TOKEN_TTL,
        }
    }

    /// Create a store with very short TTLs for testing.
    pub fn new_with_short_ttl(session_ttl: Duration, launch_token_ttl: Duration) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            launch_tokens: Mutex::new(HashMap::new()),
            session_ttl,
            launch_token_ttl,
        }
    }

    /// Mint a launch token bound to a new session with full context.
    /// Returns `(session_id, token_hex)`.
    pub fn mint_token(&self, ctx: LaunchContext) -> (String, String) {
        self.mint_token_inner(ctx, None)
    }

    /// Mint an immutable successor. The predecessor stays usable until exact
    /// activation; replaying an activated token returns the same successor.
    pub fn mint_replacement_token(
        &self,
        predecessor_session_id: &str,
        ctx: LaunchContext,
    ) -> (String, String) {
        self.mint_token_inner(ctx, Some(predecessor_session_id.to_string()))
    }

    fn mint_token_inner(
        &self,
        ctx: LaunchContext,
        predecessor_session_id: Option<String>,
    ) -> (String, String) {
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        let surface_ref = ctx.compiled_binding.binding.surface.canonical_ref.clone();
        let project_root = ctx.compiled_binding.binding.project_root.clone();
        let session = BrowserSession {
            session_id: session_id.clone(),
            created_at: now,
            expires_at: now + self.session_ttl,
            compiled_binding: ctx.compiled_binding,
            effective_surface: ctx.effective_surface,
            granted_caps: ctx.granted_caps,
            project_root,
            surface_ref,
            user_principal_id: ctx.user_principal_id,
            project_authority: ctx.project_authority,
        };

        let token_bytes: [u8; 32] = rand::random();
        let token_hex = lillux::cas::sha256_hex(&token_bytes);
        let launch_token = LaunchToken {
            session,
            predecessor_session_id,
            activated: false,
            created_at: now,
            expires_at: now + self.launch_token_ttl,
        };

        self.launch_tokens
            .lock()
            .unwrap()
            .insert(token_hex.clone(), launch_token);

        (session_id, token_hex)
    }

    /// Activate a pending session and return its ID. Successful activation is
    /// replayable until token expiry so a committed-but-lost HTTP response can
    /// be recovered without reviving the predecessor or minting a sibling.
    pub fn consume_launch_token(&self, token: &str) -> Option<String> {
        self.activate_launch_token(token)
            .map(|activation| activation.session_id)
    }

    pub fn activate_launch_token(&self, token: &str) -> Option<LaunchActivation> {
        let mut tokens = self.launch_tokens.lock().unwrap();
        let launch = tokens.get(token)?;
        if launch.expires_at < Instant::now() {
            tokens.remove(token);
            return None;
        }
        if launch.activated {
            return Some(LaunchActivation {
                session_id: launch.session.session_id.clone(),
                predecessor_session_id: launch.predecessor_session_id.clone(),
            });
        }

        let predecessor = launch.predecessor_session_id.clone();
        let successor = launch.session.clone();
        let successor_id = successor.session_id.clone();
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(predecessor_id) = predecessor.as_deref()
            && !sessions.contains_key(predecessor_id)
        {
            return None;
        }
        sessions.insert(successor_id.clone(), successor);
        if let Some(predecessor_id) = predecessor.as_deref() {
            sessions.remove(predecessor_id);
        }
        tokens
            .get_mut(token)
            .expect("activation token retained")
            .activated = true;
        if let Some(predecessor_id) = predecessor.as_deref() {
            tokens.retain(|candidate, pending| {
                candidate == token
                    || pending.predecessor_session_id.as_deref() != Some(predecessor_id)
            });
        }
        Some(LaunchActivation {
            session_id: successor_id,
            predecessor_session_id: predecessor,
        })
    }

    /// Look up a session by ID. Returns `None` if not found or expired.
    /// Snapshot of active session ids (hint fan-out).
    pub fn session_ids(&self) -> Vec<String> {
        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .keys()
            .cloned()
            .collect()
    }

    pub fn get_session(&self, session_id: &str) -> Option<BrowserSession> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(session_id)?;
        if session.expires_at < Instant::now() {
            None
        } else {
            Some(session.clone())
        }
    }

    /// Evict expired sessions and launch tokens. Called periodically.
    pub fn evict_expired(&self) {
        let now = Instant::now();
        self.sessions
            .lock()
            .unwrap()
            .retain(|_, s| s.expires_at > now);
        self.launch_tokens
            .lock()
            .unwrap()
            .retain(|_, t| t.expires_at > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_binding(
        surface_ref: &str,
        project_root: Option<&str>,
        posture: crate::compiled_binding::EffectiveUiPosture,
    ) -> Arc<crate::compiled_binding::SessionCompiledUiBinding> {
        use std::collections::BTreeMap;

        use ryeos_api::surface_views::EffectiveUiItemIdentity;
        use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

        Arc::new(crate::compiled_binding::SessionCompiledUiBinding {
            binding_digest: "11".repeat(32),
            posture,
            binding: crate::compiled_binding::CompiledUiBinding {
                contract_revision: crate::UI_BINDING_CONTRACT_REVISION.to_string(),
                principal_id: "fp:test".to_string(),
                project_root: project_root.map(str::to_string),
                request_engine_generation_identity: "generation:test".to_string(),
                node_policy_generation_digest: "22".repeat(32),
                surface: EffectiveUiItemIdentity {
                    canonical_ref: surface_ref.to_string(),
                    effective_definition_digest: EffectiveDefinitionDigest::parse("33".repeat(32))
                        .expect("valid fixture digest"),
                    effective_trust_class: TrustClass::TrustedBundle,
                },
                views: BTreeMap::new(),
                sources: BTreeMap::new(),
                affordances: BTreeMap::new(),
                surface_route: None,
                attenuated: Vec::new(),
            },
        })
    }

    fn test_context() -> LaunchContext {
        LaunchContext {
            compiled_binding: test_binding(
                "surface:ryeos/ui/base",
                Some("/tmp/project"),
                crate::compiled_binding::EffectiveUiPosture::Interactive,
            ),
            effective_surface: serde_json::json!({"kind": "Surface"}),
            granted_caps: vec!["ui.read".into()],
            user_principal_id: None,
            project_authority: None,
        }
    }

    #[test]
    fn mint_creates_session_with_full_context() {
        let store = BrowserSessionStore::new();
        let ctx = test_context();
        let (session_id, token) = store.mint_token(ctx.clone());

        // Token can be consumed.
        let redeemed = store.consume_launch_token(&token);
        assert!(redeemed.is_some());
        assert_eq!(redeemed.unwrap(), session_id);

        // Session is retrievable.
        let session = store.get_session(&session_id).unwrap();
        assert_eq!(session.granted_caps, vec!["ui.read"]);
        assert_eq!(session.project_root, Some("/tmp/project".into()));
        assert_eq!(session.surface_ref, "surface:ryeos/ui/base");
        assert!(matches!(
            session.compiled_binding.posture,
            crate::compiled_binding::EffectiveUiPosture::Interactive
        ));
        assert_eq!(session.user_principal_id, None);
    }

    #[test]
    fn session_record_derives_posture_from_compiled_binding() {
        let store = BrowserSessionStore::new();
        let ctx = LaunchContext {
            compiled_binding: test_binding(
                "surface:ryeos/test/ro",
                None,
                crate::compiled_binding::EffectiveUiPosture::ObservationOnly,
            ),
            effective_surface: serde_json::json!({"kind": "Surface"}),
            granted_caps: vec![],
            user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
            project_authority: None,
        };
        let (session_id, token) = store.mint_token(ctx);
        assert!(store.get_session(&session_id).is_none());
        assert_eq!(store.consume_launch_token(&token), Some(session_id.clone()));

        let session = store.get_session(&session_id).unwrap();
        assert_eq!(session.surface_ref, "surface:ryeos/test/ro");
        assert!(matches!(
            session.compiled_binding.posture,
            crate::compiled_binding::EffectiveUiPosture::ObservationOnly
        ));
        assert!(session.project_root.is_none());
        assert_eq!(
            session.user_principal_id,
            Some(format!("fp:{}", "ab".repeat(32)))
        );
    }

    #[test]
    fn activated_launch_token_replays_same_session() {
        let store = BrowserSessionStore::new();
        let (_, token) = store.mint_token(test_context());

        let first = store.consume_launch_token(&token);
        assert!(first.is_some());

        let second = store.consume_launch_token(&token);
        assert_eq!(
            second, first,
            "activation replay must recover one successor"
        );
    }

    #[test]
    fn replacement_redemption_retires_predecessor_only_after_delivery() {
        let store = BrowserSessionStore::new();
        let (predecessor, predecessor_token) = store.mint_token(test_context());
        assert_eq!(
            store.consume_launch_token(&predecessor_token),
            Some(predecessor.clone())
        );
        let (successor, token) = store.mint_replacement_token(&predecessor, test_context());

        assert!(store.get_session(&predecessor).is_some());
        assert!(store.get_session(&successor).is_none());
        assert_eq!(store.consume_launch_token(&token), Some(successor.clone()));
        assert!(store.get_session(&predecessor).is_none());
        assert!(store.get_session(&successor).is_some());
        assert_eq!(store.consume_launch_token(&token), Some(successor));
    }

    #[test]
    fn replacement_activation_invalidates_pending_siblings() {
        let store = BrowserSessionStore::new();
        let (predecessor, predecessor_token) = store.mint_token(test_context());
        assert_eq!(
            store.consume_launch_token(&predecessor_token),
            Some(predecessor.clone())
        );
        let (first, first_token) = store.mint_replacement_token(&predecessor, test_context());
        let (second, second_token) = store.mint_replacement_token(&predecessor, test_context());

        assert_eq!(
            store.consume_launch_token(&first_token),
            Some(first.clone())
        );
        assert!(store.consume_launch_token(&second_token).is_none());
        assert!(store.get_session(&second).is_none());
        assert!(store.get_session(&first).is_some());
    }

    #[test]
    fn expired_token_rejected() {
        let store = BrowserSessionStore {
            sessions: Mutex::new(HashMap::new()),
            launch_tokens: Mutex::new(HashMap::new()),
            session_ttl: Duration::from_millis(1),
            launch_token_ttl: Duration::from_millis(1),
        };

        let (_, token) = store.mint_token(test_context());

        std::thread::sleep(Duration::from_millis(5));

        assert!(
            store.consume_launch_token(&token).is_none(),
            "expired token should be rejected"
        );
    }

    #[test]
    fn unknown_token_returns_none() {
        let store = BrowserSessionStore::new();
        assert!(store.consume_launch_token("nonexistent").is_none());
    }

    #[test]
    fn unknown_session_returns_none() {
        let store = BrowserSessionStore::new();
        assert!(store.get_session("nonexistent").is_none());
    }

    #[test]
    fn evict_removes_expired() {
        let store = BrowserSessionStore::new_with_short_ttl(
            Duration::from_millis(1),
            Duration::from_millis(1),
        );

        let (session_id, _token) = store.mint_token(test_context());

        std::thread::sleep(Duration::from_millis(5));

        store.evict_expired();

        assert!(
            store.get_session(&session_id).is_none(),
            "expired session should be evicted"
        );
    }
}
