//! Execution provenance — the single source of truth for which engine,
//! workspace, project source, and lifeline belong to an execution.
//!
//! This replaces the previous ad-hoc callback/dispatch fields that had
//! to be reconstructed at each layer. A provenance value is constructed
//! at an entry point and then flows through dispatch, runner, native
//! launch, and callback token minting unchanged. Callback children are
//! derived only by cloning this value as a borrowed child.

use std::path::PathBuf;
use std::sync::Arc;

use ryeos_engine::engine::Engine;

use crate::temp_dir_guard::TempDirGuard;

/// Which project source the execution is bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectSourceKind {
    LiveFs,
    PushedHead,
}

/// The execution's role in the snapshot lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionRole {
    Root,
    BorrowedCallbackChild,
}

/// Full provenance for one execution. Cloning is cheap; engine and
/// workspace lifeline identity are preserved through Arcs.
#[derive(Clone)]
pub struct ExecutionProvenance {
    pub role: ExecutionRole,
    pub project_source: ProjectSourceKind,
    pub request_engine: Arc<Engine>,
    pub original_project_path: PathBuf,
    pub effective_path: PathBuf,
    pub workspace_lifeline: Option<Arc<TempDirGuard>>,
    pub snapshot_hash: Option<String>,
}

impl ExecutionProvenance {
    /// Construct Root provenance for a live filesystem execution.
    pub fn root_live_fs(effective_path: PathBuf, engine: Arc<Engine>) -> Self {
        Self {
            role: ExecutionRole::Root,
            project_source: ProjectSourceKind::LiveFs,
            request_engine: engine,
            original_project_path: effective_path.clone(),
            effective_path,
            workspace_lifeline: None,
            snapshot_hash: None,
        }
    }

    /// Construct Root provenance for a pushed-head checkout.
    pub fn root_pushed_head(
        effective_path: PathBuf,
        original_project_path: PathBuf,
        engine: Arc<Engine>,
        workspace_lifeline: Arc<TempDirGuard>,
        snapshot_hash: String,
    ) -> Self {
        Self {
            role: ExecutionRole::Root,
            project_source: ProjectSourceKind::PushedHead,
            request_engine: engine,
            original_project_path,
            effective_path,
            workspace_lifeline: Some(workspace_lifeline),
            snapshot_hash: Some(snapshot_hash),
        }
    }

    /// Derive borrowed-callback-child provenance from this parent.
    pub fn clone_for_borrowed_child(&self) -> Self {
        Self {
            role: ExecutionRole::BorrowedCallbackChild,
            project_source: self.project_source.clone(),
            request_engine: self.request_engine.clone(),
            original_project_path: self.original_project_path.clone(),
            effective_path: self.effective_path.clone(),
            workspace_lifeline: self.workspace_lifeline.clone(),
            snapshot_hash: None,
        }
    }

    /// True iff this execution must not own snapshot pin/foldback.
    pub fn skips_snapshot_lifecycle(&self) -> bool {
        matches!(self.role, ExecutionRole::BorrowedCallbackChild)
    }
}

impl std::fmt::Debug for ExecutionProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionProvenance")
            .field("role", &self.role)
            .field("project_source", &self.project_source)
            .field("original_project_path", &self.original_project_path)
            .field("effective_path", &self.effective_path)
            .field("has_lifeline", &self.workspace_lifeline.is_some())
            .field("snapshot_hash", &self.snapshot_hash)
            .field("engine_arc_strong_count", &Arc::strong_count(&self.request_engine))
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
    fn root_live_fs_has_no_lifeline_or_snapshot() {
        let p = ExecutionProvenance::root_live_fs(PathBuf::from("/tmp/live"), engine());

        assert_eq!(p.role, ExecutionRole::Root);
        assert_eq!(p.project_source, ProjectSourceKind::LiveFs);
        assert!(p.workspace_lifeline.is_none());
        assert!(p.snapshot_hash.is_none());
        assert!(!p.skips_snapshot_lifecycle());
    }

    #[test]
    fn clone_for_borrowed_child_preserves_engine_and_lifeline_identity() {
        let engine = engine();
        let lifeline = Arc::new(TempDirGuard::new(std::env::temp_dir().join(
            "ryeos-provenance-test-preserve",
        )));
        let parent = ExecutionProvenance::root_pushed_head(
            PathBuf::from("/tmp/effective"),
            PathBuf::from("/tmp/original"),
            engine.clone(),
            lifeline.clone(),
            "abc".to_string(),
        );

        let child = parent.clone_for_borrowed_child();

        assert_eq!(child.role, ExecutionRole::BorrowedCallbackChild);
        assert_eq!(child.project_source, ProjectSourceKind::PushedHead);
        assert!(Arc::ptr_eq(&child.request_engine, &engine));
        assert!(Arc::ptr_eq(
            child.workspace_lifeline.as_ref().unwrap(),
            &lifeline,
        ));
        assert!(child.snapshot_hash.is_none());
        assert!(child.skips_snapshot_lifecycle());
    }
}
