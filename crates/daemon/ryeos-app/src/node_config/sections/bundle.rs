//! `bundles` section handler for node-config.
//!
//! Each record registers one installed bundle:
//! ```yaml
//! section: bundles
//! path: <absolute path to bundle root>
//! ```

use anyhow::{bail, Context};
use serde_json::Value;

use crate::node_config::{BundleRecord, NodeConfigSection, SectionRecord, SectionSourcePolicy};

/// Section handler for `bundles` node-config items.
pub struct BundleSection;

impl NodeConfigSection for BundleSection {
    fn source_policy(&self) -> SectionSourcePolicy {
        // Bundles cannot self-register — only the system space root.
        SectionSourcePolicy::SystemAndState
    }

    fn parse(&self, name: &str, body: &Value) -> anyhow::Result<Box<dyn SectionRecord>> {
        let path = body
            .get("path")
            .and_then(|v| v.as_str())
            .context("bundle record missing 'path' field")?;

        let path_buf = std::path::PathBuf::from(path);
        if !path_buf.is_absolute() {
            bail!("bundle '{}' path must be absolute, got: {}", name, path);
        }

        let command_registration_caps = body
            .get("command_registration_caps")
            .cloned()
            .map(serde_json::from_value::<Vec<String>>)
            .transpose()
            .context("bundle record has invalid 'command_registration_caps' field")?
            .unwrap_or_default();

        let record = BundleRecord {
            name: name.to_string(),
            path: path_buf,
            command_registration_caps,
            source_file: std::path::PathBuf::new(),
        };
        Ok(Box::new(record))
    }
}
