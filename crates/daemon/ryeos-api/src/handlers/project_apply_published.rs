//! `project/apply-published` — apply a published result to the caller's
//! registered checkout.
//!
//! A worker's accepted result moves the project head when it is published and
//! changes no file of the person's checkout. This service applies the
//! difference between the tree the result was authored against and the tree it
//! produced. It never merges: a path the person has changed that the result
//! also changes is a conflict, every conflict is reported at once, and nothing
//! is written when there is one.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use lillux::cas::CasStore;
use ryeos_app::handler_error::typed_refusal;
use ryeos_app::state::AppState;
use ryeos_executor::executor::ServiceAvailability;
use ryeos_state::objects::{ProjectSnapshot, ProjectTree};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::handler_context::HandlerContext;
use crate::registry::ServiceDescriptor;
use crate::remote::pull::{self, ApplyConflict, PullResultsError};

/// Most snapshots one ancestry walk visits before it refuses to answer.
const MAX_ANCESTRY_WALK: usize = 4096;
/// Largest conflict list, as compact JSON, the answer carries. A longer list
/// is shortened and says so; the answer is never cut mid-entry.
const MAX_CONFLICT_LIST_BYTES: usize = 200_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The project, by the name its manifest declares. The caller's own
    /// registration of this project names the checkout; a path is never taken
    /// from the request.
    pub project_name: ryeos_state::project_identity::ProjectName,
    /// The published snapshot the result was authored against.
    pub base_snapshot_hash: String,
    /// The published snapshot to apply.
    pub result_snapshot_hash: String,
    /// One of the registered checkout paths. Required when the registration
    /// lists more than one.
    #[serde(default)]
    pub checkout: Option<String>,
}

pub async fn handle(req: Request, ctx: HandlerContext, state: Arc<AppState>) -> Result<Value> {
    ctx.require_verified().map_err(|e| anyhow!(e))?;
    // The fixed live-write capability, before anything is read.
    state
        .authorizer
        .authorize(
            &ctx.scopes,
            &ryeos_runtime::authorizer::AuthorizationPolicy::require(
                ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY,
            ),
        )
        .map_err(|_| {
            typed_refusal(
                "project_apply_requires_live_write",
                403,
                format!(
                    "applying a result to a checkout requires fixed capability `{}`",
                    ryeos_app::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY
                ),
            )
        })?;
    for (label, hash) in [
        ("base_snapshot_hash", &req.base_snapshot_hash),
        ("result_snapshot_hash", &req.result_snapshot_hash),
    ] {
        if !lillux::valid_hash(hash) {
            return Err(typed_refusal(
                "invalid_snapshot_hash",
                400,
                format!("{label} is not a SHA-256 digest"),
            )
            .into());
        }
    }

    // The registration must belong to the owner the calling key acts for and
    // name this project. Whether the project is unregistered or registered by
    // someone else, the refusal is the same.
    let caller_owner = ryeos_app::project_owner::owner_principal(&state, &ctx.fingerprint)?;
    let registrations: Vec<_> = ryeos_app::project_registry::load_registrations(
        &state.config.app_root,
        state.identity.verifying_key(),
    )?
    .into_iter()
    .map(|loaded| loaded.registration)
    .collect();
    let registration = select_registration(&registrations, &caller_owner, &req.project_name)
        .ok_or_else(|| {
            typed_refusal(
                "project_not_registered",
                403,
                "no checkout of this project is registered for the calling key's owner",
            )
        })?;
    let checkout_path = select_checkout(registration, req.checkout.as_deref())?;
    let (canonical, declared) =
        ryeos_app::project_registry::inspect_checkout(&state.config.app_root, checkout_path)?;
    if declared.as_ref() != Some(&req.project_name) {
        return Err(typed_refusal(
            "registered_name_mismatch",
            409,
            "the registered checkout no longer declares this project's name",
        )
        .into());
    }
    let project_directory = lillux::PinnedDirectory::open(&canonical)?
        .ok_or_else(|| anyhow!("registered checkout directory disappeared"))?;
    project_directory.ensure_path_binding()?;
    // The apply takes the project mutation lock itself; holding it here would
    // make the apply wait on this handler.

    let authority = state
        .state_store
        .with_state_db(|db| db.pinned_authority())?;
    // Every read of the store happens under the shared guard in this block,
    // which holds no await; the guard is released before the apply runs. The
    // snapshots read are reachable from the published head, so they stay
    // rooted while the apply reads their blobs.
    let (base_tree, result_tree) = {
        let _cas_read_guard = authority.acquire_shared_guard()?;
        let cas = authority.cas_store()?;

        // The result is the caller's published head or an ancestor of it, and the
        // base is an ancestor of the result: the apply follows what publication
        // already proved.
        let head_key = ryeos_state::project_identity::ProjectHeadKey::owned_by(
            &caller_owner,
            &req.project_name,
        );
        let head = state
            .state_store
            .with_state_db(|db| db.read_project_head(&head_key))?;
        let published = match head.as_deref() {
            Some(head) => snapshot_reaches(&cas, head, &req.result_snapshot_hash)?,
            None => false,
        };
        if !published {
            return Err(typed_refusal(
                "project_apply_not_published",
                409,
                "the result snapshot is not this project's published head or one of its ancestors",
            )
            .into());
        }
        if !snapshot_reaches(&cas, &req.result_snapshot_hash, &req.base_snapshot_hash)? {
            return Err(typed_refusal(
                "project_apply_base_not_ancestor",
                409,
                "the base snapshot is not an ancestor of the result snapshot",
            )
            .into());
        }
        let base_tree = load_tree(&cas, &req.base_snapshot_hash)?;
        let result_tree = load_tree(&cas, &req.result_snapshot_hash)?;
        (base_tree, result_tree)
    };

    match pull::apply_published_result(&authority, &canonical, &base_tree, &result_tree).await {
        Ok((files_updated, files_deleted)) => Ok(answer(
            &req,
            Applied::Yes {
                files_updated,
                files_deleted,
            },
        )),
        Err(PullResultsError::LocalConflicts { conflicts, total }) => {
            Ok(answer(&req, Applied::No { conflicts, total }))
        }
        Err(PullResultsError::RecoveryRequired(message)) => {
            Err(typed_refusal("project_apply_recovery_required", 409, message).into())
        }
        Err(other) => Err(anyhow::Error::from(other)),
    }
}

