//! Exact-thread authorization shared by RyeOS UI read services.
//!
//! Seat authentication establishes who is reading. It does not prove that the
//! reader may observe an arbitrary thread ID. This module reads the signed
//! thread subject first and judges it with `ryeos_app::seat_authority` before
//! any result, event, artifact, child, capsule, or definition evidence is
//! loaded. The judgement is by key and principal only; this crate builds the
//! reader from the seat and makes no ownership call of its own.

use std::collections::BTreeSet;

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::seat_authority::{SeatProjectState, ThreadReader};
use ryeos_app::state::AppState;
use ryeos_app::state_store::{AuthoritativeThreadSubject, ExecutionTreeEdgeCandidate};
use ryeos_app::thread_lifecycle::ExecutionTreeResult;

use crate::seat_auth::SeatCaller;

/// The reader a seat reads threads as: its key, the project its binding
/// sealed at mint, and whether the read comes from a project-authored source.
/// A seat whose project authority no longer validates reads nothing.
fn thread_reader(caller: &SeatCaller) -> Result<ThreadReader, HandlerError> {
    Ok(match caller {
        SeatCaller::Attachment(attachment) => {
            let bound_to_checkout = crate::seat_auth::attachment_project_query_identity(attachment)
                .map_err(|_| HandlerError::NotFound)?
                .is_some();
            let binding = &attachment.compiled_binding.binding;
            ThreadReader {
                key: binding.principal_id.clone(),
                seat_project: SeatProjectState::from_sealed(
                    bound_to_checkout,
                    &binding.signer_project,
                ),
                project_authored_source: crate::seat_auth::compiled_ui_source_is_project_authored(),
            }
        }
        SeatCaller::Operator { principal_id } => ThreadReader {
            key: principal_id.clone(),
            seat_project: SeatProjectState::Projectless,
            project_authored_source: false,
        },
    })
}

/// Authorize one or two exact thread subjects as one indistinguishable read.
/// Missing and unauthorized subjects both return `NotFound`.
pub(crate) fn authorize_exact_thread_subjects(
    ctx: &HandlerContext,
    state: &AppState,
    caller: &SeatCaller,
    thread_ids: &[&str],
) -> Result<Vec<AuthoritativeThreadSubject>, HandlerError> {
    if ctx.authenticated_origin_site_id.is_some() {
        return Err(HandlerError::NotFound);
    }
    authorize_exact_thread_subjects_for(state, caller, thread_ids)
}

/// The subject check of [`authorize_exact_thread_subjects`] for a caller that
/// has no handler context, such as an event stream opened by a browser
/// session. The caller names the seat; the answer is the same signed-subject
/// judgement, with missing and unauthorized subjects both `NotFound`.
pub(crate) fn authorize_exact_thread_subjects_for(
    state: &AppState,
    caller: &SeatCaller,
    thread_ids: &[&str],
) -> Result<Vec<AuthoritativeThreadSubject>, HandlerError> {
    let reader = thread_reader(caller)?;
    ryeos_app::seat_authority::authorize_thread_subjects(state, &reader, thread_ids)
}

/// The signed subject of one thread, `None` when no such thread exists.
/// This is the read half of [`authorize_exact_thread_subjects_for`] for a
/// caller that judges the same subject for many seats.
pub(crate) fn load_thread_subject(
    state: &AppState,
    thread_id: &str,
) -> Result<Option<AuthoritativeThreadSubject>, HandlerError> {
    Ok(
        ryeos_app::seat_authority::load_thread_subjects(state, &[thread_id])?
            .pop()
            .flatten(),
    )
}

/// What one browser session may observe: the union of what each of its
/// attachments authorizes as a seat, by the same signed-subject judgement as
/// [`authorize_exact_thread_subjects_for`]. An attachment whose project
/// authority cannot be validated authorizes nothing, and a session with no
/// attachment observes nothing. The ownership check runs on every
/// observation, so a key retired while the session is open stops observing.
pub(crate) struct SessionObserver {
    readers: Vec<ThreadReader>,
}

impl SessionObserver {
    pub(crate) fn new(
        attachments: &[std::sync::Arc<crate::browser_session::AdmittedBindingAttachment>],
    ) -> Self {
        Self {
            readers: attachments
                .iter()
                .filter_map(|attachment| {
                    thread_reader(&SeatCaller::Attachment(attachment.clone())).ok()
                })
                .collect(),
        }
    }

    pub(crate) fn observes(&self, state: &AppState, subject: &AuthoritativeThreadSubject) -> bool {
        let trust = &state.engine.trust_store;
        self.readers
            .iter()
            .any(|reader| reader.admits(trust, subject))
    }
}

