//! `bundles` section handler for node-config.
//!
//! Each record registers one installed bundle:
//! ```yaml
//! kind: node
//! path: <absolute path to bundle root>
//! ```

use anyhow::bail;
use serde_json::Value;

use crate::node_config::{
    BundleRecord, CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope,
    NodeItemContext, SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy,
    SectionTraversal,
};

pub const SECTION_NAME: &str = "bundles";

/// Section handler for `bundles` node-config items.
pub struct BundleSection;

impl CompiledNodeConfigItem for BundleRecord {
    fn section_name(&self) -> &'static str {
        SECTION_NAME
    }

    fn admit(
        self: Box<Self>,
        _target: &mut crate::node_config::loader::NodeConfigSnapshotBuilder,
        _admission: &crate::node_config::loader::NodeConfigAdmission,
    ) -> anyhow::Result<()> {
        bail!("bundle records may only enter through phase-one bootstrap")
    }
}

impl NodeConfigSection for BundleSection {
    fn name(&self) -> &'static str {
        SECTION_NAME
    }

    fn source_scope(&self) -> NodeConfigSourceScope {
        // Bundles cannot self-register — only the app root.
        NodeConfigSourceScope::AppRootOnly
    }

    fn load_spec(&self) -> SectionLoadSpec {
        SectionLoadSpec {
            phase: SectionLoadPhase::BundleBootstrap,
            traversal: SectionTraversal::Flat,
            signer: SectionSignerPolicy::Trusted,
            cardinality: SectionCardinality::AtLeastOne,
        }
    }

    fn parse(
        &self,
        ctx: &NodeItemContext,
        body: &Value,
    ) -> anyhow::Result<Box<dyn CompiledNodeConfigItem>> {
        Ok(Box::new(
            ryeos_node_runtime::bundle_bootstrap::parse_bundle(ctx, body)?,
        ))
    }
}
