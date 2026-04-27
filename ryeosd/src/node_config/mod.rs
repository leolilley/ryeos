//! Node-config: daemon-consumed control-plane configuration items.
//!
//! `kind: node` items are signed YAML files at `.ai/node/<section>/<name>.yaml`.
//! Each declares `section: <name>` which must match its parent directory.
//!
//! The daemon loads node-config at startup in two phases:
//! - **Phase 1 (bootstrap):** load only the `bundles` section from
//!   `system_data_dir` + `state_dir` to determine effective bundle roots.
//! - **Phase 2 (full pass):** build the engine with effective roots, then
//!   scan all sections from all sources.
//!
//! Trust model: signed-required, fail-closed. Unsigned, tampered, or
//! untrusted-signer items are startup errors.

pub mod loader;
pub mod sections;
pub mod writer;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::routes::raw::RawRouteSpec;

/// Which sources a section scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionSourcePolicy {
    /// Only `system_data_dir` + `state_dir`.
    /// Used by the `bundles` section so bundles can't self-register.
    SystemAndState,
    /// `state_dir` + all effective bundle roots.
    /// Used by sections like `routes` that bundles can contribute to.
    EffectiveBundleRootsAndState,
}

/// A single parsed bundle registration record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleRecord {
    /// Bundle name (filename without extension).
    pub name: String,
    /// Absolute, canonicalized path to the bundle root directory.
    pub path: PathBuf,
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
}

impl NodeConfigSnapshot {
}

/// Trait implemented by each node-config section handler.
pub trait NodeConfigSection: Send + Sync {
    /// Which sources this section scans.
    fn source_policy(&self) -> SectionSourcePolicy;

    /// Parse a verified YAML body into a section record.
    fn parse(&self, name: &str, body: &serde_json::Value) -> anyhow::Result<Box<dyn SectionRecord>>;
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
        sections.insert(
            "bundles",
            Box::new(sections::bundle::BundleSection),
        );
        sections.insert(
            "routes",
            Box::new(sections::route::RouteSection),
        );
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
