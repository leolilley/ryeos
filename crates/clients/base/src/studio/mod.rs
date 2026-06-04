//! RyeOS Studio core.
//!
//! Studio is the WASM-led browser UI model. Rust owns RyeOS product state,
//! reducers, semantic view models, scene models, and platform effects. Browser
//! JavaScript owns adapters for fetch/EventSource/DOM/Three.js and returns
//! events/effect results to this core.

pub mod dto;
pub mod effect;
pub mod event;
pub mod model;
pub mod reducer;
pub mod scene_model;
pub mod view_model;

pub use effect::{StudioEffect, StudioEffectKind, StudioEffectResult, StudioEffectResultKind};
pub use event::{StudioAction, StudioEvent, StudioFilterField, StudioUiEvent};
pub use model::{BrowserSession, BrowserViewport, StudioCore, StudioEnvelope};
pub use scene_model::StudioSceneModel;
pub use view_model::StudioViewModel;