/// The registration of `name` that belongs to `owner`. A project registered by
/// anyone else, or not at all, is the same absence.
fn select_registration<'a>(
    registrations: &'a [ryeos_app::project_registry::ProjectRegistration],
    owner: &ryeos_state::principal_id::PrincipalId,
    name: &ryeos_state::project_identity::ProjectName,
) -> Option<&'a ryeos_app::project_registry::ProjectRegistration> {
    registrations.iter().find(|registration| {
        registration.identity.owner() == owner && registration.identity.name() == name
    })
}

/// The one registered checkout the apply targets: the requested path when it
/// is one of the registered paths, or the only registered path.
fn select_checkout<'a>(
    registration: &'a ryeos_app::project_registry::ProjectRegistration,
    requested: Option<&str>,
) -> Result<&'a std::path::PathBuf, ryeos_app::handler_error::HandlerError> {
    match requested {
        Some(requested) => registration
            .paths
            .iter()
            .find(|path| path.as_path() == std::path::Path::new(requested))
            .ok_or_else(|| {
                typed_refusal(
                    "checkout_not_registered",
                    403,
                    "the requested checkout is not one of this project's registered paths",
                )
            }),
        None if registration.paths.len() == 1 => Ok(&registration.paths[0]),
        None => Err(typed_refusal(
            "checkout_required",
            400,
            "the project is registered at several checkouts; name one with `checkout`",
        )),
    }
}

enum Applied {
    Yes {
        files_updated: usize,
        files_deleted: usize,
    },
    No {
        conflicts: Vec<ApplyConflict>,
        total: usize,
    },
}

/// The service's answer. A set of conflicts is an answer, not an error, so the
/// person the service serves receives the paths: the handler has already
/// refused every key that is not the checkout's registered owner.
fn answer(req: &Request, outcome: Applied) -> Value {
    match outcome {
        Applied::Yes {
            files_updated,
            files_deleted,
        } => json!({
            "applied": true,
            "code": null,
            "files_updated": files_updated,
            "files_deleted": files_deleted,
            "conflicts": [],
            "total": 0,
            "truncated": false,
            "base_snapshot_hash": req.base_snapshot_hash,
            "result_snapshot_hash": req.result_snapshot_hash,
        }),
        Applied::No {
            mut conflicts,
            total,
        } => {
            while !conflicts.is_empty()
                && serde_json::to_vec(&conflicts).map_or(usize::MAX, |bytes| bytes.len())
                    > MAX_CONFLICT_LIST_BYTES
            {
                conflicts.pop();
            }
            let truncated = conflicts.len() < total;
            json!({
                "applied": false,
                "code": "project_apply_conflicts",
                "files_updated": 0,
                "files_deleted": 0,
                "conflicts": conflicts,
                "total": total,
                "truncated": truncated,
                "base_snapshot_hash": req.base_snapshot_hash,
                "result_snapshot_hash": req.result_snapshot_hash,
            })
        }
    }
}

