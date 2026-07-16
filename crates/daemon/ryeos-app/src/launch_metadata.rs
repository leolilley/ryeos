//! Daemon-side parallel to the engine's `SubprocessSpec`: captures
//! everything the runner allocates / decides at spawn-time that the
//! engine does NOT own.
//!
//! Engine-known data (argv, env, cwd, execution decorations like
//! `native_async`) lives in `ryeos_engine::contracts::SubprocessSpec`.
//! Spawn-time-allocated data (cancellation policy snapshot for cancel
//! routing, checkpoint dir, original params, snapshot/base hash,
//! executor chain refs, vault references…) lives here.
//!
//! Persisted as a JSON blob in `runtime_db.thread_runtime.launch_metadata`
//! so the struct can be extended without schema migrations.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ryeos_engine::contracts::{
    CancellationMode, EffectivePrincipal, ExecutionHints, NativeResumeSpec, PlanSubprocessSpec,
    Principal, ProjectContext,
};
use serde::{Deserialize, Serialize};

use crate::execution_provenance::ExecutionProvenance;
use crate::thread_lifecycle::SealedRootExecutionRequest;

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Version tag for the JSON payload persisted into
/// `runtime_db.thread_runtime.launch_metadata`. Bump when an
/// breaking shape change ships; readers MUST decode loudly so a
/// schema mismatch surfaces in logs rather than silently disabling
/// downstream behaviors (see `runtime_db::get_runtime_info`).
pub const LAUNCH_METADATA_SCHEMA_VERSION: u32 = 1;

/// Per-thread daemon-owned state directory.
///
/// Holds artifacts the daemon must survive a restart with — most
/// notably the `checkpoints/` subdir written by replay-aware
/// subprocesses and read by the resume path.
///
/// Lives under `config.app_root` (daemon-owned, persistent, NOT in
/// CAS) rather than under the project working dir, because the
/// working dir is an ephemeral CAS checkout and fold-back skips
/// `state/` and dotfile paths.
///
/// Runtime-specific transcript writers own any project-relative output that
/// should fold back into CAS. This helper is only for daemon state that must
/// remain outside that fold-back boundary.
///
/// `RuntimeLaunchMetadata` and the resume-attempts counter both live
/// in `runtime_db.thread_runtime` — the daemon's runtime ledger,
/// which is intentionally separate from the CAS projection
/// (`state_db`) because it holds OS-level facts (pids, pgids,
/// runtime decisions) that have no CAS source to project from.
pub fn daemon_thread_state_dir(app_root: &std::path::Path, thread_id: &str) -> std::path::PathBuf {
    app_root.join("threads").join(thread_id)
}

/// Subdirectory of [`daemon_thread_state_dir`] holding a replay-aware
/// runtime's checkpoints. For call sites that already hold the thread state
/// dir; everyone else goes through [`daemon_checkpoint_dir`].
pub const CHECKPOINTS_SUBDIR: &str = "checkpoints";

