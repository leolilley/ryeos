//! Shared types for the scheduler module.

use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use ryeos_engine::execution_contract::{
    ExecutionOwnership, ExecutionPolicy, ExecutionRecovery, ExecutionResponse, ExecutionTarget,
    ProjectExecutionPolicy,
};
use ryeos_engine::principal_contract::{AuthenticatedGrantAuthority, AuthorizedKeyPrincipalClass};

pub const NODE_MAINTENANCE_POLICY_SOURCE: &str = "policies/maintenance.yaml";

/// The one current wire shape for signed schedule sources.
///
/// Every execution/projection consumer deserializes this type before using a
/// schedule. Policy-owned document regeneration authenticates ownership
/// separately; it never reuses the replaced execution payload.
/// Unknown fields and partial nested records are
/// rejected instead of being filtered into a different effective schedule.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleSourceRecord {
    pub spec_version: u32,
    pub schedule_id: String,
    pub item_ref: String,
    pub ref_bindings: BTreeMap<String, String>,
    pub schedule_type: String,
    pub expression: String,
    pub params: Value,
    pub timezone: String,
    pub misfire_policy: String,
    pub overlap_policy: String,
    pub lateness_grace_secs: i64,
    pub enabled: bool,
    pub project_root: Option<String>,
    pub registered_at: i64,
    pub execution: ScheduleExecution,
    pub managed_by: Option<ScheduleManagedBy>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScheduleSourceWire {
    spec_version: u32,
    schedule_id: String,
    item_ref: String,
    ref_bindings: BTreeMap<String, String>,
    schedule_type: String,
    expression: String,
    params: Value,
    timezone: String,
    misfire_policy: String,
    overlap_policy: String,
    lateness_grace_secs: i64,
    enabled: bool,
    // Values, rather than Options, deliberately make omission a serde error.
    // Explicit null is decoded into None below.
    project_root: Value,
    registered_at: i64,
    execution: ScheduleExecution,
    managed_by: Value,
}

