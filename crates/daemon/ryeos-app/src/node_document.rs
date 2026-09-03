//! Shared format and publication mechanics for node-signed YAML documents.
//!
//! Configuration and policy have different semantic owners, but both use the
//! same bounded, path-owned, node-signed document envelope.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::identity::NodeIdentity;

pub const MAX_ITEM_BYTES: u64 = 1024 * 1024;
pub const MAX_SIGNATURE_OVERHEAD_BYTES: u64 = 512;

pub struct VerifiedNodeDocument {
    pub body: serde_json::Value,
    pub signer_fingerprint: String,
}

pub fn verify_pinned_signed_yaml(
    file: &lillux::PinnedRegularFile,
    trust_store: &ryeos_engine::trust::TrustStore,
) -> Result<VerifiedNodeDocument> {
    let content = file.read_bounded(MAX_ITEM_BYTES)?;
    let content = std::str::from_utf8(&content)
        .with_context(|| format!("node document at {} is not UTF-8", file.path().display()))?;
    let envelope = ryeos_engine::contracts::SignatureEnvelope {
        prefix: "#".into(),
        suffix: None,
        after_shebang: false,
    };
    let header = ryeos_engine::item_resolution::parse_signature_header(content, &envelope)
        .with_context(|| {
            format!(
                "node document at {} has no valid signature line",
                file.path().display()
            )
        })?;
    let signer_fingerprint = header.signer_fingerprint.clone();
    let (trust_class, _) =
        ryeos_engine::trust::verify_item_signature(content, &header, &envelope, trust_store)?;
    if trust_class != ryeos_engine::contracts::TrustClass::Trusted {
        bail!(
            "node document at {} is not trusted (trust_class: {:?})",
            file.path().display(),
            trust_class
        );
    }
    let body: serde_json::Value = serde_yaml::from_str(
        &lillux::signature::strip_signature_lines_with_envelope(content, "#", None),
    )
    .with_context(|| format!("parse YAML body of {}", file.path().display()))?;
    for forbidden in ["category", "section"] {
        if body.get(forbidden).is_some() {
            bail!(
                "node document at {} declares path-owned structural field `{forbidden}`",
                file.path().display()
            );
        }
    }
    Ok(VerifiedNodeDocument {
        body,
        signer_fingerprint,
    })
}

pub fn render_signed_item(
    section: &str,
    name: &str,
    body: &serde_json::Value,
    identity: &NodeIdentity,
) -> Result<Vec<u8>> {
    let map = body.as_object().with_context(|| {
        format!("node section `{section}` item `{name}` must be a YAML mapping")
    })?;
    let mut yaml_map = serde_yaml::Mapping::new();
    for (key, value) in map {
        if key == "section" || key == "category" {
            bail!(
                "node document writer refusing path-owned structural field `{key}` for section `{section}` item `{name}`"
            );
        }
        yaml_map.insert(
            serde_yaml::Value::String(key.clone()),
            serde_yaml::to_value(value).context("serialize node document field")?,
        );
    }
    let yaml = serde_yaml::to_string(&yaml_map).context("serialize node document body")?;
    let signed =
        lillux::signature::sign_content(&yaml, identity.signing_key(), "#", None).into_bytes();
    if signed.len() as u64 > MAX_ITEM_BYTES {
        bail!("signed node section `{section}` item `{name}` exceeds {MAX_ITEM_BYTES} bytes");
    }
    Ok(signed)
}

pub fn write_signed_item(
    base_dir: &Path,
    section: &str,
    name: &str,
    body: &serde_json::Value,
    identity: &NodeIdentity,
) -> Result<std::path::PathBuf> {
    let bytes = render_signed_item(section, name, body, identity)?;
    let base_directory = lillux::PinnedDirectory::open_or_create(base_dir)
        .context("establish no-follow node document root")?;
    let section_directory = base_directory
        .open_or_create_child(std::ffi::OsStr::new(section), 0o777)
        .with_context(|| format!("establish node document section {section}"))?;
    let _directory_lock = section_directory.lock_exclusive()?;
    let filename = format!("{name}.yaml");
    let filename = std::ffi::OsStr::new(&filename);
    let expected = section_directory.open_pinned_regular(filename, false)?;
    section_directory
        .atomic_write_pinned_if_same(filename, expected.as_ref(), &bytes, 0o600)
        .with_context(|| {
            format!(
                "write node document {}",
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
    fn rejects_path_owned_fields() {
        let error = render_signed_item(
            "schedules",
            "demo",
            &serde_json::json!({ "section": "schedules" }),
            &identity(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("path-owned structural field"));
    }

    #[test]
    fn writes_only_the_canonical_yaml_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_signed_item(
            tmp.path(),
            "schedules",
            "demo",
            &serde_json::json!({ "schedule_id": "demo" }),
            &identity(),
        )
        .unwrap();
        assert_eq!(path, tmp.path().join("schedules/demo.yaml"));
    }
}
