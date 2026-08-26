pub mod authority;
pub mod boot_validation;
pub mod composers;
pub mod config_loading;
pub mod contracts;
pub mod dispatch;
pub mod effective_validators;
pub mod engine;
pub mod error;
pub mod execution_workspace;
pub mod handlers;
pub mod history_policy;
pub mod hooks;
pub mod identity;
pub mod item_resolution;
pub mod launch;
pub mod lifecycle;
pub mod method_call;
pub mod method_wire;
mod parser_overlay_cache;
pub mod parsers;
pub mod project_content;
pub mod protocol_vocabulary;
pub mod registry;
pub mod roots;
pub mod runtime;
pub mod scope;
pub mod source_closure;
pub mod structured_session_profile;

// The grouped modules above nest this crate physically; these re-exports pin
// every pre-grouping public path, so no caller inside or outside the crate
// observes the move.
pub use authority::{capability_cover, isolation, protocols, trust};
pub use identity::{
    canonical_ref, effective_program, external_content, external_realization, resolution,
};
pub use launch::{
    execution_policy, launch_config, launch_envelope_types, launch_preparers, plan_builder,
    subprocess_spec,
};
pub use registry::{
    binary_resolver, executor_resolution, inventory, kind_registry, runtime_registry,
};

#[doc(hidden)]
pub mod test_support;

/// The working directory name used in all three spaces.
/// Every space follows: `base_path / AI_DIR / {kind_directory} / {item_id}`
pub const AI_DIR: &str = ".ai";

/// Path under `AI_DIR` where trusted key documents live.
pub const TRUST_KEYS_DIR: &str = "config/keys/trusted";

/// Path under `AI_DIR` where kind schema YAML files live.
pub const KIND_SCHEMAS_DIR: &str = "node/engine/kinds";
