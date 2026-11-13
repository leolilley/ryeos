//! Typed refusals for the credential-profile login ceremony.
//!
//! Each refusal names what the node saw and the operator act that moves the
//! profile on. The API maps every variant to HTTP 409 with its stable code.

use serde_json::{Value, json};

/// Why a credential-profile ceremony step was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialCeremonyRefusal {
    /// `begin` saw a login that is still inside its window.
    #[error(
        "credential_enrollment_in_progress: credential profile '{profile_id}' is {state} for login epoch {login_epoch}; finish that login, or let it expire and run `ryeos codex profile abandon-login {profile_id} {login_epoch}`"
    )]
    EnrollmentInProgress {
        profile_id: String,
        state: String,
        login_epoch: u64,
        login_expires_at_ms: Option<i64>,
    },
    /// `begin` saw an expired login nobody finished.
    #[error(
        "credential_enrollment_abandoned: credential profile '{profile_id}' was left {state} by expired login epoch {login_epoch}; end the login session that holds the profile, then run `ryeos codex profile abandon-login {profile_id} {login_epoch}`"
    )]
    EnrollmentAbandoned {
        profile_id: String,
        state: String,
        login_epoch: u64,
    },
    /// `confirm` needs the login session's worker to have released the profile.
    #[error(
        "credential_confirm_lock_held: credential profile '{profile_id}' is still held by {lock_owner}{}; end that login session (`ryeos worker session terminate <chain-root-id> cancelled`), wait until the profile has no lock_owner, then confirm login epoch {login_epoch}",
        holding_session_text(.holding_chain_root_id.as_deref())
    )]
    ConfirmLockHeld {
        profile_id: String,
        login_epoch: u64,
        lock_owner: String,
        holding_chain_root_id: Option<String>,
    },
    /// `confirm` arrived after the login window closed.
    #[error(
        "credential_confirm_expired: the confirmation window for credential profile '{profile_id}' login epoch {login_epoch} has closed; {}run `ryeos codex profile abandon-login {profile_id} {login_epoch}` and log in again",
        expired_lock_text(.lock_owner.as_deref())
    )]
    ConfirmExpired {
        profile_id: String,
        login_epoch: u64,
        lock_owner: Option<String>,
    },
    /// `confirm` named a ceremony the profile is not in.
    #[error(
        "credential_confirm_wrong_state: credential profile '{profile_id}' is {state} at login epoch {current_login_epoch}; confirmation needs state confirming at login epoch {requested_login_epoch}"
    )]
    ConfirmWrongState {
        profile_id: String,
        state: String,
        current_login_epoch: u64,
        requested_login_epoch: u64,
    },
    /// `abandon-login` may only clear an expired, unlocked enrollment.
    #[error(
        "credential_abandon_refused: credential profile '{profile_id}' is {state} at login epoch {current_login_epoch} (requested {requested_login_epoch}); {reason}"
    )]
    AbandonRefused {
        profile_id: String,
        state: String,
        current_login_epoch: u64,
        requested_login_epoch: u64,
        reason: String,
    },
}

fn holding_session_text(chain_root_id: Option<&str>) -> String {
    chain_root_id
        .map(|id| format!(" (login session {id})"))
        .unwrap_or_default()
}

fn expired_lock_text(lock_owner: Option<&str>) -> String {
    lock_owner
        .map(|owner| format!("end the login session that still holds it ({owner}), then "))
        .unwrap_or_default()
}

