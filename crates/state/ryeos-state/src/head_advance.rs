//! Which key advanced a project head.
//!
//! A head ref path names the owning principal, not the key that moved it,
//! and several keys may speak for one principal. So every write or advance
//! of a project head records, node-signed, the advancing key and how the
//! node resolved its owner. The record is written in the head's own
//! directory under the head lock **before** the ref is swapped. One
//! (previous, new) pair can recur over a head's life (A to B, back to A, to
//! B again), so records are append-only:
//! `advance-<sequence>-<previous hash | genesis>-<new hash>-<occurrence>`.
//! - A head never moves without its record.
//! - `sequence` counts every record in the head directory from 1, in the
//!   order they were written under the head lock; `occurrence` counts the
//!   records of one pair.
//! - Each record names the head ref it observed (`observed_head`, the
//!   signature of the head ref current when it was written, or none for a
//!   first write). Head refs carry a whole-second timestamp and a
//!   deterministic signature, so two refs to the same target written in the
//!   same second are byte-identical: an observed head alone never says which
//!   write produced it. Order does. A record's swap happened exactly when the
//!   next record in sequence (or, for the latest record, the current head)
//!   observed a different head, since a swap always changes the target.
//! - A crash-retry by the same advancer reuses the latest record when its
//!   swap never happened; anyone else, or any later advance, writes a new
//!   record.
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
    /// Position of this record among every record in the head directory,
    /// from 1, in write order under the head lock.
    pub sequence: u64,
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

/// The record file name for one advance.
pub fn record_name(
    sequence: u64,
    previous_hash: Option<&str>,
    target_hash: &str,
    occurrence: u64,
) -> String {
    format!(
        "{HEAD_ADVANCE_PREFIX}{sequence}-{}-{target_hash}-{occurrence}",
        previous_hash.unwrap_or(GENESIS)
    )
}

/// One record's coordinates, parsed from its file name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordEntry {
    sequence: u64,
    previous_hash: Option<String>,
    target_hash: String,
    occurrence: u64,
}

impl RecordEntry {
    fn parse(name: &str) -> Option<Self> {
        let rest = name.strip_prefix(HEAD_ADVANCE_PREFIX)?;
        let (sequence, rest) = rest.split_once('-')?;
        let (rest, occurrence) = rest.rsplit_once('-')?;
        let (previous, target) = rest.split_once('-')?;
        let number = |text: &str| {
            (!text.is_empty()
                && text.bytes().all(|byte| byte.is_ascii_digit())
                && !text.starts_with('0'))
            .then(|| text.parse::<u64>().ok())
            .flatten()
        };
        let previous_hash = match previous {
            GENESIS => None,
            hash if is_lower_hex_64(hash) => Some(hash.to_owned()),
            _ => return None,
        };
        if !is_lower_hex_64(target) {
            return None;
        }
        Some(Self {
            sequence: number(sequence)?,
            previous_hash,
            target_hash: target.to_owned(),
            occurrence: number(occurrence)?,
        })
    }

    fn name(&self) -> String {
        record_name(
            self.sequence,
            self.previous_hash.as_deref(),
            &self.target_hash,
            self.occurrence,
        )
    }

    fn is_pair(&self, previous_hash: Option<&str>, target_hash: &str) -> bool {
        self.previous_hash.as_deref() == previous_hash && self.target_hash == target_hash
    }
}

/// Every record in `head_directory`, ascending by sequence. A name under the
/// record prefix that does not parse, or a repeated sequence, is refused.
fn entries(head_directory: &lillux::PinnedDirectory) -> Result<Vec<RecordEntry>> {
    let mut found = Vec::new();
    for name in head_directory.entry_names()? {
        let Some(text) = name.to_str() else {
            continue;
        };
        if !text.starts_with(HEAD_ADVANCE_PREFIX) {
            continue;
        }
        found.push(
            RecordEntry::parse(text)
                .ok_or_else(|| anyhow!("malformed head advance record name {text:?}"))?,
        );
    }
    found.sort_unstable_by_key(|entry| entry.sequence);
    if found
        .windows(2)
        .any(|pair| pair[0].sequence == pair[1].sequence)
    {
        bail!("head advance records repeat a sequence number");
    }
    Ok(found)
}

