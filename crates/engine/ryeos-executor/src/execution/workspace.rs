//! Per-launch execution workspace layout.
//!
//! RyeOS owns the canonical project generation and lifecycle. A selected
//! signed isolation adapter may own opaque backend state, but the executor
//! consumes only its normalized mutation evidence.

use anyhow::Result;
use ryeos_state::objects::{ProjectFile, ProjectSnapshotPolicy, ProjectTree};

pub use ryeos_engine::execution_workspace::{BACKEND_STATE_DIR, PROJECT_DIR, WorkspaceLayout};

/// Apply a normalized adapter mutation set to an immutable project tree.
/// Only changed regular bytes are streamed into CAS; unchanged object hashes
/// are retained verbatim.
pub fn apply_workspace_delta(
    authority: &ryeos_state::PinnedStateAuthority,
    guard: &ryeos_state::CasMutationGuard,
    staged_roots: &mut ryeos_state::StagedCasRootLease,
    mutation_content: &lillux::PinnedDirectory,
    base_tree: &ProjectTree,
    policy: &ProjectSnapshotPolicy,
    mutations: &[ryeos_isolation_protocol::WorkspaceMutation],
    operational_shadow_paths: &[String],
) -> Result<Option<ProjectTree>> {
    authority.ensure_guard(guard)?;
    policy.validate()?;
    super::ingest::validate_operational_exclusions(operational_shadow_paths)?;
    if mutations.len() > ryeos_isolation_protocol::MAX_WORKSPACE_MUTATIONS {
        anyhow::bail!("workspace mutation count exceeds protocol limit");
    }
    let matcher = policy.matcher()?;
    let mut selected = Vec::new();
    for mutation in mutations {
        mutation.validate()?;
        let relative = mutation.path.as_str();
        ryeos_state::project_sync::validate_safe_relative_path(relative)?;
        // These are admitted mount/copy destinations, not another ignore
        // policy. Neither input bytes nor private mount placeholders are
        // authored outputs. Skip them before opening or importing bytes.
        if super::ingest::is_operationally_excluded(relative, operational_shadow_paths) {
            continue;
        }
        let included = !ryeos_state::project_sync::is_project_snapshot_floor_excluded(relative)
            && !matcher.is_ignored(relative)
            && (policy.sync_scope != ryeos_state::project_sync::ProjectSyncScope::AiOnly
                || matches!(
                    ryeos_state::project_sync::classify_project_ai_path(relative, Some(&matcher)),
                    ryeos_state::project_sync::ProjectAiPathClass::Deployable(_)
                ));
        if mutation.kind == ryeos_isolation_protocol::WorkspaceMutationKind::UpsertSymlink {
            if included {
                anyhow::bail!(
                    "symlink mutation lies outside an admitted output partition: {relative}"
                );
            }
            continue;
        }
        let expected =
            if mutation.kind == ryeos_isolation_protocol::WorkspaceMutationKind::UpsertRegular {
                if !included {
                    continue;
                }
                ryeos_state::project_sync::validate_project_manifest_path(
                    relative,
                    policy.sync_scope,
                    Some(&matcher),
                )?;
                // These are validated adapter expectations, not authority that the
                // bytes have already been captured. Actual pinned-file metadata and
                // streamed bytes must still agree below before any tree publication.
                let object = ProjectFile {
                    blob_hash: mutation
                        .content_hash
                        .clone()
                        .expect("validated regular mutation has a content hash"),
                    size: mutation
                        .size
                        .expect("validated regular mutation has a size"),
                    normalized_mode: mutation
                        .normalized_mode
                        .expect("validated regular mutation has a normalized mode"),
                };
                object.validate()?;
                let object_hash = ryeos_state::objects::canonical_value_digest(&object.to_value())?;
                Some((object, object_hash))
            } else {
                None
            };
        selected.push((mutation, expected));
    }

    // The existing lease owns crash/GC protection, not the mutation loop. Root
    // the complete validated expectation set once before the first CAS write,
    // while retaining the same outer CAS guard. Subsequent ordinary store calls
    // then verify the bytes without rewriting an ever-growing roots journal for
    // every file. A batch publication refusal/ambiguity aborts before capture;
    // no new journal, unrooted write path, or weaker recovery mode is introduced.
    staged_roots.protect_cas_closure_admitted(
        guard,
        selected
            .iter()
            .filter_map(|(_, expected)| expected.as_ref().map(|(_, hash)| hash.as_str())),
        selected.iter().filter_map(|(_, expected)| {
            expected
                .as_ref()
                .map(|(object, _)| object.blob_hash.as_str())
        }),
    )?;

    let cas = authority.cas_store()?;
    let mut next = base_tree.clone();
    for (mutation, expected) in selected {
        let relative = mutation.path.as_str();
        match mutation.kind {
            ryeos_isolation_protocol::WorkspaceMutationKind::UpsertSymlink => {
                anyhow::bail!("unfiltered source symlink mutation: {relative}");
            }
            ryeos_isolation_protocol::WorkspaceMutationKind::DeletePath => {
                remove_path_and_descendants(&mut next, relative);
            }
            ryeos_isolation_protocol::WorkspaceMutationKind::EnsureDirectory => {
                next.files.remove(relative);
            }
            ryeos_isolation_protocol::WorkspaceMutationKind::OpaqueDirectory => {
                next.files.remove(relative);
                remove_descendants(&mut next, relative);
            }
            ryeos_isolation_protocol::WorkspaceMutationKind::UpsertRegular => {
                let (object, expected_hash) =
                    expected.expect("selected regular mutation has validated file facts");
                let (parent, name) = open_mutation_parent(mutation_content, relative)?;
                let file = parent.open_regular(name.as_ref(), false)?.ok_or_else(|| {
                    anyhow::anyhow!("workspace mutation file disappeared: {relative}")
                })?;
                let metadata = file.metadata()?;
                if !metadata.file_type().is_file() {
                    anyhow::bail!("workspace mutation is not a regular file: {relative}");
                }
                let observed_mode = lillux::normalized_portable_regular_mode(&metadata)?;
                if object.normalized_mode != observed_mode {
                    anyhow::bail!(
                        "workspace mutation mode changed after adapter freeze: {relative}"
                    );
                }
                let streamed = cas.put_blob_from_open_regular_bounded(
                    file,
                    &parent.path().join(&name),
                    object.size,
                )?;
                // The stream's stable before/after observation also owns mode
                // identity: the earlier metadata read alone cannot fence a
                // permission change between that read and byte capture.
                if object.size != streamed.size
                    || object.blob_hash != streamed.hash
                    || object.normalized_mode != streamed.normalized_mode
                {
                    anyhow::bail!(
                        "workspace mutation bytes or mode differ from the quiesced adapter evidence: {relative}"
                    );
                }
                staged_roots.protect_blob_hash_admitted(guard, &streamed.hash)?;
                let object_hash =
                    staged_roots.store_object_admitted(guard, &cas, &object.to_value())?;
                if object_hash != expected_hash {
                    anyhow::bail!(
                        "captured project file differs from its staged identity: {relative}"
                    );
                }
                remove_descendants(&mut next, relative);
                next.files.insert(relative.to_string(), object_hash);
            }
        }
    }
    // An ancestor delete/opaque mutation can also cover an input shadow.
    // Reuse copy-based fold-back's exact base restoration and collision
    // validation rather than inventing backend-specific exclusion semantics.
    super::ingest::restore_operational_shadow_files(
        &mut next,
        base_tree,
        operational_shadow_paths,
    )?;
    ryeos_state::project_sync::validate_project_tree_paths(&next, policy)?;
    Ok((next != *base_tree).then_some(next))
}

