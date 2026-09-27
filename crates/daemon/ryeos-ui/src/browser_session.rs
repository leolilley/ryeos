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
    /// Stable principal-registry identity. This is indexing identity, never a
    /// filesystem coordinate and never inferred from browser input.
    pub registered_project_id: Option<String>,
}

/// Browser-visible coordinate for one exact admitted binding attachment.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BindingAttachmentCoordinate {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
}

/// Immutable server authority retained by one mounted UI composition.
#[derive(Debug, Clone)]
pub struct AdmittedBindingAttachment {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub registered_project_id: Option<String>,
    pub compiled_binding: Arc<crate::compiled_binding::SessionCompiledUiBinding>,
    pub effective_surface: serde_json::Value,
    pub surface_ref: String,
    /// Stable query/display projection only. Filesystem access always uses the
    /// retained descriptor authority below.
    pub project_query_identity: Option<String>,
    pub project_authority: Option<Arc<lillux::PinnedDirectory>>,
}

impl AdmittedBindingAttachment {
    pub fn coordinate(&self) -> BindingAttachmentCoordinate {
        BindingAttachmentCoordinate {
            binding_attachment_id: self.binding_attachment_id.clone(),
            binding_generation: self.binding_generation,
            binding_digest: self.compiled_binding.binding_digest.clone(),
        }
    }

    pub fn public_descriptor(
        &self,
        binding_request_bounds: ryeos_client_base::ui::UiBindingRequestBounds,
    ) -> ryeos_client_base::ui::UiBindingAttachment {
        ryeos_client_base::ui::UiBindingAttachment {
            binding_attachment_id: self.binding_attachment_id.clone(),
            binding_generation: self.binding_generation,
            binding_digest: self.compiled_binding.binding_digest.clone(),
            surface_ref: self.surface_ref.clone(),
            surface_generation: self
                .compiled_binding
                .binding
                .surface
                .effective_definition_digest
                .as_str()
                .to_owned(),
            effective_surface: self.effective_surface.clone(),
            project_path: self.project_query_identity.clone(),
            posture: match self.compiled_binding.posture {
                crate::compiled_binding::EffectiveUiPosture::ObservationOnly => {
                    ryeos_client_base::ui::UiEffectivePosture::ObservationOnly
                }
                crate::compiled_binding::EffectiveUiPosture::Interactive => {
                    ryeos_client_base::ui::UiEffectivePosture::Interactive
                }
            },
            binding_request_bounds,
        }
    }
}

/// Candidate compiled outside the session lock and published atomically.
#[derive(Debug, Clone)]
pub struct BindingAttachmentCandidate {
    pub registered_project_id: Option<String>,
    pub compiled_binding: Arc<crate::compiled_binding::SessionCompiledUiBinding>,
    pub effective_surface: serde_json::Value,
    pub project_authority: Option<Arc<lillux::PinnedDirectory>>,
}

/// Store-linearized permission for already prepared work to cross UI dispatch
/// admission. Revocation after this token is issued does not cancel that work.
#[derive(Debug, Clone)]
pub struct AdmittedAttachmentDispatch {
    attachment: Arc<AdmittedBindingAttachment>,
}

impl AdmittedAttachmentDispatch {
    pub fn attachment(&self) -> &Arc<AdmittedBindingAttachment> {
        &self.attachment
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentStoreError {
    SessionInvalid,
    AttachmentStale,
    SurfaceAttachmentImmutable,
    PrincipalMismatch,
    PolicyStale,
    Capacity,
}

impl std::fmt::Display for AttachmentStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SessionInvalid => "session expired or invalid",
            Self::AttachmentStale => "binding attachment is stale or revoked",
            Self::SurfaceAttachmentImmutable => "the original surface attachment is immutable",
            Self::PrincipalMismatch => "binding attachment principal does not match the session",
            Self::PolicyStale => "binding attachment node policy generation is stale",
            Self::Capacity => "binding attachment capacity reached",
        })
    }
}

