//! The runtime's single error type.
//!
//! It separates what the sender can know: a definite daemon answer
//! ([`RuntimeError::Daemon`]), a failure to deliver or read ([`RuntimeError::Network`],
//! [`RuntimeError::UnreadableResponse`]; the mutation may have been applied),
//! and local refusals that never reached the network.

use ryeos_client_base::ui::RyeOsUiError;

/// Which start check refused a seat that was already claimed and is left open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeatOpenStage {
    /// The replayed seat history was not accepted by the core, so whether the
    /// claimed owner is safe to give back is uncertain.
    ReplayAcceptance,
    /// The reducer did not positively accept the claimed producer.
    ProducerReadiness,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeError {
    /// The daemon answered with a non-2xx status. Carries the daemon's own
    /// message so a validation failure is never reported as "daemon down".
    #[error("daemon returned {status} for {path}: {message}")]
    Daemon {
        path: String,
        status: u16,
        message: String,
    },

    /// The request could not be delivered, or the connection failed before a
    /// complete answer. A mutation may nevertheless have been applied.
    #[error("{path}: delivery outcome is unknown: {message}")]
    Network { path: String, message: String },

    /// A 2xx answer whose body could not be read or decoded. The request was
    /// delivered and its effect is unknown.
    #[error("{path}: response outcome is unknown")]
    UnreadableResponse { path: String },

    /// JSON text was refused: a malformed or out-of-domain document on
    /// decode, or an unencodable value on encode. Decode failures read
    /// `invalid JSON at offset N: ...` with the offset in UTF-16 code units.
    #[error("{0}")]
    Decode(String),

    /// The seat producer was fenced; no further mutation may be sent.
    #[error("seat is fenced")]
    Fenced,

    /// An unrecoverable protocol or invariant violation.
    #[error("{0}")]
    Fatal(String),

    /// A claimed seat was refused during start and deliberately not given
    /// back; its lease lifecycle settles it. Carries the seat thread so the
    /// host can name it.
    #[error("{message}")]
    SeatLeftOpen {
        thread_id: String,
        stage: SeatOpenStage,
        message: String,
    },

    /// The presentation's seat is held: a writer is still active, or the seat
    /// is closed. Carries the observation, whose replacement choices are the
    /// operator's to make.
    #[error("another writer holds this presentation")]
    WriterHeld(Box<ryeos_client_base::ui::UiPresentationProducerObservation>),

    /// A request the runtime refused to build or send.
    #[error("UI binding request: {0}")]
    BindingRequest(String),

    /// The stored pending-append record is not parseable JSON.
    #[error("retained seat append is malformed: {0}")]
    RetentionMalformed(String),

    /// The stored pending-append record parsed but does not match the schema.
    #[error("retained seat append has an invalid schema")]
    RetentionInvalidSchema,

    /// The platform key-value store refused a write.
    #[error("storage write refused")]
    StoreWrite,

    /// Request authentication failed locally (for example no identity).
    #[error("sign: {0}")]
    Sign(String),
}

impl RuntimeError {
    /// HTTP status of a definite daemon answer.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Daemon { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// True when a mutation that failed this way may nevertheless have been
    /// applied: nothing definite came back from the daemon.
    pub fn is_unknown_delivery(&self) -> bool {
        matches!(self, Self::Network { .. } | Self::UnreadableResponse { .. })
    }

    /// The presentation-layer error for an effect that failed with this error.
    pub fn to_ui_error(&self) -> RyeOsUiError {
        if self.is_unknown_delivery() {
            RyeOsUiError::outcome_unknown("platform_effect_failed", self.to_string())
        } else {
            RyeOsUiError::definite("platform_effect_failed", self.to_string())
        }
    }
}

impl From<serde_json::Error> for RuntimeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Decode(error.to_string())
    }
}

impl From<ryeos_ui_presentation_contract::payload_digest::SeatPayloadDigestError> for RuntimeError {
    fn from(error: ryeos_ui_presentation_contract::payload_digest::SeatPayloadDigestError) -> Self {
        Self::Decode(error.to_string())
    }
}