/// A selected-thread check does not authorize the ancestor/sibling/descendant
/// closure returned by `execution_tree`. Validate every bounded structural
/// member against its signed subject before any decorated tree or evidence is
/// returned. The state reader accepts at most two exact IDs per invocation.
pub(crate) fn authorize_execution_tree_subjects(
    ctx: &HandlerContext,
    state: &AppState,
    caller: &SeatCaller,
    tree: &mut ExecutionTreeResult,
    selected_thread_id: &str,
    max_depth: usize,
    max_nodes: usize,
) -> Result<BTreeSet<String>, HandlerError> {
    let mut ids = BTreeSet::new();
    let mut roots = std::collections::BTreeMap::new();
    for row in &tree.threads {
        let id = row.thread.item.thread_id.clone();
        if let Some(previous) = roots.insert(id.clone(), row.thread.item.chain_root_id.clone())
            && previous != row.thread.item.chain_root_id
        {
            return Err(HandlerError::NotFound);
        }
        ids.insert(id);
    }
    if !ids.contains(selected_thread_id)
        || tree
            .root_thread_id
            .as_deref()
            .is_none_or(|root| !ids.contains(root))
        || tree.threads.iter().any(|row| {
            row.tree
                .parent_thread_id
                .as_ref()
                .is_some_and(|parent| !ids.contains(parent))
        })
    {
        return Err(HandlerError::NotFound);
    }
    let ids_vec = ids.iter().map(String::as_str).collect::<Vec<_>>();
    for pair in ids_vec.chunks(2) {
        let subjects = authorize_exact_thread_subjects(ctx, state, caller, pair)?;
        for subject in subjects {
            if roots.get(&subject.thread_id) != Some(&subject.chain_root_id) {
                return Err(HandlerError::NotFound);
            }
            for row in tree
                .threads
                .iter()
                .filter(|row| row.thread.item.thread_id == subject.thread_id)
            {
                if !projected_thread_matches_signed_subject(
                    &row.thread.item.kind,
                    &row.thread.item.item_ref,
                    &row.thread.item.launch_mode,
                    &row.thread.item.created_at,
                    &row.thread.item.updated_at,
                    &row.thread.item.status,
                    &row.thread.item.admitted_launch_capsule_hash,
                    &subject,
                ) {
                    return Err(HandlerError::NotFound);
                }
            }
        }
    }
    let mut unique_edges = BTreeSet::new();
    for row in &tree.threads {
        let Some(parent_id) = row.tree.parent_thread_id.as_ref() else {
            continue;
        };
        let Some(parent_root) = roots.get(parent_id) else {
            return Err(HandlerError::NotFound);
        };
        unique_edges.insert((
            parent_id.clone(),
            parent_root.clone(),
            row.thread.item.thread_id.clone(),
            row.thread.item.chain_root_id.clone(),
            row.tree.relation.clone(),
        ));
    }
    let edges = unique_edges
        .into_iter()
        .map(
            |(
                parent_thread_id,
                parent_chain_root_id,
                child_thread_id,
                child_chain_root_id,
                relation,
            )| {
                ExecutionTreeEdgeCandidate {
                    parent_thread_id,
                    parent_chain_root_id,
                    child_thread_id,
                    child_chain_root_id,
                    relation,
                }
            },
        )
        .collect::<Vec<_>>();
    state
        .state_store
        .verify_execution_tree_edges(&edges)
        .map_err(|error| {
            tracing::error!(error = %format!("{error:#}"), "signed execution tree edge proof failed");
            HandlerError::NotFound
        })?;
    // The decorated facet map is a rebuildable projection of facet events,
    // not the snapshot's terminal-cost facet map. Until the event-derived map
    // has its own signed proof, do not publish it as thread evidence.
    mask_unverified_tree_facets(tree);
    mask_unverified_tree_edges(tree, max_depth, max_nodes);
    Ok(ids)
}

pub(crate) fn mask_unverified_tree_facets(tree: &mut ExecutionTreeResult) {
    for row in &mut tree.threads {
        row.thread.facets.clear();
    }
}

fn projected_thread_matches_signed_subject(
    kind: &str,
    item_ref: &str,
    launch_mode: &str,
    created_at: &str,
    updated_at: &str,
    status: &str,
    admitted_launch_capsule_hash: &Option<String>,
    subject: &AuthoritativeThreadSubject,
) -> bool {
    kind == subject.kind
        && item_ref == subject.item_ref
        && launch_mode == subject.launch_mode
        && created_at == subject.created_at
        && updated_at == subject.updated_at
        && status == subject.status.as_str()
        && admitted_launch_capsule_hash == &subject.admitted_launch_capsule_hash
}

