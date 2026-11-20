//! Daemon boot/exit marker + startup disk-space check.
//!
//! The daemon writes a `running` marker at process start — before its
//! control socket exists — and an `exited` marker on any handled shutdown.
//! Two consumers:
//!
//! - The lifecycle status probe reads it to tell a *booting* daemon (live
//!   process, no socket yet) apart from a stopped one.
//! - A `SIGKILL` or hard crash cannot write an exit marker — so on the next
//!   startup, a marker still in the `running` state whose pid is no longer
//!   alive is reported as an unclean exit (inferred crash). This turns "the
//!   daemon silently died" into a visible signal on the next `start`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const MARKER_FILE: &str = "lifecycle.json";

/// Shape version of the `running` record. Version 2 names the daemon by its
/// exact process identity instead of a bare pid, so a recycled pid (a new
/// container's first processes reuse the previous container's numbers) can
/// never read as the recorded daemon.
pub const RUNNING_MARKER_SCHEMA_VERSION: u32 = 2;

/// Provisional exit reason staged before native finalization retires the
/// operator target. It never proves a clean exit: only a completed finalizer
/// replaces it with the staged reason.
pub const FINALIZING_REASON: &str = "finalizing";
const MAX_MARKER_BYTES: u64 = 64 * 1024;

/// Warn when the state filesystem has less free space than this (bytes).
const LOW_DISK_THRESHOLD_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchRoute {
    Direct,
    Supervised,
}

/// Which stop wrote latched-drain facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatchedStopOutcome {
    /// A durable admission fact was refused; supervised restart is inhibited.
    ServiceRefusalStop,
    /// The planned service-window edge was reached; restart is not inhibited.
    ServiceWindowRollover,
}

/// Facts of one stop that ran after its incarnation's service SQL was
/// latched off. That stop proved the listed threads' process groups dead but
/// could not write their resume-budget re-arm; the next start's recovery
/// applies it from this record. Bound to the stopped incarnation's digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatchedDrainFacts {
    pub outcome: LatchedStopOutcome,
    /// Lowercase hex process digest of the stopped incarnation.
    pub incarnation_digest: String,
    /// Non-terminal attached threads whose groups the stop proved dead, each
    /// with the exact identity it proved.
    pub resume_attempts_reset_deferred:
        BTreeMap<String, ryeos_app::process::ExecutionProcessIdentity>,
}

/// Lowercase hex of one incarnation digest, as lifecycle records carry it.
pub fn incarnation_digest_hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LifecycleMarker {
    /// The daemon is (or was) up. If its exact process incarnation has ended
    /// and no `Exited` marker followed, the run ended uncleanly.
    Running {
        /// Always [`RUNNING_MARKER_SCHEMA_VERSION`]; see [`decode_marker`].
        schema_version: u32,
        /// The daemon's own exact process identity, captured when the marker
        /// was written. The only record of which process this is: the pid is
        /// `identity.target_pid`, never stored separately.
        identity: lillux::process_control::ExactProcessIdentity,
        started_at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_route: Option<LaunchRoute>,
        /// Latched-drain facts of an earlier stop that recovery has not yet
        /// applied. Carried by every lifecycle record until it has.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_latched_drain: Option<LatchedDrainFacts>,
        /// Hex digest of the incarnation that last carried the pending facts;
        /// the next incarnation accepts them only as its own predecessor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        carrier_incarnation: Option<String>,
    },
    /// The daemon shut down via a handled path, recording why.
    Exited {
        reason: String,
        pid: u32,
        started_at: String,
        exited_at: String,
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_route: Option<LaunchRoute>,
        /// Latched-drain facts this run wrote, or unapplied ones it carried.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latched_drain: Option<LatchedDrainFacts>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        carrier_incarnation: Option<String>,
    },
}

impl LifecycleMarker {
    /// Unapplied latched-drain facts and their carrier, if any.
    fn carried_latched_drain(&self) -> (Option<LatchedDrainFacts>, Option<String>) {
        // Every field is named, so a new field that carries facts cannot be
        // skipped here without a compile error.
        match self {
            Self::Running {
                schema_version: _,
                identity: _,
                started_at: _,
                launch_route: _,
                pending_latched_drain,
                carrier_incarnation,
            } => (pending_latched_drain.clone(), carrier_incarnation.clone()),
            Self::Exited {
                reason: _,
                pid: _,
                started_at: _,
                exited_at: _,
                error: _,
                launch_route: _,
                latched_drain,
                carrier_incarnation,
            } => (latched_drain.clone(), carrier_incarnation.clone()),
        }
    }
}

/// Why raw marker bytes did not decode to a marker.
#[derive(Debug)]
pub enum MarkerDecodeError {
    /// A `running` record written by a daemon from before the exact-identity
    /// marker (no `schema_version`, or a version other than
    /// [`RUNNING_MARKER_SCHEMA_VERSION`]). It names its daemon by a bare pid,
    /// so its liveness cannot be judged and no legacy reader exists.
    StaleRunningSchema { found: Option<u64> },
    /// Not a lifecycle marker at all.
    Unreadable(String),
}

impl std::fmt::Display for MarkerDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleRunningSchema { found } => write!(
                f,
                "lifecycle marker is a `running` record of schema {} written by a daemon from \
                 before the exact-process-identity marker (current schema {RUNNING_MARKER_SCHEMA_VERSION}); \
                 it is a leftover of that daemon and is retired by the next start, or remove \
                 `{MARKER_FILE}` from the node state directory once no ryeosd is running",
                found.map_or_else(|| "1 (unversioned)".to_owned(), |v| v.to_string()),
            ),
            Self::Unreadable(error) => write!(f, "lifecycle marker is unreadable: {error}"),
        }
    }
}

impl std::error::Error for MarkerDecodeError {}

/// Decode marker bytes. A `running` record that is not the current schema is
/// refused with [`MarkerDecodeError::StaleRunningSchema`] before any field is
/// interpreted; there is no reader for the old shape.
pub fn decode_marker(raw: &[u8]) -> Result<LifecycleMarker, MarkerDecodeError> {
    let value: serde_json::Value = serde_json::from_slice(raw)
        .map_err(|error| MarkerDecodeError::Unreadable(error.to_string()))?;
    if value.get("state").and_then(serde_json::Value::as_str) == Some("running") {
        let found = value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64);
        if found != Some(u64::from(RUNNING_MARKER_SCHEMA_VERSION)) {
            return Err(MarkerDecodeError::StaleRunningSchema { found });
        }
    }
    serde_json::from_value(value).map_err(|error| MarkerDecodeError::Unreadable(error.to_string()))
}

pub fn read(state_dir: &Path) -> Option<LifecycleMarker> {
    let directory = lillux::PinnedDirectory::open(state_dir).ok()??;
    let marker = directory
        .open_pinned_regular(std::ffi::OsStr::new(MARKER_FILE), false)
        .ok()??;
    let observation = marker.observation().ok()?;
    let raw = marker
        .read_stable_bounded(&observation, MAX_MARKER_BYTES)
        .ok()?;
    decode_marker(&raw).ok()
}

