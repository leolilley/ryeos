//! Historical signed Bundle evidence checks shared by retained-source owners.
//! These authenticate bytes only; callers own admission and exact source joins.

use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use ryeos_state::objects::{
    RetainedBundleSignerKey, RetainedSignatureEnvelope, RetainedSignedBundleItem,
    RetainedSignedBundleManifest,
};

pub(crate) fn verify_retained_signed_bundle_item(
    item: &RetainedSignedBundleItem,
    signer: &RetainedBundleSignerKey,
    signed_bytes: &[u8],
) -> Result<()> {
    ensure!(
        item.signer_fingerprint == signer.signer_fingerprint,
        "retained recipe item signer differs from its selected verifier"
    );
    verify_retained_signed_envelope(
        signed_bytes,
        &item.signed_blob_hash,
        &item.raw_content_digest,
        &item.signature_envelope,
        signer,
    )?;
    Ok(())
}

pub(crate) fn verify_retained_signed_envelope(
    signed_bytes: &[u8],
    signed_hash: &str,
    body_hash: &str,
    envelope: &RetainedSignatureEnvelope,
    signer: &RetainedBundleSignerKey,
) -> Result<String> {
    ensure!(
        lillux::sha256_hex(signed_bytes) == signed_hash,
        "retained signed source bytes differ from their address"
    );
    let key = retained_verifier(signer)?;
    let signed = std::str::from_utf8(signed_bytes)?;
    let (raw, header) = lillux::signature::strip_canonical_signature_with_envelope(
        signed,
        &envelope.prefix,
        envelope.suffix.as_deref(),
        envelope.after_shebang,
    )?;
    let header = header.context("retained source has no canonical signature header")?;
    ensure!(
        lillux::sha256_hex(raw.as_bytes()) == body_hash
            && lillux::signature::is_valid_signature_for(
                &header.content_hash,
                &header.signature_b64,
                &header.signer_fingerprint,
                lillux::signature::content_to_sign(&raw, envelope.after_shebang),
                &key,
                &signer.signer_fingerprint,
            ),
        "retained source signature or parsed body changed"
    );
    Ok(raw)
}

pub(crate) fn verify_retained_signed_bundle_manifest(
    manifest: &RetainedSignedBundleManifest,
    signer: &RetainedBundleSignerKey,
    signed_bytes: &[u8],
) -> Result<()> {
    ensure!(
        manifest.signer_fingerprint == signer.signer_fingerprint
            && lillux::sha256_hex(signed_bytes) == manifest.signed_blob_hash,
        "retained Bundle manifest signer differs from historical verifier"
    );
    let identity = ryeos_engine::plan_builder::verify_retained_signed_bundle_manifest_bytes(
        signed_bytes,
        &manifest.bundle_name,
        &manifest.signer_fingerprint,
        &retained_verifier(signer)?,
    )?;
    ensure!(
        identity.body_digest == manifest.body_digest
            && identity.name == manifest.bundle_name
            && identity.signer_fingerprint == manifest.signer_fingerprint,
        "retained signed Bundle manifest identity changed"
    );
    Ok(())
}

pub(crate) fn retained_verifier(
    signer: &RetainedBundleSignerKey,
) -> Result<lillux::crypto::VerifyingKey> {
    let encoded = signer
        .verifying_key
        .strip_prefix("ed25519:")
        .context("retained recipe verifier is not Ed25519")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    let key_bytes: [u8; 32] = decoded
        .try_into()
        .map_err(|_| anyhow::anyhow!("retained recipe verifier length changed"))?;
    let key = lillux::crypto::VerifyingKey::from_bytes(&key_bytes)?;
    ensure!(
        lillux::crypto::fingerprint(&key) == signer.signer_fingerprint,
        "retained recipe verifier differs from fingerprint"
    );
    Ok(key)
}
