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
pub mod keymap;
pub mod model;
pub mod reducer;
pub mod scene_model;
pub mod seat;
pub mod timeline;
pub mod tokenize;
pub mod view_model;

pub use content::{ProjectedRecord, SourceBinding, ViewBinding};
pub use effect::{RyeOsEffect, RyeOsEffectKind, RyeOsEffectResult, RyeOsEffectResultKind};
pub use event::{RyeOsAction, RyeOsEvent, RyeOsFilterField, RyeOsUiEvent};
pub use keymap::{
    ryeos_key_command, RyeOsKey, RyeOsKeyCommand, RyeOsKeyContext, RyeOsKeyEvent, RyeOsKeyModifiers,
};
pub use model::{BrowserSession, BrowserViewport, RyeOsCore, RyeOsEnvelope};
pub use scene_model::RyeOsSceneModel;
pub use seat::{InputRoute, InvokeTemplate, SeatEvent, SeatEventKind, SeatFold, SeatLog};
pub use timeline::{RyeOsLiveDelta, RyeOsTimelineEntryVm};
pub use tokenize::{classify_line, InputLine, TokenizeError};
pub use view_model::RyeOsViewModel;