/// Per-thread checkpoint directory for replay-aware runtimes, under
/// [`daemon_thread_state_dir`]. Every reader/writer of checkpoints
/// (allocation, machine-continuation copy-forward, follow-resume splice,
/// GC) must derive the path through here — scattered hand-joined spellings
/// of the same location are how state roots silently diverge.
pub fn daemon_checkpoint_dir(app_root: &std::path::Path, thread_id: &str) -> std::path::PathBuf {
    daemon_thread_state_dir(app_root, thread_id).join(CHECKPOINTS_SUBDIR)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeLaunchMetadata {
    /// Exact persisted schema version. Missing or older shapes fail decoding;
    /// there is no compatibility/default reader.
    pub schema_version: u32,

    /// Cancellation policy resolved at decorate-spec time and snapshotted
    /// here so the daemon can route cancellation without re-loading the spec.
    /// `None` = use the default 3s graceful shutdown.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub cancellation_mode: Option<CancellationMode>,

    /// Resume policy carried over from `SubprocessSpec.execution.native_resume`.
    /// Presence ⇒ this thread is replay-aware. The daemon allocates
    /// `checkpoint_dir`, injects `RYEOS_CHECKPOINT_DIR` into the spawn
    /// env, and on restart consults `reconcile.rs` for auto-resume.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub native_resume: Option<NativeResumeSpec>,

    /// Per-thread checkpoint directory (`<thread_state_dir>/checkpoints/`).
    /// Allocated by the daemon at spawn time when `native_resume` is set.
    /// Carried in the manifest so reconcile/resume paths can find it
    /// without rederiving paths.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub checkpoint_dir: Option<PathBuf>,

    /// Minimum context required to reconstruct the thread's launch identity —
    /// for `reconcile.rs` to re-spawn it under the same `thread_id` after a
    /// daemon restart, and for a continuation/follow-resume successor to
    /// relaunch it. Populated at managed launch time for continuation-capable
    /// OR native-resume launches. `None` for threads that are neither.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub resume_context: Option<ResumeContext>,

    /// Immutable source identity for a continuation runtime seed. Present only
    /// while/after a successor is created from a settled source and used by
    /// startup reconciliation to reject thread-id collisions.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub continuation_source_thread_id: Option<String>,

    /// Complete sealed fresh-root request used only while a created child has
    /// not crossed its first-launch boundary. Recovery consumes this exact
    /// authority and never re-resolves the item source.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub sealed_root_request: Option<SealedRootExecutionRequest>,

    /// Validated parent execution seed used when a detached follow child is
    /// admitted later, after the live callback context is gone.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub follow_parent_context: Option<PersistedParentExecutionContext>,

    /// Durable launch-window policy for crash repair before admission.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub follow_launch_window: Option<FollowLaunchWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FollowLaunchWindow {
    pub key: String,
    pub width: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedParentExecutionContext {
    pub parent_thread_id: String,
    pub hard_limits: serde_json::Value,
    pub depth: u32,
}

impl Default for RuntimeLaunchMetadata {
    fn default() -> Self {
        Self {
            schema_version: LAUNCH_METADATA_SCHEMA_VERSION,
            cancellation_mode: None,
            native_resume: None,
            checkpoint_dir: None,
            resume_context: None,
            continuation_source_thread_id: None,
            sealed_root_request: None,
            follow_parent_context: None,
            follow_launch_window: None,
        }
    }
}

/// Snapshot identity of a pushed-head original spawn — exactly what the
/// resume path needs to rebuild `ExecutionProvenance::root_pushed_head`
/// (re-materialize the pinned checkout, look up/build the per-snapshot
/// overlay engine, and key HEAD fold-back) without consulting the
/// principal's current HEAD, which may have advanced since spawn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalPushedHeadRef {
    /// CAS `ProjectSnapshot` hash the original spawn ran against.
    pub snapshot_hash: String,
    /// Operator-side absolute project path (the HEAD-ref key) captured
    /// from the root provenance at spawn.
    pub original_project_path: PathBuf,
}

impl OriginalPushedHeadRef {
    /// Derive the persistable pushed-head identity from a launch's
    /// provenance. `Some` only for a root pushed-head spawn: borrowed
    /// children never own snapshot lineage (rebuilding them as pushed
    /// roots would turn on pin/foldback/HEAD-advance their parent owns),
    /// and live-fs spawns have no snapshot to pin.
    pub fn from_provenance(provenance: &ExecutionProvenance) -> Option<Self> {
        match provenance {
            ExecutionProvenance::RootPushedHead {
                original_project_path,
                snapshot_hash,
                ..
            } => Some(Self {
                snapshot_hash: snapshot_hash.clone(),
                original_project_path: original_project_path.clone(),
            }),
            ExecutionProvenance::RootLiveFs { .. }
            | ExecutionProvenance::BorrowedChildLiveFs { .. }
            | ExecutionProvenance::BorrowedChildPushedHead { .. } => None,
        }
    }
}

/// Minimum data the daemon needs to reconstruct an `ExecutionParams`
/// for an existing thread.
///
/// **Pinned-snapshot resume policy:** `project_context` is captured
/// at original spawn time and reused verbatim on resume. If the
/// daemon allocated a base snapshot at spawn (the
/// `prepare_cas_context` `base_snapshot_hash`), it is persisted as
/// `original_snapshot_hash`. The reconciler prefers the snapshot hash
/// over the live LocalPath when reconstructing the resume request, so
/// resume runs against the project version that was current at the
/// time the checkpoint was written, NOT the current head of the
/// working directory. See `docs/future/native-resume-snapshot-pinning.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumeContext {
    pub kind: String,
    pub item_ref: String,
    /// Complete canonical secondary execution identity. Required on disk;
    /// absence is a schema error, never an empty-map fallback.
    pub ref_bindings: BTreeMap<String, String>,
    pub launch_mode: String,
    pub parameters: serde_json::Value,
    /// Full engine `ProjectContext` from the original `PlanContext`.
    /// Carries enough information for the engine resolver to identify
    /// the project (LocalPath / SnapshotHash / ProjectRef).
    pub project_context: ProjectContext,
    /// Snapshot hash captured by the runner at original spawn time
    /// (`prepare_cas_context`'s `base_snapshot_hash`). When set, the
    /// reconciler prefers a `ProjectContext::SnapshotHash` form over
    /// the original LocalPath so resume targets the pinned project
    /// version, not the current head. `None` when the original spawn
    /// went through the live-FS path with no allocated snapshot.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub original_snapshot_hash: Option<String>,
    /// Pushed-head identity captured at original spawn time — set iff the
    /// spawn's provenance was `RootPushedHead`. The resume path uses it to
    /// rebuild the snapshot-scoped overlay engine + checkout instead of
    /// resolving against the daemon's live engine. `None` for LocalPath
    /// spawns. NOT interchangeable with `original_snapshot_hash`, which is
    /// the LocalPath native-resume pin allocated by the runner.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub original_pushed_head_ref: Option<OriginalPushedHeadRef>,
    /// Deliberate runtime state-root override captured at original spawn
    /// time (`/execute` `state_root`). Re-applied to the rebuilt provenance
    /// on resume so a crashed overridden run keeps writing its state — and
    /// advertising its callback identity — under the override instead of
    /// silently reverting into the source project.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub state_root: Option<std::path::PathBuf>,
    pub current_site_id: String,
    pub origin_site_id: String,
    /// Full engine principal from the original `PlanContext`.
    /// Carries scopes / delegation envelope so the resumed thread is
    /// re-planned under the same principal that launched it.
    pub requested_by: EffectivePrincipal,
    /// `ExecutionHints` from the original `PlanContext`. Carried
    /// verbatim so executor-specific flags survive resume.
    pub execution_hints: ExecutionHints,
    /// Composed `effective_caps` captured at original spawn time. The
    /// reconciler re-mints a callback token for the resumed subprocess and
    /// the daemon enforces caps on every callback dispatch — this set is
    /// what gets enforced. Empty `Vec` means deny-all.
    ///
    /// Overload: on an *unlaunched follow-child root row* this instead
    /// carries the PARENT's effective caps — the bounding authority the
    /// launcher feeds to `CapabilityPolicy::FollowChildHybrid`. The child's
    /// own composed caps overwrite it in launch metadata once the child is
    /// launched and policy resolution succeeds.
    pub effective_caps: Vec<String>,
    /// Persisted executor identity (`native:<binary>`) of the runtime that
    /// launched this thread. Runtime-registry (delegate) kinds — directive,
    /// graph — carry no item `executor_id`, so a continuation successor
    /// reconstructs its launch identity from this. Captured at fresh managed
    /// launch; preferred over re-deriving from the registry so a later default
    /// change cannot silently switch runtimes.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub executor_ref: Option<String>,
    /// Persisted canonical ref (`runtime:<name>`) of the runtime that launched
    /// this thread, so a successor reattaches by-ref rather than re-resolving
    /// the default for the kind.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub runtime_ref: Option<String>,
}

