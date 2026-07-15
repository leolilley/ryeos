//! ThreadSnapshot — current durable state of one thread.
//!
//! A new snapshot is created on every state transition. Previous snapshots
//! remain in CAS (immutable). The chain_state points to the latest.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

use super::validate_object_kind;

/// The single current thread-snapshot format. Every chain root carries a
/// concrete captured history policy; unsupported shapes fail closed.
pub const THREAD_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSubject {
    pub namespace: String,
    pub subject: String,
}

impl UsageSubject {
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_usage_namespace(&self.namespace)?;
        validate_usage_subject(&self.subject)?;
        Ok(())
    }
}

fn validate_usage_namespace(value: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > 64 {
        anyhow::bail!("usage_subject.namespace must be 1..=64 characters");
    }
    let mut chars = value.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        anyhow::bail!("usage_subject.namespace must start with lowercase ASCII or digit");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
    {
        anyhow::bail!(
            "usage_subject.namespace may contain only lowercase ASCII, digits, '_' and '-'"
        );
    }
    Ok(())
}

fn validate_usage_subject(value: &str) -> anyhow::Result<()> {
    if value.is_empty() || value.len() > 256 {
        anyhow::bail!("usage_subject.subject must be 1..=256 characters");
    }
    if value.chars().any(char::is_control) {
        anyhow::bail!("usage_subject.subject must not contain control characters");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadUsage {
    pub completed_turns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub spend_usd: f64,
    pub spawns_used: u32,
    pub started_at: String,
    pub settled_at: String,
    pub last_settled_turn_seq: u64,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl ThreadUsage {
    pub fn validate(&self) -> anyhow::Result<()> {
        let started_at = parse_canonical_timestamp(&self.started_at)
            .map_err(|error| anyhow::anyhow!("invalid thread usage started_at: {error}"))?;
        let settled_at = parse_canonical_timestamp(&self.settled_at)
            .map_err(|error| anyhow::anyhow!("invalid thread usage settled_at: {error}"))?;
        if settled_at < started_at {
            anyhow::bail!("thread usage settled_at must not precede started_at");
        }
        if !self.spend_usd.is_finite() || self.spend_usd < 0.0 {
            anyhow::bail!("thread usage spend_usd must be finite and non-negative");
        }
        Ok(())
    }
}

/// Parse the one canonical timestamp spelling accepted by authoritative state:
/// whole-second UTC `YYYY-MM-DDTHH:MM:SSZ`, from the Unix epoch through year
/// 9999 inclusive. Callers compare the returned values for chronology rather
/// than reparsing timestamps with a broader RFC 3339 grammar.
pub fn parse_canonical_timestamp(value: &str) -> anyhow::Result<DateTime<Utc>> {
    let bytes = value.as_bytes();
    let separators_are_canonical = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z';
    let digits_are_ascii = separators_are_canonical
        && [0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
            .into_iter()
            .flatten()
            .all(|index| bytes[index].is_ascii_digit());
    if !digits_are_ascii {
        anyhow::bail!("timestamp must use canonical whole-second UTC YYYY-MM-DDTHH:MM:SSZ form");
    }
    if value[17..19].parse::<u8>().is_ok_and(|second| second > 59) {
        anyhow::bail!("timestamp seconds must be within 00..=59");
    }

    let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%SZ").map_err(|error| {
        anyhow::anyhow!("timestamp is not a valid UTC calendar instant: {error}")
    })?;
    let timestamp = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
    let year: u16 = value[..4]
        .parse()
        .map_err(|_| anyhow::anyhow!("timestamp year is invalid"))?;
    if !(1970..=9999).contains(&year) {
        anyhow::bail!("timestamp must be within 1970-01-01T00:00:00Z..=9999-12-31T23:59:59Z");
    }
    Ok(timestamp)
}

pub(crate) fn validate_canonical_hash(label: &str, value: &str) -> anyhow::Result<()> {
    if !lillux::valid_hash(value) || value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        anyhow::bail!("{label} is not a canonical lowercase hash: {value}");
    }
    Ok(())
}

/// Deserialize a nullable field while still requiring its key to be present.
/// Serde otherwise treats a missing `Option<T>` as `None`, which would make
/// current-schema objects silently accept incomplete wire shapes.
fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Validate the engine's kind-agnostic canonical-ref grammar without
/// resolving the kind against any registry. Authoritative state must reject a
/// malformed captured subject during every typed load/rebuild.
fn validate_canonical_item_ref(value: &str) -> anyhow::Result<()> {
    let (kind, remainder) = value
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("bare refs are not canonical"))?;
    if kind.is_empty()
        || !kind.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-')
        })
    {
        anyhow::bail!("kind prefix is empty or contains invalid characters");
    }
    if remainder.is_empty() {
        anyhow::bail!("bare item ID is empty");
    }
    let (bare_id, suffix) = remainder
        .split_once('@')
        .map_or((remainder, None), |(bare_id, suffix)| {
            (bare_id, Some(suffix))
        });
    if bare_id.is_empty()
        || !bare_id.chars().all(|character| {
            character.is_alphanumeric() || matches!(character, '/' | '-' | '_' | '.')
        })
        || bare_id.starts_with('/')
        || bare_id.ends_with('/')
        || bare_id
            .split('/')
            .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        anyhow::bail!("bare item ID is not canonical");
    }
    let Some(suffix) = suffix else {
        return Ok(());
    };
    if let Some(rest) = suffix.strip_prefix("cap:") {
        let parts = rest.splitn(3, ':').collect::<Vec<_>>();
        if parts.len() == 3 && parts.iter().all(|part| !part.is_empty()) {
            return Ok(());
        }
        anyhow::bail!("cap suffix requires three non-empty fields");
    }
    if let Some(rest) = suffix.strip_prefix("sig:") {
        let parts = rest.splitn(2, ':').collect::<Vec<_>>();
        if parts.len() == 2 && parts.iter().all(|part| !part.is_empty()) {
            return Ok(());
        }
        anyhow::bail!("sig suffix requires two non-empty fields");
    }
    if suffix
        .strip_prefix("t:")
        .is_some_and(|timestamp| !timestamp.is_empty())
    {
        return Ok(());
    }
    anyhow::bail!("canonical ref suffix is unknown or incomplete")
}

