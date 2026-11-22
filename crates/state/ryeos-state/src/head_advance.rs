//! Which key advanced a project head.
//!
//! A head ref path names the owning principal, not the key that moved it,
//! and several keys may speak for one principal. So every write or advance
//! of a project head records, node-signed, the advancing key and how the
//! node resolved its owner. The record is written in the head's own
//! directory under the head lock **before** the ref is swapped. One
//! (previous, new) pair can recur over a head's life (A to B, back to A, to
//! B again), so records are append-only per occurrence:
//! `advance-<previous hash | genesis>-<new hash>-<occurrence>`.
//! - A head never moves without its record.
//! - Each record names the head ref it observed (`observed_head`, the
//!   signature of the head ref current when it was written, or none for a
//!   first write). If the head ref is still exactly that one, the swap after
//!   the record never happened (a crash): the same advancer retrying reuses
//!   the record; anyone else writes the next occurrence. Otherwise every
//!   advance writes the next occurrence.
//! - Readers list every occurrence of a pair; a record whose swap never
//!   happened is evidence of an attempt only.
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
    /// Which occurrence of this (previous, new) pair, from 1.
    pub occurrence: u64,
    /// Signature of the head ref current when this record was written;
    /// `None` when the head did not exist yet.
    pub observed_head: Option<String>,
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

fn pair_prefix(previous_hash: Option<&str>, target_hash: &str) -> String {
    format!(
        "{HEAD_ADVANCE_PREFIX}{}-{target_hash}-",
        previous_hash.unwrap_or(GENESIS)
    )
}

/// The record file name for one occurrence of a (previous, new) advance.
pub fn record_name(previous_hash: Option<&str>, target_hash: &str, occurrence: u64) -> String {
    format!("{}{occurrence}", pair_prefix(previous_hash, target_hash))
}

/// The occurrences of one pair recorded in `head_directory`, ascending.
fn occurrences(
    head_directory: &lillux::PinnedDirectory,
    previous_hash: Option<&str>,
    target_hash: &str,
) -> Result<Vec<u64>> {
    let prefix = pair_prefix(previous_hash, target_hash);
    let mut found = Vec::new();
    for name in head_directory.entry_names()? {
        let Some(text) = name.to_str() else {
            continue;
        };
        if let Some(occurrence) = text
            .strip_prefix(&prefix)
            .and_then(|rest| rest.parse::<u64>().ok())
        {
            found.push(occurrence);
        }
    }
    found.sort_unstable();
    Ok(found)
}

impl HeadAdvanceRecord {
    fn unsigned(&self) -> serde_json::Value {
        json!({
            "schema": self.schema,
            "kind": self.kind,
            "ref_path": self.ref_path,
            "previous_hash": self.previous_hash,
            "target_hash": self.target_hash,
            "occurrence": self.occurrence,
            "observed_head": self.observed_head,
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
        if self.occurrence == 0 {
            bail!("head advance record occurrences count from 1");
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

/// Write the record for one advance in the head's directory, or reuse the
/// latest occurrence on a crash-retry by the same advancer. Called by the
/// head writers under the head lock, after the compare and before the ref
/// swap. `observed_head` is the signature of the head ref current now
/// (`None` for a first write).
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_head_advance(
    head_directory: &lillux::PinnedDirectory,
    ref_path: &str,
    previous_hash: Option<&str>,
    target_hash: &str,
    observed_head: Option<&str>,
    advancer: &HeadAdvancer,
    signer: &dyn Signer,
    trust_store: &TrustStore,
) -> Result<()> {
    advancer.validate()?;
    let existing = occurrences(head_directory, previous_hash, target_hash)?;
    let next = existing.last().map_or(1, |latest| latest + 1);
    if let Some(&latest) = existing.last() {
        let name = record_name(previous_hash, target_hash, latest);
        let record = read_record(head_directory, std::ffi::OsStr::new(&name), trust_store)?
            .with_context(|| format!("head advance record {name} vanished"))?;
        if record.ref_path != ref_path
            || record.previous_hash.as_deref() != previous_hash
            || record.target_hash != target_hash
            || record.occurrence != latest
        {
            bail!("head advance record {name} does not describe this advance");
        }
        // The head ref is exactly the one this record observed: its swap
        // never happened. The same advancer retrying reuses it.
        let unswapped = record.observed_head.as_deref() == observed_head;
        if unswapped && &record.advancer == advancer {
            return Ok(());
        }
    }
    let name = record_name(previous_hash, target_hash, next);
    let file_name = std::ffi::OsStr::new(&name);
    let mut record = HeadAdvanceRecord {
        schema: HEAD_ADVANCE_SCHEMA,
        kind: HEAD_ADVANCE_KIND.to_owned(),
        ref_path: ref_path.to_owned(),
        previous_hash: previous_hash.map(str::to_owned),
        target_hash: target_hash.to_owned(),
        occurrence: next,
        observed_head: observed_head.map(str::to_owned),
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

/// One recorded occurrence and whether its swap happened, derived on read
/// (records are never rewritten).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadHeadAdvance {
    pub record: HeadAdvanceRecord,
    /// `false` when the swap after this record never happened: a later
    /// occurrence of the same pair observed the same head ref (another key
    /// retried after a crash), or, for the latest occurrence, the head ref is
    /// still the one it observed.
    pub applied: bool,
}

/// Every verified occurrence of one (previous, new) advance of the head in
/// `head_directory`, ascending, each with its derived `applied` flag. A
/// record whose advancer is the node key with no `via_root` is a node
/// rewrite (compaction); readers show it as such.
pub fn read_head_advances(
    head_directory: &lillux::PinnedDirectory,
    previous_hash: Option<&str>,
    target_hash: &str,
    trust_store: &TrustStore,
) -> Result<Vec<ReadHeadAdvance>> {
    let mut records = Vec::new();
    for occurrence in occurrences(head_directory, previous_hash, target_hash)? {
        let name = record_name(previous_hash, target_hash, occurrence);
        if let Some(record) = read_record(head_directory, std::ffi::OsStr::new(&name), trust_store)?
        {
            records.push(record);
        }
    }
    let current_head = crate::refs::read_verified_ref_in_directory(
        head_directory,
        std::ffi::OsStr::new("head"),
        trust_store,
    )?
    .map(|head| head.signature);
    let count = records.len();
    Ok(records
        .iter()
        .enumerate()
        .map(|(index, record)| {
            let superseded = records[index + 1..]
                .iter()
                .any(|later| later.observed_head == record.observed_head);
            let unswapped_latest = index + 1 == count && current_head == record.observed_head;
            ReadHeadAdvance {
                record: record.clone(),
                applied: !superseded && !unswapped_latest,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_names_key_each_occurrence_of_an_advance() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        assert_eq!(record_name(None, &a, 1), format!("advance-genesis-{a}-1"));
        assert_eq!(record_name(Some(&a), &b, 3), format!("advance-{a}-{b}-3"));
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