impl ResumeContext {
    /// Return the exact captured principal identifier for resumed planning and
    /// row authority. The captured `EffectivePrincipal` is exhaustive, so a
    /// resume never needs to invent a fallback identity.
    pub fn principal_identifier(&self) -> &str {
        match &self.requested_by {
            EffectivePrincipal::Local(Principal { fingerprint, .. }) => fingerprint,
            EffectivePrincipal::Delegated(delegated) => &delegated.caller_fingerprint,
        }
    }

    /// Snapshot that can reconstruct this project as a fresh, non-lineage
    /// workspace. A locally pinned snapshot wins; a pushed-root snapshot is an
    /// equivalent immutable source when a borrowed child must not inherit the
    /// parent's pushed-head ownership semantics.
    pub fn durable_project_snapshot_hash(&self) -> Option<&str> {
        self.original_snapshot_hash.as_deref().or_else(|| {
            self.original_pushed_head_ref
                .as_ref()
                .map(|pinned| pinned.snapshot_hash.as_str())
        })
    }

    /// Canonical project identity persisted into authoritative thread
    /// snapshots. This deliberately distinguishes every ProjectContext shape:
    /// local paths retain their path attribution and optional immutable launch
    /// pin, while remote/reference contexts must resolve to an immutable CAS
    /// snapshot before a continuation can be committed.
    pub(crate) fn authoritative_project_identity(
        &self,
    ) -> anyhow::Result<(Option<PathBuf>, Option<String>)> {
        match &self.project_context {
            ProjectContext::None => {
                if self.durable_project_snapshot_hash().is_some() {
                    anyhow::bail!("project_context none contradicts a durable project snapshot");
                }
                Ok((None, None))
            }
            ProjectContext::LocalPath { path } => {
                if self.original_pushed_head_ref.is_some() {
                    anyhow::bail!(
                        "local-path project context contradicts pushed-head project identity"
                    );
                }
                Ok((Some(path.clone()), self.original_snapshot_hash.clone()))
            }
            ProjectContext::SnapshotHash { hash } => {
                for pinned in [
                    self.original_snapshot_hash.as_deref(),
                    self.original_pushed_head_ref
                        .as_ref()
                        .map(|pushed| pushed.snapshot_hash.as_str()),
                ]
                .into_iter()
                .flatten()
                {
                    if pinned != hash.as_str() {
                        anyhow::bail!(
                            "snapshot project context {hash} contradicts durable launch pin {pinned}"
                        );
                    }
                }
                Ok((None, Some(hash.clone())))
            }
            ProjectContext::ProjectRef { .. } => {
                let original = self.original_snapshot_hash.as_deref();
                let pushed = self
                    .original_pushed_head_ref
                    .as_ref()
                    .map(|pushed| pushed.snapshot_hash.as_str());
                if let (Some(original), Some(pushed)) = (original, pushed) {
                    if original != pushed {
                        anyhow::bail!(
                            "project-ref continuation has contradictory immutable snapshot pins"
                        );
                    }
                }
                let pinned = original.or(pushed).ok_or_else(|| {
                    anyhow::anyhow!(
                        "project-ref continuation has no immutable resolved snapshot pin"
                    )
                })?;
                Ok((None, Some(pinned.to_owned())))
            }
        }
    }
}

