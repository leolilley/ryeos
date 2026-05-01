pub mod canonical_ref;
pub mod binary_resolver;
pub mod composers;
pub mod contracts;
pub mod delegation;
pub mod dispatch;
pub mod engine;
pub mod error;
pub mod executor_resolution;
pub mod handlers;
pub mod inventory;
pub mod item_resolution;
pub mod kind_registry;
pub mod lifecycle;
pub mod parsers;
pub mod boot_validation;
pub mod plan_builder;
pub mod resolution;
pub mod roots;
pub mod runtime;
pub mod runtime_registry;
pub mod scope;
pub mod trust;

pub mod launch_envelope_types;
pub mod protocol_vocabulary;
pub mod subprocess_spec;

#[doc(hidden)]
pub mod test_support;

/// The working directory name used in all three spaces.
/// Every space follows: `base_path / AI_DIR / {kind_directory} / {item_id}`
pub const AI_DIR: &str = ".ai";

/// Path under `AI_DIR` where trusted key documents live.
pub const TRUST_KEYS_DIR: &str = "config/keys/trusted";

/// Path under `AI_DIR` where kind schema YAML files live.
pub const KIND_SCHEMAS_DIR: &str = "node/engine/kinds";
