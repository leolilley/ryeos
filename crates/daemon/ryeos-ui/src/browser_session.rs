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

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub mod admission;
pub mod selected_initialization;
pub mod surface_opening;
pub mod transfer_reservation;
pub use transfer_reservation::HiddenTransferReservationOutcome;

/// Default session TTL: 8 hours.
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(8 * 3600);

/// Default launch token TTL: 60 seconds.
const DEFAULT_LAUNCH_TOKEN_TTL: Duration = Duration::from_secs(60);

/// Admission bounds for presentations, Surfaces and retained attachments.
/// Every value is taken from the node's signed UI browser-session policy; the
/// store has no bound of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentationLimits {
    pub max_presentations: usize,
    pub max_attachments: usize,
    pub max_surface_instances: usize,
    pub max_label_bytes: usize,
}

impl PresentationLimits {
    pub fn from_policy(
        policy: &ryeos_node_runtime::node_policy::sections::ui_browser_sessions::UiBrowserSessionPolicy,
    ) -> Result<Self, AttachmentStoreError> {
        let bound = |value: u64| usize::try_from(value).map_err(|_| AttachmentStoreError::Capacity);
        Ok(Self {
            max_presentations: bound(u64::from(policy.max_presentations_per_session))?,
            max_attachments: bound(u64::from(policy.max_live_binding_attachments_per_session))?,
            max_surface_instances: bound(u64::from(policy.max_surface_instances_per_presentation))?,
            max_label_bytes: bound(u64::from(policy.shared_presentation.max_label_bytes))?,
        })
    }
}

/// Context provided by a client launcher when minting a browser session.
#[derive(Debug, Clone)]
pub struct LaunchContext {
    pub compiled_binding: Arc<crate::compiled_binding::SessionCompiledUiBinding>,
    pub presentation: ryeos_client_base::presentation::UiCompositionPresentation,
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingAttachmentCoordinate {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub binding_digest: String,
}

enum SurfaceSubjectAdmission<'a> {
    ExistingRoot,
    RegisteredProject(&'a admission::RegisteredProjectSurfaceAdmission),
    Particular(&'a admission::ParticularSurfaceAdmission),
}

/// Immutable server authority retained by one mounted UI composition.
#[derive(Debug, Clone)]
pub struct AdmittedBindingAttachment {
    pub binding_attachment_id: String,
    pub binding_generation: u64,
    pub registered_project_id: Option<String>,
    pub compiled_binding: Arc<crate::compiled_binding::SessionCompiledUiBinding>,
    pub presentation: ryeos_client_base::presentation::UiCompositionPresentation,
    pub root_ref: String,
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
            root_ref: self.root_ref.clone(),
            root_generation: self
                .compiled_binding
                .binding
                .surface
                .effective_definition_digest
                .as_str()
                .to_owned(),
            presentation: self.presentation.clone(),
            project_path: self.project_query_identity.clone(),
            subject: match (&self.project_authority, &self.registered_project_id) {
                (None, None) => ryeos_client_base::ui::binding::UiBindingSubject::Projectless,
                (Some(_), None) => {
                    ryeos_client_base::ui::binding::UiBindingSubject::UnregisteredProject
                }
                (_, Some(local_id)) => {
                    ryeos_client_base::ui::binding::UiBindingSubject::RegisteredProject {
                        local_id: local_id.clone(),
                    }
                }
            },
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
    pub presentation: ryeos_client_base::presentation::UiCompositionPresentation,
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
    RootAttachmentImmutable,
    PrincipalMismatch,
    PolicyStale,
    Capacity,
    PresentationInvalid,
    PresentationConflict,
}

impl std::fmt::Display for AttachmentStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SessionInvalid => "session expired or invalid",
            Self::AttachmentStale => "binding attachment is stale or revoked",
            Self::RootAttachmentImmutable => "the original root attachment is immutable",
            Self::PrincipalMismatch => "binding attachment principal does not match the session",
            Self::PolicyStale => "binding attachment node policy generation is stale",
            Self::Capacity => "binding attachment capacity reached",
            Self::PresentationInvalid => "presentation membership is invalid",
            Self::PresentationConflict => {
                "presentation membership conflicts with retained ownership"
            }
        })
    }
}

impl std::error::Error for AttachmentStoreError {}

/// Session-retained association only; a seat is bound after durable admission.
#[derive(Debug, Clone, PartialEq)]
pub struct PresentationMembership {
    pub presentation_id: String,
    pub creation_request_id: String,
    pub root_coordinate: BindingAttachmentCoordinate,
    pub thread_id: Option<String>,
    pub chain_root_id: Option<String>,
    pub surfaces: HashMap<String, SurfaceMembership>,
    /// Activated transfer allocations are canonical presentation membership,
    /// not a parallel reservation registry. They remain hidden until the same
    /// atomic transition publishes their attachments.
    pub(crate) activated_transfer_reservations:
        HashMap<String, transfer_reservation::ActivatedTransferReservationMembership>,
    pub next_mount_id: u64,
    pub(crate) initialization: ryeos_client_base::ui::binding::UiPresentationInitialization,
    pub(crate) initialization_digest: String,
}

