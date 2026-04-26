//! Node-config writer: signed YAML mutations.
//!
//! All daemon-issued `kind: node` item writes go through here.
//! Uses atomic writes (tmp + fsync + rename) for crash safety.

use std::path::Path;

use anyhow::{Context, Result};

use crate::identity::NodeIdentity;
use crate::io::atomic;

/// Write a signed `kind: node` item.
///
/// Builds a YAML body with the `section` field prepended, signs it with
/// the node's identity, and writes atomically.
///
/// Output path: `<base_dir>/<section>/<name>.yaml`.
///
/// # Trust continuity
///
/// The daemon's identity MUST be in the trust store the daemon will use
/// on next boot. Otherwise the daemon's own writes won't verify.
pub fn write_signed_node_item(
    base_dir: &Path,
    section: &str,
    name: &str,
    body: &serde_json::Value,
    identity: &NodeIdentity,
) -> Result<std::path::PathBuf> {
    // Build canonical YAML: section first, then remaining body fields
    let mut yaml_map = serde_yaml::Mapping::new();
    yaml_map.insert(
        serde_yaml::Value::String("section".into()),
        serde_yaml::Value::String(section.into()),
    );

    // Merge body fields (skip "section" if present in body — ours wins)
    if let Some(map) = body.as_object() {
        for (k, v) in map {
            if k == "section" {
                continue;
            }
            yaml_map.insert(
                serde_yaml::Value::String(k.clone()),
                serde_yaml::to_value(v)
                    .context("failed to serialize body field to YAML")?,
            );
        }
    }

    let yaml_str = serde_yaml::to_string(&yaml_map)
        .context("failed to serialize node config body to YAML")?;

    // Sign with node identity
    let signed = lillux::signature::sign_content(
        &yaml_str,
        identity.signing_key(),
        "#",
        None,
    );

    // Compute output path
    let section_dir = base_dir.join(section);
    let output_path = section_dir.join(format!("{}.yaml", name));

    // Atomic write
    atomic::atomic_write(&output_path, signed.as_bytes())
        .with_context(|| format!("failed to write node config item {}", output_path.display()))?;

    Ok(output_path)
}
