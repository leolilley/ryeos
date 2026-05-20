//! Execution provenance — single source of truth, type-state encoded.
//!
//! A provenance value is constructed exactly once at the entry point
//! (HTTP route, SSE launch, scheduler tick, callback handler, resume
//! reconciler). It flows through dispatch, runner, native launch, and
//! callback token minting unchanged. Callback children are derived only
//! by cloning this value as a borrowed child.
//!
//! The four variants enumerate the four legal shapes. Invalid shapes
//! (for example, "Root PushedHead without lifeline" or "borrowed child
//! with snapshot hash") do not compile.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ryeos_engine::engine::Engine;

use crate::temp_dir_guard::TempDirGuard;

/// Project source dimension. Used only as an accessor return type for
/// capability checks and tracing. The enum is not stored as a field on
/// `ExecutionProvenance`; it is derived from the variant tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectSourceKind {
    LiveFs,
    PushedHead,
}

#[derive(Clone)]
#[doc(hidden)]
pub struct ProvenanceSeal(());

/// Single source of truth for what engine, workspace, lineage, and role
/// belong to an execution.
///
/// Construct via `root_live_fs` / `root_pushed_head`, or derive callback
/// children via `clone_for_borrowed_child`. The private seal field on
/// each variant prevents construction outside this module while still
/// permitting explicit variant matching by consumers.
#[derive(Clone)]
pub enum ExecutionProvenance {
    /// Top-level run against the live filesystem. No CAS temp dir to
    /// pin and no snapshot lineage to fold back.
    RootLiveFs {
        request_engine: Arc<Engine>,
        /// Live project root. Doubles as effective_path and
        /// original_project_path.
        project_path: PathBuf,
        __seal: ProvenanceSeal,
    },

    /// Top-level run against a pushed CAS snapshot. Owns the snapshot
    /// lineage (pin + foldback) and pins the materialized checkout dir
    /// via `workspace_lifeline`.
    RootPushedHead {
        request_engine: Arc<Engine>,
        /// Operator-side absolute path (HEAD-ref key).
        original_project_path: PathBuf,
        /// Daemon-side temp checkout the execution runs against. Must
        /// equal `workspace_lifeline.path()`.
        effective_path: PathBuf,
        workspace_lifeline: Arc<TempDirGuard>,
        snapshot_hash: String,
        __seal: ProvenanceSeal,
    },

    /// Callback child of a `RootLiveFs` parent. The child runs in the
    /// parent's live tree. No lifeline, no snapshot lineage.
    BorrowedChildLiveFs {
        request_engine: Arc<Engine>,
        project_path: PathBuf,
        __seal: ProvenanceSeal,
    },

    /// Callback child of a `RootPushedHead` parent (or another borrowed
    /// pushed child). Inherits engine, paths, and lifeline. It never
    /// carries the snapshot hash: the root owns lineage.
    BorrowedChildPushedHead {
        request_engine: Arc<Engine>,
        original_project_path: PathBuf,
        effective_path: PathBuf,
        workspace_lifeline: Arc<TempDirGuard>,
        __seal: ProvenanceSeal,
    },
}

impl ExecutionProvenance {
    /// Construct Root provenance for a live filesystem execution.
    pub fn root_live_fs(project_path: PathBuf, request_engine: Arc<Engine>) -> Self {
        Self::RootLiveFs {
            request_engine,
            project_path,
            __seal: ProvenanceSeal(()),
        }
    }

    /// Construct Root provenance for a pushed-head checkout.
    ///
    /// # Panics
    ///
    /// Panics if `workspace_lifeline.path()` is `None` (disarmed) or
    /// does not equal `effective_path`. This is a programmer error and
    /// is surfaced at the construction site.
    pub fn root_pushed_head(
        effective_path: PathBuf,
        original_project_path: PathBuf,
        request_engine: Arc<Engine>,
        workspace_lifeline: Arc<TempDirGuard>,
        snapshot_hash: String,
    ) -> Self {
        match workspace_lifeline.path() {
            Some(p) if p == effective_path => {}
            Some(p) => panic!(
                "ExecutionProvenance::root_pushed_head: lifeline path {} \
                 does not match effective_path {} — caller mis-paired \
                 the temp dir guard",
                p.display(),
                effective_path.display(),
            ),
            None => panic!(
                "ExecutionProvenance::root_pushed_head: lifeline is \
                 disarmed — caller passed a TempDirGuard whose dir was \
                 already taken"
            ),
        }

        Self::RootPushedHead {
            request_engine,
            original_project_path,
            effective_path,
            workspace_lifeline,
            snapshot_hash,
            __seal: ProvenanceSeal(()),
        }
    }