/// Generic, kind-agnostic retention carried by authoritative root history.
/// Every current-format root must carry an explicit captured policy; Durable
/// is represented by the enum value, never inferred from a missing field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadHistoryRetention {
    Durable,
    TerminalFor { seconds: u64 },
}

/// Largest whole-second terminal retention accepted by the authoritative
/// state wire contract. The canonical timestamp domain ends at Unix second
/// 253,402,300,799 (`9999-12-31T23:59:59Z`), so this is the exact largest
/// duration whose deadline remains representable by signed whole-second
/// projection arithmetic for every accepted terminal instant.
pub const MAX_TERMINAL_DURATION_SECONDS: u64 = (i64::MAX as u64) - 253_402_300_799;

impl ThreadHistoryRetention {
    fn validate(&self) -> anyhow::Result<()> {
        if let Self::TerminalFor { seconds } = self {
            if *seconds == 0 {
                anyhow::bail!("captured history terminal retention must be positive");
            }
            if *seconds > MAX_TERMINAL_DURATION_SECONDS {
                anyhow::bail!(
                    "captured history terminal retention exceeds the maximum of \
                     {MAX_TERMINAL_DURATION_SECONDS} seconds"
                );
            }
        }
        Ok(())
    }
}

/// Signature verification result for the exact item bytes whose history
/// policy was captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapturedItemTrustClass {
    Trusted,
    Untrusted,
    Unsigned,
}

/// Effective trust of the final composed value that supplied an authored
/// history override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapturedEffectiveTrustClass {
    TrustedBundle,
    TrustedProject,
    UntrustedProject,
    Unsigned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapturedItemSpace {
    Project,
    Bundle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapturedNodeHistoryPolicyProvenance {
    MissingConfig,
    SignedConfig {
        path: PathBuf,
        space: CapturedItemSpace,
        content_hash: String,
        signer_fingerprint: String,
    },
}

impl CapturedNodeHistoryPolicyProvenance {
    fn validate(&self) -> anyhow::Result<()> {
        let Self::SignedConfig {
            path,
            content_hash,
            signer_fingerprint,
            ..
        } = self
        else {
            return Ok(());
        };
        if path != Path::new("config/execution/execution.yaml") {
            anyhow::bail!(
                "captured node history policy path must be exactly config/execution/execution.yaml"
            );
        }
        validate_canonical_hash("captured node history policy content_hash", content_hash)?;
        validate_canonical_hash(
            "captured node history policy signer_fingerprint",
            signer_fingerprint,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedThreadHistoryMinimumClamp {
    pub requested_seconds: u64,
    pub minimum_seconds: u64,
}

/// Auditable reason why the captured concrete retention was selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CapturedPolicyProvenance {
    NodeDefault {
        node_policy: CapturedNodeHistoryPolicyProvenance,
    },
    ItemAuthored {
        composed_path: String,
        requested_seconds: u64,
        effective_trust_class: CapturedEffectiveTrustClass,
        #[serde(deserialize_with = "deserialize_required_option")]
        minimum_clamp: Option<CapturedThreadHistoryMinimumClamp>,
        node_policy: CapturedNodeHistoryPolicyProvenance,
    },
}

impl CapturedPolicyProvenance {
    fn validate(
        &self,
        retention: &ThreadHistoryRetention,
        item_trust_class: CapturedItemTrustClass,
    ) -> anyhow::Result<()> {
        match self {
            Self::NodeDefault { node_policy } => {
                node_policy.validate()?;
                if matches!(
                    node_policy,
                    CapturedNodeHistoryPolicyProvenance::MissingConfig
                ) && retention != &ThreadHistoryRetention::Durable
                {
                    anyhow::bail!(
                        "missing node history config can only supply the built-in durable default"
                    );
                }
                Ok(())
            }
            Self::ItemAuthored {
                composed_path,
                requested_seconds,
                effective_trust_class,
                minimum_clamp,
                node_policy,
            } => {
                node_policy.validate()?;
                if composed_path.is_empty()
                    || composed_path.trim() != composed_path
                    || composed_path.chars().any(char::is_control)
                {
                    anyhow::bail!(
                        "captured item-authored history composed_path must be non-empty, trimmed, and free of control characters"
                    );
                }
                if item_trust_class != CapturedItemTrustClass::Trusted {
                    anyhow::bail!("item-authored history requires a trusted item signature");
                }
                if !matches!(
                    effective_trust_class,
                    CapturedEffectiveTrustClass::TrustedBundle
                        | CapturedEffectiveTrustClass::TrustedProject
                ) {
                    anyhow::bail!("item-authored history requires trusted effective composition");
                }
                if *requested_seconds == 0 || *requested_seconds > MAX_TERMINAL_DURATION_SECONDS {
                    anyhow::bail!(
                        "captured item-authored requested_seconds must be within the supported positive duration range"
                    );
                }
                let ThreadHistoryRetention::TerminalFor { seconds } = retention else {
                    anyhow::bail!("item-authored history must resolve to terminal retention");
                };
                match minimum_clamp {
                    None if seconds != requested_seconds => anyhow::bail!(
                        "unclamped item-authored retention must equal requested_seconds"
                    ),
                    Some(clamp) => {
                        if matches!(
                            node_policy,
                            CapturedNodeHistoryPolicyProvenance::MissingConfig
                        ) {
                            anyhow::bail!(
                                "missing node history config cannot impose a minimum clamp"
                            );
                        }
                        if clamp.requested_seconds != *requested_seconds {
                            anyhow::bail!(
                                "captured history clamp requested_seconds does not match provenance"
                            );
                        }
                        if clamp.minimum_seconds <= clamp.requested_seconds
                            || clamp.minimum_seconds > MAX_TERMINAL_DURATION_SECONDS
                        {
                            anyhow::bail!(
                                "captured history clamp must raise the request to a supported larger minimum"
                            );
                        }
                        if *seconds != clamp.minimum_seconds {
                            anyhow::bail!(
                                "captured terminal retention does not equal the recorded clamp minimum"
                            );
                        }
                    }
                    None => {}
                }
                Ok(())
            }
        }
    }
}

/// Verified execution policy captured once when a chain root is created.
/// Continuation members omit this field and inherit the root's policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedThreadHistoryPolicy {
    pub retention: ThreadHistoryRetention,
    pub canonical_item_ref: String,
    pub item_content_hash: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub item_signer_fingerprint: Option<String>,
    pub item_trust_class: CapturedItemTrustClass,
    pub kind_schema_content_hash: String,
    pub resolved_from: CapturedPolicyProvenance,
}

impl CapturedThreadHistoryPolicy {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.retention.validate()?;
        validate_canonical_item_ref(&self.canonical_item_ref).map_err(|error| {
            anyhow::anyhow!("invalid captured history canonical_item_ref: {error}")
        })?;
        for (label, value) in [
            ("item_content_hash", self.item_content_hash.as_str()),
            (
                "kind_schema_content_hash",
                self.kind_schema_content_hash.as_str(),
            ),
        ] {
            validate_canonical_hash(&format!("captured history {label}"), value)?;
        }
        match (self.item_trust_class, &self.item_signer_fingerprint) {
            (CapturedItemTrustClass::Unsigned, None) => {}
            (CapturedItemTrustClass::Unsigned, Some(_)) => {
                anyhow::bail!("unsigned captured history policy cannot name an item signer")
            }
            (
                CapturedItemTrustClass::Trusted | CapturedItemTrustClass::Untrusted,
                Some(signer_fingerprint),
            ) => {
                validate_canonical_hash(
                    "captured history item_signer_fingerprint",
                    signer_fingerprint,
                )?;
            }
            (CapturedItemTrustClass::Trusted | CapturedItemTrustClass::Untrusted, None) => {
                anyhow::bail!("signed captured history policy must name an item signer")
            }
        }
        self.resolved_from
            .validate(&self.retention, self.item_trust_class)?;
        Ok(())
    }
}

/// Thread status enum — must match the CHECK constraint in db.rs exactly:
/// created, running, completed, failed, cancelled, killed, timed_out, continued
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadStatus {
    Created,
    Running,
    Completed,
    Failed,
    Cancelled,
    Killed,
    TimedOut,
    Continued,
}

impl ThreadStatus {
    /// Convert to the string representation used in the database CHECK constraint.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Killed => "killed",
            Self::TimedOut => "timed_out",
            Self::Continued => "continued",
        }
    }

    /// Parse from the database string representation.
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s {
            "created" => Some(Self::Created),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "killed" => Some(Self::Killed),
            "timed_out" => Some(Self::TimedOut),
            "continued" => Some(Self::Continued),
            _ => None,
        }
    }

    /// Whether this is a terminal status (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::Killed
                | Self::TimedOut
                | Self::Continued
        )
    }

    /// A terminal status that ended in failure (carries a cause), as opposed to
    /// a successful or operator-driven terminal (`Completed`/`Continued`/
    /// `Cancelled`). Drives where a thread's terminal payload is recorded — the
    /// `error` field for failures, `result` otherwise.
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed | Self::Killed | Self::TimedOut)
    }
}