/// SQL's exact has-more/has-children flags may describe an uninspected
/// foreign branch. This pure projection publishes only edges already present
/// in the signed-authorized bounded page; it is not a negative proof.
pub(crate) fn mask_unverified_tree_edges(
    tree: &mut ExecutionTreeResult,
    max_depth: usize,
    max_nodes: usize,
) {
    // Publish only edges already present in this signed-authorized bounded
    // page. A saturated request is incomplete even when the next unobserved
    // node is unknown; never erase an existing truncation signal.
    let visible_parents = tree
        .threads
        .iter()
        .filter_map(|row| row.tree.parent_thread_id.clone())
        .collect::<BTreeSet<_>>();
    tree.truncated |= tree.threads.len() >= max_nodes
        || tree.threads.iter().any(|row| row.tree.depth >= max_depth);
    for row in &mut tree.threads {
        row.tree.has_children = visible_parents.contains(&row.thread.item.thread_id);
    }
}

#[cfg(test)]
mod tests {
    use ryeos_state::objects::{
        ChildProjectAuthorityPolicy, EnvironmentAuthority, ExecutionProjectAuthority,
        PinnedProjectRealization, ThreadStatus,
    };

    use super::*;

    fn subject(
        requested_by: Option<&str>,
        project_authority: ExecutionProjectAuthority,
    ) -> AuthoritativeThreadSubject {
        AuthoritativeThreadSubject {
            thread_id: "T-subject".to_string(),
            chain_root_id: "T-subject".to_string(),
            kind: "tool".to_string(),
            item_ref: "tool:test/subject".to_string(),
            executor_ref: "native:test".to_string(),
            launch_mode: "wait".to_string(),
            current_site_id: "site:host".to_string(),
            origin_site_id: "site:host".to_string(),
            upstream_thread_id: None,
            project_root: None,
            created_at: "created".to_string(),
            updated_at: "updated".to_string(),
            started_at: None,
            finished_at: None,
            status: ThreadStatus::Running,
            requested_by: requested_by.map(str::to_string),
            project_authority,
            admitted_launch_capsule_hash: None,
            result_project_snapshot_hash: None,
            result_workspace_output_capture_hash: None,
        }
    }

    fn pinned() -> ExecutionProjectAuthority {
        ExecutionProjectAuthority::PinnedGeneration {
            stable_project_identity: "project".to_string(),
            display_path: None,
            base_snapshot_hash: "a".repeat(64),
            snapshot_hash: "b".repeat(64),
            realization: PinnedProjectRealization::ReadOnly,
            workspace_outputs: None,
            environment: EnvironmentAuthority::None,
            capability_ceiling: Vec::new(),
            child_policy: ChildProjectAuthorityPolicy::Inherit,
            signer_project: ryeos_state::project_identity::LogicalProject::Absent(
                ryeos_state::project_identity::LogicalProjectAbsence::NotRegistered,
            ),
        }
    }

    #[test]
    fn projected_thread_facts_must_match_signed_subject() {
        let signed = subject(Some("fp:owner"), pinned());
        let matches = |kind: &str,
                       item_ref: &str,
                       launch_mode: &str,
                       created_at: &str,
                       updated_at: &str,
                       status: &str,
                       capsule: &Option<String>| {
            projected_thread_matches_signed_subject(
                kind,
                item_ref,
                launch_mode,
                created_at,
                updated_at,
                status,
                capsule,
                &signed,
            )
        };
        let valid = || {
            matches(
                "tool",
                "tool:test/subject",
                "wait",
                "created",
                "updated",
                signed.status.as_str(),
                &None,
            )
        };
        assert!(valid());
        assert!(!matches(
            "forged",
            "tool:test/subject",
            "wait",
            "created",
            "updated",
            signed.status.as_str(),
            &None
        ));
        assert!(!matches(
            "tool",
            "tool:forged/subject",
            "wait",
            "created",
            "updated",
            signed.status.as_str(),
            &None
        ));
        assert!(!matches(
            "tool",
            "tool:test/subject",
            "detached",
            "created",
            "updated",
            signed.status.as_str(),
            &None
        ));
        assert!(!matches(
            "tool",
            "tool:test/subject",
            "wait",
            "forged",
            "updated",
            signed.status.as_str(),
            &None
        ));
        assert!(!matches(
            "tool",
            "tool:test/subject",
            "wait",
            "created",
            "forged",
            signed.status.as_str(),
            &None
        ));
        assert!(!matches(
            "tool",
            "tool:test/subject",
            "wait",
            "created",
            "updated",
            "forged",
            &None
        ));
        assert!(!matches(
            "tool",
            "tool:test/subject",
            "wait",
            "created",
            "updated",
            signed.status.as_str(),
            &Some("forged-capsule".to_string())
        ));
    }
}