/// Retire exactly the retained startup-failure marker an operator has
/// acknowledged with `ryeos stop`.
///
/// Callers must first prove there is no live daemon peer and, for supervised
/// nodes, establish native down intent. The pinned compare-and-remove keeps a
/// replacement startup marker from being removed by this acknowledgement.
pub fn acknowledge_startup_failure(state_dir: &Path) -> anyhow::Result<()> {
    let directory = lillux::PinnedDirectory::open(state_dir)?
        .ok_or_else(|| anyhow::anyhow!("lifecycle state directory is absent"))?;
    let marker = directory
        .open_pinned_regular(std::ffi::OsStr::new(MARKER_FILE), false)?
        .ok_or_else(|| anyhow::anyhow!("startup-failure marker is absent"))?;
    let observation = marker.observation()?;
    let raw = marker.read_stable_bounded(&observation, MAX_MARKER_BYTES)?;
    let decoded = decode_marker(&raw)?;
    if !matches!(
        decoded,
        LifecycleMarker::Exited { ref reason, .. } if reason == "startup_failed"
    ) {
        anyhow::bail!("lifecycle marker is not an acknowledged startup failure");
    }
    // Removing the marker would drop facts the next start's recovery must
    // apply, so the acknowledgement refuses rather than lose them.
    if decoded.carried_latched_drain().0.is_some() {
        anyhow::bail!(
            "startup-failure marker carries unapplied latched-drain facts; the next start must apply them"
        );
    }
    directory.remove_pinned_regular_if_same(&marker)
}

pub(crate) const RESTART_INHIBIT_FILE: &str = "restart-inhibited.json";
const MAX_RESTART_INHIBIT_BYTES: u64 = 16 * 1024;

/// Durable record that this node stopped itself in a state where an
/// automatic restart would loop: each restart could spend another native
/// incarnation and recovery attempt without fixing the cause. The node
/// itself refuses to start while it exists, so the inhibition holds under any
/// supervisor or container restart policy, not only one whose desired state
/// the node can set. Only an explicit operator clear removes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartInhibition {
    pub reason: String,
    pub detail: Option<String>,
    pub recorded_at: String,
    pub pid: u32,
}

impl RestartInhibition {
    /// Plain-language refusal for a start attempted while inhibited.
    pub fn refusal(&self) -> String {
        format!(
            "start refused: this node stopped itself at {} with `{}`{} and must not be restarted \
             automatically; inspect `ryeos node doctor`, resolve the cause, then run \
             `ryeos node allow-restart` to permit the next start",
            self.recorded_at,
            self.reason,
            self.detail
                .as_deref()
                .map(|detail| format!(" ({detail})"))
                .unwrap_or_default()
        )
    }
}

/// Record a restart inhibition. The first recorded cause is kept until an
/// operator clears it; a later inhibition does not replace it.
pub fn inhibit_restart(state_dir: &Path, reason: &str, detail: Option<&str>) -> anyhow::Result<()> {
    let directory = lillux::PinnedDirectory::open(state_dir)?
        .ok_or_else(|| anyhow::anyhow!("lifecycle state directory is absent"))?;
    if directory
        .open_pinned_regular(std::ffi::OsStr::new(RESTART_INHIBIT_FILE), false)?
        .is_some()
    {
        return Ok(());
    }
    let record = RestartInhibition {
        reason: reason.to_owned(),
        detail: detail.map(|detail| {
            // Keep the record inside its bound; the lifecycle marker keeps
            // the complete error.
            detail.chars().take(4096).collect()
        }),
        recorded_at: lillux::time::iso8601_now(),
        pid: std::process::id(),
    };
    let body = serde_json::to_vec(&record)?;
    anyhow::ensure!(
        u64::try_from(body.len())? <= MAX_RESTART_INHIBIT_BYTES,
        "restart inhibition record exceeds its bound"
    );
    directory.atomic_write_pinned_if_same(
        std::ffi::OsStr::new(RESTART_INHIBIT_FILE),
        None,
        &body,
        0o600,
        crate::NODE_TEMP_PURPOSE,
    )?;
    Ok(())
}

/// The current restart inhibition, if any. An unreadable record is an error,
/// never absence: a start must not proceed past a record it cannot read.
pub fn read_restart_inhibition(state_dir: &Path) -> anyhow::Result<Option<RestartInhibition>> {
    let Some(directory) = lillux::PinnedDirectory::open(state_dir)? else {
        return Ok(None);
    };
    let Some(record) =
        directory.open_pinned_regular(std::ffi::OsStr::new(RESTART_INHIBIT_FILE), false)?
    else {
        return Ok(None);
    };
    let observation = record.observation()?;
    let raw = record.read_stable_bounded(&observation, MAX_RESTART_INHIBIT_BYTES)?;
    Ok(Some(serde_json::from_slice(&raw).map_err(|error| {
        anyhow::anyhow!("restart inhibition record is unreadable: {error}")
    })?))
}

/// Where the restart inhibition record of `state_dir` lives.
pub fn restart_inhibition_record(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join(RESTART_INHIBIT_FILE)
}

/// Refuse a start while a restart inhibition is recorded.
pub fn require_restart_allowed(state_dir: &Path) -> anyhow::Result<()> {
    match read_restart_inhibition(state_dir)? {
        None => Ok(()),
        Some(record) => anyhow::bail!("{}", record.refusal()),
    }
}

const RESTART_CLEAR_LOG: &str = "restart-inhibition-clears.jsonl";
/// The clear log keeps its newest entries within this bound; each entry is
/// a few kilobytes at most, so dozens of clears are retained.
const MAX_RESTART_CLEAR_LOG_BYTES: usize = 256 * 1024;

/// One operator clear of a restart inhibition, appended to the clear log in
/// the state directory before the inhibition is removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartInhibitionClear {
    pub cleared: RestartInhibition,
    pub cleared_at: String,
    /// The operating-system account that ran the clear.
    pub actor: lillux::process_control::ControllerAccount,
    pub command: String,
}

/// Operator clear, only while the node is stopped: `held` must be the node's
/// own state lock, which a running daemon holds, so a clear can never race a
/// live node. The clear is first appended to the clear log with its time and
/// actor, then exactly the record that was read is removed, so an inhibition
/// recorded by a later stop is not lost. Returns the cleared record, or
/// `None` when there was nothing to clear.
pub fn clear_restart_inhibition(
    app_root: &Path,
    held: &ryeos_node_runtime::state_lock::StateLock,
    command: &str,
) -> anyhow::Result<Option<RestartInhibition>> {
    held.ensure_protects_app_root(app_root)?;
    let state_dir = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).state();
    let Some(directory) = lillux::PinnedDirectory::open(&state_dir)? else {
        return Ok(None);
    };
    let Some(record) =
        directory.open_pinned_regular(std::ffi::OsStr::new(RESTART_INHIBIT_FILE), false)?
    else {
        return Ok(None);
    };
    let observation = record.observation()?;
    let raw = record.read_stable_bounded(&observation, MAX_RESTART_INHIBIT_BYTES)?;
    // An unreadable record is still cleared on explicit operator request,
    // since it can carry no facts the next start must apply.
    let cleared =
        serde_json::from_slice::<RestartInhibition>(&raw).unwrap_or_else(|_| RestartInhibition {
            reason: "unreadable".to_owned(),
            detail: None,
            recorded_at: String::new(),
            pid: 0,
        });
    let entry = RestartInhibitionClear {
        cleared: cleared.clone(),
        cleared_at: lillux::time::iso8601_now(),
        actor: lillux::process_control::ControllerAccount::current().map_err(anyhow::Error::msg)?,
        command: command.to_owned(),
    };
    append_restart_clear(&directory, &entry)?;
    directory.remove_pinned_regular_if_same(&record)?;
    Ok(Some(cleared))
}

/// Every recorded clear, oldest first.
pub fn read_restart_clears(state_dir: &Path) -> anyhow::Result<Vec<RestartInhibitionClear>> {
    let Some(directory) = lillux::PinnedDirectory::open(state_dir)? else {
        return Ok(Vec::new());
    };
    let Some(log) =
        directory.open_pinned_regular(std::ffi::OsStr::new(RESTART_CLEAR_LOG), false)?
    else {
        return Ok(Vec::new());
    };
    let observation = log.observation()?;
    let raw = log.read_stable_bounded(&observation, MAX_RESTART_CLEAR_LOG_BYTES as u64)?;
    raw.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice(line)
                .map_err(|error| anyhow::anyhow!("restart clear log entry is unreadable: {error}"))
        })
        .collect()
}