impl RuntimeLaunchMetadata {
    /// Build metadata from a finalized engine `SubprocessSpec`.
    ///
    /// Captures the `native_async` cancellation policy and the
    /// `native_resume` policy, both of which are spec-level
    /// `DecorateSpec`-phase outputs the daemon needs to route
    /// shutdown / resume without re-loading the spec.
    ///
    /// `checkpoint_dir` is left `None` here — it's a daemon-allocated
    /// path that depends on the spawn-time thread state directory. The
    /// runner fills it in via [`Self::with_checkpoint_dir`] after
    /// allocation.
    pub fn from_spec(spec: &PlanSubprocessSpec) -> Self {
        Self {
            schema_version: LAUNCH_METADATA_SCHEMA_VERSION,
            cancellation_mode: spec
                .execution
                .native_async
                .as_ref()
                .map(|na| na.cancellation_mode),
            native_resume: spec.execution.native_resume.clone(),
            checkpoint_dir: None,
            resume_context: None,
            continuation_source_thread_id: None,
            sealed_root_request: None,
            follow_parent_context: None,
            follow_launch_window: None,
        }
    }

    /// True when this carries no spawn-time metadata — i.e. a wire caller (a UDS
    /// `runtime.attach_process` self-attach, which sends only thread/pid) let the
    /// fields default. `attach_process` uses this to avoid clobbering metadata
    /// already seeded on the row at spawn (resume context).
    pub fn is_empty(&self) -> bool {
        self.cancellation_mode.is_none()
            && self.native_resume.is_none()
            && self.checkpoint_dir.is_none()
            && self.resume_context.is_none()
            && self.continuation_source_thread_id.is_none()
            && self.sealed_root_request.is_none()
            && self.follow_parent_context.is_none()
            && self.follow_launch_window.is_none()
    }

    /// Set the daemon-allocated checkpoint directory.
    pub fn with_checkpoint_dir(mut self, dir: PathBuf) -> Self {
        self.checkpoint_dir = Some(dir);
        self
    }