impl std::error::Error for AttachmentStoreError {}

/// Server-side browser session record exposed to handlers and verifiers.
#[derive(Debug, Clone)]
pub struct BrowserSession {
    pub session_id: String,
    pub created_at: Instant,
    pub expires_at: Instant,
    pub principal_id: String,
    pub granted_caps: Vec<String>,
    pub user_principal_id: Option<String>,
    /// Original authored launch composition. It is immutable provenance, not
    /// a mutable current/default project authority.
    pub surface_attachment_id: String,
    pub attachments: HashMap<String, Arc<AdmittedBindingAttachment>>,
    pub(crate) next_attachment_generation: u64,
}

/// Short-lived, idempotent activation token for a pending session.
#[derive(Debug)]
struct LaunchToken {
    session: BrowserSession,
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

/// Exact result of one launch-token activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchActivation {
    pub session_id: String,
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
    pub fn mint_token(
        &self,
        ctx: LaunchContext,
        max_attachments: usize,
        current_policy_generation_digest: &str,
    ) -> Result<(String, String, Arc<AdmittedBindingAttachment>), AttachmentStoreError> {
        if max_attachments == 0 {
            return Err(AttachmentStoreError::Capacity);
        }
        self.mint_token_inner(ctx, current_policy_generation_digest)
    }

    fn mint_token_inner(
        &self,
        ctx: LaunchContext,
        current_policy_generation_digest: &str,
    ) -> Result<(String, String, Arc<AdmittedBindingAttachment>), AttachmentStoreError> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = Instant::now();
        let principal_id = ctx.compiled_binding.binding.principal_id.clone();
        let attachment_id = uuid::Uuid::new_v4().to_string();
        let attachment = build_attachment(
            attachment_id.clone(),
            1,
            BindingAttachmentCandidate {
                registered_project_id: ctx.registered_project_id,
                compiled_binding: ctx.compiled_binding,
                effective_surface: ctx.effective_surface,
                project_authority: ctx.project_authority,
            },
            &principal_id,
            current_policy_generation_digest,
        )?;
        let attachment = Arc::new(attachment);
        let session = BrowserSession {
            session_id: session_id.clone(),
            created_at: now,
            expires_at: now + self.session_ttl,
            principal_id,
            granted_caps: ctx.granted_caps,
            user_principal_id: ctx.user_principal_id,
            surface_attachment_id: attachment_id.clone(),
            attachments: HashMap::from([(attachment_id, attachment.clone())]),
            next_attachment_generation: 2,
        };

        let token_bytes: [u8; 32] = rand::random();
        let token_hex = lillux::cas::sha256_hex(&token_bytes);
        let launch_token = LaunchToken {
            session,
            activated: false,
            created_at: now,
            expires_at: now + self.launch_token_ttl,
        };

        self.launch_tokens
            .lock()
            .unwrap()
            .insert(token_hex.clone(), launch_token);

