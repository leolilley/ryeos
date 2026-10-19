//! Bounded, typed records for host-authorized scheduler history retirement.
//!
//! These records carry evidence only. Filesystem and SQLite effects remain
//! owned by the selected host service and Lillux's retained native owner.

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

pub const SCHEDULER_RETIREMENT_RECORD_MAX_BYTES: usize = 8 * 1024;
pub const SCHEDULER_RETIREMENT_PURPOSE: &str = "scheduler_history_reset_v1";
pub const SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE: &str = "scheduler_history_resolution_v1";
/// Four signed resolution intent/outcome pairs and deterministic R names for
/// each of the three roles.
pub const SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE: u8 = 4;
pub const SCHEDULER_RETIREMENT_ROLE_COUNT: usize = 3;
pub const SCHEDULER_RETIREMENT_RESOLUTION_RECORDS_PER_GENERATION: usize =
    4 * SCHEDULER_RETIREMENT_ROLE_COUNT * 2;
pub const SCHEDULER_RETIREMENT_RESOLUTION_COORDINATES_PER_GENERATION: usize =
    4 * SCHEDULER_RETIREMENT_ROLE_COUNT;
pub const SCHEDULER_RETIREMENT_PHASE_RECORDS_PER_GENERATION: usize = 32;
pub const SCHEDULER_RETIREMENT_MAX_GENERATIONS: usize = 4;
pub const SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS: usize = 256;
pub const SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES_TOTAL: usize = 2 * 1024 * 1024;
pub const SCHEDULER_RETIREMENT_MAX_CLEANUP_COORDINATES: usize = 256;
pub const SCHEDULER_RETIREMENT_MAX_PUBLICATION_RESIDUE: usize = 1;
pub const SCHEDULER_RETIREMENT_MAX_DIRECTORY_ENTRIES: usize = 4096;
/// Three public roles plus the public rollback journal, every generation's
/// Q/U and R role coordinates, and the main-file rollback journal derived
/// from each Q/U/R source. At four generations this is 100 fixed names.
pub const SCHEDULER_RETIREMENT_MAX_RECOVERY_CANDIDATES: usize = SCHEDULER_RETIREMENT_ROLE_COUNT
    + 1
    + SCHEDULER_RETIREMENT_MAX_GENERATIONS
        * (SCHEDULER_RETIREMENT_ROLE_COUNT
            * (2 + SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE as usize)
            + 2
            + SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE as usize);
pub const SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION: usize = 1
    + SCHEDULER_RETIREMENT_PHASE_RECORDS_PER_GENERATION
    + SCHEDULER_RETIREMENT_RESOLUTION_RECORDS_PER_GENERATION;
pub const SCHEDULER_RETIREMENT_MAX_BASE_AND_ROLE_ENTRIES: usize =
    SCHEDULER_RETIREMENT_MAX_GENERATIONS
        * (SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION
            + SCHEDULER_RETIREMENT_RESOLUTION_COORDINATES_PER_GENERATION);
pub const SCHEDULER_RETIREMENT_MAX_RESERVED_ENTRIES: usize =
    SCHEDULER_RETIREMENT_MAX_DIRECTORY_ENTRIES;
pub const SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES: usize =
    SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION * SCHEDULER_RETIREMENT_RECORD_MAX_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRetirementRole {
    Main,
    Wal,
    Shm,
}

impl SchedulerRetirementRole {
    pub const ALL: [Self; 3] = [Self::Main, Self::Wal, Self::Shm];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Wal => "wal",
            Self::Shm => "shm",
        }
    }
}

const fn role_index(role: SchedulerRetirementRole) -> usize {
    match role {
        SchedulerRetirementRole::Main => 0,
        SchedulerRetirementRole::Wal => 1,
        SchedulerRetirementRole::Shm => 2,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRetirementPhase {
    Captured,
    SqlDrainAuthorized,
    SqlDrained,
    QuarantineAuthorized,
    Quarantined,
    PurgeAuthorized,
    Purged,
    ReleaseAuthorized,
    Released,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementRoleIdentity {
    pub device: u64,
    pub inode: u64,
    pub byte_length: u64,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    pub content_sha256: Option<String>,
}

impl SchedulerRetirementRoleIdentity {
    pub fn validate(&self) -> Result<()> {
        if let Some(digest) = &self.content_sha256 {
            ensure!(
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                "scheduler retirement role digest must be lowercase SHA-256 hex"
            );
        }
        ensure!(
            self.inode != 0,
            "scheduler retirement role inode is invalid"
        );
        Ok(())
    }
}

/// Immutable first publication, captured while the offline namespace and CAS
/// exclusion are held. `runtime_identity` is an opaque canonical serialization
/// of Lillux's pinned directory identity; policy never interprets its fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementIntent {
    pub schema: u32,
    pub purpose: String,
    pub generation: String,
    pub owner_fingerprint: String,
    pub runtime_identity: String,
    pub roles: [Option<SchedulerRetirementRoleIdentity>; 3],
    pub operation_id: String,
    pub operation_sha256: String,
    pub scheduler_accounting: crate::SchedulerFireHistoryAccounting,
    pub scheduler_sources: [SchedulerRetirementStoppedRolePlan; 3],
    /// Required schema-3 discriminator. Older or unknown intake is never
    /// inferred to have avoided live SQLite/schema/WAL work.
    pub intake_mode: String,
    /// Original mode carries its sole complete intake plan here. Recovery mode
    /// requires explicit null and carries the sole plan in its signed binding,
    /// preserving the existing per-record envelope rather than duplicating it.
    #[serde(deserialize_with = "required_nullable_observation_field")]
    pub stopped_plan: Option<SchedulerRetirementStoppedPlan>,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    pub recovery: Option<SchedulerRetirementRecoveryBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementRecoveryResolutionRef {
    pub role: SchedulerRetirementRole,
    pub ordinal: u8,
    pub intent_sha256: String,
    pub outcome_sha256: Option<String>,
}

/// Authenticated parent lineage and exact stopped source plan for a new
/// generation that resumes an interrupted retirement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementRecoveryBinding {
    pub parent_generation: String,
    pub parent_intent_sha256: String,
    /// Hashes for the dense parent phase prefix; vector index is sequence.
    pub parent_phase_sha256: Vec<String>,
    pub parent_resolutions: Vec<SchedulerRetirementRecoveryResolutionRef>,
    pub stopped_plan: SchedulerRetirementStoppedPlan,
}

impl SchedulerRetirementRecoveryBinding {
    pub fn validate_for(&self, intent: &SchedulerRetirementIntent) -> Result<()> {
        validate_generation(&self.parent_generation)?;
        validate_hex(&self.parent_intent_sha256, 64, "parent intent digest")?;
        ensure!(
            self.parent_generation != intent.generation
                && self.stopped_plan.generation == intent.generation
                && self.stopped_plan.runtime_identity == intent.runtime_identity
                && self
                    .stopped_plan
                    .generations
                    .contains(&self.parent_generation),
            "scheduler recovery binding is not joined to the new generation/runtime"
        );
        self.stopped_plan.validate()?;
        ensure!(
            self.parent_phase_sha256.len() <= SCHEDULER_RETIREMENT_PHASE_RECORDS_PER_GENERATION,
            "scheduler recovery parent phase prefix exceeds its bound"
        );
        for digest in &self.parent_phase_sha256 {
            validate_hex(digest, 64, "parent phase digest")?;
        }
        ensure!(
            self.parent_resolutions.len()
                <= SCHEDULER_RETIREMENT_RESOLUTION_RECORDS_PER_GENERATION / 2,
            "scheduler recovery parent resolution slots exceed their bound"
        );
        let mut next = [0u8; SCHEDULER_RETIREMENT_ROLE_COUNT];
        let mut pending = [false; SCHEDULER_RETIREMENT_ROLE_COUNT];
        let mut prior = None;
        for slot in &self.parent_resolutions {
            let index = role_index(slot.role);
            ensure!(
                prior.is_none_or(|(role, ordinal)| {
                    (role_index(role), ordinal) < (index, slot.ordinal)
                }) && slot.ordinal == next[index]
                    && !pending[index],
                "scheduler recovery parent resolution lineage is not dense"
            );
            ensure!(
                slot.ordinal < SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE,
                "scheduler recovery parent resolution ordinal is out of range"
            );
            validate_hex(&slot.intent_sha256, 64, "parent resolution intent digest")?;
            if let Some(outcome) = &slot.outcome_sha256 {
                validate_hex(outcome, 64, "parent resolution outcome digest")?;
            } else {
                pending[index] = true;
            }
            next[index] = slot.ordinal + 1;
            prior = Some((slot.role, slot.ordinal));
        }
        Ok(())
    }
}

impl SchedulerRetirementIntent {
    pub fn stopped_intake_plan(&self) -> Result<&SchedulerRetirementStoppedPlan> {
        self.validate()?;
        match &self.recovery {
            Some(binding) => Ok(&binding.stopped_plan),
            None => self
                .stopped_plan
                .as_ref()
                .context("original stopped intake plan absent"),
        }
    }

    pub fn validate_same_operation(&self, other: &Self) -> Result<()> {
        self.validate()?;
        other.validate()?;
        ensure!(
            self.operation_id == other.operation_id
                && self.operation_sha256 == other.operation_sha256
                && self.scheduler_accounting == other.scheduler_accounting
                && self.scheduler_sources == other.scheduler_sources
                && self.runtime_identity == other.runtime_identity
                && self.owner_fingerprint == other.owner_fingerprint,
            "scheduler generations differ from the original operation evidence"
        );
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 3,
            "unsupported scheduler retirement intent schema"
        );
        ensure!(
            self.purpose == SCHEDULER_RETIREMENT_PURPOSE,
            "scheduler retirement intent has an unsupported purpose"
        );
        validate_hex(&self.generation, 32, "generation")?;
        validate_hex(&self.owner_fingerprint, 64, "owner fingerprint")?;
        ensure!(
            !self.runtime_identity.is_empty(),
            "runtime identity is empty"
        );
        ensure!(
            self.roles.iter().all(Option::is_none),
            "scheduler retirement intent must defer role facts to capture acknowledgements"
        );
        for identity in self.roles.iter().flatten() {
            identity.validate()?;
        }
        validate_hex(&self.operation_id, 32, "original operation id")?;
        validate_hex(&self.operation_sha256, 64, "original operation digest")?;
        if let crate::SchedulerFireHistoryAccounting::Exact { counts } = &self.scheduler_accounting
        {
            ensure!(
                counts.total_rows().is_some(),
                "original scheduler accounting overflows"
            );
        }
        let mut physical_sources = std::collections::BTreeSet::new();
        for source in &self.scheduler_sources {
            match (&source.source, &source.current) {
                (SchedulerRetirementStoppedSource::Absent, None) => {}
                (SchedulerRetirementStoppedSource::Canonical, Some(identity)) => {
                    identity.validate()?;
                    ensure!(
                        identity.content_sha256.is_some()
                            && physical_sources.insert((identity.device, identity.inode)),
                        "original scheduler sources lack digests or alias a physical role"
                    );
                }
                _ => anyhow::bail!("original scheduler source must be canonical or absent"),
            }
        }
        if self.scheduler_sources[0].current.is_none() {
            ensure!(self.scheduler_sources.iter().all(|source| source.current.is_none())
                && self.scheduler_accounting == crate::SchedulerFireHistoryAccounting::Exact {
                    counts: Default::default(),
                }, "absent original scheduler cannot supply sidecars or nonzero/unavailable accounting");
        }
        if let Some(recovery) = &self.recovery {
            ensure!(
                self.intake_mode == "stopped-recovery" && self.stopped_plan.is_none(),
                "scheduler recovery intake mode carries a duplicated or unrelated plan"
            );
            recovery.validate_for(self)?;
        } else {
            let plan = self
                .stopped_plan
                .as_ref()
                .context("scheduler original intake lacks required authenticated stopped plan")?;
            plan.validate()?;
            ensure!(
                plan.generation == self.generation
                    && plan.runtime_identity == self.runtime_identity
                    && self.intake_mode == "stopped-original"
                    && plan.roles == self.scheduler_sources,
                "scheduler original stopped intake differs from authenticated copied sources"
            );
            ensure!(
                self.generation == self.operation_id,
                "initial scheduler generation differs from original operation id"
            );
        }
        Ok(())
    }
}

/// Schema-3 stopped intake uses the existing capture/drain phase order:
/// Captured is copied metadata, SqlDrainAuthorized authorizes verification of
/// no live SQL and retained barriers, and SqlDrained records the genuine stable
/// digest from that SAME stopped owner. It does not assert an old SQL close.
/// Older intake records cannot be reinterpreted without the required intent mode.
/// One immutable phase/ack record. The predecessor digest makes a missing,
/// duplicated, reordered, or forked phase fail closed during recovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementRecord {
    pub schema: u32,
    pub purpose: String,
    pub generation: String,
    pub sequence: u8,
    pub previous_sha256: Option<String>,
    pub owner_fingerprint: String,
    pub runtime_identity: String,
    pub role: SchedulerRetirementRole,
    pub phase: SchedulerRetirementPhase,
    pub role_identity: Option<SchedulerRetirementRoleIdentity>,
}

