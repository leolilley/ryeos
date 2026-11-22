//! Which key advanced a project head.
//!
//! A head ref path names the owning principal, not the key that moved it,
//! and several keys may speak for one principal. So every write or advance
//! of a project head records, node-signed, the advancing key and how the
//! node resolved its owner. The record is written in the head's own
//! directory under the head lock **before** the ref is swapped, named
//! `advance-<previous hash | genesis>-<new hash>`:
//! - a head never moves without its record;
//! - a record whose new hash never became the head (the swap failed after
//!   the record was written) is ignored by readers, which only ever look up
//!   the record for an exact (previous, new) pair;
//! - a replayed advance finds its record and does not duplicate it; a replay
//!   that names a different key or resolution is refused.
//!
//! Records live under the head's directory, so the explicit project-head
//! cutover removes them with the head.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::refs::TrustStore;
use crate::signer::Signer;

/// Schema of a head-advance record.
pub const HEAD_ADVANCE_SCHEMA: u32 = 1;

/// Kind tag of a head-advance record.
pub const HEAD_ADVANCE_KIND: &str = "project_head_advance";

/// File-name prefix of head-advance records in a head directory.
pub const HEAD_ADVANCE_PREFIX: &str = "advance-";

const GENESIS: &str = "genesis";

/// The key moving a head and how the node resolved its owner. Built only
/// from the node's ownership resolver at the advance site.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadAdvancer {
    /// `fp:<64 lowercase hex>` of the advancing key.
    pub key: String,
    /// The chain root the key's owner was resolved through, or `None` when
    /// the key is the root of its own principal. Evidence only.
    pub via_root: Option<String>,
}

/// One node-signed head-advance record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadAdvanceRecord {
    pub schema: u32,
    pub kind: String,
    pub ref_path: String,
    pub previous_hash: Option<String>,
    pub target_hash: String,
    pub advancer: HeadAdvancer,
    pub recorded_at: String,
    pub signer: String,
    pub signature: String,
}

fn is_lower_hex_64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl HeadAdvancer {
    pub fn validate(&self) -> Result<()> {
        let hex = self
            .key
            .strip_prefix("fp:")
            .filter(|hex| is_lower_hex_64(hex))
            .ok_or_else(|| anyhow!("head advancer key must be fp:<64 lowercase hex>"))?;
        let _ = hex;
        if let Some(root) = &self.via_root
            && !is_lower_hex_64(root)
        {
            bail!("head advancer via_root must be 64 lowercase hex");
        }
        Ok(())
    }
}

/// The record file name for one (previous, new) advance.
pub fn record_name(previous_hash: Option<&str>, target_hash: &str) -> String {
    format!(
        "{HEAD_ADVANCE_PREFIX}{}-{target_hash}",
        previous_hash.unwrap_or(GENESIS)
    )
}