impl std::fmt::Display for ThreadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Current durable state of one thread.
///
/// Matches the JSON schema from ARCHITECTURE.md §3:
/// ```json
/// {
///   "schema": 1,
///   "kind": "thread_snapshot",
///   "thread_id": "T-root",
///   "chain_root_id": "T-root",
///   "status": "running",
///   "kind_name": "agent",
///   "item_ref": "directive:foo/bar",
///   "executor_ref": "native:directive-runtime",
///   "launch_mode": "inline",
///   "current_site_id": "site:host",
///   "origin_site_id": "site:host",
///   "upstream_thread_id": null,
///   "requested_by": "user:alice",
///   "captured_history_policy": {
///     "retention": { "mode": "durable" },
///     "canonical_item_ref": "directive:foo/bar",
///     "item_content_hash": "1111111111111111111111111111111111111111111111111111111111111111",
///     "item_signer_fingerprint": "2222222222222222222222222222222222222222222222222222222222222222",
///     "item_trust_class": "trusted",
///     "kind_schema_content_hash": "3333333333333333333333333333333333333333333333333333333333333333",
///     "resolved_from": {"node_default": {"node_policy": "missing_config"}}
///   },
///   "created_at": "...",
///   "updated_at": "...",
///   "started_at": "...",
///   "finished_at": null,
///   "result": null,
///   "outcome_code": null,
///   "error": null,
///   "budget": { ... },
///   "artifacts": [ ... ],
///   "facets": { ... },
///   "last_event_hash": "<hash>",
///   "last_chain_seq": 7,
///   "last_thread_seq": 2
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadSnapshot {
    pub schema: u32,
    pub kind: String,
    pub thread_id: String,
    pub chain_root_id: String,
    pub status: ThreadStatus,
    /// The thread kind (e.g. "agent", "tool", "graph").
    pub kind_name: String,
    /// The item reference being executed (e.g. "directive:foo/bar").
    pub item_ref: String,
    /// The executor reference (e.g. "native:directive-runtime").
    pub executor_ref: String,
    /// Launch mode: "inline" or "detached".
    pub launch_mode: String,
    /// Current execution site (e.g. "site:host").
    pub current_site_id: String,
    /// Origin site where the thread was created.
    pub origin_site_id: String,
    /// The thread that spawned or continued this thread (if any).
    #[serde(deserialize_with = "deserialize_required_option")]
    pub upstream_thread_id: Option<String>,
    /// Who requested the execution (e.g. "user:alice").
    #[serde(deserialize_with = "deserialize_required_option")]
    pub requested_by: Option<String>,
    /// Normalized local project root captured at execution creation. Remote and
    /// snapshot-backed project contexts intentionally remain unattributed.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub project_root: Option<PathBuf>,
    /// History policy captured from verified, typed execution resolution. Only
    /// roots may carry it; every root must carry it in the current format.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub captured_history_policy: Option<CapturedThreadHistoryPolicy>,
    /// The project snapshot hash at the start of execution.
    /// Set when execution begins against a specific project state.
    /// Immutable for this thread. Null for non-CS executions.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub base_project_snapshot_hash: Option<String>,
    /// The project snapshot hash after fold-back.
    /// Set on finalization if the working directory changed.
    /// Null if no changes or not applicable.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub result_project_snapshot_hash: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub started_at: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub finished_at: Option<String>,
    /// Final result payload (set on terminal snapshots).
    #[serde(deserialize_with = "deserialize_required_option")]
    pub result: Option<serde_json::Value>,
    /// Terminal outcome code (e.g. "success", "required_secret_missing").
    #[serde(deserialize_with = "deserialize_required_option")]
    pub outcome_code: Option<String>,
    /// Error payload (set on failed snapshots).
    #[serde(deserialize_with = "deserialize_required_option")]
    pub error: Option<serde_json::Value>,
    /// Budget / usage information (typed ThreadUsage).
    #[serde(deserialize_with = "deserialize_required_option")]
    pub budget: Option<ThreadUsage>,
    /// Published artifacts.
    pub artifacts: Vec<serde_json::Value>,
    /// Key-value facets (e.g. cost annotations). Uses BTreeMap for deterministic serialization.
    #[serde(
        serialize_with = "serialize_btreemap",
        deserialize_with = "deserialize_btreemap"
    )]
    pub facets: BTreeMap<String, String>,
    /// Hash of the last event in this thread.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub last_event_hash: Option<String>,
    /// Last chain sequence number at the time of this snapshot.
    pub last_chain_seq: u64,
    /// Last thread sequence number at the time of this snapshot.
    pub last_thread_seq: u64,
}

