//! Seat caller authentication for ryeos-ui services.
//!
//! Signed UI sources are invoked either by a signed client or through an exact
//! entry in a daemon-compiled browser-session binding. A browser cookie is
//! transport identity only: it cannot call source services directly and
//! thereby bypass the compiled source coordinate and parameter contract.

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;

use crate::browser_session::AdmittedBindingAttachment;
use crate::state::get_ui_state;

/// An exact project traversal coordinate whose descriptor owner remains alive
/// for at least as long as the pathname.  The path is intentionally not
/// exposed without this owner: `/proc/self/fd/N` becomes ambient and unsafe as
/// soon as the directory descriptor is dropped and the number can be reused.
#[derive(Debug, Clone)]
pub struct RetainedProjectAccess {
    authority: std::sync::Arc<lillux::PinnedDirectory>,
    descriptor_path: std::path::PathBuf,
}

impl RetainedProjectAccess {
    pub fn path(&self) -> &std::path::Path {
        &self.descriptor_path
    }

    pub fn try_clone_directory(&self) -> anyhow::Result<lillux::PinnedDirectory> {
        self.authority.try_clone()
    }

    /// The retained directory as project-content authority: every project
    /// read through it traverses from the held descriptor.
    pub fn content_authority(
        &self,
    ) -> &dyn ryeos_engine::project_content::AuthoritativeProjectContent {
        self.authority.as_ref()
    }
}

/// Resolve `item_ref` over `roots`. When the roots' project space is this
/// retained access's descriptor path, project candidates are judged by the
/// retained directory owner: the descriptor pathname is a magic link that
/// no-follow pathname traversal refuses, and it is never existence authority.
/// Without retained access, resolution walks the roots' pathnames.
/// At most `max_bytes` of the item source at `path`, with the source's full
/// length. A source beneath `access` is read through its retained directory
/// owner, never by reopening the descriptor pathname; any other source, such
/// as a bundle item, is read by its own pathname.
pub(crate) fn read_item_source(
    path: &std::path::Path,
    access: Option<&RetainedProjectAccess>,
    max_bytes: usize,
) -> Option<(Vec<u8>, u64)> {
    if let Some(access) = access
        && let Ok(relative) = path.strip_prefix(&access.descriptor_path)
    {
        let file = access
            .authority
            .open_pinned_regular_descendant(relative, false)
            .ok()??;
        let size = file.size().ok()?;
        return Some((file.read_prefix(max_bytes).ok()?, size));
    }
    let file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(file, max_bytes as u64), &mut bytes)
        .ok()?;
    Some((bytes, size))
}

