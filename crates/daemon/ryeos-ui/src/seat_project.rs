//! The project a seat states, computed for the binding descriptor.
//!
//! A binding descriptor carries `seat_project` as display data. The daemon
//! judges it with `ryeos_app::seat_authority::resolve_seat_project` when it
//! admits a binding (mint) and again each time it admits a successor
//! generation (rebind, revise, Surface open). The judgement is made from the
//! key's live registration, so a registration that lapses after mint changes
//! the next successor's state and nothing already admitted.
//!
//! The carried value is never an input to an authority decision. A thread
//! read rebuilds its reader from the sealed binding and the live trust store,
//! a launch or live write judges the project in its own check, and nothing
//! here or elsewhere reads `seat_project` back from a descriptor, a stored
//! payload or a client.

use std::path::PathBuf;

use anyhow::Result;
use ryeos_app::seat_authority::{SeatProjectState, SeatProjectSubject, resolve_seat_project};
use ryeos_app::state::AppState;
use ryeos_client_base::ui::{UiSeatAbsence, UiSeatProject};
use ryeos_state::project_identity::LogicalProject;

use crate::compiled_binding::SessionCompiledUiBinding;

/// The wire form of a seat project state.
pub(crate) fn to_wire(state: &SeatProjectState) -> UiSeatProject {
    match state {
        SeatProjectState::Stated(identity) => UiSeatProject::Stated {
            project: identity.to_string(),
        },
        SeatProjectState::Absent(absence) => UiSeatProject::Absent {
            absence: UiSeatAbsence::from_code(absence.code()),
        },
        SeatProjectState::Projectless => UiSeatProject::Projectless,
    }
}

/// The seat project of `compiled` as this node judges it now, for the seat
/// key the binding was compiled for and the session's `scopes`. A binding with
/// no checkout is projectless. A binding on a checkout that sealed no identity
/// is absent for the reason it sealed. A binding that sealed an identity is
/// `stated` only while the key still resolves the checkout to it.
pub(crate) fn resolve_for_binding(
    state: &AppState,
    compiled: &SessionCompiledUiBinding,
    scopes: &[String],
) -> Result<UiSeatProject> {
    let binding = &compiled.binding;
    let subject = match (binding.project_root.as_deref(), &binding.signer_project) {
        (None, _) => SeatProjectSubject::Projectless,
        (Some(root), LogicalProject::Stated(identity)) => SeatProjectSubject::Project {
            identity: identity.clone(),
            checkout: PathBuf::from(root),
        },
        (Some(_), sealed @ LogicalProject::Absent(_)) => {
            return Ok(to_wire(&SeatProjectState::from_sealed(true, sealed)));
        }
    };
    let resolved = resolve_seat_project(state, &binding.principal_id, scopes, &subject)?;
    Ok(to_wire(&resolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_app::seat_authority::SeatProjectAbsence;
    use ryeos_state::project_identity::ProjectIdentity;

    fn identity() -> ProjectIdentity {
        format!("pid:v1:{}/snap-track", "ab".repeat(32))
            .parse()
            .unwrap()
    }

    #[test]
    fn every_daemon_state_has_the_client_wire_shape() {
        for state in [
            SeatProjectState::Stated(identity()),
            SeatProjectState::Absent(SeatProjectAbsence::Unregistered),
            SeatProjectState::Absent(SeatProjectAbsence::OwnerKeyNotActive),
            SeatProjectState::Absent(SeatProjectAbsence::NoProjectName),
            SeatProjectState::Absent(SeatProjectAbsence::CheckoutNameChanged),
            SeatProjectState::Projectless,
        ] {
            let daemon_wire = serde_json::to_value(&state).unwrap();
            let client = to_wire(&state);
            assert_eq!(serde_json::to_value(&client).unwrap(), daemon_wire);
            if let UiSeatProject::Absent { absence } = &client {
                assert!(!matches!(absence, UiSeatAbsence::Unknown(_)));
            }
            // The client reads exactly what the daemon wrote.
            assert_eq!(
                serde_json::from_value::<UiSeatProject>(daemon_wire).unwrap(),
                client
            );
        }
    }
}
