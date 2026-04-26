//! `bundle` section handler for node-config.
//!
//! Each record registers one installed bundle:
//! ```yaml
//! section: bundle
//! path: <absolute path to bundle root>
//! ```

use anyhow::{bail, Context};
use serde_json::Value;

use crate::node_config::{
    BundleRecord, NodeConfigSection, SectionRecord, SectionSourcePolicy,
};

/// Section handler for `bundle` node-config items.
pub struct BundleSection;

impl NodeConfigSection for BundleSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        // Bundles cannot self-register — only system_data_dir + state_dir.
        SectionSourcePolicy::SystemAndState
    }

    fn parse(&self, name: &str, body: &Value) -> anyhow::Result<Box<dyn SectionRecord>> {
        let path = body
            .get("path")
            .and_then(|v| v.as_str())
            .context("bundle record missing 'path' field")?;

        let path_buf = std::path::PathBuf::from(path);
        if !path_buf.is_absolute() {
            bail!(
                "bundle '{}' path must be absolute, got: {}",
                name,
                path
            );
        }

        let record = BundleRecord {
            name: name.to_string(),
            path: path_buf,
            source_file: std::path::PathBuf::new(),
        };
        Ok(Box::new(record))
    }
}