fn load_snapshot(cas: &CasStore, hash: &str) -> Result<ProjectSnapshot> {
    let object = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        hash,
        ryeos_state::object_closure::ObjectClosureLimits::default().max_object_bytes,
    )?;
    ProjectSnapshot::from_value(&object)
}

fn load_tree(cas: &CasStore, snapshot_hash: &str) -> Result<ProjectTree> {
    let snapshot = load_snapshot(cas, snapshot_hash)?;
    let tree = ryeos_state::object_closure::load_exact_cas_object_with_cas(
        cas,
        &snapshot.project_tree_hash,
        ryeos_state::object_closure::ObjectClosureLimits::default().max_object_bytes,
    )?;
    ProjectTree::from_value(&tree)
}

/// Whether `target` is `from` or reachable from it through parent hashes. A
/// walk past [`MAX_ANCESTRY_WALK`] snapshots refuses rather than guessing.
fn snapshot_reaches(cas: &CasStore, from: &str, target: &str) -> Result<bool> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::from([from.to_owned()]);
    while let Some(hash) = queue.pop_front() {
        if hash == target {
            return Ok(true);
        }
        if !seen.insert(hash.clone()) {
            continue;
        }
        if seen.len() > MAX_ANCESTRY_WALK {
            return Err(typed_refusal(
                "project_apply_history_too_deep",
                409,
                "the published history is longer than this service walks",
            )
            .into());
        }
        queue.extend(load_snapshot(cas, &hash)?.parent_hashes);
    }
    Ok(false)
}

