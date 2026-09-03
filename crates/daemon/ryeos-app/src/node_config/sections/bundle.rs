//! `bundles` section handler for node-config.
//!
//! Each record registers one installed bundle:
//! ```yaml
//! kind: node
//! path: <absolute path to bundle root>
//! ```

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::node_config::{
    BundleRecord, CompiledNodeConfigItem, NodeConfigSection, NodeConfigSourceScope,
    NodeItemContext, SectionCardinality, SectionLoadPhase, SectionLoadSpec, SectionSignerPolicy,
    SectionTraversal,
};

pub const SECTION_NAME: &str = "bundles";

/// Section handler for `bundles` node-config items.
pub struct BundleSection;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBundleRecord {
    kind: String,
    path: std::path::PathBuf,
}

impl BundleSection {
    pub(crate) fn parse_bundle(
        &self,
        ctx: &NodeItemContext,
        body: &Value,
    ) -> anyhow::Result<BundleRecord> {
        let raw: RawBundleRecord =
            serde_json::from_value(body.clone()).context("failed to parse bundle record")?;
        if raw.kind != "node" {
            bail!(
                "bundle '{}' declares kind {:?}, expected kind 'node'",
                ctx.id,
                raw.kind
            );
        }

        if !raw.path.is_absolute() {
            bail!(
                "bundle '{}' path must be absolute, got: {}",
                ctx.id,
                raw.path.display()
            );
        }

        Ok(BundleRecord {
            name: ctx.id.clone(),
            path: raw.path,
            source_file: std::path::PathBuf::new(),
        })
    }
}

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
        Ok(Box::new(self.parse_bundle(ctx, body)?))
    }
}
