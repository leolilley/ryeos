//! ryeos-tui-core — Platform-neutral TUI model, layout, views, text surfaces,
//! scene primitives, animation, reducers, and frame construction.
//!
//! This crate must not depend on crossterm, tokio, hyper, ryeos-cli,
//! wasm-bindgen, or any platform-specific I/O.

pub mod animation;
pub mod command_registry;
pub mod commands;
pub mod effects;
pub mod effective_surface;
pub mod frame;
pub mod ids;
pub mod input;
pub mod layout;
pub mod math3d;
pub mod model;
pub mod scene;
pub mod scene_config;
pub mod store;
pub mod substrate;
pub mod surface;
pub mod text_surface;
pub mod theme;
pub mod update;
pub mod views;
pub mod widgets;
pub mod workspace;