pub(super) fn open_mutation_parent(
    root: &lillux::PinnedDirectory,
    relative: &str,
) -> Result<(lillux::PinnedDirectory, std::ffi::OsString)> {
    let mut components = relative.split('/').collect::<Vec<_>>();
    let name = components
        .pop()
        .ok_or_else(|| anyhow::anyhow!("workspace mutation path is empty"))?;
    let mut parent = root.try_clone()?;
    for component in components {
        parent = parent
            .open_child_directory(component.as_ref())?
            .ok_or_else(|| anyhow::anyhow!("workspace mutation parent is missing: {relative}"))?;
    }
    Ok((parent, std::ffi::OsString::from(name)))
}

fn remove_path_and_descendants(tree: &mut ProjectTree, path: &str) {
    tree.files.remove(path);
    remove_descendants(tree, path);
}

fn remove_descendants(tree: &mut ProjectTree, path: &str) {
    let descendant_prefix = format!("{path}/");
    // ProjectTree already owns sorted path identity. Visit only the exact
    // descendant range: scanning every unrelated file on each upsert makes a
    // large ordinary retained output quadratic even after batched CAS rooting.
    loop {
        let descendant = tree
            .files
            .range(descendant_prefix.clone()..)
            .next()
            .filter(|(candidate, _)| candidate.starts_with(&descendant_prefix))
            .map(|(candidate, _)| candidate.clone());
        let Some(descendant) = descendant else {
            break;
        };
        tree.files.remove(&descendant);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_isolation_protocol::{WorkspaceMutation, WorkspaceMutationKind};

    fn regular_mutation(path: &str, bytes: &[u8]) -> WorkspaceMutation {
        WorkspaceMutation {
            path: path.to_owned(),
            kind: WorkspaceMutationKind::UpsertRegular,
            normalized_mode: Some(ProjectFile::REGULAR_MODE),
            size: Some(bytes.len() as u64),
            content_hash: Some(lillux::cas::sha256_hex(bytes)),
            target: None,
        }
    }

    fn expected_file(mutation: &WorkspaceMutation) -> ProjectFile {
        ProjectFile {
            blob_hash: mutation.content_hash.clone().unwrap(),
            size: mutation.size.unwrap(),
            normalized_mode: mutation.normalized_mode.unwrap(),
        }
    }

    #[test]
    fn descendant_ranges_preserve_exact_files_and_adjacent_prefixes() {
        let original = ProjectTree {
            files: ["a", "a/b", "a/b/c", "a/b/c/d", "a/bb", "a0", "ab", "z"]
                .into_iter()
                .map(|path| (path.to_owned(), "a".repeat(64)))
                .collect(),
        };
        let mut descendants = original.clone();
        remove_descendants(&mut descendants, "a/b");
        assert_eq!(
            descendants
                .files
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["a", "a/b", "a/bb", "a0", "ab", "z"]
        );
        let mut deleted = original.clone();
        remove_path_and_descendants(&mut deleted, "a/b");
        assert_eq!(
            deleted.files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["a", "a/bb", "a0", "ab", "z"]
        );
        remove_path_and_descendants(&mut deleted, "missing");
        remove_descendants(&mut deleted, "z");
        assert_eq!(deleted.files.len(), 5);
    }

    #[test]
    fn foldback_roots_complete_capture_before_opening_any_input_and_preserves_equality() {
        let temporary = tempfile::tempdir().unwrap();
        let state = ryeos_state::StateDb::open(
            temporary.path(),
            std::sync::Arc::new(ryeos_state::TrustStore::new()),
        )
        .unwrap();
        let authority = state.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let mut roots = authority
            .require_recovery()
            .unwrap()
            .begin_staged_cas_roots_admitted(&guard, "batch-capture-test")
            .unwrap();
        let bytes = tempfile::tempdir().unwrap();
        let content = lillux::PinnedDirectory::open(bytes.path())
            .unwrap()
            .unwrap();
        let policy = ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            Default::default(),
        )
        .unwrap();
        let base = ProjectTree {
            files: Default::default(),
        };
        let link = WorkspaceMutation {
            path: "products/runtime/link".into(),
            kind: WorkspaceMutationKind::UpsertSymlink,
            normalized_mode: None,
            size: None,
            content_hash: None,
            target: Some("member".into()),
        };
        let error = apply_workspace_delta(
            &authority,
            &guard,
            &mut roots,
            &content,
            &base,
            &policy,
            std::slice::from_ref(&link),
            &[],
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside an admitted output partition")
        );
        // An admitted output root is excluded before any source-side open.
        // No target exists in this fixture, proving this is not link following.
        assert!(
            apply_workspace_delta(
                &authority,
                &guard,
                &mut roots,
                &content,
                &base,
                &policy,
                &[link],
                &["products/runtime".into()]
            )
            .unwrap()
            .is_none()
        );
        let mutations = [regular_mutation("a", b"one"), regular_mutation("b", b"two")];
        let expected = mutations.iter().map(expected_file).collect::<Vec<_>>();
        let expected_objects = expected
            .iter()
            .map(|file| ryeos_state::objects::canonical_value_digest(&file.to_value()).unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        let expected_blobs = expected
            .iter()
            .map(|file| file.blob_hash.clone())
            .collect::<std::collections::BTreeSet<_>>();
        // The first source is absent. Even this early refusal must leave the
        // complete expectation set protected, but no captured CAS objects.
        assert!(
            apply_workspace_delta(
                &authority,
                &guard,
                &mut roots,
                &content,
                &base,
                &policy,
                &mutations,
                &[],
            )
            .unwrap_err()
            .to_string()
            .contains("disappeared")
        );
        let staged = authority
            .require_recovery()
            .unwrap()
            .inspect_staged_cas_root_hashes_read_only()
            .unwrap();
        assert_eq!(
            staged
                .object_hashes
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            expected_objects
        );
        assert_eq!(
            staged
                .blob_hashes
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            expected_blobs
        );
        let cas = authority.cas_store().unwrap();
        for hash in &expected_objects {
            assert!(cas.get_object(hash).unwrap().is_none());
        }
        for hash in &expected_blobs {
            assert!(!cas.has_blob(hash).unwrap());
        }
        std::fs::write(bytes.path().join("a"), b"one").unwrap();
        std::fs::write(bytes.path().join("b"), b"two").unwrap();
        let captured = apply_workspace_delta(
            &authority,
            &guard,
            &mut roots,
            &content,
            &base,
            &policy,
            &mutations,
            &[],
        )
        .unwrap()
        .unwrap();
        for (mutation, file) in mutations.iter().zip(&expected) {
            let hash = &captured.files[&mutation.path];
            assert_eq!(cas.get_object(hash).unwrap(), Some(file.to_value()));
        }
        assert!(
            apply_workspace_delta(
                &authority,
                &guard,
                &mut roots,
                &content,
                &captured,
                &policy,
                &mutations,
                &[],
            )
            .unwrap()
            .is_none()
        );
        // A valid prior observation is not permission to trust changed bytes.
        std::fs::write(bytes.path().join("b"), b"bad").unwrap();
        assert!(
            apply_workspace_delta(
                &authority,
                &guard,
                &mut roots,
                &content,
                &captured,
                &policy,
                &mutations,
                &[],
            )
            .unwrap_err()
            .to_string()
            .contains("bytes or mode differ")
        );
        // The existing bounded CAS stream rejects growth beyond the admitted
        // size instead of reading an unbounded replacement before comparison.
        std::fs::write(bytes.path().join("b"), b"oversized").unwrap();
        assert!(
            apply_workspace_delta(
                &authority,
                &guard,
                &mut roots,
                &content,
                &captured,
                &policy,
                &mutations,
                &[],
            )
            .unwrap_err()
            .to_string()
            .contains("exceeds")
        );
    }

    #[test]
    fn malformed_later_mutation_refuses_before_staging_or_capturing_earlier_files() {
        let temporary = tempfile::tempdir().unwrap();
        let state = ryeos_state::StateDb::open(
            temporary.path(),
            std::sync::Arc::new(ryeos_state::TrustStore::new()),
        )
        .unwrap();
        let authority = state.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let mut roots = authority
            .require_recovery()
            .unwrap()
            .begin_staged_cas_roots_admitted(&guard, "invalid-batch-test")
            .unwrap();
        let bytes = tempfile::tempdir().unwrap();
        std::fs::write(bytes.path().join("a"), b"one").unwrap();
        let content = lillux::PinnedDirectory::open(bytes.path())
            .unwrap()
            .unwrap();
        let policy = ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            Default::default(),
        )
        .unwrap();
        let first = regular_mutation("a", b"one");
        let mut invalid = regular_mutation("b", b"two");
        invalid.content_hash = Some("not-a-digest".to_owned());
        assert!(
            apply_workspace_delta(
                &authority,
                &guard,
                &mut roots,
                &content,
                &ProjectTree {
                    files: Default::default()
                },
                &policy,
                &[first.clone(), invalid],
                &[],
            )
            .is_err()
        );
        let staged = authority
            .require_recovery()
            .unwrap()
            .inspect_staged_cas_root_hashes_read_only()
            .unwrap();
        assert!(staged.object_hashes.is_empty());
        assert!(staged.blob_hashes.is_empty());
        assert!(
            !authority
                .cas_store()
                .unwrap()
                .has_blob(first.content_hash.as_deref().unwrap())
                .unwrap()
        );
    }

    #[test]
    fn operational_shadow_delta_uses_the_same_input_boundary_as_capture() {
        let temporary = tempfile::tempdir().unwrap();
        let state = ryeos_state::StateDb::open(
            temporary.path(),
            std::sync::Arc::new(ryeos_state::TrustStore::new()),
        )
        .unwrap();
        let authority = state.pinned_authority().unwrap();
        let guard = authority.acquire_shared_guard().unwrap();
        let mut roots = authority
            .require_recovery()
            .unwrap()
            .begin_staged_cas_roots_admitted(&guard, "shadow-test")
            .unwrap();
        let bytes = tempfile::tempdir().unwrap();
        let content = lillux::PinnedDirectory::open(bytes.path())
            .unwrap()
            .unwrap();
        let policy = ProjectSnapshotPolicy::new(
            ryeos_state::project_sync::ProjectSyncScope::FullProject,
            vec![],
            vec![],
            Default::default(),
        )
        .unwrap();
        let base = ProjectTree {
            files: [
                ("inputs/base".to_owned(), "a".repeat(64)),
                ("inputs/ordinary".to_owned(), "b".repeat(64)),
            ]
            .into(),
        };
        let excluded = vec!["inputs/base".to_owned(), "inputs/new".to_owned()];
        let mutation = |path: &str, kind| WorkspaceMutation {
            path: path.to_owned(),
            kind,
            normalized_mode: None,
            size: None,
            content_hash: None,
            target: None,
        };
        let placeholder = |path: &str| WorkspaceMutation {
            normalized_mode: Some(ProjectFile::REGULAR_MODE),
            size: Some(0),
            content_hash: Some(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
            ),
            ..mutation(path, WorkspaceMutationKind::UpsertRegular)
        };
        // No placeholder files exist in the content root: importing them
        // would fail. Both a new placeholder and a hidden base file are no-ops.
        assert!(
            apply_workspace_delta(
                &authority,
                &guard,
                &mut roots,
                &content,
                &base,
                &policy,
                &[placeholder("inputs/new"), placeholder("inputs/base")],
                &excluded
            )
            .unwrap()
            .is_none()
        );
        for kind in [
            WorkspaceMutationKind::DeletePath,
            WorkspaceMutationKind::OpaqueDirectory,
        ] {
            let tree = apply_workspace_delta(
                &authority,
                &guard,
                &mut roots,
                &content,
                &base,
                &policy,
                &[mutation("inputs", kind)],
                &excluded,
            )
            .unwrap()
            .unwrap();
            assert_eq!(
                tree.files,
                [("inputs/base".to_owned(), "a".repeat(64))].into()
            );
        }
        std::fs::write(bytes.path().join("inputs"), b"").unwrap();
        let error = apply_workspace_delta(
            &authority,
            &guard,
            &mut roots,
            &content,
            &base,
            &policy,
            &[placeholder("inputs")],
            &excluded,
        )
        .unwrap_err();
        assert!(error.to_string().contains("nested below regular file"));
    }
}
