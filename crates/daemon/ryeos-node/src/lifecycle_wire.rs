use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Deserialize a nullable wire field while still requiring its key to be
/// present. Serde otherwise treats a missing `Option<T>` as `None`, which
/// would make the versioned lifecycle contract a compatibility reader.
fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Version of the local lifecycle status contract carried by
/// `lifecycle.status` over the daemon UDS.
pub const LIFECYCLE_PROTOCOL_VERSION: u32 = 1;

/// Maximum MessagePack frame accepted by either side of the local lifecycle
/// transport. Keeping the ceiling in the shared wire contract prevents the
/// daemon and lifecycle clients from silently drifting apart.
pub const LIFECYCLE_FRAME_MAX_BYTES: u32 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleWireState {
    Starting,
    Running,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StartupPhase {
    Bootstrapping,
    OpeningProjection,
    RebuildingProjection,
    ReplayingHeadChanges,
    RecoveringSchedulerProjection,
    ReconcilingThreads,
    ReconcilingFollow,
    ReconcilingScheduler,
    Ready,
    Failed,
}

impl StartupPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bootstrapping => "bootstrapping",
            Self::OpeningProjection => "opening_projection",
            Self::RebuildingProjection => "rebuilding_projection",
            Self::ReplayingHeadChanges => "replaying_head_changes",
            Self::RecoveringSchedulerProjection => "recovering_scheduler_projection",
            Self::ReconcilingThreads => "reconciling_threads",
            Self::ReconcilingFollow => "reconciling_follow",
            Self::ReconcilingScheduler => "reconciling_scheduler",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// Machine-readable startup progress. Counters are monotonic within a phase;
/// `elapsed_ms` is process-start elapsed time at the instant of the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StartupSnapshot {
    /// Monotonic coordinator publication sequence.
    pub sequence: u64,
    pub phase: StartupPhase,
    pub started_at: String,
    pub phase_started_at: String,
    pub updated_at: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub ready_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub failed_at: Option<String>,
    pub elapsed_ms: u64,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub chains_total: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub chains_done: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub threads_restored: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub events_projected: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub pending_head_changes: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub recovery_threads: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub message: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub error: Option<String>,
}

impl StartupSnapshot {
    pub fn bootstrapping(started_at: impl Into<String>) -> Self {
        let started_at = started_at.into();
        Self {
            sequence: 0,
            phase: StartupPhase::Bootstrapping,
            phase_started_at: started_at.clone(),
            updated_at: started_at.clone(),
            started_at,
            ready_at: None,
            failed_at: None,
            elapsed_ms: 0,
            chains_total: None,
            chains_done: None,
            threads_restored: None,
            events_projected: None,
            pending_head_changes: None,
            recovery_threads: None,
            message: None,
            error: None,
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.started_at.trim().is_empty()
            || self.phase_started_at.trim().is_empty()
            || self.updated_at.trim().is_empty()
        {
            return Err("startup timestamps must not be empty");
        }
        if let (Some(done), Some(total)) = (self.chains_done, self.chains_total) {
            if done > total {
                return Err("startup chains_done cannot exceed chains_total");
            }
        }
        if self
            .error
            .as_deref()
            .is_some_and(|error| error.trim().is_empty() || error.trim() != error)
        {
            return Err("startup error must be non-empty and trimmed when present");
        }
        Ok(())
    }
}

/// Stable identity fields returned throughout boot. The listener can publish
/// this before the application state or projection exists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LifecycleIdentity {
    pub pid: u32,
    pub bind: String,
    pub uds_path: PathBuf,
    pub app_root: PathBuf,
    pub started_at: String,
    pub version: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub revision: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub build_date: Option<String>,
}

/// Versioned `lifecycle.status` result. `Running` is valid only when `ready`
/// is true; clients deliberately reject inconsistent responses.
#[derive(Debug, Clone, PartialEq)]
pub struct LifecycleResponse {
    pub schema: u32,
    pub status: LifecycleWireState,
    pub ready: bool,
    pub identity: LifecycleIdentity,
    pub ready_at: Option<String>,
    pub failed_at: Option<String>,
    pub startup: StartupSnapshot,
    /// Required nullable local failure detail. Public HTTP health sanitizes
    /// this value; the authenticated local lifecycle surface returns it.
    pub error: Option<String>,
    pub thread_projection: Option<serde_json::Value>,
}