    /// Set the replay-aware `native_resume` policy. Runtime-registry launches
    /// read it from the serving runtime's YAML; the subprocess path reads it
    /// from the spec.
    pub fn with_native_resume(mut self, spec: NativeResumeSpec) -> Self {
        self.native_resume = Some(spec);
        self
    }

    /// Set the resume context (origin params + project context) so
    /// `reconcile.rs` can re-spawn this thread after a daemon restart.
    pub fn with_resume_context(mut self, ctx: ResumeContext) -> Self {
        self.resume_context = Some(ctx);
        self
    }

    pub fn with_continuation_source(mut self, source_thread_id: impl Into<String>) -> Self {
        self.continuation_source_thread_id = Some(source_thread_id.into());
        self
    }

    /// Derive the durable launch seed for a continuation successor. Runtime
    /// policy and the reconstructable request identity survive the handoff;
    /// fresh-root-only admission records do not. A replay-aware successor gets
    /// its own checkpoint directory rather than inheriting the source path.
    pub fn for_continuation_successor(
        &self,
        source_thread_id: &str,
        checkpoint_dir: PathBuf,
    ) -> Self {
        Self {
            schema_version: self.schema_version,
            cancellation_mode: self.cancellation_mode,
            native_resume: self.native_resume.clone(),
            checkpoint_dir: self.native_resume.as_ref().map(|_| checkpoint_dir),
            resume_context: self.resume_context.clone(),
            continuation_source_thread_id: Some(source_thread_id.to_string()),
            sealed_root_request: None,
            follow_parent_context: None,
            follow_launch_window: None,
        }
    }

    pub fn with_sealed_root_request(mut self, request: SealedRootExecutionRequest) -> Self {
        self.sealed_root_request = Some(request);
        self
    }