        Ok((session_id, token_hex, attachment))
    }

    /// Activate a pending session and return its ID. Successful activation is
    /// replayable until token expiry so a committed-but-lost HTTP response can
    /// be recovered without minting a sibling.
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
            });
        }

        let successor = launch.session.clone();
        let successor_id = successor.session_id.clone();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(successor_id.clone(), successor);
        tokens
            .get_mut(token)
            .expect("activation token retained")
            .activated = true;
        Some(LaunchActivation {
            session_id: successor_id,
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
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.get(session_id)?;
        if session.expires_at < Instant::now() {
            sessions.remove(session_id);
            None
        } else {
            Some(session.clone())
        }
    }

    /// Resolve exact attachment authority for preparation. This does not admit
    /// a later mutation; callers must cross `admit_attachment_dispatch` after
    /// all fallible/awaited preparation.
    pub fn resolve_attachment(
        &self,
        session_id: &str,
        coordinate: &BindingAttachmentCoordinate,
    ) -> Result<Arc<AdmittedBindingAttachment>, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        exact_attachment(session, coordinate)
    }

    /// Linearize irreversible UI dispatch against attachment revocation.
    pub fn admit_attachment_dispatch(
        &self,
        session_id: &str,
        coordinate: &BindingAttachmentCoordinate,
    ) -> Result<AdmittedAttachmentDispatch, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        Ok(AdmittedAttachmentDispatch {
            attachment: exact_attachment(session, coordinate)?,
        })
    }

    /// Fence an awaited read before its result is exposed or committed.
    pub fn recheck_attachment_after_read(
        &self,
        session_id: &str,
        coordinate: &BindingAttachmentCoordinate,
    ) -> Result<(), AttachmentStoreError> {
        self.resolve_attachment(session_id, coordinate).map(drop)
    }

    /// Publish a candidate compiled outside the store lock. Capacity, session,
    /// principal and current policy are rechecked in this one bounded section.
    pub fn publish_attachment(
        &self,
        session_id: &str,
        origin: &BindingAttachmentCoordinate,
        candidate: BindingAttachmentCandidate,
        max_attachments: usize,
        current_policy_generation_digest: &str,
    ) -> Result<Arc<AdmittedBindingAttachment>, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        exact_attachment(session, origin)?;
        if session.attachments.len() >= max_attachments {
            return Err(AttachmentStoreError::Capacity);
        }
        let attachment_id = uuid::Uuid::new_v4().to_string();
        let generation = session.next_attachment_generation;
        let attachment = Arc::new(build_attachment(
            attachment_id.clone(),
            generation,
            candidate,
            &session.principal_id,
            current_policy_generation_digest,
        )?);
        session.next_attachment_generation = generation
            .checked_add(1)
            .ok_or(AttachmentStoreError::Capacity)?;
        session
            .attachments
            .insert(attachment_id, attachment.clone());
        Ok(attachment)
    }

    pub fn revoke_attachment(
        &self,
        session_id: &str,
        coordinate: &BindingAttachmentCoordinate,
    ) -> Result<Arc<AdmittedBindingAttachment>, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        exact_attachment(session, coordinate)?;
        session
            .attachments
            .remove(&coordinate.binding_attachment_id)
            .ok_or(AttachmentStoreError::AttachmentStale)
    }

    /// Return whether an exact non-surface attachment is still owned by this
    /// session. An absent coordinate is an opaque idempotent completed detach.
    pub fn attachment_for_detach(
        &self,
        session_id: &str,
        coordinate: &BindingAttachmentCoordinate,
    ) -> Result<Option<Arc<AdmittedBindingAttachment>>, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, session| session.expires_at > now);
        let session = sessions
            .get(session_id)
            .ok_or(AttachmentStoreError::SessionInvalid)?;
        if coordinate.binding_attachment_id.trim().is_empty()
            || coordinate.binding_generation == 0
            || coordinate.binding_digest.trim().is_empty()
        {
            return Err(AttachmentStoreError::AttachmentStale);
        }
        if coordinate.binding_attachment_id == session.surface_attachment_id {
            return Err(AttachmentStoreError::SurfaceAttachmentImmutable);
        }
        if let Some(attachment) = session.attachments.get(&coordinate.binding_attachment_id) {
            return exact_attachment(session, coordinate).map(|_| Some(attachment.clone()));
        }
        Ok(None)
    }

    /// Bounded scan owned by the existing session map. Callers coordinating a
    /// registry deletion must still serialize publish-vs-forget around this
    /// predicate; a check followed by a separate registry write is racy.
    pub fn has_retained_attachment_for_project(
        &self,
        principal_id: &str,
        registered_project_id: &str,
    ) -> bool {
        let now = Instant::now();
        // Preserve activation's launch_tokens -> sessions lock order. Pending
        // launch authority counts as retained project use: forget must not win
        // now and let the token activate a forgotten registration later.
        let mut launch_tokens = self.launch_tokens.lock().unwrap();
        launch_tokens.retain(|_, launch| launch.expires_at > now);
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, session| session.expires_at > now);
        let retains_project = |session: &BrowserSession| {
            session.principal_id == principal_id
                && session.attachments.values().any(|attachment| {
                    attachment.registered_project_id.as_deref() == Some(registered_project_id)
                })
        };
        launch_tokens
            .values()
            .any(|launch| !launch.activated && retains_project(&launch.session))
            || sessions.values().any(retains_project)
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