impl CredentialCeremonyRefusal {
    /// Stable machine code clients branch on.
    pub fn code(&self) -> &'static str {
        match self {
            Self::EnrollmentInProgress { .. } => "credential_enrollment_in_progress",
            Self::EnrollmentAbandoned { .. } => "credential_enrollment_abandoned",
            Self::ConfirmLockHeld { .. } => "credential_confirm_lock_held",
            Self::ConfirmExpired { .. } => "credential_confirm_expired",
            Self::ConfirmWrongState { .. } => "credential_confirm_wrong_state",
            Self::AbandonRefused { .. } => "credential_abandon_refused",
        }
    }

    /// Structured response body: the code, the full refusal and what was seen.
    pub fn body(&self) -> Value {
        let mut body = json!({
            "code": self.code(),
            "error": self.to_string(),
        });
        let seen = match self {
            Self::EnrollmentInProgress {
                profile_id,
                state,
                login_epoch,
                login_expires_at_ms,
            } => json!({
                "profile_id": profile_id,
                "state": state,
                "login_epoch": login_epoch,
                "login_expires_at_ms": login_expires_at_ms,
            }),
            Self::EnrollmentAbandoned {
                profile_id,
                state,
                login_epoch,
            } => json!({
                "profile_id": profile_id,
                "state": state,
                "login_epoch": login_epoch,
            }),
            Self::ConfirmLockHeld {
                profile_id,
                login_epoch,
                lock_owner,
                holding_chain_root_id,
            } => json!({
                "profile_id": profile_id,
                "login_epoch": login_epoch,
                "lock_owner": lock_owner,
                "holding_chain_root_id": holding_chain_root_id,
            }),
            Self::ConfirmExpired {
                profile_id,
                login_epoch,
                lock_owner,
            } => json!({
                "profile_id": profile_id,
                "login_epoch": login_epoch,
                "lock_owner": lock_owner,
            }),
            Self::ConfirmWrongState {
                profile_id,
                state,
                current_login_epoch,
                requested_login_epoch,
            } => json!({
                "profile_id": profile_id,
                "state": state,
                "login_epoch": current_login_epoch,
                "requested_login_epoch": requested_login_epoch,
            }),
            Self::AbandonRefused {
                profile_id,
                state,
                current_login_epoch,
                requested_login_epoch,
                reason,
            } => json!({
                "profile_id": profile_id,
                "state": state,
                "login_epoch": current_login_epoch,
                "requested_login_epoch": requested_login_epoch,
                "reason": reason,
            }),
        };
        if let (Some(body), Value::Object(seen)) = (body.as_object_mut(), seen) {
            body.extend(seen);
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_refusal_body_carries_its_code_and_profile() {
        let refusals = [
            CredentialCeremonyRefusal::EnrollmentInProgress {
                profile_id: "p".into(),
                state: "enrolling".into(),
                login_epoch: 2,
                login_expires_at_ms: Some(10),
            },
            CredentialCeremonyRefusal::EnrollmentAbandoned {
                profile_id: "p".into(),
                state: "confirming".into(),
                login_epoch: 2,
            },
            CredentialCeremonyRefusal::ConfirmLockHeld {
                profile_id: "p".into(),
                login_epoch: 2,
                lock_owner: "worker".into(),
                holding_chain_root_id: Some("T-login".into()),
            },
            CredentialCeremonyRefusal::ConfirmExpired {
                profile_id: "p".into(),
                login_epoch: 2,
                lock_owner: None,
            },
            CredentialCeremonyRefusal::ConfirmWrongState {
                profile_id: "p".into(),
                state: "active".into(),
                current_login_epoch: 2,
                requested_login_epoch: 1,
            },
            CredentialCeremonyRefusal::AbandonRefused {
                profile_id: "p".into(),
                state: "active".into(),
                current_login_epoch: 2,
                requested_login_epoch: 2,
                reason: "no login to abandon".into(),
            },
        ];
        for refusal in refusals {
            let body = refusal.body();
            assert_eq!(body["code"], refusal.code());
            assert_eq!(body["profile_id"], "p");
            assert!(
                body["error"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("{}:", refusal.code()))
            );
        }
    }

    #[test]
    fn lock_held_names_the_session_and_expired_names_abandon() {
        let held = CredentialCeremonyRefusal::ConfirmLockHeld {
            profile_id: "p".into(),
            login_epoch: 3,
            lock_owner: "worker".into(),
            holding_chain_root_id: Some("T-login".into()),
        }
        .to_string();
        assert!(held.contains("T-login"));
        assert!(held.contains("worker session terminate"));
        let expired = CredentialCeremonyRefusal::ConfirmExpired {
            profile_id: "p".into(),
            login_epoch: 3,
            lock_owner: None,
        }
        .to_string();
        assert!(expired.contains("ryeos codex profile abandon-login p 3"));
    }
}
