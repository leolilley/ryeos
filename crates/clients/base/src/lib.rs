//! ryeos-client-base — Platform-neutral TUI model, layout, views, text surfaces,
//! scene primitives, animation, reducers, and frame construction.
//!
//! This crate must not depend on crossterm, tokio, hyper, ryeos-cli,
//! wasm-bindgen, or any platform-specific I/O.

pub mod atlas;
pub mod effective_surface;
pub mod ids;
pub mod layout;
pub mod math3d;
pub mod radial_tree;
pub mod scene;
pub mod scene_config;
pub mod surface;
pub mod text_surface;
pub mod theme;
pub mod ui;
pub mod workspace;