    /// Derive borrowed-callback-child provenance from this parent.
    pub fn clone_for_borrowed_child(&self) -> Self {
        match self {
            Self::RootLiveFs {
                request_engine,
                project_path,
                ..
            }
            | Self::BorrowedChildLiveFs {
                request_engine,
                project_path,
                ..
            } => Self::BorrowedChildLiveFs {
                request_engine: request_engine.clone(),
                project_path: project_path.clone(),
                __seal: ProvenanceSeal(()),
            },
            Self::RootPushedHead {
                request_engine,
                original_project_path,
                effective_path,
                workspace_lifeline,
                ..
            }
            | Self::BorrowedChildPushedHead {
                request_engine,
                original_project_path,
                effective_path,
                workspace_lifeline,
                ..
            } => Self::BorrowedChildPushedHead {
                request_engine: request_engine.clone(),
                original_project_path: original_project_path.clone(),
                effective_path: effective_path.clone(),
                workspace_lifeline: workspace_lifeline.clone(),
                __seal: ProvenanceSeal(()),
            },
        }
    }

    /// The engine to use for resolution / verification / execution.
    pub fn request_engine(&self) -> &Arc<Engine> {
        match self {
            Self::RootLiveFs { request_engine, .. }
            | Self::RootPushedHead { request_engine, .. }
            | Self::BorrowedChildLiveFs { request_engine, .. }
            | Self::BorrowedChildPushedHead { request_engine, .. } => request_engine,
        }
    }

    /// The directory the execution runs against.
    pub fn effective_path(&self) -> &Path {
        match self {
            Self::RootLiveFs { project_path, .. }
            | Self::BorrowedChildLiveFs { project_path, .. } => project_path.as_path(),
            Self::RootPushedHead { effective_path, .. }
            | Self::BorrowedChildPushedHead { effective_path, .. } => effective_path.as_path(),
        }
    }

    /// The caller-side project root. For LiveFs variants this equals
    /// `effective_path()`.
    pub fn original_project_path(&self) -> &Path {
        match self {
            Self::RootLiveFs { project_path, .. }
            | Self::BorrowedChildLiveFs { project_path, .. } => project_path.as_path(),
            Self::RootPushedHead {
                original_project_path,
                ..
            }
            | Self::BorrowedChildPushedHead {
                original_project_path,
                ..
            } => original_project_path.as_path(),
        }
    }

    /// Project source dimension for capability gating and tracing.
    pub fn project_source(&self) -> ProjectSourceKind {
        match self {
            Self::RootLiveFs { .. } | Self::BorrowedChildLiveFs { .. } => {
                ProjectSourceKind::LiveFs
            }
            Self::RootPushedHead { .. } | Self::BorrowedChildPushedHead { .. } => {
                ProjectSourceKind::PushedHead
            }
        }
    }

    /// True iff this execution must skip pin + foldback because a root
    /// parent owns the snapshot lifecycle.
    ///
    /// Written as an exhaustive 4-arm `match` (not `matches!`) so that
    /// adding a future fifth variant is a compile error here. The
    /// runner's lifecycle gates depend on this predicate; a silent
    /// default would skip or duplicate pin/foldback for the new role.
    pub fn is_borrowed_child(&self) -> bool {
        match self {
            Self::RootLiveFs { .. } | Self::RootPushedHead { .. } => false,
            Self::BorrowedChildLiveFs { .. } | Self::BorrowedChildPushedHead { .. } => true,
        }
    }
}

