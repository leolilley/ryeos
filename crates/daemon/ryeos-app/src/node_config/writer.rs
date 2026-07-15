//! Node-config writer: signed YAML mutations.
//!
//! All daemon-issued `kind: node` item writes go through here.
//! Uses atomic writes (tmp + fsync + rename) for crash safety.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::identity::NodeIdentity;
/// Render the exact signed bytes for one path-owned `kind: node` item without
/// touching the filesystem. Callers that already hold a pinned namespace can
/// publish these bytes with an inode-bound conditional replacement.
pub fn render_signed_node_item(
    section: &str,
    name: &str,
    body: &serde_json::Value,
    identity: &NodeIdentity,
) -> Result<Vec<u8>> {
    // Build canonical YAML from body fields. Structural node-config metadata is
    // path-derived and must never be serialized.
    let mut yaml_map = serde_yaml::Mapping::new();

    if let Some(map) = body.as_object() {
        for (k, v) in map {
            if k == "section" || k == "category" {
                bail!(
                    "node config writer refusing legacy structural field '{}' for section '{}' item '{}'",
                    k,
                    section,
                    name
                );
            }
            if section == "commands" && k == "name" {
                bail!(
                    "node config writer refusing command structural field 'name' for item '{}'",
                    name
                );
            }
            yaml_map.insert(
                serde_yaml::Value::String(k.clone()),
                serde_yaml::to_value(v).context("failed to serialize body field to YAML")?,
            );
        }
    }

    let yaml_str =
        serde_yaml::to_string(&yaml_map).context("failed to serialize node config body to YAML")?;

    // Sign with node identity
    Ok(lillux::signature::sign_content(&yaml_str, identity.signing_key(), "#", None).into_bytes())
}

/// Write a signed `kind: node` item.
///
/// Serializes the provided YAML body as-is, signs it with the node's identity,
/// and publishes atomically relative to a pinned, directory-locked section.
/// Section identity comes from the output path, not the body.
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
    let bytes = render_signed_node_item(section, name, body, identity)?;

    let base_directory = lillux::PinnedDirectory::open_or_create(base_dir)
        .context("establish no-follow node config root")?;
    let section_directory = base_directory
        .open_or_create_child(std::ffi::OsStr::new(section), 0o777)
        .with_context(|| format!("establish node config section {section}"))?;
    let _directory_lock = section_directory.lock_exclusive()?;
    let filename = format!("{name}.yaml");
    let filename = std::ffi::OsStr::new(&filename);
    let expected = section_directory.open_regular(filename, false)?;
    section_directory
        .atomic_write_if_same(filename, expected.as_ref(), &bytes, 0o600)
        .with_context(|| {
            format!(
                "failed to write node config item {}",
                section_directory.path().join(filename).display()
            )
        })?;

    Ok(section_directory.path().join(filename))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lillux::crypto::EncodePrivateKey;
    use rand::rngs::OsRng;

    fn identity() -> NodeIdentity {
        let tmp = tempfile::tempdir().unwrap();
        let key_path = tmp.path().join("identity/private_key.pem");
        std::fs::create_dir_all(key_path.parent().unwrap()).unwrap();
        let key = lillux::crypto::SigningKey::generate(&mut OsRng);
        std::fs::write(
            &key_path,
            key.to_pkcs8_pem(Default::default()).unwrap().as_bytes(),
        )
        .unwrap();
        NodeIdentity::load(&key_path).unwrap()
    }

    #[test]
    fn rejects_legacy_section_field() {
        let tmp = tempfile::tempdir().unwrap();
        let err = write_signed_node_item(
            tmp.path(),
            "schedules",
            "demo",
            &serde_json::json!({ "section": "schedules", "schedule_id": "demo" }),
            &identity(),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("legacy structural field 'section'"),
            "got: {err:#}"
        );
    }

    #[test]
    fn rejects_command_name_field() {
        let tmp = tempfile::tempdir().unwrap();
        let err = write_signed_node_item(
            tmp.path(),
            "commands",
            "demo",
            &serde_json::json!({ "name": "demo", "tokens": ["demo"] }),
            &identity(),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("command structural field 'name'"),
            "got: {err:#}"
        );
    }

    #[test]
    fn writes_only_the_canonical_yaml_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_signed_node_item(
            tmp.path(),
            "schedules",
            "demo",
            &serde_json::json!({ "schedule_id": "demo" }),
            &identity(),
        )
        .unwrap();

        assert_eq!(path, tmp.path().join("schedules/demo.yaml"));
        assert!(path.is_file());
        assert_eq!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("yaml")
        );
    }
}
