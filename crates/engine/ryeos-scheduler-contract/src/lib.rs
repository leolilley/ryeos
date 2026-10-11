//! Canonical pure schedule contracts for App and selected scheduler composition.
//! No DB, timer, daemon, App or executor implementation belongs here.

mod accounting;
mod execution;
mod expression;
mod maintenance;
mod project;
mod retirement;
mod vocabulary;

pub use accounting::{SchedulerFireHistoryAccounting, SchedulerFireHistoryDiscardReport};
pub use execution::{
    ScheduleExecution, ScheduleExecutionAuthority, validate_schedule_execution,
    validate_schedule_ref_bindings,
};
pub use expression::{parse_iso_ts, validate_expression, validate_schedule_id, validate_timezone};
pub use maintenance::{
    NodeMaintenancePolicy, NodeMaintenanceSchedulePolicy, OfflineNativeQuota, OfflineResetQuota, SECTION_NAME,
    OFFLINE_RESET_SOURCE_ROLES, validate_offline_native_class,
};
pub use vocabulary::{MisfirePolicy, OverlapPolicy, parse_misfire_policy, parse_overlap_policy};

pub use project::{
    ProjectScheduleCaller, ProjectScheduleDeployParticipant, ProjectScheduleDeployRequest,
    ProjectScheduleFuture, ProjectScheduleSession, ScheduleDeployReport,
    ScheduleRecoveryBeforeImage,
};
pub use retirement::{
    SCHEDULER_RETIREMENT_MAX_BASE_AND_ROLE_ENTRIES, SCHEDULER_RETIREMENT_MAX_CLEANUP_COORDINATES,
    SCHEDULER_RETIREMENT_MAX_DIRECTORY_ENTRIES, SCHEDULER_RETIREMENT_MAX_GENERATIONS,
    SCHEDULER_RETIREMENT_MAX_PUBLICATION_RESIDUE, SCHEDULER_RETIREMENT_MAX_RECOVERY_CANDIDATES,
    SCHEDULER_RETIREMENT_MAX_RESERVED_ENTRIES, SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES,
    SCHEDULER_RETIREMENT_MAX_SIGNED_BYTES_TOTAL, SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS,
    SCHEDULER_RETIREMENT_MAX_SIGNED_RECORDS_PER_GENERATION,
    SCHEDULER_RETIREMENT_PHASE_RECORDS_PER_GENERATION, SCHEDULER_RETIREMENT_PURPOSE,
    SCHEDULER_RETIREMENT_RECORD_MAX_BYTES,
    SCHEDULER_RETIREMENT_RESOLUTION_COORDINATES_PER_GENERATION,
    SCHEDULER_RETIREMENT_RESOLUTION_PURPOSE,
    SCHEDULER_RETIREMENT_RESOLUTION_RECORDS_PER_GENERATION,
    SCHEDULER_RETIREMENT_RESOLUTIONS_PER_ROLE, SchedulerRetirementEffect,
    SchedulerRetirementFailureDisposition, SchedulerRetirementFailureFact,
    SchedulerRetirementFailureStage, SchedulerRetirementIntent, SchedulerRetirementNonRegularKind,
    SchedulerRetirementObservedCoordinate, SchedulerRetirementObservedQuarantine,
    SchedulerRetirementObservedStoppedCoordinate, SchedulerRetirementPhase,
    SchedulerRetirementRecord, SchedulerRetirementRecoveryBinding,
    SchedulerRetirementRecoveryResolutionRef, SchedulerRetirementResolutionAction,
    SchedulerRetirementResolutionAttemptFact, SchedulerRetirementResolutionIntent,
    SchedulerRetirementResolutionObservation, SchedulerRetirementResolutionOutcome,
    SchedulerRetirementResolutionState, SchedulerRetirementRole, SchedulerRetirementRoleIdentity,
    SchedulerRetirementStoppedPlan, SchedulerRetirementStoppedRolePlan,
    SchedulerRetirementStoppedSource, decode_scheduler_retirement_record,
    encode_scheduler_retirement_record,
};
