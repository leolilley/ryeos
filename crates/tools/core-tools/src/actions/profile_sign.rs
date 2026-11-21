//! Standalone init-profile signing.
//!
//! An init profile is a `#`-prefixed signed YAML document that `ryeos init`
//! loads from `<source>/.ai/node/init/profiles/<name>.yaml` and verifies
//! against the pinned publisher keys before it trusts anything it names. This
//! signs one such file in place with a publisher key, producing exactly the
//! envelope init verifies, so a bundle author can ship a profile without
//! reimplementing the envelope.
//!
//! The body is parsed and validated as an init profile before it is signed, so
//! a malformed profile is refused here instead of at node init.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lillux::crypto::SigningKey;
use lillux::signature::{compute_fingerprint, sign_content, strip_signature_lines};
use ryeos_node_runtime::node_policy::NodePolicyTable;
use ryeos_node_runtime::node_policy::generation::NodeInitProfile;
use serde::Serialize;

/// Largest profile this will read; init reads seed documents with its own
/// bound and a profile is a few KiB.
const PROFILE_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Serialize)]
pub struct ProfileSignReport {
    pub profile: PathBuf,
    pub signer_fingerprint: String,
    /// Whether the file was rewritten. An already-valid signature by the same
    /// key over an unchanged body leaves the file untouched.
    pub changed: bool,
}

/// Sign the init profile at `profile` in place with `signing_key`.
pub fn profile_sign(profile: &Path, signing_key: &SigningKey) -> Result<ProfileSignReport> {
    let bytes = lillux::read_regular_file_bounded_no_follow(profile, PROFILE_MAX_BYTES)
        .with_context(|| format!("read init profile {}", profile.display()))?;
    let raw = String::from_utf8(bytes)
        .with_context(|| format!("init profile {} is not UTF-8", profile.display()))?;
    let body = strip_signature_lines(&raw);

    let parsed: NodeInitProfile = serde_yaml::from_str(&body)
        .with_context(|| format!("parse init profile {}", profile.display()))?;
    parsed.validate(&NodePolicyTable::new(), profile)?;
    let signer_fingerprint = compute_fingerprint(&signing_key.verifying_key());
    let signed = sign_content(&body, signing_key, "#", None);
    // Ed25519 is deterministic and the signature covers the body hash only, so
    // an unchanged body signed by the same key yields the same hash, signature
    // and fingerprint; keep the existing timestamp rather than churn the file.
    let unchanged = same_signature(&raw, &signed);
    if !unchanged {
        lillux::atomic_write(profile, signed.as_bytes())
            .with_context(|| format!("write signed init profile {}", profile.display()))?;
    }
    Ok(ProfileSignReport {
        profile: profile.to_path_buf(),
        signer_fingerprint,
        changed: !unchanged,
    })
}

/// True when both documents carry the same `hash:signature:fingerprint` on
/// their first line (the timestamp is envelope metadata, not signed).
fn same_signature(existing: &str, fresh: &str) -> bool {
    fn tail(doc: &str) -> Option<(&str, &str, &str)> {
        let rest = doc.lines().next()?.strip_prefix("# ryeos:signed:")?;
        // `<timestamp>:<hash>:<sig>:<fp>`; the timestamp has colons of its own.
        let mut parts = rest.rsplitn(4, ':');
        let fp = parts.next()?;
        let sig = parts.next()?;
        let hash = parts.next()?;
        Some((hash, sig, fp))
    }
    matches!((tail(existing), tail(fresh)), (Some(a), Some(b)) if a == b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryeos_engine::contracts::SignatureEnvelope;
    use ryeos_engine::trust::{SignerContext, SignerScope, TrustStore, TrustedSignerEntry};

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn store_for(key: &SigningKey) -> TrustStore {
        use base64::Engine as _;
        let vk = key.verifying_key();
        TrustStore::from_signer_entries(&[TrustedSignerEntry {
            fingerprint: compute_fingerprint(&vk),
            public_key: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(vk.as_bytes())
            ),
            label: "test".into(),
            scope: SignerScope::publisher_default(),
            endorsements: None,
        }])
        .unwrap()
    }

    /// The same check init applies to a seed document.
    fn verify(content: &str, store: &TrustStore) -> bool {
        let envelope = SignatureEnvelope {
            prefix: "#".into(),
            suffix: None,
            after_shebang: false,
        };
        let Some(header) =
            ryeos_engine::item_resolution::parse_signature_header(content, &envelope)
        else {
            return false;
        };
        matches!(
            ryeos_engine::trust::verify_item_signature(
                content,
                &header,
                &envelope,
                store,
                &SignerContext::bundle("init_seed"),
            ),
            Ok((ryeos_engine::contracts::TrustClass::Trusted, _))
        )
    }

    /// A shipped profile, unsigned, copied under the name the test gives it.
    fn profile_in(dir: &Path) -> PathBuf {
        let shipped = include_str!("../../../../../bundles/.ai/node/init/profiles/core.yaml");
        let body = strip_signature_lines(shipped);
        let path = dir.join("core.yaml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn signed_profile_is_accepted_by_matching_trust_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = profile_in(tmp.path());
        let signer = key(7);
        let report = profile_sign(&path, &signer).unwrap();
        assert!(report.changed);
        assert_eq!(
            report.signer_fingerprint,
            compute_fingerprint(&signer.verifying_key())
        );
        let signed = std::fs::read_to_string(&path).unwrap();
        assert!(verify(&signed, &store_for(&signer)));
        assert!(!verify(&signed, &store_for(&key(8))));
    }

    #[test]
    fn edited_profile_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = profile_in(tmp.path());
        let signer = key(7);
        profile_sign(&path, &signer).unwrap();
        let signed = std::fs::read_to_string(&path).unwrap();
        let edited = format!("{signed}# extra\n");
        assert!(!verify(&edited, &store_for(&signer)));
    }

    #[test]
    fn resigning_unchanged_profile_leaves_file_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let path = profile_in(tmp.path());
        let signer = key(7);
        profile_sign(&path, &signer).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        let report = profile_sign(&path, &signer).unwrap();
        assert!(!report.changed);
        assert_eq!(first, std::fs::read_to_string(&path).unwrap());
        // A different key re-signs.
        assert!(profile_sign(&path, &key(9)).unwrap().changed);
    }

    #[test]
    fn invalid_profile_is_not_signed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("core.yaml");
        std::fs::write(&path, "not: a profile\n").unwrap();
        assert!(profile_sign(&path, &key(7)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not: a profile\n");
    }
}
