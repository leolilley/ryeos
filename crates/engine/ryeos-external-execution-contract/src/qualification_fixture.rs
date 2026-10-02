//! Signed finite Q fixture data, not mount, execution, or export authority.
//!
//! Exactly one regular, nonexecutable file at depth one is permitted. Native
//! owners must enforce directory shape and descriptor custody; these portable
//! bytes cannot prove either. Callers must bound the containing wire document
//! before deserialization. No filesystem or CAS dependency belongs here.

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

pub const Q_FIXTURE_CONTRACT_SCHEMA: u32 = 1;
pub const Q_FIXTURE_MOUNT_DESTINATION: &str = "/ryeos/qualification-fixture";
pub const MAX_Q_FIXTURE_BYTES: u64 = 65_536;
pub const MAX_Q_FIXTURE_LEAF_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QFixtureContract {
    pub schema: u32,
    pub mount_destination: String,
    pub leaf: String,
    /// Canonical padded base64 of the exact signed initial bytes; empty is valid.
    pub initial_content_b64: String,
    pub initial_blob_hash: String,
    /// Ceiling for both initial and captured content, not ordinary export bytes.
    pub max_bytes: u64,
}

impl QFixtureContract {
    pub fn validate(&self) -> Result<()> {
        self.decoded_initial_content().map(|_| ())
    }

    /// Bound encoded length before allocation, then authenticate exact bytes.
    /// The owner still must create the file under its original pinned directory.
    pub fn decoded_initial_content(&self) -> Result<Vec<u8>> {
        ensure!(
            self.schema == Q_FIXTURE_CONTRACT_SCHEMA,
            "unsupported Q fixture schema"
        );
        ensure!(
            self.mount_destination == Q_FIXTURE_MOUNT_DESTINATION,
            "Q fixture mount changed"
        );
        ensure!(
            !self.leaf.is_empty()
                && self.leaf.len() <= MAX_Q_FIXTURE_LEAF_BYTES
                && self.leaf.as_bytes()[0].is_ascii_alphanumeric()
                && self
                    .leaf
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "Q fixture leaf must be one bounded safe component"
        );
        ensure!(
            (1..=MAX_Q_FIXTURE_BYTES).contains(&self.max_bytes),
            "Q fixture byte ceiling is invalid"
        );
        ensure!(
            self.initial_blob_hash.len() == 64
                && self
                    .initial_blob_hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "Q fixture initial blob hash is not canonical"
        );
        ensure!(
            self.initial_content_b64.len() as u64 <= self.max_bytes.div_ceil(3) * 4,
            "Q fixture initial encoding exceeds its ceiling"
        );
        // Padded base64 may encode up to two more bytes than max_bytes in the
        // same encoded length. A bounded output slice refuses those cases
        // without allocating beyond the signed decoded-byte ceiling.
        let capacity = self
            .max_bytes
            .min(self.initial_content_b64.len() as u64 / 4 * 3);
        let mut bytes = vec![0; capacity as usize];
        let decoded = STANDARD
            .decode_slice(&self.initial_content_b64, &mut bytes)
            .context("decode Q fixture initial content")?;
        bytes.truncate(decoded);
        ensure!(
            bytes.len() as u64 <= self.max_bytes
                && STANDARD.encode(&bytes) == self.initial_content_b64
                && hex::encode(Sha256::digest(&bytes)) == self.initial_blob_hash,
            "Q fixture initial bytes changed their bound or commitment"
        );
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(bytes: &[u8]) -> QFixtureContract {
        QFixtureContract {
            schema: Q_FIXTURE_CONTRACT_SCHEMA,
            mount_destination: Q_FIXTURE_MOUNT_DESTINATION.into(),
            leaf: "candidate.txt".into(),
            initial_content_b64: STANDARD.encode(bytes),
            initial_blob_hash: hex::encode(Sha256::digest(bytes)),
            max_bytes: MAX_Q_FIXTURE_BYTES,
        }
    }

    #[test]
    fn exact_initial_bytes_include_empty_and_maximum() {
        for bytes in [vec![], vec![0; MAX_Q_FIXTURE_BYTES as usize]] {
            assert_eq!(contract(&bytes).decoded_initial_content().unwrap(), bytes);
        }
        assert!(
            contract(&vec![0; MAX_Q_FIXTURE_BYTES as usize + 1])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn rejects_path_authority_and_byte_substitution() {
        for leaf in [
            "",
            ".",
            "..",
            "../secret",
            "/secret",
            "a/b",
            "a\\b",
            "é",
            "a\0b",
        ] {
            let mut value = contract(b"old");
            value.leaf = leaf.into();
            assert!(value.validate().is_err(), "{leaf:?}");
        }
        let mut value = contract(b"old");
        value.mount_destination = "/workspace".into();
        assert!(value.validate().is_err());
        value = contract(b"old");
        value.initial_content_b64 = STANDARD.encode(b"new");
        assert!(value.validate().is_err());
        value = contract(b"a");
        value.initial_content_b64 = "YQ".into();
        assert!(value.validate().is_err());
        value = contract(b"four");
        value.max_bytes = 3;
        assert!(value.validate().is_err());
        value = contract(b"a");
        value.schema = 0;
        assert!(value.validate().is_err());
    }

    #[test]
    fn wire_is_closed_and_all_fields_are_required() {
        let value = serde_json::to_value(contract(b"a")).unwrap();
        for key in value.as_object().unwrap().keys() {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<QFixtureContract>(missing).is_err());
        }
        let mut extra = value;
        extra["executable"] = true.into();
        assert!(serde_json::from_value::<QFixtureContract>(extra).is_err());
    }
}
