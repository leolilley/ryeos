//! RyeOS UI core.
//!
//! RyeOS is the WASM-led browser UI model. Rust owns RyeOS product state,
//! reducers, semantic view models, scene models, and platform effects. Browser
//! JavaScript owns adapters for fetch/EventSource/DOM/Three.js and returns
//! events/effect results to this core.

pub mod admission_operation;
pub mod attachment;
pub mod binding;
pub mod binding_context;
pub mod cold_replay;
pub mod composition_completion;
pub mod composition_delta;
pub mod composition_identity;
pub mod composition_lens;
pub mod composition_operation;
pub mod composition_replay;
#[cfg(test)]
mod composition_subject_inputs_tests;
pub mod composition_subjects;
pub mod content;
pub mod dto;
pub mod effect;
pub mod event;
pub mod field;
pub mod keymap;
pub mod layout_preferences;
pub mod local_projection;
pub mod model;
pub mod presentation_initialization;
pub mod presentation_state;
pub mod reducer;
mod reproof;
pub mod scene_model;
pub mod seat;
pub mod seat_digest;
pub mod source_key;
#[cfg(test)]
mod surface_discovery_tests;
pub mod timeline;
pub mod tokenize;
pub mod transfer_cancellation_proof_frame;
mod transfer_capture;
pub mod transfer_export;
pub mod transfer_freeze;
pub mod transfer_projection;
pub mod transfer_proof_frame;
pub mod transfer_reservation;
pub mod view_model;

pub use admission_operation::{
    UiCompletedPresentationOperation, UiPreparedAdmissionOperation,
    UiPreparedPresentationOperation, UiPreparedPresentationOperationBody,
    UiPresentationOperationKind,
};
pub use binding::{
    UiBindingAttachment, UiBindingAttachmentCoordinate, UiBindingCoordinate, UiBindingPayload,
    UiBindingRequest, UiBindingRequestBounds, UiBindingRequestError, UiBindingRouteContext,
    UiBindingSubject, UiEffectivePosture, UiInvocationRequest, UiObservedPresentationProducer,
    UiPresentationMembership, UiPresentationProducer, UiPresentationProducerObservation,
    UiSeatControlPredecessor, UiSeatControlRequest, UiSeatProducerClaim,
};
pub use content::{ProjectedRecord, SourceBinding, ViewBinding};
pub use effect::{
    RyeOsEffect, RyeOsEffectKind, RyeOsEffectOutcome, RyeOsEffectResult, RyeOsEffectResultKind,
    RyeOsUiError,
};
pub use event::{
    RyeOsEvent, RyeOsFilterField, RyeOsTransportChannel, RyeOsTransportFreshness, RyeOsUiEvent,
    RyeOsUiIntent,
};
pub use field::{RyeOsFieldVm, project_field};
pub use keymap::{
    RyeOsKey, RyeOsKeyCommand, RyeOsKeyContext, RyeOsKeyEvent, RyeOsKeyModifiers, ryeos_key_command,
};
pub use model::{
    BrowserSession, BrowserViewport, RyeOsCore, RyeOsEnvelope, RyeOsTransportChannelState,
    RyeOsTransportState,
};
pub use reproof::AuthenticatedReproofGuard;
pub use scene_model::RyeOsSceneModel;
pub use seat::{InputRoute, InvokeTemplate, SeatEvent, SeatEventKind, SeatFold, SeatLog};
pub use seat_digest::{SeatPayloadDigestError, seat_payload_digest};
pub use source_key::{RyeOsSourceChannel, RyeOsSourceInstanceKey};
pub use timeline::{RyeOsLiveDelta, RyeOsTimelineEntryVm};
pub use tokenize::{InputLine, TokenizeError, classify_line};
pub use view_model::RyeOsViewModel;
