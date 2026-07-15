//! `bundles` section handler for node-config.
//!
//! Each record registers one installed bundle:
//! ```yaml
//! kind: node
//! path: <absolute path to bundle root>
//! ```

use anyhow::{bail, Context};
use serde::Deserialize;
use serde_json::Value;

use crate::node_config::{
    BundleRecord, NodeConfigSection, NodeItemContext, SectionRecord, SectionSourcePolicy,
};

/// Section handler for `bundles` node-config items.
pub struct BundleSection;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBundleRecord {
    kind: String,
    path: std::path::PathBuf,
    #[serde(default)]
    command_registration_caps: Vec<String>,
}

impl NodeConfigSection for BundleSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        // Bundles cannot self-register — only the app root.
        SectionSourcePolicy::SystemAndState
    }

    fn parse(&self, ctx: &NodeItemContext, body: &Value) -> anyhow::Result<Box<dyn SectionRecord>> {
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

        let record = BundleRecord {
            name: ctx.id.clone(),
            path: raw.path,
            command_registration_caps: raw.command_registration_caps,
            source_file: std::path::PathBuf::new(),
        };
        Ok(Box::new(record))
    }
}