impl<'de> Deserialize<'de> for ScheduleSourceRecord {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ScheduleSourceWire::deserialize(deserializer)?;
        let project_root = serde_json::from_value::<Option<String>>(wire.project_root)
            .map_err(D::Error::custom)?;
        let managed_by = serde_json::from_value::<Option<ScheduleManagedBy>>(wire.managed_by)
            .map_err(D::Error::custom)?;
        Ok(Self {
            spec_version: wire.spec_version,
            schedule_id: wire.schedule_id,
            item_ref: wire.item_ref,
            ref_bindings: wire.ref_bindings,
            schedule_type: wire.schedule_type,
            expression: wire.expression,
            params: wire.params,
            timezone: wire.timezone,
            misfire_policy: wire.misfire_policy,
            overlap_policy: wire.overlap_policy,
            lateness_grace_secs: wire.lateness_grace_secs,
            enabled: wire.enabled,
            project_root,
            registered_at: wire.registered_at,
            execution: wire.execution,
            managed_by,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleExecution {
    pub authority: ScheduleExecutionAuthority,
    pub capabilities: Vec<String>,
    pub policy: ExecutionPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleExecutionAuthority {
    Node {
        principal_id: String,
        effective_origin_site_id: String,
    },
    Authenticated {
        principal_id: String,
        principal_class: AuthorizedKeyPrincipalClass,
        effective_origin_site_id: String,
        grant_authority: AuthenticatedGrantAuthority,
        /// Canonical digest of the typed schedule-registration request. This
        /// is semantic request identity, not the ingress nonce or signature.
        registration_request_hash: String,
    },
}

impl ScheduleExecutionAuthority {
    pub fn principal_id(&self) -> &str {
        match self {
            Self::Node { principal_id, .. } | Self::Authenticated { principal_id, .. } => {
                principal_id
            }
        }
    }

    pub fn origin_site_id(&self) -> &str {
        match self {
            Self::Node {
                effective_origin_site_id,
                ..
            }
            | Self::Authenticated {
                effective_origin_site_id,
                ..
            } => effective_origin_site_id,
        }
    }
}

impl ScheduleExecution {
    pub fn principal_id(&self) -> &str {
        self.authority.principal_id()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ScheduleManagedBy {
    #[serde(rename = "project_ai_sync")]
    ProjectAiSync {
        project_root: String,
        project_key: String,
        source_snapshot_hash: String,
        source_path: String,
        source_body_hash: String,
    },
    #[serde(rename = "node_maintenance_policy")]
    NodeMaintenancePolicy { source: String },
}

impl ScheduleSourceRecord {
    pub fn validate(&self, expected_id: Option<&str>) -> Result<()> {
        if self.spec_version != 2 {
            anyhow::bail!(
                "schedule {} declares unsupported spec_version {}",
                self.schedule_id,
                self.spec_version
            );
        }
        super::crontab::validate_schedule_id(&self.schedule_id)?;
        if let Some(expected_id) = expected_id
            && self.schedule_id != expected_id
        {
            anyhow::bail!(
                "schedule record declares schedule_id '{}' but filename is '{}'",
                self.schedule_id,
                expected_id
            );
        }
        ryeos_engine::canonical_ref::CanonicalRef::parse(&self.item_ref)
            .with_context(|| format!("invalid scheduled item_ref: {}", self.item_ref))?;
        validate_schedule_ref_bindings(&self.ref_bindings)?;
        super::crontab::validate_expression(&self.schedule_type, &self.expression)?;
        super::crontab::validate_timezone(&self.timezone)?;
        super::misfire::parse_misfire_policy(&self.misfire_policy)?;
        super::overlap::parse_overlap_policy(&self.overlap_policy)?;
        if self.lateness_grace_secs <= 0 {
            anyhow::bail!(
                "lateness_grace_secs must be positive, got {}",
                self.lateness_grace_secs
            );
        }
        if !self.params.is_object() {
            anyhow::bail!("schedule params must be a mapping");
        }
        if self.registered_at < 0 {
            anyhow::bail!("schedule registered_at must not be negative");
        }
        require_non_empty(
            self.execution.authority.principal_id(),
            "execution.authority.principal_id",
        )?;
        if self.execution.capabilities.is_empty() {
            anyhow::bail!("schedule execution.capabilities must not be empty");
        }
        for capability in &self.execution.capabilities {
            require_non_empty(capability, "execution.capabilities entry")?;
        }
        validate_schedule_execution(&self.execution)?;
        if let Some(project_root) = &self.project_root {
            validate_canonical_absolute_path(project_root, "project_root")?;
        }
        match (&self.execution.policy.project, &self.project_root) {
            (ProjectExecutionPolicy::Projectless, None) => {}
            (ProjectExecutionPolicy::Projectless, Some(_)) => {
                anyhow::bail!("projectless scheduled execution must not declare project_root")
            }
            (_, Some(_)) => {}
            (_, None) => anyhow::bail!("project-backed scheduled execution requires project_root"),
        }
        if let Some(managed_by) = &self.managed_by {
            validate_managed_by(managed_by, self.project_root.as_deref())?;
        }
        Ok(())
    }

    pub fn to_spec_record(
        &self,
        signer_fingerprint: &str,
        spec_hash: &str,
    ) -> Result<ScheduleSpecRecord> {
        self.validate(None)?;
        if !is_canonical_hash(signer_fingerprint) {
            anyhow::bail!("schedule signer_fingerprint must be lowercase SHA-256 hex");
        }
        if !is_canonical_hash(spec_hash) {
            anyhow::bail!("schedule spec_hash must be lowercase SHA-256 hex");
        }
        let record = ScheduleSpecRecord {
            schedule_id: self.schedule_id.clone(),
            item_ref: self.item_ref.clone(),
            ref_bindings: self.ref_bindings.clone(),
            params: serde_json::to_string(&self.params)
                .context("encode authored schedule params")?,
            schedule_type: self.schedule_type.clone(),
            expression: self.expression.clone(),
            timezone: self.timezone.clone(),
            misfire_policy: self.misfire_policy.clone(),
            overlap_policy: self.overlap_policy.clone(),
            lateness_grace_secs: self.lateness_grace_secs,
            enabled: self.enabled,
            project_root: self.project_root.clone(),
            signer_fingerprint: signer_fingerprint.to_owned(),
            spec_hash: spec_hash.to_owned(),
            registered_at: self.registered_at,
            execution: self.execution.clone(),
        };
        validate_schedule_spec_record(&record)?;
        Ok(record)
    }
}

fn validate_managed_by(
    managed_by: &ScheduleManagedBy,
    schedule_project_root: Option<&str>,
) -> Result<()> {
    match managed_by {
        ScheduleManagedBy::ProjectAiSync {
            project_root,
            project_key,
            source_snapshot_hash,
            source_path,
            source_body_hash,
        } => {
            validate_canonical_absolute_path(project_root, "managed_by.project_root")?;
            if schedule_project_root != Some(project_root.as_str()) {
                anyhow::bail!(
                    "project-managed schedule project_root must exactly match managed_by.project_root"
                );
            }
            for (value, field) in [
                (project_key, "managed_by.project_key"),
                (source_snapshot_hash, "managed_by.source_snapshot_hash"),
                (source_body_hash, "managed_by.source_body_hash"),
            ] {
                if !is_canonical_hash(value) {
                    anyhow::bail!("schedule {field} must be lowercase SHA-256 hex");
                }
            }
            validate_canonical_source_path(source_path)?;
        }
        ScheduleManagedBy::NodeMaintenancePolicy { source } => {
            if source != NODE_MAINTENANCE_POLICY_SOURCE {
                anyhow::bail!(
                    "node-maintenance schedule source must be {}",
                    NODE_MAINTENANCE_POLICY_SOURCE
                );
            }
            if schedule_project_root.is_some() {
                anyhow::bail!("node-maintenance schedule must not declare project_root");
            }
        }
    }
    Ok(())
}

fn require_non_empty(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("schedule {field} must be non-empty");
    }
    Ok(())
}

pub fn validate_schedule_ref_bindings(ref_bindings: &BTreeMap<String, String>) -> Result<()> {
    if ref_bindings.len() > 32 {
        anyhow::bail!("schedule ref_bindings exceeds the limit of 32");
    }
    for (name, raw_ref) in ref_bindings {
        let mut segments = name.split('_');
        let valid_name = !name.is_empty()
            && name.len() <= 64
            && segments.next().is_some_and(|segment| {
                segment
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_lowercase)
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            })
            && segments.all(|segment| {
                !segment.is_empty()
                    && segment
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            });
        if !valid_name {
            anyhow::bail!(
                "schedule ref binding names must be lower snake case and at most 64 bytes"
            );
        }
        if raw_ref.len() > 2_048 {
            anyhow::bail!("schedule ref binding '{name}' exceeds 2048 UTF-8 bytes");
        }
        let canonical = ryeos_engine::canonical_ref::CanonicalRef::parse(raw_ref)
            .with_context(|| format!("schedule ref binding '{name}' is not a canonical ref"))?;
        if canonical.to_string() != *raw_ref {
            anyhow::bail!("schedule ref binding '{name}' is not in canonical form");
        }
    }
    Ok(())
}

fn is_canonical_hash(value: &str) -> bool {
    lillux::cas::valid_hash(value) && value.bytes().all(|byte| !byte.is_ascii_uppercase())
}

fn validate_canonical_absolute_path(value: &str, field: &str) -> Result<()> {
    require_non_empty(value, field)?;
    let path = Path::new(value);
    if !path.is_absolute() {
        anyhow::bail!("schedule {field} must be an absolute path");
    }
    let mut rebuilt = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Normal(_) => rebuilt.push(component.as_os_str()),
            _ => anyhow::bail!("schedule {field} must be lexically normalized"),
        }
    }
    if rebuilt.to_string_lossy() != value {
        anyhow::bail!("schedule {field} must be lexically normalized UTF-8");
    }
    Ok(())
}

fn validate_canonical_source_path(value: &str) -> Result<()> {
    require_non_empty(value, "managed_by.source_path")?;
    if value.contains('\\') {
        anyhow::bail!("schedule managed_by.source_path must use '/' separators");
    }
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || !path.starts_with(".ai/config/schedules")
        || path.extension().and_then(|extension| extension.to_str()) != Some("yaml")
    {
        anyhow::bail!(
            "schedule managed_by.source_path must be a contained canonical .ai/config/schedules/*.yaml path"
        );
    }
    let rebuilt = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if rebuilt != value {
        anyhow::bail!("schedule managed_by.source_path must be lexically normalized UTF-8");
    }
    Ok(())
}

/// Projection record for a schedule spec in `scheduler.sqlite3`.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleSpecRecord {
    pub schedule_id: String,
    pub item_ref: String,
    pub ref_bindings: BTreeMap<String, String>,
    pub params: String,
    pub schedule_type: String,
    pub expression: String,
    pub timezone: String,
    pub misfire_policy: String,
    pub overlap_policy: String,
    pub lateness_grace_secs: i64,
    pub enabled: bool,
    pub project_root: Option<String>,
    pub signer_fingerprint: String,
    pub spec_hash: String,
    /// Immutable scheduling anchor (millis since epoch).
    /// Set once at creation, preserved on updates. Used by timer for fire-time calculation.
    pub registered_at: i64,
    /// Complete admitted execution authority and ordinary execution policy.
    pub execution: ScheduleExecution,
}

/// Validate the complete, authored schedule contract before it enters the
/// projection. Policy fields are never inferred here: omission/invalid data is
/// a source-spec error and must be corrected in signed configuration.
pub fn validate_schedule_spec_record(record: &ScheduleSpecRecord) -> Result<()> {
    super::crontab::validate_schedule_id(&record.schedule_id)?;
    ryeos_engine::canonical_ref::CanonicalRef::parse(&record.item_ref)
        .with_context(|| format!("invalid scheduled item_ref: {}", record.item_ref))?;
    validate_schedule_ref_bindings(&record.ref_bindings)?;
    super::crontab::validate_expression(&record.schedule_type, &record.expression)?;
    super::crontab::validate_timezone(&record.timezone)?;
    super::misfire::parse_misfire_policy(&record.misfire_policy)?;
    super::overlap::parse_overlap_policy(&record.overlap_policy)?;
    if record.lateness_grace_secs <= 0 {
        anyhow::bail!(
            "lateness_grace_secs must be positive, got {}",
            record.lateness_grace_secs
        );
    }
    let params: serde_json::Value =
        serde_json::from_str(&record.params).context("schedule params must be valid JSON")?;
    if !params.is_object() {
        anyhow::bail!("schedule params must be a JSON object");
    }
    if record.registered_at < 0 {
        anyhow::bail!("schedule registered_at must not be negative");
    }
    if !is_canonical_hash(&record.signer_fingerprint) {
        anyhow::bail!("schedule signer_fingerprint must be lowercase SHA-256 hex");
    }
    if !is_canonical_hash(&record.spec_hash) {
        anyhow::bail!("schedule spec_hash must be lowercase SHA-256 hex");
    }
    if record.execution.capabilities.is_empty()
        || record
            .execution
            .capabilities
            .iter()
            .any(|capability| capability.trim().is_empty())
    {
        anyhow::bail!("schedule capabilities must contain only non-empty strings");
    }
    if let Some(project_root) = &record.project_root {
        validate_canonical_absolute_path(project_root, "project_root")?;
    }
    match (&record.execution.policy.project, &record.project_root) {
        (ProjectExecutionPolicy::Projectless, None) => {}
        (ProjectExecutionPolicy::Projectless, Some(_)) => {
            anyhow::bail!("projectless scheduled execution must not declare project_root")
        }
        (_, Some(_)) => {}
        (_, None) => anyhow::bail!("project-backed scheduled execution requires project_root"),
    }
    validate_schedule_execution(&record.execution)?;
    Ok(())
}

fn validate_schedule_execution(execution: &ScheduleExecution) -> Result<()> {
    execution.policy.validate()?;
    if execution.policy.ownership != ExecutionOwnership::DaemonOwned
        || execution.policy.recovery != ExecutionRecovery::RestartRecoverable
        || execution.policy.response != ExecutionResponse::Accepted
        || execution.policy.target != ExecutionTarget::Here
    {
        anyhow::bail!(
            "scheduled execution must be daemon-owned, restart-recoverable, accepted, and targeted here"
        );
    }
    // Scheduling does not redefine the ordinary live-versus-pinned policy
    // distinction. A signed `live_direct` policy deliberately follows the
    // named live path; current-head and explicit-snapshot policies retain one
    // immutable authority for the fire. Capture-live remains an interactive
    // admission operation: a recurring schedule must either follow live bytes
    // explicitly or point at already-durable project state.
    if matches!(
        &execution.policy.project,
        ProjectExecutionPolicy::Pinned {
            source: ryeos_engine::execution_contract::PinnedSource::CaptureLive { .. },
            ..
        }
    ) {
        anyhow::bail!(
            "scheduled execution cannot use capture_live; select explicit live_direct, current_head, or a snapshot"
        );
    }
    if execution.capabilities.is_empty() {
        anyhow::bail!("schedule execution.capabilities must not be empty");
    }
    let mut previous: Option<&str> = None;
    for capability in &execution.capabilities {
        require_non_empty(capability, "execution.capabilities entry")?;
        if previous.is_some_and(|value| value >= capability.as_str()) {
            anyhow::bail!("schedule execution.capabilities must be sorted and unique");
        }
        previous = Some(capability);
    }
    ryeos_engine::principal_contract::validate_canonical_site_id(
        execution.authority.origin_site_id(),
    )?;
    match &execution.authority {
        ScheduleExecutionAuthority::Node { principal_id, .. } => {
            validate_principal_id(principal_id)?;
        }
        ScheduleExecutionAuthority::Authenticated {
            principal_id,
            principal_class,
            effective_origin_site_id,
            grant_authority,
            registration_request_hash,
        } => {
            validate_principal_id(principal_id)?;
            grant_authority.validate_for_class(*principal_class)?;
            for (label, hash) in [
                (
                    "principal grant",
                    grant_authority.principal_grant_hash.as_str(),
                ),
                ("registering request", registration_request_hash.as_str()),
            ] {
                validate_lower_hash(label, hash)?;
            }
            match principal_class {
                AuthorizedKeyPrincipalClass::LocalClient => {
                    if grant_authority.forwarding.is_some() {
                        anyhow::bail!(
                            "local-client schedule authority cannot carry forwarding evidence"
                        );
                    }
                }
                AuthorizedKeyPrincipalClass::RemoteNode => {
                    if grant_authority.forwarding.is_some() {
                        anyhow::bail!(
                            "remote-node schedule authority cannot carry forwarding evidence"
                        );
                    }
                }
                AuthorizedKeyPrincipalClass::RemoteOperator => {
                    let forwarding = grant_authority.forwarding.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "remote-operator schedule authority requires forwarding evidence"
                        )
                    })?;
                    validate_principal_id(&format!("fp:{}", forwarding.source_node_fingerprint))?;
                    validate_lower_hash("source-node grant", &forwarding.source_node_grant_hash)?;
                }
            }
            if matches!(principal_class, AuthorizedKeyPrincipalClass::LocalClient)
                && effective_origin_site_id.is_empty()
            {
                anyhow::bail!("local-client schedule authority has no target site identity");
            }
        }
    }
    Ok(())
}