/// Exact Surface admission within one presentation. This is not another
/// registry or a layout copy: transient authenticated membership stays in the
/// existing session store; the core/seat owns the composition and its history.
#[derive(Debug, Clone, PartialEq)]
pub struct SurfaceMembership {
    pub surface_instance_id: String,
    pub creation_request_id: String,
    pub ordinal: u64,
    pub title: String,
    pub coordinate: BindingAttachmentCoordinate,
    pub mount_manifest: ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest,
    pub(crate) initial_seed: Option<ryeos_client_base::presentation::PresentationSurfaceSeed>,
    pub(crate) closed: bool,
    pub(crate) particular_resume: Option<ParticularSurfaceReceipt>,
    pub(crate) saved_resume: Option<SavedSurfaceReceipt>,
    /// Exact admission receipts retain imported descriptors before the seat's
    /// completion ACK. They never replace this Surface's origin or seed.
    pub(crate) tab_openings: BTreeMap<String, surface_opening::SurfaceTabsOpeningReceipt>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SavedSurfaceReceipt {
    pub provenance: ryeos_client_base::ui::binding::UiSurfaceCompositionProvenance,
    pub title: String,
    pub origin: ryeos_client_base::surface::saved_surfaces::SavedSurfaceOrigin,
    pub active_view_set_id: String,
    pub body: SavedSurfaceReceiptBody,
}

/// Stable pre-admission request identity has no allocated attachment fields.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SavedSurfaceRequestIdentity {
    pub provenance: ryeos_client_base::ui::binding::UiSurfaceCompositionProvenance,
    pub title: String,
    pub origin: ryeos_client_base::surface::saved_surfaces::SavedSurfaceOrigin,
    pub active_view_set_id: String,
    pub body: SavedSurfaceRequestBody,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedSurfaceRequestBody {
    Particular { tabs: Vec<ParticularSurfaceReceipt> },
    Reusable { tabs: Vec<ReusableSurfaceTabRecord> },
    Composed { tabs: Vec<ReusableSurfaceTabRecord> },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReusableSurfaceTabRecord {
    pub saved_id: String,
    pub origin: ryeos_client_base::surface::saved_surfaces::SavedSurfaceOrigin,
    pub template: ryeos_client_base::surface::view_sets::SavedViewSetTemplate,
    pub fresh_subjects: BTreeMap<String, BTreeMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedSurfaceReceiptBody {
    Particular {
        tabs: Vec<SavedSurfaceTabReceipt>,
    },
    Reusable {
        tabs: Vec<ReusableSurfaceTabReceipt>,
    },
    Composed {
        tabs: Vec<ReusableSurfaceTabReceipt>,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReusableSurfaceTabReceipt {
    pub record: ReusableSurfaceTabRecord,
    pub coordinate: BindingAttachmentCoordinate,
}

impl SavedSurfaceRequestIdentity {
    pub fn tab_count(&self) -> usize {
        match &self.body {
            SavedSurfaceRequestBody::Particular { tabs } => tabs.len(),
            SavedSurfaceRequestBody::Reusable { tabs }
            | SavedSurfaceRequestBody::Composed { tabs } => tabs.len(),
        }
    }
    pub fn tab_id(&self, index: usize) -> Option<&str> {
        match &self.body {
            SavedSurfaceRequestBody::Particular { tabs } => {
                tabs.get(index).map(|tab| tab.saved_id.as_str())
            }
            SavedSurfaceRequestBody::Reusable { tabs }
            | SavedSurfaceRequestBody::Composed { tabs } => {
                tabs.get(index).map(|tab| tab.saved_id.as_str())
            }
        }
    }
    pub fn admitted_body(
        &self,
        coordinates: &[BindingAttachmentCoordinate],
    ) -> Result<SavedSurfaceReceiptBody, AttachmentStoreError> {
        if self.tab_count() != coordinates.len() {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        Ok(match &self.body {
            SavedSurfaceRequestBody::Particular { tabs } => SavedSurfaceReceiptBody::Particular {
                tabs: tabs
                    .iter()
                    .zip(coordinates)
                    .map(|(record, coordinate)| SavedSurfaceTabReceipt {
                        record: record.clone(),
                        coordinate: coordinate.clone(),
                    })
                    .collect(),
            },
            SavedSurfaceRequestBody::Reusable { tabs } => SavedSurfaceReceiptBody::Reusable {
                tabs: tabs
                    .iter()
                    .zip(coordinates)
                    .map(|(record, coordinate)| ReusableSurfaceTabReceipt {
                        record: record.clone(),
                        coordinate: coordinate.clone(),
                    })
                    .collect(),
            },
            SavedSurfaceRequestBody::Composed { tabs } => SavedSurfaceReceiptBody::Composed {
                tabs: tabs
                    .iter()
                    .zip(coordinates)
                    .map(|(record, coordinate)| ReusableSurfaceTabReceipt {
                        record: record.clone(),
                        coordinate: coordinate.clone(),
                    })
                    .collect(),
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SavedSurfaceTabReceipt {
    pub record: ParticularSurfaceReceipt,
    pub coordinate: BindingAttachmentCoordinate,
}

impl SavedSurfaceReceipt {
    pub fn particular_selection(
        &self,
    ) -> Option<ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection> {
        let (saved_id, library_revision, _) = self
            .public_descriptor()
            .library_identity()
            .map(|(id, revision, digest)| (id.to_owned(), revision, digest.to_owned()))?;
        matches!(&self.body, SavedSurfaceReceiptBody::Particular { .. }).then(|| {
            ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection::Surface {
                id: saved_id,
                expected_revision: library_revision,
            }
        })
    }

    fn matches_request(&self, other: &SavedSurfaceRequestIdentity) -> bool {
        self.provenance == other.provenance
            && self.title == other.title
            && self.origin == other.origin
            && self.active_view_set_id == other.active_view_set_id
            && match (&self.body, &other.body) {
                (
                    SavedSurfaceReceiptBody::Particular { tabs: left },
                    SavedSurfaceRequestBody::Particular { tabs: right },
                ) => {
                    left.len() == right.len()
                        && left
                            .iter()
                            .zip(right)
                            .all(|(left, right)| left.record.same_request(right))
                }
                (
                    SavedSurfaceReceiptBody::Reusable { tabs: left },
                    SavedSurfaceRequestBody::Reusable { tabs: right },
                )
                | (
                    SavedSurfaceReceiptBody::Composed { tabs: left },
                    SavedSurfaceRequestBody::Composed { tabs: right },
                ) => {
                    left.len() == right.len()
                        && left
                            .iter()
                            .zip(right)
                            .all(|(left, right)| &left.record == right)
                }
                _ => false,
            }
    }
    pub fn coordinates(&self) -> Vec<&BindingAttachmentCoordinate> {
        match &self.body {
            SavedSurfaceReceiptBody::Particular { tabs } => {
                tabs.iter().map(|tab| &tab.coordinate).collect()
            }
            SavedSurfaceReceiptBody::Reusable { tabs }
            | SavedSurfaceReceiptBody::Composed { tabs } => {
                tabs.iter().map(|tab| &tab.coordinate).collect()
            }
        }
    }
    fn seeds(
        &self,
    ) -> Vec<(
        ryeos_client_base::surface::view_sets::ViewSetSeedSpec,
        ryeos_client_base::ui::binding::UiBindingAttachmentCoordinate,
    )> {
        self.public_descriptor().mount_seeds()
    }
    pub fn public_descriptor(&self) -> ryeos_client_base::ui::binding::UiSavedSurfaceComposition {
        use ryeos_client_base::ui::binding::{
            UiReusableSurfaceTab, UiSavedSurfaceComposition, UiSavedSurfaceCompositionBody,
            UiSavedSurfaceTab,
        };
        UiSavedSurfaceComposition {
            provenance: self.provenance.clone(),
            active_view_set_id: self.active_view_set_id.clone(),
            body: match &self.body {
                SavedSurfaceReceiptBody::Particular { tabs } => {
                    UiSavedSurfaceCompositionBody::Particular {
                        tabs: tabs
                            .iter()
                            .map(|tab| UiSavedSurfaceTab {
                                saved_set_id: tab.record.saved_id.clone(),
                                binding: public_coordinate(&tab.coordinate),
                                resolved: tab.record.resolved.clone(),
                            })
                            .collect(),
                    }
                }
                SavedSurfaceReceiptBody::Reusable { tabs } => {
                    UiSavedSurfaceCompositionBody::Reusable {
                        tabs: tabs
                            .iter()
                            .map(|tab| UiReusableSurfaceTab {
                                saved_set_id: tab.record.saved_id.clone(),
                                binding: public_coordinate(&tab.coordinate),
                                origin: tab.record.origin.clone(),
                                template: tab.record.template.clone(),
                                fresh_subjects: tab.record.fresh_subjects.clone(),
                            })
                            .collect(),
                    }
                }
                SavedSurfaceReceiptBody::Composed { tabs } => {
                    UiSavedSurfaceCompositionBody::Composed {
                        tabs: tabs
                            .iter()
                            .map(|tab| UiReusableSurfaceTab {
                                saved_set_id: tab.record.saved_id.clone(),
                                binding: public_coordinate(&tab.coordinate),
                                origin: tab.record.origin.clone(),
                                template: tab.record.template.clone(),
                                fresh_subjects: tab.record.fresh_subjects.clone(),
                            })
                            .collect(),
                    }
                }
            },
        }
    }
}

pub struct InitialSurfaceCandidate {
    pub seed: ryeos_client_base::presentation::PresentationSurfaceSeed,
    pub candidate: BindingAttachmentCandidate,
}

/// Bounded first admission response, owned by the existing Surface membership.
/// It retains no client layout runtime, drafts, grants or producer authority.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ParticularSurfaceReceipt {
    pub saved_id: String,
    pub library_revision: u64,
    pub record_digest: String,
    pub surface_ref: String,
    pub registered_project_id: Option<String>,
    pub resolved: ryeos_client_base::surface::view_sets::ResolvedParticularViewSet,
}

impl ParticularSurfaceReceipt {
    pub fn particular_selection(
        &self,
    ) -> ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection {
        ryeos_client_base::ui::admission_operation::UiParticularAdmissionSelection::ViewSet {
            id: self.saved_id.clone(),
            expected_revision: self.library_revision,
            surface_ref: self.surface_ref.clone(),
        }
    }

    fn same_request(&self, other: &Self) -> bool {
        self.saved_id == other.saved_id
            && self.library_revision == other.library_revision
            && self.record_digest == other.record_digest
            && self.surface_ref == other.surface_ref
            && self.registered_project_id == other.registered_project_id
    }
}

#[cfg(test)]
mod particular_receipt_tests {
    use super::ParticularSurfaceReceipt;

    #[test]
    fn retry_identity_fences_saved_record_subject_and_revision() {
        let receipt = ParticularSurfaceReceipt {
            saved_id: "saved".into(), library_revision: 3,
            record_digest: "record".into(), surface_ref: "surface:test/base".into(),
            registered_project_id: Some("project".into()),
            resolved: serde_json::from_value(serde_json::json!({
                "id":"saved", "name":"Saved", "composition":{"schema":2,"id":"saved","title":"Saved","root":null},
                "relationships":[], "resolved_selection_work":{"thread":"T-head","chain_root":"T-root"}
            })).expect("typed resolved receipt"),
        };
        let mut retry = receipt.clone();
        retry
            .resolved
            .resolved_selection_work
            .as_mut()
            .expect("work")
            .thread = "T-next".into();
        assert!(
            receipt.same_request(&retry),
            "retry returns original retained resolution"
        );
        retry.library_revision += 1;
        assert!(!receipt.same_request(&retry));
        retry = receipt.clone();
        retry.registered_project_id = None;
        assert!(!receipt.same_request(&retry));
        retry = receipt.clone();
        retry.record_digest = "changed".into();
        assert!(!receipt.same_request(&retry));
        retry = receipt.clone();
        retry.surface_ref = "surface:test/other".into();
        assert!(!receipt.same_request(&retry));
    }
}

impl SurfaceMembership {
    pub fn owns_coordinate(&self, coordinate: &BindingAttachmentCoordinate) -> bool {
        !self.closed
            && (self.coordinate == *coordinate
                || self.saved_resume.as_ref().is_some_and(|receipt| {
                    receipt.coordinates().iter().any(|tab| **tab == *coordinate)
                }))
            || self
                .tab_openings
                .values()
                .any(|opening| opening.current_coordinates.contains(coordinate))
    }
    pub fn public_descriptor(&self) -> ryeos_client_base::ui::binding::UiSurfaceMembership {
        ryeos_client_base::ui::binding::UiSurfaceMembership {
            surface_instance_id: self.surface_instance_id.clone(),
            ordinal: self.ordinal,
            title: self.title.clone(),
            binding: ryeos_client_base::ui::binding::UiBindingAttachmentCoordinate {
                binding_attachment_id: self.coordinate.binding_attachment_id.clone(),
                binding_generation: self.coordinate.binding_generation,
                binding_digest: self.coordinate.binding_digest.clone(),
            },
            saved_surface: self
                .saved_resume
                .as_ref()
                .map(SavedSurfaceReceipt::public_descriptor),
            mount_manifest: self.mount_manifest.clone(),
        }
    }
}

impl PresentationMembership {
    /// The initialization this presentation's seat must carry as its first
    /// signed history: the seat's replay is authentic only against exactly it.
    pub fn initialization(&self) -> &ryeos_client_base::ui::binding::UiPresentationInitialization {
        &self.initialization
    }

    pub fn public_descriptor(&self) -> ryeos_client_base::ui::binding::UiPresentationMembership {
        let mut surfaces = self
            .surfaces
            .values()
            .filter(|surface| !surface.closed)
            .map(SurfaceMembership::public_descriptor)
            .collect::<Vec<_>>();
        surfaces.sort_by_key(|surface| surface.ordinal);
        ryeos_client_base::ui::binding::UiPresentationMembership {
            presentation_id: self.presentation_id.clone(),
            root_binding: ryeos_client_base::ui::binding::UiBindingAttachmentCoordinate {
                binding_attachment_id: self.root_coordinate.binding_attachment_id.clone(),
                binding_generation: self.root_coordinate.binding_generation,
                binding_digest: self.root_coordinate.binding_digest.clone(),
            },
            thread_id: self.thread_id.clone(),
            chain_root_id: self.chain_root_id.clone(),
            surfaces,
            next_mount_id: self.next_mount_id,
            initialization_digest: self.initialization_digest.clone(),
        }
    }
}

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
    pub root_attachment_id: String,
    pub attachments: HashMap<String, Arc<AdmittedBindingAttachment>>,
    /// Accepted transfer attachments consume session capacity but remain
    /// invisible to ordinary resolution until their activation testimony is
    /// durable. The operation id is only an index; each entry retains and
    /// rechecks the complete transfer identity and attachment commitments.
    pub(crate) pending_transfer_attachments:
        HashMap<String, transfer_reservation::StagedTransferAttachmentBatch>,
    pub presentations: HashMap<String, PresentationMembership>,
    pub(crate) next_attachment_generation: u64,
    pub(crate) next_surface_ordinal: u64,
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
                presentation: ctx.presentation,
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
            root_attachment_id: attachment_id.clone(),
            attachments: HashMap::from([(attachment_id, attachment.clone())]),
            pending_transfer_attachments: HashMap::new(),
            presentations: HashMap::new(),
            next_surface_ordinal: 1,
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

    /// Hold the existing session owner across a bounded, read-only disclosure.
    /// The caller must already hold the UI seat-transition exclusion: existing
    /// preparation writers recheck this owner after acquiring state locks under
    /// that same exclusion. Authorization involving state must happen before
    /// this call. The action must not reenter this store or acquire UI locks.
    pub fn with_exact_presentation_read_session<T>(
        &self,
        session_id: &str,
        expected: &[PresentationMembership],
        current_policy_digest: &str,
        action: impl FnOnce() -> T,
    ) -> Result<T, AttachmentStoreError> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get(session_id)
            .filter(|session| session.expires_at > Instant::now())
            .ok_or(AttachmentStoreError::SessionInvalid)?;
        if expected.is_empty() {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        for membership in expected {
            if session.presentations.get(&membership.presentation_id) != Some(membership) {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            require_shared_presentation_root(
                session,
                &membership.root_coordinate,
                current_policy_digest,
            )?;
        }
        Ok(action())
    }

    /// Hold the exact complete session projection while the caller checks
    /// completed paired signed heads or publishes through an existing
    /// conditional writer. The borrowed session remains read-only; the callback
    /// must not reenter the session store or acquire UI locks.
    pub fn with_exact_presentation_read_snapshot<T>(
        &self,
        session_id: &str,
        expected: &[PresentationMembership],
        current_policy_digest: &str,
        action: impl FnOnce(&BrowserSession) -> T,
    ) -> Result<T, AttachmentStoreError> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions
            .get(session_id)
            .filter(|session| session.expires_at > Instant::now())
            .ok_or(AttachmentStoreError::SessionInvalid)?;
        Self::validate_exact_presentation_snapshot(session, expected, current_policy_digest)?;
        Ok(action(session))
    }

    /// Hold the same complete presentation snapshot through Accepted's
    /// conditional writer. The callback may reserve only its exact hidden
    /// transfer batch using the in-session owner; it must not lock this store.
    pub fn with_exact_transfer_reservation_snapshot<T>(
        &self,
        session_id: &str,
        expected: &[PresentationMembership],
        current_policy_digest: &str,
        action: impl FnOnce(&mut BrowserSession) -> T,
    ) -> Result<T, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        Self::validate_exact_presentation_snapshot(session, expected, current_policy_digest)?;
        Ok(action(session))
    }

    fn validate_exact_presentation_snapshot(
        session: &BrowserSession,
        expected: &[PresentationMembership],
        current_policy_digest: &str,
    ) -> Result<(), AttachmentStoreError> {
        // An authenticated session may not have opened a presentation yet.
        // Its launch root still needs the current-policy check; an empty
        // membership loop must not accidentally bypass it. Only a presentation
        // membership demands a shared-presentation root, checked below.
        let root = session
            .attachments
            .get(&session.root_attachment_id)
            .ok_or(AttachmentStoreError::AttachmentStale)?;
        if root.compiled_binding.binding.node_policy_generation_digest != current_policy_digest {
            return Err(AttachmentStoreError::PolicyStale);
        }
        if expected.len() != session.presentations.len()
            || expected
                .iter()
                .map(|membership| &membership.presentation_id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != expected.len()
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        for membership in expected {
            if session.presentations.get(&membership.presentation_id) != Some(membership) {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            require_shared_presentation_root(
                session,
                &membership.root_coordinate,
                current_policy_digest,
            )?;
        }
        Ok(())
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
        if transfer_reservation::occupied_attachment_count(session) >= max_attachments {
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

    pub fn prepare_presentation(
        &self,
        session_id: &str,
        request_id: &str,
        coordinate: &BindingAttachmentCoordinate,
        limits: &PresentationLimits,
        current_policy_digest: &str,
        request_bounds: ryeos_client_base::ui::UiBindingRequestBounds,
    ) -> Result<PresentationMembership, AttachmentStoreError> {
        self.prepare_presentation_with_initial_surfaces(
            session_id,
            request_id,
            coordinate,
            Vec::new(),
            limits,
            current_policy_digest,
            request_bounds,
            |_, _| Ok(()),
        )
    }

    /// Read-only retry lookup; callers can avoid resolving initial selections
    /// again. It checks the current root even when the request was retained.
    pub fn prepared_presentation(
        &self,
        session_id: &str,
        request_id: &str,
        coordinate: &BindingAttachmentCoordinate,
        current_policy_digest: &str,
    ) -> Result<Option<PresentationMembership>, AttachmentStoreError> {
        require_canonical_presentation_uuid(request_id)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        require_shared_presentation_root(session, coordinate, current_policy_digest)?;
        match session
            .presentations
            .values()
            .find(|entry| entry.creation_request_id == request_id)
        {
            Some(existing) if existing.root_coordinate != *coordinate => {
                Err(AttachmentStoreError::PresentationConflict)
            }
            existing => Ok(existing.cloned()),
        }
    }

    pub fn prepare_presentation_with_initial_surfaces(
        &self,
        session_id: &str,
        request_id: &str,
        coordinate: &BindingAttachmentCoordinate,
        candidates: Vec<InitialSurfaceCandidate>,
        limits: &PresentationLimits,
        current_policy_digest: &str,
        request_bounds: ryeos_client_base::ui::UiBindingRequestBounds,
        validate_publication: impl FnOnce(
            &PresentationMembership,
            &[Arc<AdmittedBindingAttachment>],
        ) -> Result<(), AttachmentStoreError>,
    ) -> Result<PresentationMembership, AttachmentStoreError> {
        require_canonical_presentation_uuid(request_id)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        let attachment =
            require_shared_presentation_root(session, coordinate, current_policy_digest)?;
        if let Some(existing) = session
            .presentations
            .values()
            .find(|entry| entry.creation_request_id == request_id)
        {
            return if existing.root_coordinate == *coordinate
                && matches!(existing.initialization.selection, ryeos_client_base::ui::presentation_initialization::UiPresentationInitialSelection::ConfiguredDefaults) {
                let descriptors = existing
                    .initialization
                    .binding_attachments
                    .iter()
                    .map(|descriptor| {
                        let retained = exact_attachment(
                            session,
                            &BindingAttachmentCoordinate {
                                binding_attachment_id: descriptor.binding_attachment_id.clone(),
                                binding_generation: descriptor.binding_generation,
                                binding_digest: descriptor.binding_digest.clone(),
                            },
                        )?;
                        validate_retained_project_path(&retained)?;
                        Ok(retained)
                    })
                    .collect::<Result<Vec<_>, AttachmentStoreError>>()?;
                validate_publication(existing, &descriptors)?;
                Ok(existing.clone())
            } else {
                Err(AttachmentStoreError::PresentationConflict)
            };
        }
        if session.presentations.len() >= limits.max_presentations {
            return Err(AttachmentStoreError::Capacity);
        }
        let ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation { config } =
            &attachment.presentation.definition
        else {
            return Err(AttachmentStoreError::PresentationInvalid);
        };
        let seeds = &config.shared_presentation.initial_surfaces;
        if candidates.len() != seeds.len() {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        if seeds.len() > limits.max_surface_instances
            || transfer_reservation::occupied_attachment_count(session)
                .checked_add(seeds.len())
                .is_none_or(|count| count > limits.max_attachments)
        {
            return Err(AttachmentStoreError::Capacity);
        }
        let presentation_id = uuid::Uuid::new_v4().to_string();
        let mut membership = PresentationMembership {
            presentation_id: presentation_id.clone(),
            creation_request_id: request_id.to_owned(),
            root_coordinate: coordinate.clone(),
            thread_id: None,
            chain_root_id: None,
            surfaces: HashMap::new(),
            activated_transfer_reservations: HashMap::new(),
            next_mount_id: 1,
            initialization: ryeos_client_base::ui::binding::UiPresentationInitialization {
                schema: ryeos_ui_presentation_contract::initialization::PRESENTATION_INITIALIZATION_SCHEMA,
                selection: ryeos_client_base::ui::presentation_initialization::UiPresentationInitialSelection::ConfiguredDefaults,
                presentation_id,
                creation_request_id: request_id.to_owned(),
                root_binding: public_coordinate(coordinate),
                next_mount_id: 1,
                surfaces: Vec::new(),
                binding_attachments: vec![attachment.public_descriptor(request_bounds)],
            },
            initialization_digest: String::new(),
        };
        let mut staged_attachments = Vec::with_capacity(seeds.len());
        let mut generation = session.next_attachment_generation;
        let mut ordinal = session.next_surface_ordinal;
        for (authored, initial) in seeds.iter().zip(candidates) {
            if initial.seed != *authored
                || authored.id.trim().is_empty()
                || authored.id.len() > limits.max_label_bytes
                || authored.id.chars().any(char::is_control)
                || authored.title.trim().is_empty()
                || authored.title.len() > limits.max_label_bytes
                || authored.title.chars().any(char::is_control)
                || membership.surfaces.values().any(|surface| {
                    surface
                        .initial_seed
                        .as_ref()
                        .is_some_and(|seed| seed.id == authored.id)
                })
            {
                return Err(AttachmentStoreError::PresentationInvalid);
            }
            let candidate = initial.candidate;
            if !candidate.compiled_binding.binding.root_is_kind("surface")
                || candidate.compiled_binding.binding.surface.canonical_ref != authored.surface
                || candidate
                    .compiled_binding
                    .binding
                    .request_engine_generation_identity
                    != attachment
                        .compiled_binding
                        .binding
                        .request_engine_generation_identity
                || candidate.compiled_binding.binding.project_root
                    != attachment.compiled_binding.binding.project_root
                || candidate.registered_project_id != attachment.registered_project_id
                || match (&candidate.project_authority, &attachment.project_authority) {
                    (None, None) => false,
                    (Some(candidate), Some(root)) => !Arc::ptr_eq(candidate, root),
                    _ => true,
                }
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            let surface_attachment = Arc::new(build_attachment(
                uuid::Uuid::new_v4().to_string(),
                generation,
                candidate,
                &session.principal_id,
                current_policy_digest,
            )?);
            let surface_instance_id = uuid::Uuid::new_v4().to_string();
            let seeds = surface_attachment
                .presentation
                .definition
                .surface()
                .ok_or(AttachmentStoreError::PresentationInvalid)?
                .to_mount_seeds()
                .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
            let active = seeds.first().map(|seed| seed.id.clone());
            let manifest = allocate_mount_manifest(
                &membership.presentation_id,
                &surface_instance_id,
                membership.next_mount_id,
                seeds
                    .into_iter()
                    .map(|seed| (seed, public_coordinate(&surface_attachment.coordinate())))
                    .collect(),
                active.as_deref(),
            )?;
            membership.next_mount_id = manifest.next_mount_id;
            let surface = SurfaceMembership {
                surface_instance_id,
                creation_request_id: uuid::Uuid::new_v4().to_string(),
                ordinal,
                title: authored.title.clone(),
                coordinate: surface_attachment.coordinate(),
                mount_manifest: manifest,
                initial_seed: Some(authored.clone()),
                closed: false,
                particular_resume: None,
                saved_resume: None,
                tab_openings: BTreeMap::new(),
            };
            generation = generation
                .checked_add(1)
                .ok_or(AttachmentStoreError::Capacity)?;
            ordinal = ordinal
                .checked_add(1)
                .ok_or(AttachmentStoreError::Capacity)?;
            membership
                .surfaces
                .insert(surface.surface_instance_id.clone(), surface);
            staged_attachments.push(surface_attachment);
        }
        let mut initial_surfaces = membership
            .surfaces
            .values()
            .map(SurfaceMembership::public_descriptor)
            .collect::<Vec<_>>();
        initial_surfaces.sort_by_key(|surface| surface.ordinal);
        membership.initialization.surfaces = initial_surfaces;
        membership.initialization.next_mount_id = membership.next_mount_id;
        membership.initialization.binding_attachments.extend(
            staged_attachments
                .iter()
                .map(|attachment| attachment.public_descriptor(request_bounds)),
        );
        membership
            .initialization
            .binding_attachments
            .sort_by(|left, right| left.binding_attachment_id.cmp(&right.binding_attachment_id));
        membership
            .initialization
            .validate()
            .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
        membership.initialization_digest = membership
            .initialization
            .digest()
            .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
        // All fallible validation and allocation is complete. Commit the one
        // session-owned batch, never publishing a partial seed collection.
        let mut initial_attachments = vec![attachment.clone()];
        initial_attachments.extend(staged_attachments.iter().cloned());
        initial_attachments
            .sort_by(|left, right| left.binding_attachment_id.cmp(&right.binding_attachment_id));
        validate_publication(&membership, &initial_attachments)?;
        for staged in staged_attachments {
            session
                .attachments
                .insert(staged.binding_attachment_id.clone(), staged);
        }
        session.next_attachment_generation = generation;
        session.next_surface_ordinal = ordinal;
        session
            .presentations
            .insert(membership.presentation_id.clone(), membership.clone());
        Ok(membership)
    }

    pub fn bind_presentation_seat(
        &self,
        session_id: &str,
        presentation_id: &str,
        coordinate: &BindingAttachmentCoordinate,
        thread_id: &str,
        chain_root_id: &str,
        current_policy_generation_digest: &str,
    ) -> Result<PresentationMembership, AttachmentStoreError> {
        require_canonical_presentation_uuid(presentation_id)?;
        if thread_id.is_empty() || chain_root_id.is_empty() {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        let root = exact_attachment(session, coordinate)?;
        if root.compiled_binding.binding.node_policy_generation_digest
            != current_policy_generation_digest
        {
            return Err(AttachmentStoreError::PolicyStale);
        }
        if session.presentations.values().any(|entry| {
            entry.presentation_id != presentation_id
                && (entry.thread_id.as_deref() == Some(thread_id)
                    || entry.chain_root_id.as_deref() == Some(chain_root_id))
        }) {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let membership = session
            .presentations
            .get_mut(presentation_id)
            .ok_or(AttachmentStoreError::PresentationInvalid)?;
        if membership.root_coordinate != *coordinate {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        match (&membership.thread_id, &membership.chain_root_id) {
            (None, None) => {
                membership.thread_id = Some(thread_id.to_owned());
                membership.chain_root_id = Some(chain_root_id.to_owned());
            }
            (Some(thread), Some(chain)) if thread == thread_id && chain == chain_root_id => {}
            _ => return Err(AttachmentStoreError::PresentationConflict),
        }
        Ok(membership.clone())
    }

    pub fn require_presentation(
        &self,
        session_id: &str,
        presentation_id: &str,
        thread_id: Option<&str>,
        chain_root_id: Option<&str>,
        current_policy_generation_digest: &str,
    ) -> Result<PresentationMembership, AttachmentStoreError> {
        require_canonical_presentation_uuid(presentation_id)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        let membership = session
            .presentations
            .get(presentation_id)
            .ok_or(AttachmentStoreError::PresentationInvalid)?;
        let root = exact_attachment(session, &membership.root_coordinate)?;
        if root.compiled_binding.binding.node_policy_generation_digest
            != current_policy_generation_digest
        {
            return Err(AttachmentStoreError::PolicyStale);
        }
        if thread_id.is_some_and(|thread| membership.thread_id.as_deref() != Some(thread))
            || chain_root_id.is_some_and(|chain| membership.chain_root_id.as_deref() != Some(chain))
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        Ok(membership.clone())
    }

    /// Short, single-lock recheck for the two-seat preparation writer. This
    /// runs only after StateStore and both ordered chain locks are held; it
    /// must not call back into state, CAS, engine or route admission. A false
    /// result aborts publication without consuming an attachment generation.
    pub fn matches_transfer_preparation_memberships(
        &self,
        session_id: &str,
        source: &PresentationMembership,
        destination: &PresentationMembership,
        expected_next_attachment_generation: u64,
        current_policy_digest: &str,
    ) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        let Ok(session) = live_session(&mut sessions, session_id) else {
            return false;
        };
        if source.presentation_id == destination.presentation_id
            || session.next_attachment_generation != expected_next_attachment_generation
        {
            return false;
        }
        let matches = |expected: &PresentationMembership| {
            session.presentations.get(&expected.presentation_id) == Some(expected)
                && session
                    .attachments
                    .get(&expected.root_coordinate.binding_attachment_id)
                    .is_some_and(|root| {
                        root.binding_generation == expected.root_coordinate.binding_generation
                            && root.compiled_binding.binding_digest
                                == expected.root_coordinate.binding_digest
                            && root.compiled_binding.binding.node_policy_generation_digest
                                == current_policy_digest
                    })
        };
        matches(source) && matches(destination)
    }

    /// Rebuildable forward-only cache. Only the seat owner calls this after
    /// validating authoritative ordered history; no endpoint accepts a frontier.
    /// A larger published-but-unsettled admission is never rolled backward.
    pub fn reconcile_presentation_mount_frontier(
        &self,
        session_id: &str,
        presentation_id: &str,
        coordinate: &BindingAttachmentCoordinate,
        current_policy_digest: &str,
        journal_frontier: u64,
    ) -> Result<(), AttachmentStoreError> {
        if journal_frontier == 0 {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        require_shared_presentation_root(session, coordinate, current_policy_digest)?;
        let membership = session
            .presentations
            .get_mut(presentation_id)
            .filter(|membership| membership.root_coordinate == *coordinate)
            .ok_or(AttachmentStoreError::PresentationConflict)?;
        membership.next_mount_id = membership.next_mount_id.max(journal_frontier);
        Ok(())
    }

    pub fn presentation_memberships(
        &self,
        session_id: &str,
    ) -> Result<Vec<PresentationMembership>, AttachmentStoreError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        let mut memberships = session.presentations.values().cloned().collect::<Vec<_>>();
        memberships.sort_by(|left, right| left.presentation_id.cmp(&right.presentation_id));
        Ok(memberships)
    }

    /// Atomically publish a compiled Surface and its presentation membership.
    /// The caller holds the seat-transition lock and proves its current writer
    /// before crossing this membership publication. Opening content never
    /// creates a session/cookie or rotates that presentation's producer.
    pub fn publish_saved_surface(
        &self,
        session_id: &str,
        presentation_id: &str,
        creation_request_id: &str,
        root_coordinate: &BindingAttachmentCoordinate,
        origin: BindingAttachmentCandidate,
        tabs: Vec<BindingAttachmentCandidate>,
        admission: &admission::SavedSurfaceAdmission,
        limits: &PresentationLimits,
        current_policy_digest: &str,
        validate_publication: impl FnOnce(
            &SurfaceMembership,
            &Arc<AdmittedBindingAttachment>,
            &[Arc<AdmittedBindingAttachment>],
        ) -> Result<(), AttachmentStoreError>,
    ) -> Result<
        (
            SurfaceMembership,
            Arc<AdmittedBindingAttachment>,
            Vec<Arc<AdmittedBindingAttachment>>,
        ),
        AttachmentStoreError,
    > {
        require_canonical_presentation_uuid(presentation_id)?;
        require_canonical_presentation_uuid(creation_request_id)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        let root = exact_presentation_attachment(
            session,
            presentation_id,
            root_coordinate,
            current_policy_digest,
        )?;
        let membership = session
            .presentations
            .get(presentation_id)
            .ok_or(AttachmentStoreError::PresentationInvalid)?;
        let receipt = admission.receipt();
        if membership.root_coordinate != *root_coordinate
            || tabs.len() != receipt.tab_count()
            || tabs.is_empty()
            || tabs.len() > ryeos_client_base::surface::view_sets::MAX_VIEW_SETS
            || !admission.matches_origin(&session.principal_id, &origin)
            || origin
                .compiled_binding
                .binding
                .request_engine_generation_identity
                != root
                    .compiled_binding
                    .binding
                    .request_engine_generation_identity
            || receipt.title.trim().is_empty()
            || receipt.title.len() > limits.max_label_bytes
            || receipt.title.chars().any(char::is_control)
        {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let mut generation = session.next_attachment_generation;
        let origin_attachment = Arc::new(build_attachment(
            uuid::Uuid::new_v4().to_string(),
            generation,
            origin,
            &session.principal_id,
            current_policy_digest,
        )?);
        generation = generation
            .checked_add(1)
            .ok_or(AttachmentStoreError::Capacity)?;
        let mut staged_tabs = Vec::with_capacity(tabs.len());
        let mut ids = std::collections::BTreeSet::new();
        for (index, candidate) in tabs.into_iter().enumerate() {
            if !admission.matches_tab(index, &session.principal_id, &candidate)
                || candidate
                    .compiled_binding
                    .binding
                    .request_engine_generation_identity
                    != root
                        .compiled_binding
                        .binding
                        .request_engine_generation_identity
                || !ids.insert(
                    receipt
                        .tab_id(index)
                        .ok_or(AttachmentStoreError::PresentationInvalid)?
                        .to_string(),
                )
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            let attachment = Arc::new(build_attachment(
                uuid::Uuid::new_v4().to_string(),
                generation,
                candidate,
                &session.principal_id,
                current_policy_digest,
            )?);
            if attachment.presentation.definition.surface().is_none() {
                return Err(AttachmentStoreError::PresentationInvalid);
            }
            staged_tabs.push(attachment);
            generation = generation
                .checked_add(1)
                .ok_or(AttachmentStoreError::Capacity)?;
        }
        let retained_receipt = SavedSurfaceReceipt {
            provenance: receipt.provenance.clone(),
            title: receipt.title.clone(),
            origin: receipt.origin.clone(),
            active_view_set_id: receipt.active_view_set_id.clone(),
            body: receipt.admitted_body(
                &staged_tabs
                    .iter()
                    .map(|tab| tab.coordinate())
                    .collect::<Vec<_>>(),
            )?,
        };
        retained_receipt
            .public_descriptor()
            .validate()
            .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
        if origin_attachment
            .presentation
            .definition
            .surface()
            .is_none()
            || !ids.contains(&receipt.active_view_set_id)
        {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        if let Some(existing) = membership
            .surfaces
            .values()
            .find(|surface| surface.creation_request_id == creation_request_id)
        {
            if existing.closed
                || !existing
                    .saved_resume
                    .as_ref()
                    .is_some_and(|retained| retained.matches_request(receipt))
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            let retained_origin = exact_attachment(session, &existing.coordinate)?;
            validate_retained_project_path(&retained_origin)?;
            let retained_tabs = existing
                .saved_resume
                .as_ref()
                .unwrap()
                .coordinates()
                .into_iter()
                .map(|coordinate| exact_attachment(session, coordinate))
                .collect::<Result<Vec<_>, _>>()?;
            for retained in &retained_tabs {
                validate_retained_project_path(retained)?;
            }
            let same = |left: &Arc<AdmittedBindingAttachment>,
                        right: &Arc<AdmittedBindingAttachment>| {
                left.compiled_binding.binding == right.compiled_binding.binding
                    && left.presentation == right.presentation
                    && left.registered_project_id == right.registered_project_id
            };
            if !same(&retained_origin, &origin_attachment)
                || !retained_tabs
                    .iter()
                    .zip(&staged_tabs)
                    .all(|(left, right)| same(left, right))
            {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            validate_publication(existing, &retained_origin, &retained_tabs)?;
            return Ok((existing.clone(), retained_origin, retained_tabs));
        }
        if membership.surfaces.len() >= limits.max_surface_instances
            || transfer_reservation::occupied_attachment_count(session)
                .checked_add(1 + staged_tabs.len())
                .is_none_or(|count| count > limits.max_attachments)
        {
            return Err(AttachmentStoreError::Capacity);
        }
        let ordinal = session.next_surface_ordinal;
        let next_ordinal = ordinal
            .checked_add(1)
            .ok_or(AttachmentStoreError::Capacity)?;
        let surface_instance_id = uuid::Uuid::new_v4().to_string();
        let manifest = allocate_mount_manifest(
            presentation_id,
            &surface_instance_id,
            membership.next_mount_id,
            retained_receipt.seeds(),
            Some(&retained_receipt.active_view_set_id),
        )?;
        let next_mount_id = manifest.next_mount_id;
        let surface = SurfaceMembership {
            surface_instance_id,
            creation_request_id: creation_request_id.into(),
            ordinal,
            title: receipt.title.clone(),
            coordinate: origin_attachment.coordinate(),
            mount_manifest: manifest,
            initial_seed: None,
            closed: false,
            particular_resume: None,
            saved_resume: Some(retained_receipt),
            tab_openings: BTreeMap::new(),
        };
        validate_publication(&surface, &origin_attachment, &staged_tabs)?;
        session.attachments.insert(
            origin_attachment.binding_attachment_id.clone(),
            origin_attachment.clone(),
        );
        for attachment in &staged_tabs {
            session
                .attachments
                .insert(attachment.binding_attachment_id.clone(), attachment.clone());
        }
        session
            .presentations
            .get_mut(presentation_id)
            .expect("checked presentation")
            .surfaces
            .insert(surface.surface_instance_id.clone(), surface.clone());
        session
            .presentations
            .get_mut(presentation_id)
            .expect("checked presentation")
            .next_mount_id = next_mount_id;
        session.next_attachment_generation = generation;
        session.next_surface_ordinal = next_ordinal;
        Ok((surface, origin_attachment, staged_tabs))
    }

    pub fn publish_surface(
        &self,
        session_id: &str,
        presentation_id: &str,
        creation_request_id: &str,
        root_coordinate: &BindingAttachmentCoordinate,
        candidate: BindingAttachmentCandidate,
        limits: &PresentationLimits,
        current_policy_generation_digest: &str,
        validate_publication: impl FnOnce(
            &SurfaceMembership,
            &Arc<AdmittedBindingAttachment>,
        ) -> Result<(), AttachmentStoreError>,
    ) -> Result<(SurfaceMembership, Arc<AdmittedBindingAttachment>), AttachmentStoreError> {
        self.publish_surface_internal(
            session_id,
            presentation_id,
            creation_request_id,
            root_coordinate,
            candidate,
            limits,
            current_policy_generation_digest,
            SurfaceSubjectAdmission::ExistingRoot,
            validate_publication,
        )
    }

    pub fn publish_registered_project_surface(
        &self,
        session_id: &str,
        presentation_id: &str,
        creation_request_id: &str,
        root_coordinate: &BindingAttachmentCoordinate,
        candidate: BindingAttachmentCandidate,
        limits: &PresentationLimits,
        current_policy_generation_digest: &str,
        admission: &admission::RegisteredProjectSurfaceAdmission,
        validate_publication: impl FnOnce(
            &SurfaceMembership,
            &Arc<AdmittedBindingAttachment>,
        ) -> Result<(), AttachmentStoreError>,
    ) -> Result<(SurfaceMembership, Arc<AdmittedBindingAttachment>), AttachmentStoreError> {
        self.publish_surface_internal(
            session_id,
            presentation_id,
            creation_request_id,
            root_coordinate,
            candidate,
            limits,
            current_policy_generation_digest,
            SurfaceSubjectAdmission::RegisteredProject(admission),
            validate_publication,
        )
    }

    pub fn publish_particular_surface(
        &self,
        session_id: &str,
        presentation_id: &str,
        creation_request_id: &str,
        root_coordinate: &BindingAttachmentCoordinate,
        candidate: BindingAttachmentCandidate,
        limits: &PresentationLimits,
        current_policy_generation_digest: &str,
        admission: &admission::ParticularSurfaceAdmission,
        validate_publication: impl FnOnce(
            &SurfaceMembership,
            &Arc<AdmittedBindingAttachment>,
        ) -> Result<(), AttachmentStoreError>,
    ) -> Result<(SurfaceMembership, Arc<AdmittedBindingAttachment>), AttachmentStoreError> {
        self.publish_surface_internal(
            session_id,
            presentation_id,
            creation_request_id,
            root_coordinate,
            candidate,
            limits,
            current_policy_generation_digest,
            SurfaceSubjectAdmission::Particular(admission),
            validate_publication,
        )
    }

    fn publish_surface_internal(
        &self,
        session_id: &str,
        presentation_id: &str,
        creation_request_id: &str,
        root_coordinate: &BindingAttachmentCoordinate,
        candidate: BindingAttachmentCandidate,
        limits: &PresentationLimits,
        current_policy_generation_digest: &str,
        admission: SurfaceSubjectAdmission<'_>,
        validate_publication: impl FnOnce(
            &SurfaceMembership,
            &Arc<AdmittedBindingAttachment>,
        ) -> Result<(), AttachmentStoreError>,
    ) -> Result<(SurfaceMembership, Arc<AdmittedBindingAttachment>), AttachmentStoreError> {
        require_canonical_presentation_uuid(presentation_id)?;
        require_canonical_presentation_uuid(creation_request_id)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        let root = exact_presentation_attachment(
            session,
            presentation_id,
            root_coordinate,
            current_policy_generation_digest,
        )?;
        let membership = session
            .presentations
            .get(presentation_id)
            .ok_or(AttachmentStoreError::PresentationInvalid)?;
        if membership.root_coordinate != *root_coordinate {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        // Content selection does not select new project authority. The handler
        // compiles from this retained root, and the store verifies that fact.
        let root_mismatch = candidate.compiled_binding.binding.project_root
            != root.compiled_binding.binding.project_root
            || candidate.registered_project_id != root.registered_project_id
            || match (&candidate.project_authority, &root.project_authority) {
                (None, None) => false,
                (Some(candidate), Some(root)) => !Arc::ptr_eq(candidate, root),
                _ => true,
            };
        let particular_resume = match &admission {
            SurfaceSubjectAdmission::Particular(proof) => Some(proof.receipt().clone()),
            _ => None,
        };
        let receipt_limit = match &admission {
            SurfaceSubjectAdmission::Particular(proof) => proof
                .max_receipt_bytes()
                .min(ryeos_client_base::surface::view_sets::MAX_PARTICULAR_VIEW_SET_LIBRARY_BYTES),
            _ => 0,
        };
        if particular_resume.as_ref().is_some_and(|receipt| {
            serde_json::to_vec(receipt).map_or(true, |encoded| {
                receipt_limit == 0 || encoded.len() > receipt_limit
            })
        }) {
            return Err(AttachmentStoreError::Capacity);
        }
        let subject_matches = match admission {
            SurfaceSubjectAdmission::ExistingRoot => !root_mismatch,
            SurfaceSubjectAdmission::RegisteredProject(proof) => {
                proof.matches_candidate(&session.principal_id, &candidate)
            }
            SurfaceSubjectAdmission::Particular(proof) => {
                proof.matches_candidate(&session.principal_id, &candidate)
            }
        };
        if !subject_matches {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        let generation = session.next_attachment_generation;
        let attachment = Arc::new(build_attachment(
            uuid::Uuid::new_v4().to_string(),
            generation,
            candidate,
            &session.principal_id,
            current_policy_generation_digest,
        )?);
        if !attachment.compiled_binding.binding.root_is_kind("surface") {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        let title = attachment
            .presentation
            .definition
            .surface()
            .ok_or(AttachmentStoreError::PresentationInvalid)?
            .display_title()
            .to_string();
        if title.trim().is_empty()
            || title.len() > limits.max_label_bytes
            || title.chars().any(char::is_control)
        {
            return Err(AttachmentStoreError::PresentationInvalid);
        }
        if let Some(existing) = membership
            .surfaces
            .values()
            .find(|surface| surface.creation_request_id == creation_request_id)
        {
            if existing.closed {
                return Err(AttachmentStoreError::PresentationConflict);
            }
            let retained = exact_attachment(session, &existing.coordinate)?;
            validate_retained_project_path(&retained)?;
            let same_purpose = match (&existing.particular_resume, &particular_resume) {
                (None, None) => true,
                (Some(retained), Some(current)) => retained.same_request(current),
                _ => false,
            };
            return if same_purpose
                && retained.registered_project_id == attachment.registered_project_id
                && retained.compiled_binding.binding_digest
                    == attachment.compiled_binding.binding_digest
                && retained.compiled_binding.binding == attachment.compiled_binding.binding
                && retained.presentation == attachment.presentation
            {
                validate_publication(existing, &retained)?;
                Ok((existing.clone(), retained))
            } else {
                Err(AttachmentStoreError::PresentationConflict)
            };
        }
        // Check every fallible bound/counter before publishing either half.
        if membership.surfaces.len() >= limits.max_surface_instances
            || transfer_reservation::occupied_attachment_count(session) >= limits.max_attachments
        {
            return Err(AttachmentStoreError::Capacity);
        }
        let ordinal = session.next_surface_ordinal;
        let next_ordinal = ordinal
            .checked_add(1)
            .ok_or(AttachmentStoreError::Capacity)?;
        let next_generation = generation
            .checked_add(1)
            .ok_or(AttachmentStoreError::Capacity)?;
        let surface_instance_id = uuid::Uuid::new_v4().to_string();
        let seeds = match &particular_resume {
            Some(receipt) => vec![receipt.resolved.composition.clone()],
            None => attachment
                .presentation
                .definition
                .surface()
                .ok_or(AttachmentStoreError::PresentationInvalid)?
                .to_mount_seeds()
                .map_err(|_| AttachmentStoreError::PresentationInvalid)?,
        };
        let active = seeds.first().map(|seed| seed.id.clone());
        let manifest = allocate_mount_manifest(
            presentation_id,
            &surface_instance_id,
            membership.next_mount_id,
            seeds
                .into_iter()
                .map(|seed| (seed, public_coordinate(&attachment.coordinate())))
                .collect(),
            active.as_deref(),
        )?;
        let next_mount_id = manifest.next_mount_id;
        let surface = SurfaceMembership {
            surface_instance_id,
            creation_request_id: creation_request_id.to_owned(),
            ordinal,
            title,
            coordinate: attachment.coordinate(),
            mount_manifest: manifest,
            initial_seed: None,
            closed: false,
            particular_resume,
            saved_resume: None,
            tab_openings: BTreeMap::new(),
        };
        // Validate the exact allocated result under this lock, before any
        // membership, attachment, receipt or counter becomes observable.
        // Retained retries above pass through the same current bounds.
        validate_publication(&surface, &attachment)?;
        // Membership existence was checked above under this same store lock.
        session
            .presentations
            .get_mut(presentation_id)
            .expect("validated presentation membership")
            .surfaces
            .insert(surface.surface_instance_id.clone(), surface.clone());
        session
            .presentations
            .get_mut(presentation_id)
            .expect("validated presentation membership")
            .next_mount_id = next_mount_id;
        session
            .attachments
            .insert(attachment.binding_attachment_id.clone(), attachment.clone());
        session.next_surface_ordinal = next_ordinal;
        session.next_attachment_generation = next_generation;
        Ok((surface, attachment))
    }

    /// Retire only presentation membership, preserving exact admission history
    /// and the presentation seat. Durable close evidence is owned by SeatLog.
    pub fn retire_surface_membership(
        &self,
        session_id: &str,
        presentation_id: &str,
        surface_instance_id: &str,
        coordinate: &BindingAttachmentCoordinate,
        _current_policy_digest: &str,
    ) -> Result<(), AttachmentStoreError> {
        require_canonical_presentation_uuid(presentation_id)?;
        require_canonical_presentation_uuid(surface_instance_id)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        let membership = session
            .presentations
            .get(presentation_id)
            .ok_or(AttachmentStoreError::PresentationInvalid)?;
        // This owner consumes already-canonical close testimony. Retiring
        // exact membership does not re-admit execution, and a later policy
        // revocation must not strand its imported attachment references.
        exact_shared_presentation_root(session, &membership.root_coordinate)?;
        let surface = membership
            .surfaces
            .get(surface_instance_id)
            .ok_or(AttachmentStoreError::PresentationInvalid)?;
        if surface.coordinate != *coordinate {
            return Err(AttachmentStoreError::PresentationConflict);
        }
        // Check retained attachment identity even on idempotent close retries.
        exact_attachment(session, coordinate)?;
        session
            .presentations
            .get_mut(presentation_id)
            .expect("validated presentation")
            .surfaces
            .get_mut(surface_instance_id)
            .expect("validated Surface")
            .closed = true;
        transfer_reservation::retire_surface_transfer_membership(
            session,
            presentation_id,
            surface_instance_id,
        );
        Ok(())
    }

    /// Check the exact attachment-to-presentation association. Session-wide
    /// admission alone does not permit one presented core to dispatch through
    /// a sibling Surface. This is membership, not a capability grant.
    pub fn require_presentation_attachment(
        &self,
        session_id: &str,
        presentation_id: &str,
        coordinate: &BindingAttachmentCoordinate,
        current_policy_generation_digest: &str,
    ) -> Result<Arc<AdmittedBindingAttachment>, AttachmentStoreError> {
        require_canonical_presentation_uuid(presentation_id)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        exact_presentation_attachment(
            session,
            presentation_id,
            coordinate,
            current_policy_generation_digest,
        )
    }

    /// Store-linearized presentation dispatch admission. The caller also holds
    /// UiState's existing seat-transition lock while proving the current
    /// producer, so takeover and revocation cannot interleave that proof with
    /// this admission. Once issued, this token is already admitted work; it is
    /// not cancelled by a subsequent UI producer replacement.
    pub fn admit_presentation_dispatch(
        &self,
        session_id: &str,
        presentation_id: &str,
        coordinate: &BindingAttachmentCoordinate,
        current_policy_generation_digest: &str,
    ) -> Result<AdmittedAttachmentDispatch, AttachmentStoreError> {
        require_canonical_presentation_uuid(presentation_id)?;
        let mut sessions = self.sessions.lock().unwrap();
        let session = live_session(&mut sessions, session_id)?;
        Ok(AdmittedAttachmentDispatch {
            attachment: exact_presentation_attachment(
                session,
                presentation_id,
                coordinate,
                current_policy_generation_digest,
            )?,
        })
    }

    /// Return whether an exact non-root attachment is still owned by this
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
        if coordinate.binding_attachment_id == session.root_attachment_id {
            return Err(AttachmentStoreError::RootAttachmentImmutable);
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
                && (session.attachments.values().any(|attachment| {
                    attachment.registered_project_id.as_deref() == Some(registered_project_id)
                }) || transfer_reservation::pending_attachments(session).any(|attachment| {
                    attachment.registered_project_id.as_deref() == Some(registered_project_id)
                }))
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

fn require_canonical_presentation_uuid(value: &str) -> Result<(), AttachmentStoreError> {
    let parsed =
        uuid::Uuid::parse_str(value).map_err(|_| AttachmentStoreError::PresentationInvalid)?;
    if parsed.to_string() != value {
        return Err(AttachmentStoreError::PresentationInvalid);
    }
    Ok(())
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

fn require_shared_presentation_root(
    session: &BrowserSession,
    coordinate: &BindingAttachmentCoordinate,
    current_policy_digest: &str,
) -> Result<Arc<AdmittedBindingAttachment>, AttachmentStoreError> {
    let attachment = exact_shared_presentation_root(session, coordinate)?;
    if attachment
        .compiled_binding
        .binding
        .node_policy_generation_digest
        != current_policy_digest
    {
        return Err(AttachmentStoreError::PolicyStale);
    }
    Ok(attachment)
}

fn exact_shared_presentation_root(
    session: &BrowserSession,
    coordinate: &BindingAttachmentCoordinate,
) -> Result<Arc<AdmittedBindingAttachment>, AttachmentStoreError> {
    let attachment = exact_attachment(session, coordinate)?;
    if !attachment.compiled_binding.binding.root_is_kind("config")
        || !matches!(
            &attachment.presentation.definition,
            ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation { .. }
        )
    {
        return Err(AttachmentStoreError::PresentationInvalid);
    }
    Ok(attachment)
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

fn exact_presentation_attachment(
    session: &BrowserSession,
    presentation_id: &str,
    coordinate: &BindingAttachmentCoordinate,
    current_policy_generation_digest: &str,
) -> Result<Arc<AdmittedBindingAttachment>, AttachmentStoreError> {
    let membership = session
        .presentations
        .get(presentation_id)
        .ok_or(AttachmentStoreError::PresentationInvalid)?;
    let root = exact_attachment(session, &membership.root_coordinate)?;
    if root.compiled_binding.binding.node_policy_generation_digest
        != current_policy_generation_digest
    {
        return Err(AttachmentStoreError::PolicyStale);
    }
    if membership.root_coordinate != *coordinate
        && !membership
            .surfaces
            .values()
            .any(|surface| surface.owns_coordinate(coordinate))
        && !membership
            .activated_transfer_reservations
            .values()
            .any(|reservation| reservation.owns_coordinate(coordinate))
    {
        return Err(AttachmentStoreError::PresentationConflict);
    }
    let attachment = exact_attachment(session, coordinate)?;
    if attachment
        .compiled_binding
        .binding
        .node_policy_generation_digest
        != current_policy_generation_digest
    {
        return Err(AttachmentStoreError::PolicyStale);
    }
    Ok(attachment)
}

/// Retrying an admission must not return a descriptor for a predecessor pin
/// merely because a new directory at the same path has identical content.
fn public_coordinate(
    coordinate: &BindingAttachmentCoordinate,
) -> ryeos_client_base::ui::UiBindingAttachmentCoordinate {
    ryeos_client_base::ui::UiBindingAttachmentCoordinate {
        binding_attachment_id: coordinate.binding_attachment_id.clone(),
        binding_generation: coordinate.binding_generation,
        binding_digest: coordinate.binding_digest.clone(),
    }
}

fn allocate_mount_manifest(
    presentation_id: &str,
    surface_instance_id: &str,
    frontier: u64,
    seeds: Vec<(
        ryeos_client_base::surface::view_sets::ViewSetSeedSpec,
        ryeos_client_base::ui::UiBindingAttachmentCoordinate,
    )>,
    active: Option<&str>,
) -> Result<ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest, AttachmentStoreError>
{
    let presentation = ryeos_client_base::ids::PresentationId::try_new(presentation_id.to_owned())
        .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
    let surface =
        ryeos_client_base::ids::SurfaceInstanceId::try_new(surface_instance_id.to_owned())
            .map_err(|_| AttachmentStoreError::PresentationInvalid)?;
    ryeos_client_base::ui::composition_identity::UiSurfaceMountManifest::allocate(
        presentation,
        surface,
        frontier,
        seeds,
        active,
    )
    .map_err(|_| AttachmentStoreError::PresentationInvalid)
}

fn validate_retained_project_path(
    attachment: &AdmittedBindingAttachment,
) -> Result<(), AttachmentStoreError> {
    match (
        &attachment.project_authority,
        attachment.compiled_binding.binding.project_root.as_deref(),
        attachment.project_query_identity.as_deref(),
    ) {
        (None, None, None) => Ok(()),
        (Some(authority), Some(root), Some(query))
            if authority.path() == std::path::Path::new(root) && query == root =>
        {
            authority
                .ensure_path_binding()
                .map_err(|_| AttachmentStoreError::AttachmentStale)
        }
        _ => Err(AttachmentStoreError::AttachmentStale),
    }
}

fn build_attachment(
    binding_attachment_id: String,
    binding_generation: u64,
    candidate: BindingAttachmentCandidate,
    session_principal_id: &str,
    current_policy_generation_digest: &str,
) -> Result<AdmittedBindingAttachment, AttachmentStoreError> {
    let binding = &candidate.compiled_binding.binding;
    if !binding.root_is_kind(candidate.presentation.definition.expected_kind()) {
        return Err(AttachmentStoreError::AttachmentStale);
    }
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
    if candidate
        .registered_project_id
        .as_deref()
        .is_some_and(|id| {
            project_query_identity.is_none()
                || id.is_empty()
                || id.len() > ryeos_client_base::surface::view_sets::MAX_PROJECT_LOCAL_ID_BYTES
                || id.trim() != id
                || id.chars().any(char::is_control)
        })
    {
        return Err(AttachmentStoreError::AttachmentStale);
    }
    Ok(AdmittedBindingAttachment {
        binding_attachment_id,
        binding_generation,
        registered_project_id: candidate.registered_project_id,
        root_ref: binding.surface.canonical_ref.clone(),
        compiled_binding: candidate.compiled_binding,
        presentation: candidate.presentation,
        project_query_identity,
        project_authority: candidate.project_authority,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn empty_session_snapshot_still_requires_current_shell_policy() {
        let store = super::BrowserSessionStore::new();
        let policy = "22".repeat(32);
        let (id, token, _) = store.mint_token(test_shared_context(), 4, &policy).unwrap();
        store.consume_launch_token(&token).unwrap();
        assert!(store.get_session(&id).unwrap().presentations.is_empty());
        let mut called = false;
        assert!(matches!(
            store.with_exact_presentation_read_snapshot(&id, &[], &"33".repeat(32), |_| {
                called = true;
            }),
            Err(super::AttachmentStoreError::PolicyStale)
        ));
        assert!(!called);
        assert!(matches!(
            store.with_exact_transfer_reservation_snapshot(&id, &[], &"33".repeat(32), |_| {
                called = true;
            }),
            Err(super::AttachmentStoreError::PolicyStale)
        ));
        assert!(
            !called,
            "mutable reservation callback cannot bypass shell policy"
        );
        assert_eq!(
            store
                .with_exact_presentation_read_snapshot(&id, &[], &policy, |session| session
                    .presentations
                    .len())
                .unwrap(),
            0
        );
    }

    use super::*;

    pub(crate) fn limits(
        presentations: usize,
        attachments: usize,
        surfaces: usize,
        label: usize,
    ) -> PresentationLimits {
        PresentationLimits {
            max_presentations: presentations,
            max_attachments: attachments,
            max_surface_instances: surfaces,
            max_label_bytes: label,
        }
    }

    fn test_bounds() -> ryeos_client_base::ui::UiBindingRequestBounds {
        ryeos_client_base::ui::UiBindingRequestBounds {
            max_request_bytes: 65536,
            max_input_bytes: 65536,
            max_seat_append_bytes: 65536,
        }
    }

    #[test]
    fn journal_frontier_cache_only_advances_and_preserves_initialization() {
        let store = BrowserSessionStore::new();
        let (id, token) = mint(&store, test_shared_context());
        store.activate_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let policy = "22".repeat(32);
        let membership = store
            .prepare_presentation(
                &id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(4, 16, 16, 128),
                &policy,
                test_bounds(),
            )
            .unwrap();
        store
            .reconcile_presentation_mount_frontier(
                &id,
                &membership.presentation_id,
                &root,
                &policy,
                7,
            )
            .unwrap();
        store
            .reconcile_presentation_mount_frontier(
                &id,
                &membership.presentation_id,
                &root,
                &policy,
                3,
            )
            .unwrap();
        let repaired =
            store.get_session(&id).unwrap().presentations[&membership.presentation_id].clone();
        assert_eq!(repaired.next_mount_id, 7);
        assert_eq!(repaired.initialization, membership.initialization);
        assert_eq!(
            repaired.initialization_digest,
            membership.initialization_digest
        );
        assert!(
            store
                .reconcile_presentation_mount_frontier(
                    &id,
                    &membership.presentation_id,
                    &root,
                    &policy,
                    0
                )
                .is_err()
        );
        assert!(
            store
                .reconcile_presentation_mount_frontier(
                    &id,
                    &membership.presentation_id,
                    &root,
                    &"44".repeat(32),
                    9
                )
                .is_err()
        );
        let mut wrong = root.clone();
        wrong.binding_generation += 1;
        assert!(
            store
                .reconcile_presentation_mount_frontier(
                    &id,
                    &membership.presentation_id,
                    &wrong,
                    &policy,
                    9
                )
                .is_err()
        );
        assert_eq!(
            store.get_session(&id).unwrap().presentations[&membership.presentation_id]
                .next_mount_id,
            7
        );
    }

    #[test]
    fn retained_retry_project_pin_refuses_replaced_path_and_missing_association() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("project");
        std::fs::create_dir(&path).unwrap();
        let authority = Arc::new(lillux::PinnedDirectory::open(&path).unwrap().unwrap());
        let mut context = test_context();
        Arc::make_mut(&mut context.compiled_binding)
            .binding
            .project_root = Some(path.display().to_string());
        let mut attachment = build_attachment(
            uuid::Uuid::new_v4().to_string(),
            1,
            BindingAttachmentCandidate {
                registered_project_id: Some("project-one".into()),
                compiled_binding: context.compiled_binding,
                presentation: context.presentation,
                project_authority: Some(authority),
            },
            "fp:test",
            &"22".repeat(32),
        )
        .unwrap();
        assert!(validate_retained_project_path(&attachment).is_ok());
        let registered = attachment.public_descriptor(test_bounds());
        assert_eq!(
            registered.subject,
            ryeos_client_base::ui::UiBindingSubject::RegisteredProject {
                local_id: "project-one".into()
            }
        );
        assert!(registered.validate().is_ok());
        attachment.registered_project_id = None;
        assert_eq!(
            attachment.public_descriptor(test_bounds()).subject,
            ryeos_client_base::ui::UiBindingSubject::UnregisteredProject
        );
        attachment.registered_project_id = Some("project-one".into());
        assert!(
            build_attachment(
                uuid::Uuid::new_v4().to_string(),
                2,
                BindingAttachmentCandidate {
                    registered_project_id: Some("x".repeat(
                        ryeos_client_base::surface::view_sets::MAX_PROJECT_LOCAL_ID_BYTES + 1
                    )),
                    compiled_binding: attachment.compiled_binding.clone(),
                    presentation: attachment.presentation.clone(),
                    project_authority: attachment.project_authority.clone(),
                },
                "fp:test",
                &"22".repeat(32)
            )
            .is_err()
        );
        let pinned = attachment.project_authority.take();
        assert!(
            validate_retained_project_path(&attachment).is_err(),
            "project root without its retained pin is not projectless"
        );
        attachment.project_authority = pinned;
        std::fs::rename(&path, temporary.path().join("predecessor")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            lillux::PinnedDirectory::open(&path).unwrap().is_some(),
            "replacement directory can be freshly pinned"
        );
        assert_eq!(
            validate_retained_project_path(&attachment).unwrap_err(),
            AttachmentStoreError::AttachmentStale
        );
        let context = test_context();
        let projectless = build_attachment(
            uuid::Uuid::new_v4().to_string(),
            1,
            BindingAttachmentCandidate {
                registered_project_id: None,
                compiled_binding: context.compiled_binding,
                presentation: context.presentation,
                project_authority: None,
            },
            "fp:test",
            &"22".repeat(32),
        )
        .unwrap();
        assert!(validate_retained_project_path(&projectless).is_ok());
        assert_eq!(
            projectless.public_descriptor(test_bounds()).subject,
            ryeos_client_base::ui::UiBindingSubject::Projectless
        );
        let context = test_context();
        assert!(
            build_attachment(
                uuid::Uuid::new_v4().to_string(),
                1,
                BindingAttachmentCandidate {
                    registered_project_id: Some("invented".into()),
                    compiled_binding: context.compiled_binding,
                    presentation: context.presentation,
                    project_authority: None,
                },
                "fp:test",
                &"22".repeat(32)
            )
            .is_err()
        );
    }

    fn test_presentation() -> ryeos_client_base::presentation::UiCompositionPresentation {
        ryeos_client_base::presentation::UiCompositionPresentation::from_effective_surface(
            serde_json::json!({ "name": "test-surface" }),
        )
        .expect("valid typed fixture Surface")
    }

    #[test]
    fn reusable_batch_keeps_kind_origin_and_fresh_request_identity() {
        let store = BrowserSessionStore::new();
        let (id, token) = mint(&store, test_shared_context());
        store.consume_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let policy = "22".repeat(32);
        let membership = store
            .prepare_presentation(
                &id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(4, 16, 16, 128),
                &policy,
                test_bounds(),
            )
            .unwrap();
        let candidate = || {
            let context = test_context();
            BindingAttachmentCandidate {
                registered_project_id: None,
                compiled_binding: context.compiled_binding,
                presentation: context.presentation,
                project_authority: None,
            }
        };
        let origin = ryeos_client_base::surface::saved_surfaces::SavedSurfaceOrigin {
            surface_ref: "surface:ryeos/ui/base".into(),
            project: None,
        };
        let identity = SavedSurfaceRequestIdentity {
            provenance: ryeos_client_base::ui::binding::UiSurfaceCompositionProvenance::Library {saved_id: "reusable".into(), library_revision: 3, record_digest: "ab".repeat(32)}, title: "Reusable".into(), origin: origin.clone(), active_view_set_id: "tab".into(),
            body: SavedSurfaceRequestBody::Reusable { tabs: vec![ReusableSurfaceTabRecord {
                saved_id: "tab".into(), origin,
                template: serde_json::from_value(serde_json::json!({"id":"tab","name":"Tab","composition":{"schema":2,"id":"tab","title":"Tab","root":null},"relationships":[]})).unwrap(),
                fresh_subjects: BTreeMap::new(),
            }] },
        };
        let proof = admission::SavedSurfaceAdmission::reusable(
            "fp:test".into(),
            identity.clone(),
            None,
            vec![None],
        )
        .unwrap();
        let creation = uuid::Uuid::new_v4().to_string();
        let before = store.get_session(&id).unwrap().attachments.len();
        assert!(
            store
                .publish_saved_surface(
                    &id,
                    &membership.presentation_id,
                    &creation,
                    &root,
                    candidate(),
                    vec![candidate()],
                    &proof,
                    &limits(16, 8, 8, 128),
                    &policy,
                    |_, _, _| Err(AttachmentStoreError::Capacity)
                )
                .is_err()
        );
        assert_eq!(store.get_session(&id).unwrap().attachments.len(), before);
        let (surface, _, _) = store
            .publish_saved_surface(
                &id,
                &membership.presentation_id,
                &creation,
                &root,
                candidate(),
                vec![candidate()],
                &proof,
                &limits(16, 8, 8, 128),
                &policy,
                |_, _, _| Ok(()),
            )
            .unwrap();
        let receipt = surface.saved_resume.as_ref().unwrap();
        let projected = receipt.public_descriptor();
        assert!(projected.particular_tabs().is_none());
        assert_eq!(
            projected.reusable_tabs().unwrap()[0].origin.surface_ref,
            "surface:ryeos/ui/base"
        );
        assert!(receipt.matches_request(&identity));
        let mut changed = identity.clone();
        if let SavedSurfaceRequestBody::Reusable { tabs } = &mut changed.body {
            tabs[0].fresh_subjects.insert(
                "run".into(),
                BTreeMap::from([("selection.work".into(), serde_json::json!("other"))]),
            );
        }
        assert!(!receipt.matches_request(&changed));
        let (retry, _, _) = store
            .publish_saved_surface(
                &id,
                &membership.presentation_id,
                &creation,
                &root,
                candidate(),
                vec![candidate()],
                &proof,
                &limits(16, 8, 8, 128),
                &policy,
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert_eq!(surface.surface_instance_id, retry.surface_instance_id);
        assert_eq!(
            store.get_session(&id).unwrap().attachments.len(),
            before + 2
        );
    }

    #[test]
    fn selected_document_prepares_once_without_defaults_or_producer_and_rejects_partial_publication()
     {
        use ryeos_client_base::surface::opening::{
            SurfaceOpenPlacement, SurfaceOpenRequest, SurfaceOpenSelection,
        };
        let store = BrowserSessionStore::new();
        let (id, token) = mint(&store, test_shared_context());
        store.consume_launch_token(&token).unwrap();
        let before = store.get_session(&id).unwrap();
        let root = before.attachments[&before.root_attachment_id].coordinate();
        let opening = SurfaceOpenRequest {
            creation_request_id: uuid::Uuid::new_v4().to_string(),
            selection: SurfaceOpenSelection::Authored {
                surface_ref: "surface:ryeos/ui/base".into(),
            },
            placement: SurfaceOpenPlacement::NewPresentation {
                root_binding: public_coordinate(&root),
            },
        };
        let candidate = || {
            let context = test_context();
            BindingAttachmentCandidate {
                registered_project_id: None,
                compiled_binding: context.compiled_binding,
                presentation: context.presentation,
                project_authority: None,
            }
        };
        let publish = |opening: &SurfaceOpenRequest, limit, fail| {
            store.prepare_selected_presentation(
                &id,
                opening,
                candidate(),
                vec![],
                None,
                &limits(4, limit, 4, 128),
                &"22".repeat(32),
                test_bounds(),
                |_, _| {
                    if fail {
                        Err(AttachmentStoreError::Capacity)
                    } else {
                        Ok(())
                    }
                },
            )
        };
        assert!(publish(&opening, 0, false).is_err());
        assert!(publish(&opening, 32, true).is_err());
        let refused = store.get_session(&id).unwrap();
        assert_eq!(refused.presentations, before.presentations);
        assert_eq!(
            refused.next_attachment_generation,
            before.next_attachment_generation
        );
        assert_eq!(refused.next_surface_ordinal, before.next_surface_ordinal);
        assert_eq!(refused.attachments.len(), before.attachments.len());
        let child = publish(&opening, 32, false).unwrap();
        child.initialization.validate().unwrap();
        assert_eq!(child.surfaces.len(), 1);
        assert!(
            child
                .surfaces
                .values()
                .all(|surface| surface.initial_seed.is_none())
        );
        assert!(child.thread_id.is_some());
        assert_eq!(child.thread_id, child.chain_root_id);
        let receipt = ryeos_client_base::ui::binding::UiPreparedPresentationDocument {
            presentation_id: child.presentation_id.clone(),
            thread_id: child.thread_id.clone().unwrap(),
            chain_root_id: child.chain_root_id.clone().unwrap(),
            root_binding: public_coordinate(&root),
            initialization: child.initialization.clone(),
        };
        assert!(child.matches_prepared_document(&opening.creation_request_id, &receipt));
        let mut changed_receipt = receipt.clone();
        changed_receipt.root_binding.binding_generation += 1;
        assert!(!child.matches_prepared_document(&opening.creation_request_id, &changed_receipt));
        let mut changed_receipt = receipt.clone();
        changed_receipt.initialization.surfaces[0].title = "Altered".into();
        assert!(!child.matches_prepared_document(&opening.creation_request_id, &changed_receipt));
        let mut changed_receipt = receipt;
        changed_receipt.thread_id = "T-other".into();
        assert!(!child.matches_prepared_document(&opening.creation_request_id, &changed_receipt));
        // A reserved thread coordinate is not a producer or a signed seat.
        // Durable materialization and the child's later Fresh claim are tested
        // by their separate existing journal/control owners.
        let after = store.get_session(&id).unwrap();
        assert_eq!(publish(&opening, 32, false).unwrap(), child);
        let retry = store.get_session(&id).unwrap();
        assert_eq!(
            retry.next_attachment_generation,
            after.next_attachment_generation
        );
        assert_eq!(retry.attachments.len(), after.attachments.len());
        let mut changed = opening.clone();
        changed.selection = SurfaceOpenSelection::Authored {
            surface_ref: "surface:test/other".into(),
        };
        assert!(publish(&changed, 32, false).is_err());
        assert!(
            store
                .prepare_presentation_with_initial_surfaces(
                    &id,
                    &opening.creation_request_id,
                    &root,
                    vec![],
                    &limits(4, 32, 4, 128),
                    &"22".repeat(32),
                    test_bounds(),
                    |_, _| Ok(())
                )
                .is_err()
        );
        assert_eq!(
            store.get_session(&id).unwrap().presentations,
            after.presentations
        );
    }

    #[test]
    fn all_tab_opening_is_atomic_retryable_and_releases_only_settled_unowned_bindings() {
        use ryeos_client_base::{
            surface::opening::{SurfaceOpenPlacement, SurfaceOpenRequest, SurfaceOpenSelection},
            ui::{
                composition_replay::UiCompositionReplay,
                presentation_initialization::UiPresentationInitializationRecord,
                seat::{SeatEvent, SeatEventKind},
            },
        };
        let seed = ryeos_client_base::presentation::PresentationSurfaceSeed {
            id: "destination".into(),
            title: "Destination".into(),
            surface: "surface:ryeos/ui/base".into(),
        };
        let mut context = test_shared_context();
        let ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation { config } =
            &mut context.presentation.definition
        else {
            unreachable!()
        };
        let other_seed = ryeos_client_base::presentation::PresentationSurfaceSeed {
            id: "other".into(),
            title: "Other".into(),
            surface: seed.surface.clone(),
        };
        config.shared_presentation.initial_surfaces = vec![seed.clone(), other_seed.clone()];
        let store = BrowserSessionStore::new();
        let (id, token) = mint(&store, context);
        store.consume_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let candidate = || {
            let context = test_context();
            BindingAttachmentCandidate {
                registered_project_id: None,
                compiled_binding: context.compiled_binding,
                presentation: context.presentation,
                project_authority: None,
            }
        };
        let policy = "22".repeat(32);
        let membership = store
            .prepare_presentation_with_initial_surfaces(
                &id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                vec![
                    InitialSurfaceCandidate {
                        seed,
                        candidate: candidate(),
                    },
                    InitialSurfaceCandidate {
                        seed: other_seed,
                        candidate: candidate(),
                    },
                ],
                &limits(4, 32, 8, 128),
                &policy,
                test_bounds(),
                |_, _| Ok(()),
            )
            .unwrap();
        let initial = membership.initialization.clone();
        let event = SeatEvent {
            seq: 0,
            producer_engine_seq: None,
            kind: SeatEventKind::PresentationInitialized(UiPresentationInitializationRecord {
                operation_id: initial.creation_request_id.clone(),
                initialization: initial.clone(),
            }),
        };
        let mut replay = UiCompositionReplay::begin(&initial, &event).unwrap();
        let destination = membership.surfaces.values().next().unwrap();
        let opening = SurfaceOpenRequest {
            creation_request_id: uuid::Uuid::new_v4().to_string(),
            selection: SurfaceOpenSelection::Authored {
                surface_ref: "surface:ryeos/ui/base".into(),
            },
            placement: SurfaceOpenPlacement::AppendTabs {
                destination_surface_instance_id:
                    ryeos_client_base::ids::SurfaceInstanceId::try_new(
                        &destination.surface_instance_id,
                    )
                    .unwrap(),
                destination_binding: public_coordinate(&destination.coordinate),
                expected_composition_revision: replay.revision,
                expected_next_mount_id: replay.next_mount_id,
                insertion_index: 0,
                activate_imported_tab: false,
            },
        };
        let before = store.get_session(&id).unwrap();
        for field in ["revision", "frontier", "binding", "index"] {
            let mut stale = opening.clone();
            let SurfaceOpenPlacement::AppendTabs {
                expected_composition_revision,
                expected_next_mount_id,
                destination_binding,
                insertion_index,
                ..
            } = &mut stale.placement
            else {
                unreachable!()
            };
            match field {
                "revision" => *expected_composition_revision += 1,
                "frontier" => *expected_next_mount_id += 1,
                "binding" => destination_binding.binding_generation += 1,
                _ => *insertion_index = usize::MAX,
            }
            assert!(
                store
                    .publish_surface_tabs(
                        &id,
                        &membership.presentation_id,
                        &root,
                        &stale,
                        &replay,
                        candidate(),
                        vec![],
                        None,
                        &limits(16, 32, 16, 128),
                        test_bounds(),
                        &policy,
                        |_| panic!("stale destination must refuse before publication")
                    )
                    .is_err()
            );
            assert_eq!(
                store.get_session(&id).unwrap().presentations,
                before.presentations
            );
        }
        {
            let origin = ryeos_client_base::surface::saved_surfaces::SavedSurfaceOrigin {
                surface_ref: "surface:ryeos/ui/base".into(),
                project: None,
            };
            let tabs=["one","two"].into_iter().map(|id| ryeos_client_base::surface::opening::ComposedSurfaceTab {
                origin:origin.clone(),template:serde_json::from_value(serde_json::json!({"id":id,"name":id,"composition":{"schema":2,"id":id,"title":id,"root":null},"relationships":[]})).unwrap(),fresh_subjects:BTreeMap::new(),
            }).collect::<Vec<_>>();
            let composition = ryeos_client_base::surface::opening::ComposedSurfaceSelection {
                origin: origin.clone(),
                title: "Composed".into(),
                active_view_set_id: "one".into(),
                tabs: tabs.clone(),
            };
            let identity = SavedSurfaceRequestIdentity {
                provenance:
                    ryeos_client_base::ui::binding::UiSurfaceCompositionProvenance::Composed {
                        composition: composition.clone(),
                    },
                title: composition.title.clone(),
                origin,
                active_view_set_id: composition.active_view_set_id.clone(),
                body: SavedSurfaceRequestBody::Composed {
                    tabs: tabs
                        .into_iter()
                        .map(|tab| ReusableSurfaceTabRecord {
                            saved_id: tab.template.id.clone(),
                            origin: tab.origin,
                            template: tab.template,
                            fresh_subjects: tab.fresh_subjects,
                        })
                        .collect(),
                },
            };
            let proof = admission::SavedSurfaceAdmission::reusable(
                "fp:test".into(),
                identity,
                None,
                vec![None, None],
            )
            .unwrap();
            let mut composed = opening.clone();
            composed.selection = SurfaceOpenSelection::Composed(composition);
            let mut bad = candidate();
            Arc::make_mut(&mut bad.compiled_binding)
                .binding
                .node_policy_generation_digest = "ff".repeat(32);
            assert!(
                store
                    .publish_surface_tabs(
                        &id,
                        &membership.presentation_id,
                        &root,
                        &composed,
                        &replay,
                        candidate(),
                        vec![candidate(), bad],
                        Some(&proof),
                        &limits(16, 32, 16, 128),
                        test_bounds(),
                        &policy,
                        |_| panic!("bad later tab must not publish earlier tabs")
                    )
                    .is_err()
            );
            let after = store.get_session(&id).unwrap();
            assert_eq!(after.presentations, before.presentations);
            assert_eq!(after.attachments.len(), before.attachments.len());
            assert_eq!(
                after.next_attachment_generation,
                before.next_attachment_generation
            );
        }
        let publish = |limit, fail| {
            store.publish_surface_tabs(
                &id,
                &membership.presentation_id,
                &root,
                &opening,
                &replay,
                candidate(),
                vec![],
                None,
                &limits(16, limit, 16, 128),
                test_bounds(),
                &policy,
                |_| {
                    if fail {
                        Err(AttachmentStoreError::Capacity)
                    } else {
                        Ok(())
                    }
                },
            )
        };
        assert!(publish(0, false).is_err());
        assert!(publish(32, true).is_err());
        let refused = store.get_session(&id).unwrap();
        assert_eq!(
            refused.next_attachment_generation,
            before.next_attachment_generation
        );
        assert_eq!(refused.attachments.len(), before.attachments.len());
        assert_eq!(refused.presentations, before.presentations);
        let result = publish(32, false).unwrap();
        assert_eq!(publish(32, false).unwrap(), result);
        let published = store.get_session(&id).unwrap();
        let live = &published.presentations[&membership.presentation_id];
        assert_eq!(
            live.surfaces[&destination.surface_instance_id].coordinate,
            destination.coordinate
        );
        assert_eq!(
            live.surfaces[&destination.surface_instance_id].mount_manifest,
            destination.mount_manifest
        );
        assert_eq!(
            live.surfaces[&destination.surface_instance_id]
                .tab_openings
                .len(),
            1
        );
        let imported = result.origin_attachment.binding_attachment_id.clone();
        {
            // Store-owner reconciliation consumes an already-checked replay.
            // A moved tab's insertion edge, then its hidden return frame, keep
            // the import alive after the original hosting Surface closes.
            let mut moved = published.clone();
            moved
                .presentations
                .get_mut(&membership.presentation_id)
                .unwrap()
                .surfaces
                .get_mut(&destination.surface_instance_id)
                .unwrap()
                .closed = true;
            let mut moved_replay = replay.clone();
            moved_replay.composition.close_surface(
                &ryeos_client_base::ids::SurfaceInstanceId::try_new(
                    &destination.surface_instance_id,
                )
                .unwrap(),
            );
            Arc::make_mut(&mut moved_replay.settled).insert(opening.creation_request_id.clone());
            let other_set = moved_replay.composition.view_sets().next().unwrap().1;
            let other_set_id = other_set.id;
            let other_instance = moved_replay
                .composition
                .dialogs()
                .values()
                .next()
                .unwrap()
                .view()
                .instance_key()
                .clone();
            let imported_coordinate = ryeos_client_base::ui::UiBindingAttachmentCoordinate {
                binding_attachment_id: imported.clone(),
                binding_generation: result.origin_attachment.binding_generation,
                binding_digest: result.origin_attachment.binding_digest.clone(),
            };
            let previous = moved_replay
                .insertion_bindings
                .insert(other_set_id, imported_coordinate.clone());
            assert_eq!(
                surface_opening::reconcile_tab_openings(
                    &mut moved,
                    &membership.presentation_id,
                    &moved_replay
                )
                .unwrap(),
                0
            );
            assert!(moved.attachments.contains_key(&imported));
            match previous {
                Some(binding) => {
                    moved_replay
                        .insertion_bindings
                        .insert(other_set_id, binding);
                }
                None => {
                    moved_replay.insertion_bindings.remove(&other_set_id);
                }
            }
            moved_replay.lens_frames.insert(
                other_instance.clone(),
                vec![ryeos_client_base::ui::composition_lens::RetainedLensFrame {
                    binding: imported_coordinate,
                    view_ref: "view:test/home".into(),
                    attachment:
                        ryeos_client_base::ui::attachment::SelectionAttachment::FollowViewSet {
                            view_set_id: other_set_id,
                        },
                    input_route: None,
                    label: None,
                }],
            );
            assert_eq!(
                surface_opening::reconcile_tab_openings(
                    &mut moved,
                    &membership.presentation_id,
                    &moved_replay
                )
                .unwrap(),
                0
            );
            assert!(moved.attachments.contains_key(&imported));
            moved_replay.lens_frames.remove(&other_instance);
            assert_eq!(
                surface_opening::reconcile_tab_openings(
                    &mut moved,
                    &membership.presentation_id,
                    &moved_replay
                )
                .unwrap(),
                1
            );
            assert!(!moved.attachments.contains_key(&imported));
        }
        {
            let mut closed = published.clone();
            closed
                .presentations
                .get_mut(&membership.presentation_id)
                .unwrap()
                .surfaces
                .get_mut(&destination.surface_instance_id)
                .unwrap()
                .closed = true;
            let mut closed_replay = replay.clone();
            closed_replay.composition.close_surface(
                &ryeos_client_base::ids::SurfaceInstanceId::try_new(
                    &destination.surface_instance_id,
                )
                .unwrap(),
            );
            assert_eq!(
                surface_opening::reconcile_tab_openings(
                    &mut closed,
                    &membership.presentation_id,
                    &closed_replay
                )
                .unwrap(),
                1
            );
            assert!(!closed.attachments.contains_key(&imported));
            assert!(closed.attachments.contains_key(&root.binding_attachment_id));
        }
        {
            let mut sessions = store.sessions.lock().unwrap();
            let session = sessions.get_mut(&id).unwrap();
            // Pending result still owns its exact attachments before ACK.
            assert_eq!(
                surface_opening::reconcile_tab_openings(
                    session,
                    &membership.presentation_id,
                    &replay
                )
                .unwrap(),
                0
            );
            assert!(session.attachments.contains_key(&imported));
        }
        Arc::make_mut(&mut replay.settled).insert(opening.creation_request_id.clone());
        let retained_set = replay.composition.view_sets().next().unwrap().1.id;
        let previous_insertion = replay.insertion_bindings.insert(
            retained_set,
            ryeos_client_base::ui::UiBindingAttachmentCoordinate {
                binding_attachment_id: result.origin_attachment.binding_attachment_id.clone(),
                binding_generation: result.origin_attachment.binding_generation,
                binding_digest: result.origin_attachment.binding_digest.clone(),
            },
        );
        {
            let mut sessions = store.sessions.lock().unwrap();
            let session = sessions.get_mut(&id).unwrap();
            // Current insertion authority keeps a completed batch live even
            // when its empty tab has no visible tile from which to infer it.
            assert_eq!(
                surface_opening::reconcile_tab_openings(
                    session,
                    &membership.presentation_id,
                    &replay
                )
                .unwrap(),
                0
            );
            assert!(session.attachments.contains_key(&imported));
        }
        match previous_insertion {
            Some(binding) => {
                replay.insertion_bindings.insert(retained_set, binding);
            }
            None => {
                replay.insertion_bindings.remove(&retained_set);
            }
        }
        // Model a settled rejection/closed imported batch: there are no live
        // imported edges. The immutable receipt remains but grants nothing.
        {
            let mut sessions = store.sessions.lock().unwrap();
            let session = sessions.get_mut(&id).unwrap();
            assert_eq!(
                surface_opening::reconcile_tab_openings(
                    session,
                    &membership.presentation_id,
                    &replay
                )
                .unwrap(),
                1
            );
            assert!(!session.attachments.contains_key(&imported));
            let receipt = &session.presentations[&membership.presentation_id].surfaces
                [&destination.surface_instance_id]
                .tab_openings[&opening.creation_request_id];
            assert_eq!(receipt.result, result);
            assert!(receipt.current_coordinates.is_empty());
            assert!(
                session
                    .attachments
                    .contains_key(&root.binding_attachment_id)
            );
        }
    }

    #[test]
    fn exact_read_session_refuses_changed_policy_membership_and_expiry_before_action() {
        let store = BrowserSessionStore::new();
        let (id, token) = mint(&store, test_shared_context());
        store.consume_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let policy = "22".repeat(32);
        let membership = store
            .prepare_presentation(
                &id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(4, 16, 16, 128),
                &policy,
                test_bounds(),
            )
            .unwrap();
        let mut calls = 0;
        assert_eq!(
            store
                .with_exact_presentation_read_session(
                    &id,
                    std::slice::from_ref(&membership),
                    &policy,
                    || {
                        calls += 1;
                        17
                    },
                )
                .unwrap(),
            17
        );
        assert!(
            store
                .with_exact_presentation_read_session(
                    &id,
                    std::slice::from_ref(&membership),
                    &"44".repeat(32),
                    || {
                        calls += 1;
                    },
                )
                .is_err()
        );
        let mut changed = membership.clone();
        changed.thread_id = Some("another-thread".into());
        assert!(
            store
                .with_exact_presentation_read_session(&id, &[changed], &policy, || {
                    calls += 1;
                },)
                .is_err()
        );
        assert!(
            store
                .with_exact_presentation_read_session(&id, &[], &policy, || {
                    calls += 1;
                },)
                .is_err()
        );
        assert_eq!(
            store
                .with_exact_presentation_read_snapshot(
                    &id,
                    std::slice::from_ref(&membership),
                    &policy,
                    |locked| {
                        calls += 1;
                        locked.presentations.len()
                    },
                )
                .unwrap(),
            1
        );
        assert!(
            store
                .with_exact_presentation_read_snapshot(&id, &[], &policy, |_| {
                    calls += 1;
                },)
                .is_err()
        );
        let mut stale = membership.clone();
        stale.next_mount_id += 1;
        assert!(
            store
                .with_exact_presentation_read_snapshot(&id, &[stale], &policy, |_| {
                    calls += 1;
                },)
                .is_err()
        );
        store
            .sessions
            .lock()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .expires_at = Instant::now();
        assert!(
            store
                .with_exact_presentation_read_session(&id, &[membership], &policy, || {
                    calls += 1;
                },)
                .is_err()
        );
        assert_eq!(calls, 2);
        // Read admission neither renews nor removes an expired session.
        assert!(store.sessions.lock().unwrap().contains_key(&id));
    }

    #[test]
    fn saved_surface_batch_preflight_late_failure_retry_and_close_are_atomic() {
        let store = BrowserSessionStore::new();
        let (id, token) = mint(&store, test_shared_context());
        store.consume_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let policy = "22".repeat(32);
        let membership = store
            .prepare_presentation(
                &id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(4, 16, 16, 128),
                &policy,
                test_bounds(),
            )
            .unwrap();
        let initial = store.get_session(&id).unwrap();
        let candidate = || {
            let context = test_context();
            BindingAttachmentCandidate {
                registered_project_id: None,
                compiled_binding: context.compiled_binding,
                presentation: context.presentation,
                project_authority: None,
            }
        };
        let record = |name: &str| ParticularSurfaceReceipt {
            saved_id: name.into(),
            library_revision: 1,
            record_digest: format!("digest-{name}"),
            surface_ref: "surface:ryeos/ui/base".into(),
            registered_project_id: None,
            resolved: serde_json::from_value(serde_json::json!({"id":name,"name":name,
                "composition":{"schema":2,"id":name,"title":name,"root":null},"relationships":[],
                "resolved_selection_work":{"thread":"T-one","chain_root":"T-root"}}))
            .unwrap(),
        };
        let identity = SavedSurfaceRequestIdentity {
            provenance: ryeos_client_base::ui::binding::UiSurfaceCompositionProvenance::Library {
                saved_id: "collection".into(),
                library_revision: 3,
                record_digest: "cd".repeat(32),
            },
            title: "Collection".into(),
            origin: ryeos_client_base::surface::saved_surfaces::SavedSurfaceOrigin {
                surface_ref: "surface:ryeos/ui/base".into(),
                project: None,
            },
            active_view_set_id: "second".into(),
            body: SavedSurfaceRequestBody::Particular {
                tabs: vec![record("first"), record("second")],
            },
        };
        let proof =
            admission::SavedSurfaceAdmission::projectless_fixture("fp:test", identity.clone());
        let request = uuid::Uuid::new_v4().to_string();
        assert!(
            store
                .publish_saved_surface(
                    &id,
                    &membership.presentation_id,
                    &request,
                    &root,
                    candidate(),
                    vec![candidate(), candidate()],
                    &proof,
                    &limits(16, 8, 8, 128),
                    &policy,
                    |_, _, _| Err(AttachmentStoreError::Capacity)
                )
                .is_err()
        );
        let rejected = store.get_session(&id).unwrap();
        assert_eq!(rejected.attachments.len(), initial.attachments.len());
        assert_eq!(
            rejected.next_attachment_generation,
            initial.next_attachment_generation
        );
        assert_eq!(rejected.next_surface_ordinal, initial.next_surface_ordinal);
        let mut wrong = candidate();
        Arc::make_mut(&mut wrong.compiled_binding)
            .binding
            .node_policy_generation_digest = "44".repeat(32);
        assert!(
            store
                .publish_saved_surface(
                    &id,
                    &membership.presentation_id,
                    &request,
                    &root,
                    candidate(),
                    vec![candidate(), wrong],
                    &proof,
                    &limits(16, 8, 8, 128),
                    &policy,
                    |_, _, _| Ok(())
                )
                .is_err()
        );
        assert_eq!(
            store.get_session(&id).unwrap().attachments.len(),
            initial.attachments.len()
        );
        let (surface, _, tabs) = store
            .publish_saved_surface(
                &id,
                &membership.presentation_id,
                &request,
                &root,
                candidate(),
                vec![candidate(), candidate()],
                &proof,
                &limits(16, 8, 8, 128),
                &policy,
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert_eq!(
            store.get_session(&id).unwrap().attachments.len(),
            initial.attachments.len() + 3
        );
        let SavedSurfaceReceiptBody::Particular {
            tabs: retained_tabs,
        } = &surface.saved_resume.as_ref().unwrap().body
        else {
            panic!("particular receipt expected")
        };
        assert_eq!(retained_tabs[0].record.saved_id, "first");
        for tab in &tabs {
            assert!(
                store
                    .require_presentation_attachment(
                        &id,
                        &membership.presentation_id,
                        &tab.coordinate(),
                        &policy
                    )
                    .is_ok()
            );
        }
        let mut later = identity;
        let SavedSurfaceRequestBody::Particular { tabs: later_tabs } = &mut later.body else {
            panic!("particular request expected")
        };
        later_tabs[0]
            .resolved
            .resolved_selection_work
            .as_mut()
            .unwrap()
            .thread = "T-next".into();
        let later = admission::SavedSurfaceAdmission::projectless_fixture("fp:test", later);
        let before = store.get_session(&id).unwrap();
        let (retry, _, _) = store
            .publish_saved_surface(
                &id,
                &membership.presentation_id,
                &request,
                &root,
                candidate(),
                vec![candidate(), candidate()],
                &later,
                &limits(16, 0, 0, 128),
                &policy,
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert_eq!(
            retry, surface,
            "retry retains original exact resolved batch"
        );
        assert_eq!(
            store.get_session(&id).unwrap().next_attachment_generation,
            before.next_attachment_generation
        );
        store
            .retire_surface_membership(
                &id,
                &membership.presentation_id,
                &surface.surface_instance_id,
                &surface.coordinate,
                &policy,
            )
            .unwrap();
        for tab in &tabs {
            assert!(
                store
                    .require_presentation_attachment(
                        &id,
                        &membership.presentation_id,
                        &tab.coordinate(),
                        &policy
                    )
                    .is_err()
            );
        }
        assert!(
            store
                .publish_saved_surface(
                    &id,
                    &membership.presentation_id,
                    &request,
                    &root,
                    candidate(),
                    vec![candidate(), candidate()],
                    &proof,
                    &limits(16, 8, 8, 128),
                    &policy,
                    |_, _, _| Ok(())
                )
                .is_err(),
            "closed collection never reappears on retry"
        );
    }

    #[test]
    fn attachment_admission_rejects_definition_root_kind_mismatch() {
        let mut context = test_context();
        let mut compiled = (*context.compiled_binding).clone();
        compiled.binding.surface.canonical_ref = "config:ryeos/test/shared".into();
        context.compiled_binding = Arc::new(compiled);
        let store = BrowserSessionStore::new();
        assert_eq!(
            store.mint_token(context, 4, &"22".repeat(32)).unwrap_err(),
            AttachmentStoreError::AttachmentStale
        );
    }

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

    pub(crate) fn test_context() -> LaunchContext {
        LaunchContext {
            compiled_binding: test_binding(
                "surface:ryeos/ui/base",
                None,
                crate::compiled_binding::EffectiveUiPosture::Interactive,
            ),
            presentation: test_presentation(),
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

    pub(crate) fn test_shared_context() -> LaunchContext {
        let mut context = test_context();
        let binding = Arc::make_mut(&mut context.compiled_binding);
        binding.binding.surface.canonical_ref = "config:ryeos/test/presentation".into();
        binding.posture = crate::compiled_binding::EffectiveUiPosture::ObservationOnly;
        let config = serde_json::from_value(serde_json::json!({
            "category": "ryeos/test",
            "shared_presentation": {
                "schema": 1, "title": "Test presentation", "launcher": "launch",
                "dialogs": [{"id": "launch", "title": "Launch", "view": "view:ryeos/test/launcher"}],
                "capabilities": {"sources": [], "affordances": []}
            }
        })).expect("typed shared Config fixture");
        context.presentation = ryeos_client_base::presentation::UiCompositionPresentation {
            definition:
                ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation {
                    config,
                },
            views: std::collections::BTreeMap::from([(
                "view:ryeos/test/launcher".into(),
                serde_json::json!({"widget":"rows"}),
            )]),
        };
        context
    }

    #[test]
    fn initial_surfaces_publish_as_one_exact_batch_and_retry_does_not_reseed() {
        use ryeos_client_base::presentation::{PresentationSurfaceSeed, UiCompositionDefinition};
        let surface_ref = test_context()
            .compiled_binding
            .binding
            .surface
            .canonical_ref
            .clone();
        let seeds = vec![
            PresentationSurfaceSeed {
                id: "first".into(),
                title: "First".into(),
                surface: surface_ref.clone(),
            },
            PresentationSurfaceSeed {
                id: "second".into(),
                title: "Second".into(),
                surface: surface_ref,
            },
        ];
        let mut context = test_shared_context();
        let UiCompositionDefinition::SharedPresentation { config } =
            &mut context.presentation.definition
        else {
            panic!("shared fixture")
        };
        config.shared_presentation.initial_surfaces = seeds.clone();
        let store = BrowserSessionStore::new();
        let (id, token) = mint(&store, context);
        store.activate_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let request = uuid::Uuid::new_v4().to_string();
        let policy = "22".repeat(32);
        let candidates = || {
            seeds
                .iter()
                .map(|seed| {
                    let context = test_context();
                    InitialSurfaceCandidate {
                        seed: seed.clone(),
                        candidate: BindingAttachmentCandidate {
                            registered_project_id: None,
                            compiled_binding: context.compiled_binding,
                            presentation: context.presentation,
                            project_authority: None,
                        },
                    }
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            store
                .prepared_presentation(&id, &request, &root, &policy)
                .unwrap(),
            None
        );
        assert!(
            store
                .prepare_presentation(
                    &id,
                    &request,
                    &root,
                    &limits(4, 16, 16, 128),
                    &policy,
                    test_bounds()
                )
                .is_err()
        );
        let mut invalid = candidates();
        Arc::make_mut(&mut invalid[1].candidate.compiled_binding)
            .binding
            .node_policy_generation_digest = "44".repeat(32);
        assert!(
            store
                .prepare_presentation_with_initial_surfaces(
                    &id,
                    &request,
                    &root,
                    invalid,
                    &limits(4, 8, 8, 128),
                    &policy,
                    test_bounds(),
                    |_, _| Ok(()),
                )
                .is_err()
        );
        let rejected = store.get_session(&id).unwrap();
        assert!(rejected.presentations.is_empty());
        assert_eq!(rejected.attachments.len(), 1);
        assert_eq!(
            rejected.next_attachment_generation,
            session.next_attachment_generation
        );
        assert_eq!(rejected.next_surface_ordinal, session.next_surface_ordinal);
        // The complete allocated batch is available to the signed-envelope
        // validator before any membership, attachment or frontier is visible.
        assert_eq!(
            store
                .prepare_presentation_with_initial_surfaces(
                    &id,
                    &request,
                    &root,
                    candidates(),
                    &limits(4, 8, 8, 128),
                    &policy,
                    test_bounds(),
                    |membership, descriptors| {
                        assert_eq!(descriptors.len(), 3);
                        assert!(membership.next_mount_id > 1);
                        assert!(
                            membership
                                .surfaces
                                .values()
                                .all(|surface| surface.mount_manifest.validate().is_ok())
                        );
                        Err(AttachmentStoreError::Capacity)
                    },
                )
                .unwrap_err(),
            AttachmentStoreError::Capacity
        );
        let preflight_rejected = store.get_session(&id).unwrap();
        assert!(preflight_rejected.presentations.is_empty());
        assert_eq!(preflight_rejected.attachments.len(), 1);
        assert_eq!(
            preflight_rejected.next_attachment_generation,
            session.next_attachment_generation
        );
        assert_eq!(
            preflight_rejected.next_surface_ordinal,
            session.next_surface_ordinal
        );
        assert!(
            store
                .prepare_presentation_with_initial_surfaces(
                    &id,
                    &request,
                    &root,
                    candidates(),
                    &limits(4, 2, 8, 128),
                    &policy,
                    test_bounds(),
                    |_, _| Ok(()),
                )
                .is_err()
        );
        let membership = store
            .prepare_presentation_with_initial_surfaces(
                &id,
                &request,
                &root,
                candidates(),
                &limits(4, 8, 8, 128),
                &policy,
                test_bounds(),
                |_, _| Ok(()),
            )
            .unwrap();
        let mut ordered = membership.surfaces.values().collect::<Vec<_>>();
        ordered.sort_by_key(|surface| surface.ordinal);
        assert_eq!(
            ordered
                .iter()
                .map(|surface| surface.title.as_str())
                .collect::<Vec<_>>(),
            vec!["First", "Second"]
        );
        assert_eq!(ordered[0].initial_seed.as_ref(), Some(&seeds[0]));
        assert_eq!(ordered[1].initial_seed.as_ref(), Some(&seeds[1]));
        assert_eq!(ordered[0].mount_manifest.allocation_start, 1);
        assert_eq!(
            ordered[1].mount_manifest.allocation_start,
            ordered[0].mount_manifest.next_mount_id
        );
        assert_eq!(
            membership.next_mount_id,
            ordered[1].mount_manifest.next_mount_id
        );
        assert!(
            ordered
                .iter()
                .all(|surface| surface.mount_manifest.validate().is_ok())
        );
        let published = store.get_session(&id).unwrap();
        assert_eq!(published.attachments.len(), 3);
        assert_eq!(membership.initialization.binding_attachments.len(), 3);
        assert_eq!(membership.initialization.surfaces.len(), 2);
        assert_eq!(
            membership.initialization_digest,
            membership.initialization.digest().unwrap()
        );
        assert_eq!(
            store
                .prepared_presentation(&id, &request, &root, &policy)
                .unwrap(),
            Some(membership.clone())
        );
        // A retained retry never recompiles or reopens seed selections, even
        // if current creation capacity has since been exhausted.
        assert_eq!(
            store
                .prepare_presentation_with_initial_surfaces(
                    &id,
                    &request,
                    &root,
                    Vec::new(),
                    &limits(0, 0, 0, 0),
                    &policy,
                    test_bounds(),
                    |_, _| Ok(()),
                )
                .unwrap(),
            membership
        );
        let replayed = store.get_session(&id).unwrap();
        assert_eq!(
            replayed.next_attachment_generation,
            published.next_attachment_generation
        );
        assert_eq!(
            replayed.next_surface_ordinal,
            published.next_surface_ordinal
        );
        let retired = ordered[0];
        let sibling = ordered[1];
        let revoked_policy = "33".repeat(32);
        assert!(
            store
                .require_presentation_attachment(
                    &id,
                    &membership.presentation_id,
                    &root,
                    &revoked_policy,
                )
                .is_err()
        );
        assert!(
            store
                .retire_surface_membership(
                    &id,
                    &membership.presentation_id,
                    &retired.surface_instance_id,
                    &sibling.coordinate,
                    &policy
                )
                .is_err()
        );
        store
            .retire_surface_membership(
                &id,
                &membership.presentation_id,
                &retired.surface_instance_id,
                &retired.coordinate,
                &revoked_policy,
            )
            .unwrap();
        store
            .retire_surface_membership(
                &id,
                &membership.presentation_id,
                &retired.surface_instance_id,
                &retired.coordinate,
                &revoked_policy,
            )
            .unwrap();
        assert!(
            store
                .require_presentation_attachment(
                    &id,
                    &membership.presentation_id,
                    &retired.coordinate,
                    &policy
                )
                .is_err()
        );
        assert!(
            store
                .require_presentation_attachment(
                    &id,
                    &membership.presentation_id,
                    &sibling.coordinate,
                    &policy
                )
                .is_ok()
        );
        assert!(
            store
                .require_presentation_attachment(&id, &membership.presentation_id, &root, &policy)
                .is_ok()
        );
        let closed = store.get_session(&id).unwrap();
        let closed_membership = &closed.presentations[&membership.presentation_id];
        assert_eq!(closed_membership.initialization, membership.initialization);
        assert_eq!(
            closed_membership.initialization_digest,
            membership.initialization_digest
        );
        assert_eq!(
            closed_membership.public_descriptor().initialization_digest,
            membership.initialization_digest
        );
        assert_eq!(closed.attachments.len(), published.attachments.len());
        assert_eq!(closed.next_surface_ordinal, published.next_surface_ordinal);
        assert_eq!(
            closed.next_attachment_generation,
            published.next_attachment_generation
        );
        assert_eq!(
            closed.presentations[&membership.presentation_id]
                .public_descriptor()
                .surfaces
                .len(),
            1
        );
        let context = test_context();
        assert!(
            store
                .publish_surface(
                    &id,
                    &membership.presentation_id,
                    &retired.creation_request_id,
                    &root,
                    BindingAttachmentCandidate {
                        registered_project_id: None,
                        compiled_binding: context.compiled_binding,
                        presentation: context.presentation,
                        project_authority: None
                    },
                    &limits(16, 8, 8, 128),
                    &policy,
                    |_, _| Ok(()),
                )
                .is_err()
        );
        assert!(
            store
                .prepared_presentation(&id, &request, &root, &"55".repeat(32))
                .is_err()
        );
    }

    #[test]
    fn surface_publication_is_atomic_idempotent_and_session_ordinal() {
        let store = BrowserSessionStore::new();
        let (id, token) = mint(&store, test_shared_context());
        store.activate_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let policy = "22".repeat(32);
        let first = store
            .prepare_presentation(
                &id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(4, 16, 16, 128),
                &policy,
                test_bounds(),
            )
            .unwrap();
        let second = store
            .prepare_presentation(
                &id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(4, 16, 16, 128),
                &policy,
                test_bounds(),
            )
            .unwrap();
        let candidate = || {
            let context = test_context();
            BindingAttachmentCandidate {
                registered_project_id: None,
                compiled_binding: context.compiled_binding,
                presentation: context.presentation,
                project_authority: None,
            }
        };
        let request = uuid::Uuid::new_v4().to_string();
        let generation_before = session.next_attachment_generation;
        assert_eq!(
            store
                .publish_surface_internal(
                    &id,
                    &first.presentation_id,
                    &request,
                    &root,
                    candidate(),
                    &limits(16, 8, 8, 128),
                    &policy,
                    SurfaceSubjectAdmission::ExistingRoot,
                    |surface, attachment| {
                        assert_eq!(surface.ordinal, 1);
                        assert_eq!(surface.coordinate, attachment.coordinate());
                        Err(AttachmentStoreError::Capacity)
                    },
                )
                .unwrap_err(),
            AttachmentStoreError::Capacity
        );
        let rejected = store.get_session(&id).unwrap();
        assert_eq!(rejected.next_attachment_generation, generation_before);
        assert_eq!(rejected.next_surface_ordinal, 1);
        assert_eq!(rejected.attachments.len(), 1);
        assert_eq!(
            rejected.presentations[&first.presentation_id].next_mount_id,
            1
        );
        assert!(
            rejected.presentations[&first.presentation_id]
                .surfaces
                .is_empty()
        );
        assert!(
            store
                .publish_surface(
                    &id,
                    &first.presentation_id,
                    "not-uuid",
                    &root,
                    candidate(),
                    &limits(16, 8, 8, 128),
                    &policy,
                    |_, _| Ok(()),
                )
                .is_err()
        );
        assert_eq!(
            store
                .publish_surface(
                    &id,
                    &first.presentation_id,
                    &request,
                    &root,
                    candidate(),
                    &limits(16, 1, 8, 128),
                    &policy,
                    |_, _| Ok(()),
                )
                .unwrap_err(),
            AttachmentStoreError::Capacity
        );
        let unchanged = store.get_session(&id).unwrap();
        assert_eq!(unchanged.attachments.len(), 1);
        assert!(
            unchanged.presentations[&first.presentation_id]
                .surfaces
                .is_empty()
        );
        assert_eq!(unchanged.next_surface_ordinal, 1);
        let (published, attachment) = store
            .publish_surface(
                &id,
                &first.presentation_id,
                &request,
                &root,
                candidate(),
                &limits(16, 8, 8, 128),
                &policy,
                |_, _| Ok(()),
            )
            .unwrap();
        assert_eq!(published.ordinal, 1);
        assert_eq!(published.mount_manifest.allocation_start, 1);
        assert!(published.mount_manifest.validate().is_ok());
        assert_eq!(
            store.get_session(&id).unwrap().presentations[&first.presentation_id].next_mount_id,
            published.mount_manifest.next_mount_id
        );
        assert_eq!(
            store
                .publish_surface_internal(
                    &id,
                    &first.presentation_id,
                    &request,
                    &root,
                    candidate(),
                    &limits(16, 8, 8, 128),
                    &policy,
                    SurfaceSubjectAdmission::ExistingRoot,
                    |surface, retained| {
                        assert_eq!(surface.surface_instance_id, published.surface_instance_id);
                        assert_eq!(retained.coordinate(), attachment.coordinate());
                        Err(AttachmentStoreError::Capacity)
                    },
                )
                .unwrap_err(),
            AttachmentStoreError::Capacity
        );
        let (retry, retained) = store
            .publish_surface(
                &id,
                &first.presentation_id,
                &request,
                &root,
                candidate(),
                &limits(16, 1, 0, 128),
                &policy,
                |_, _| Ok(()),
            )
            .unwrap();
        assert_eq!(retry, published);
        assert!(Arc::ptr_eq(&retained, &attachment));
        assert_eq!(
            store.get_session(&id).unwrap().presentations[&first.presentation_id].next_mount_id,
            published.mount_manifest.next_mount_id
        );
        let mut different = candidate();
        Arc::make_mut(&mut different.compiled_binding)
            .binding
            .surface = test_binding(
            "surface:ryeos/test/different",
            None,
            crate::compiled_binding::EffectiveUiPosture::Interactive,
        )
        .binding
        .surface
        .clone();
        assert_eq!(
            store
                .publish_surface(
                    &id,
                    &first.presentation_id,
                    &request,
                    &root,
                    different,
                    &limits(16, 8, 8, 128),
                    &policy,
                    |_, _| Ok(()),
                )
                .unwrap_err(),
            AttachmentStoreError::PresentationConflict
        );
        let (sibling, _) = store
            .publish_surface(
                &id,
                &second.presentation_id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                candidate(),
                &limits(16, 8, 8, 128),
                &policy,
                |_, _| Ok(()),
            )
            .unwrap();
        assert_eq!(sibling.ordinal, 2);
        assert_ne!(sibling.surface_instance_id, published.surface_instance_id);
        let mut wrong_principal = candidate();
        Arc::make_mut(&mut wrong_principal.compiled_binding)
            .binding
            .principal_id = "fp:foreign".into();
        assert_eq!(
            store
                .publish_surface(
                    &id,
                    &first.presentation_id,
                    &uuid::Uuid::new_v4().to_string(),
                    &root,
                    wrong_principal,
                    &limits(16, 8, 8, 128),
                    &policy,
                    |_, _| Ok(()),
                )
                .unwrap_err(),
            AttachmentStoreError::PrincipalMismatch
        );
        let mut wrong_project = candidate();
        Arc::make_mut(&mut wrong_project.compiled_binding)
            .binding
            .project_root = Some("/foreign".into());
        assert_eq!(
            store
                .publish_surface(
                    &id,
                    &first.presentation_id,
                    &uuid::Uuid::new_v4().to_string(),
                    &root,
                    wrong_project,
                    &limits(16, 8, 8, 128),
                    &policy,
                    |_, _| Ok(()),
                )
                .unwrap_err(),
            AttachmentStoreError::PresentationConflict
        );
        assert_eq!(store.get_session(&id).unwrap().attachments.len(), 3);
    }

    #[test]
    fn presentation_membership_fences_retry_capacity_seat_and_revocation() {
        let store = BrowserSessionStore::new();
        let (id, token) = mint(&store, test_shared_context());
        store.activate_launch_token(&token).unwrap();
        let session = store.get_session(&id).unwrap();
        let coordinate = session.attachments[&session.root_attachment_id].coordinate();
        let request = uuid::Uuid::new_v4().to_string();
        let (surface_session, surface_token) = mint(&store, test_context());
        store.activate_launch_token(&surface_token).unwrap();
        let surface = store.get_session(&surface_session).unwrap();
        let surface_coordinate = surface.attachments[&surface.root_attachment_id].coordinate();
        assert_eq!(
            store
                .prepare_presentation(
                    &surface_session,
                    &request,
                    &surface_coordinate,
                    &limits(0, 16, 16, 128),
                    &"22".repeat(32),
                    test_bounds()
                )
                .unwrap_err(),
            AttachmentStoreError::PresentationInvalid
        );
        let membership = store
            .prepare_presentation(
                &id,
                &request,
                &coordinate,
                &limits(1, 16, 16, 128),
                &"22".repeat(32),
                test_bounds(),
            )
            .unwrap();
        assert!(membership.thread_id.is_none() && membership.surfaces.is_empty());
        assert_eq!(
            store
                .prepare_presentation(
                    &id,
                    &request,
                    &coordinate,
                    &limits(1, 16, 16, 128),
                    &"22".repeat(32),
                    test_bounds()
                )
                .unwrap(),
            membership
        );
        assert_eq!(
            store
                .prepare_presentation(
                    &id,
                    &uuid::Uuid::new_v4().to_string(),
                    &coordinate,
                    &limits(1, 16, 16, 128),
                    &"22".repeat(32),
                    test_bounds()
                )
                .unwrap_err(),
            AttachmentStoreError::Capacity
        );
        assert_eq!(
            store
                .prepare_presentation(
                    &id,
                    "invalid",
                    &coordinate,
                    &limits(1, 16, 16, 128),
                    &"22".repeat(32),
                    test_bounds()
                )
                .unwrap_err(),
            AttachmentStoreError::PresentationInvalid
        );
        assert_eq!(
            store
                .prepare_presentation(
                    &id,
                    &request,
                    &coordinate,
                    &limits(1, 16, 16, 128),
                    &"44".repeat(32),
                    test_bounds()
                )
                .unwrap_err(),
            AttachmentStoreError::PolicyStale
        );
        let second = store
            .publish_attachment(
                &id,
                &coordinate,
                BindingAttachmentCandidate {
                    registered_project_id: None,
                    compiled_binding: test_shared_context().compiled_binding,
                    presentation: test_shared_context().presentation,
                    project_authority: None,
                },
                4,
                &"22".repeat(32),
            )
            .unwrap();
        assert_eq!(
            store
                .prepare_presentation(
                    &id,
                    &request,
                    &second.coordinate(),
                    &limits(1, 16, 16, 128),
                    &"22".repeat(32),
                    test_bounds()
                )
                .unwrap_err(),
            AttachmentStoreError::PresentationConflict
        );
        let bound = store
            .bind_presentation_seat(
                &id,
                &membership.presentation_id,
                &coordinate,
                "seat-one",
                "chain-one",
                &"22".repeat(32),
            )
            .unwrap();
        assert_eq!(
            store
                .bind_presentation_seat(
                    &id,
                    &membership.presentation_id,
                    &coordinate,
                    "seat-one",
                    "chain-one",
                    &"44".repeat(32)
                )
                .unwrap_err(),
            AttachmentStoreError::PolicyStale
        );
        assert_eq!(
            store
                .require_presentation(
                    &id,
                    &membership.presentation_id,
                    None,
                    None,
                    &"44".repeat(32)
                )
                .unwrap_err(),
            AttachmentStoreError::PolicyStale
        );
        assert_eq!(
            store
                .bind_presentation_seat(
                    &id,
                    &membership.presentation_id,
                    &coordinate,
                    "seat-one",
                    "chain-one",
                    &"22".repeat(32)
                )
                .unwrap(),
            bound
        );
        assert_eq!(
            store
                .bind_presentation_seat(
                    &id,
                    &membership.presentation_id,
                    &coordinate,
                    "seat-two",
                    "chain-one",
                    &"22".repeat(32)
                )
                .unwrap_err(),
            AttachmentStoreError::PresentationConflict
        );
        assert_eq!(
            store
                .require_presentation(
                    &id,
                    &membership.presentation_id,
                    Some("wrong-seat"),
                    None,
                    &"22".repeat(32)
                )
                .unwrap_err(),
            AttachmentStoreError::PresentationConflict
        );
        let (other, other_token) = mint(&store, test_context());
        store.activate_launch_token(&other_token).unwrap();
        assert_eq!(
            store
                .require_presentation(
                    &other,
                    &membership.presentation_id,
                    None,
                    None,
                    &"22".repeat(32)
                )
                .unwrap_err(),
            AttachmentStoreError::PresentationInvalid
        );
        assert_eq!(store.presentation_memberships(&id).unwrap(), vec![bound]);
        store.revoke_attachment(&id, &coordinate).unwrap();
        assert_eq!(
            store
                .require_presentation(
                    &id,
                    &membership.presentation_id,
                    None,
                    None,
                    &"22".repeat(32)
                )
                .unwrap_err(),
            AttachmentStoreError::AttachmentStale
        );
    }

    #[test]
    fn transfer_preparation_rechecks_two_memberships_under_one_session_lock() {
        let store = BrowserSessionStore::new();
        let (session_id, token) = mint(&store, test_shared_context());
        store.activate_launch_token(&token).unwrap();
        let session = store.get_session(&session_id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let policy = "22".repeat(32);
        let source = store
            .prepare_presentation(
                &session_id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(2, 16, 16, 128),
                &policy,
                test_bounds(),
            )
            .unwrap();
        let destination = store
            .prepare_presentation(
                &session_id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(2, 16, 16, 128),
                &policy,
                test_bounds(),
            )
            .unwrap();
        let generation = store
            .get_session(&session_id)
            .unwrap()
            .next_attachment_generation;
        assert!(store.matches_transfer_preparation_memberships(
            &session_id,
            &source,
            &destination,
            generation,
            &policy,
        ));
        assert!(!store.matches_transfer_preparation_memberships(
            &session_id,
            &source,
            &source,
            generation,
            &policy,
        ));
        assert!(!store.matches_transfer_preparation_memberships(
            &session_id,
            &source,
            &destination,
            generation + 1,
            &policy,
        ));
        assert!(!store.matches_transfer_preparation_memberships(
            &session_id,
            &source,
            &destination,
            generation,
            &"44".repeat(32),
        ));
        store
            .bind_presentation_seat(
                &session_id,
                &source.presentation_id,
                &root,
                "source-seat",
                "source-seat",
                &policy,
            )
            .unwrap();
        assert_eq!(
            store
                .get_session(&session_id)
                .unwrap()
                .next_attachment_generation,
            generation
        );
        assert!(!store.matches_transfer_preparation_memberships(
            &session_id,
            &source,
            &destination,
            generation,
            &policy,
        ));
        store.revoke_attachment(&session_id, &root).unwrap();
        assert!(!store.matches_transfer_preparation_memberships(
            &session_id,
            &source,
            &destination,
            generation,
            &policy,
        ));
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
            .get(&session.root_attachment_id)
            .expect("launch attachment");
        assert_eq!(attachment.root_ref, "surface:ryeos/ui/base");
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
            .get(&first.root_attachment_id)
            .expect("surface")
            .coordinate();
        assert!(matches!(
            store.attachment_for_detach(&first_id, &surface),
            Err(AttachmentStoreError::RootAttachmentImmutable)
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
                    presentation: test_presentation(),
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
            presentation: test_presentation(),
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
            .get(&session.root_attachment_id)
            .expect("launch attachment");
        assert_eq!(attachment.root_ref, "surface:ryeos/test/ro");
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
            .get(&session.root_attachment_id)
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
    fn shared_config_revocation_fences_dispatch_and_seat_without_revoking_admitted_owner() {
        let store = BrowserSessionStore::new();
        let (session_id, token) = mint(&store, test_shared_context());
        store.consume_launch_token(&token).unwrap();
        let session = store.get_session(&session_id).unwrap();
        let root = session.attachments[&session.root_attachment_id].coordinate();
        let policy = "22".repeat(32);
        let member = store
            .prepare_presentation(
                &session_id,
                &uuid::Uuid::new_v4().to_string(),
                &root,
                &limits(4, 16, 16, 128),
                &policy,
                test_bounds(),
            )
            .unwrap();
        store
            .bind_presentation_seat(
                &session_id,
                &member.presentation_id,
                &root,
                "shared-seat",
                "shared-seat",
                &policy,
            )
            .unwrap();
        let admitted = store.admit_attachment_dispatch(&session_id, &root).unwrap();
        assert!(matches!(
            &admitted.attachment().presentation.definition,
            ryeos_client_base::presentation::UiCompositionDefinition::SharedPresentation { .. }
        ));
        assert!(
            store
                .require_presentation(
                    &session_id,
                    &member.presentation_id,
                    Some("shared-seat"),
                    Some("shared-seat"),
                    &policy,
                )
                .is_ok()
        );
        // Operator detach does not grant root revocation. Exercise the internal
        // invalidation owner directly, not an invented browser capability.
        assert!(matches!(
            store.attachment_for_detach(&session_id, &root),
            Err(AttachmentStoreError::RootAttachmentImmutable)
        ));
        store.revoke_attachment(&session_id, &root).unwrap();
        assert!(matches!(
            store.admit_attachment_dispatch(&session_id, &root),
            Err(AttachmentStoreError::AttachmentStale)
        ));
        assert!(matches!(
            store.require_presentation(
                &session_id,
                &member.presentation_id,
                Some("shared-seat"),
                Some("shared-seat"),
                &policy,
            ),
            Err(AttachmentStoreError::AttachmentStale)
        ));
        assert!(
            store
                .prepare_presentation(
                    &session_id,
                    &uuid::Uuid::new_v4().to_string(),
                    &root,
                    &limits(4, 16, 16, 128),
                    &policy,
                    test_bounds(),
                )
                .is_err()
        );
        // Invalidation blocks future admission; an already admitted execution
        // retains its immutable owner rather than being cancelled by UI expiry.
        assert_eq!(admitted.attachment().coordinate(), root);
        assert_eq!(
            store.get_session(&session_id).unwrap().presentations.len(),
            1
        );
    }

    #[test]
    fn publish_rechecks_capacity_principal_and_policy() {
        let store = BrowserSessionStore::new();
        let (session_id, token) = mint(&store, test_context());
        store.consume_launch_token(&token).expect("activate");
        let project = tempfile::tempdir().unwrap();
        let authority = Arc::new(
            lillux::PinnedDirectory::open(project.path())
                .unwrap()
                .unwrap(),
        );
        let project_path = project.path().to_str().unwrap();
        let candidate = BindingAttachmentCandidate {
            registered_project_id: Some("project-a".into()),
            compiled_binding: test_binding(
                "surface:ryeos/ui/base",
                Some(project_path),
                crate::compiled_binding::EffectiveUiPosture::Interactive,
            ),
            presentation: test_presentation(),
            project_authority: Some(authority),
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
        let project = tempfile::tempdir().unwrap();
        context.project_authority = Some(Arc::new(
            lillux::PinnedDirectory::open(project.path())
                .unwrap()
                .unwrap(),
        ));
        Arc::make_mut(&mut context.compiled_binding)
            .binding
            .project_root = Some(project.path().display().to_string());
        context.registered_project_id = Some("project-pending".into());
        let (session_id, token) = mint(&store, context);

        assert!(store.has_retained_attachment_for_project("fp:test", "project-pending"));
        store.consume_launch_token(&token).expect("activate");
        let session = store.get_session(&session_id).expect("active session");
        let coordinate = session
            .attachments
            .get(&session.root_attachment_id)
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
