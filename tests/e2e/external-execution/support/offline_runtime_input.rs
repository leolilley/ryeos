//! Authored expectation for the minimal retained-project offline runtime input.
//! This TWO-entry content/tree.v2 product is distinct from environment-products'
//! FOUR-entry large.v2 product (which also has `current` and `empty-subdir`).
//! Reusing its executable bytes does not reuse that other product's identity.
//! No manifest, CAS object, capture witness, or admission authority is created
//! here. The public capture service must independently reproduce this exact pin.

use anyhow::{Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ryeos_state::external_content::products::ProductCaptureEvidence;

pub const MANIFEST_HASH: &str = "2eebec060cca7246a1226b2500a5233ed4f1a31580d90d76e0bd0e94a4d35d21";
pub const PROGRAM_HASH: &str = "3a55c0c7914fdeccc41ac91789116675ca07befdc1c64d8c4b1b77c4b7296d77";
pub const PROGRAM_BYTES: usize = 9688;

pub fn bytes() -> Result<Vec<u8>> {
    let bytes = STANDARD.decode(
        include_str!("../../environment-products/runtime-program.b64")
            .split_whitespace()
            .collect::<String>(),
    )?;
    ensure!(
        bytes.len() == PROGRAM_BYTES,
        "offline program length differs: expected {PROGRAM_BYTES}, observed {}",
        bytes.len()
    );
    require_coordinate("program hash", &lillux::sha256_hex(&bytes), PROGRAM_HASH)?;
    Ok(bytes)
}

fn require_coordinate(label: &str, observed: &str, expected: &str) -> Result<()> {
    ensure!(
        observed == expected,
        "offline runtime {label} differs: expected {expected}, observed {observed}"
    );
    Ok(())
}

/// Compare only public coordinates/metrics, never dump full signed evidence or
/// any credential-bearing response. The caller already validates the witness.
pub fn verify_capture(
    evidence: &ProductCaptureEvidence,
    thread: &str,
    chain: &str,
    owner: &str,
) -> Result<()> {
    for (label, observed, expected) in [
        ("producer thread", evidence.thread_id.as_str(), thread),
        ("producer chain", evidence.chain_root_id.as_str(), chain),
        ("owner principal", evidence.owner_principal.as_str(), owner),
        (
            "manifest hash",
            evidence.manifest_hash.as_str(),
            MANIFEST_HASH,
        ),
        (
            "manifest kind",
            evidence.manifest_kind.as_str(),
            "external_content_manifest",
        ),
    ] {
        require_coordinate(label, observed, expected)?;
    }
    ensure!(
        evidence.entry_count == 2,
        "offline runtime entry count differs: expected 2, observed {}",
        evidence.entry_count
    );
    ensure!(
        evidence.total_bytes == PROGRAM_BYTES as u64,
        "offline runtime byte count differs: expected {PROGRAM_BYTES}, observed {}",
        evidence.total_bytes
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_two_entry_content_expectation_matches_program_bytes_and_mode() {
        assert_eq!(bytes().unwrap().len(), PROGRAM_BYTES);
        // Static authored expectation used ONLY for this pure digest check,
        // never stored or supplied as a replacement for the public capture.
        let authored = serde_json::json!({
            "entries":[{"kind":"dir","path":"bin"},
                {"blob_hash":PROGRAM_HASH,"kind":"file","mode":0o755,"path":"bin/program","size":PROGRAM_BYTES}],
            "entry_count":2,"kind":"external_content_manifest",
            "schema":"ryeos.external_content.tree.v2","total_bytes":PROGRAM_BYTES,
        });
        assert_eq!(
            lillux::sha256_hex(lillux::canonical_json(&authored).unwrap().as_bytes()),
            MANIFEST_HASH
        );
        let old: serde_json::Value = serde_json::from_str(include_str!(
            "../../environment-products/expected-runtime-manifest.json"
        ))
        .unwrap();
        assert_eq!(old["entry_count"], 4);
        assert_eq!(old["kind"], "external_large_content_manifest");
        assert_ne!(
            lillux::sha256_hex(lillux::canonical_json(&old).unwrap().as_bytes()),
            MANIFEST_HASH
        );
    }

    #[test]
    fn mismatch_diagnostic_names_only_exact_coordinate() {
        let error = require_coordinate("manifest hash", "observed", "expected")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "offline runtime manifest hash differs: expected expected, observed observed"
        );
    }
}
