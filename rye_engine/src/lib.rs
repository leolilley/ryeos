pub mod canonical_ref;
pub mod contracts;
pub mod delegation;
pub mod dispatch;
pub mod engine;
pub mod error;
pub mod executor_registry;
pub mod kind_registry;
pub mod lifecycle;
pub mod metadata;
pub mod plan_builder;
pub mod resolution;
pub mod scope;
pub mod trust;

/// The working directory name used in all three spaces.
/// Every space follows: `base_path / AI_DIR / {kind_directory} / {item_id}`
pub const AI_DIR: &str = ".ai";