fn serialize_btreemap<S: serde::Serializer>(
    map: &BTreeMap<String, String>,
    s: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut seq = s.serialize_map(Some(map.len()))?;
    for (k, v) in map {
        seq.serialize_entry(k, v)?;
    }
    seq.end()
}

fn deserialize_btreemap<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let map: std::collections::HashMap<String, String> = serde::Deserialize::deserialize(d)?;
    Ok(map.into_iter().collect())
}

impl ThreadSnapshot {
    /// Validate this snapshot's invariants.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_object_kind(&self.kind, "thread_snapshot")?;
        if self.schema != THREAD_SNAPSHOT_SCHEMA_VERSION {
            anyhow::bail!("unexpected schema version: {}", self.schema);
        }
        if self.thread_id.is_empty() {
            anyhow::bail!("thread_id must not be empty");
        }
        if self.chain_root_id.is_empty() {
            anyhow::bail!("chain_root_id must not be empty");
        }
        if self.kind_name.is_empty() {
            anyhow::bail!("kind_name must not be empty");
        }
        if self.item_ref.is_empty() {
            anyhow::bail!("item_ref must not be empty");
        }
        if self.executor_ref.is_empty() {
            anyhow::bail!("executor_ref must not be empty");
        }
        if !matches!(self.launch_mode.as_str(), "inline" | "detached") {
            anyhow::bail!(
                "invalid launch_mode: '{}' (expected 'inline' or 'detached')",
                self.launch_mode
            );
        }
        if self.current_site_id.is_empty() {
            anyhow::bail!("current_site_id must not be empty");
        }
        if self.origin_site_id.is_empty() {
            anyhow::bail!("origin_site_id must not be empty");
        }
        if self.thread_id == self.chain_root_id && self.captured_history_policy.is_none() {
            anyhow::bail!("chain root snapshot is missing captured_history_policy");
        }
        if self.thread_id != self.chain_root_id && self.captured_history_policy.is_some() {
            anyhow::bail!("captured_history_policy is valid only on a chain root snapshot");
        }
        if let Some(policy) = &self.captured_history_policy {
            policy.validate()?;
            if policy.canonical_item_ref != self.item_ref {
                anyhow::bail!(
                    "captured history policy subject '{}' does not match root item_ref '{}'",
                    policy.canonical_item_ref,
                    self.item_ref
                );
            }
        }
        let created_at = parse_canonical_timestamp(&self.created_at)
            .map_err(|error| anyhow::anyhow!("invalid created_at: {error}"))?;
        let updated_at = parse_canonical_timestamp(&self.updated_at)
            .map_err(|error| anyhow::anyhow!("invalid updated_at: {error}"))?;
        if updated_at < created_at {
            anyhow::bail!("updated_at must not precede created_at");
        }
        let started_at = self
            .started_at
            .as_deref()
            .map(parse_canonical_timestamp)
            .transpose()
            .map_err(|error| anyhow::anyhow!("invalid started_at: {error}"))?;
        let finished_at = self
            .finished_at
            .as_deref()
            .map(parse_canonical_timestamp)
            .transpose()
            .map_err(|error| anyhow::anyhow!("invalid finished_at: {error}"))?;
        if started_at
            .as_ref()
            .is_some_and(|started_at| *started_at < created_at || *started_at > updated_at)
        {
            anyhow::bail!("started_at must be within created_at..=updated_at");
        }
        if finished_at.as_ref().is_some_and(|finished_at| {
            *finished_at < created_at
                || *finished_at > updated_at
                || started_at
                    .as_ref()
                    .is_some_and(|started_at| finished_at < started_at)
        }) {
            anyhow::bail!(
                "finished_at must be within created_at..=updated_at and not precede started_at"
            );
        }
        match self.status {
            ThreadStatus::Created if self.started_at.is_some() || self.finished_at.is_some() => {
                anyhow::bail!("created snapshot cannot have started_at or finished_at")
            }
            ThreadStatus::Running if self.started_at.is_none() || self.finished_at.is_some() => {
                anyhow::bail!("running snapshot requires started_at and cannot have finished_at")
            }
            status if status.is_terminal() && self.finished_at.is_none() => {
                anyhow::bail!("terminal snapshot requires finished_at")
            }
            _ => {}
        }
        if let Some(usage) = &self.budget {
            usage.validate()?;
        }
        for (label, hash) in [
            (
                "base_project_snapshot_hash",
                self.base_project_snapshot_hash.as_deref(),
            ),
            (
                "result_project_snapshot_hash",
                self.result_project_snapshot_hash.as_deref(),
            ),
            ("last_event_hash", self.last_event_hash.as_deref()),
        ] {
            if let Some(hash) = hash {
                validate_canonical_hash(label, hash)?;
            }
        }
        Ok(())
    }

    /// Convert to a `serde_json::Value` for CAS storage.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("ThreadSnapshot serialization cannot fail")
    }
}