fn live_session<'a>(
    sessions: &'a mut HashMap<String, BrowserSession>,
    session_id: &str,
) -> Result<&'a mut BrowserSession, AttachmentStoreError> {
    if sessions
        .get(session_id)
        .is_some_and(|session| session.expires_at <= Instant::now())
    {
        sessions.remove(session_id);
    }
    sessions
        .get_mut(session_id)
        .ok_or(AttachmentStoreError::SessionInvalid)
}

fn exact_attachment(
    session: &BrowserSession,
    coordinate: &BindingAttachmentCoordinate,
) -> Result<Arc<AdmittedBindingAttachment>, AttachmentStoreError> {
    let attachment = session
        .attachments
        .get(&coordinate.binding_attachment_id)
        .ok_or(AttachmentStoreError::AttachmentStale)?;
    if attachment.binding_generation != coordinate.binding_generation
        || attachment.compiled_binding.binding_digest != coordinate.binding_digest
    {
        return Err(AttachmentStoreError::AttachmentStale);
    }
    Ok(attachment.clone())
}

fn build_attachment(
    binding_attachment_id: String,
    binding_generation: u64,
    candidate: BindingAttachmentCandidate,
    session_principal_id: &str,
    current_policy_generation_digest: &str,
) -> Result<AdmittedBindingAttachment, AttachmentStoreError> {
    let binding = &candidate.compiled_binding.binding;
    if binding.principal_id != session_principal_id {
        return Err(AttachmentStoreError::PrincipalMismatch);
    }
    if binding.node_policy_generation_digest != current_policy_generation_digest {
        return Err(AttachmentStoreError::PolicyStale);
    }
    let project_query_identity = match (
        candidate.project_authority.as_ref(),
        binding.project_root.as_deref(),
    ) {
        (None, None) => None,
        (Some(authority), Some(project_root)) => {
            authority
                .ensure_path_binding()
                .map_err(|_| AttachmentStoreError::AttachmentStale)?;
            if authority.path() != std::path::Path::new(project_root) {
                return Err(AttachmentStoreError::AttachmentStale);
            }
            Some(project_root.to_owned())
        }
        _ => return Err(AttachmentStoreError::AttachmentStale),
    };
    Ok(AdmittedBindingAttachment {
        binding_attachment_id,
        binding_generation,
        registered_project_id: candidate.registered_project_id,
        surface_ref: binding.surface.canonical_ref.clone(),
        compiled_binding: candidate.compiled_binding,
        effective_surface: candidate.effective_surface,
        project_query_identity,
        project_authority: candidate.project_authority,
    })
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
                None,
                crate::compiled_binding::EffectiveUiPosture::Interactive,
            ),
            effective_surface: serde_json::json!({"kind": "Surface"}),
            granted_caps: vec!["ui.read".into()],
            user_principal_id: None,
            project_authority: None,
            registered_project_id: None,
        }
    }

    fn mint(store: &BrowserSessionStore, context: LaunchContext) -> (String, String) {
        let (session_id, token, _) = store
            .mint_token(context, 4, &"22".repeat(32))
            .expect("mint fixture");
        (session_id, token)
    }

    #[test]
    fn mint_creates_session_with_full_context() {
        let store = BrowserSessionStore::new();
        let ctx = test_context();
        let (session_id, token) = mint(&store, ctx.clone());

        // Token can be consumed.
        let redeemed = store.consume_launch_token(&token);
        assert!(redeemed.is_some());
        assert_eq!(redeemed.unwrap(), session_id);

        // Session is retrievable.
        let session = store.get_session(&session_id).unwrap();
        assert_eq!(session.granted_caps, vec!["ui.read"]);
        let attachment = session
            .attachments
            .get(&session.surface_attachment_id)
            .expect("launch attachment");
        assert_eq!(attachment.surface_ref, "surface:ryeos/ui/base");
        assert!(matches!(
            attachment.compiled_binding.posture,
            crate::compiled_binding::EffectiveUiPosture::Interactive
        ));
        assert_eq!(session.user_principal_id, None);
    }

    #[test]
    fn detach_lookup_refuses_surface_and_other_session_and_replays_absent() {
        let store = BrowserSessionStore::new();
        let (first_id, first_token) = mint(&store, test_context());
        store
            .consume_launch_token(&first_token)
            .expect("activate first");
        let first = store.get_session(&first_id).expect("first session");
        let surface = first
            .attachments
            .get(&first.surface_attachment_id)
            .expect("surface")
            .coordinate();
        assert!(matches!(
            store.attachment_for_detach(&first_id, &surface),
            Err(AttachmentStoreError::SurfaceAttachmentImmutable)
        ));

        let secondary = store
            .publish_attachment(
                &first_id,
                &surface,
                BindingAttachmentCandidate {
                    registered_project_id: None,
                    compiled_binding: test_binding(
                        "surface:ryeos/test/secondary",
                        None,
                        crate::compiled_binding::EffectiveUiPosture::Interactive,
                    ),
                    effective_surface: serde_json::json!({"kind":"Surface"}),
                    project_authority: None,
                },
                4,
                &"22".repeat(32),
            )
            .expect("publish secondary");
        let coordinate = secondary.coordinate();
        let (second_id, second_token) = mint(&store, test_context());
        store
            .consume_launch_token(&second_token)
            .expect("activate second");
        assert!(
            store
                .attachment_for_detach(&second_id, &coordinate)
                .expect("foreign coordinate is indistinguishable from absent")
                .is_none()
        );
        assert!(store.resolve_attachment(&first_id, &coordinate).is_ok());
        let invalid = BindingAttachmentCoordinate {
            binding_attachment_id: String::new(),
            binding_generation: 0,
            binding_digest: String::new(),
        };
        assert!(matches!(
            store.attachment_for_detach(&second_id, &invalid),
            Err(AttachmentStoreError::AttachmentStale)
        ));
        assert!(
            store
                .attachment_for_detach(&first_id, &coordinate)
                .expect("owned")
                .is_some()
        );
        store
            .revoke_attachment(&first_id, &coordinate)
            .expect("revoke");
        assert!(
            store
                .attachment_for_detach(&first_id, &coordinate)
                .expect("idempotent replay")
                .is_none()
        );
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
            registered_project_id: None,
        };
        let (session_id, token) = mint(&store, ctx);
        assert!(store.get_session(&session_id).is_none());
        assert_eq!(store.consume_launch_token(&token), Some(session_id.clone()));

        let session = store.get_session(&session_id).unwrap();
        let attachment = session
            .attachments
            .get(&session.surface_attachment_id)
            .expect("launch attachment");
        assert_eq!(attachment.surface_ref, "surface:ryeos/test/ro");
        assert!(matches!(
            attachment.compiled_binding.posture,
            crate::compiled_binding::EffectiveUiPosture::ObservationOnly
        ));
        assert!(attachment.project_query_identity.is_none());
        assert_eq!(
            session.user_principal_id,
            Some(format!("fp:{}", "ab".repeat(32)))
        );
    }

    #[test]
    fn activated_launch_token_replays_same_session() {
        let store = BrowserSessionStore::new();
        let (_, token) = mint(&store, test_context());

        let first = store.consume_launch_token(&token);
        assert!(first.is_some());

        let second = store.consume_launch_token(&token);
        assert_eq!(
            second, first,
            "activation replay must recover one successor"
        );
    }

    #[test]
    fn expired_token_rejected() {
        let store = BrowserSessionStore {
            sessions: Mutex::new(HashMap::new()),
            launch_tokens: Mutex::new(HashMap::new()),
            session_ttl: Duration::from_millis(1),
            launch_token_ttl: Duration::from_millis(1),
        };

        let (_, token) = mint(&store, test_context());

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

        let (session_id, _token) = mint(&store, test_context());

        std::thread::sleep(Duration::from_millis(5));

        store.evict_expired();

        assert!(
            store.get_session(&session_id).is_none(),
            "expired session should be evicted"
        );
    }

    #[test]
    fn revoke_before_dispatch_admission_is_fenced_but_admitted_arc_survives() {
        let store = BrowserSessionStore::new();
        let (session_id, token) = mint(&store, test_context());
        store.consume_launch_token(&token).expect("activate");
        let session = store.get_session(&session_id).expect("session");
        let attachment = session
            .attachments
            .get(&session.surface_attachment_id)
            .expect("launch attachment");
        let coordinate = attachment.coordinate();
        let admitted = store
            .admit_attachment_dispatch(&session_id, &coordinate)
            .expect("admit dispatch");

        store
            .revoke_attachment(&session_id, &coordinate)
            .expect("revoke");
        assert!(matches!(
            store.admit_attachment_dispatch(&session_id, &coordinate),
            Err(AttachmentStoreError::AttachmentStale)
        ));
        assert_eq!(
            admitted.attachment().binding_attachment_id,
            coordinate.binding_attachment_id
        );
    }

    #[test]
    fn publish_rechecks_capacity_principal_and_policy() {
        let store = BrowserSessionStore::new();
        let (session_id, token) = mint(&store, test_context());
        store.consume_launch_token(&token).expect("activate");
        let candidate = BindingAttachmentCandidate {
            registered_project_id: Some("project-a".into()),
            compiled_binding: test_binding(
                "surface:ryeos/ui/base",
                None,
                crate::compiled_binding::EffectiveUiPosture::Interactive,
            ),
            effective_surface: serde_json::json!({"kind": "Surface"}),
            project_authority: None,
        };
        assert!(matches!(
            store.publish_attachment(
                &session_id,
                &store
                    .get_session(&session_id)
                    .expect("session")
                    .attachments
                    .values()
                    .next()
                    .expect("origin")
                    .coordinate(),
                candidate.clone(),
                1,
                &"22".repeat(32)
            ),
            Err(AttachmentStoreError::Capacity)
        ));
        assert!(matches!(
            store.publish_attachment(
                &session_id,
                &store
                    .get_session(&session_id)
                    .expect("session")
                    .attachments
                    .values()
                    .next()
                    .expect("origin")
                    .coordinate(),
                candidate.clone(),
                4,
                &"44".repeat(32)
            ),
            Err(AttachmentStoreError::PolicyStale)
        ));
        let origin = store
            .get_session(&session_id)
            .expect("session")
            .attachments
            .values()
            .next()
            .expect("origin")
            .coordinate();
        let published = store
            .publish_attachment(&session_id, &origin, candidate, 4, &"22".repeat(32))
            .expect("publish");
        assert_eq!(published.binding_generation, 2);
        assert!(store.has_retained_attachment_for_project("fp:test", "project-a"));
    }

    #[test]
    fn pending_launch_attachment_blocks_project_forget_scan() {
        let store = BrowserSessionStore::new();
        let mut context = test_context();
        context.registered_project_id = Some("project-pending".into());
        let (session_id, token) = mint(&store, context);

        assert!(store.has_retained_attachment_for_project("fp:test", "project-pending"));
        store.consume_launch_token(&token).expect("activate");
        let session = store.get_session(&session_id).expect("active session");
        let coordinate = session
            .attachments
            .get(&session.surface_attachment_id)
            .expect("surface attachment")
            .coordinate();
        store
            .revoke_attachment(&session_id, &coordinate)
            .expect("revoke active attachment");
        assert!(
            !store.has_retained_attachment_for_project("fp:test", "project-pending"),
            "activated replay token must not retain independently revocable project authority"
        );
    }
}