/// Exact flat v1 wire shape. A dedicated DTO avoids Serde's unsupported
/// `flatten` + `deny_unknown_fields` combination while keeping the public Rust
/// API's lifecycle identity grouped as one value.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LifecycleResponseWire {
    schema: u32,
    status: LifecycleWireState,
    ready: bool,
    pid: u32,
    bind: String,
    uds_path: PathBuf,
    app_root: PathBuf,
    started_at: String,
    version: String,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    revision: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    build_date: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    ready_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    failed_at: Option<String>,
    startup: StartupSnapshot,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    error: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    thread_projection: Option<serde_json::Value>,
}

impl Serialize for LifecycleResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        LifecycleResponseWire {
            schema: self.schema,
            status: self.status,
            ready: self.ready,
            pid: self.identity.pid,
            bind: self.identity.bind.clone(),
            uds_path: self.identity.uds_path.clone(),
            app_root: self.identity.app_root.clone(),
            started_at: self.identity.started_at.clone(),
            version: self.identity.version.clone(),
            revision: self.identity.revision.clone(),
            build_date: self.identity.build_date.clone(),
            ready_at: self.ready_at.clone(),
            failed_at: self.failed_at.clone(),
            startup: self.startup.clone(),
            error: self.error.clone(),
            thread_projection: self.thread_projection.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LifecycleResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = LifecycleResponseWire::deserialize(deserializer)?;
        Ok(Self {
            schema: wire.schema,
            status: wire.status,
            ready: wire.ready,
            identity: LifecycleIdentity {
                pid: wire.pid,
                bind: wire.bind,
                uds_path: wire.uds_path,
                app_root: wire.app_root,
                started_at: wire.started_at,
                version: wire.version,
                revision: wire.revision,
                build_date: wire.build_date,
            },
            ready_at: wire.ready_at,
            failed_at: wire.failed_at,
            startup: wire.startup,
            error: wire.error,
            thread_projection: wire.thread_projection,
        })
    }
}

impl LifecycleResponse {
    pub fn starting(identity: LifecycleIdentity, startup: StartupSnapshot) -> Self {
        Self {
            schema: LIFECYCLE_PROTOCOL_VERSION,
            status: LifecycleWireState::Starting,
            ready: false,
            identity,
            ready_at: None,
            failed_at: None,
            startup,
            error: None,
            thread_projection: None,
        }
    }

    pub fn running(
        identity: LifecycleIdentity,
        ready_at: impl Into<String>,
        mut startup: StartupSnapshot,
    ) -> Self {
        let ready_at = ready_at.into();
        startup.sequence = startup.sequence.saturating_add(1);
        startup.phase = StartupPhase::Ready;
        startup.phase_started_at = ready_at.clone();
        startup.updated_at = ready_at.clone();
        startup.ready_at = Some(ready_at.clone());
        startup.failed_at = None;
        startup.error = None;
        Self {
            schema: LIFECYCLE_PROTOCOL_VERSION,
            status: LifecycleWireState::Running,
            ready: true,
            identity,
            ready_at: Some(ready_at),
            failed_at: None,
            startup,
            error: None,
            thread_projection: None,
        }
    }