pub const DESCRIPTOR: ServiceDescriptor = ServiceDescriptor {
    service_ref: "service:project/apply-published",
    endpoint: "project.apply-published",
    availability: ServiceAvailability::DaemonOnly,
    required_caps: &["ryeos.execute.service.project/apply-published"],
    handler: |params, ctx, state| {
        Box::pin(async move {
            let req: Request = crate::handler_error::parse_request(params)?;
            handle(req, ctx, state).await
        })
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    fn request_json(extra: Value) -> Value {
        let mut value = json!({
            "project_name": "demo",
            "base_snapshot_hash": "a".repeat(64),
            "result_snapshot_hash": "b".repeat(64),
        });
        if let (Some(target), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        value
    }

    #[test]
    fn the_request_takes_no_path_and_no_unknown_field() {
        assert!(serde_json::from_value::<Request>(request_json(json!({}))).is_ok());
        for forbidden in ["project_path", "path", "root", "expected_owner"] {
            let bad = request_json(json!({ forbidden: "/etc" }));
            assert!(
                serde_json::from_value::<Request>(bad).is_err(),
                "{forbidden} must not be accepted"
            );
        }
    }

    #[test]
    fn a_long_conflict_list_is_shortened_and_says_so() {
        let req: Request = serde_json::from_value(request_json(json!({}))).unwrap();
        let conflicts = (0..256)
            .map(|index| ApplyConflict {
                path: format!("{index:03}-{}", "x".repeat(2000)),
                reason: pull::ApplyConflictReason::Drifted,
            })
            .collect::<Vec<_>>();
        let value = answer(
            &req,
            Applied::No {
                conflicts,
                total: 300,
            },
        );
        assert_eq!(value["applied"], false);
        assert_eq!(value["code"], "project_apply_conflicts");
        assert_eq!(value["total"], 300);
        assert_eq!(value["truncated"], true);
        let kept = value["conflicts"].as_array().unwrap();
        assert!(!kept.is_empty() && kept.len() < 256);
        assert!(serde_json::to_vec(&value["conflicts"]).unwrap().len() <= MAX_CONFLICT_LIST_BYTES);

        let applied = answer(
            &req,
            Applied::Yes {
                files_updated: 2,
                files_deleted: 1,
            },
        );
        assert_eq!(applied["applied"], true);
        assert_eq!(applied["truncated"], false);
        assert!(applied["code"].is_null());
    }

    fn owner(byte: char) -> ryeos_state::principal_id::PrincipalId {
        ryeos_state::principal_id::PrincipalId::from_storage_component(&format!(
            "pid-v1-{}",
            byte.to_string().repeat(64)
        ))
        .unwrap()
    }

    fn name(value: &str) -> ryeos_state::project_identity::ProjectName {
        value.parse().unwrap()
    }

    fn registration(
        owner: &ryeos_state::principal_id::PrincipalId,
        project: &str,
        paths: &[&str],
    ) -> ryeos_app::project_registry::ProjectRegistration {
        ryeos_app::project_registry::ProjectRegistration {
            schema: ryeos_app::project_registry::PROJECT_REGISTRATION_SCHEMA,
            identity: ryeos_state::project_identity::ProjectIdentity::new(
                owner.clone(),
                name(project),
            ),
            paths: paths.iter().map(std::path::PathBuf::from).collect(),
            request_digest: "0".repeat(64),
        }
    }

    #[test]
    fn only_the_callers_own_registration_of_the_named_project_is_selected() {
        let (mine, theirs) = (owner('a'), owner('b'));
        let registrations = vec![
            registration(&theirs, "demo", &["/home/other/demo"]),
            registration(&mine, "other", &["/home/me/other"]),
            registration(&mine, "demo", &["/home/me/demo"]),
        ];
        let picked = select_registration(&registrations, &mine, &name("demo")).unwrap();
        assert_eq!(
            picked.paths,
            vec![std::path::PathBuf::from("/home/me/demo")]
        );
        // Another owner's project of the same name, and a project the caller
        // never registered, are the same absence.
        assert!(select_registration(&registrations, &owner('c'), &name("demo")).is_none());
        assert!(select_registration(&registrations, &mine, &name("missing")).is_none());
        // Project A's registration is never returned for project B.
        let a = select_registration(&registrations, &mine, &name("other")).unwrap();
        assert_ne!(a.paths, picked.paths);
    }

    #[test]
    fn the_checkout_is_one_of_the_registered_paths_never_a_path_the_caller_names() {
        let mine = owner('a');
        let single = registration(&mine, "demo", &["/home/me/demo"]);
        assert_eq!(
            select_checkout(&single, None).unwrap(),
            &std::path::PathBuf::from("/home/me/demo")
        );
        assert!(select_checkout(&single, Some("/home/me/demo")).is_ok());
        assert!(select_checkout(&single, Some("/etc")).is_err());
        assert!(select_checkout(&single, Some("/home/me/demo/../../../etc")).is_err());

        let several = registration(&mine, "demo", &["/home/me/a", "/home/me/b"]);
        assert!(select_checkout(&several, None).is_err());
        assert_eq!(
            select_checkout(&several, Some("/home/me/b")).unwrap(),
            &std::path::PathBuf::from("/home/me/b")
        );
    }

    fn store_snapshot(cas: &CasStore, parents: &[&str], marker: &str) -> String {
        let tree = ProjectTree {
            files: std::collections::BTreeMap::new(),
        };
        let snapshot = ProjectSnapshot {
            project_tree_hash: cas.store_object(&tree.to_value()).unwrap(),
            effective_policy_hash: "0".repeat(64),
            parent_hashes: parents.iter().map(|hash| (*hash).to_owned()).collect(),
            message: Some(marker.to_owned()),
            created_at: "2026-11-26T00:00:00Z".to_owned(),
            source: "test".to_owned(),
        };
        cas.store_object(&snapshot.to_value()).unwrap()
    }

    #[test]
    fn a_snapshot_of_another_project_is_not_reachable_from_this_projects_head() {
        let dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(dir.path().join("cas"));
        let a1 = store_snapshot(&cas, &[], "a1");
        let a2 = store_snapshot(&cas, &[&a1], "a2");
        let a3 = store_snapshot(&cas, &[&a2], "a3");
        let b1 = store_snapshot(&cas, &[], "b1");

        // Project A: the head reaches itself and every ancestor.
        assert!(snapshot_reaches(&cas, &a3, &a3).unwrap());
        assert!(snapshot_reaches(&cas, &a3, &a1).unwrap());
        // An ancestor does not reach a descendant.
        assert!(!snapshot_reaches(&cas, &a1, &a3).unwrap());
        // Project B's candidate can never be applied against project A's head.
        assert!(!snapshot_reaches(&cas, &a3, &b1).unwrap());
    }
}