/// Append one entry by pinned compare-and-write, dropping the oldest whole
/// entries only when the log would exceed its bound.
fn append_restart_clear(
    directory: &lillux::PinnedDirectory,
    entry: &RestartInhibitionClear,
) -> anyhow::Result<()> {
    let existing = directory.open_pinned_regular(std::ffi::OsStr::new(RESTART_CLEAR_LOG), false)?;
    let mut body = match &existing {
        Some(log) => {
            let observation = log.observation()?;
            log.read_stable_bounded(&observation, MAX_RESTART_CLEAR_LOG_BYTES as u64)?
        }
        None => Vec::new(),
    };
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    anyhow::ensure!(
        line.len() <= MAX_RESTART_CLEAR_LOG_BYTES,
        "restart clear entry exceeds the log bound"
    );
    while body.len() + line.len() > MAX_RESTART_CLEAR_LOG_BYTES {
        match body.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                body.drain(..=end);
            }
            None => body.clear(),
        }
    }
    body.extend_from_slice(&line);
    directory.atomic_write_pinned_if_same(
        std::ffi::OsStr::new(RESTART_CLEAR_LOG),
        existing.as_ref(),
        &body,
        0o600,
        crate::NODE_TEMP_PURPOSE,
    )?;
    Ok(())
}

/// Wall-clock age of the current marker file. The running marker is written
/// immediately before startup listener publication, so this bounds how long a
/// live marker may reasonably be treated as the narrow pre-control bootstrap
/// window. A backwards clock jump yields no age rather than a false timeout.
pub fn age(state_dir: &Path) -> Option<Duration> {
    let directory = lillux::PinnedDirectory::open(state_dir).ok()??;
    let marker = directory
        .open_pinned_regular(std::ffi::OsStr::new(MARKER_FILE), false)
        .ok()??;
    marker.modification_age().ok()?
}

/// Replace the marker with the record `build` derives from the current one,
/// by pinned compare-and-write. An unreadable current marker refuses, so a
/// record carrying unapplied facts is never overwritten blind. The record
/// must fit the marker bound; it is never truncated.
fn write_strict(
    state_dir: &Path,
    build: impl FnOnce(Option<&LifecycleMarker>) -> anyhow::Result<LifecycleMarker>,
) -> anyhow::Result<()> {
    let directory = lillux::PinnedDirectory::open(state_dir)?
        .ok_or_else(|| anyhow::anyhow!("lifecycle state directory is absent"))?;
    let existing = directory.open_pinned_regular(std::ffi::OsStr::new(MARKER_FILE), false)?;
    let current = match &existing {
        Some(file) => {
            let observation = file.observation()?;
            let raw = file.read_stable_bounded(&observation, MAX_MARKER_BYTES)?;
            match decode_marker(&raw) {
                Ok(marker) => Some(marker),
                // Advisory liveness state of a daemon from before this
                // schema: replaced, as `take_previous_exit` already reported.
                Err(MarkerDecodeError::StaleRunningSchema { .. }) => None,
                Err(error) => {
                    return Err(anyhow::anyhow!("current {error}"));
                }
            }
        }
        None => None,
    };
    let marker = build(current.as_ref())?;
    let body = serde_json::to_vec(&marker)?;
    anyhow::ensure!(
        u64::try_from(body.len())? <= MAX_MARKER_BYTES,
        "lifecycle marker of {} bytes exceeds its {MAX_MARKER_BYTES}-byte bound",
        body.len()
    );
    directory.atomic_write_pinned_if_same(
        std::ffi::OsStr::new(MARKER_FILE),
        existing.as_ref(),
        &body,
        0o600,
        crate::NODE_TEMP_PURPOSE,
    )?;
    Ok(())
}

/// What the previous run left in the marker file.
#[derive(Debug, Clone)]
pub enum PreviousRun {
    Marker(LifecycleMarker),
    /// A `running` record of a pre-identity schema: retired, not judged.
    RetiredStaleRunning {
        found_schema: Option<u64>,
    },
}

/// A previous run that ended without a handled exit: its marker still says
/// `running` and the exact process it names is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncleanExit {
    /// The recorded daemon's pid, for display only; the verdict is the exact
    /// identity's.
    pub pid: u32,
    pub started_at: String,
    pub launch_route: Option<LaunchRoute>,
}

/// The unclean exit `previous` shows, if it shows one. A `running` marker
/// names a run that is over exactly when [`recorded_process`] proves its
/// recorded incarnation ended: another boot, an absent process, or another
/// birth at the same pid (a container restart reuses the pid, including this
/// process's own). A run still running, one that cannot be classified, and a
/// retired marker of an older schema are never an unclean exit.
pub fn inferred_unclean_exit(previous: Option<&PreviousRun>) -> Option<UncleanExit> {
    let Some(PreviousRun::Marker(LifecycleMarker::Running {
        identity,
        started_at,
        launch_route,
        schema_version: _,
        pending_latched_drain: _,
        carrier_incarnation: _,
    })) = previous
    else {
        return None;
    };
    (recorded_process(identity) == RecordedProcess::Ended).then(|| UncleanExit {
        pid: identity.target_pid,
        started_at: started_at.clone(),
        launch_route: *launch_route,
    })
}

/// The previous run's marker, read before this start overwrites it. `None`
/// means no marker exists. An unreadable marker (bad bytes, over its bound,
/// a refused pinned open) is an error: the start must fail closed and leave
/// it in place, since it may carry facts recovery has not applied.
///
/// A `running` record of an older schema is the one exception: it is retired
/// (reported by [`report_previous_exit`], replaced by this start's record)
/// rather than refused. The marker is advisory process-liveness state, and
/// the caller holds the state lock, so no daemon of that older shape can be
/// running. The only durable content such a record can carry is unapplied
/// latched-drain facts; losing them degrades to the same crash treatment a
/// stale-predecessor drop already gets in [`bind_pending_latched_drain`].
/// Refusing instead would wedge every first start after the upgrade until an
/// operator hand-deleted the file.
pub fn take_previous_exit(state_dir: &Path) -> anyhow::Result<Option<PreviousRun>> {
    let Some(directory) = lillux::PinnedDirectory::open(state_dir)? else {
        return Ok(None);
    };
    let Some(marker) = directory.open_pinned_regular(std::ffi::OsStr::new(MARKER_FILE), false)?
    else {
        return Ok(None);
    };
    let observation = marker.observation()?;
    let raw = marker.read_stable_bounded(&observation, MAX_MARKER_BYTES)?;
    match decode_marker(&raw) {
        Ok(decoded) => Ok(Some(PreviousRun::Marker(decoded))),
        Err(MarkerDecodeError::StaleRunningSchema { found }) => {
            Ok(Some(PreviousRun::RetiredStaleRunning {
                found_schema: found,
            }))
        }
        Err(error) => Err(anyhow::anyhow!("previous {error}")),
    }
}