    pub fn failed(
        identity: LifecycleIdentity,
        failed_at: impl Into<String>,
        error: impl Into<String>,
        mut startup: StartupSnapshot,
    ) -> Self {
        let failed_at = failed_at.into();
        let error = error.into();
        startup.sequence = startup.sequence.saturating_add(1);
        startup.phase = StartupPhase::Failed;
        startup.phase_started_at = failed_at.clone();
        startup.updated_at = failed_at.clone();
        startup.ready_at = None;
        startup.failed_at = Some(failed_at.clone());
        startup.error = Some(error.clone());
        Self {
            schema: LIFECYCLE_PROTOCOL_VERSION,
            status: LifecycleWireState::Failed,
            ready: false,
            identity,
            ready_at: None,
            failed_at: Some(failed_at),
            startup,
            error: Some(error),
            thread_projection: None,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != LIFECYCLE_PROTOCOL_VERSION {
            return Err("unsupported lifecycle protocol version");
        }
        self.startup.validate()?;
        if self.startup.started_at != self.identity.started_at {
            return Err("lifecycle identity and startup timestamps disagree");
        }
        match self.status {
            LifecycleWireState::Starting
                if !self.ready
                    && self.ready_at.is_none()
                    && self.failed_at.is_none()
                    && self.startup.ready_at.is_none()
                    && self.startup.failed_at.is_none()
                    && self.error.is_none()
                    && self.startup.error.is_none()
                    && self.startup.phase != StartupPhase::Ready
                    && self.startup.phase != StartupPhase::Failed =>
            {
                Ok(())
            }
            LifecycleWireState::Running
                if self.ready
                    && self.ready_at.is_some()
                    && self.failed_at.is_none()
                    && self.ready_at == self.startup.ready_at
                    && self.startup.failed_at.is_none()
                    && self.error.is_none()
                    && self.startup.error.is_none()
                    && self.startup.phase == StartupPhase::Ready =>
            {
                Ok(())
            }
            LifecycleWireState::Failed
                if !self.ready
                    && self.ready_at.is_none()
                    && self.failed_at.is_some()
                    && self.failed_at == self.startup.failed_at
                    && self.startup.ready_at.is_none()
                    && self.startup.phase == StartupPhase::Failed
                    && self.error.is_some()
                    && self.error == self.startup.error =>
            {
                Ok(())
            }
            _ => Err("inconsistent lifecycle status, readiness, and startup phase"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> LifecycleIdentity {
        LifecycleIdentity {
            pid: 42,
            bind: "127.0.0.1:7400".into(),
            uds_path: "/tmp/ryeosd.sock".into(),
            app_root: "/tmp/ryeos".into(),
            started_at: "2026-07-14T00:00:00Z".into(),
            version: "test".into(),
            revision: None,
            build_date: None,
        }
    }

    #[test]
    fn running_requires_ready_and_ready_phase() {
        let mut response = LifecycleResponse::running(
            identity(),
            "2026-07-14T00:00:01Z",
            StartupSnapshot::bootstrapping("2026-07-14T00:00:00Z"),
        );
        assert_eq!(response.validate(), Ok(()));
        response.ready = false;
        assert!(response.validate().is_err());
    }

    #[test]
    fn failed_requires_concrete_error() {
        let response = LifecycleResponse::failed(
            identity(),
            "2026-07-14T00:00:01Z",
            "",
            StartupSnapshot::bootstrapping("2026-07-14T00:00:00Z"),
        );
        assert!(response.validate().is_err());
    }

    #[test]
    fn lifecycle_rejects_impossible_progress() {
        let mut response = LifecycleResponse::starting(
            identity(),
            StartupSnapshot::bootstrapping("2026-07-14T00:00:00Z"),
        );
        response.startup.chains_total = Some(1);
        response.startup.chains_done = Some(2);
        assert_eq!(
            response.validate(),
            Err("startup chains_done cannot exceed chains_total")
        );
    }

    #[test]
    fn lifecycle_requires_every_nullable_wire_key() {
        let complete = serde_json::to_value(LifecycleResponse::starting(
            identity(),
            StartupSnapshot::bootstrapping("2026-07-14T00:00:00Z"),
        ))
        .unwrap();

        for key in [
            "revision",
            "build_date",
            "ready_at",
            "failed_at",
            "error",
            "thread_projection",
        ] {
            let mut missing = complete.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<LifecycleResponse>(missing).is_err(),
                "missing nullable lifecycle key {key} must fail exact decoding"
            );
        }

        for key in [
            "ready_at",
            "failed_at",
            "chains_total",
            "chains_done",
            "threads_restored",
            "events_projected",
            "pending_head_changes",
            "recovery_threads",
            "message",
            "error",
        ] {
            let mut missing = complete.clone();
            missing["startup"].as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<LifecycleResponse>(missing).is_err(),
                "missing nullable startup key {key} must fail exact decoding"
            );
        }

        let mut unknown = complete;
        unknown["compat_status"] = serde_json::json!("booting");
        assert!(serde_json::from_value::<LifecycleResponse>(unknown).is_err());

        let mut unknown_identity = serde_json::to_value(identity()).unwrap();
        unknown_identity["socket"] = serde_json::json!("/tmp/legacy.sock");
        assert!(serde_json::from_value::<LifecycleIdentity>(unknown_identity).is_err());
    }
}
