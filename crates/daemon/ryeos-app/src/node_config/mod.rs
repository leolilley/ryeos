//! Node-config: daemon-consumed control-plane configuration items.
//!
//! `kind: node` items are signed YAML files at `.ai/node/<section>/...`.
//! The first path segment under `.ai/node` selects the section handler; the
//! section is loader-owned structure, not YAML payload.
//!
//! Section directories (routes, commands) support recursive subfolders:
//!
//!   .ai/node/routes/ui/ryeos-ui/dimension-get.yaml
//!   .ai/node/routes/ui/ryeos-ui/items/list.yaml
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

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::node_config::sections::command::CommandRecord;
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
pub enum NodeConfigSourceScope {
    /// Only `app_root`.
    /// Used by the `bundles` section so bundles can't self-register.
    AppRootOnly,
    /// `app_root` + all effective bundle roots.
    /// Used by sections like `routes` and `commands` that bundles can contribute to.
    AppRootAndBundleRoots,
}

/// When a registered section participates in node-config loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionLoadPhase {
    /// Establishes the exact installed bundle generation used by every later
    /// scan. This is the first bootstrap boundary.
    BundleBootstrap,
    /// Loaded by the generic full-section pass.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionTraversal {
    Flat,
    Recursive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionSignerPolicy {
    Trusted,
    CurrentNode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionCardinality {
    Any,
    AtLeastOne,
    AtMostOne,
    ExactlyOne,
}

/// Closed loading contract owned by the section compiler rather than the
/// filesystem walker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionLoadSpec {
    pub phase: SectionLoadPhase,
    pub traversal: SectionTraversal,
    pub signer: SectionSignerPolicy,
    pub cardinality: SectionCardinality,
}

/// A single parsed bundle registration record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// All loaded command definitions.
    pub commands: Vec<CommandRecord>,
}

impl NodeConfigSnapshot {}

/// Erased typed contribution returned by one registered node-config compiler.
///
/// Each section owns admission of its concrete record into the typed snapshot.
/// The generic loader verifies the registered section identity and invokes
/// this interface; it never enumerates record variants or branches on names.
pub(crate) trait CompiledNodeConfigItem: std::fmt::Debug + Send + Sync {
    fn section_name(&self) -> &'static str;

    fn admit(
        self: Box<Self>,
        target: &mut loader::NodeConfigSnapshotBuilder,
        admission: &loader::NodeConfigAdmission,
    ) -> anyhow::Result<()>;
}

/// Trait implemented by each node-config section handler.
pub(crate) trait NodeConfigSection: Send + Sync {
    /// Canonical first path segment and registry key for this compiler.
    fn name(&self) -> &'static str;

    /// Which sources this section scans.
    fn source_scope(&self) -> NodeConfigSourceScope;

    /// Complete loader-owned mechanics for this section.
    fn load_spec(&self) -> SectionLoadSpec;

    /// Parse a verified YAML body into a section record.
    fn parse(
        &self,
        ctx: &NodeItemContext,
        body: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn CompiledNodeConfigItem>>;
}

/// Registry of all known sections, keyed by section name.
pub struct NodeConfigTable {
    sections: Vec<Box<dyn NodeConfigSection>>,
}

impl NodeConfigTable {
    /// Build the section table with all known sections.
    pub fn new() -> Self {
        Self::from_sections(vec![
            Box::new(sections::bundle::BundleSection),
            Box::new(sections::command::CommandSection),
            Box::new(sections::route::RouteSection),
        ])
        .expect("built-in node-config section table is valid")
    }

    fn from_sections(sections: Vec<Box<dyn NodeConfigSection>>) -> anyhow::Result<Self> {
        let mut names = BTreeSet::new();
        for section in &sections {
            let name = section.name();
            validate_section_name(name)?;
            if !names.insert(name) {
                anyhow::bail!("duplicate node-config section `{name}`");
            }
        }
        Ok(Self { sections })
    }

    pub(crate) fn sections(&self) -> impl Iterator<Item = &dyn NodeConfigSection> + '_ {
        self.sections.iter().map(|section| section.as_ref())
    }
}

impl Default for NodeConfigTable {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_section_name(name: &str) -> anyhow::Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !name.ends_with('_')
        && !name.contains("__");
    if !valid {
        anyhow::bail!("invalid node-config section name `{name}`");
    }
    Ok(())
}