/// Compute the CAS content hash of a [`ThreadSnapshot`] using canonical JSON.
pub fn hash_snapshot(snapshot: &ThreadSnapshot) -> Result<String, lillux::CanonicalJsonError> {
    let value = snapshot.to_value();
    let canonical = lillux::canonical_json(&value)?;
    Ok(lillux::sha256_hex(canonical.as_bytes()))
}

/// Fluent builder for constructing [`ThreadSnapshot`] instances.
pub struct ThreadSnapshotBuilder {
    thread_id: String,
    chain_root_id: String,
    status: ThreadStatus,
    kind_name: String,
    item_ref: String,
    executor_ref: String,
    launch_mode: String,
    current_site_id: String,
    origin_site_id: String,
    upstream_thread_id: Option<String>,
    requested_by: Option<String>,
    project_root: Option<PathBuf>,
    captured_history_policy: Option<CapturedThreadHistoryPolicy>,
    base_project_snapshot_hash: Option<String>,
    result_project_snapshot_hash: Option<String>,
    created_at: String,
    updated_at: String,
    started_at: Option<String>,
    finished_at: Option<String>,
    result: Option<serde_json::Value>,
    outcome_code: Option<String>,
    error: Option<serde_json::Value>,
    budget: Option<ThreadUsage>,
    artifacts: Vec<serde_json::Value>,
    facets: BTreeMap<String, String>,
    last_event_hash: Option<String>,
    last_chain_seq: u64,
    last_thread_seq: u64,
}

impl ThreadSnapshotBuilder {
    /// Start building a new thread snapshot with required fields.
    pub fn new(
        thread_id: impl Into<String>,
        chain_root_id: impl Into<String>,
        kind_name: impl Into<String>,
        item_ref: impl Into<String>,
        executor_ref: impl Into<String>,
    ) -> Self {
        let now = lillux::time::iso8601_now();
        Self {
            thread_id: thread_id.into(),
            chain_root_id: chain_root_id.into(),
            status: ThreadStatus::Created,
            kind_name: kind_name.into(),
            item_ref: item_ref.into(),
            executor_ref: executor_ref.into(),
            launch_mode: "inline".to_string(),
            current_site_id: "site:host".to_string(),
            origin_site_id: "site:host".to_string(),
            upstream_thread_id: None,
            requested_by: None,
            project_root: None,
            captured_history_policy: None,
            base_project_snapshot_hash: None,
            result_project_snapshot_hash: None,
            created_at: now.clone(),
            updated_at: now,
            started_at: None,
            finished_at: None,
            result: None,
            outcome_code: None,
            error: None,
            budget: None,
            artifacts: Vec::new(),
            facets: BTreeMap::new(),
            last_event_hash: None,
            last_chain_seq: 0,
            last_thread_seq: 0,
        }
    }

    pub fn status(mut self, status: ThreadStatus) -> Self {
        self.status = status;
        self
    }

    pub fn launch_mode(mut self, mode: impl Into<String>) -> Self {
        self.launch_mode = mode.into();
        self
    }

    pub fn current_site_id(mut self, site: impl Into<String>) -> Self {
        self.current_site_id = site.into();
        self
    }

    pub fn origin_site_id(mut self, site: impl Into<String>) -> Self {
        self.origin_site_id = site.into();
        self
    }

    pub fn upstream_thread_id(mut self, id: Option<String>) -> Self {
        self.upstream_thread_id = id;
        self
    }

    pub fn requested_by(mut self, who: Option<String>) -> Self {
        self.requested_by = who;
        self
    }

    pub fn project_root(mut self, root: Option<PathBuf>) -> Self {
        self.project_root = root;
        self
    }

    pub fn captured_history_policy(mut self, policy: Option<CapturedThreadHistoryPolicy>) -> Self {
        self.captured_history_policy = policy;
        self
    }

    pub fn base_project_snapshot_hash(mut self, hash: impl Into<String>) -> Self {
        self.base_project_snapshot_hash = Some(hash.into());
        self
    }

    pub fn result_project_snapshot_hash(mut self, hash: impl Into<String>) -> Self {
        self.result_project_snapshot_hash = Some(hash.into());
        self
    }

    pub fn started_at(mut self, ts: Option<String>) -> Self {
        self.started_at = ts;
        self
    }

    pub fn finished_at(mut self, ts: Option<String>) -> Self {
        self.finished_at = ts;
        self
    }

    pub fn result(mut self, result: Option<serde_json::Value>) -> Self {
        self.result = result;
        self
    }

    pub fn outcome_code(mut self, outcome_code: Option<String>) -> Self {
        self.outcome_code = outcome_code;
        self
    }

    pub fn error(mut self, error: Option<serde_json::Value>) -> Self {
        self.error = error;
        self
    }

    pub fn budget(mut self, budget: Option<ThreadUsage>) -> Self {
        self.budget = budget;
        self
    }

    pub fn artifacts(mut self, artifacts: Vec<serde_json::Value>) -> Self {
        self.artifacts = artifacts;
        self
    }

    pub fn facets(mut self, facets: BTreeMap<String, String>) -> Self {
        self.facets = facets;
        self
    }

    pub fn last_event_hash(mut self, hash: Option<String>) -> Self {
        self.last_event_hash = hash;
        self
    }

    pub fn last_chain_seq(mut self, seq: u64) -> Self {
        self.last_chain_seq = seq;
        self
    }

    pub fn last_thread_seq(mut self, seq: u64) -> Self {
        self.last_thread_seq = seq;
        self
    }

    pub fn created_at(mut self, ts: String) -> Self {
        self.created_at = ts;
        self
    }

    pub fn updated_at(mut self, ts: String) -> Self {
        self.updated_at = ts;
        self
    }

