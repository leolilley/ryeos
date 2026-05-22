//! ryeos-tui-core — Platform-neutral TUI model, layout, views, text surfaces,
//! scene primitives, animation, reducers, and frame construction.
//!
//! This crate must not depend on crossterm, tokio, hyper, ryeos-cli,
//! wasm-bindgen, or any platform-specific I/O.

pub mod animation;
pub mod effects;
pub mod frame;
pub mod ids;
pub mod input;
pub mod layout;
pub mod model;
pub mod scene;
pub mod store;
pub mod text_surface;
pub mod theme;
pub mod update;
pub mod views;
pub mod workspace;
