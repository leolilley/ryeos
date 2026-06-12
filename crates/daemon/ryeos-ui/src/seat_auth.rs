//! Seat caller authentication for studio services.
//!
//! Studio sources serve two caller lanes with one gate:
//! - **browser sessions** (cookie → `session:<id>` fingerprint), and
//! - **verified operators** (signed requests through the one daemon
//!   invocation path — `/execute` → service dispatch).
//!
//! An operator-signed call is strictly more privileged than a browser
//! session, so every session-gated read source accepts both. Session-only
//! values (bound project root, read-only) fall back per the operator's
//! request parameters.

use ryeos_app::handler_context::HandlerContext;
use ryeos_app::handler_error::HandlerError;
use ryeos_app::state::AppState;

use crate::browser_session::BrowserSession;
use crate::state::get_ui_state;

pub enum SeatCaller {
    Session(BrowserSession),
    Operator { fingerprint: String },
}

impl SeatCaller {
    /// Project root bound to the caller, if any (sessions only —
    /// operators pass `project_path` in params).
    pub fn project_root(&self) -> Option<&str> {
        match self {
            SeatCaller::Session(session) => session.project_root.as_deref(),
            SeatCaller::Operator { .. } => None,
        }
    }

    pub fn read_only(&self) -> bool {
        match self {
            SeatCaller::Session(session) => session.read_only,
            SeatCaller::Operator { .. } => false,
        }
    }
}

/// Accept a valid browser session OR a verified operator principal.
pub fn require_seat_caller(
    ctx: &HandlerContext,
    state: &AppState,
) -> Result<SeatCaller, HandlerError> {
    if let Some(session_id) = ctx.fingerprint.strip_prefix("session:") {
        return get_ui_state(state)
            .expect("UiState not set")
            .browser_sessions
            .get_session(session_id)
            .map(SeatCaller::Session)
            .ok_or(HandlerError::Forbidden("session expired or invalid".into()));
    }
    if ctx.verified && !ctx.fingerprint.is_empty() {
        return Ok(SeatCaller::Operator {
            fingerprint: ctx.fingerprint.clone(),
        });
    }
    Err(HandlerError::Forbidden(
        "browser session or verified operator required".into(),
    ))
}