/// Mechanical live resolution actions. These are authorized only by a
/// separately signed, exact-observation record and are never inferred from a
/// restart-time pathname observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRetirementResolutionAction {
    SyncQuarantine,
    SyncRemoved,
    RearmOriginal,
    RearmQuarantined,
    FinishPurgeAtU,
    FinishPurgeAtResolutionQuarantine { source_ordinal: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRetirementEffect {
    Quarantine,
    Purge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRetirementFailureDisposition {
    BeforeCommit,
    DurabilityUncertain,
    NamespaceChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRetirementFailureStage {
    Unchanged,
    RestoredUnexpected,
    PreservedUnexpected,
    QuarantinedOriginal,
    UnlinkedOriginal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRetirementNonRegularKind {
    Directory,
    Symlink,
    CharacterDevice,
    BlockDevice,
    Fifo,
    Socket,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum SchedulerRetirementObservedCoordinate {
    Absent,
    SameOriginal {
        device: u64,
        inode: u64,
    },
    ForeignRegular {
        device: u64,
        inode: u64,
    },
    NonRegular {
        kind: SchedulerRetirementNonRegularKind,
    },
    Unreadable {
        error_sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementFailureFact {
    pub operation: SchedulerRetirementEffect,
    pub disposition: SchedulerRetirementFailureDisposition,
    pub stage: SchedulerRetirementFailureStage,
    pub substep_observed: bool,
    pub error_sha256: String,
}

impl SchedulerRetirementFailureFact {
    pub fn validate(&self) -> Result<()> {
        validate_hex(&self.error_sha256, 64, "retirement error digest")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementObservedQuarantine {
    pub ordinal: u8,
    pub coordinate: SchedulerRetirementObservedCoordinate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementResolutionAttemptFact {
    pub ordinal: u8,
    pub action: SchedulerRetirementResolutionAction,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    pub outcome: Option<SchedulerRetirementResolutionState>,
    pub substep_observed: bool,
    pub original_unlink_observed: bool,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    pub effect_failure: Option<SchedulerRetirementFailureFact>,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    pub error_sha256: Option<String>,
}

/// Copied observations from the same retained native owner. The record never
/// contains a descriptor and cannot reconstruct one after process restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerRetirementResolutionObservation {
    pub generation: String,
    pub role: SchedulerRetirementRole,
    pub runtime_identity: String,
    pub epoch: u64,
    pub barriers_held: bool,
    pub recovery_required: bool,
    pub attempted: SchedulerRetirementEffect,
    pub original: Option<SchedulerRetirementRoleIdentity>,
    pub original_link_count: Option<u64>,
    /// The exact current source selected by a signed stopped-restart plan.
    /// `None` denotes the ordinary live-owner path.
    pub source: SchedulerRetirementObservedCoordinate,
    pub selected_source: Option<SchedulerRetirementStoppedSource>,
    pub stopped_plan: Option<SchedulerRetirementStoppedPlan>,
    pub stopped_coordinates: Vec<SchedulerRetirementObservedStoppedCoordinate>,
    pub quarantine_name: String,
    pub purge_name: String,
    pub original_failure: Option<SchedulerRetirementFailureFact>,
    pub latest_failure: Option<SchedulerRetirementFailureFact>,
    pub resolutions: Vec<SchedulerRetirementResolutionAttemptFact>,
    pub public: SchedulerRetirementObservedCoordinate,
    pub quarantine: SchedulerRetirementObservedCoordinate,
    pub purge: SchedulerRetirementObservedCoordinate,
    pub resolution_quarantines: Vec<SchedulerRetirementObservedQuarantine>,
}

// Strict current wire. Native facts retain names and roles; signed bytes carry
// dense coordinates in one canonical plan-derived inventory.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolutionObservationWire {
    generation: String,
    role: SchedulerRetirementRole,
    runtime_identity: String,
    epoch: u64,
    barriers_held: bool,
    recovery_required: bool,
    attempted: SchedulerRetirementEffect,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    original: Option<SchedulerRetirementRoleIdentity>,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    original_link_count: Option<u64>,
    source: ObservationCoordinateWire,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    selected_source: Option<SchedulerRetirementStoppedSource>,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    stopped_plan: Option<SchedulerRetirementStoppedPlan>,
    stopped_coordinates: Vec<ObservationCoordinateWire>,
    quarantine_name: String,
    purge_name: String,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    original_failure: Option<SchedulerRetirementFailureFact>,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    latest_failure: Option<SchedulerRetirementFailureFact>,
    resolutions: Vec<SchedulerRetirementResolutionAttemptFact>,
    public: ObservationCoordinateWire,
    quarantine: ObservationCoordinateWire,
    purge: ObservationCoordinateWire,
    resolution_quarantines: Vec<ObservationQuarantineWire>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ObservationCoordinateWire {
    // Tagged unit variants otherwise discard unknown fields.
    Absent {},
    SameOriginal {
        device: u64,
        inode: u64,
    },
    ForeignRegular {
        device: u64,
        inode: u64,
    },
    NonRegular {
        kind: SchedulerRetirementNonRegularKind,
    },
    Unreadable {
        error_sha256: String,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationQuarantineWire {
    ordinal: u8,
    coordinate: ObservationCoordinateWire,
}
fn required_nullable_observation_field<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}
impl From<SchedulerRetirementObservedCoordinate> for ObservationCoordinateWire {
    fn from(coordinate: SchedulerRetirementObservedCoordinate) -> Self {
        match coordinate {
            SchedulerRetirementObservedCoordinate::Absent => Self::Absent {},
            SchedulerRetirementObservedCoordinate::SameOriginal { device, inode } => {
                Self::SameOriginal { device, inode }
            }
            SchedulerRetirementObservedCoordinate::ForeignRegular { device, inode } => {
                Self::ForeignRegular { device, inode }
            }
            SchedulerRetirementObservedCoordinate::NonRegular { kind } => Self::NonRegular { kind },
            SchedulerRetirementObservedCoordinate::Unreadable { error_sha256 } => {
                Self::Unreadable { error_sha256 }
            }
        }
    }
}
impl From<ObservationCoordinateWire> for SchedulerRetirementObservedCoordinate {
    fn from(coordinate: ObservationCoordinateWire) -> Self {
        match coordinate {
            ObservationCoordinateWire::Absent {} => Self::Absent,
            ObservationCoordinateWire::SameOriginal { device, inode } => {
                Self::SameOriginal { device, inode }
            }
            ObservationCoordinateWire::ForeignRegular { device, inode } => {
                Self::ForeignRegular { device, inode }
            }
            ObservationCoordinateWire::NonRegular { kind } => Self::NonRegular { kind },
            ObservationCoordinateWire::Unreadable { error_sha256 } => {
                Self::Unreadable { error_sha256 }
            }
        }
    }
}
impl Serialize for SchedulerRetirementResolutionObservation {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        // Validate ORIGINAL named facts before discarding any name or role.
        self.validate().map_err(serde::ser::Error::custom)?;
        let observation = self.clone();
        ResolutionObservationWire {
            generation: observation.generation,
            role: observation.role,
            runtime_identity: observation.runtime_identity,
            epoch: observation.epoch,
            barriers_held: observation.barriers_held,
            recovery_required: observation.recovery_required,
            attempted: observation.attempted,
            original: observation.original,
            original_link_count: observation.original_link_count,
            source: observation.source.into(),
            selected_source: observation.selected_source,
            stopped_plan: observation.stopped_plan,
            stopped_coordinates: observation
                .stopped_coordinates
                .into_iter()
                .map(|item| item.coordinate.into())
                .collect(),
            quarantine_name: observation.quarantine_name,
            purge_name: observation.purge_name,
            original_failure: observation.original_failure,
            latest_failure: observation.latest_failure,
            resolutions: observation.resolutions,
            public: observation.public.into(),
            quarantine: observation.quarantine.into(),
            purge: observation.purge.into(),
            resolution_quarantines: observation
                .resolution_quarantines
                .into_iter()
                .map(|item| ObservationQuarantineWire {
                    ordinal: item.ordinal,
                    coordinate: item.coordinate.into(),
                })
                .collect(),
        }
        .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for SchedulerRetirementResolutionObservation {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let wire = ResolutionObservationWire::deserialize(deserializer)?;
        let inventory = if let Some(plan) = &wire.stopped_plan {
            plan.validate().map_err(serde::de::Error::custom)?;
            plan.expected_coordinate_inventory()
        } else {
            std::collections::BTreeMap::new()
        };
        if wire.stopped_coordinates.len() != inventory.len() {
            return Err(serde::de::Error::custom(
                "compact stopped observation is not the exact dense inventory",
            ));
        }
        let stopped_coordinates = inventory
            .into_iter()
            .zip(wire.stopped_coordinates)
            .map(
                |((name, role), coordinate)| SchedulerRetirementObservedStoppedCoordinate {
                    role,
                    name,
                    coordinate: coordinate.into(),
                },
            )
            .collect();
        let observation = Self {
            generation: wire.generation,
            role: wire.role,
            runtime_identity: wire.runtime_identity,
            epoch: wire.epoch,
            barriers_held: wire.barriers_held,
            recovery_required: wire.recovery_required,
            attempted: wire.attempted,
            original: wire.original,
            original_link_count: wire.original_link_count,
            source: wire.source.into(),
            selected_source: wire.selected_source,
            stopped_plan: wire.stopped_plan,
            stopped_coordinates,
            quarantine_name: wire.quarantine_name,
            purge_name: wire.purge_name,
            original_failure: wire.original_failure,
            latest_failure: wire.latest_failure,
            resolutions: wire.resolutions,
            public: wire.public.into(),
            quarantine: wire.quarantine.into(),
            purge: wire.purge.into(),
            resolution_quarantines: wire
                .resolution_quarantines
                .into_iter()
                .map(|item| SchedulerRetirementObservedQuarantine {
                    ordinal: item.ordinal,
                    coordinate: item.coordinate.into(),
                })
                .collect(),
        };
        observation.validate().map_err(serde::de::Error::custom)?;
        Ok(observation)
    }
}

fn stopped_resolution_coordinates_absent(
    observation: &SchedulerRetirementResolutionObservation,
) -> bool {
    observation.stopped_plan.is_none()
        || observation
            .stopped_coordinates
            .iter()
            .filter(|item| item.role == Some(observation.role))
            .all(|item| {
                matches!(
                    &item.coordinate,
                    SchedulerRetirementObservedCoordinate::Absent
                )
            })
}

fn stopped_original_at_selected_source(
    observation: &SchedulerRetirementResolutionObservation,
) -> bool {
    let Some(plan) = &observation.stopped_plan else {
        return false;
    };
    let Some(source_name) = plan.source_name(observation.role) else {
        return false;
    };
    let mut source_seen = false;
    for coordinate in observation
        .stopped_coordinates
        .iter()
        .filter(|item| item.role == Some(observation.role))
    {
        if coordinate.name == source_name {
            if !matches!(
                &coordinate.coordinate,
                SchedulerRetirementObservedCoordinate::SameOriginal { .. }
            ) {
                return false;
            }
            source_seen = true;
        } else if !matches!(
            &coordinate.coordinate,
            SchedulerRetirementObservedCoordinate::Absent
        ) {
            return false;
        }
    }
    source_seen
}

fn stopped_source_is_absent_if_planned(
    observation: &SchedulerRetirementResolutionObservation,
) -> bool {
    observation.stopped_plan.is_none()
        || matches!(
            &observation.source,
            SchedulerRetirementObservedCoordinate::Absent
        )
}

/// Fixed, enum-only source coordinates used by the stopped-owner restart
/// intake. Caller-provided paths are never part of the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRetirementStoppedSource {
    Absent,
    Canonical,
    Quarantine { generation: String },
    Purge { generation: String },
    Resolution { generation: String, ordinal: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementStoppedRolePlan {
    pub source: SchedulerRetirementStoppedSource,
    #[serde(deserialize_with = "required_nullable_observation_field")]
    pub current: Option<SchedulerRetirementRoleIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementStoppedPlan {
    pub generation: String,
    pub generations: Vec<String>,
    pub runtime_identity: String,
    pub roles: [SchedulerRetirementStoppedRolePlan; 3],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementObservedStoppedCoordinate {
    /// `None` identifies a derived main rollback-journal name.
    pub role: Option<SchedulerRetirementRole>,
    pub name: String,
    pub coordinate: SchedulerRetirementObservedCoordinate,
}

impl SchedulerRetirementStoppedPlan {
    pub fn validate(&self) -> Result<()> {
        validate_generation(&self.generation)?;
        ensure!(
            !self.runtime_identity.is_empty(),
            "stopped scheduler runtime identity is empty"
        );
        ensure!(
            !self.generations.is_empty()
                && self.generations.len() <= SCHEDULER_RETIREMENT_MAX_GENERATIONS
                && self.generations.windows(2).all(|pair| pair[0] < pair[1])
                && self.generations.contains(&self.generation),
            "stopped scheduler generations are not sorted, unique, and inclusive of the new generation"
        );
        for role in SchedulerRetirementRole::ALL {
            let role_plan = &self.roles[role_index(role)];
            match &role_plan.source {
                SchedulerRetirementStoppedSource::Absent => ensure!(
                    role_plan.current.is_none(),
                    "absent stopped scheduler role has a current identity"
                ),
                SchedulerRetirementStoppedSource::Canonical => {
                    validate_stopped_current(role_plan.current.as_ref())?;
                }
                SchedulerRetirementStoppedSource::Quarantine { generation }
                | SchedulerRetirementStoppedSource::Purge { generation }
                | SchedulerRetirementStoppedSource::Resolution { generation, .. } => {
                    validate_generation(generation)?;
                    ensure!(
                        generation != &self.generation
                            && self.generations.binary_search(generation).is_ok(),
                        "stopped scheduler source is not an older listed generation"
                    );
                    if let SchedulerRetirementStoppedSource::Resolution { ordinal, .. } =
                        &role_plan.source
                    {
                        ensure!(
                            *ordinal < SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE,
                            "stopped scheduler source ordinal is out of range"
                        );
                    }
                    validate_stopped_current(role_plan.current.as_ref())?;
                }
            }
        }
        for left in 0..SCHEDULER_RETIREMENT_ROLE_COUNT {
            for right in left + 1..SCHEDULER_RETIREMENT_ROLE_COUNT {
                if let (Some(left), Some(right)) =
                    (&self.roles[left].current, &self.roles[right].current)
                {
                    ensure!(
                        left.device != right.device || left.inode != right.inode,
                        "stopped scheduler roles alias one physical inode"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn source_name(&self, role: SchedulerRetirementRole) -> Option<String> {
        let role_name = role.name();
        Some(match &self.roles[role_index(role)].source {
            SchedulerRetirementStoppedSource::Absent => return None,
            SchedulerRetirementStoppedSource::Canonical => match role {
                SchedulerRetirementRole::Main => "scheduler.sqlite3".to_owned(),
                SchedulerRetirementRole::Wal => "scheduler.sqlite3-wal".to_owned(),
                SchedulerRetirementRole::Shm => "scheduler.sqlite3-shm".to_owned(),
            },
            SchedulerRetirementStoppedSource::Quarantine { generation } => {
                format!("scheduler-retirement.role.{generation}.{role_name}.quarantine")
            }
            SchedulerRetirementStoppedSource::Purge { generation } => {
                format!("scheduler-retirement.role.{generation}.{role_name}.purge")
            }
            SchedulerRetirementStoppedSource::Resolution {
                generation,
                ordinal,
            } => format!(
                "scheduler-retirement.resolve.{generation}.{role_name}.{ordinal:02}.quarantine"
            ),
        })
    }

    pub fn expected_coordinate_names(&self) -> std::collections::BTreeSet<String> {
        self.expected_coordinate_inventory().into_keys().collect()
    }
    pub fn expected_coordinate_inventory(
        &self,
    ) -> std::collections::BTreeMap<String, Option<SchedulerRetirementRole>> {
        let mut inventory = std::collections::BTreeMap::new();
        for (role, public) in [
            (SchedulerRetirementRole::Main, "scheduler.sqlite3"),
            (SchedulerRetirementRole::Wal, "scheduler.sqlite3-wal"),
            (SchedulerRetirementRole::Shm, "scheduler.sqlite3-shm"),
        ] {
            inventory.insert(public.to_owned(), Some(role));
        }
        inventory.insert("scheduler.sqlite3-journal".to_owned(), None);
        for generation in &self.generations {
            for role in SchedulerRetirementRole::ALL {
                let coordinates = ["quarantine", "purge"].into_iter().map(|suffix| {
                    format!("scheduler-retirement.role.{generation}.{}.{suffix}", role.name())
                }).chain((0..SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE).map(|ordinal| {
                    format!("scheduler-retirement.resolve.{generation}.{}.{ordinal:02}.quarantine", role.name())
                }));
                for name in coordinates {
                    inventory.insert(name.clone(), Some(role));
                    if role == SchedulerRetirementRole::Main {
                        inventory.insert(format!("{name}-journal"), None);
                    }
                }
            }
        }
        inventory
    }
}

fn validate_stopped_current(identity: Option<&SchedulerRetirementRoleIdentity>) -> Result<()> {
    let identity = identity.context("selected stopped scheduler role lacks a current identity")?;
    identity.validate()?;
    ensure!(
        identity.content_sha256.is_some(),
        "selected stopped scheduler role lacks a content digest"
    );
    Ok(())
}

impl SchedulerRetirementResolutionObservation {
    pub fn validate(&self) -> Result<()> {
        use SchedulerRetirementObservedCoordinate as Coordinate;

        validate_hex(&self.generation, 32, "generation")?;
        ensure!(
            !self.runtime_identity.is_empty(),
            "runtime identity is empty"
        );
        ensure!(
            self.barriers_held,
            "scheduler retirement barriers are not held"
        );
        ensure!(
            !self.quarantine_name.is_empty() && !self.purge_name.is_empty(),
            "scheduler role mutation coordinates are empty"
        );
        match (&self.stopped_plan, &self.selected_source) {
            (None, None) => ensure!(
                self.stopped_coordinates.is_empty(),
                "ordinary scheduler observation contains stopped-intake coordinates"
            ),
            (Some(plan), Some(selected_source)) => {
                validate_stopped_resolution_observation(self, plan, selected_source)?;
            }
            _ => anyhow::bail!("stopped scheduler source and plan are incomplete"),
        }
        let generation = &self.generation;
        let role = self.role.name();
        ensure!(
            self.quarantine_name
                == format!("scheduler-retirement.role.{generation}.{role}.quarantine")
                && self.purge_name
                    == format!("scheduler-retirement.role.{generation}.{role}.purge"),
            "scheduler role mutation coordinates do not match the fixed generation names"
        );
        ensure!(
            self.original_failure.is_some() && self.latest_failure.is_some(),
            "scheduler role failure testimony is incomplete"
        );
        ensure!(
            self.resolution_quarantines.len()
                <= usize::from(SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE),
            "scheduler retirement resolution coordinate limit exceeded"
        );
        for (index, observed) in self.resolution_quarantines.iter().enumerate() {
            ensure!(
                observed.ordinal < SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE
                    && usize::from(observed.ordinal) == index,
                "scheduler retirement resolution coordinates are not dense and complete"
            );
            validate_coordinate(&observed.coordinate)?;
        }
        ensure!(
            self.resolutions.len() == self.resolution_quarantines.len(),
            "scheduler resolution attempts and R observations differ"
        );
        for (index, attempt) in self.resolutions.iter().enumerate() {
            ensure!(
                usize::from(attempt.ordinal) == index
                    && self.resolution_quarantines[index].ordinal == attempt.ordinal,
                "scheduler resolution attempt/R ordinals are not dense"
            );
            ensure!(
                attempt.outcome != Some(SchedulerRetirementResolutionState::Held),
                "held resolution cannot be a durable native outcome"
            );
            ensure!(
                !attempt.original_unlink_observed || attempt.substep_observed,
                "original unlink testimony lacks an observed native substep"
            );
            if let Some(failure) = &attempt.effect_failure {
                failure.validate()?;
            }
            if let Some(error) = &attempt.error_sha256 {
                validate_hex(error, 64, "resolution attempt error digest")?;
            }
        }
        for coordinate in [&self.public, &self.quarantine, &self.purge] {
            validate_coordinate(coordinate)?;
        }
        if let Some(original) = &self.original {
            original.validate()?;
        }
        ensure!(
            self.original.is_some() == self.original_link_count.is_some(),
            "scheduler retained original identity/link-count presence differs"
        );
        for coordinate in [&self.public, &self.quarantine, &self.purge]
            .into_iter()
            .chain(
                self.resolution_quarantines
                    .iter()
                    .map(|item| &item.coordinate),
            )
        {
            ensure!(
                !coordinate_is_same_original(coordinate)
                    || self.original.as_ref().is_some_and(|original| {
                        coordinate_same_identity(coordinate, original.device, original.inode)
                    }),
                "same-original observation differs from the retained descriptor"
            );
            if let (Coordinate::ForeignRegular { device, inode }, Some(original)) =
                (coordinate, self.original.as_ref())
            {
                ensure!(
                    *device != original.device || *inode != original.inode,
                    "foreign coordinate aliases the retained original"
                );
            }
        }
        for failure in [&self.original_failure, &self.latest_failure]
            .into_iter()
            .flatten()
        {
            failure.validate()?;
        }
        Ok(())
    }
}

fn validate_stopped_resolution_observation(
    observation: &SchedulerRetirementResolutionObservation,
    plan: &SchedulerRetirementStoppedPlan,
    selected_source: &SchedulerRetirementStoppedSource,
) -> Result<()> {
    use SchedulerRetirementObservedCoordinate as Coordinate;

    plan.validate()?;
    ensure!(
        plan.generation == observation.generation
            && plan.runtime_identity == observation.runtime_identity
            && plan.roles[role_index(observation.role)].source == *selected_source,
        "stopped scheduler observation differs from its selected plan"
    );
    let expected_inventory = plan.expected_coordinate_inventory();
    let expected_names = plan.expected_coordinate_names();
    ensure!(
        expected_names.len() <= SCHEDULER_RETIREMENT_MAX_RECOVERY_CANDIDATES
            && observation.stopped_coordinates.len() == expected_names.len(),
        "stopped scheduler observation has an incomplete fixed-coordinate set"
    );
    let mut by_name = std::collections::BTreeMap::new();
    let mut previous_name: Option<&str> = None;
    for item in &observation.stopped_coordinates {
        ensure!(
            previous_name.is_none_or(|previous| previous < item.name.as_str()),
            "stopped scheduler coordinates are not strictly name-sorted"
        );
        previous_name = Some(&item.name);
        ensure!(
            expected_inventory.get(&item.name) == Some(&item.role),
            "stopped scheduler observation name/role differs from the fixed inventory"
        );
        validate_coordinate(&item.coordinate)?;
        ensure!(
            by_name.insert(item.name.as_str(), item).is_none(),
            "stopped scheduler observation contains a duplicate coordinate"
        );
    }
    ensure!(
        by_name.len() == expected_names.len(),
        "stopped scheduler observation omitted a fixed coordinate"
    );
    for role in SchedulerRetirementRole::ALL {
        let role_plan = &plan.roles[role_index(role)];
        let selected_name = plan.source_name(role);
        let selected = role_plan.current.as_ref();
        let mut allowed_locations = std::collections::BTreeSet::new();
        if let Some(name) = selected_name {
            allowed_locations.insert(name);
        }
        for suffix in ["quarantine", "purge"] {
            allowed_locations.insert(format!(
                "scheduler-retirement.role.{}.{}.{suffix}",
                plan.generation,
                role.name()
            ));
        }
        for ordinal in 0..SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE {
            allowed_locations.insert(format!(
                "scheduler-retirement.resolve.{}.{}.{ordinal:02}.quarantine",
                plan.generation,
                role.name()
            ));
        }
        let mut same_original_count = 0usize;
        for item in observation
            .stopped_coordinates
            .iter()
            .filter(|item| item.role == Some(role))
        {
            match &item.coordinate {
                Coordinate::Absent => {}
                Coordinate::SameOriginal { device, inode } => {
                    let identity = selected.context(
                        "stopped scheduler role without a source identity became present",
                    )?;
                    ensure!(
                        *device == identity.device
                            && *inode == identity.inode
                            && allowed_locations.contains(&item.name),
                        "stopped scheduler original is at an unauthorized coordinate"
                    );
                    same_original_count += 1;
                }
                _ => anyhow::bail!(
                    "stopped scheduler coordinate is foreign, nonregular, or unreadable"
                ),
            }
        }
        ensure!(
            same_original_count <= 1,
            "stopped scheduler original appears at multiple coordinates"
        );
        if selected.is_none() {
            ensure!(
                same_original_count == 0,
                "absent stopped scheduler role has a present original"
            );
        }
    }
    for item in observation
        .stopped_coordinates
        .iter()
        .filter(|item| item.role.is_none())
    {
        ensure!(
            item.coordinate == Coordinate::Absent,
            "stopped scheduler rollback journal is present or unreadable"
        );
    }
    let role_plan = &plan.roles[role_index(observation.role)];
    ensure!(
        match (&observation.original, &role_plan.current) {
            (None, None) => true,
            (Some(observed), Some(planned)) => {
                observed.device == planned.device
                    && observed.inode == planned.inode
                    && observed.byte_length == planned.byte_length
                    && observed
                        .content_sha256
                        .as_ref()
                        .is_none_or(|digest| planned.content_sha256.as_ref() == Some(digest))
            }
            _ => false,
        },
        "stopped scheduler role identity differs from the signed plan"
    );
    let absent = Coordinate::Absent;
    let selected_coordinate = plan
        .source_name(observation.role)
        .and_then(|name| by_name.get(name.as_str()).copied())
        .map(|item| &item.coordinate)
        .unwrap_or(&absent);
    ensure!(
        &observation.source == selected_coordinate,
        "stopped scheduler selected-source projection differs from fixed coordinates"
    );
    let public_name = match observation.role {
        SchedulerRetirementRole::Main => "scheduler.sqlite3",
        SchedulerRetirementRole::Wal => "scheduler.sqlite3-wal",
        SchedulerRetirementRole::Shm => "scheduler.sqlite3-shm",
    };
    let quarantine_name = format!(
        "scheduler-retirement.role.{}.{}.quarantine",
        observation.generation,
        observation.role.name()
    );
    let purge_name = format!(
        "scheduler-retirement.role.{}.{}.purge",
        observation.generation,
        observation.role.name()
    );
    for (name, projected) in [
        (public_name.to_owned(), &observation.public),
        (quarantine_name, &observation.quarantine),
        (purge_name, &observation.purge),
    ] {
        ensure!(
            by_name
                .get(name.as_str())
                .is_some_and(|item| &item.coordinate == projected),
            "stopped scheduler canonical projection differs from fixed coordinates"
        );
    }
    for coordinate in &observation.resolution_quarantines {
        let name = format!(
            "scheduler-retirement.resolve.{}.{}.{:02}.quarantine",
            observation.generation,
            observation.role.name(),
            coordinate.ordinal
        );
        ensure!(
            by_name
                .get(name.as_str())
                .is_some_and(|item| item.coordinate == coordinate.coordinate),
            "stopped scheduler R projection differs from fixed coordinates"
        );
    }
    Ok(())
}

fn validate_coordinate(coordinate: &SchedulerRetirementObservedCoordinate) -> Result<()> {
    if let SchedulerRetirementObservedCoordinate::Unreadable { error_sha256 } = coordinate {
        validate_hex(error_sha256, 64, "retirement observation error digest")?;
    }
    Ok(())
}

fn coordinate_is_same_original(coordinate: &SchedulerRetirementObservedCoordinate) -> bool {
    matches!(
        coordinate,
        SchedulerRetirementObservedCoordinate::SameOriginal { .. }
    )
}

fn coordinate_same_identity(
    coordinate: &SchedulerRetirementObservedCoordinate,
    device: u64,
    inode: u64,
) -> bool {
    matches!(
        coordinate,
        SchedulerRetirementObservedCoordinate::SameOriginal {
            device: observed_device,
            inode: observed_inode,
        } if *observed_device == device && *observed_inode == inode
    )
}

/// Signed before-effect resolution authorization. One ordinal is consumed
/// before calling the retained host owner, even when the action later holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementResolutionIntent {
    pub schema: u32,
    pub purpose: String,
    pub generation: String,
    pub operation_id: String,
    pub operation_sha256: String,
    pub owner_fingerprint: String,
    pub runtime_identity: String,
    pub parent_journal_sha256: String,
    pub role: SchedulerRetirementRole,
    pub ordinal: u8,
    pub previous_outcome_sha256: Option<String>,
    pub expected: SchedulerRetirementResolutionObservation,
    pub action: SchedulerRetirementResolutionAction,
}

impl SchedulerRetirementResolutionIntent {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 2, "unsupported scheduler resolution schema");
        ensure!(
            self.purpose == SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE,
            "scheduler resolution has an unsupported purpose"
        );
        validate_hex(&self.generation, 32, "generation")?;
        validate_hex(&self.operation_id, 32, "original operation id")?;
        validate_hex(&self.operation_sha256, 64, "original operation digest")?;
        validate_hex(&self.owner_fingerprint, 64, "owner fingerprint")?;
        validate_hex(&self.parent_journal_sha256, 64, "parent journal digest")?;
        if let Some(previous) = &self.previous_outcome_sha256 {
            validate_hex(previous, 64, "previous resolution outcome digest")?;
        }
        ensure!(
            !self.runtime_identity.is_empty(),
            "runtime identity is empty"
        );
        ensure!(
            self.ordinal < SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE,
            "scheduler retirement resolution budget exhausted"
        );
        ensure!(
            self.expected.generation == self.generation
                && self.expected.role == self.role
                && self.expected.runtime_identity == self.runtime_identity,
            "scheduler resolution authorization differs from its observation"
        );
        self.expected.validate()?;
        ensure!(
            self.expected.recovery_required,
            "resolution intent requires the actual recovery-required owner"
        );
        validate_resolution_action(self)
    }

    /// Join copied native policy facts to the independently authenticated
    /// generation intent, not to a plan chosen by this observation itself.
    pub fn validate_for_generation(&self, generation: &SchedulerRetirementIntent) -> Result<()> {
        generation.validate()?;
        self.validate()?;
        ensure!(
            self.generation == generation.generation
                && self.operation_id == generation.operation_id
                && self.operation_sha256 == generation.operation_sha256
                && self.runtime_identity == generation.runtime_identity
                && self.owner_fingerprint == generation.owner_fingerprint,
            "scheduler resolution differs from its authenticated generation/root/signer"
        );
        let plan = generation.stopped_intake_plan()?;
        ensure!(
            self.expected.stopped_plan.as_ref() == Some(plan)
                && self.expected.selected_source.as_ref()
                    == Some(&plan.roles[role_index(self.role)].source),
            "scheduler resolution stopped plan/source differs from authenticated intake"
        );
        Ok(())
    }

    pub fn intent_filename(&self) -> Result<String> {
        validate_generation(&self.generation)?;
        ensure!(
            self.ordinal < SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE,
            "scheduler retirement resolution budget exhausted"
        );
        Ok(format!(
            "scheduler-retirement.resolution.{}.{}.{:02}.intent",
            self.generation,
            self.role.name(),
            self.ordinal
        ))
    }

    pub fn outcome_filename(&self) -> Result<String> {
        let mut name = self.intent_filename()?;
        name.truncate(name.len() - "intent".len());
        name.push_str("outcome");
        Ok(name)
    }

    pub fn quarantine_filename(&self) -> Result<String> {
        validate_generation(&self.generation)?;
        ensure!(
            self.ordinal < SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE,
            "scheduler retirement resolution budget exhausted"
        );
        Ok(format!(
            "scheduler-retirement.resolve.{}.{}.{:02}.quarantine",
            self.generation,
            self.role.name(),
            self.ordinal
        ))
    }
}

fn validate_resolution_action(intent: &SchedulerRetirementResolutionIntent) -> Result<()> {
    use SchedulerRetirementObservedCoordinate as Coordinate;

    let observation = &intent.expected;
    let latest = observation
        .latest_failure
        .as_ref()
        .expect("observation validation requires latest failure");
    ensure!(
        observation.resolution_quarantines.len() == usize::from(intent.ordinal),
        "scheduler resolution observation does not include every prior R coordinate"
    );
    ensure!(
        observation.resolutions.len() == usize::from(intent.ordinal),
        "scheduler resolution observation does not include every prior attempt"
    );
    let all_resolution_coordinates_absent = observation
        .resolution_quarantines
        .iter()
        .all(|item| matches!(&item.coordinate, Coordinate::Absent));
    let no_resolution_effect = observation.resolutions.iter().all(|attempt| {
        attempt.effect_failure.is_none()
            && !matches!(
                attempt.action,
                SchedulerRetirementResolutionAction::FinishPurgeAtU
                    | SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine { .. }
            )
    });
    let resolution_unlink = observation.resolutions.last().is_some_and(|attempt| {
        attempt.original_unlink_observed
            && attempt.substep_observed
            && attempt.effect_failure.as_ref().is_none_or(|failure| {
                failure.operation == SchedulerRetirementEffect::Purge
                    && failure.disposition
                        == SchedulerRetirementFailureDisposition::DurabilityUncertain
                    && failure.substep_observed
                    && failure.stage == SchedulerRetirementFailureStage::UnlinkedOriginal
            })
    });
    let ordinary_unlink = latest.operation == SchedulerRetirementEffect::Purge
        && latest.disposition == SchedulerRetirementFailureDisposition::DurabilityUncertain
        && latest.substep_observed
        && latest.stage == SchedulerRetirementFailureStage::UnlinkedOriginal;

    match intent.action {
        SchedulerRetirementResolutionAction::SyncQuarantine => ensure!(
            no_resolution_effect
                && observation.attempted == SchedulerRetirementEffect::Quarantine
                && latest.operation == SchedulerRetirementEffect::Quarantine
                && latest.disposition == SchedulerRetirementFailureDisposition::DurabilityUncertain
                && latest.substep_observed
                && latest.stage == SchedulerRetirementFailureStage::QuarantinedOriginal
                && matches!(&observation.public, Coordinate::Absent)
                && stopped_source_is_absent_if_planned(observation)
                && matches!(&observation.quarantine, Coordinate::SameOriginal { .. })
                && matches!(&observation.purge, Coordinate::Absent)
                && observation
                    .original_link_count
                    .is_some_and(|links| links > 0)
                && all_resolution_coordinates_absent,
            "SyncQuarantine is not justified by retained quarantine testimony"
        ),
        SchedulerRetirementResolutionAction::SyncRemoved => ensure!(
            ((ordinary_unlink && no_resolution_effect) || resolution_unlink)
                && observation.original_link_count == Some(0)
                && matches!(&observation.public, Coordinate::Absent)
                && matches!(&observation.quarantine, Coordinate::Absent)
                && matches!(&observation.purge, Coordinate::Absent)
                && stopped_resolution_coordinates_absent(observation)
                && all_resolution_coordinates_absent,
            "SyncRemoved lacks retained successful-unlink testimony"
        ),
        SchedulerRetirementResolutionAction::RearmOriginal => ensure!(
            no_resolution_effect
                && observation.attempted == SchedulerRetirementEffect::Quarantine
                && latest.operation == SchedulerRetirementEffect::Quarantine
                && matches!(
                    (latest.disposition, latest.stage),
                    (
                        SchedulerRetirementFailureDisposition::BeforeCommit,
                        SchedulerRetirementFailureStage::Unchanged
                            | SchedulerRetirementFailureStage::RestoredUnexpected
                    ) | (
                        SchedulerRetirementFailureDisposition::DurabilityUncertain,
                        SchedulerRetirementFailureStage::RestoredUnexpected
                    )
                )
                && latest.substep_observed
                && if observation.stopped_plan.is_some() {
                    stopped_original_at_selected_source(observation)
                } else {
                    matches!(&observation.public, Coordinate::SameOriginal { .. })
                        && matches!(&observation.quarantine, Coordinate::Absent)
                        && matches!(&observation.purge, Coordinate::Absent)
                }
                && observation
                    .original_link_count
                    .is_some_and(|links| links > 0)
                && all_resolution_coordinates_absent,
            "RearmOriginal is not justified by retained no-move testimony"
        ),
        SchedulerRetirementResolutionAction::RearmQuarantined => ensure!(
            no_resolution_effect
                && observation.attempted == SchedulerRetirementEffect::Purge
                && latest.operation == SchedulerRetirementEffect::Purge
                && matches!(
                    (latest.disposition, latest.stage),
                    (
                        SchedulerRetirementFailureDisposition::BeforeCommit,
                        SchedulerRetirementFailureStage::Unchanged
                            | SchedulerRetirementFailureStage::RestoredUnexpected
                    ) | (
                        SchedulerRetirementFailureDisposition::DurabilityUncertain,
                        SchedulerRetirementFailureStage::RestoredUnexpected
                    )
                )
                && latest.substep_observed
                && matches!(&observation.public, Coordinate::Absent)
                && stopped_source_is_absent_if_planned(observation)
                && matches!(&observation.quarantine, Coordinate::SameOriginal { .. })
                && matches!(&observation.purge, Coordinate::Absent)
                && observation
                    .original_link_count
                    .is_some_and(|links| links > 0)
                && all_resolution_coordinates_absent,
            "RearmQuarantined is not justified by retained no-move testimony"
        ),
        SchedulerRetirementResolutionAction::FinishPurgeAtU => ensure!(
            no_resolution_effect
                && observation.attempted == SchedulerRetirementEffect::Purge
                && latest.operation == SchedulerRetirementEffect::Purge
                && latest.disposition == SchedulerRetirementFailureDisposition::DurabilityUncertain
                && latest.substep_observed
                && latest.stage == SchedulerRetirementFailureStage::QuarantinedOriginal
                && matches!(&observation.public, Coordinate::Absent)
                && stopped_source_is_absent_if_planned(observation)
                && matches!(&observation.quarantine, Coordinate::Absent)
                && matches!(&observation.purge, Coordinate::SameOriginal { .. })
                && observation
                    .original_link_count
                    .is_some_and(|links| links > 0)
                && all_resolution_coordinates_absent,
            "FinishPurgeAtU is not justified by retained purge testimony"
        ),
        SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
            source_ordinal,
        } => ensure!(
            observation.resolutions.last().is_some_and(|attempt| {
                attempt.ordinal + 1 == intent.ordinal
                    && matches!(
                        attempt.action,
                        SchedulerRetirementResolutionAction::FinishPurgeAtU
                            | SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
                                ..
                            }
                    )
                    && attempt.effect_failure.as_ref().is_some_and(|failure| {
                        let moved_to_latest = failure.operation == SchedulerRetirementEffect::Purge
                            && failure.disposition == SchedulerRetirementFailureDisposition::DurabilityUncertain
                            && failure.substep_observed
                            && failure.stage == SchedulerRetirementFailureStage::QuarantinedOriginal
                            && source_ordinal == attempt.ordinal;
                        let retained_prior = match attempt.action {
                            SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
                                source_ordinal: prior,
                            } => {
                                prior == source_ordinal
                                    && matches!(
                                        (failure.disposition, failure.stage),
                                        (
                                            SchedulerRetirementFailureDisposition::BeforeCommit,
                                            SchedulerRetirementFailureStage::Unchanged
                                                | SchedulerRetirementFailureStage::RestoredUnexpected
                                        ) | (
                                            SchedulerRetirementFailureDisposition::DurabilityUncertain,
                                            SchedulerRetirementFailureStage::RestoredUnexpected
                                        )
                                    )
                            }
                            _ => false,
                        };
                        moved_to_latest || retained_prior
                    })
                })
                && matches!(&observation.public, Coordinate::Absent)
                && stopped_source_is_absent_if_planned(observation)
                && matches!(&observation.quarantine, Coordinate::Absent)
                && matches!(&observation.purge, Coordinate::Absent)
                && observation.resolution_quarantines.iter().all(|item| {
                    if item.ordinal == source_ordinal {
                        matches!(&item.coordinate, Coordinate::SameOriginal { .. })
                    } else {
                        matches!(&item.coordinate, Coordinate::Absent)
                    }
                })
                && observation
                    .resolution_quarantines
                    .iter()
                    .any(|item| item.ordinal == source_ordinal)
                && source_ordinal < intent.ordinal
                && observation
                    .original_link_count
                    .is_some_and(|links| links > 0),
            "FinishPurgeAtResolutionQuarantine is not justified by retained purge testimony"
        ),
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerRetirementResolutionState {
    RearmedOriginal,
    RearmedQuarantined,
    QuarantinedDurable,
    RemovedDurable,
    Held,
}

/// Immutable result acknowledgement for one resolution intent. A `Held`
/// outcome records refusal and keeps the same owner and all mutation barriers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulerRetirementResolutionOutcome {
    pub schema: u32,
    pub purpose: String,
    pub generation: String,
    pub owner_fingerprint: String,
    pub runtime_identity: String,
    pub role: SchedulerRetirementRole,
    pub ordinal: u8,
    pub intent_sha256: String,
    pub state: SchedulerRetirementResolutionState,
    pub observed: SchedulerRetirementResolutionObservation,
}

impl SchedulerRetirementResolutionOutcome {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported scheduler resolution schema");
        ensure!(
            self.purpose == SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE,
            "scheduler resolution has an unsupported purpose"
        );
        validate_hex(&self.generation, 32, "generation")?;
        validate_hex(&self.owner_fingerprint, 64, "owner fingerprint")?;
        validate_hex(&self.intent_sha256, 64, "resolution intent digest")?;
        ensure!(
            !self.runtime_identity.is_empty(),
            "runtime identity is empty"
        );
        ensure!(
            self.ordinal < SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE,
            "scheduler retirement resolution budget exhausted"
        );
        ensure!(
            self.observed.generation == self.generation
                && self.observed.role == self.role
                && self.observed.runtime_identity == self.runtime_identity,
            "scheduler resolution outcome differs from its observation"
        );
        self.observed.validate()?;
        ensure!(
            self.observed.recovery_required
                == (self.state == SchedulerRetirementResolutionState::Held),
            "resolution outcome state differs from retained native recovery state"
        );
        use SchedulerRetirementObservedCoordinate as Coordinate;
        let stopped = self.observed.stopped_plan.is_some();
        match self.state {
            SchedulerRetirementResolutionState::RearmedOriginal => ensure!(
                if stopped {
                    stopped_original_at_selected_source(&self.observed)
                } else {
                    matches!(&self.observed.public, Coordinate::SameOriginal { .. })
                        && matches!(&self.observed.quarantine, Coordinate::Absent)
                        && matches!(&self.observed.purge, Coordinate::Absent)
                },
                "RearmedOriginal outcome does not match observed coordinates"
            ),
            SchedulerRetirementResolutionState::RearmedQuarantined => ensure!(
                matches!(&self.observed.public, Coordinate::Absent)
                    && stopped_source_is_absent_if_planned(&self.observed)
                    && matches!(&self.observed.quarantine, Coordinate::SameOriginal { .. })
                    && matches!(&self.observed.purge, Coordinate::Absent),
                "RearmedQuarantined outcome does not match observed coordinates"
            ),
            SchedulerRetirementResolutionState::QuarantinedDurable => ensure!(
                matches!(&self.observed.public, Coordinate::Absent)
                    && (!stopped || matches!(&self.observed.source, Coordinate::Absent))
                    && matches!(&self.observed.quarantine, Coordinate::SameOriginal { .. })
                    && matches!(&self.observed.purge, Coordinate::Absent),
                "QuarantinedDurable outcome does not match observed coordinates"
            ),
            SchedulerRetirementResolutionState::RemovedDurable => ensure!(
                self.observed.original_link_count == Some(0)
                    && matches!(&self.observed.public, Coordinate::Absent)
                    && matches!(&self.observed.quarantine, Coordinate::Absent)
                    && matches!(&self.observed.purge, Coordinate::Absent)
                    && self
                        .observed
                        .resolution_quarantines
                        .iter()
                        .all(|item| matches!(&item.coordinate, Coordinate::Absent))
                    && stopped_resolution_coordinates_absent(&self.observed),
                "RemovedDurable outcome lacks unlink testimony and absent coordinates"
            ),
            SchedulerRetirementResolutionState::Held => ensure!(
                self.observed.recovery_required,
                "Held outcome is not retained by a recovery-required owner"
            ),
        }
        Ok(())
    }

    pub fn validate_for_intent(
        &self,
        intent: &SchedulerRetirementResolutionIntent,
        signed_intent_sha256: &str,
    ) -> Result<()> {
        self.validate()?;
        intent.validate()?;
        validate_hex(signed_intent_sha256, 64, "signed resolution intent digest")?;
        ensure!(
            self.generation == intent.generation
                && self.owner_fingerprint == intent.owner_fingerprint
                && self.runtime_identity == intent.runtime_identity
                && self.role == intent.role
                && self.ordinal == intent.ordinal,
            "scheduler resolution outcome does not match its authorization"
        );
        ensure!(
            self.intent_sha256 == signed_intent_sha256,
            "scheduler resolution outcome names another signed intent"
        );
        ensure!(
            self.observed.resolutions.len() == usize::from(intent.ordinal) + 1
                && self.observed.resolutions[..usize::from(intent.ordinal)]
                    == intent.expected.resolutions
                && self.observed.resolutions.last().is_some_and(|attempt| {
                    attempt.ordinal == intent.ordinal
                        && attempt.action == intent.action
                        && match self.state {
                            SchedulerRetirementResolutionState::Held => {
                                attempt.outcome.is_none()
                                    && (attempt.effect_failure.is_some()
                                        || attempt.error_sha256.is_some())
                            }
                            state => {
                                attempt.outcome == Some(state)
                                    && attempt.effect_failure.is_none()
                                    && attempt.error_sha256.is_none()
                            }
                        }
                }),
            "scheduler resolution outcome does not acknowledge the exact native attempt"
        );
        ensure!(
            self.observed.original == intent.expected.original
                && self.observed.original_failure == intent.expected.original_failure
                && self
                    .observed
                    .resolutions
                    .iter()
                    .take(usize::from(intent.ordinal))
                    .eq(intent.expected.resolutions.iter()),
            "scheduler resolution outcome changed its retained original owner or prior attempts"
        );
        ensure!(
            self.observed.stopped_plan == intent.expected.stopped_plan
                && self.observed.selected_source == intent.expected.selected_source,
            "scheduler resolution outcome changed its stopped source plan"
        );
        let allocates_quarantine = matches!(
            intent.action,
            SchedulerRetirementResolutionAction::FinishPurgeAtU
                | SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine { .. }
        );
        let moved_source = match intent.action {
            SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine {
                source_ordinal,
            } => Some(source_ordinal),
            _ => None,
        };
        let prefix_preserved = self
            .observed
            .resolution_quarantines
            .iter()
            .take(usize::from(intent.ordinal))
            .zip(&intent.expected.resolution_quarantines)
            .all(|(observed, expected)| {
                if Some(observed.ordinal) == moved_source {
                    matches!(
                        &expected.coordinate,
                        SchedulerRetirementObservedCoordinate::SameOriginal { .. }
                    ) && matches!(
                        &observed.coordinate,
                        SchedulerRetirementObservedCoordinate::SameOriginal { .. }
                            | SchedulerRetirementObservedCoordinate::Absent
                    )
                } else {
                    observed == expected
                }
            });
        if allocates_quarantine {
            ensure!(
                self.observed.resolution_quarantines.len() == usize::from(intent.ordinal) + 1
                    && prefix_preserved
                    && self
                        .observed
                        .resolution_quarantines
                        .last()
                        .is_some_and(|item| {
                            item.ordinal == intent.ordinal
                                && matches!(
                                    &item.coordinate,
                                    SchedulerRetirementObservedCoordinate::Absent
                                        | SchedulerRetirementObservedCoordinate::SameOriginal { .. }
                                )
                        }),
                "purge resolution outcome omitted its allocated quarantine coordinate"
            );
        } else {
            ensure!(
                self.observed.resolution_quarantines.len() == usize::from(intent.ordinal) + 1
                    && prefix_preserved
                    && matches!(
                        self.observed
                            .resolution_quarantines
                            .last()
                            .map(|item| &item.coordinate),
                        Some(SchedulerRetirementObservedCoordinate::Absent)
                    ),
                "resolution outcome changed the allocated R set"
            );
        }
        let valid_state = match intent.action {
            SchedulerRetirementResolutionAction::SyncQuarantine => matches!(
                self.state,
                SchedulerRetirementResolutionState::QuarantinedDurable
                    | SchedulerRetirementResolutionState::Held
            ),
            SchedulerRetirementResolutionAction::SyncRemoved => matches!(
                self.state,
                SchedulerRetirementResolutionState::RemovedDurable
                    | SchedulerRetirementResolutionState::Held
            ),
            SchedulerRetirementResolutionAction::RearmOriginal => matches!(
                self.state,
                SchedulerRetirementResolutionState::RearmedOriginal
                    | SchedulerRetirementResolutionState::Held
            ),
            SchedulerRetirementResolutionAction::RearmQuarantined => matches!(
                self.state,
                SchedulerRetirementResolutionState::RearmedQuarantined
                    | SchedulerRetirementResolutionState::Held
            ),
            SchedulerRetirementResolutionAction::FinishPurgeAtU
            | SchedulerRetirementResolutionAction::FinishPurgeAtResolutionQuarantine { .. } => {
                matches!(
                    self.state,
                    SchedulerRetirementResolutionState::RemovedDurable
                        | SchedulerRetirementResolutionState::Held
                )
            }
        };
        ensure!(
            valid_state,
            "scheduler resolution outcome differs from its action"
        );
        ensure!(
            self.observed.epoch > intent.expected.epoch,
            "scheduler resolution outcome does not follow its authorized observation"
        );
        Ok(())
    }
}

impl SchedulerRetirementRecord {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1,
            "unsupported scheduler retirement record schema"
        );
        ensure!(
            self.purpose == SCHEDULER_RETIREMENT_PURPOSE,
            "scheduler retirement record has an unsupported purpose"
        );
        validate_hex(&self.generation, 32, "generation")?;
        ensure!(
            usize::from(self.sequence) < SCHEDULER_RETIREMENT_PHASE_RECORDS_PER_GENERATION,
            "scheduler retirement record limit exceeded"
        );
        if let Some(previous) = &self.previous_sha256 {
            validate_hex(previous, 64, "predecessor digest")?;
        }
        validate_hex(&self.owner_fingerprint, 64, "owner fingerprint")?;
        ensure!(
            !self.runtime_identity.is_empty(),
            "runtime identity is empty"
        );
        if let Some(identity) = &self.role_identity {
            identity.validate()?;
        }
        Ok(())
    }
}

pub fn encode_scheduler_retirement_record<T: Serialize>(record: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(record)?;
    ensure!(
        bytes.len() <= SCHEDULER_RETIREMENT_RECORD_MAX_BYTES,
        "scheduler retirement record exceeds the bounded size"
    );
    Ok(bytes)
}

pub fn decode_scheduler_retirement_record<T>(bytes: &[u8]) -> Result<T>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    ensure!(
        bytes.len() <= SCHEDULER_RETIREMENT_RECORD_MAX_BYTES,
        "scheduler retirement record exceeds the bounded size"
    );
    let record: T = serde_json::from_slice(bytes)?;
    ensure!(
        serde_json::to_vec(&record)? == bytes,
        "scheduler retirement record is not in canonical field order/encoding"
    );
    Ok(record)
}

fn validate_generation(value: &str) -> Result<()> {
    validate_hex(value, 32, "generation")
}

fn validate_hex(value: &str, length: usize, label: &str) -> Result<()> {
    ensure!(
        value.len() == length
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "scheduler retirement {label} must be lowercase hexadecimal"
    );
    Ok(())
}

#[cfg(test)]
mod compact_observation_controls {
    use super::*;
    use SchedulerRetirementObservedCoordinate as Coordinate;

    fn observation() -> SchedulerRetirementResolutionObservation {
        let generations = (1..=4).map(|i| format!("{i:032x}")).collect::<Vec<_>>();
        let generation = generations[3].clone();
        let identity = |role: usize| SchedulerRetirementRoleIdentity {
            device: u64::MAX,
            inode: u64::MAX - role as u64,
            byte_length: u64::MAX,
            content_sha256: Some("ab".repeat(32)),
        };
        let plan = SchedulerRetirementStoppedPlan {
            generation: generation.clone(),
            generations,
            runtime_identity: "{\"device\":18446744073709551615,\"inode\":18446744073709551615}"
                .to_owned(),
            roles: std::array::from_fn(|role| SchedulerRetirementStoppedRolePlan {
                source: SchedulerRetirementStoppedSource::Canonical,
                current: Some(identity(role)),
            }),
        };
        let quarantine_name = format!("scheduler-retirement.role.{generation}.main.quarantine");
        let stopped_coordinates = plan
            .expected_coordinate_inventory()
            .into_iter()
            .map(|(name, role)| {
                let current_role = if name == quarantine_name {
                    Some(0)
                } else if name == "scheduler.sqlite3-wal" {
                    Some(1)
                } else if name == "scheduler.sqlite3-shm" {
                    Some(2)
                } else {
                    None
                };
                SchedulerRetirementObservedStoppedCoordinate {
                    name,
                    role,
                    coordinate: current_role.map_or(Coordinate::Absent, |role| {
                        Coordinate::SameOriginal {
                            device: u64::MAX,
                            inode: u64::MAX - role as u64,
                        }
                    }),
                }
            })
            .collect();
        let failure = SchedulerRetirementFailureFact {
            operation: SchedulerRetirementEffect::Quarantine,
            disposition: SchedulerRetirementFailureDisposition::DurabilityUncertain,
            stage: SchedulerRetirementFailureStage::QuarantinedOriginal,
            substep_observed: true,
            error_sha256: "cd".repeat(32),
        };
        let mut original = identity(0);
        original.content_sha256 = None;
        SchedulerRetirementResolutionObservation {
            generation: generation.clone(),
            role: SchedulerRetirementRole::Main,
            runtime_identity: plan.runtime_identity.clone(),
            epoch: u64::MAX - 1,
            barriers_held: true,
            recovery_required: true,
            attempted: SchedulerRetirementEffect::Quarantine,
            original: Some(original),
            original_link_count: Some(u64::MAX),
            source: Coordinate::Absent,
            selected_source: Some(SchedulerRetirementStoppedSource::Canonical),
            stopped_plan: Some(plan),
            stopped_coordinates,
            quarantine_name,
            purge_name: format!("scheduler-retirement.role.{generation}.main.purge"),
            original_failure: Some(failure.clone()),
            latest_failure: Some(failure),
            resolutions: (0..3)
                .map(|ordinal| SchedulerRetirementResolutionAttemptFact {
                    ordinal,
                    action: SchedulerRetirementResolutionAction::SyncQuarantine,
                    outcome: None,
                    substep_observed: false,
                    original_unlink_observed: false,
                    effect_failure: None,
                    error_sha256: Some("ef".repeat(32)),
                })
                .collect(),
            public: Coordinate::Absent,
            quarantine: Coordinate::SameOriginal {
                device: u64::MAX,
                inode: u64::MAX,
            },
            purge: Coordinate::Absent,
            resolution_quarantines: (0..3)
                .map(|ordinal| SchedulerRetirementObservedQuarantine {
                    ordinal,
                    coordinate: Coordinate::Absent,
                })
                .collect(),
        }
    }

    #[test]
    fn target_role_rearm_and_removal_preserve_other_roles_and_full_inventory() -> Result<()> {
        let mut observation = observation();
        let source = SchedulerRetirementStoppedSource::Quarantine {
            generation: format!("{:032x}", 1),
        };
        observation.stopped_plan.as_mut().unwrap().roles[0].source = source.clone();
        observation.selected_source = Some(source);
        observation.source = Coordinate::Absent;
        observation.attempted = SchedulerRetirementEffect::Purge;
        let failure = SchedulerRetirementFailureFact {
            operation: SchedulerRetirementEffect::Purge,
            disposition: SchedulerRetirementFailureDisposition::BeforeCommit,
            stage: SchedulerRetirementFailureStage::Unchanged,
            substep_observed: false,
            error_sha256: "cd".repeat(32),
        };
        observation.original_failure = Some(failure.clone());
        observation.latest_failure = Some(failure);
        observation.resolutions.clear();
        observation.resolution_quarantines.clear();
        let intent = SchedulerRetirementResolutionIntent {
            schema: 2,
            operation_id: format!("{:032x}", 1),
            operation_sha256: "aa".repeat(32),
            purpose: SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: observation.generation.clone(),
            role: observation.role,
            ordinal: 0,
            owner_fingerprint: "ab".repeat(32),
            runtime_identity: observation.runtime_identity.clone(),
            parent_journal_sha256: "cd".repeat(32),
            previous_outcome_sha256: None,
            action: SchedulerRetirementResolutionAction::RearmQuarantined,
            expected: observation.clone(),
        };
        intent.validate()?; // native RearmQuarantined retains own NEW Q, old source absent
        let mut observed = observation.clone();
        observed.recovery_required = false;
        observed.epoch = u64::MAX;
        observed
            .resolutions
            .push(SchedulerRetirementResolutionAttemptFact {
                ordinal: 0,
                action: SchedulerRetirementResolutionAction::RearmQuarantined,
                outcome: Some(SchedulerRetirementResolutionState::RearmedQuarantined),
                substep_observed: false,
                original_unlink_observed: false,
                effect_failure: None,
                error_sha256: None,
            });
        observed
            .resolution_quarantines
            .push(SchedulerRetirementObservedQuarantine {
                ordinal: 0,
                coordinate: Coordinate::Absent,
            });
        let outcome = SchedulerRetirementResolutionOutcome {
            schema: 1,
            purpose: SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: observed.generation.clone(),
            role: observed.role,
            ordinal: 0,
            owner_fingerprint: intent.owner_fingerprint.clone(),
            runtime_identity: observed.runtime_identity.clone(),
            intent_sha256: "ef".repeat(32),
            state: SchedulerRetirementResolutionState::RearmedQuarantined,
            observed,
        };
        outcome.validate_for_intent(&intent, &outcome.intent_sha256)?;
        let mut removed = outcome.clone();
        removed.state = SchedulerRetirementResolutionState::RemovedDurable;
        removed.observed.original_link_count = Some(0);
        removed.observed.quarantine = Coordinate::Absent;
        removed.observed.resolutions[0].action = SchedulerRetirementResolutionAction::SyncRemoved;
        removed.observed.resolutions[0].outcome =
            Some(SchedulerRetirementResolutionState::RemovedDurable);
        for item in &mut removed.observed.stopped_coordinates {
            if item.role == Some(SchedulerRetirementRole::Main) {
                item.coordinate = Coordinate::Absent;
            }
        }
        removed.validate()?; // WAL/SHM still present; only target role is removed
        let mut truncated = removed.clone();
        truncated.observed.stopped_coordinates.pop();
        assert!(truncated.validate().is_err());
        let mut alias = removed.clone();
        alias
            .observed
            .stopped_coordinates
            .iter_mut()
            .find(|item| item.role == Some(SchedulerRetirementRole::Main))
            .unwrap()
            .coordinate = Coordinate::SameOriginal {
            device: u64::MAX,
            inode: u64::MAX,
        };
        assert!(alias.validate().is_err());
        let mut forged = observation;
        forged.original.as_mut().unwrap().content_sha256 = Some("ef".repeat(32));
        assert!(forged.validate().is_err());
        Ok(())
    }

    #[test]
    fn resolution_requires_exact_authenticated_generation_plan_and_live_absence() -> Result<()> {
        let expected = observation();
        let mut generation = SchedulerRetirementIntent {
            schema: 3,
            operation_id: format!("{:032x}", 1),
            operation_sha256: "aa".repeat(32),
            scheduler_accounting:
                crate::SchedulerFireHistoryAccounting::UnavailableIncompatibleSchema {},
            scheduler_sources: expected.stopped_plan.as_ref().unwrap().roles.clone(),
            purpose: SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
            generation: expected.generation.clone(),
            owner_fingerprint: "ab".repeat(32),
            runtime_identity: expected.runtime_identity.clone(),
            roles: [None, None, None],
            intake_mode: "stopped-recovery".to_owned(),
            stopped_plan: None,
            recovery: Some(SchedulerRetirementRecoveryBinding {
                parent_generation: format!("{:032x}", 1),
                parent_intent_sha256: "cd".repeat(32),
                parent_phase_sha256: vec!["ef".repeat(32); 9],
                parent_resolutions: Vec::new(),
                stopped_plan: expected.stopped_plan.clone().unwrap(),
            }),
        };
        let intent = SchedulerRetirementResolutionIntent {
            schema: 2,
            operation_id: generation.operation_id.clone(),
            operation_sha256: generation.operation_sha256.clone(),
            purpose: SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE.to_owned(),
            generation: expected.generation.clone(),
            role: expected.role,
            ordinal: 3,
            owner_fingerprint: generation.owner_fingerprint.clone(),
            runtime_identity: expected.runtime_identity.clone(),
            parent_journal_sha256: "cd".repeat(32),
            previous_outcome_sha256: Some("ef".repeat(32)),
            expected,
            action: SchedulerRetirementResolutionAction::SyncQuarantine,
        };
        intent.validate_for_generation(&generation)?;
        for changed in ["operation_id", "operation_sha256"] {
            let mut substituted = intent.clone();
            if changed == "operation_id" {
                substituted.operation_id = "ff".repeat(16);
            } else {
                substituted.operation_sha256 = "ff".repeat(32);
            }
            substituted.validate()?;
            assert!(substituted.validate_for_generation(&generation).is_err());
            let mut missing = serde_json::to_value(&intent)?;
            missing.as_object_mut().unwrap().remove(changed);
            assert!(
                serde_json::from_value::<SchedulerRetirementResolutionIntent>(missing).is_err()
            );
        }
        let mut legacy_resolution = intent.clone();
        legacy_resolution.schema = 1;
        assert!(legacy_resolution.validate().is_err());
        // A shorter plan and its shortened coordinate facts remain internally
        // valid; only the authenticated generation join refuses the substitution.
        let mut shorter = intent.clone();
        let plan = shorter.expected.stopped_plan.as_mut().unwrap();
        plan.generations.remove(1);
        let names = plan.expected_coordinate_names();
        shorter
            .expected
            .stopped_coordinates
            .retain(|item| names.contains(&item.name));
        shorter.validate()?;
        assert!(shorter.validate_for_generation(&generation).is_err());

        let mut selected = intent.clone();
        let source = SchedulerRetirementStoppedSource::Quarantine {
            generation: format!("{:032x}", 1),
        };
        selected.expected.stopped_plan.as_mut().unwrap().roles[0].source = source.clone();
        selected.expected.selected_source = Some(source);
        selected.validate()?;
        assert!(selected.validate_for_generation(&generation).is_err());
        let mut wrong_selected = intent.clone();
        wrong_selected.expected.selected_source = Some(SchedulerRetirementStoppedSource::Absent);
        assert!(wrong_selected.validate_for_generation(&generation).is_err());
        for change in ["root", "signer", "generation"] {
            let mut changed = generation.clone();
            match change {
                "root" => {
                    changed.runtime_identity.push('x');
                    changed
                        .recovery
                        .as_mut()
                        .unwrap()
                        .stopped_plan
                        .runtime_identity = changed.runtime_identity.clone();
                }
                "signer" => changed.owner_fingerprint = "ff".repeat(32),
                _ => {
                    changed.generation = format!("{:032x}", 3);
                    changed.recovery.as_mut().unwrap().stopped_plan.generation =
                        changed.generation.clone();
                }
            }
            assert!(
                intent.validate_for_generation(&changed).is_err(),
                "generation join accepted changed {change}"
            );
        }
        generation.recovery = None;
        generation.operation_id = generation.generation.clone();
        assert!(intent.validate_for_generation(&generation).is_err());
        let mut live = intent;
        live.operation_id = generation.operation_id.clone();
        live.expected.stopped_plan = None;
        live.expected.selected_source = None;
        live.expected.stopped_coordinates.clear();
        live.validate_for_generation(&generation)?;
        Ok(())
    }

    #[test]
    fn required_original_evidence_refuses_legacy_missing_and_spliced_generation_facts() -> Result<()>
    {
        let observation = observation();
        let initial = SchedulerRetirementIntent {
            schema: 3,
            purpose: SCHEDULER_RETIREMENT_PURPOSE.to_owned(),
            generation: format!("{:032x}", 1),
            operation_id: format!("{:032x}", 1),
            operation_sha256: "aa".repeat(32),
            scheduler_accounting: crate::SchedulerFireHistoryAccounting::Exact {
                counts: crate::SchedulerFireHistoryDiscardReport {
                    fires: 7,
                    outbox_snapshots: 3,
                    cursors: 2,
                },
            },
            scheduler_sources: observation.stopped_plan.as_ref().unwrap().roles.clone(),
            owner_fingerprint: "ab".repeat(32),
            runtime_identity: observation.runtime_identity.clone(),
            roles: [None, None, None],
            intake_mode: "stopped-original".to_owned(),
            stopped_plan: Some(SchedulerRetirementStoppedPlan {
                generation: format!("{:032x}", 1),
                generations: vec![format!("{:032x}", 1)],
                runtime_identity: observation.runtime_identity.clone(),
                roles: observation.stopped_plan.as_ref().unwrap().roles.clone(),
            }),
            recovery: None,
        };
        initial.validate()?;
        let mut child = initial.clone();
        child.generation = observation.generation.clone();
        child.recovery = Some(SchedulerRetirementRecoveryBinding {
            parent_generation: initial.generation.clone(),
            parent_intent_sha256: "cd".repeat(32),
            parent_phase_sha256: vec!["ef".repeat(32); 9],
            parent_resolutions: Vec::new(),
            stopped_plan: observation.stopped_plan.unwrap(),
        });
        child.intake_mode = "stopped-recovery".to_owned();
        child.stopped_plan = None;
        child.validate_same_operation(&initial)?;
        for changed in ["id", "hash", "accounting", "source"] {
            let mut substituted = child.clone();
            match changed {
                "id" => substituted.operation_id = "ff".repeat(16),
                "hash" => substituted.operation_sha256 = "ff".repeat(32),
                "accounting" => {
                    substituted.scheduler_accounting =
                        crate::SchedulerFireHistoryAccounting::UnavailableIncompatibleSchema {}
                }
                _ => {
                    substituted.scheduler_sources[0]
                        .current
                        .as_mut()
                        .unwrap()
                        .content_sha256 = Some("ff".repeat(32))
                }
            }
            substituted.validate()?;
            assert!(
                substituted.validate_same_operation(&initial).is_err(),
                "accepted spliced {changed}"
            );
        }
        let encoded = serde_json::to_value(&initial)?;
        for field in [
            "operation_id",
            "operation_sha256",
            "scheduler_accounting",
            "scheduler_sources",
            "intake_mode",
            "stopped_plan",
            "recovery",
        ] {
            let mut missing = encoded.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<SchedulerRetirementIntent>(missing).is_err(),
                "accepted missing {field}"
            );
        }
        let mut legacy = initial.clone();
        legacy.schema = 2;
        assert!(legacy.validate().is_err());
        let mut missing_plan = initial.clone();
        missing_plan.stopped_plan = None;
        assert!(missing_plan.validate().is_err());
        let mut unknown_mode = initial.clone();
        unknown_mode.intake_mode = "live-sql".to_owned();
        assert!(unknown_mode.validate().is_err());
        let mut changed_plan = initial.clone();
        changed_plan.stopped_plan.as_mut().unwrap().roles[0]
            .current
            .as_mut()
            .unwrap()
            .content_sha256 = Some("ff".repeat(32));
        assert!(changed_plan.validate().is_err());
        legacy.schema = 1;
        assert!(legacy.validate().is_err());
        let mut unrelated_initial = initial.clone();
        unrelated_initial.generation = "ff".repeat(16);
        assert!(unrelated_initial.validate().is_err());
        let mut malformed = initial.clone();
        malformed.scheduler_sources[0]
            .current
            .as_mut()
            .unwrap()
            .content_sha256 = None;
        assert!(malformed.validate().is_err());
        malformed = initial.clone();
        malformed.scheduler_sources[1] = malformed.scheduler_sources[0].clone();
        assert!(malformed.validate().is_err());
        malformed = initial;
        malformed.scheduler_accounting = crate::SchedulerFireHistoryAccounting::Exact {
            counts: crate::SchedulerFireHistoryDiscardReport {
                fires: usize::MAX,
                outbox_snapshots: 1,
                cursors: 0,
            },
        };
        assert!(malformed.validate().is_err());
        Ok(())
    }

    #[test]
    fn compact_hundred_coordinate_wire_round_trips_full_named_facts() -> Result<()> {
        let observation = observation();
        observation.validate()?;
        assert_eq!(observation.stopped_coordinates.len(), 100);
        let bytes = encode_scheduler_retirement_record(&observation)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let coordinates = value["stopped_coordinates"].as_array().unwrap();
        assert_eq!(coordinates.len(), 100);
        assert!(coordinates
            .iter()
            .all(|item| item.get("name").is_none() && item.get("role").is_none()));
        assert_eq!(
            decode_scheduler_retirement_record::<SchedulerRetirementResolutionObservation>(&bytes)?,
            observation
        );
        Ok(())
    }

    #[test]
    fn serializer_refuses_name_role_sort_presence_and_foreign_fact_changes() {
        for change in [
            "name",
            "role",
            "journal-role",
            "order",
            "duplicate",
            "truncate",
            "foreign",
            "unreadable",
        ] {
            let mut observation = observation();
            match change {
                "name" => observation.stopped_coordinates[0].name.push_str("-foreign"),
                "role" => {
                    observation.stopped_coordinates[0].role = Some(SchedulerRetirementRole::Shm)
                }
                "journal-role" => {
                    let journal = observation
                        .stopped_coordinates
                        .iter_mut()
                        .find(|item| item.role.is_none())
                        .unwrap();
                    journal.role = Some(SchedulerRetirementRole::Main);
                }
                "order" => observation.stopped_coordinates.swap(0, 1),
                "duplicate" => {
                    observation.stopped_coordinates[1] = observation.stopped_coordinates[0].clone()
                }
                "truncate" => {
                    observation.stopped_coordinates.pop();
                }
                "foreign" => {
                    observation.stopped_coordinates[0].coordinate = Coordinate::ForeignRegular {
                        device: 1,
                        inode: 2,
                    }
                }
                _ => {
                    observation.stopped_coordinates[0].coordinate = Coordinate::Unreadable {
                        error_sha256: "ab".repeat(32),
                    }
                }
            }
            assert!(
                serde_json::to_vec(&observation).is_err(),
                "serializer accepted {change}"
            );
        }
    }

    #[test]
    fn strict_decoder_refuses_each_missing_field_legacy_names_extras_and_reordered_facts(
    ) -> Result<()> {
        let observation = observation();
        let value = serde_json::to_value(&observation)?;
        // Every observation key is required, including nullable Option fields.
        for key in value.as_object().unwrap().keys() {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                serde_json::from_value::<SchedulerRetirementResolutionObservation>(missing)
                    .is_err(),
                "decoder accepted missing {key}"
            );
        }
        for (parent, field) in [
            ("/original", "content_sha256"),
            ("/stopped_plan/roles/0", "current"),
            ("/resolutions/0", "outcome"),
            ("/resolutions/0", "effect_failure"),
            ("/resolutions/0", "error_sha256"),
        ] {
            let mut missing = value.clone();
            missing
                .pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                serde_json::from_value::<SchedulerRetirementResolutionObservation>(missing)
                    .is_err(),
                "decoder accepted missing nested {parent}/{field}"
            );
        }
        for change in [
            "legacy",
            "extra",
            "coordinate-extra",
            "truncate",
            "extend",
            "reorder",
            "foreign",
            "no-plan",
        ] {
            let mut changed = value.clone();
            match change {
                "legacy" => {
                    changed["stopped_coordinates"] =
                        serde_json::to_value(&observation.stopped_coordinates)?
                }
                "extra" => {
                    changed
                        .as_object_mut()
                        .unwrap()
                        .insert("authority".into(), serde_json::json!(true));
                }
                "coordinate-extra" => {
                    changed["stopped_coordinates"][0]
                        .as_object_mut()
                        .unwrap()
                        .insert("name".into(), serde_json::json!("untrusted"));
                }
                "truncate" => {
                    changed["stopped_coordinates"].as_array_mut().unwrap().pop();
                }
                "extend" => {
                    changed["stopped_coordinates"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"state":"absent"}));
                }
                "reorder" => {
                    let main = observation
                        .stopped_coordinates
                        .iter()
                        .position(|item| item.name == observation.quarantine_name)
                        .unwrap();
                    let wal = observation
                        .stopped_coordinates
                        .iter()
                        .position(|item| item.name == "scheduler.sqlite3-wal")
                        .unwrap();
                    changed["stopped_coordinates"]
                        .as_array_mut()
                        .unwrap()
                        .swap(main, wal);
                }
                "foreign" => {
                    changed["stopped_coordinates"][0] =
                        serde_json::json!({"state":"foreign_regular","device":1,"inode":2})
                }
                _ => changed["stopped_plan"] = serde_json::Value::Null,
            }
            assert!(
                serde_json::from_value::<SchedulerRetirementResolutionObservation>(changed)
                    .is_err(),
                "decoder accepted {change}"
            );
        }
        let mut ordinary = observation.clone();
        ordinary.stopped_plan = None;
        ordinary.selected_source = None;
        ordinary.stopped_coordinates.clear();
        ordinary.validate()?;
        let wire = serde_json::to_value(&ordinary)?;
        assert!(wire["stopped_plan"].is_null() && wire["selected_source"].is_null());
        assert_eq!(
            serde_json::from_value::<SchedulerRetirementResolutionObservation>(wire)?,
            ordinary
        );
        Ok(())
    }
}
