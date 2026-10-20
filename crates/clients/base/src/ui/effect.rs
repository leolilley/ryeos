use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::binding::{UiBindingRequest, UiBindingRequestBounds};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsEffect {
    pub id: u64,
    pub kind: RyeOsEffectKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RyeOsEffectKind {
    /// The existing seat owner has prepared this child and the parent retained
    /// completion has been acknowledged. Expose a same-origin explicit link;
    /// never prepare again, claim the child, or open a popup automatically.
    ShowPreparedPresentationDocument {
        presentation_id: String,
    },
    /// Browser host preparation through the existing seat owner. Show an
    /// explicit non-bearer link only after preparation; the child independently
    /// authenticates membership and claims its seat. Never encode session ID
    /// into that link or automatically open a document.
    OpenNewPresentationDocument {
        expected_session_id: String,
        root_binding: super::binding::UiBindingAttachmentCoordinate,
    },
    /// Read-only existing-seat observation for one already-captured source.
    /// Adapters post only the destination seat coordinate. The captured
    /// export remains in Core's pending effect for exact result-time comparison.
    ObserveTransferDestination {
        destination_presentation_id: String,
        destination_thread_id: String,
        destination_chain_root_id: String,
        destination_root_binding: super::binding::UiBindingAttachmentCoordinate,
        destination_initialization_digest: String,
        source_producer: super::binding::UiPresentationProducer,
        source_chain_root_id: String,
        source_root_binding: super::binding::UiBindingAttachmentCoordinate,
        source_revision: u64,
        source_export: super::transfer_export::UiTransferExport,
        request_bounds: UiBindingRequestBounds,
    },
    /// Signed transfer Prepare contact only. MOVE requires acknowledged,
    /// authenticated original preparation/completion links. Its response is
    /// observation, never release or destination activation authority.
    PrepareTransfer {
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
        export: super::transfer_export::UiTransferExport,
        freeze: Option<ryeos_ui_presentation_contract::transfer::MoveFreezeEvidence>,
        request_bounds: UiBindingRequestBounds,
    },
    /// The source/destination pairing was completed at one authenticated
    /// cold cut; this remains a contact, never response-derived ownership.
    ContactTransfer {
        request: UiTransferContactRequest,
        request_bounds: UiBindingRequestBounds,
    },
    /// Fetch one source from the daemon-compiled session binding. The
    /// renderer receives a coordinate and bounded payload, never the source's
    /// executable item ref or capability requirements.
    FetchSource {
        tile_id: String,
        request: UiBindingRequest,
        request_bounds: UiBindingRequestBounds,
    },
    /// Invoke one content-declared affordance through the daemon-compiled
    /// session binding. The signed view owns the target and substitution
    /// template; the client sends only the producer payload.
    InvokeBinding {
        request: UiBindingRequest,
        request_bounds: UiBindingRequestBounds,
        intent: InvokeIntent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        success_notice: Option<String>,
        /// Exact mounted view that issued an affordance invocation. Delayed
        /// observation refreshes target this retained mount, never whatever
        /// view or view set happens to be active when the result arrives.
        invocation_origin: Option<crate::ids::RyeOsViewInstanceKey>,
        /// Exact mounted input that issued this invocation. Delayed results
        /// may mutate only this coordinate; current focus is never authority.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_origin: Option<super::model::RyeOsInputAddress>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route_seq: Option<u64>,
        #[serde(default)]
        ratchet_on_thread_id: bool,
    },
    /// Emitted only after the exact prepared seat prefix is acknowledged, or
    /// recovered from authoritative replay. Its original identity is immutable.
    InvokePresentationOperation {
        operation: super::admission_operation::UiPreparedPresentationOperation,
    },
    SetLocationHash {
        hash: String,
    },
    CopyToClipboard {
        text: String,
    },
    OpenUrl {
        url: String,
    },
    /// Replace an immutable UI authority generation. Every renderer redeems
    /// the one-shot URL through the predecessor session, then adopts only the
    /// authenticated successor and reloads its compiled presentation.
    ReplaceSession {
        session_id: String,
        launch_url: String,
    },
    /// Explicitly release one exact non-surface binding attachment from the
    /// authenticated session. This is session-wide and never implied by
    /// closing local presentation.
    ReleaseBindingAttachment {
        binding_attachment_id: String,
        binding_generation: u64,
        binding_digest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UiTransferContactRequest {
    AcceptView {
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
        inbound: ryeos_ui_presentation_contract::transfer::TransferEvidence,
        preparation: ryeos_ui_presentation_contract::transfer::TransferEvidence,
        surface_instance_id: crate::ids::SurfaceInstanceId,
        view_set_id: crate::ids::ViewSetId,
        placement: super::composition_operation::CompositionPlacement,
    },
    AcceptTabs {
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
        inbound: ryeos_ui_presentation_contract::transfer::TransferEvidence,
        preparation: ryeos_ui_presentation_contract::transfer::TransferEvidence,
        surface_instance_id: crate::ids::SurfaceInstanceId,
    },
    AcceptSharedDock {
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
        inbound: ryeos_ui_presentation_contract::transfer::TransferEvidence,
        preparation: ryeos_ui_presentation_contract::transfer::TransferEvidence,
        edge: String,
    },
    Commit {
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
    },
    Abort {
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
    },
    Activate {
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
    },
    CancelFreeze {
        transfer: ryeos_ui_presentation_contract::transfer::TransferIdentity,
    },
}

impl UiTransferContactRequest {
    pub fn action_name(&self) -> &'static str {
        match self {
            Self::AcceptView { .. } => "accept_view",
            Self::AcceptTabs { .. } => "accept_tabs",
            Self::AcceptSharedDock { .. } => "accept_shared_dock",
            Self::Commit { .. } => "commit",
            Self::Abort { .. } => "abort",
            Self::Activate { .. } => "activate",
            Self::CancelFreeze { .. } => "cancel_freeze",
        }
    }

    pub fn transfer(&self) -> &ryeos_ui_presentation_contract::transfer::TransferIdentity {
        match self {
            Self::AcceptView { transfer, .. }
            | Self::AcceptTabs { transfer, .. }
            | Self::AcceptSharedDock { transfer, .. }
            | Self::Commit { transfer }
            | Self::Abort { transfer }
            | Self::Activate { transfer }
            | Self::CancelFreeze { transfer } => transfer,
        }
    }

    pub fn actor_seat(&self) -> &ryeos_ui_presentation_contract::transfer::TransferSeat {
        let transfer = self.transfer();
        match self {
            Self::AcceptView { .. }
            | Self::AcceptTabs { .. }
            | Self::AcceptSharedDock { .. }
            | Self::Activate { .. } => &transfer.destination,
            Self::Commit { .. } | Self::Abort { .. } | Self::CancelFreeze { .. } => {
                &transfer.source
            }
        }
    }
}

#[cfg(test)]
mod transfer_contact_tests {
    use super::*;
    use ryeos_ui_presentation_contract::transfer::{TransferIdentity, TransferSeat};

    #[test]
    fn phase_contact_actor_and_flat_wire_action_are_exact() {
        let seat = |id: &str| TransferSeat {
            presentation_id: id.into(),
            thread_id: id.into(),
            chain_root_id: id.into(),
            producer_incarnation: "a".repeat(64),
            revision: 0,
        };
        let transfer = TransferIdentity {
            operation_id: "11111111-1111-4111-8111-111111111111".into(),
            source: seat("22222222-2222-4222-8222-222222222222"),
            destination: seat("33333333-3333-4333-8333-333333333333"),
            export_digest: "b".repeat(64),
        };
        for (request, actor, action) in [
            (
                UiTransferContactRequest::Commit {
                    transfer: transfer.clone(),
                },
                &transfer.source,
                "commit",
            ),
            (
                UiTransferContactRequest::Abort {
                    transfer: transfer.clone(),
                },
                &transfer.source,
                "abort",
            ),
            (
                UiTransferContactRequest::CancelFreeze {
                    transfer: transfer.clone(),
                },
                &transfer.source,
                "cancel_freeze",
            ),
            (
                UiTransferContactRequest::Activate {
                    transfer: transfer.clone(),
                },
                &transfer.destination,
                "activate",
            ),
        ] {
            assert_eq!(request.transfer(), &transfer);
            assert_eq!(request.actor_seat(), actor);
            let value = serde_json::to_value(request).unwrap();
            assert_eq!(value["action"], action);
            assert_eq!(value["transfer"]["operation_id"], transfer.operation_id);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsEffectResult {
    pub id: u64,
    pub ok: bool,
    pub kind: RyeOsEffectResultKind,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<RyeOsUiError>,
}

/// Renderer-neutral failure returned by a platform effect or transport.
///
/// Keep the stable machine code, retryability, outcome certainty and
/// remediation separate from presentation copy. In particular, an unknown
/// mutation outcome is not an ordinary retryable refusal: renderers must
/// preserve the effect coordinate and offer observation/recovery rather than
/// submitting the operation again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RyeOsUiError {
    pub code: String,
    /// Canonical daemon error envelopes call this field `error`; Rust keeps the
    /// less ambiguous `message` name at presentation call sites.
    #[serde(rename = "error")]
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub outcome: RyeOsEffectOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Bounded structured fields retained for exact inspection. Renderers must
    /// treat this as inert data, never executable presentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl RyeOsUiError {
    pub fn definite(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            outcome: RyeOsEffectOutcome::Refused,
            remediation: None,
            details: None,
        }
    }

    pub fn outcome_unknown(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            outcome: RyeOsEffectOutcome::Unknown,
            ..Self::definite(code, message)
        }
    }

    /// Fail closed if an adapter receives a contradictory wire envelope.
    /// Unknown mutation contact is observable/recoverable, never an automatic
    /// retry merely because an upstream transport also said `retryable`.
    pub fn normalized(mut self) -> Self {
        if self.outcome == RyeOsEffectOutcome::Unknown {
            self.retryable = false;
        }
        self
    }
}

impl From<String> for RyeOsUiError {
    fn from(message: String) -> Self {
        Self::definite("platform_effect_failed", message)
    }
}

impl From<&str> for RyeOsUiError {
    fn from(message: &str) -> Self {
        Self::from(message.to_string())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsEffectOutcome {
    #[default]
    Refused,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RyeOsEffectResultKind {
    BindingInvoked,
    SourceData,
    BrowserOnly,
    TransferDestinationObserved,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiTransferDestinationObservationResult {
    /// Fresh adapter CSPRNG UUID, only an operation uniqueness coordinate.
    pub operation_id: String,
    pub observation: super::binding::UiPresentationProducerObservation,
}

/// Whether a generic invocation launches/continues a conversation or is a
/// discrete service/command intent. Set at the emit site (each site knows its
/// own intent); the result handler branches on this rather than sniffing the
/// target ref — the structural facts (`route_seq`/`ratchet`) are ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvokeIntent {
    /// Launches or continues a conversation (the routed foot input); the result
    /// runs the delivery/ratchet tower.
    Launch,
    /// A discrete service or command intent (row-management affordances); the
    /// result refreshes the affected surface and preserves the input.
    Service,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_error_uses_canonical_wire_field_and_unknown_is_never_retryable() {
        let mut error = RyeOsUiError::outcome_unknown("contact_unknown", "observe first");
        error.retryable = true;
        let value = serde_json::to_value(error.normalized()).unwrap();
        assert_eq!(value["error"], "observe first");
        assert!(value.get("message").is_none());
        assert_eq!(value["outcome"], "unknown");
        assert_eq!(value["retryable"], false);
    }
}