fn read_entry(
    head_directory: &lillux::PinnedDirectory,
    entry: &RecordEntry,
    trust_store: &TrustStore,
) -> Result<HeadAdvanceRecord> {
    let name = entry.name();
    let record = read_record(head_directory, std::ffi::OsStr::new(&name), trust_store)?
        .with_context(|| format!("head advance record {name} vanished"))?;
    if record.sequence != entry.sequence
        || record.previous_hash != entry.previous_hash
        || record.target_hash != entry.target_hash
        || record.occurrence != entry.occurrence
    {
        bail!("head advance record {name} does not match its name");
    }
    Ok(record)
}

impl HeadAdvanceRecord {
    fn unsigned(&self) -> serde_json::Value {
        json!({
            "schema": self.schema,
            "kind": self.kind,
            "ref_path": self.ref_path,
            "previous_hash": self.previous_hash,
            "target_hash": self.target_hash,
            "sequence": self.sequence,
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
        if self.occurrence == 0 || self.sequence == 0 {
            bail!("head advance record sequences and occurrences count from 1");
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
/// latest record on a crash-retry by the same advancer. Called by the head
/// writers under the head lock, after the compare and before the ref swap.
/// `observed_head` is the signature of the head ref current now (`None` for
/// a first write).
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
    if previous_hash == Some(target_hash) {
        bail!(
            "head_advance_target_unchanged: an advance to the head's current target {target_hash} \
             moves nothing and is never recorded"
        );
    }
    let existing = entries(head_directory)?;
    if let Some(latest) = existing.last() {
        let record = read_entry(head_directory, latest, trust_store)?;
        if record.ref_path != ref_path {
            bail!("head advance record {} names another head", latest.name());
        }
        // The latest record of all is this same advance and the head is
        // still exactly the one it observed: its swap never happened (a
        // swap always changes the target). The same advancer retrying
        // reuses it.
        if latest.is_pair(previous_hash, target_hash)
            && record.observed_head.as_deref() == observed_head
            && &record.advancer == advancer
        {
            return Ok(());
        }
    }
    let sequence = existing.last().map_or(1, |latest| latest.sequence + 1);
    let occurrence = existing
        .iter()
        .filter(|entry| entry.is_pair(previous_hash, target_hash))
        .map(|entry| entry.occurrence)
        .max()
        .map_or(1, |latest| latest + 1);
    let name = record_name(sequence, previous_hash, target_hash, occurrence);
    let file_name = std::ffi::OsStr::new(&name);
    let mut record = HeadAdvanceRecord {
        schema: HEAD_ADVANCE_SCHEMA,
        kind: HEAD_ADVANCE_KIND.to_owned(),
        ref_path: ref_path.to_owned(),
        previous_hash: previous_hash.map(str::to_owned),
        target_hash: target_hash.to_owned(),
        sequence,
        occurrence,
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
    /// `false` when the swap after this record never happened: the next
    /// record in sequence observed the same head ref this one did, or, for
    /// the latest record, the head ref is still the one it observed.
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
    let all = entries(head_directory)?;
    let current_head = crate::refs::read_verified_ref_in_directory(
        head_directory,
        std::ffi::OsStr::new("head"),
        trust_store,
    )?
    .map(|head| head.signature);
    let mut records = Vec::new();
    for (index, entry) in all.iter().enumerate() {
        if !entry.is_pair(previous_hash, target_hash) {
            continue;
        }
        let record = read_entry(head_directory, entry, trust_store)?;
        let next_observed = match all.get(index + 1) {
            Some(next) => read_entry(head_directory, next, trust_store)?.observed_head,
            None => current_head.clone(),
        };
        let applied = next_observed != record.observed_head;
        records.push(ReadHeadAdvance { record, applied });
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_names_key_each_occurrence_of_an_advance() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        assert_eq!(
            record_name(1, None, &a, 1),
            format!("advance-1-genesis-{a}-1")
        );
        assert_eq!(
            record_name(7, Some(&a), &b, 3),
            format!("advance-7-{a}-{b}-3")
        );
        let entry = RecordEntry::parse(&format!("advance-7-{a}-{b}-3")).unwrap();
        assert_eq!(
            entry,
            RecordEntry {
                sequence: 7,
                previous_hash: Some(a.clone()),
                target_hash: b.clone(),
                occurrence: 3,
            }
        );
        assert_eq!(entry.name(), format!("advance-7-{a}-{b}-3"));
        for malformed in [
            format!("advance-0-{a}-{b}-3"),
            format!("advance-07-{a}-{b}-3"),
            format!("advance-7-{a}-{b}-"),
            format!("advance-7-other-{b}-1"),
            format!("advance-{a}-{b}-3"),
        ] {
            assert!(RecordEntry::parse(&malformed).is_none(), "{malformed}");
        }
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