/// Record this exact daemon startup attempt. Call after the state lock is held
/// and after [`report_previous_exit`] has inspected the prior run's marker;
/// readiness remains the lifecycle protocol's separate authority. Unapplied
/// latched-drain facts in the previous marker are carried forward with their
/// carrier. Strict: a refused write returns an error and leaves the previous
/// marker in place, and the start must not continue to its native debit,
/// since the facts could not then be bound to it.
pub fn record_running(state_dir: &Path, launch_route: LaunchRoute) -> anyhow::Result<String> {
    let started_at = lillux::time::iso8601_now();
    // The same capture the native capacity incarnation uses for this process.
    let identity = lillux::process_control::capture_current_process_identity()
        .map_err(|error| anyhow::anyhow!("capture the daemon's own process identity: {error}"))?;
    write_strict(state_dir, |current| {
        let (pending_latched_drain, carrier_incarnation) = current
            .map(LifecycleMarker::carried_latched_drain)
            .unwrap_or((None, None));
        Ok(LifecycleMarker::Running {
            schema_version: RUNNING_MARKER_SCHEMA_VERSION,
            identity: identity.clone(),
            started_at: started_at.clone(),
            launch_route: Some(launch_route),
            pending_latched_drain,
            carrier_incarnation,
        })
    })
    .map_err(|error| error.context("write daemon running marker; previous marker retained"))?;
    Ok(started_at)
}

/// This run's own running record: the same process and the same start.
fn require_own_running(
    identity: &lillux::process_control::ExactProcessIdentity,
    started_at: &str,
    own_started_at: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        identity.target_pid == std::process::id() && started_at == own_started_at,
        "lifecycle marker belongs to another run"
    );
    Ok(())
}

/// Bind unapplied latched-drain facts to this start once its own capacity
/// debit is durable. They are accepted only when `predecessor`, the
/// incarnation this start's debit settled, is the incarnation that last
/// carried them (or, with no carrier yet, the stopped incarnation itself);
/// this start then becomes their carrier. Anything else is stale: the facts
/// are dropped with a warning and recovery treats the stop as a crash.
pub fn bind_pending_latched_drain(
    state_dir: &Path,
    own_started_at: &str,
    own: &[u8; 32],
    predecessor: Option<&[u8; 32]>,
) -> anyhow::Result<Option<LatchedDrainFacts>> {
    let mut accepted = None;
    write_strict(state_dir, |current| {
        let Some(LifecycleMarker::Running {
            schema_version,
            identity,
            started_at,
            launch_route,
            pending_latched_drain,
            carrier_incarnation,
        }) = current.cloned()
        else {
            anyhow::bail!("lifecycle marker is not this run's running record");
        };
        require_own_running(&identity, &started_at, own_started_at)?;
        let pending_latched_drain = match pending_latched_drain {
            Some(facts) => {
                let expected = carrier_incarnation
                    .clone()
                    .unwrap_or_else(|| facts.incarnation_digest.clone());
                if predecessor.map(incarnation_digest_hex).as_deref() == Some(expected.as_str()) {
                    accepted = Some(facts.clone());
                    Some(facts)
                } else {
                    tracing::warn!(
                        expected_predecessor = %expected,
                        "latched-drain facts do not bind to this start's predecessor; ignored"
                    );
                    None
                }
            }
            None => None,
        };
        let carrier_incarnation = pending_latched_drain
            .as_ref()
            .map(|_| incarnation_digest_hex(own));
        Ok(LifecycleMarker::Running {
            schema_version,
            identity,
            started_at,
            launch_route,
            pending_latched_drain,
            carrier_incarnation,
        })
    })?;
    Ok(accepted)
}

/// Drop applied latched-drain facts. Called only after recovery applied
/// them; this is the only write that removes them.
pub fn clear_pending_latched_drain(state_dir: &Path, own_started_at: &str) -> anyhow::Result<()> {
    write_strict(state_dir, |current| {
        let Some(LifecycleMarker::Running {
            schema_version,
            identity,
            started_at,
            launch_route,
            pending_latched_drain: _,
            carrier_incarnation: _,
        }) = current.cloned()
        else {
            anyhow::bail!("lifecycle marker is not this run's running record");
        };
        require_own_running(&identity, &started_at, own_started_at)?;
        Ok(LifecycleMarker::Running {
            schema_version,
            identity,
            started_at,
            launch_route,
            pending_latched_drain: None,
            carrier_incarnation: None,
        })
    })
}

/// Record a clean/handled shutdown with its `reason` (e.g. `"signal"`).
/// Unapplied latched-drain facts in the current marker are carried.
pub fn record_exit(
    state_dir: &Path,
    reason: &str,
    started_at: &str,
    error: Option<&str>,
    launch_route: LaunchRoute,
) {
    let result = write_strict(state_dir, |current| {
        let (latched_drain, carrier) = current
            .map(LifecycleMarker::carried_latched_drain)
            .unwrap_or((None, None));
        Ok(exited_marker(
            reason,
            started_at,
            error,
            launch_route,
            latched_drain,
            carrier,
        ))
    });
    if let Err(e) = result {
        tracing::warn!(error = %format!("{e:#}"), "failed to write daemon lifecycle marker");
    }
}

/// Record this run's provisional exit together with the latched-drain facts
/// of its own stop. Older facts this run still carries unapplied are merged
/// in by thread, so a re-arm an earlier stop deferred is never dropped; where
/// both name a thread, this stop's later proof wins. The merged record is
/// bound to this stop's incarnation. Strict: a failure is returned, and the
/// caller then holds the obligation the facts would have deferred.
pub fn record_exit_with_latched_drain(
    state_dir: &Path,
    reason: &str,
    started_at: &str,
    error: Option<&str>,
    launch_route: LaunchRoute,
    facts: &LatchedDrainFacts,
) -> anyhow::Result<()> {
    write_strict(state_dir, |current| {
        let older = current
            .map(LifecycleMarker::carried_latched_drain)
            .and_then(|(older, _)| older);
        let merged = merge_latched_drain(older, facts);
        Ok(exited_marker(
            reason,
            started_at,
            error,
            launch_route,
            Some(merged),
            None,
        ))
    })
}

/// `newer` with every thread of `older` it does not itself name.
fn merge_latched_drain(
    older: Option<LatchedDrainFacts>,
    newer: &LatchedDrainFacts,
) -> LatchedDrainFacts {
    let LatchedDrainFacts {
        outcome,
        incarnation_digest,
        resume_attempts_reset_deferred,
    } = newer.clone();
    let mut threads = resume_attempts_reset_deferred;
    if let Some(LatchedDrainFacts {
        outcome: _,
        incarnation_digest: older_incarnation,
        resume_attempts_reset_deferred: older_threads,
    }) = older
    {
        tracing::info!(
            older_incarnation = %older_incarnation,
            carried = older_threads.len(),
            "unapplied latched-drain facts are merged into this stop's record"
        );
        for (thread, identity) in older_threads {
            threads.entry(thread).or_insert(identity);
        }
    }
    LatchedDrainFacts {
        outcome,
        incarnation_digest,
        resume_attempts_reset_deferred: threads,
    }
}

fn exited_marker(
    reason: &str,
    started_at: &str,
    error: Option<&str>,
    launch_route: LaunchRoute,
    latched_drain: Option<LatchedDrainFacts>,
    carrier_incarnation: Option<String>,
) -> LifecycleMarker {
    const MAX_ERROR_BYTES: usize = 8 * 1024;
    let error = error.map(|value| {
        let mut end = value.len().min(MAX_ERROR_BYTES);
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value[..end].trim().to_owned()
    });
    LifecycleMarker::Exited {
        reason: reason.to_string(),
        pid: std::process::id(),
        started_at: started_at.to_owned(),
        exited_at: lillux::time::iso8601_now(),
        error,
        launch_route: Some(launch_route),
        latched_drain,
        carrier_incarnation,
    }
}