/// The whole UTF-8 item source at `path`, read as [`read_item_source`] reads
/// it. A source over the engine's item-source bound reads as absent.
pub(crate) fn read_whole_item_source(
    path: &std::path::Path,
    access: Option<&RetainedProjectAccess>,
) -> Option<String> {
    let limit = ryeos_engine::item_resolution::MAX_ITEM_SOURCE_BYTES;
    let (bytes, size) = read_item_source(path, access, limit as usize)?;
    if size > limit || bytes.len() as u64 != size {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Enumerate `kind`'s refs over `roots`, with project-space candidates listed
/// by the retained directory owner exactly as [`resolve_item_for_access`]
/// judges them. An unreadable corpus enumerates nothing, as for
/// [`ryeos_engine::item_resolution::enumerate_kind_refs`].
pub(crate) fn enumerate_kind_refs_for_access(
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    schema: &ryeos_engine::kind_registry::KindSchema,
    kind: &str,
    access: Option<&RetainedProjectAccess>,
) -> Vec<ryeos_engine::canonical_ref::CanonicalRef> {
    match access {
        Some(access) => ryeos_engine::item_resolution::enumerate_kind_refs_under_project_authority(
            roots,
            schema,
            kind,
            &access.descriptor_path,
            access.authority.as_ref(),
        )
        .unwrap_or_default(),
        None => ryeos_engine::item_resolution::enumerate_kind_refs(roots, schema, kind),
    }
}

pub(crate) fn resolve_item_for_access(
    roots: &ryeos_engine::item_resolution::ResolutionRoots,
    schema: &ryeos_engine::kind_registry::KindSchema,
    item_ref: &ryeos_engine::canonical_ref::CanonicalRef,
    access: Option<&RetainedProjectAccess>,
) -> Result<ryeos_engine::item_resolution::ResolutionResult, ryeos_engine::error::EngineError> {
    match access {
        Some(access) => ryeos_engine::item_resolution::resolve_item_full_under_project_authority(
            roots,
            schema,
            item_ref,
            &access.descriptor_path,
            access.authority.as_ref(),
        ),
        None => ryeos_engine::item_resolution::resolve_item_full(roots, schema, item_ref),
    }
}

fn validate_attachment_project(
    attachment: &AdmittedBindingAttachment,
) -> Result<Option<&std::sync::Arc<lillux::PinnedDirectory>>, HandlerError> {
    // The attachment's query identity is a copy derived from the compiled
    // binding at admission; the compiled binding's own project root stays
    // the identity the directory authority must answer to.
    let compiled_root = attachment.compiled_binding.binding.project_root.as_deref();
    if attachment.project_query_identity.as_deref() != compiled_root {
        return Err(HandlerError::Forbidden(
            "project authority contradicts the compiled project identity".into(),
        ));
    }
    match (attachment.project_authority.as_ref(), compiled_root) {
        (None, None) => Ok(None),
        (Some(authority), Some(project_root)) => {
            authority
                .ensure_path_binding()
                .map_err(|_| HandlerError::Forbidden("project authority changed".into()))?;
            if authority.path() != std::path::Path::new(project_root) {
                return Err(HandlerError::Forbidden(
                    "project authority contradicts the compiled project identity".into(),
                ));
            }
            Ok(Some(authority))
        }
        _ => Err(HandlerError::Forbidden(
            "project authority contradicts the compiled project identity".into(),
        )),
    }
}

pub(crate) fn attachment_project_access(
    attachment: &AdmittedBindingAttachment,
) -> Result<Option<RetainedProjectAccess>, HandlerError> {
    validate_attachment_project(attachment)?
        .map(|authority| {
            Ok(RetainedProjectAccess {
                authority: authority.clone(),
                descriptor_path: authority
                    .descriptor_path()
                    .map_err(|error| HandlerError::Internal(error.to_string()))?,
            })
        })
        .transpose()
}

pub(crate) fn attachment_project_query_identity(
    attachment: &AdmittedBindingAttachment,
) -> Result<Option<std::path::PathBuf>, HandlerError> {
    Ok(validate_attachment_project(attachment)?.map(|authority| authority.path().to_path_buf()))
}

tokio::task_local! {
    /// Daemon-retained authority for one compiled UI dispatch. This sideband
    /// is scoped by `ui.invocations.dispatch`; it is neither serialized into
    /// a request nor reconstructible from a browser cookie.
    static COMPILED_UI_ATTACHMENT: std::sync::Arc<AdmittedBindingAttachment>;
}

pub async fn with_compiled_ui_attachment<F>(
    attachment: std::sync::Arc<AdmittedBindingAttachment>,
    future: F,
) -> F::Output
where
    F: std::future::Future,
{
    COMPILED_UI_ATTACHMENT.scope(attachment, future).await
}

pub fn compiled_ui_attachment() -> Option<std::sync::Arc<AdmittedBindingAttachment>> {
    COMPILED_UI_ATTACHMENT.try_with(Clone::clone).ok()
}

pub enum SeatCaller {
    Attachment(std::sync::Arc<AdmittedBindingAttachment>),
    Operator { principal_id: String },
}

impl SeatCaller {
    /// Durable principal already authenticated by signed execution or retained
    /// in the compiled UI binding. Read projections use this as their owner
    /// filter; browser-session transport never broadens it to node-wide data.
    pub fn principal_id(&self) -> &str {
        match self {
            Self::Attachment(attachment) => &attachment.compiled_binding.binding.principal_id,
            Self::Operator { principal_id } => principal_id,
        }
    }

    /// Exact descriptor-rooted project access with its owner retained.  This
    /// is for filesystem resolution only, never projection filtering.
    pub fn project_access(&self) -> Result<Option<RetainedProjectAccess>, HandlerError> {
        let Self::Attachment(attachment) = self else {
            return Ok(None);
        };
        attachment_project_access(attachment)
    }

    /// Stable validated identity used by thread and field projections.  This
    /// pathname is not filesystem authority and must never be reopened to
    /// grant access.
    pub fn project_query_identity(&self) -> Result<Option<std::path::PathBuf>, HandlerError> {
        let Self::Attachment(attachment) = self else {
            return Ok(None);
        };
        attachment_project_query_identity(attachment)
    }
}

/// Require the verified principal restored by signed execution or compiled
/// binding dispatch. Raw browser-session principals are deliberately refused.
pub fn require_seat_caller(
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<SeatCaller, HandlerError> {
    if let Some(attachment) = compiled_ui_attachment() {
        return Ok(SeatCaller::Attachment(attachment));
    }
    if let Some(session_id) = ctx.fingerprint.strip_prefix("session:") {
        let valid = get_ui_state(state)
            .ok_or_else(|| HandlerError::Internal("UiState not set".into()))?
            .browser_sessions
            .get_session(session_id)
            .is_some();
        return Err(HandlerError::Forbidden(if valid {
            "browser data access requires a compiled binding coordinate".into()
        } else {
            "session expired or invalid".into()
        }));
    }
    let fingerprint = ryeos_app::operator_authority::require_admitted_operator(state, ctx)
        .map_err(|_| HandlerError::Forbidden("admitted operator required".into()))?;
    // Admission answers with the bare key fingerprint; durable owners, thread
    // subjects and compiled bindings all name principals in `fp:` form.
    Ok(SeatCaller::Operator {
        principal_id: format!("fp:{fingerprint}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ryeos_api::surface_views::EffectiveUiItemIdentity;
    use ryeos_engine::resolution::{EffectiveDefinitionDigest, TrustClass};

    fn attachment() -> Arc<AdmittedBindingAttachment> {
        Arc::new(AdmittedBindingAttachment {
            binding_attachment_id: "attachment-sideband".into(),
            binding_generation: 1,
            registered_project_id: None,
            compiled_binding: Arc::new(crate::compiled_binding::SessionCompiledUiBinding {
                binding_digest: "11".repeat(32),
                posture: crate::compiled_binding::EffectiveUiPosture::ObservationOnly,
                binding: crate::compiled_binding::CompiledUiBinding {
                    contract_revision: crate::UI_BINDING_CONTRACT_REVISION.into(),
                    principal_id: "fp:test".into(),
                    project_root: None,
                    request_engine_generation_identity: "generation:test".into(),
                    node_policy_generation_digest: "22".repeat(32),
                    surface: EffectiveUiItemIdentity {
                        canonical_ref: "surface:ryeos/ui/base-observe".into(),
                        effective_definition_digest: EffectiveDefinitionDigest::parse(
                            "33".repeat(32),
                        )
                        .expect("fixture digest"),
                        effective_trust_class: TrustClass::TrustedBundle,
                    },
                    views: Default::default(),
                    sources: Default::default(),
                    affordances: Default::default(),
                    surface_route: None,
                    attenuated: Vec::new(),
                },
            }),
            effective_surface: serde_json::json!({}),
            surface_ref: "surface:ryeos/ui/base-observe".into(),
            project_query_identity: None,
            project_authority: None,
        })
    }

    fn attachment_with_project(path: &std::path::Path) -> Arc<AdmittedBindingAttachment> {
        let canonical = path.canonicalize().expect("canonical fixture project");
        let mut attachment = attachment();
        Arc::get_mut(&mut attachment)
            .expect("fixture owns attachment")
            .project_query_identity = Some(canonical.display().to_string());
        Arc::get_mut(
            &mut Arc::get_mut(&mut attachment)
                .expect("fixture owns attachment")
                .compiled_binding,
        )
        .expect("fixture owns compiled binding")
        .binding
        .project_root = Some(canonical.display().to_string());
        Arc::get_mut(&mut attachment)
            .expect("fixture owns attachment")
            .project_authority = Some(Arc::new(
            lillux::PinnedDirectory::open(&canonical)
                .expect("open fixture project")
                .expect("fixture project exists"),
        ));
        attachment
    }

    #[tokio::test]
    async fn compiled_attachment_authority_exists_only_inside_dispatch_scope() {
        assert!(compiled_ui_attachment().is_none());
        with_compiled_ui_attachment(attachment(), async {
            let retained = compiled_ui_attachment().expect("retained dispatch authority");
            assert_eq!(retained.binding_attachment_id, "attachment-sideband");
        })
        .await;
        assert!(compiled_ui_attachment().is_none());
    }

    #[tokio::test]
    async fn retained_project_access_owns_descriptor_across_yield_and_fd_churn() {
        let project = tempfile::tempdir().expect("project tempdir");
        std::fs::write(project.path().join("marker"), b"retained").expect("write project marker");
        let caller = SeatCaller::Attachment(attachment_with_project(project.path()));
        let access = caller
            .project_access()
            .expect("project access")
            .expect("bound project");
        drop(caller);

        tokio::task::yield_now().await;
        let churn = (0..256)
            .map(|_| std::fs::File::open("/dev/null").expect("open churn descriptor"))
            .collect::<Vec<_>>();
        assert_eq!(
            std::fs::read(access.path().join("marker")).expect("read through retained descriptor"),
            b"retained"
        );
        drop(churn);
    }

    #[test]
    fn query_identity_is_stable_across_independent_descriptors() {
        let project = tempfile::tempdir().expect("project tempdir");
        let first = SeatCaller::Attachment(attachment_with_project(project.path()));
        let second = SeatCaller::Attachment(attachment_with_project(project.path()));

        assert_ne!(
            first
                .project_access()
                .expect("first access")
                .expect("first project")
                .path(),
            second
                .project_access()
                .expect("second access")
                .expect("second project")
                .path()
        );
        assert_eq!(
            first.project_query_identity().expect("first identity"),
            second.project_query_identity().expect("second identity")
        );
    }

    #[test]
    fn replacement_is_refused_without_invalidating_already_retained_access() {
        let parent = tempfile::tempdir().expect("parent tempdir");
        let project = parent.path().join("project");
        let moved = parent.path().join("moved");
        std::fs::create_dir(&project).expect("create project");
        std::fs::write(project.join("marker"), b"original").expect("write marker");
        let caller = SeatCaller::Attachment(attachment_with_project(&project));
        let access = caller
            .project_access()
            .expect("initial access")
            .expect("bound project");

        std::fs::rename(&project, &moved).expect("move original project");
        std::fs::create_dir(&project).expect("create replacement project");
        std::fs::write(project.join("marker"), b"replacement").expect("write replacement marker");

        assert!(caller.project_access().is_err());
        assert!(caller.project_query_identity().is_err());
        assert_eq!(
            std::fs::read(access.path().join("marker")).expect("read pinned original"),
            b"original"
        );
    }

    #[test]
    fn compiled_identity_cannot_be_paired_with_a_different_directory_authority() {
        let project = tempfile::tempdir().expect("project tempdir");
        let other = tempfile::tempdir().expect("other tempdir");
        let mut attachment = attachment_with_project(project.path());
        Arc::get_mut(
            &mut Arc::get_mut(&mut attachment)
                .expect("fixture owns attachment")
                .compiled_binding,
        )
        .expect("fixture owns compiled binding")
        .binding
        .project_root = Some(
            other
                .path()
                .canonicalize()
                .expect("canonical other project")
                .display()
                .to_string(),
        );
        let caller = SeatCaller::Attachment(attachment);

        assert!(caller.project_access().is_err());
        assert!(caller.project_query_identity().is_err());
    }
}
