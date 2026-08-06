//! RyeOS UI core.
//!
//! RyeOS is the WASM-led browser UI model. Rust owns RyeOS product state,
//! reducers, semantic view models, scene models, and platform effects. Browser
//! JavaScript owns adapters for fetch/EventSource/DOM/Three.js and returns
//! events/effect results to this core.

pub mod content;
pub mod dto;
pub mod effect;
pub mod event;
pub mod field;
pub mod keymap;
pub mod model;
pub mod reducer;
pub mod scene_model;
pub mod seat;
pub mod source_key;
pub mod timeline;
pub mod tokenize;
pub mod view_model;

pub use content::{ProjectedRecord, SourceBinding, ViewBinding};
pub use effect::{RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
pub use event::{RyeOsEvent, RyeOsFilterField, RyeOsUiEvent, RyeOsUiIntent};
pub use field::{RyeOsFieldVm, project_field};
pub use keymap::{
    RyeOsKey, RyeOsKeyCommand, RyeOsKeyContext, RyeOsKeyEvent, RyeOsKeyModifiers, ryeos_key_command,
};
pub use model::{BrowserSession, BrowserViewport, RyeOsCore, RyeOsEnvelope};
pub use scene_model::RyeOsSceneModel;
pub use seat::{InputRoute, InvokeTemplate, SeatEvent, SeatEventKind, SeatFold, SeatLog};
pub use source_key::{RyeOsSourceChannel, RyeOsSourceInstanceKey};
pub use timeline::{RyeOsLiveDelta, RyeOsTimelineEntryVm};
pub use tokenize::{InputLine, TokenizeError, classify_line};
pub use view_model::RyeOsViewModel;