/// Replace this run's own provisional [`FINALIZING_REASON`] marker with its
/// final reason. The pinned compare-and-write refuses any other marker, so a
/// successor's marker written after the exclusion was released is never
/// overwritten. Returns whether the final reason was recorded.
pub fn complete_finalizing_exit(
    state_dir: &Path,
    reason: &str,
    started_at: &str,
    error: Option<&str>,
    launch_route: LaunchRoute,
) -> bool {
    let result = (|| -> anyhow::Result<()> {
        let directory = lillux::PinnedDirectory::open(state_dir)?
            .ok_or_else(|| anyhow::anyhow!("lifecycle state directory is absent"))?;
        let existing = directory
            .open_pinned_regular(std::ffi::OsStr::new(MARKER_FILE), false)?
            .ok_or_else(|| anyhow::anyhow!("provisional lifecycle marker is absent"))?;
        let observation = existing.observation()?;
        let raw = existing.read_stable_bounded(&observation, MAX_MARKER_BYTES)?;
        let decoded: LifecycleMarker = serde_json::from_slice(&raw)?;
        let own_provisional = matches!(
            &decoded,
            LifecycleMarker::Exited { reason, pid, started_at: retained, .. }
                if reason == FINALIZING_REASON
                    && *pid == std::process::id()
                    && retained == started_at
        );
        anyhow::ensure!(
            own_provisional,
            "lifecycle marker is not this run's provisional exit"
        );
        // The provisional record's latched-drain facts survive completion.
        let (latched_drain, carrier) = decoded.carried_latched_drain();
        let body = serde_json::to_vec(&exited_marker(
            reason,
            started_at,
            error,
            launch_route,
            latched_drain,
            carrier,
        ))?;
        directory.atomic_write_pinned_if_same(
            std::ffi::OsStr::new(MARKER_FILE),
            Some(&existing),
            &body,
            0o600,
            crate::NODE_TEMP_PURPOSE,
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "provisional daemon lifecycle marker was not completed");
            false
        }
    }
}

/// Inspect the previous run's marker and log its outcome: a recorded clean
/// exit, an unproved finalization, or an inferred unclean exit when the marker
/// is still `running` but the pid is gone.
/// Log what the previous run's marker says about how it ended. The marker is
/// taken by the caller before its own running record is written; this only
/// narrates it, so it runs once the tracing subscriber exists, otherwise the
/// crash and unclean-exit warnings are emitted into nothing.
pub fn report_previous_run(previous: Option<&PreviousRun>) {
    match previous {
        Some(PreviousRun::Marker(marker)) => report_previous_exit(Some(marker)),
        Some(PreviousRun::RetiredStaleRunning { found_schema }) => {
            tracing::warn!(
                found_schema = ?found_schema,
                current_schema = RUNNING_MARKER_SCHEMA_VERSION,
                "previous lifecycle marker was retired: it is a `running` record written by a daemon from before the exact-process-identity marker, so its liveness cannot be judged; any unapplied latched-drain facts it carried are dropped and recovery treats that stop as a crash"
            );
        }
        None => {}
    }
}

/// Verdict on whether the daemon a `running` marker names is still the live
/// process, from its exact identity. Only proof of absence is "ended";
/// an indeterminate observation is never read as gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedProcess {
    /// Boot, birth or presence proves the recorded incarnation is gone.
    Ended,
    /// The recorded incarnation is still the occupant of its coordinate.
    Running,
    /// Observation failed with a live occupant: not provably gone.
    Indeterminate(String),
}

impl RecordedProcess {
    pub fn from_has_ended(observed: Result<bool, String>) -> Self {
        match observed {
            Ok(true) => Self::Ended,
            Ok(false) => Self::Running,
            Err(error) => Self::Indeterminate(error),
        }
    }

    /// Fail-closed reading: anything but proof of absence keeps the recorded
    /// daemon possibly alive.
    pub fn not_provably_gone(&self) -> bool {
        !matches!(self, Self::Ended)
    }
}

/// Classify the daemon a `running` marker names by its exact identity.
pub fn recorded_process(
    identity: &lillux::process_control::ExactProcessIdentity,
) -> RecordedProcess {
    RecordedProcess::from_has_ended(identity.has_ended())
}

pub fn report_previous_exit(previous: Option<&LifecycleMarker>) {
    match previous {
        Some(LifecycleMarker::Exited {
            reason,
            pid,
            started_at: _,
            exited_at,
            error,
            launch_route: _,
            latched_drain,
            carrier_incarnation: _,
        }) if reason == FINALIZING_REASON => {
            tracing::warn!(
                pid,
                at = %exited_at,
                detail = error.as_deref().unwrap_or(""),
                carries_latched_drain = latched_drain.is_some(),
                "previous ryeosd run did not prove native finalization (provisional exit marker) — treat its exit as unclean"
            );
        }
        Some(LifecycleMarker::Exited {
            reason,
            pid: _,
            started_at: _,
            exited_at,
            error: _,
            launch_route: _,
            latched_drain,
            carrier_incarnation: _,
        }) => {
            tracing::info!(
                reason = %reason,
                at = %exited_at,
                carries_latched_drain = latched_drain.is_some(),
                "previous ryeosd run exited cleanly"
            );
        }
        Some(LifecycleMarker::Running {
            schema_version: _,
            identity,
            started_at,
            launch_route,
            pending_latched_drain,
            carrier_incarnation: _,
        }) => {
            let pid = identity.target_pid;
            if pending_latched_drain.is_some() {
                tracing::info!(
                    pid,
                    "previous ryeosd run carried unapplied latched-drain facts"
                );
            }
            match recorded_process(identity) {
                RecordedProcess::Running => tracing::warn!(
                    pid,
                    ?launch_route,
                    "lifecycle marker shows a running ryeosd (pid {pid}, same incarnation) — another instance may be active"
                ),
                RecordedProcess::Indeterminate(error) => tracing::warn!(
                    pid,
                    ?launch_route,
                    error = %error,
                    "lifecycle marker names a ryeosd (pid {pid}) whose liveness could not be determined — treated as not provably gone; another instance may be active"
                ),
                RecordedProcess::Ended => tracing::warn!(
                    pid,
                    started_at = %started_at,
                    ?launch_route,
                    "previous ryeosd run did not shut down cleanly (no exit marker; that incarnation of pid {pid} has ended) — a crash, SIGKILL, container replacement, or failed startup"
                ),
            }
        }
        None => {}
    }
}

/// Warn if the state filesystem is low on free space. Best-effort: a failed
/// probe is silently ignored (never blocks startup).
pub fn check_disk_space(state_dir: &Path) {
    if let Some(free) = available_bytes(state_dir)
        && free < LOW_DISK_THRESHOLD_BYTES
    {
        tracing::warn!(
            free_mib = free / (1024 * 1024),
            threshold_mib = LOW_DISK_THRESHOLD_BYTES / (1024 * 1024),
            path = %state_dir.display(),
            "low free disk space on the state filesystem — runtime writes (events, CAS, traces) may fail"
        );
    }
}

/// Whether the daemon a `running` marker names may still be alive. A crash
/// leaves a `running` marker behind, and the OS may recycle its pid onto an
/// unrelated process, even the new daemon itself in a fresh container, so
/// classifying by pid would block `ryeos start` indefinitely. The recorded
/// exact identity (boot id, birth ticks, group) rules recycling out, which
/// subsumes the former executable-name filter: only the recorded incarnation
/// can match, and the marker is written only by `ryeosd`. An indeterminate
/// observation counts as alive, never as gone. (`stop` has its own
/// fail-closed variant with per-reason errors; this one only classifies.)
pub fn running_marker_not_provably_gone(
    identity: &lillux::process_control::ExactProcessIdentity,
) -> bool {
    recorded_process(identity).not_provably_gone()
}

