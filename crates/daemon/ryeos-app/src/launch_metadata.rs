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
//! Persisted as an exact-epoch JSON blob in
//! `runtime_db.thread_runtime.launch_metadata`. Shape changes advance the
//! epoch and require the explicit owned-runtime reset; no compatibility
//! normalizer interprets predecessor authority.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context as _;
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

fn validate_canonical_capabilities(label: &str, capabilities: &[String]) -> anyhow::Result<()> {
    if capabilities.iter().any(|capability| {
        capability.trim() != capability
            || capability.is_empty()
            || capability.bytes().any(|byte| byte.is_ascii_control())
    }) {
        anyhow::bail!("{label} contain an empty, untrimmed, or control-bearing value");
    }
    let mut canonical = capabilities.to_vec();
    canonical.sort();
    canonical.dedup();
    if canonical != capabilities {
        anyhow::bail!("{label} are not sorted and de-duplicated");
    }
    Ok(())
}

/// Version tag for the JSON payload persisted into
/// `runtime_db.thread_runtime.launch_metadata`. Bump when an
/// breaking shape change ships; readers MUST decode loudly so a
/// schema mismatch surfaces in logs rather than silently disabling
/// downstream behaviors (see `runtime_db::get_runtime_info`).
// Exact durable launch metadata contract. Changes to any embedded authority
// shape require a new epoch so startup rejects the old store before nested
// deserialization can reinterpret (or partially decode) that authority.
pub const LAUNCH_METADATA_SCHEMA_VERSION: u32 = 12;

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

    /// Exact executor boundary selected at admission. Recovery must dispatch
    /// from this field rather than inferring behavior from item kinds, optional
    /// refs, or canonical-ref prefixes.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub launch_driver: Option<ryeos_state::objects::ExecutionLaunchDriver>,

    /// Exact ownership/recovery authority for an in-process handler. Managed
    /// subprocess launches carry this inside `resume_context`; in-process
    /// handlers have no resume capsule and therefore require this dedicated,
    /// current-schema field.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub in_process_lifecycle_authority: Option<ryeos_state::objects::ExecutionLifecycleAuthority>,

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

    /// Project authority sealed into the immutable admitted-launch capsule.
    /// For a fresh root this equals `resume_context.project_authority`. A
    /// continuation keeps this birth authority exactly while its operational
    /// pinned-COW generation may advance in `resume_context`.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub admitted_project_authority: Option<ryeos_state::objects::ExecutionProjectAuthority>,

    /// Exact runtime/protocol/executable closure admitted before thread birth.
    /// Names remain diagnostic only; recovery must reproduce this identity
    /// byte-for-byte before it may spawn.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub admitted_artifact_identity: Option<ryeos_state::objects::AdmittedLaunchArtifactIdentity>,

    /// Exact wire schema of the CAS-rooted admitted launch capsule.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub admitted_launch_capsule_schema: Option<u32>,

    /// Exact secret-free managed launch-preparation result rooted in the
    /// admitted capsule. Runtime SQLite carries an operational copy only;
    /// recovery reads the authoritative value from CAS.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub admitted_prepared_launch: Option<serde_json::Value>,

    /// Validated parent execution seed used when a detached follow child is
    /// admitted later, after the live callback context is gone.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub follow_parent_context: Option<PersistedParentExecutionContext>,

    /// Durable launch-window policy for crash repair before admission.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub follow_launch_window: Option<FollowLaunchWindow>,

    /// Exact secret-free isolation generation and compiled-plan identity used
    /// at the spawn boundary. `None` only before isolation compilation or for
    /// execution paths that never launch a subprocess.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub isolation: Option<ryeos_engine::isolation::IsolationLaunchProvenance>,
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
            launch_driver: None,
            in_process_lifecycle_authority: None,
            cancellation_mode: None,
            native_resume: None,
            checkpoint_dir: None,
            resume_context: None,
            continuation_source_thread_id: None,
            sealed_root_request: None,
            admitted_project_authority: None,
            admitted_artifact_identity: None,
            admitted_launch_capsule_schema: None,
            admitted_prepared_launch: None,
            follow_parent_context: None,
            follow_launch_window: None,
            isolation: None,
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

/// Stable attribution and authorization identity for a project. This is never
/// an execution cwd and never grants permission to open a path on this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StableProjectIdentity {
    pub normalized_logical_key: String,
    pub origin_site: String,
    pub display_path: PathBuf,
}

impl StableProjectIdentity {
    pub fn from_path(path: &std::path::Path, origin_site: &str) -> anyhow::Result<Self> {
        if !path.is_absolute() {
            anyhow::bail!(
                "stable project identity path must be absolute: {}",
                path.display()
            );
        }
        let display = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("stable project identity path is not UTF-8"))?;
        if display.bytes().any(|byte| byte.is_ascii_control())
            || origin_site.is_empty()
            || origin_site.bytes().any(|byte| byte.is_ascii_control())
        {
            anyhow::bail!("stable project identity contains invalid control characters");
        }
        Ok(Self {
            normalized_logical_key: format!("{origin_site}:{display}"),
            origin_site: origin_site.to_string(),
            display_path: path.to_path_buf(),
        })
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let canonical = Self::from_path(&self.display_path, &self.origin_site)?;
        if canonical.normalized_logical_key != self.normalized_logical_key {
            anyhow::bail!("stable project identity logical key is not canonical");
        }
        Ok(())
    }
}