impl HeadAdvanceRecord {
    fn unsigned(&self) -> serde_json::Value {
        json!({
            "schema": self.schema,
            "kind": self.kind,
            "ref_path": self.ref_path,
            "previous_hash": self.previous_hash,
            "target_hash": self.target_hash,
            "advancer": self.advancer,
            "recorded_at": self.recorded_at,
            "signer": self.signer,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.schema != HEAD_ADVANCE_SCHEMA || self.kind != HEAD_ADVANCE_KIND {
            bail!("not a version {HEAD_ADVANCE_SCHEMA} project head advance record");
        }
        if !is_lower_hex_64(&self.target_hash)
            || self
                .previous_hash
                .as_deref()
                .is_some_and(|previous| !is_lower_hex_64(previous))
        {
            bail!("head advance record hashes must be 64 lowercase hex");
        }
        self.advancer.validate()?;
        if self.signature.is_empty() {
            bail!("head advance record is unsigned");
        }
        Ok(())
    }

    /// Verify the record's node signature under `trust_store`.
    pub fn verify(&self, trust_store: &TrustStore) -> Result<()> {
        self.validate()?;
        let key = trust_store
            .get(&self.signer)
            .ok_or_else(|| anyhow!("head advance signer {} not in trust store", self.signer))?;
        let canonical =
            lillux::canonical_json(&self.unsigned()).context("canonicalize head advance record")?;
        let signature = base64::engine::general_purpose::STANDARD
            .decode(&self.signature)
            .context("decode head advance signature")?;
        let signature = lillux::crypto::Signature::from_slice(&signature)
            .map_err(|error| anyhow!("parse head advance signature: {error}"))?;
        lillux::crypto::Verifier::verify(key, canonical.as_bytes(), &signature)
            .map_err(|error| anyhow!("head advance signature does not verify: {error}"))
    }
}

/// Write (or, on a replay, find) the record for one advance in the head's
/// directory. Called by the head writers under the head lock, before the
/// ref swap.
pub(crate) fn record_head_advance(
    head_directory: &lillux::PinnedDirectory,
    ref_path: &str,
    previous_hash: Option<&str>,
    target_hash: &str,
    advancer: &HeadAdvancer,
    signer: &dyn Signer,
    trust_store: &TrustStore,
) -> Result<()> {
    advancer.validate()?;
    let name = record_name(previous_hash, target_hash);
    let file_name = std::ffi::OsStr::new(&name);
    if let Some(existing) = read_record(head_directory, file_name, trust_store)? {
        if existing.ref_path != ref_path
            || existing.previous_hash.as_deref() != previous_hash
            || existing.target_hash != target_hash
        {
            bail!("head advance record {name} does not describe this advance");
        }
        if &existing.advancer != advancer {
            bail!(
                "head_advance_key_conflict: the advance {} -> {target_hash} of {ref_path} is \
                 already recorded for key {}, not {}",
                previous_hash.unwrap_or(GENESIS),
                existing.advancer.key,
                advancer.key
            );
        }
        return Ok(());
    }
    let mut record = HeadAdvanceRecord {
        schema: HEAD_ADVANCE_SCHEMA,
        kind: HEAD_ADVANCE_KIND.to_owned(),
        ref_path: ref_path.to_owned(),
        previous_hash: previous_hash.map(str::to_owned),
        target_hash: target_hash.to_owned(),
        advancer: advancer.clone(),
        recorded_at: lillux::time::iso8601_now(),
        signer: signer.fingerprint().to_owned(),
        signature: String::new(),
    };
    let canonical =
        lillux::canonical_json(&record.unsigned()).context("canonicalize head advance record")?;
    record.signature =
        base64::engine::general_purpose::STANDARD.encode(signer.sign(canonical.as_bytes()));
    record.validate()?;
    let bytes = lillux::canonical_json(&serde_json::to_value(&record)?)
        .context("canonicalize signed head advance record")?;
    head_directory
        .atomic_write_if_same(
            file_name,
            None,
            bytes.as_bytes(),
            0o666,
            crate::STATE_TEMP_PURPOSE,
        )
        .with_context(|| format!("write head advance record {name}"))
}

fn read_record(
    head_directory: &lillux::PinnedDirectory,
    file_name: &std::ffi::OsStr,
    trust_store: &TrustStore,
) -> Result<Option<HeadAdvanceRecord>> {
    let Some(file) = head_directory.open_regular(file_name, false)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut std::io::Read::take(&file, 64 * 1024), &mut bytes)
        .context("read head advance record")?;
    let record: HeadAdvanceRecord =
        serde_json::from_slice(&bytes).context("decode head advance record")?;
    record.verify(trust_store)?;
    Ok(Some(record))
}

/// The verified record for an exact advance of the head in
/// `head_directory`, if one was recorded. Readers look records up only by
/// the (previous, new) pair of an advance that actually happened.
pub fn read_head_advance(
    head_directory: &lillux::PinnedDirectory,
    previous_hash: Option<&str>,
    target_hash: &str,
    trust_store: &TrustStore,
) -> Result<Option<HeadAdvanceRecord>> {
    read_record(
        head_directory,
        std::ffi::OsStr::new(&record_name(previous_hash, target_hash)),
        trust_store,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_names_key_the_exact_advance() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        assert_eq!(record_name(None, &a), format!("advance-genesis-{a}"));
        assert_eq!(record_name(Some(&a), &b), format!("advance-{a}-{b}"));
    }

    #[test]
    fn an_advancer_must_name_a_key_fingerprint() {
        let good = HeadAdvancer {
            key: format!("fp:{}", "c".repeat(64)),
            via_root: Some("d".repeat(64)),
        };
        assert!(good.validate().is_ok());
        for bad in [
            HeadAdvancer {
                key: format!("pid:v1:{}", "c".repeat(64)),
                via_root: None,
            },
            HeadAdvancer {
                key: "c".repeat(64),
                via_root: None,
            },
            HeadAdvancer {
                key: format!("fp:{}", "c".repeat(64)),
                via_root: Some("short".into()),
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
    }
}