fn validate_principal_id(value: &str) -> Result<()> {
    let fingerprint = value
        .strip_prefix("fp:")
        .ok_or_else(|| anyhow::anyhow!("schedule principal_id must begin with `fp:`"))?;
    validate_lower_hash("principal fingerprint", fingerprint)
}

fn validate_lower_hash(label: &str, value: &str) -> Result<()> {
    if value.len() != 64
        || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
        || value.bytes().any(|byte| byte.is_ascii_uppercase())
    {
        anyhow::bail!("schedule {label} must be lowercase SHA-256 hex");
    }
    Ok(())
}

/// Rebuildable planning cursor for a schedule.
///
/// The fire-id rows remain the source of idempotency. Startup validates this
/// cursor against the indexed latest fire before using it as the bounded
/// misfire anchor, and repairs any mismatch without replaying JSONL history.
#[derive(Debug, Clone, Serialize)]
pub struct ScheduleCursorRecord {
    pub schedule_id: String,
    pub spec_hash: String,
    pub last_scheduled_at: Option<i64>,
    pub next_fire_at: Option<i64>,
    pub last_evaluated_at: Option<i64>,
    pub updated_at: i64,
}

pub use ryeos_state::gc::retention::FireRecord;

/// Signal sent via mpsc to reload the timer loop.
#[derive(Debug, Clone)]
pub struct ReloadSignal {
    pub schedule_id: Option<String>,
}

