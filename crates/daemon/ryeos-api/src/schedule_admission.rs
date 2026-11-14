//! Admission checks shared by schedule registration, reauthorization and
//! project deployment: a schedule whose authority its first fire would refuse
//! is refused when it is written.

use anyhow::Result;
use serde_json::Value;

/// Code of the refusal for a schedule whose pinned authority would be refused
/// at its first fire.
pub const SCHEDULED_AUTHORITY_REFUSED: &str = "scheduled_authority_refused";
/// Code of the refusal for a schedule whose capabilities do not cover what its
/// execution policy needs.
pub const SCHEDULE_POLICY_CAPABILITY_NOT_COVERED: &str = "schedule_policy_capability_not_covered";

pub(crate) fn structured_refusal(
    status: u16,
    code: &str,
    message: String,
    detail: Value,
) -> anyhow::Error {
    let mut body = serde_json::json!({ "error": message, "error_code": code });
    if let (Some(body), Value::Object(detail)) = (body.as_object_mut(), detail) {
        body.extend(detail);
    }
    anyhow::Error::new(ryeos_app::handler_error::HandlerError::Structured {
        code: code.to_owned(),
        status,
        body,
    })
}

/// Refuse at registration, and when project deployment writes a schedule, a
/// schedule whose authority the first fire would
/// refuse: the same revalidation a fire runs (the pinned grant generations,
/// class, origin, capability coverage and, for a forwarded principal, the
/// delegation's expiry and revocation) and the same execution-policy
/// preauthorization (live project access, advance-head publication and
/// operator-vault exposure must each be covered by the schedule's own
/// capabilities, because a fire runs with exactly those). Without this a
/// schedule registers, then pauses at its first fire.
pub(crate) fn refuse_authority_that_cannot_hold_at_fire(
    parts: &ryeos_app::scheduled_execution_authority::ScheduleAuthorityParts,
    execution: &ryeos_scheduler_contract::ScheduleExecution,
) -> Result<()> {
    let handler_context =
        ryeos_app::scheduled_execution_authority::revalidate_scheduled_execution_with(
            parts, execution,
        )
        .map_err(
            |error| match ryeos_scheduler_contract::ScheduledAuthorityRefusal::find(&error) {
                Some(refusal) => structured_refusal(
                    403,
                    SCHEDULED_AUTHORITY_REFUSED,
                    format!(
                        "this schedule's authority would be refused at its first fire \
                             ({}): {}",
                        refusal.kind, refusal.detail
                    ),
                    serde_json::json!({
                        "refusal": refusal.kind,
                        "outcome": refusal.kind.fire_outcome(),
                    }),
                ),
                None => error,
            },
        )?;
    crate::routes::response_modes::execute_mode::preauthorize_execution_policy_with(
        &execution.policy,
        &handler_context.scopes,
        &parts.authorizer,
    )
    .map_err(|error| {
        use ryeos_app::execution_policy::OperatorVaultCapabilityRequired;
        match error.downcast_ref::<OperatorVaultCapabilityRequired>() {
            Some(refusal) => structured_refusal(
                403,
                OperatorVaultCapabilityRequired::CODE,
                refusal.to_string(),
                serde_json::json!({ "required_capability": refusal.required_capability() }),
            ),
            None => structured_refusal(
                403,
                SCHEDULE_POLICY_CAPABILITY_NOT_COVERED,
                format!(
                    "this schedule's execution policy would be refused at its first fire: \
                     {error}; add the capability to the schedule's capabilities and register \
                     again"
                ),
                Value::Null,
            ),
        }
    })
}
