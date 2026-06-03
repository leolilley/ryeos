//! Browser session store for `/ui` routes.
//!
//! In-memory store with TTL eviction. Sessions are created by
//! `ui.launch.mint` and consumed by the configured `service:ui/launch` route, which
//! sets a session cookie. Session-authed routes (`/ui/api/*`,
//! `/ui/events/*`) validate the cookie against this store.
//!
//! ## Lifecycle
//!
//! 1. `client:ryeos/web` launcher calls `ui.launch.mint` on the daemon.
//! 2. Daemon creates a session record with context (surface_ref,
//!    project_path, read_only) and a one-shot launch token.
//! 3. Browser hits the daemon-returned launch URL, token is consumed,
//!    session cookie is set, browser is redirected to `/ui`.
//! 4. Session-authed routes validate the cookie against this store.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default session TTL: 8 hours.
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(8 * 3600);

/// Default launch token TTL: 60 seconds.
const DEFAULT_LAUNCH_TOKEN_TTL: Duration = Duration::from_secs(60);

/// Context provided by a client launcher when minting a browser session.
#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub surface_ref: String,
    pub project_path: Option<String>,
    pub read_only: bool,
    pub granted_caps: Vec<String>,
    pub user_principal_id: Option<String>,
}

/// Server-side browser session record exposed to handlers and verifiers.
#[derive(Debug, Clone)]
pub struct BrowserSession {
    pub session_id: String,
    pub created_at: Instant,
    pub expires_at: Instant,
    pub granted_caps: Vec<String>,
    pub project_root: Option<String>,
    pub surface_ref: String,
    pub read_only: bool,
    pub user_principal_id: Option<String>,
}

/// Single-use launch token that redeems for a session.
#[derive(Debug)]
struct LaunchToken {
    session_id: String,
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
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        let session = BrowserSession {
            session_id: session_id.clone(),
            created_at: now,
            expires_at: now + self.session_ttl,
            granted_caps: ctx.granted_caps,
            project_root: ctx.project_path,
            surface_ref: ctx.surface_ref,
            read_only: ctx.read_only,
            user_principal_id: ctx.user_principal_id,
        };

        let token_bytes: [u8; 32] = rand::random();
        let token_hex = lillux::cas::sha256_hex(&token_bytes);
        let launch_token = LaunchToken {
            session_id: session_id.clone(),
            created_at: now,
            expires_at: now + self.launch_token_ttl,
        };

        self.sessions
            .lock()
            .unwrap()
            .insert(session_id.clone(), session);
        self.launch_tokens
            .lock()
            .unwrap()
            .insert(token_hex.clone(), launch_token);

        (session_id, token_hex)
    }

    /// Consume a launch token and return the session ID.
    /// Returns `None` if the token doesn't exist, is expired, or already consumed.
    pub fn consume_launch_token(&self, token: &str) -> Option<String> {
        let mut tokens = self.launch_tokens.lock().unwrap();
        let launch = tokens.remove(token)?;
        if launch.expires_at < Instant::now() {
            None
        } else {
            Some(launch.session_id)
        }
    }

    /// Look up a session by ID. Returns `None` if not found or expired.
    pub fn get_session(&self, session_id: &str) -> Option<BrowserSession> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(session_id)?;
        if session.expires_at < Instant::now() {
            None
        } else {
            Some(session.clone())
        }
    }

    /// Update the project root bound to an active session.
    ///
    /// Returns the updated session. Expired, unknown, or read-only sessions return `None`.
    pub fn set_project_root(
        &self,
        session_id: &str,
        project_root: Option<String>,
    ) -> Option<BrowserSession> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get_mut(session_id)?;
        if session.expires_at < Instant::now() {
            return None;
        }
        if session.read_only {
            return None;
        }
        session.project_root = project_root;
        Some(session.clone())
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

    fn test_context() -> LaunchContext {
        LaunchContext {
            surface_ref: "surface:ryeos/cockpit/base".into(),
            project_path: Some("/tmp/project".into()),
            read_only: false,
            granted_caps: vec!["ui.read".into()],
            user_principal_id: None,
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
        assert_eq!(session.surface_ref, "surface:ryeos/cockpit/base");
        assert!(!session.read_only);
        assert_eq!(session.user_principal_id, None);
    }

    #[test]
    fn session_record_carries_surface_ref_and_read_only() {
        let store = BrowserSessionStore::new();
        let ctx = LaunchContext {
            surface_ref: "surface:ryeos/test/ro".into(),
            project_path: None,
            read_only: true,
            granted_caps: vec![],
            user_principal_id: Some(format!("fp:{}", "ab".repeat(32))),
        };
        let (session_id, _token) = store.mint_token(ctx);

        let session = store.get_session(&session_id).unwrap();
        assert_eq!(session.surface_ref, "surface:ryeos/test/ro");
        assert!(session.read_only);
        assert!(session.project_root.is_none());
        assert_eq!(
            session.user_principal_id,
            Some(format!("fp:{}", "ab".repeat(32)))
        );
    }

    #[test]
    fn set_project_root_updates_active_session() {
        let store = BrowserSessionStore::new();
        let (session_id, _token) = store.mint_token(test_context());

        let updated = store
            .set_project_root(&session_id, Some("/tmp/other-project".into()))
            .expect("active session should update");

        assert_eq!(updated.project_root, Some("/tmp/other-project".into()));
        assert_eq!(
            store.get_session(&session_id).unwrap().project_root,
            Some("/tmp/other-project".into())
        );
    }

    #[test]
    fn set_project_root_rejects_read_only_session() {
        let store = BrowserSessionStore::new();
        let (session_id, _token) = store.mint_token(LaunchContext {
            surface_ref: "surface:ryeos/test/ro".into(),
            project_path: None,
            read_only: true,
            granted_caps: vec![],
            user_principal_id: None,
        });

        let updated = store.set_project_root(&session_id, Some("/tmp/project".into()));

        assert!(updated.is_none());
        assert!(store
            .get_session(&session_id)
            .unwrap()
            .project_root
            .is_none());
    }

    #[test]
    fn launch_token_consumed_once() {
        let store = BrowserSessionStore::new();
        let (_, token) = store.mint_token(test_context());

        let first = store.consume_launch_token(&token);
        assert!(first.is_some());

        let second = store.consume_launch_token(&token);
        assert!(second.is_none(), "token should not be reusable");
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
