//! Meaning-blind integrity checks for retained external-content payloads.
//! These establish resident bytes, not source authorization or qualification.

use anyhow::{Context as _, bail};

pub fn verify_large_manifest_payload(
    authority: &crate::PinnedStateAuthority,
    manifest: &crate::objects::ExternalLargeContentManifestObject,
    limits: crate::object_closure::ObjectClosureLimits,
) -> anyhow::Result<()> {
    manifest.validate()?;
    let cas = authority.cas_store()?;
    let store = authority.large_object_store()?;
    for entry in &manifest.entries {
        if let Some(hash) = entry.file_sha256.as_deref() {
            store
                .verify_manifest_commitment(entry)
                .with_context(|| format!("verify retained content entry {}", entry.path))?;
            let findings = store.scrub_object(hash)?;
            if !findings.is_empty() {
                bail!(
                    "retained content entry {} failed byte integrity: {findings:?}",
                    entry.path
                );
            }
        } else if let Some(hash) = entry.blob_hash.as_deref() {
            let bytes = crate::object_closure::load_exact_cas_blob_with_cas(
                &cas,
                hash,
                crate::objects::MAX_EXTERNAL_CONTENT_FILE_BYTES.min(limits.max_blob_bytes),
            )?;
            if entry.size != Some(bytes.len() as u64) {
                bail!(
                    "retained content blob {} size contradicts its manifest",
                    entry.path
                );
            }
        }
    }
    Ok(())
}