    /// True iff the spec declared `native_resume`. NOTE: this is a
    /// pure factual check on the persisted spec — actual reconciler
    /// eligibility additionally requires `resume_context.is_some()`,
    /// and lives in `reconcile::decide_resume`.
    pub fn declares_native_resume(&self) -> bool {
        self.native_resume.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::contracts::{ExecutionDecorations, NativeAsyncSpec};
    use std::collections::HashMap;

    fn empty_spec() -> PlanSubprocessSpec {
        PlanSubprocessSpec {
            cmd: "/bin/true".to_string(),
            verified_command: None,
            args: Vec::new(),
            cwd: None,
            env: HashMap::new(),
            env_sources: HashMap::new(),
            stdin_data: None,
            timeout_secs: 60,
            execution: ExecutionDecorations::default(),
        }
    }

    fn local_principal() -> EffectivePrincipal {
        EffectivePrincipal::Local(Principal {
            fingerprint: "fp:test".to_string(),
            scopes: vec!["execute".to_string()],
        })
    }

    fn local_path_ctx() -> ProjectContext {
        ProjectContext::LocalPath {
            path: PathBuf::from("/tmp/proj"),
        }
    }

    fn resume_context(project_context: ProjectContext) -> ResumeContext {
        ResumeContext {
            kind: "tool_run".to_string(),
            item_ref: "tool:test/run".to_string(),
            ref_bindings: BTreeMap::new(),
            launch_mode: "detached".to_string(),
            parameters: serde_json::json!({}),
            project_context,
            original_snapshot_hash: None,
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            requested_by: local_principal(),
            execution_hints: ExecutionHints::default(),
            effective_caps: Vec::new(),
            executor_ref: Some("native:test".to_string()),
            runtime_ref: None,
        }
    }

    #[test]
    fn from_spec_no_native_async_yields_none() {
        let m = RuntimeLaunchMetadata::from_spec(&empty_spec());
        assert!(m.cancellation_mode.is_none());
        assert!(m.checkpoint_dir.is_none());
        assert_eq!(m.schema_version, LAUNCH_METADATA_SCHEMA_VERSION);
    }

    #[test]
    fn from_spec_native_async_hard_propagates() {
        let mut spec = empty_spec();
        spec.execution.native_async = Some(NativeAsyncSpec {
            cancellation_mode: CancellationMode::Hard,
        });
        let m = RuntimeLaunchMetadata::from_spec(&spec);
        assert_eq!(m.cancellation_mode, Some(CancellationMode::Hard));
    }

    #[test]
    fn from_spec_native_async_graceful_propagates() {
        let mut spec = empty_spec();
        spec.execution.native_async = Some(NativeAsyncSpec {
            cancellation_mode: CancellationMode::Graceful { grace_secs: 12 },
        });
        let m = RuntimeLaunchMetadata::from_spec(&spec);
        assert_eq!(
            m.cancellation_mode,
            Some(CancellationMode::Graceful { grace_secs: 12 })
        );
    }

    #[test]
    fn json_roundtrip_preserves_fields() {
        let m = RuntimeLaunchMetadata {
            schema_version: LAUNCH_METADATA_SCHEMA_VERSION,
            cancellation_mode: Some(CancellationMode::Graceful { grace_secs: 7 }),
            native_resume: None,
            checkpoint_dir: Some(PathBuf::from("/tmp/ckpt")),
            resume_context: None,
            continuation_source_thread_id: None,
            sealed_root_request: None,
            follow_parent_context: None,
            follow_launch_window: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: RuntimeLaunchMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            serde_json::to_value(&back).unwrap()
        );
    }

    #[test]
    fn json_roundtrip_default_emits_schema_version() {
        let m = RuntimeLaunchMetadata::default();
        let json = serde_json::to_string(&m).unwrap();
        let back: RuntimeLaunchMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, LAUNCH_METADATA_SCHEMA_VERSION);
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            serde_json::to_value(&back).unwrap()
        );
    }

    #[test]
    fn from_spec_native_resume_propagates() {
        use ryeos_engine::contracts::NativeResumeSpec;
        let mut spec = empty_spec();
        spec.execution.native_resume = Some(NativeResumeSpec {
            checkpoint_interval_secs: 60,
            max_auto_resume_attempts: 3,
        });
        let m = RuntimeLaunchMetadata::from_spec(&spec);
        let nr = m.native_resume.as_ref().expect("native_resume");
        assert_eq!(nr.checkpoint_interval_secs, 60);
        assert_eq!(nr.max_auto_resume_attempts, 3);
        assert!(m.declares_native_resume());
    }

    #[test]
    fn declares_native_resume_false_without_native_resume() {
        let m = RuntimeLaunchMetadata::from_spec(&empty_spec());
        assert!(!m.declares_native_resume());
    }

    #[test]
    fn with_checkpoint_dir_assigns_path() {
        let m = RuntimeLaunchMetadata::default()
            .with_checkpoint_dir(PathBuf::from("/var/state/T-x/checkpoints"));
        assert_eq!(
            m.checkpoint_dir,
            Some(PathBuf::from("/var/state/T-x/checkpoints"))
        );
    }

    #[test]
    fn original_pushed_head_ref_derived_only_from_pushed_root_provenance() {
        use std::sync::Arc;

        let engine = || {
            Arc::new(ryeos_engine::engine::Engine::new(
                ryeos_engine::kind_registry::KindRegistry::empty(),
                ryeos_engine::parsers::dispatcher::ParserDispatcher::new(
                    ryeos_engine::parsers::registry::ParserRegistry::empty(),
                    Arc::new(ryeos_engine::handlers::registry::HandlerRegistry::empty()),
                ),
                vec![],
            ))
        };

        let live_root = ExecutionProvenance::root_live_fs(PathBuf::from("/live"), engine());
        assert!(OriginalPushedHeadRef::from_provenance(&live_root).is_none());
        assert!(
            OriginalPushedHeadRef::from_provenance(&live_root.clone_for_borrowed_child()).is_none()
        );

        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(crate::temp_dir_guard::TempDirGuard::new(
            dir.path().to_path_buf(),
        ));
        let pushed_root = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop/proj"),
            engine(),
            lifeline,
            "snap-abc".into(),
        );
        assert_eq!(
            OriginalPushedHeadRef::from_provenance(&pushed_root),
            Some(OriginalPushedHeadRef {
                snapshot_hash: "snap-abc".to_string(),
                original_project_path: PathBuf::from("/laptop/proj"),
            })
        );
        // A borrowed pushed child never owns the snapshot lineage.
        assert!(
            OriginalPushedHeadRef::from_provenance(&pushed_root.clone_for_borrowed_child())
                .is_none()
        );
    }

    #[test]
    fn daemon_thread_state_dir_is_under_app_root() {
        let dir = daemon_thread_state_dir(std::path::Path::new("/var/lib/ryeosd"), "T-abc");
        assert_eq!(dir, PathBuf::from("/var/lib/ryeosd/threads/T-abc"));
    }

    #[test]
    fn resume_context_full_roundtrip_through_metadata() {
        let ctx = ResumeContext {
            kind: "tool_run".to_string(),
            item_ref: "ns/foo".to_string(),
            ref_bindings: BTreeMap::new(),
            launch_mode: "detached".to_string(),
            parameters: serde_json::json!({"x": 1}),
            project_context: local_path_ctx(),
            original_snapshot_hash: Some("abc123".to_string()),
            original_pushed_head_ref: Some(OriginalPushedHeadRef {
                snapshot_hash: "snap-ph".to_string(),
                original_project_path: PathBuf::from("/tmp/orig"),
            }),
            state_root: Some(PathBuf::from("/tmp/smoke-state")),
            current_site_id: "site:a".to_string(),
            origin_site_id: "site:a".to_string(),
            requested_by: local_principal(),
            execution_hints: ExecutionHints::default(),
            effective_caps: vec!["ryeos.execute.tool.test".to_string()],
            executor_ref: Some("native:test-runtime".to_string()),
            runtime_ref: Some("runtime:test".to_string()),
        };
        let m = RuntimeLaunchMetadata::default().with_resume_context(ctx);
        let json = serde_json::to_string(&m).unwrap();
        let back: RuntimeLaunchMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            serde_json::to_value(&back).unwrap()
        );
        let back_ctx = back.resume_context.expect("resume_context");
        assert_eq!(back_ctx.kind, "tool_run");
        assert_eq!(back_ctx.item_ref, "ns/foo");
        assert_eq!(back_ctx.original_snapshot_hash.as_deref(), Some("abc123"));
        assert_eq!(
            back_ctx.original_pushed_head_ref,
            Some(OriginalPushedHeadRef {
                snapshot_hash: "snap-ph".to_string(),
                original_project_path: PathBuf::from("/tmp/orig"),
            })
        );
        match back_ctx.requested_by {
            EffectivePrincipal::Local(p) => {
                assert_eq!(p.fingerprint, "fp:test");
                assert_eq!(p.scopes, vec!["execute".to_string()]);
            }
            _ => panic!("expected Local principal"),
        }
        match back_ctx.project_context {
            ProjectContext::LocalPath { path } => {
                assert_eq!(path, PathBuf::from("/tmp/proj"));
            }
            _ => panic!("expected LocalPath"),
        }
        // V5.5 P2: effective_caps survive resume serialization so the
        // reconciler restores the same daemon-enforced cap set.
        assert_eq!(
            back_ctx.effective_caps,
            vec!["ryeos.execute.tool.test".to_string()]
        );
    }

    #[test]
    fn authoritative_project_identity_none_is_explicitly_empty() {
        let context = resume_context(ProjectContext::None);
        assert_eq!(
            context.authoritative_project_identity().unwrap(),
            (None, None)
        );

        let mut contradictory = context;
        contradictory.original_snapshot_hash = Some("a".repeat(64));
        assert!(contradictory.authoritative_project_identity().is_err());
    }

    #[test]
    fn authoritative_project_identity_local_path_carries_path_and_optional_pin() {
        let mut context = resume_context(ProjectContext::LocalPath {
            path: PathBuf::from("/work/project"),
        });
        context.original_snapshot_hash = Some("b".repeat(64));
        assert_eq!(
            context.authoritative_project_identity().unwrap(),
            (Some(PathBuf::from("/work/project")), Some("b".repeat(64)))
        );
    }

    #[test]
    fn authoritative_project_identity_snapshot_hash_is_the_pin() {
        let hash = "c".repeat(64);
        let context = resume_context(ProjectContext::SnapshotHash { hash: hash.clone() });
        assert_eq!(
            context.authoritative_project_identity().unwrap(),
            (None, Some(hash))
        );

        let mut contradictory = context;
        contradictory.original_snapshot_hash = Some("d".repeat(64));
        assert!(contradictory.authoritative_project_identity().is_err());
    }

    #[test]
    fn authoritative_project_identity_project_ref_requires_resolved_pin() {
        let mut context = resume_context(ProjectContext::ProjectRef {
            principal: "fp:test".to_string(),
            ref_name: "projects/demo".to_string(),
        });
        assert!(context.authoritative_project_identity().is_err());

        context.original_snapshot_hash = Some("e".repeat(64));
        assert_eq!(
            context.authoritative_project_identity().unwrap(),
            (None, Some("e".repeat(64)))
        );
    }
}