fn available_bytes(path: &Path) -> Option<u64> {
    lillux::PinnedDirectory::open(path)
        .ok()??
        .filesystem_capacity()
        .ok()
        .map(|capacity| capacity.available_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_inhibition_holds_until_an_operator_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        let app_root = tmp.path();
        let state = ryeos_engine::roots::RuntimeRoot::new(app_root.to_path_buf()).state();
        std::fs::create_dir_all(&state).unwrap();
        let state = state.as_path();
        require_restart_allowed(state).unwrap();
        inhibit_restart(
            state,
            "service_refusal_stop",
            Some("admission fact refused"),
        )
        .unwrap();
        inhibit_restart(state, "startup_failed", None).unwrap();
        let refusal = require_restart_allowed(state).unwrap_err().to_string();
        assert!(refusal.contains("service_refusal_stop"), "{refusal}");
        assert!(refusal.contains("admission fact refused"), "{refusal}");
        assert!(refusal.contains("ryeos node allow-restart"), "{refusal}");

        // Only the node's own state lock admits a clear.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(other.path().join(ryeos_engine::AI_DIR).join("state")).unwrap();
        let foreign = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(other.path()),
        )
        .unwrap();
        assert!(clear_restart_inhibition(app_root, &foreign, "fixture").is_err());
        let held = ryeos_node_runtime::state_lock::StateLock::acquire(
            &ryeos_node_runtime::state_lock::default_lock_path(app_root),
        )
        .unwrap();
        // A second holder, such as a running daemon, excludes the clear.
        assert!(
            ryeos_node_runtime::state_lock::StateLock::acquire(
                &ryeos_node_runtime::state_lock::default_lock_path(app_root),
            )
            .is_err()
        );

        let cleared = clear_restart_inhibition(app_root, &held, "fixture clear")
            .unwrap()
            .unwrap();
        assert_eq!(cleared.reason, "service_refusal_stop");
        require_restart_allowed(state).unwrap();
        assert!(
            clear_restart_inhibition(app_root, &held, "fixture clear")
                .unwrap()
                .is_none()
        );
        std::fs::write(state.join(RESTART_INHIBIT_FILE), b"not json").unwrap();
        assert!(require_restart_allowed(state).is_err());
        assert!(
            clear_restart_inhibition(app_root, &held, "fixture clear")
                .unwrap()
                .is_some()
        );
        require_restart_allowed(state).unwrap();

        let clears = read_restart_clears(state).unwrap();
        assert_eq!(clears.len(), 2);
        assert_eq!(clears[0].cleared.reason, "service_refusal_stop");
        assert_eq!(clears[0].command, "fixture clear");
        assert_eq!(
            clears[0].actor,
            lillux::process_control::ControllerAccount::current().unwrap()
        );
        assert!(!clears[0].cleared_at.is_empty());
        assert_eq!(clears[1].cleared.reason, "unreadable");
    }

    #[test]
    fn clean_exit_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let started_at = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Running { .. })
        ));
        record_exit(
            tmp.path(),
            "startup_failed",
            &started_at,
            Some("failed to bind 127.0.0.1:7400"),
            LaunchRoute::Direct,
        );
        match read(tmp.path()) {
            Some(LifecycleMarker::Exited {
                reason,
                started_at: retained_start,
                error,
                launch_route,
                ..
            }) => {
                assert_eq!(reason, "startup_failed");
                assert_eq!(retained_start, started_at);
                assert_eq!(error.as_deref(), Some("failed to bind 127.0.0.1:7400"));
                assert_eq!(launch_route, Some(LaunchRoute::Direct));
            }
            other => panic!("expected exited marker, got {other:?}"),
        }
    }

    #[test]
    fn startup_failure_acknowledgement_removes_only_the_pinned_failed_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let started_at = record_running(tmp.path(), LaunchRoute::Supervised).unwrap();
        record_exit(
            tmp.path(),
            "startup_failed",
            &started_at,
            Some("fixture failure"),
            LaunchRoute::Supervised,
        );
        acknowledge_startup_failure(tmp.path()).unwrap();
        assert!(read(tmp.path()).is_none());
        assert!(acknowledge_startup_failure(tmp.path()).is_err());

        let started_at = record_running(tmp.path(), LaunchRoute::Supervised).unwrap();
        record_exit(
            tmp.path(),
            "signal",
            &started_at,
            None,
            LaunchRoute::Supervised,
        );
        assert!(acknowledge_startup_failure(tmp.path()).is_err());
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Exited { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_marker_never_follows_a_replaced_entry() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"retained").unwrap();
        symlink(&victim, tmp.path().join(MARKER_FILE)).unwrap();
        assert!(record_running(tmp.path(), LaunchRoute::Direct).is_err());
        assert_eq!(std::fs::read(victim).unwrap(), b"retained");
        assert!(read(tmp.path()).is_none());
    }

    fn identity_at(
        boot: &str,
        pid: u32,
        ticks: u64,
    ) -> lillux::process_control::ExactProcessIdentity {
        lillux::process_control::ExactProcessIdentity {
            boot_id: boot.to_owned(),
            target_pid: pid,
            target_start_time_ticks: ticks,
            group_leader_pid: pid,
            group_leader_start_time_ticks: ticks,
            pid_namespace: "pid:[4026531836]".into(),
            group_basis: Default::default(),
        }
    }

    fn running_with(identity: lillux::process_control::ExactProcessIdentity) -> LifecycleMarker {
        LifecycleMarker::Running {
            schema_version: RUNNING_MARKER_SCHEMA_VERSION,
            identity,
            started_at: "2026-01-01T00:00:00Z".into(),
            launch_route: Some(LaunchRoute::Direct),
            pending_latched_drain: None,
            carrier_incarnation: None,
        }
    }

    #[test]
    fn running_marker_of_another_boot_is_ended_even_at_our_own_pid() {
        // A new container's first processes reuse the previous container's
        // pids, including this one. A different boot id proves it ended.
        let tmp = tempfile::tempdir().unwrap();
        let own = std::process::id();
        let marker = running_with(identity_at("00000000-dead-beef-0000-000000000000", own, 5));
        write_strict(tmp.path(), |_| Ok(marker.clone())).unwrap();
        let Some(LifecycleMarker::Running { identity, .. }) = read(tmp.path()) else {
            panic!("marker round-trips");
        };
        assert_eq!(recorded_process(&identity), RecordedProcess::Ended);
        assert!(!running_marker_not_provably_gone(&identity));
        report_previous_exit(read(tmp.path()).as_ref());
    }

    #[test]
    fn running_marker_carries_the_pid_namespace_and_requires_it() {
        let live = lillux::process_control::capture_current_process_identity().unwrap();
        let marker = serde_json::to_value(running_with(live.clone())).unwrap();
        assert_eq!(
            marker["identity"]["pid_namespace"],
            lillux::process_control::current_pid_namespace().unwrap()
        );
        let raw = serde_json::to_vec(&marker).unwrap();
        assert!(matches!(
            decode_marker(&raw),
            Ok(LifecycleMarker::Running { identity, .. }) if identity == live
        ));
        // No legacy reader: a schema-2 marker written before the namespace
        // field existed does not decode.
        let mut without = marker;
        without["identity"]
            .as_object_mut()
            .unwrap()
            .remove("pid_namespace");
        let error = decode_marker(&serde_json::to_vec(&without).unwrap()).unwrap_err();
        assert!(
            matches!(&error, MarkerDecodeError::Unreadable(text) if text.contains("pid_namespace")),
            "{error}"
        );
    }

    #[test]
    fn marker_from_another_pid_namespace_is_not_provably_gone() {
        // Same boot, our own pid and birth, but recorded in another
        // namespace: the numbers prove nothing, so it is neither running nor
        // ended, and a caller holding no state lock must keep it as possibly
        // alive.
        let mut foreign = lillux::process_control::capture_current_process_identity().unwrap();
        foreign.pid_namespace = "pid:[1]".to_owned();
        let verdict = recorded_process(&foreign);
        assert!(
            matches!(&verdict, RecordedProcess::Indeterminate(text) if text.contains("pid namespace")),
            "{verdict:?}"
        );
        assert!(running_marker_not_provably_gone(&foreign));
    }

    #[test]
    fn the_journal_infers_an_unclean_exit_only_from_an_ended_exact_identity() {
        let running = |identity| PreviousRun::Marker(running_with(identity));
        let live = lillux::process_control::capture_current_process_identity().unwrap();
        // Still the running incarnation: not an exit.
        assert!(inferred_unclean_exit(Some(&running(live.clone()))).is_none());
        // Another boot: ended; the pid is only displayed.
        let exit = inferred_unclean_exit(Some(&running(identity_at(
            "00000000-dead-beef-0000-000000000000",
            u32::MAX - 1,
            5,
        ))))
        .expect("another boot ended the recorded run");
        assert_eq!(exit.pid, u32::MAX - 1);
        assert_eq!(exit.started_at, "2026-01-01T00:00:00Z");
        assert_eq!(exit.launch_route, Some(LaunchRoute::Direct));
        // Our own pid at another birth (a restarted container): ended.
        let mut recycled = live.clone();
        recycled.target_start_time_ticks += 1;
        assert!(inferred_unclean_exit(Some(&running(recycled))).is_some());
        // A foreign pid namespace cannot be classified: never an exit.
        let mut foreign = live;
        foreign.pid_namespace = "pid:[1]".to_owned();
        assert!(inferred_unclean_exit(Some(&running(foreign))).is_none());
        // A retired older-schema marker, no marker, and a handled exit.
        assert!(
            inferred_unclean_exit(Some(&PreviousRun::RetiredStaleRunning {
                found_schema: None
            }))
            .is_none()
        );
        assert!(inferred_unclean_exit(None).is_none());
    }

    #[test]
    fn same_boot_different_birth_at_our_own_pid_is_ended() {
        let live = lillux::process_control::capture_current_process_identity().unwrap();
        let mut recycled = live.clone();
        recycled.target_start_time_ticks += 1;
        assert_eq!(recorded_process(&recycled), RecordedProcess::Ended);
        assert!(!running_marker_not_provably_gone(&recycled));
    }

    #[test]
    fn exact_match_is_running_and_not_gone() {
        let live = lillux::process_control::capture_current_process_identity().unwrap();
        assert_eq!(recorded_process(&live), RecordedProcess::Running);
        assert!(running_marker_not_provably_gone(&live));
    }

    #[test]
    fn indeterminate_observation_is_not_provably_gone() {
        let verdict = RecordedProcess::from_has_ended(Err("probe denied".to_owned()));
        assert_eq!(
            verdict,
            RecordedProcess::Indeterminate("probe denied".into())
        );
        assert!(verdict.not_provably_gone());
        assert!(!RecordedProcess::from_has_ended(Ok(true)).not_provably_gone());
        assert!(RecordedProcess::from_has_ended(Ok(false)).not_provably_gone());
    }

    #[test]
    fn running_marker_round_trips_with_its_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let started_at = record_running(tmp.path(), LaunchRoute::Supervised).unwrap();
        let Some(LifecycleMarker::Running {
            schema_version,
            identity,
            started_at: recorded,
            ..
        }) = read(tmp.path())
        else {
            panic!("running marker");
        };
        assert_eq!(schema_version, RUNNING_MARKER_SCHEMA_VERSION);
        assert_eq!(recorded, started_at);
        assert_eq!(
            identity,
            lillux::process_control::capture_current_process_identity().unwrap()
        );
        assert_eq!(identity.target_pid, std::process::id());
    }

    #[test]
    fn schema_one_running_marker_is_retired_not_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = br#"{"state":"running","pid":7,"started_at":"2026-01-01T00:00:00Z"}"#;
        std::fs::write(tmp.path().join(MARKER_FILE), legacy).unwrap();
        assert!(matches!(
            decode_marker(legacy),
            Err(MarkerDecodeError::StaleRunningSchema { found: None })
        ));
        let message = decode_marker(legacy).unwrap_err().to_string();
        assert!(message.contains(MARKER_FILE), "{message}");
        assert!(read(tmp.path()).is_none());
        assert!(matches!(
            take_previous_exit(tmp.path()).unwrap(),
            Some(PreviousRun::RetiredStaleRunning { found_schema: None })
        ));
        // The start replaces it with a current record.
        record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Running { .. })
        ));
        // A running record of any other schema is stale too.
        let other = br#"{"state":"running","schema_version":1,"pid":7}"#;
        assert!(matches!(
            decode_marker(other),
            Err(MarkerDecodeError::StaleRunningSchema { found: Some(1) })
        ));
    }

    #[test]
    fn running_marker_with_dead_pid_is_detectable() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = running_with(identity_at(
            "00000000-dead-beef-0000-000000000000",
            u32::MAX - 1,
            5,
        ));
        write_strict(tmp.path(), |_| Ok(marker.clone())).unwrap();
        let Some(LifecycleMarker::Running { identity, .. }) = read(tmp.path()) else {
            panic!("marker round-trips");
        };
        assert_eq!(recorded_process(&identity), RecordedProcess::Ended);
        report_previous_exit(read(tmp.path()).as_ref());
    }

    #[test]
    fn unclean_exit_is_inferred_only_from_a_running_marker_of_a_gone_process() {
        let running = |pid| LifecycleMarker::Running {
            pid,
            started_at: "2026-01-01T00:00:00Z".into(),
            launch_route: Some(LaunchRoute::Supervised),
            pending_latched_drain: None,
            carrier_incarnation: None,
        };
        assert_eq!(
            inferred_unclean_exit(Some(&running(u32::MAX - 1))),
            Some(UncleanExit {
                pid: u32::MAX - 1,
                started_at: "2026-01-01T00:00:00Z".into(),
                launch_route: Some(LaunchRoute::Supervised),
            })
        );
        // A previous run that reused this process's pid is still a previous run.
        assert!(inferred_unclean_exit(Some(&running(std::process::id()))).is_some());
        assert!(inferred_unclean_exit(None).is_none());
        let exited = exited_marker(
            "signal",
            "2026-01-01T00:00:00Z",
            None,
            LaunchRoute::Direct,
            None,
            None,
        );
        assert!(inferred_unclean_exit(Some(&exited)).is_none());
    }

    #[test]
    fn provisional_finalizing_marker_completes_only_for_its_own_run() {
        let tmp = tempfile::tempdir().unwrap();
        let started_at = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        record_exit(
            tmp.path(),
            FINALIZING_REASON,
            &started_at,
            Some("staged reason: signal"),
            LaunchRoute::Direct,
        );
        report_previous_exit(read(tmp.path()).as_ref());
        assert!(!complete_finalizing_exit(
            tmp.path(),
            "signal",
            "another-start",
            None,
            LaunchRoute::Direct,
        ));
        assert!(complete_finalizing_exit(
            tmp.path(),
            "signal",
            &started_at,
            None,
            LaunchRoute::Direct,
        ));
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Exited { reason, .. }) if reason == "signal"
        ));
        assert!(!complete_finalizing_exit(
            tmp.path(),
            "runtime_error",
            &started_at,
            None,
            LaunchRoute::Direct,
        ));
        record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        assert!(!complete_finalizing_exit(
            tmp.path(),
            "signal",
            &started_at,
            None,
            LaunchRoute::Direct,
        ));
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Running { .. })
        ));
    }

    fn latched_facts(stopped: &[u8; 32]) -> LatchedDrainFacts {
        LatchedDrainFacts {
            outcome: LatchedStopOutcome::ServiceWindowRollover,
            incarnation_digest: incarnation_digest_hex(stopped),
            resume_attempts_reset_deferred: BTreeMap::new(),
        }
    }

    #[test]
    fn latched_drain_facts_survive_completion_and_bind_to_their_successor_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let stopped = [1u8; 32];
        let started_at = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        let facts = latched_facts(&stopped);
        record_exit_with_latched_drain(
            tmp.path(),
            FINALIZING_REASON,
            &started_at,
            Some("staged reason: rollover"),
            LaunchRoute::Direct,
            &facts,
        )
        .unwrap();
        assert!(complete_finalizing_exit(
            tmp.path(),
            "service_window_rollover",
            &started_at,
            None,
            LaunchRoute::Direct,
        ));
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Exited { latched_drain: Some(ref carried), .. }) if carried == &facts
        ));
        assert!(take_previous_exit(tmp.path()).unwrap().is_some());

        // The first start carries them and binds them to its own debit.
        let first_start = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        let first = [2u8; 32];
        assert_eq!(
            bind_pending_latched_drain(tmp.path(), &first_start, &first, Some(&stopped)).unwrap(),
            Some(facts.clone())
        );
        // A crash before recovery: the next start accepts them only when its
        // predecessor is the carrier.
        let second_start = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        let second = [3u8; 32];
        assert_eq!(
            bind_pending_latched_drain(tmp.path(), &second_start, &second, Some(&first)).unwrap(),
            Some(facts.clone())
        );
        clear_pending_latched_drain(tmp.path(), &second_start).unwrap();
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Running {
                pending_latched_drain: None,
                carrier_incarnation: None,
                ..
            })
        ));
    }

    #[test]
    fn latched_drain_facts_from_another_predecessor_are_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let stopped = [1u8; 32];
        let started_at = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        record_exit_with_latched_drain(
            tmp.path(),
            FINALIZING_REASON,
            &started_at,
            None,
            LaunchRoute::Direct,
            &latched_facts(&stopped),
        )
        .unwrap();
        let next_start = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        assert_eq!(
            bind_pending_latched_drain(tmp.path(), &next_start, &[2u8; 32], Some(&[9u8; 32]))
                .unwrap(),
            None
        );
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Running {
                pending_latched_drain: None,
                ..
            })
        ));
    }

    fn thread_identity(pid: u32) -> ryeos_app::process::ExecutionProcessIdentity {
        ryeos_app::process::ExecutionProcessIdentity {
            schema_version: 1,
            boot_id: "00000000-0000-0000-0000-000000000000".to_owned(),
            target_pid: i64::from(pid),
            target_start_time_ticks: 1,
            group_leader_pid: i64::from(pid),
            group_leader_start_time_ticks: 1,
            pid_namespace: "pid:[4026531836]".into(),
            process_scope: None,
            resource_selections: Vec::new(),
            resource_settlement_authority: None,
            resource_operations: Vec::new(),
            resource_allocation_limit: None,
            resource_occupancy_start: None,
            resource_occupancy_limit: None,
            resource_cleanup_allowance_ms: None,
        }
    }

    #[test]
    fn a_later_latched_stop_merges_unapplied_facts_by_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let first_stop = [1u8; 32];
        let started_at = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        let mut older = latched_facts(&first_stop);
        older.outcome = LatchedStopOutcome::ServiceRefusalStop;
        older
            .resume_attempts_reset_deferred
            .insert("T-a".to_owned(), thread_identity(10));
        older
            .resume_attempts_reset_deferred
            .insert("T-b".to_owned(), thread_identity(11));
        record_exit_with_latched_drain(
            tmp.path(),
            FINALIZING_REASON,
            &started_at,
            None,
            LaunchRoute::Direct,
            &older,
        )
        .unwrap();

        // The next start carries them but stops again before recovery
        // applied them; its own stop names one of the same threads.
        let next_start = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        let second_stop = [2u8; 32];
        bind_pending_latched_drain(tmp.path(), &next_start, &second_stop, Some(&first_stop))
            .unwrap();
        let mut newer = latched_facts(&second_stop);
        newer
            .resume_attempts_reset_deferred
            .insert("T-b".to_owned(), thread_identity(21));
        newer
            .resume_attempts_reset_deferred
            .insert("T-c".to_owned(), thread_identity(22));
        record_exit_with_latched_drain(
            tmp.path(),
            FINALIZING_REASON,
            &next_start,
            None,
            LaunchRoute::Direct,
            &newer,
        )
        .unwrap();

        let Some(LifecycleMarker::Exited {
            latched_drain: Some(merged),
            carrier_incarnation,
            ..
        }) = read(tmp.path())
        else {
            panic!("expected an exit carrying merged facts");
        };
        assert_eq!(carrier_incarnation, None);
        assert_eq!(merged.outcome, LatchedStopOutcome::ServiceWindowRollover);
        assert_eq!(
            merged.incarnation_digest,
            incarnation_digest_hex(&second_stop)
        );
        let threads: Vec<&str> = merged
            .resume_attempts_reset_deferred
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(threads, ["T-a", "T-b", "T-c"]);
        assert_eq!(
            merged.resume_attempts_reset_deferred["T-a"],
            thread_identity(10)
        );
        assert_eq!(
            merged.resume_attempts_reset_deferred["T-b"],
            thread_identity(21)
        );
    }

    #[test]
    fn only_this_exact_start_may_bind_or_clear_carried_facts() {
        let tmp = tempfile::tempdir().unwrap();
        let stopped = [1u8; 32];
        let started_at = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        record_exit_with_latched_drain(
            tmp.path(),
            FINALIZING_REASON,
            &started_at,
            None,
            LaunchRoute::Direct,
            &latched_facts(&stopped),
        )
        .unwrap();
        let this_start = record_running(tmp.path(), LaunchRoute::Direct).unwrap();
        // Same pid, different start: a recycled pid or an earlier attempt.
        let error = bind_pending_latched_drain(
            tmp.path(),
            "1970-01-01T00:00:00Z",
            &[2u8; 32],
            Some(&stopped),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("another run"), "{error}");
        assert!(clear_pending_latched_drain(tmp.path(), "1970-01-01T00:00:00Z").is_err());
        assert!(matches!(
            read(tmp.path()),
            Some(LifecycleMarker::Running {
                pending_latched_drain: Some(_),
                ..
            })
        ));
        assert!(
            bind_pending_latched_drain(tmp.path(), &this_start, &[2u8; 32], Some(&stopped))
                .unwrap()
                .is_some()
        );
        clear_pending_latched_drain(tmp.path(), &this_start).unwrap();
    }

    #[test]
    fn unreadable_previous_marker_fails_closed_and_is_retained() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(MARKER_FILE), b"not json").unwrap();
        assert!(take_previous_exit(tmp.path()).is_err());
        assert!(record_running(tmp.path(), LaunchRoute::Direct).is_err());
        assert_eq!(
            std::fs::read(tmp.path().join(MARKER_FILE)).unwrap(),
            b"not json"
        );
    }

    #[test]
    fn disk_check_is_best_effort_and_never_panics() {
        let tmp = tempfile::tempdir().unwrap();
        check_disk_space(tmp.path());
        // A non-existent path must not panic; the probe is best-effort,
        // so the value itself is platform-dependent and unasserted.
        let _ = available_bytes(&tmp.path().join("does-not-exist"));
    }
}