impl std::fmt::Debug for ExecutionProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionProvenance")
            .field(
                "role",
                &if self.is_borrowed_child() {
                    "BorrowedCallbackChild"
                } else {
                    "Root"
                },
            )
            .field("project_source", &self.project_source())
            .field("original_project_path", &self.original_project_path())
            .field("effective_path", &self.effective_path())
            .field(
                "has_lifeline",
                &match self {
                    Self::RootLiveFs { .. } | Self::BorrowedChildLiveFs { .. } => false,
                    Self::RootPushedHead { .. } | Self::BorrowedChildPushedHead { .. } => true,
                },
            )
            .field(
                "snapshot_hash",
                &match self {
                    Self::RootPushedHead { snapshot_hash, .. } => Some(snapshot_hash.as_str()),
                    Self::RootLiveFs { .. }
                    | Self::BorrowedChildLiveFs { .. }
                    | Self::BorrowedChildPushedHead { .. } => None,
                },
            )
            .field("engine_arc_strong_count", &Arc::strong_count(self.request_engine()))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Arc<Engine> {
        Arc::new(Engine::new(
            ryeos_engine::kind_registry::KindRegistry::empty(),
            ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                ryeos_engine::parsers::registry::ParserRegistry::empty(),
                Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
            ),
            None,
            vec![],
        ))
    }

    #[test]
    fn root_live_fs_constructor_sets_paths_equal() {
        let p = ExecutionProvenance::root_live_fs(PathBuf::from("/live"), engine());

        assert_eq!(p.effective_path(), Path::new("/live"));
        assert_eq!(p.original_project_path(), Path::new("/live"));
        assert!(matches!(p, ExecutionProvenance::RootLiveFs { .. }));
        assert_eq!(p.project_source(), ProjectSourceKind::LiveFs);
        assert!(!p.is_borrowed_child());
    }

    #[test]
    fn root_pushed_head_constructor_succeeds_with_matching_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let lifeline = Arc::new(TempDirGuard::new(path.clone()));

        let p = ExecutionProvenance::root_pushed_head(
            path.clone(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "snap".into(),
        );

        assert!(matches!(p, ExecutionProvenance::RootPushedHead { .. }));
        assert_eq!(p.effective_path(), path.as_path());
        assert_eq!(p.original_project_path(), Path::new("/laptop"));
        assert_eq!(p.project_source(), ProjectSourceKind::PushedHead);
    }

    #[test]
    #[should_panic(expected = "does not match effective_path")]
    fn root_pushed_head_constructor_panics_on_lifeline_path_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));

        ExecutionProvenance::root_pushed_head(
            PathBuf::from("/somewhere/else"),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "snap".into(),
        );
    }

    #[test]
    #[should_panic(expected = "disarmed")]
    fn root_pushed_head_constructor_panics_on_disarmed_lifeline() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        lifeline.disarm();

        ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "snap".into(),
        );
    }

    #[test]
    fn clone_for_borrowed_child_from_live_fs_root_produces_borrowed_live_fs() {
        let parent = ExecutionProvenance::root_live_fs(PathBuf::from("/live"), engine());
        let child = parent.clone_for_borrowed_child();

        assert!(matches!(child, ExecutionProvenance::BorrowedChildLiveFs { .. }));
        assert!(child.is_borrowed_child());
        assert_eq!(child.project_source(), ProjectSourceKind::LiveFs);
        assert_eq!(child.effective_path(), Path::new("/live"));
    }

    #[test]
    fn clone_for_borrowed_child_from_pushed_root_produces_borrowed_pushed() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let parent = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "snap".into(),
        );

        let child = parent.clone_for_borrowed_child();

        assert!(matches!(
            child,
            ExecutionProvenance::BorrowedChildPushedHead { .. }
        ));
        assert!(child.is_borrowed_child());
        assert_eq!(child.project_source(), ProjectSourceKind::PushedHead);
        assert_eq!(child.original_project_path(), Path::new("/laptop"));
    }

    #[test]
    fn clone_for_borrowed_child_preserves_engine_arc_identity() {
        let eng = engine();
        let parent = ExecutionProvenance::root_live_fs(PathBuf::from("/x"), eng.clone());
        let child = parent.clone_for_borrowed_child();

        assert!(Arc::ptr_eq(parent.request_engine(), child.request_engine()));
        assert!(Arc::ptr_eq(child.request_engine(), &eng));
    }

    #[test]
    fn clone_for_borrowed_child_preserves_lifeline_arc_identity_through_nesting() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let root = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline.clone(),
            "snap".into(),
        );
        let child = root.clone_for_borrowed_child();
        let grandchild = child.clone_for_borrowed_child();

        let extract = |p: &ExecutionProvenance| -> Arc<TempDirGuard> {
            match p {
                ExecutionProvenance::RootPushedHead {
                    workspace_lifeline,
                    ..
                }
                | ExecutionProvenance::BorrowedChildPushedHead {
                    workspace_lifeline,
                    ..
                } => workspace_lifeline.clone(),
                _ => panic!("expected pushed variant"),
            }
        };
        let l_root = extract(&root);
        let l_child = extract(&child);
        let l_grand = extract(&grandchild);

        assert!(Arc::ptr_eq(&l_root, &l_child));
        assert!(Arc::ptr_eq(&l_child, &l_grand));
        assert!(Arc::ptr_eq(&l_root, &lifeline));
    }

    #[test]
    fn is_borrowed_child_true_only_for_borrowed_variants() {
        let eng = engine();
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));

        let live_root = ExecutionProvenance::root_live_fs(PathBuf::from("/x"), eng.clone());
        let pushed_root = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/y"),
            eng,
            lifeline,
            "snap".into(),
        );
        let live_child = live_root.clone_for_borrowed_child();
        let pushed_child = pushed_root.clone_for_borrowed_child();

        assert!(!live_root.is_borrowed_child());
        assert!(!pushed_root.is_borrowed_child());
        assert!(live_child.is_borrowed_child());
        assert!(pushed_child.is_borrowed_child());
    }

    #[test]
    fn borrowed_pushed_child_has_no_snapshot_hash_field() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let root = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "snap".into(),
        );

        match root.clone_for_borrowed_child() {
            ExecutionProvenance::BorrowedChildPushedHead { .. } => {}
            other => panic!("expected BorrowedChildPushedHead, got {other:?}"),
        }
    }

    #[test]
    fn root_pushed_head_carries_snapshot_hash_only_on_root_variant() {
        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(TempDirGuard::new(dir.path().to_path_buf()));
        let root = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop"),
            engine(),
            lifeline,
            "snap".into(),
        );

        match &root {
            ExecutionProvenance::RootPushedHead { snapshot_hash, .. } => {
                assert_eq!(snapshot_hash, "snap");
            }
            other => panic!("expected RootPushedHead, got {other:?}"),
        }
    }
}
