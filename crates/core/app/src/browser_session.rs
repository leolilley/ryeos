//! Browser session store for `/ui` routes.
//!
//! In-memory store with TTL eviction. Sessions are created by the launch
//! token flow (`GET /ui/launch?token=...`) and consumed by session-authed
//! routes (`/ui/api/*`, `/ui/events/*`).
//!
//! ## Lifecycle
//!
//! 1. `client:ryeos/web` launcher mints a launch token via the daemon.
//! 2. Browser hits `GET /ui/launch?token=...`, token is consumed, session
//!    cookie is set.
//! 3. Session-authed routes validate the cookie against this store.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default session TTL: 8 hours.
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(8 * 3600);

/// Default launch token TTL: 60 seconds.
const DEFAULT_LAUNCH_TOKEN_TTL: Duration = Duration::from_secs(60);

/// Server-side session record.
#[derive(Debug, Clone)]
pub struct BrowserSession {
    pub session_id: String,
    pub created_at: Instant,
    pub expires_at: Instant,
    /// Capabilities granted to this session (intersected from caller + surface).
    pub granted_caps: Vec<String>,
    /// Project root the session is bound to (from launch context).
    pub project_root: Option<String>,
}

/// Single-use launch token that redeems for a session.
#[derive(Debug)]
struct LaunchToken {
    session_id: String,
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

    /// Create a new session and return its ID.
    /// Also returns a launch token that can be used once to associate
    /// a browser request with this session.
    pub fn create_session(
        &self,
        granted_caps: Vec<String>,
        project_root: Option<String>,
    ) -> (String, String) {
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        let session = BrowserSession {
            session_id: session_id.clone(),
            created_at: now,
            expires_at: now + self.session_ttl,
            granted_caps,
            project_root,
        };

        // Mint a launch token for this session.
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

    /// Create a new session with a custom TTL and return its ID + launch token.
    pub fn create_session_with_ttl(
        &self,
        granted_caps: Vec<String>,
        project_root: Option<String>,
        ttl: Duration,
    ) -> (String, String) {
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        let session = BrowserSession {
            session_id: session_id.clone(),
            created_at: now,
            expires_at: now + ttl,
            granted_caps,
            project_root,
        };

        let token_bytes: [u8; 32] = rand::random();
        let token_hex = lillux::cas::sha256_hex(&token_bytes);
        let launch_token = LaunchToken {
            session_id: session_id.clone(),
            created_at: now,
            expires_at: now + Duration::min(ttl, self.launch_token_ttl),
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

    #[test]
    fn create_and_retrieve_session() {
        let store = BrowserSessionStore::new();
        let (session_id, token) =
            store.create_session(vec!["ui.read".into()], Some("/tmp/project".into()));

        // Token can be consumed.
        let redeemed = store.consume_launch_token(&token);
        assert!(redeemed.is_some());
        assert_eq!(redeemed.unwrap(), session_id);

        // Session is retrievable.
        let session = store.get_session(&session_id);
        assert!(session.is_some());
        let s = session.unwrap();
        assert_eq!(s.granted_caps, vec!["ui.read"]);
        assert_eq!(s.project_root, Some("/tmp/project".into()));
    }

    #[test]
    fn launch_token_consumed_once() {
        let store = BrowserSessionStore::new();
        let (_, token) = store.create_session(vec![], None);

        let first = store.consume_launch_token(&token);
        assert!(first.is_some());

        let second = store.consume_launch_token(&token);
        assert!(second.is_none(), "token should not be reusable");
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
        let store = BrowserSessionStore {
            sessions: Mutex::new(HashMap::new()),
            launch_tokens: Mutex::new(HashMap::new()),
            session_ttl: Duration::from_millis(1),
            launch_token_ttl: Duration::from_millis(1),
        };

        let (session_id, _token) = store.create_session(vec![], None);

        // Wait for expiry.
        std::thread::sleep(Duration::from_millis(5));

        store.evict_expired();

        assert!(
            store.get_session(&session_id).is_none(),
            "expired session should be evicted"
        );
    }
}