impl OriginalPushedHeadRef {
    /// Derive the persistable pushed-head identity from a launch's
    /// provenance. `Some` only for a root pushed-head spawn: borrowed
    /// children never own snapshot lineage (rebuilding them as pushed
    /// roots would turn on pin/foldback/HEAD-advance their parent owns),
    /// and live-fs spawns have no snapshot to pin.
    pub fn from_provenance(provenance: &ExecutionProvenance) -> Option<Self> {
        if !provenance.advances_project_head() {
            return None;
        }
        match provenance {
            ExecutionProvenance::RootPinnedGeneration {
                original_project_path,
                snapshot_hash,
                ..
            } => Some(Self {
                snapshot_hash: snapshot_hash.clone(),
                original_project_path: original_project_path.clone(),
            }),
            ExecutionProvenance::Projectless { .. }
            | ExecutionProvenance::RootLiveProject { .. }
            | ExecutionProvenance::ChildLiveProject { .. }
            | ExecutionProvenance::ChildPinnedGeneration { .. } => None,
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
/// working directory. See
/// `.ai/knowledge/ryeos/future/native-resume-snapshot-pinning.md`.
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
    /// Canonical typed execution authority. Paths and optional snapshot fields
    /// below are retained only as launch/reconstruction details and must agree
    /// with this value.
    pub project_authority: ryeos_state::objects::ExecutionProjectAuthority,
    /// Owner and restart contract sealed at admission. Response timing is not
    /// persisted because returning early never changes daemon ownership.
    pub lifecycle_authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    /// Stable logical identity. It survives materialization and resume and is
    /// never treated as an effective filesystem path.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub stable_project_identity: Option<StableProjectIdentity>,
    /// Admission-validated node-local root used only for local overlays such as
    /// `.env`. Remote attribution paths never populate this field.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub local_overlay_root: Option<PathBuf>,
    /// Snapshot hash captured by the runner at original spawn time
    /// (`prepare_cas_context`'s `base_snapshot_hash`). When set, the
    /// reconciler prefers a `ProjectContext::SnapshotHash` form over
    /// the original LocalPath so resume targets the pinned project
    /// version, not the current head. `None` when the original spawn
    /// went through the live-FS path with no allocated snapshot.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub original_snapshot_hash: Option<String>,
    /// Pushed-head identity captured at original spawn time — set iff the
    /// spawn's provenance was `RootPinnedGeneration`. The resume path uses it to
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
    /// Composed CHILD `effective_caps` captured at original spawn time. The
    /// reconciler re-mints a callback token for the resumed subprocess and
    /// the daemon enforces caps on every callback dispatch — this set is
    /// what gets enforced. Empty `Vec` means deny-all.
    ///
    pub effective_caps: Vec<String>,
    /// Delegating parent's effective capability ceiling for a follow/detached
    /// child. This is distinct from the child's own `effective_caps`; recovery
    /// must reapply the same parent bound without ever treating parent grants
    /// as capabilities held by the child.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub parent_delegation_caps: Option<Vec<String>>,
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

    /// Verify the only authority transition a running continuation may make.
    /// Ordinary successors inherit the complete admitted launch envelope. A
    /// pinned COW source may additionally advance to the exact terminal
    /// generation produced by that source; no other project, principal,
    /// capability, runtime, or invocation field may drift.
    pub(crate) fn validate_continuation_transition_from(
        &self,
        source: &Self,
        source_result_snapshot_hash: Option<&str>,
    ) -> anyhow::Result<()> {
        let transition = match source_result_snapshot_hash {
            Some(result_snapshot_hash) => {
                ryeos_state::objects::OperationalProjectAuthorityTransition::AdvancePinnedCowContinuation {
                    result_snapshot_hash,
                }
            }
            None => ryeos_state::objects::OperationalProjectAuthorityTransition::InheritContinuation,
        };
        let mut expected = source.clone();
        expected.project_authority = source
            .project_authority
            .transition_operational_generation(transition)?;
        if let Some(result_hash) = source_result_snapshot_hash {
            expected.original_snapshot_hash = Some(result_hash.to_string());
            expected.original_pushed_head_ref = None;
        }
        if self != &expected {
            anyhow::bail!(
                "continuation launch authority differs from the admitted source outside its declared pinned-generation transition"
            );
        }
        self.authoritative_project_identity()?;
        Ok(())
    }

    /// Canonical project identity persisted into authoritative thread
    /// snapshots. This deliberately distinguishes every ProjectContext shape:
    /// local paths retain their path attribution and optional immutable launch
    /// pin, while remote/reference contexts must resolve to an immutable CAS
    /// snapshot before a continuation can be committed.
    pub(crate) fn authoritative_project_identity(
        &self,
    ) -> anyhow::Result<(Option<PathBuf>, Option<String>)> {
        self.project_authority.validate()?;
        self.lifecycle_authority.validate()?;
        if let Some(identity) = &self.stable_project_identity {
            identity.validate()?;
        }
        match self.project_authority.environment() {
            ryeos_state::objects::EnvironmentAuthority::ProjectOverlay {
                source_identity, ..
            } => {
                let authority_root = self
                    .project_authority
                    .project_root_projection()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "project-overlay environment authority has no local project root"
                        )
                    })?;
                let overlay_root = self.local_overlay_root.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "project-overlay environment authority has no sealed local overlay root"
                    )
                })?;
                if overlay_root != authority_root {
                    anyhow::bail!(
                        "sealed local overlay root {} contradicts project authority root {}",
                        overlay_root.display(),
                        authority_root.display()
                    );
                }
                let expected_source = format!("dotenv:{}", overlay_root.join(".env").display());
                if source_identity != &expected_source {
                    anyhow::bail!(
                        "project-overlay source identity {source_identity:?} contradicts sealed root {}",
                        overlay_root.display()
                    );
                }
            }
            ryeos_state::objects::EnvironmentAuthority::None
            | ryeos_state::objects::EnvironmentAuthority::Vault { .. }
            | ryeos_state::objects::EnvironmentAuthority::Delegated { .. } => {
                if self.local_overlay_root.is_some() {
                    anyhow::bail!(
                        "launch authority without a project overlay carries a local overlay root"
                    );
                }
            }
        }
        match (&self.project_authority, &self.project_context) {
            (
                ryeos_state::objects::ExecutionProjectAuthority::Projectless { .. },
                ProjectContext::None,
            ) => {
                if self.stable_project_identity.is_some() {
                    anyhow::bail!(
                        "projectless execution authority cannot carry stable project identity"
                    );
                }
                if self.durable_project_snapshot_hash().is_some() {
                    anyhow::bail!(
                        "projectless execution authority cannot carry a project snapshot"
                    );
                }
                Ok((None, None))
            }
            (
                ryeos_state::objects::ExecutionProjectAuthority::LiveProject {
                    canonical_root, ..
                },
                ProjectContext::LocalPath { path },
            ) => {
                if path != canonical_root {
                    anyhow::bail!(
                        "live project context {} contradicts canonical authority root {}",
                        path.display(),
                        canonical_root.display()
                    );
                }
                let expected_identity =
                    StableProjectIdentity::from_path(canonical_root, &self.origin_site_id)?;
                match &self.stable_project_identity {
                    Some(identity) if identity == &expected_identity => {}
                    None if self.lifecycle_authority.recovery
                        != ryeos_state::objects::ExecutionRecoveryAuthority::RestartRecoverable => {
                    }
                    Some(_) => anyhow::bail!(
                        "stable project identity contradicts live project authority root"
                    ),
                    None => anyhow::bail!(
                        "restartable live project authority has no stable project identity"
                    ),
                }
                if self.durable_project_snapshot_hash().is_some() {
                    anyhow::bail!("live project authority contradicts immutable snapshot pin");
                }
                Ok((Some(canonical_root.clone()), None))
            }
            (
                ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                    display_path,
                    snapshot_hash,
                    ..
                },
                ProjectContext::LocalPath { .. } | ProjectContext::SnapshotHash { .. },
            ) => {
                if self.durable_project_snapshot_hash() != Some(snapshot_hash.as_str()) {
                    anyhow::bail!("pinned project authority contradicts durable launch snapshot");
                }
                Ok((display_path.clone(), Some(snapshot_hash.clone())))
            }
            _ => anyhow::bail!(
                "project_context {:?} has no exact immutable execution authority",
                self.project_context
            ),
        }
    }
}

