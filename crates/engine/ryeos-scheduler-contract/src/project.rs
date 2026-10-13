//! Project schedule deployment port. Values carry no publication authority.
//! One service-owned session retains the same gate, directory lock and partial
//! mutation state through resolution; dropping it never implies success.

use crate::ScheduleExecutionAuthority;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{future::Future, path::PathBuf, pin::Pin, sync::Arc};

#[derive(Debug, Clone, Default)]
pub struct ScheduleDeployReport {
    pub declared: usize,
    pub created: usize,
    pub updated: usize,
    pub deleted: usize,
}

/// Exact signed source before apply, rather than a reconstructed projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRecoveryBeforeImage {
    pub schedule_id: String,
    pub signed_yaml: Option<String>,
}

/// Adapter to the caller's existing verified registration authority.
pub trait ProjectScheduleCaller: Send + Sync {
    fn require_registration(&self) -> Result<()>;
    fn require_owner(&self, schedule_id: &str, owner_fingerprint: &str) -> Result<()>;
    fn require_capability(&self, capability: &str) -> Result<()>;
    fn authenticated_execution_authority(
        &self,
        registration_request_hash: String,
    ) -> Result<ScheduleExecutionAuthority>;
}

pub struct ProjectScheduleDeployRequest {
    pub staging_root: PathBuf,
    pub project_path: PathBuf,
    pub project_key: String,
    pub snapshot_hash: String,
    pub caller: Arc<dyn ProjectScheduleCaller>,
}

pub type ProjectScheduleFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub trait ProjectScheduleDeployParticipant: Send + Sync {
    /// Retain the bound service's existing write gate for the entire session.
    fn acquire_project_schedules(
        &self,
    ) -> ProjectScheduleFuture<'_, Box<dyn ProjectScheduleSession>>;

    /// Try to acquire a project session without waiting on the scheduler gate.
    /// Recovery loops use this for pending-acquisition markers so one busy
    /// gate cannot block retries of other operations. `None` means busy.
    fn try_acquire_project_schedules(&self) -> Result<Option<Box<dyn ProjectScheduleSession>>>;
}

/// Private phases and concrete transactions remain with the service. Invalid
/// sequencing fails before mutation. Errors retain locks, before-images and
/// partial changes; rollback failure never discards the recovery owner.
pub trait ProjectScheduleSession: Send {
    fn plan(&mut self, request: ProjectScheduleDeployRequest) -> Result<ScheduleDeployReport>;
    fn has_actions(&self) -> Result<bool>;
    /// Capture and retain exact before-images and the directory lock before
    /// the API publishes its authenticated apply journal.
    fn prepare_recovery(&mut self) -> ProjectScheduleFuture<'_, ()>;
    fn before_images(&self) -> Result<&[ScheduleRecoveryBeforeImage]>;
    fn prepare(&mut self) -> Result<ScheduleDeployReport>;
    fn finalize(&mut self) -> Result<()>;
    fn rollback(&mut self) -> Result<()>;
    /// Preserve unresolved obligations and dispatch refusal; this cannot infer
    /// publication or journal durability, which remain the API's authority.
    fn retain_for_recovery(&mut self) -> Result<()>;
    /// Validate authentic journal images and retain/reuse the directory lock
    /// before entering the synchronous CAS section.
    fn prepare_restoration<'a>(
        &'a mut self,
        entries: &'a [ScheduleRecoveryBeforeImage],
    ) -> ProjectScheduleFuture<'a, ()>;
    /// Restore synchronously under retained locks; failures retain ownership.
    fn restore(&mut self) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_images_preserve_exact_signed_source_and_absence() {
        for signed_yaml in [None, Some("signed: source\n# exact bytes\n".to_owned())] {
            let image = ScheduleRecoveryBeforeImage {
                schedule_id: "project-schedule".to_owned(),
                signed_yaml: signed_yaml.clone(),
            };
            let encoded = serde_json::to_value(&image).unwrap();
            assert_eq!(encoded["schedule_id"], "project-schedule");
            assert_eq!(
                encoded["signed_yaml"],
                serde_json::to_value(&signed_yaml).unwrap()
            );
            let decoded: ScheduleRecoveryBeforeImage = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded.signed_yaml, signed_yaml);
        }
    }

    #[test]
    fn recovery_images_refuse_unknown_fields_and_missing_identity() {
        assert!(
            serde_json::from_value::<ScheduleRecoveryBeforeImage>(serde_json::json!({
                "schedule_id": "project-schedule", "signed_yaml": null, "authorized": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ScheduleRecoveryBeforeImage>(serde_json::json!({
                "signed_yaml": null
            }))
            .is_err()
        );
    }
}