    /// Build the [`ThreadSnapshot`].
    pub fn build(self) -> ThreadSnapshot {
        ThreadSnapshot {
            schema: THREAD_SNAPSHOT_SCHEMA_VERSION,
            kind: "thread_snapshot".to_string(),
            thread_id: self.thread_id,
            chain_root_id: self.chain_root_id,
            status: self.status,
            kind_name: self.kind_name,
            item_ref: self.item_ref,
            executor_ref: self.executor_ref,
            launch_mode: self.launch_mode,
            current_site_id: self.current_site_id,
            origin_site_id: self.origin_site_id,
            upstream_thread_id: self.upstream_thread_id,
            requested_by: self.requested_by,
            project_root: self.project_root,
            captured_history_policy: self.captured_history_policy,
            base_project_snapshot_hash: self.base_project_snapshot_hash,
            result_project_snapshot_hash: self.result_project_snapshot_hash,
            created_at: self.created_at,
            updated_at: self.updated_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            result: self.result,
            outcome_code: self.outcome_code,
            error: self.error,
            budget: self.budget,
            artifacts: self.artifacts,
            facets: self.facets,
            last_event_hash: self.last_event_hash,
            last_chain_seq: self.last_chain_seq,
            last_thread_seq: self.last_thread_seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn durable_policy() -> CapturedThreadHistoryPolicy {
        CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::Durable,
            canonical_item_ref: "directive:test".to_string(),
            item_content_hash: "11".repeat(32),
            item_signer_fingerprint: Some("22".repeat(32)),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: "33".repeat(32),
            resolved_from: CapturedPolicyProvenance::NodeDefault {
                node_policy: CapturedNodeHistoryPolicyProvenance::MissingConfig,
            },
        }
    }

    fn signed_node_policy() -> CapturedNodeHistoryPolicyProvenance {
        CapturedNodeHistoryPolicyProvenance::SignedConfig {
            path: PathBuf::from("config/execution/execution.yaml"),
            space: CapturedItemSpace::Project,
            content_hash: "44".repeat(32),
            signer_fingerprint: "55".repeat(32),
        }
    }

    fn child_snapshot() -> ThreadSnapshot {
        ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build()
    }

    #[test]
    fn canonical_timestamp_accepts_only_whole_second_utc_epoch_domain() {
        assert!(parse_canonical_timestamp("1970-01-01T00:00:00Z").is_ok());
        assert!(parse_canonical_timestamp("9999-12-31T23:59:59Z").is_ok());
        for invalid in [
            "1969-12-31T23:59:59Z",
            "2026-04-21T12:00:00.000Z",
            "2026-04-21T12:00:00+00:00",
            "2026-4-21T12:00:00Z",
            "2026-04-21 12:00:00Z",
            "2026-04-21T12:00:60Z",
            "10000-01-01T00:00:00Z",
        ] {
            assert!(parse_canonical_timestamp(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn captured_policy_rejects_unknown_fields_and_noncanonical_trust() {
        let value = serde_json::json!({
            "retention": { "mode": "durable" },
            "canonical_item_ref": "directive:test",
            "item_content_hash": "11".repeat(32),
            "item_signer_fingerprint": null,
            "item_trust_class": "unsigned",
            "kind_schema_content_hash": "33".repeat(32),
            "resolved_from": {
                "node_default": { "node_policy": "missing_config" }
            },
            "unexpected": true,
        });
        assert!(serde_json::from_value::<CapturedThreadHistoryPolicy>(value).is_err());

        let mut policy = durable_policy();
        policy.item_content_hash = "AA".repeat(32);
        assert!(policy.validate().is_err());
        policy.item_content_hash = "11".repeat(32);
        policy.item_signer_fingerprint = Some("AA".repeat(32));
        assert!(policy.validate().is_err());
        policy.item_signer_fingerprint = Some("22".repeat(32));
        policy.item_trust_class = CapturedItemTrustClass::Unsigned;
        assert!(policy.validate().is_err());
        policy.item_signer_fingerprint = None;
        assert!(policy.validate().is_ok());

        policy.item_trust_class = CapturedItemTrustClass::Trusted;
        assert!(policy.validate().is_err());
        policy.item_trust_class = CapturedItemTrustClass::Unsigned;

        let mut missing_signer = serde_json::to_value(&policy).unwrap();
        missing_signer
            .as_object_mut()
            .unwrap()
            .remove("item_signer_fingerprint");
        assert!(
            serde_json::from_value::<CapturedThreadHistoryPolicy>(missing_signer).is_err(),
            "nullable signer must remain an explicit current wire field"
        );

        for malformed in [
            "directive",
            "Directive:test",
            "directive:",
            "directive:/absolute",
            "directive:../escape",
            "directive:test//item",
            "directive:test@unknown:value",
        ] {
            let mut policy = durable_policy();
            policy.canonical_item_ref = malformed.to_string();
            assert!(
                policy.validate().is_err(),
                "accepted malformed ref {malformed}"
            );
        }
    }

    #[test]
    fn signed_node_policy_requires_the_exact_current_relative_path() {
        let mut policy = signed_node_policy();
        assert!(policy.validate().is_ok());
        let CapturedNodeHistoryPolicyProvenance::SignedConfig { path, .. } = &mut policy else {
            unreachable!()
        };
        *path = PathBuf::from("/app/.ai/config/execution/execution.yaml");
        assert!(policy.validate().is_err());
    }

    #[test]
    fn item_authored_policy_requires_exact_trusted_clamp_provenance() {
        let mut policy = durable_policy();
        policy.retention = ThreadHistoryRetention::TerminalFor { seconds: 60 };
        policy.resolved_from = CapturedPolicyProvenance::ItemAuthored {
            composed_path: "history".to_string(),
            requested_seconds: 30,
            effective_trust_class: CapturedEffectiveTrustClass::TrustedProject,
            minimum_clamp: Some(CapturedThreadHistoryMinimumClamp {
                requested_seconds: 30,
                minimum_seconds: 60,
            }),
            node_policy: signed_node_policy(),
        };
        assert!(policy.validate().is_ok());
        let mut wire = serde_json::to_value(&policy).unwrap();
        wire["resolved_from"]["item_authored"]
            .as_object_mut()
            .unwrap()
            .remove("minimum_clamp");
        assert!(serde_json::from_value::<CapturedThreadHistoryPolicy>(wire).is_err());

        policy.item_trust_class = CapturedItemTrustClass::Untrusted;
        assert!(policy.validate().is_err());
        policy.item_trust_class = CapturedItemTrustClass::Trusted;
        policy.resolved_from = CapturedPolicyProvenance::ItemAuthored {
            composed_path: "history".to_string(),
            requested_seconds: 30,
            effective_trust_class: CapturedEffectiveTrustClass::TrustedProject,
            minimum_clamp: Some(CapturedThreadHistoryMinimumClamp {
                requested_seconds: 30,
                minimum_seconds: 61,
            }),
            node_policy: signed_node_policy(),
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn missing_node_config_cannot_claim_a_finite_node_default() {
        let mut policy = durable_policy();
        policy.retention = ThreadHistoryRetention::TerminalFor { seconds: 60 };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn snapshot_validates_status_timestamp_shape_and_lowercase_hashes() {
        let mut snapshot = child_snapshot();
        snapshot.status = ThreadStatus::Running;
        assert!(snapshot.validate().is_err());

        snapshot.started_at = Some("2026-04-21T12:00:01Z".to_string());
        snapshot.updated_at = "2026-04-21T12:00:01Z".to_string();
        assert!(snapshot.validate().is_ok());

        snapshot.status = ThreadStatus::Completed;
        assert!(snapshot.validate().is_err());
        snapshot.finished_at = Some("2026-04-21T12:00:02Z".to_string());
        snapshot.updated_at = "2026-04-21T12:00:02Z".to_string();
        assert!(snapshot.validate().is_ok());

        snapshot.last_event_hash = Some("AA".repeat(32));
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn thread_usage_requires_canonical_chronology_and_finite_nonnegative_spend() {
        let mut usage = ThreadUsage {
            completed_turns: 1,
            input_tokens: 2,
            output_tokens: 3,
            spend_usd: 0.5,
            spawns_used: 0,
            started_at: "2026-04-21T12:00:00Z".to_string(),
            settled_at: "2026-04-21T12:00:01Z".to_string(),
            last_settled_turn_seq: 1,
            elapsed_ms: 1_000,
            provider_id: None,
            model: None,
            profile: None,
        };
        assert!(usage.validate().is_ok());
        usage.settled_at = "2026-04-21T11:59:59Z".to_string();
        assert!(usage.validate().is_err());
        usage.settled_at = "2026-04-21T12:00:01Z".to_string();
        usage.spend_usd = f64::INFINITY;
        assert!(usage.validate().is_err());
        usage.spend_usd = -0.01;
        assert!(usage.validate().is_err());
    }

    #[test]
    fn snapshot_builder_defaults() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();

        assert_eq!(snap.schema, THREAD_SNAPSHOT_SCHEMA_VERSION);
        assert_eq!(snap.kind, "thread_snapshot");
        assert_eq!(snap.thread_id, "T-1");
        assert_eq!(snap.chain_root_id, "T-root");
        assert_eq!(snap.status, ThreadStatus::Created);
        assert_eq!(snap.launch_mode, "inline");
        assert_eq!(snap.current_site_id, "site:host");
        assert!(snap.upstream_thread_id.is_none());
        assert!(snap.requested_by.is_none());
        assert!(snap.captured_history_policy.is_none());
        assert!(snap.started_at.is_none());
        assert!(snap.finished_at.is_none());
        assert!(snap.result.is_none());
        assert!(snap.error.is_none());
        assert!(snap.budget.is_none());
        assert!(snap.artifacts.is_empty());
        assert!(snap.facets.is_empty());
        assert!(snap.last_event_hash.is_none());
        assert_eq!(snap.last_chain_seq, 0);
        assert_eq!(snap.last_thread_seq, 0);
    }

    #[test]
    fn current_schema_requires_nullable_wire_fields() {
        for field in [
            "upstream_thread_id",
            "requested_by",
            "project_root",
            "captured_history_policy",
            "base_project_snapshot_hash",
            "result_project_snapshot_hash",
            "started_at",
            "finished_at",
            "result",
            "outcome_code",
            "error",
            "budget",
            "last_event_hash",
        ] {
            let mut value = serde_json::to_value(child_snapshot()).unwrap();
            value.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<ThreadSnapshot>(value).is_err(),
                "missing {field} was accepted"
            );
        }
    }

    #[test]
    fn snapshot_without_captured_history_policy_is_invalid() {
        let snap = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .build();
        let mut value = serde_json::to_value(&snap).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("captured_history_policy");
        assert!(serde_json::from_value::<ThreadSnapshot>(value).is_err());
    }

    #[test]
    fn root_history_policy_must_name_the_snapshot_item() {
        let snapshot = ThreadSnapshotBuilder::new(
            "T-root",
            "T-root",
            "agent",
            "directive:other",
            "native:directive-runtime",
        )
        .captured_history_policy(Some(durable_policy()))
        .build();

        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn continuation_cannot_capture_root_history_policy() {
        let hash = "a".repeat(64);
        let policy = CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::TerminalFor { seconds: 60 },
            canonical_item_ref: "directive:test".into(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: CapturedPolicyProvenance::NodeDefault {
                node_policy: signed_node_policy(),
            },
        };
        let snap = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .captured_history_policy(Some(policy))
        .build();
        assert!(snap.validate().is_err());
    }

    #[test]
    fn captured_history_policy_enforces_state_duration_bound() {
        let hash = "a".repeat(64);
        let mut policy = CapturedThreadHistoryPolicy {
            retention: ThreadHistoryRetention::TerminalFor {
                seconds: MAX_TERMINAL_DURATION_SECONDS,
            },
            canonical_item_ref: "directive:test".into(),
            item_content_hash: hash.clone(),
            item_signer_fingerprint: Some(hash.clone()),
            item_trust_class: CapturedItemTrustClass::Trusted,
            kind_schema_content_hash: hash,
            resolved_from: CapturedPolicyProvenance::NodeDefault {
                node_policy: signed_node_policy(),
            },
        };
        policy.validate().unwrap();

        policy.retention = ThreadHistoryRetention::TerminalFor {
            seconds: MAX_TERMINAL_DURATION_SECONDS + 1,
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn snapshot_builder_fluent() {
        let mut facets = BTreeMap::new();
        facets.insert("cost.spend".to_string(), "0.12".to_string());
        facets.insert("cost.tokens".to_string(), "1500".to_string());

        let snap = ThreadSnapshotBuilder::new(
            "T-child",
            "T-root",
            "agent",
            "directive:foo/bar",
            "native:directive-runtime",
        )
        .status(ThreadStatus::Completed)
        .launch_mode("detached")
        .upstream_thread_id(Some("T-root".to_string()))
        .requested_by(Some("user:alice".to_string()))
        .started_at(Some("2026-04-21T12:00:01Z".to_string()))
        .finished_at(Some("2026-04-21T12:05:00Z".to_string()))
        .result(Some(serde_json::json!({"output": "done"})))
        .facets(facets)
        .last_chain_seq(7)
        .last_thread_seq(3)
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:05:00Z".to_string())
        .build();

        assert_eq!(snap.status, ThreadStatus::Completed);
        assert_eq!(snap.launch_mode, "detached");
        assert_eq!(snap.upstream_thread_id.as_deref(), Some("T-root"));
        assert_eq!(snap.requested_by.as_deref(), Some("user:alice"));
        assert_eq!(snap.facets.get("cost.spend").unwrap(), "0.12");
        assert_eq!(snap.facets.get("cost.tokens").unwrap(), "1500");
        assert_eq!(snap.last_chain_seq, 7);
        assert_eq!(snap.last_thread_seq, 3);
    }

    #[test]
    fn snapshot_validation_passes() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        assert!(snap.validate().is_ok());
    }

    #[test]
    fn snapshot_validation_rejects_bad_kind() {
        let mut snap = ThreadSnapshotBuilder::new(
            "T-1",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        snap.kind = "wrong".to_string();
        assert!(snap.validate().is_err());
    }

    #[test]
    fn snapshot_validation_rejects_invalid_launch_mode() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .launch_mode("invalid_mode")
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        assert!(snap.validate().is_err());
    }

    #[test]
    fn snapshot_validation_rejects_invalid_event_hash() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .last_event_hash(Some("bad-hash".to_string()))
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        assert!(snap.validate().is_err());
    }

    #[test]
    fn snapshot_serialization_roundtrip() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .status(ThreadStatus::Failed)
        .error(Some(serde_json::json!({"message": "oom"})))
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:05:00Z".to_string())
        .finished_at(Some("2026-04-21T12:05:00Z".to_string()))
        .build();

        let json = serde_json::to_value(&snap).unwrap();
        let deserialized: ThreadSnapshot = serde_json::from_value(json).unwrap();
        assert_eq!(snap.thread_id, deserialized.thread_id);
        assert_eq!(snap.status, deserialized.status);
        assert_eq!(snap.error, deserialized.error);
    }

    #[test]
    fn snapshot_canonical_json_determinism() {
        let mut facets = BTreeMap::new();
        facets.insert("z_key".to_string(), "3".to_string());
        facets.insert("a_key".to_string(), "1".to_string());

        let snap = ThreadSnapshotBuilder::new(
            "T-1",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .facets(facets)
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();

        let hash1 = hash_snapshot(&snap).unwrap();
        let hash2 = hash_snapshot(&snap).unwrap();
        assert_eq!(hash1, hash2, "canonical JSON must be deterministic");
        assert!(lillux::valid_hash(&hash1));
    }

    #[test]
    fn thread_status_matches_db_values() {
        // These must exactly match the CHECK constraint in db.rs
        assert_eq!(ThreadStatus::Created.as_str(), "created");
        assert_eq!(ThreadStatus::Running.as_str(), "running");
        assert_eq!(ThreadStatus::Completed.as_str(), "completed");
        assert_eq!(ThreadStatus::Failed.as_str(), "failed");
        assert_eq!(ThreadStatus::Cancelled.as_str(), "cancelled");
        assert_eq!(ThreadStatus::Killed.as_str(), "killed");
        assert_eq!(ThreadStatus::TimedOut.as_str(), "timed_out");
        assert_eq!(ThreadStatus::Continued.as_str(), "continued");
    }

    #[test]
    fn thread_status_from_str_lossy_roundtrip() {
        for status in [
            ThreadStatus::Created,
            ThreadStatus::Running,
            ThreadStatus::Completed,
            ThreadStatus::Failed,
            ThreadStatus::Cancelled,
            ThreadStatus::Killed,
            ThreadStatus::TimedOut,
            ThreadStatus::Continued,
        ] {
            let s = status.as_str();
            let back = ThreadStatus::from_str_lossy(s).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn thread_status_from_str_lossy_unknown() {
        assert!(ThreadStatus::from_str_lossy("unknown_status").is_none());
    }

    #[test]
    fn thread_status_is_terminal() {
        assert!(!ThreadStatus::Created.is_terminal());
        assert!(!ThreadStatus::Running.is_terminal());
        assert!(ThreadStatus::Completed.is_terminal());
        assert!(ThreadStatus::Failed.is_terminal());
        assert!(ThreadStatus::Cancelled.is_terminal());
        assert!(ThreadStatus::Killed.is_terminal());
        assert!(ThreadStatus::TimedOut.is_terminal());
        assert!(ThreadStatus::Continued.is_terminal());
    }

    #[test]
    fn thread_status_serde_roundtrip() {
        for status in [
            ThreadStatus::Created,
            ThreadStatus::Running,
            ThreadStatus::Completed,
            ThreadStatus::Failed,
            ThreadStatus::Cancelled,
            ThreadStatus::Killed,
            ThreadStatus::TimedOut,
            ThreadStatus::Continued,
        ] {
            let json = serde_json::to_value(status).unwrap();
            let back: ThreadStatus = serde_json::from_value(json).unwrap();
            assert_eq!(status, back);
        }
    }

    #[test]
    fn snapshot_detached_launch_mode_validates() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .launch_mode("detached")
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:00:00Z".to_string())
        .build();
        assert!(snap.validate().is_ok());
    }

    #[test]
    fn snapshot_with_result_and_error_serializes() {
        let snap = ThreadSnapshotBuilder::new(
            "T-1",
            "T-root",
            "agent",
            "directive:test",
            "native:directive-runtime",
        )
        .status(ThreadStatus::Completed)
        .result(Some(serde_json::json!({"answer": 42})))
        .created_at("2026-04-21T12:00:00Z".to_string())
        .updated_at("2026-04-21T12:05:00Z".to_string())
        .finished_at(Some("2026-04-21T12:05:00Z".to_string()))
        .build();

        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: ThreadSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.result.unwrap()["answer"], 42);
    }
}