impl RuntimeLaunchMetadata {
    pub fn lifecycle_authority(
        &self,
    ) -> anyhow::Result<Option<ryeos_state::objects::ExecutionLifecycleAuthority>> {
        match (
            self.launch_driver,
            self.in_process_lifecycle_authority,
            self.resume_context
                .as_ref()
                .map(|resume| resume.lifecycle_authority),
        ) {
            (
                Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler),
                Some(authority),
                None,
            ) => {
                authority.validate()?;
                Ok(Some(authority))
            }
            (Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler), _, _) => {
                anyhow::bail!("in-process launch has contradictory lifecycle authority")
            }
            (_, None, authority) => Ok(authority),
            (_, Some(_), _) => {
                anyhow::bail!("non-in-process launch carries in-process lifecycle authority")
            }
        }
    }

    /// Merge subprocess-produced attempt facts into launch authority already
    /// seeded at admission. Overlapping durable fields must agree exactly;
    /// missing attempt fields inherit the authoritative value. Isolation is an
    /// attempt fact and may be replaced by the newly compiled attempt.
    pub fn merge_for_process_attach(&self, attempt: &Self) -> anyhow::Result<Self> {
        if self.schema_version != LAUNCH_METADATA_SCHEMA_VERSION
            || attempt.schema_version != LAUNCH_METADATA_SCHEMA_VERSION
        {
            anyhow::bail!("cannot merge non-current launch metadata at process attach");
        }

        fn exact<T>(
            label: &str,
            authoritative: &Option<T>,
            attempt: &Option<T>,
        ) -> anyhow::Result<Option<T>>
        where
            T: Clone + Serialize,
        {
            if let (Some(authoritative), Some(attempt)) = (authoritative, attempt) {
                if serde_json::to_value(authoritative)? != serde_json::to_value(attempt)? {
                    anyhow::bail!("process attach changed admitted {label}");
                }
            }
            Ok(authoritative.clone().or_else(|| attempt.clone()))
        }

        let merged = Self {
            schema_version: LAUNCH_METADATA_SCHEMA_VERSION,
            launch_driver: exact("launch driver", &self.launch_driver, &attempt.launch_driver)?,
            in_process_lifecycle_authority: exact(
                "in-process lifecycle authority",
                &self.in_process_lifecycle_authority,
                &attempt.in_process_lifecycle_authority,
            )?,
            cancellation_mode: exact(
                "cancellation policy",
                &self.cancellation_mode,
                &attempt.cancellation_mode,
            )?,
            native_resume: exact(
                "native-resume policy",
                &self.native_resume,
                &attempt.native_resume,
            )?,
            checkpoint_dir: exact(
                "checkpoint directory",
                &self.checkpoint_dir,
                &attempt.checkpoint_dir,
            )?,
            resume_context: exact(
                "resume authority",
                &self.resume_context,
                &attempt.resume_context,
            )?,
            continuation_source_thread_id: exact(
                "continuation source",
                &self.continuation_source_thread_id,
                &attempt.continuation_source_thread_id,
            )?,
            sealed_root_request: exact(
                "sealed admitted request",
                &self.sealed_root_request,
                &attempt.sealed_root_request,
            )?,
            admitted_project_authority: exact(
                "admitted project authority",
                &self.admitted_project_authority,
                &attempt.admitted_project_authority,
            )?,
            admitted_artifact_identity: exact(
                "admitted launch artifacts",
                &self.admitted_artifact_identity,
                &attempt.admitted_artifact_identity,
            )?,
            admitted_launch_capsule_schema: exact(
                "admitted launch capsule schema",
                &self.admitted_launch_capsule_schema,
                &attempt.admitted_launch_capsule_schema,
            )?,
            admitted_prepared_launch: exact(
                "admitted prepared launch",
                &self.admitted_prepared_launch,
                &attempt.admitted_prepared_launch,
            )?,
            follow_parent_context: exact(
                "follow parent authority",
                &self.follow_parent_context,
                &attempt.follow_parent_context,
            )?,
            follow_launch_window: exact(
                "follow launch window",
                &self.follow_launch_window,
                &attempt.follow_launch_window,
            )?,
            isolation: attempt.isolation.clone().or_else(|| self.isolation.clone()),
        };
        merged.validate()?;
        Ok(merged)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.schema_version != LAUNCH_METADATA_SCHEMA_VERSION {
            anyhow::bail!(
                "launch metadata schema {} is not current schema {}",
                self.schema_version,
                LAUNCH_METADATA_SCHEMA_VERSION
            );
        }
        if self.launch_driver == Some(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
        {
            let lifecycle_authority = self.in_process_lifecycle_authority.ok_or_else(|| {
                anyhow::anyhow!("in-process handler launch metadata has no lifecycle authority")
            })?;
            lifecycle_authority.validate()?;
            if lifecycle_authority
                != ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE
            {
                anyhow::bail!(
                    "in-process handler lifecycle authority must be daemon-owned and non-recoverable"
                );
            }
            if self.cancellation_mode.is_some()
                || self.native_resume.is_some()
                || self.checkpoint_dir.is_some()
                || self.resume_context.is_some()
                || self.continuation_source_thread_id.is_some()
                || self.sealed_root_request.is_some()
                || self.admitted_project_authority.is_some()
                || self.admitted_artifact_identity.is_some()
                || self.admitted_launch_capsule_schema.is_some()
                || self.admitted_prepared_launch.is_some()
                || self.follow_parent_context.is_some()
                || self.follow_launch_window.is_some()
                || self.isolation.is_some()
            {
                anyhow::bail!(
                    "in-process handler launch metadata contains subprocess-only authority"
                );
            }
        } else if self.in_process_lifecycle_authority.is_some() {
            anyhow::bail!(
                "in-process lifecycle authority requires the in-process handler launch driver"
            );
        }
        if let Some(resume) = &self.resume_context {
            resume.authoritative_project_identity()?;
        }
        if self.resume_context.is_some() && self.launch_driver.is_none() {
            anyhow::bail!("recoverable launch metadata has no admitted launch driver");
        }
        if self.sealed_root_request.is_some() && self.resume_context.is_none() {
            anyhow::bail!("sealed launch metadata has no resume authority");
        }
        if self.sealed_root_request.is_some() && self.admitted_project_authority.is_none() {
            anyhow::bail!("sealed launch metadata has no immutable admitted project authority");
        }
        if self.sealed_root_request.is_some() && self.admitted_artifact_identity.is_none() {
            anyhow::bail!("sealed launch metadata has no immutable admitted artifact identity");
        }
        if self.sealed_root_request.is_none() && self.admitted_project_authority.is_some() {
            anyhow::bail!(
                "launch metadata has admitted project authority without a sealed request"
            );
        }
        if self.sealed_root_request.is_none() && self.admitted_artifact_identity.is_some() {
            anyhow::bail!("launch metadata has admitted artifacts without a sealed request");
        }
        if let Some(schema) = self.admitted_launch_capsule_schema {
            if schema != ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION {
                anyhow::bail!(
                    "admitted launch capsule is not the exact current contract: stored schema={schema}, current schema={}",
                    ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION
                );
            }
            if self.sealed_root_request.is_none() {
                anyhow::bail!("launch metadata has a capsule schema without a sealed request");
            }
        }
        if self.sealed_root_request.is_some() && self.admitted_launch_capsule_schema.is_none() {
            anyhow::bail!("sealed launch metadata has no admitted capsule schema");
        }
        if let Some(admitted) = &self.admitted_project_authority {
            admitted.validate()?;
        }
        if let Some(artifacts) = &self.admitted_artifact_identity {
            artifacts.validate()?;
            if self.launch_driver != Some(artifacts.launch_driver()) {
                anyhow::bail!("admitted artifact identity contradicts launch driver");
            }
        }
        if let Some(resume) = &self.resume_context {
            validate_canonical_capabilities("effective capabilities", &resume.effective_caps)?;
            if let Some(parent_caps) = &resume.parent_delegation_caps {
                validate_canonical_capabilities("parent delegation capabilities", parent_caps)?;
            } else if self.follow_parent_context.is_some() {
                anyhow::bail!(
                    "persisted parent execution context has no parent delegation capabilities"
                );
            }
        }
        if self.follow_launch_window.is_some() && self.follow_parent_context.is_none() {
            anyhow::bail!("follow launch window has no parent execution context");
        }
        if self.continuation_source_thread_id.is_some() && self.resume_context.is_none() {
            anyhow::bail!("continuation launch metadata has no resume authority");
        }
        if self.sealed_root_request.is_some() {
            self.admitted_launch_capsule()?;
        }
        Ok(())
    }

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
            // The plan describes the subprocess, not which admitted daemon
            // boundary owns it. The caller seals ManagedRuntime versus
            // DirectItemExecutor before thread birth.
            launch_driver: None,
            in_process_lifecycle_authority: None,
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
            admitted_project_authority: None,
            admitted_artifact_identity: None,
            admitted_launch_capsule_schema: None,
            admitted_prepared_launch: None,
            follow_parent_context: None,
            follow_launch_window: None,
            isolation: None,
        }
    }

    /// True when this carries no spawn-time metadata — i.e. a wire caller (a UDS
    /// `runtime.attach_process` self-attach, which sends only thread/pid) let the
    /// fields default. `attach_process` uses this to avoid clobbering metadata
    /// already seeded on the row at spawn (resume context).
    pub fn is_empty(&self) -> bool {
        self.cancellation_mode.is_none()
            && self.launch_driver.is_none()
            && self.in_process_lifecycle_authority.is_none()
            && self.native_resume.is_none()
            && self.checkpoint_dir.is_none()
            && self.resume_context.is_none()
            && self.continuation_source_thread_id.is_none()
            && self.sealed_root_request.is_none()
            && self.admitted_project_authority.is_none()
            && self.admitted_artifact_identity.is_none()
            && self.admitted_launch_capsule_schema.is_none()
            && self.admitted_prepared_launch.is_none()
            && self.follow_parent_context.is_none()
            && self.follow_launch_window.is_none()
            && self.isolation.is_none()
    }

    /// Build the immutable CAS launch closure at admission. The runtime ledger
    /// remains the operational copy; this object is the authoritative,
    /// chain-reachable identity used to prove recovery did not re-resolve
    /// mutable source. Spawn-attempt isolation provenance is deliberately not
    /// part of this object: it is compiled after birth and remains an audited
    /// attempt fact, while this capsule must be atomically rooted by birth.
    pub fn admitted_launch_capsule(
        &self,
    ) -> anyhow::Result<Option<ryeos_state::objects::AdmittedLaunchCapsule>> {
        let Some(sealed) = self.sealed_root_request.as_ref() else {
            return Ok(None);
        };
        let resume = self
            .resume_context
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sealed launch has no persisted resume authority"))?;
        if sealed.item_ref() != resume.item_ref
            || sealed.runtime_ref() != resume.runtime_ref.as_deref().unwrap_or_default()
            || sealed.executor_ref() != resume.executor_ref.as_deref().unwrap_or_default()
        {
            anyhow::bail!("sealed program and resume launch identity disagree");
        }
        if sealed.project_authority() != &resume.project_authority {
            anyhow::bail!("sealed invocation and resume project authority disagree");
        }
        let admitted_project_authority = self
            .admitted_project_authority
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sealed launch has no admitted project authority"))?;
        if self.continuation_source_thread_id.is_none()
            && sealed.project_authority() != admitted_project_authority
        {
            anyhow::bail!("fresh sealed invocation and admitted project authority disagree");
        }
        let mut effective_caps = resume.effective_caps.clone();
        effective_caps.sort();
        effective_caps.dedup();
        let capsule = ryeos_state::objects::AdmittedLaunchCapsule {
            schema: self
                .admitted_launch_capsule_schema
                .ok_or_else(|| anyhow::anyhow!("sealed launch has no admitted capsule schema"))?,
            kind: "admitted_launch_capsule".to_string(),
            exact_program: sealed.admitted_program_value()?,
            exact_program_hash: sealed.admitted_program_hash()?,
            sealed_invocation: serde_json::to_value(sealed)
                .context("serialize sealed admitted invocation")?,
            project_authority: admitted_project_authority.clone(),
            lifecycle_authority: resume.lifecycle_authority,
            launch_driver: self
                .launch_driver
                .ok_or_else(|| anyhow::anyhow!("sealed launch has no admitted launch driver"))?,
            artifact_identity: self.admitted_artifact_identity.clone().ok_or_else(|| {
                anyhow::anyhow!("sealed launch has no admitted artifact identity")
            })?,
            prepared_launch: self.admitted_prepared_launch.clone(),
            effective_caps,
            runtime_ref: sealed.runtime_ref().to_string(),
            executor_ref: sealed.executor_ref().to_string(),
        };
        capsule.validate()?;
        Ok(Some(capsule))
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

    pub fn with_launch_driver(
        mut self,
        driver: ryeos_state::objects::ExecutionLaunchDriver,
    ) -> Self {
        self.launch_driver = Some(driver);
        self
    }

    pub fn with_in_process_lifecycle_authority(
        mut self,
        authority: ryeos_state::objects::ExecutionLifecycleAuthority,
    ) -> Self {
        self.in_process_lifecycle_authority = Some(authority);
        self
    }

    pub fn with_admitted_artifact_identity(
        mut self,
        identity: ryeos_state::objects::AdmittedLaunchArtifactIdentity,
    ) -> Self {
        self.admitted_artifact_identity = Some(identity);
        self
    }

    pub fn with_admitted_prepared_launch(mut self, prepared: serde_json::Value) -> Self {
        self.admitted_prepared_launch = Some(prepared);
        self
    }

    pub fn with_continuation_source(mut self, source_thread_id: impl Into<String>) -> Self {
        self.continuation_source_thread_id = Some(source_thread_id.into());
        self
    }

    /// Derive the durable launch seed for a continuation successor. Runtime
    /// policy and the exact admitted program survive the handoff. A
    /// replay-aware successor gets its own checkpoint directory rather than
    /// inheriting the source path.
    pub fn for_continuation_successor(
        &self,
        source_thread_id: &str,
        checkpoint_dir: PathBuf,
    ) -> Self {
        Self {
            schema_version: self.schema_version,
            launch_driver: self.launch_driver,
            in_process_lifecycle_authority: self.in_process_lifecycle_authority,
            cancellation_mode: self.cancellation_mode,
            native_resume: self.native_resume.clone(),
            checkpoint_dir: self.native_resume.as_ref().map(|_| checkpoint_dir),
            resume_context: self.resume_context.clone(),
            continuation_source_thread_id: Some(source_thread_id.to_string()),
            sealed_root_request: self.sealed_root_request.clone(),
            admitted_project_authority: self.admitted_project_authority.clone(),
            admitted_artifact_identity: self.admitted_artifact_identity.clone(),
            admitted_launch_capsule_schema: self.admitted_launch_capsule_schema,
            admitted_prepared_launch: self.admitted_prepared_launch.clone(),
            follow_parent_context: None,
            follow_launch_window: None,
            // Isolation is compiled against one concrete spawn and verified
            // bundle generation. The successor receives fresh provenance when
            // its own launch plan is compiled.
            isolation: None,
        }
    }

    /// Project this thread's execution-level policy into a newly-created
    /// continuation successor.
    ///
    /// A continuation is another segment of the same managed execution, so its
    /// cancellation and native-resume declarations remain authoritative. The
    /// checkpoint directory and follow-child admission fields are owned by one
    /// concrete thread, however, and must never be copied to a different thread
    /// identity. Launch preparation allocates the successor's own checkpoint
    /// directory before it attaches.
    pub fn continuation_successor_seed(&self, resume_context: ResumeContext) -> Self {
        Self {
            launch_driver: self.launch_driver,
            in_process_lifecycle_authority: self.in_process_lifecycle_authority,
            cancellation_mode: self.cancellation_mode,
            native_resume: self.native_resume.clone(),
            resume_context: Some(resume_context),
            sealed_root_request: self.sealed_root_request.clone(),
            admitted_project_authority: self.admitted_project_authority.clone(),
            admitted_artifact_identity: self.admitted_artifact_identity.clone(),
            admitted_launch_capsule_schema: self.admitted_launch_capsule_schema,
            ..Self::default()
        }
    }

    pub fn with_sealed_root_request(mut self, request: SealedRootExecutionRequest) -> Self {
        self.set_sealed_root_request(request);
        self
    }

    pub fn set_sealed_root_request(&mut self, request: SealedRootExecutionRequest) {
        let first_seal = self.sealed_root_request.is_none();
        if self.admitted_project_authority.is_none() {
            self.admitted_project_authority = self
                .resume_context
                .as_ref()
                .map(|resume| resume.project_authority.clone());
        }
        if first_seal && self.admitted_launch_capsule_schema.is_none() {
            self.admitted_launch_capsule_schema =
                Some(ryeos_state::objects::ADMITTED_LAUNCH_CAPSULE_SCHEMA_VERSION);
        }
        self.sealed_root_request = Some(request);
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
    use ryeos_engine::contracts::{ExecutionDecorations, NativeAsyncSpec, NativeResumeSpec};
    use std::collections::HashMap;

    #[test]
    fn in_process_handler_metadata_has_exact_lifecycle_and_no_launch_capsule() {
        let metadata = RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
            .with_in_process_lifecycle_authority(
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
            );
        metadata.validate().unwrap();
        assert!(metadata.admitted_launch_capsule().unwrap().is_none());

        let mut contradictory = metadata;
        contradictory.admitted_project_authority =
            Some(ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS);
        let error = contradictory.validate().unwrap_err();
        assert!(error
            .to_string()
            .contains("contains subprocess-only authority"));
    }

    #[test]
    fn in_process_handler_metadata_rejects_missing_or_recoverable_lifecycle_authority() {
        let missing = RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler);
        assert!(missing
            .validate()
            .unwrap_err()
            .to_string()
            .contains("has no lifecycle authority"));

        let recoverable = RuntimeLaunchMetadata::default()
            .with_launch_driver(ryeos_state::objects::ExecutionLaunchDriver::InProcessHandler)
            .with_in_process_lifecycle_authority(
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            );
        let error = recoverable
            .validate()
            .expect_err("in-process handlers must never acquire replay authority");
        let message = error.to_string();
        assert!(message.contains("daemon-owned"));
        assert!(message.contains("non-recoverable"));

        let no_driver = RuntimeLaunchMetadata::default().with_in_process_lifecycle_authority(
            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_NON_RECOVERABLE,
        );
        assert!(no_driver
            .validate()
            .unwrap_err()
            .to_string()
            .contains("requires the in-process handler launch driver"));
    }

    #[test]
    fn launch_metadata_rejects_predecessor_schema_epoch() {
        let mut predecessor = RuntimeLaunchMetadata::default();
        predecessor.schema_version = LAUNCH_METADATA_SCHEMA_VERSION - 1;
        let error = predecessor.validate().unwrap_err();
        assert!(error.to_string().contains(&format!(
            "schema {} is not current schema {}",
            LAUNCH_METADATA_SCHEMA_VERSION - 1,
            LAUNCH_METADATA_SCHEMA_VERSION
        )));
    }

    #[test]
    fn launch_metadata_current_schema_requires_explicit_driver_and_in_process_authority_fields() {
        for field in [
            "launch_driver",
            "in_process_lifecycle_authority",
            "admitted_launch_capsule_schema",
        ] {
            let mut value = serde_json::to_value(RuntimeLaunchMetadata::default()).unwrap();
            value
                .as_object_mut()
                .expect("launch metadata object")
                .remove(field);
            assert!(
                serde_json::from_value::<RuntimeLaunchMetadata>(value).is_err(),
                "current launch metadata accepted missing field {field}"
            );
        }
    }

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
        let stable_project_identity = match &project_context {
            ProjectContext::LocalPath { path } => {
                Some(StableProjectIdentity::from_path(path, "site:test").unwrap())
            }
            _ => None,
        };
        let project_authority = match &project_context {
            ProjectContext::None | ProjectContext::ProjectRef { .. } => {
                ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS
            }
            ProjectContext::LocalPath { path } => {
                ryeos_state::objects::ExecutionProjectAuthority::live(
                    path.clone(),
                    format!("local:{}", path.display()),
                    ryeos_state::objects::LiveProjectAccess::ReadWrite,
                    ryeos_state::objects::LiveFilesystemConfinement::standard_descriptor_rooted(),
                    ryeos_state::objects::EnvironmentAuthority::None,
                    Vec::new(),
                )
                .unwrap()
            }
            ProjectContext::SnapshotHash { hash } => {
                ryeos_state::objects::ExecutionProjectAuthority::pinned(
                    format!("snapshot:{hash}"),
                    None,
                    hash.clone(),
                    ryeos_state::objects::PinnedProjectRealization::Cow {
                        terminal_publication:
                            ryeos_state::objects::PinnedTerminalPublication::Discard,
                    },
                    ryeos_state::objects::EnvironmentAuthority::None,
                    Vec::new(),
                )
                .unwrap()
            }
        };
        let original_snapshot_hash = project_authority
            .base_snapshot_projection()
            .map(str::to_owned);
        let lifecycle_authority =
            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE;
        ResumeContext {
            kind: "tool_run".to_string(),
            item_ref: "tool:test/run".to_string(),
            ref_bindings: BTreeMap::new(),
            launch_mode: "detached".to_string(),
            parameters: serde_json::json!({}),
            project_context,
            project_authority,
            lifecycle_authority,
            stable_project_identity,
            local_overlay_root: None,
            original_snapshot_hash,
            original_pushed_head_ref: None,
            state_root: None,
            current_site_id: "site:test".to_string(),
            origin_site_id: "site:test".to_string(),
            requested_by: local_principal(),
            execution_hints: ExecutionHints::default(),
            effective_caps: Vec::new(),
            parent_delegation_caps: None,
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
            launch_driver: None,
            in_process_lifecycle_authority: None,
            cancellation_mode: Some(CancellationMode::Graceful { grace_secs: 7 }),
            native_resume: None,
            checkpoint_dir: Some(PathBuf::from("/tmp/ckpt")),
            resume_context: None,
            continuation_source_thread_id: None,
            sealed_root_request: None,
            admitted_project_authority: None,
            admitted_artifact_identity: None,
            admitted_launch_capsule_schema: None,
            admitted_prepared_launch: None,
            follow_parent_context: None,
            follow_launch_window: None,
            isolation: None,
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
    fn continuation_successor_seed_preserves_policy_and_clears_thread_owned_state() {
        let native_resume = NativeResumeSpec {
            checkpoint_interval_secs: 17,
            max_auto_resume_attempts: 4,
        };
        let live_dir = tempfile::tempdir().unwrap();
        let source = RuntimeLaunchMetadata {
            cancellation_mode: Some(CancellationMode::Hard),
            native_resume: Some(native_resume.clone()),
            checkpoint_dir: Some(PathBuf::from("/state/threads/source/checkpoints")),
            resume_context: Some(resume_context(ProjectContext::LocalPath {
                path: live_dir.path().to_path_buf(),
            })),
            continuation_source_thread_id: Some("earlier-source".to_string()),
            follow_parent_context: Some(PersistedParentExecutionContext {
                parent_thread_id: "follow-parent".to_string(),
                hard_limits: serde_json::json!({"max_depth": 2}),
                depth: 1,
            }),
            follow_launch_window: Some(FollowLaunchWindow {
                key: "follow:source".to_string(),
                width: 2,
            }),
            ..RuntimeLaunchMetadata::default()
        };
        let successor_resume = resume_context(ProjectContext::SnapshotHash {
            hash: "a".repeat(64),
        });

        let successor = source.continuation_successor_seed(successor_resume.clone());

        assert_eq!(successor.cancellation_mode, Some(CancellationMode::Hard));
        assert_eq!(successor.native_resume, Some(native_resume));
        assert_eq!(successor.resume_context, Some(successor_resume));
        assert!(successor.checkpoint_dir.is_none());
        assert!(successor.continuation_source_thread_id.is_none());
        assert!(successor.sealed_root_request.is_none());
        assert!(successor.follow_parent_context.is_none());
        assert!(successor.follow_launch_window.is_none());
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

        let live_dir = tempfile::tempdir().unwrap();
        let live_authority = crate::execution_policy::resolve_standard_local_live_authority(
            live_dir.path(),
            vec![crate::execution_policy::LIVE_PROJECT_WRITE_CAPABILITY.to_string()],
            &ryeos_engine::isolation::IsolationRuntime::default(),
        )
        .unwrap()
        .project;
        let live_root = ExecutionProvenance::root_live_fs(
            live_dir.path().canonicalize().unwrap(),
            engine(),
            live_authority,
        )
        .unwrap();
        assert!(OriginalPushedHeadRef::from_provenance(&live_root).is_none());
        assert!(
            OriginalPushedHeadRef::from_provenance(&live_root.clone_for_borrowed_child()).is_none()
        );

        let dir = tempfile::tempdir().unwrap();
        let lifeline = Arc::new(crate::temp_dir_guard::TempDirGuard::new(
            dir.path().to_path_buf(),
        ));
        let snapshot_hash = "a".repeat(64);
        let pinned_authority = |terminal_publication| {
            ryeos_state::objects::ExecutionProjectAuthority::pinned(
                "site:test:/laptop/proj".to_string(),
                Some(PathBuf::from("/laptop/proj")),
                snapshot_hash.clone(),
                ryeos_state::objects::PinnedProjectRealization::Cow {
                    terminal_publication,
                },
                ryeos_state::objects::EnvironmentAuthority::None,
                Vec::new(),
            )
            .unwrap()
        };
        let retained_root = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop/proj"),
            engine(),
            lifeline.clone(),
            snapshot_hash.clone(),
            pinned_authority(ryeos_state::objects::PinnedTerminalPublication::RetainResult),
        )
        .unwrap();
        assert!(OriginalPushedHeadRef::from_provenance(&retained_root).is_none());
        let pushed_root = ExecutionProvenance::root_pushed_head(
            dir.path().to_path_buf(),
            PathBuf::from("/laptop/proj"),
            engine(),
            lifeline,
            snapshot_hash.clone(),
            pinned_authority(
                ryeos_state::objects::PinnedTerminalPublication::AdvanceHead {
                    head_ref: "refs/heads/main".to_string(),
                    expected_hash: snapshot_hash.clone(),
                },
            ),
        )
        .unwrap();
        assert_eq!(
            OriginalPushedHeadRef::from_provenance(&pushed_root),
            Some(OriginalPushedHeadRef {
                snapshot_hash,
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
            project_authority: ryeos_state::objects::ExecutionProjectAuthority::PinnedGeneration {
                stable_project_identity: "site:a:/tmp/proj".to_string(),
                display_path: Some(PathBuf::from("/tmp/proj")),
                snapshot_hash: "abc123".to_string(),
                realization: ryeos_state::objects::PinnedProjectRealization::Cow {
                    terminal_publication: ryeos_state::objects::PinnedTerminalPublication::Discard,
                },
                environment: ryeos_state::objects::EnvironmentAuthority::None,
                capability_ceiling: vec!["ryeos.execute.tool.test".to_string()],
                child_policy: ryeos_state::objects::ChildProjectAuthorityPolicy::Inherit,
            },
            lifecycle_authority:
                ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE,
            stable_project_identity: Some(
                StableProjectIdentity::from_path(std::path::Path::new("/tmp/proj"), "site:a")
                    .unwrap(),
            ),
            local_overlay_root: Some(PathBuf::from("/tmp/proj")),
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
            parent_delegation_caps: None,
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
        let live_dir = tempfile::tempdir().unwrap();
        let mut context = resume_context(ProjectContext::LocalPath {
            path: live_dir.path().to_path_buf(),
        });
        assert_eq!(
            context.authoritative_project_identity().unwrap(),
            (Some(live_dir.path().to_path_buf()), None)
        );
        context.original_snapshot_hash = Some("b".repeat(64));
        assert!(context.authoritative_project_identity().is_err());
    }

    #[test]
    fn live_restartable_authority_roundtrips_without_a_snapshot_pin() {
        let live_dir = tempfile::tempdir().unwrap();
        let context = resume_context(ProjectContext::LocalPath {
            path: live_dir.path().to_path_buf(),
        });
        assert_eq!(
            context.lifecycle_authority,
            ryeos_state::objects::ExecutionLifecycleAuthority::DAEMON_RESTARTABLE
        );
        assert!(matches!(
            context.project_authority,
            ryeos_state::objects::ExecutionProjectAuthority::LiveProject { .. }
        ));
        assert!(context.original_snapshot_hash.is_none());
        context.authoritative_project_identity().unwrap();

        let encoded = serde_json::to_vec(&context).unwrap();
        let decoded: ResumeContext = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, context);
        assert!(decoded.original_snapshot_hash.is_none());
    }

    #[test]
    fn live_restartable_authority_requires_exact_context_and_stable_identity() {
        let live_dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let mut context = resume_context(ProjectContext::LocalPath {
            path: live_dir.path().to_path_buf(),
        });
        context.project_context = ProjectContext::LocalPath {
            path: other_dir.path().to_path_buf(),
        };
        assert!(context.authoritative_project_identity().is_err());

        context.project_context = ProjectContext::LocalPath {
            path: live_dir.path().to_path_buf(),
        };
        context.stable_project_identity = None;
        assert!(context.authoritative_project_identity().is_err());
    }

    #[test]
    fn projectless_authority_rejects_stable_project_identity() {
        let mut context = resume_context(ProjectContext::None);
        context.stable_project_identity = Some(
            StableProjectIdentity::from_path(
                std::path::Path::new("/tmp/not-a-project"),
                "site:test",
            )
            .unwrap(),
        );
        assert!(context.authoritative_project_identity().is_err());
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
    fn authoritative_project_identity_rejects_project_ref_without_typed_resolution_evidence() {
        let mut context = resume_context(ProjectContext::ProjectRef {
            principal: "fp:test".to_string(),
            ref_name: "projects/demo".to_string(),
        });
        assert!(context.authoritative_project_identity().is_err());

        context.original_snapshot_hash = Some("e".repeat(64));
        assert!(context.authoritative_project_identity().is_err());
        context.project_authority = ryeos_state::objects::ExecutionProjectAuthority::pinned(
            "project-ref:fp:test:projects/demo".to_string(),
            None,
            "e".repeat(64),
            ryeos_state::objects::PinnedProjectRealization::ReadOnly,
            ryeos_state::objects::EnvironmentAuthority::None,
            Vec::new(),
        )
        .unwrap();
        assert!(context.authoritative_project_identity().is_err());
    }
}
