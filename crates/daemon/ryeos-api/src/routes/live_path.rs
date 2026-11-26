//! Admission of a request-named live project path at a route.

use std::path::{Path, PathBuf};

use ryeos_app::live_project_path::{LiveProjectPathRefusal, admit_live_project_path};

use crate::route_error::RouteDispatchError;
use crate::routes::invocation::RoutePrincipal;

/// The route answer for a refused live project path: one fixed 403 body for
/// a key that is not the operator, whatever the cause; the operator's own
/// texts otherwise.
pub(crate) fn route_error_for(refusal: LiveProjectPathRefusal) -> RouteDispatchError {
    match refusal {
        LiveProjectPathRefusal::NotRegisteredToCaller | LiveProjectPathRefusal::NodeInternal(_) => {
            RouteDispatchError::Forbidden(refusal.to_string())
        }
        LiveProjectPathRefusal::BadPath(_) => RouteDispatchError::BadRequest(refusal.to_string()),
    }
}

/// Admit `requested` as a live project for the route's principal and return
/// its canonical root. No principal is a key that is not the operator, so it
/// is refused like any other unregistered caller.
pub(crate) fn admit_route_live_path(
    state: &ryeos_app::state::AppState,
    principal: Option<&RoutePrincipal>,
    requested: &Path,
) -> Result<PathBuf, RouteDispatchError> {
    let (key, operator) = match principal {
        Some(principal) => (
            principal.id.as_str(),
            ryeos_app::operator_authority::require_admitted_operator(
                state,
                &principal.handler_context(),
            )
            .is_ok(),
        ),
        None => ("", false),
    };
    admit_live_project_path(state, key, operator, requested)
        .map(|admitted| admitted.canonical)
        .map_err(route_error_for)
}
