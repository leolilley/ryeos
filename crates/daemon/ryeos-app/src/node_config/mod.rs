//! Node-config: daemon-consumed control-plane configuration items.
//!
//! `kind: node` items are signed YAML files at `.ai/node/<section>/...`.
//! The first path segment under `.ai/node` selects the section handler; the
//! section is loader-owned structure, not YAML payload.
//!
//! Section directories (routes, commands) support recursive subfolders:
//!
//!   .ai/node/routes/ui/studio/dimension-get.yaml
//!   .ai/node/routes/ui/studio/items/list.yaml
//!   .ai/node/commands/web.yaml
//!
//! The `bundles` section remains flat (no subdirectories).
//!
//! The daemon loads node-config at startup in two phases:
//! - **Phase 1 (bootstrap):** load only the `bundles` section from
//!   `app_root` to determine effective bundle roots.
//! - **Phase 2 (full pass):** build the engine with effective roots, then
//!   scan all sections from all sources (recursive for routes/commands).
//!
//! Trust model: signed-required, fail-closed. Unsigned, tampered, or
//! untrusted-signer items are startup errors.

pub mod loader;
pub mod sections;
pub mod writer;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::node_config::sections::command::CommandRecord;
use crate::node_config::sections::command_registration::CommandRegistrationPolicyRecord;
use crate::node_config::sections::hosted_node::HostedNodePolicyRecord;
use crate::route_raw::RawRouteSpec;

/// Loader-derived structural context for a node-config item.
#[derive(Debug, Clone)]
pub struct NodeItemContext {
    /// Section name selected by `.ai/node/<section>/...`.
    pub section: String,
    /// Relative item id below the section root, without extension.
    pub id: String,
    /// Filename stem.
    pub stem: String,
    /// Path relative to the section root, including extension.
    pub rel_path: PathBuf,
    /// Absolute source file path.
    pub source_file: PathBuf,
    /// Trusted signer fingerprint from the verified signature.
    pub signer_fingerprint: String,
}

/// Which sources a section scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionSourcePolicy {
    /// Only `app_root`.
    /// Used by the `bundles` section so bundles can't self-register.
    SystemAndState,
    /// `app_root` + all effective bundle roots.
    /// Used by sections like `routes` and `commands` that bundles can contribute to.
    EffectiveBundleRootsAndState,
}

/// A single parsed bundle registration record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleRecord {
    /// Bundle name (filename without extension).
    pub name: String,
    /// Absolute, canonicalized path to the bundle root directory.
    pub path: PathBuf,
    /// Node-owned command registration grants for commands loaded from this bundle.
    #[serde(default)]
    pub command_registration_caps: Vec<String>,
    /// Path to the `.yaml` file that declared this record.
    pub source_file: PathBuf,
}

/// Immutable snapshot of all node-config sections loaded at startup.
#[derive(Debug, Clone)]
pub struct NodeConfigSnapshot {
    /// All registered bundle records, in load order.
    pub bundles: Vec<BundleRecord>,
    /// All loaded route specifications, in load order.
    pub routes: Vec<RawRouteSpec>,
    /// All loaded command definitions.
    pub commands: Vec<CommandRecord>,
    /// Hosted node operator policies loaded from installed bundle/runtime state dirs.
    pub hosted_node_policies: Vec<HostedNodePolicyRecord>,
    /// Effective command registration admission policy.
    pub command_registration_policy: CommandRegistrationPolicyRecord,
}

impl NodeConfigSnapshot {}

/// Trait implemented by each node-config section handler.
pub trait NodeConfigSection: Send + Sync {
    /// Which sources this section scans.
    fn source_policy(&self) -> SectionSourcePolicy;

    /// Parse a verified YAML body into a section record.
    fn parse(
        &self,
        ctx: &NodeItemContext,
        body: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn SectionRecord>>;
}

/// A parsed section record (type-erased).
pub trait SectionRecord: Send + Sync + std::fmt::Debug {
    /// Downcast to `Any` for concrete type recovery.
    fn as_any(&self) -> &dyn std::any::Any;
}

impl SectionRecord for BundleRecord {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Registry of all known sections, keyed by section name.
pub struct SectionTable {
    sections: HashMap<&'static str, Box<dyn NodeConfigSection>>,
}

impl SectionTable {
    /// Build the section table with all known sections.
    pub fn new() -> Self {
        let mut sections: HashMap<&'static str, Box<dyn NodeConfigSection>> = HashMap::new();
        sections.insert("bundles", Box::new(sections::bundle::BundleSection));
        sections.insert("commands", Box::new(sections::command::CommandSection));
        sections.insert(
            "command_registration",
            Box::new(sections::command_registration::CommandRegistrationSection),
        );
        sections.insert(
            "hosted",
            Box::new(sections::hosted_node::HostedNodePolicySection),
        );
        sections.insert("routes", Box::new(sections::route::RouteSection));
        Self { sections }
    }

    /// Get a section handler by name.
    pub fn get(&self, name: &str) -> Option<&dyn NodeConfigSection> {
        self.sections.get(name).map(|s| s.as_ref())
    }

    /// Iterate over all registered section names.
    pub fn section_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.sections.keys().copied()
    }
}

impl Default for SectionTable {
    fn default() -> Self {
        Self::new()
    }
}