/// A pending fire to dispatch (from misfire catch-up or recovery).
#[derive(Debug, Clone)]
pub struct PendingFire {
    pub fire_id: String,
    pub scheduled_at: i64,
    pub reason: &'static str,
}

/// Compute the deterministic fire_id for a schedule at a given time.
pub fn fire_id(schedule_id: &str, scheduled_at_ms: i64) -> String {
    format!("{}@{}", schedule_id, scheduled_at_ms)
}

/// Compute the deterministic thread_id from a fire_id.
/// Uses `lillux::cas::sha256_hex` (same hash used throughout ryeosd).
/// Takes the first 32 hex chars (128 bits) and formats them as a
/// canonical RyeOS execution thread id (`T-{uuid}`). Scheduler-specific
/// identity remains in `fire_id`; execution threads must stay in the
/// global `T-*` namespace enforced by thread lifecycle.
pub fn thread_id_from_fire(fire_id: &str) -> String {
    let hash = lillux::cas::sha256_hex(fire_id.as_bytes());
    format!(
        "T-{}-{}-{}-{}-{}",
        &hash[0..8],
        &hash[8..12],
        &hash[12..16],
        &hash[16..20],
        &hash[20..32],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_with_policy(
        policy: ryeos_engine::execution_contract::ExecutionPolicy,
        project_root: Option<&str>,
    ) -> ScheduleSourceRecord {
        ScheduleSourceRecord {
            spec_version: 2,
            schedule_id: "policy-lane".to_string(),
            item_ref: "directive:test/job".to_string(),
            ref_bindings: BTreeMap::new(),
            schedule_type: "interval".to_string(),
            expression: "60".to_string(),
            params: serde_json::json!({}),
            timezone: "UTC".to_string(),
            misfire_policy: "skip".to_string(),
            overlap_policy: "skip".to_string(),
            lateness_grace_secs: 60,
            enabled: true,
            project_root: project_root.map(str::to_string),
            registered_at: 1_000,
            execution: ScheduleExecution {
                authority: ScheduleExecutionAuthority::Node {
                    principal_id: format!("fp:{}", "33".repeat(32)),
                    effective_origin_site_id: "site:test".to_string(),
                },
                capabilities: vec!["ryeos.execute.directive.test/job".to_string()],
                policy,
            },
            managed_by: None,
        }
    }

    #[test]
    fn fire_id_format() {
        let fid = fire_id("test-sched", 1700000000000);
        assert_eq!(fid, "test-sched@1700000000000");
    }

    #[test]
    fn fire_id_deterministic() {
        let a = fire_id("my-schedule", 1000);
        let b = fire_id("my-schedule", 1000);
        assert_eq!(a, b);
    }

    #[test]
    fn fire_id_different_times() {
        let a = fire_id("my-schedule", 1000);
        let b = fire_id("my-schedule", 2000);
        assert_ne!(a, b);
    }

    #[test]
    fn fire_id_different_schedules() {
        let a = fire_id("schedule-a", 1000);
        let b = fire_id("schedule-b", 1000);
        assert_ne!(a, b);
    }

    #[test]
    fn thread_id_has_canonical_thread_prefix() {
        let tid = thread_id_from_fire("test@1000");
        assert!(tid.starts_with("T-"));
    }

    #[test]
    fn thread_id_is_uuid_shaped() {
        let tid = thread_id_from_fire("test@1000");
        let uuid_part = tid.strip_prefix("T-").expect("T- prefix");
        let groups: Vec<&str> = uuid_part.split('-').collect();
        assert_eq!(groups.len(), 5);
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(
            groups
                .iter()
                .flat_map(|group| group.chars())
                .all(|c| c.is_ascii_hexdigit())
        );
    }

    #[test]
    fn thread_id_deterministic() {
        let a = thread_id_from_fire("test@1000");
        let b = thread_id_from_fire("test@1000");
        assert_eq!(a, b);
    }

    #[test]
    fn thread_id_different_fires() {
        let a = thread_id_from_fire("sched@1000");
        let b = thread_id_from_fire("sched@2000");
        assert_ne!(a, b);
    }

    #[test]
    fn scheduled_execution_preserves_explicit_live_direct_lane() {
        let source = source_with_policy(
            ryeos_engine::execution_contract::ExecutionPolicy::local_live(
                ryeos_engine::execution_contract::ExecutionResponse::Accepted,
            ),
            Some("/project"),
        );

        source.validate(None).unwrap();
        assert!(matches!(
            source.execution.policy.project,
            ProjectExecutionPolicy::LiveDirect { .. }
        ));
    }

    #[test]
    fn scheduled_execution_preserves_explicit_pinned_lane() {
        let source = source_with_policy(
            ryeos_engine::execution_contract::ExecutionPolicy::local_pinned_current_head(
                ryeos_engine::execution_contract::ExecutionResponse::Accepted,
            ),
            Some("/project"),
        );

        source.validate(None).unwrap();
        assert!(matches!(
            source.execution.policy.project,
            ProjectExecutionPolicy::Pinned {
                source: ryeos_engine::execution_contract::PinnedSource::CurrentHead,
                ..
            }
        ));
    }

    #[test]
    fn scheduled_execution_rejects_interactive_capture_live_lane() {
        let source = source_with_policy(
            ryeos_engine::execution_contract::ExecutionPolicy::local_pinned_capture(
                ryeos_engine::execution_contract::ExecutionResponse::Accepted,
            ),
            Some("/project"),
        );

        let error = source.validate(None).unwrap_err();
        assert!(format!("{error:#}").contains("cannot use capture_live"));
    }
}
