//! A directory of node-signed, hash-chained entries with count-based
//! rotation into a node-signed checkpoint.
//!
//! Two ledgers keep their entries this way: the offline-operation ledger and
//! the node journal. What they share lives here: the file layout (one
//! `<sequence:020>.json` per entry, plus `checkpoint.json`), the verified walk
//! of the retained chain, the no-overwrite durable write, and rotation. What
//! each entry says, and what is derived from the walk, stays with the ledger.

use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use lillux::crypto::VerifyingKey;
use ryeos_node_runtime::identity::NodeIdentity;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::offline_operation::contract::{CheckpointBody, SignedCheckpoint};

const CHECKPOINT_FILE: &str = "checkpoint.json";

/// One kind of chained, node-signed entry and the bounds its ledger keeps.
pub trait ChainEntry: Serialize + DeserializeOwned + Sized {
    /// What an entry is called in errors ("receipt").
    const NOUN: &'static str;
    /// What the ledger is called in errors.
    const LEDGER_NAME: &'static str;
    /// Schema string the body of each entry must carry.
    const SCHEMA: &'static str;
    /// Schema string the ledger's checkpoint must carry.
    const CHECKPOINT_SCHEMA: &'static str;
    /// Largest entry or checkpoint file read.
    const MAX_BYTES: u64;
    /// Entries kept before rotation drops the oldest into a checkpoint.
    const ROTATE_AT: usize;
    /// Entries kept after a rotation.
    const KEEP: usize;

    /// The head of a ledger with no entries for `node_fingerprint`.
    fn genesis_head(node_fingerprint: &str) -> String;
    fn sequence(&self) -> u64;
    fn previous(&self) -> &str;
    fn node_fingerprint(&self) -> &str;
    fn schema(&self) -> &str;
    fn recorded_at(&self) -> &str;
    /// Digest of the signed body: the value the next entry carries as its
    /// `previous`.
    fn digest(&self) -> Result<String>;
    /// Verify the signature (and any shape rule) of this entry.
    fn verify(&self, key: &VerifyingKey) -> Result<()>;
}

/// What a verified walk of the retained chain found, independent of what the
/// entries mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainTip {
    /// Digest of the last entry (or the checkpoint's tip, or the genesis).
    pub head: String,
    /// The head the walk started from: the genesis, or the checkpoint's tip.
    pub start_head: String,
    pub last_sequence: u64,
    pub retained: usize,
    pub dropped: u64,
    /// When the oldest retained entry was recorded.
    pub oldest_recorded_at: Option<String>,
    /// When the checkpoint standing for the dropped entries was written.
    pub checkpoint_recorded_at: Option<String>,
}

pub struct Chain<'a, E: ChainEntry> {
    dir: PathBuf,
    node: &'a NodeIdentity,
    entry: PhantomData<E>,
}

fn entry_name(sequence: u64) -> String {
    format!("{sequence:020}.json")
}

impl<'a, E: ChainEntry> Chain<'a, E> {
    pub fn new(dir: PathBuf, node: &'a NodeIdentity) -> Self {
        Self {
            dir,
            node,
            entry: PhantomData,
        }
    }

    fn checkpoint(&self) -> Result<Option<SignedCheckpoint>> {
        let path = self.dir.join(CHECKPOINT_FILE);
        let Some(bytes) =
            lillux::read_optional_regular_file_bounded_no_follow(&path, E::MAX_BYTES)?
        else {
            return Ok(None);
        };
        let checkpoint: SignedCheckpoint =
            serde_json::from_slice(&bytes).context("parse ledger checkpoint")?;
        checkpoint.verify(self.node.verifying_key())?;
        ensure!(
            checkpoint.body.schema == E::CHECKPOINT_SCHEMA
                && checkpoint.body.node_fingerprint == self.node.fingerprint(),
            "ledger checkpoint is not this node's"
        );
        Ok(Some(checkpoint))
    }

    /// The sequences of the entries present. Names that are not entries are
    /// ignored (a stray file does not stop the node's repair); a name that is
    /// an entry's but is not a regular file is an error naming it, and a
    /// directory too full to list whole is an error, since a partial list
    /// would read as a broken chain.
    pub fn sequences(&self) -> Result<Vec<u64>> {
        let limit = E::ROTATE_AT * 4;
        let names = bounded_names(&self.dir, limit)
            .with_context(|| format!("list the {} {}", E::LEDGER_NAME, self.dir.display()))?;
        ensure!(
            !names.truncated,
            "the {} {} holds more than {limit} entries",
            E::LEDGER_NAME,
            self.dir.display(),
        );
        let mut sequences = Vec::new();
        for (name, regular) in names.entries {
            let Some(stem) = name
                .strip_suffix(".json")
                .filter(|stem| stem.len() == 20 && stem.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            ensure!(
                regular,
                "ledger entry {} is not a regular file",
                self.dir.join(&name).display()
            );
            sequences.push(stem.parse::<u64>()?);
        }
        sequences.sort_unstable();
        Ok(sequences)
    }

    pub fn read_entry(&self, sequence: u64) -> Result<E> {
        let path = self.dir.join(entry_name(sequence));
        let bytes = lillux::read_regular_file_bounded_no_follow(&path, E::MAX_BYTES)?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {} {}", E::NOUN, path.display()))
    }

    /// Read and verify the whole retained chain: signatures, sequence, links,
    /// and the node each entry names. `visit` sees each verified entry with
    /// its digest, oldest first.
    pub fn walk(&self, mut visit: impl FnMut(&E, &str)) -> Result<ChainTip> {
        let checkpoint = self.checkpoint()?;
        let (start_head, mut last_sequence, dropped) = match &checkpoint {
            Some(checkpoint) => (
                checkpoint.body.tip.clone(),
                checkpoint.body.sequence,
                checkpoint.body.dropped_receipts,
            ),
            None => (E::genesis_head(self.node.fingerprint()), 0, 0),
        };
        let mut head = start_head.clone();
        let mut retained = 0;
        let mut oldest_recorded_at = None;
        for sequence in self.sequences()? {
            if sequence <= last_sequence {
                // Already covered by the checkpoint: a rotation that died
                // before removing it.
                continue;
            }
            ensure!(
                sequence == last_sequence + 1,
                "ledger skips from {} {last_sequence} to {sequence}",
                E::NOUN
            );
            let entry = self.read_entry(sequence)?;
            entry.verify(self.node.verifying_key())?;
            ensure!(
                entry.schema() == E::SCHEMA
                    && entry.sequence() == sequence
                    && entry.node_fingerprint() == self.node.fingerprint(),
                "{} {sequence} is not this ledger's",
                E::NOUN
            );
            ensure!(
                entry.previous() == head,
                "{} {sequence} does not continue the chain",
                E::NOUN
            );
            if oldest_recorded_at.is_none() {
                oldest_recorded_at = Some(entry.recorded_at().to_owned());
            }
            head = entry.digest()?;
            visit(&entry, &head);
            last_sequence = sequence;
            retained += 1;
        }
        Ok(ChainTip {
            head,
            start_head,
            last_sequence,
            retained,
            dropped,
            oldest_recorded_at,
            checkpoint_recorded_at: checkpoint.map(|checkpoint| checkpoint.body.recorded_at),
        })
    }

    /// The newest `limit` retained entries up to `last_sequence`, oldest
    /// first.
    pub fn recent(&self, last_sequence: u64, limit: usize) -> Result<Vec<E>> {
        let sequences: Vec<u64> = self
            .sequences()?
            .into_iter()
            .filter(|sequence| *sequence <= last_sequence)
            .collect();
        let start = sequences.len().saturating_sub(limit);
        sequences[start..]
            .iter()
            .map(|sequence| self.read_entry(*sequence))
            .collect()
    }

    /// Durably write `entry` as `sequence`, refusing to overwrite.
    pub fn write_entry(&self, sequence: u64, entry: &E) -> Result<()> {
        let path = self.dir.join(entry_name(sequence));
        ensure!(
            lillux::inspect_optional_entry_no_follow(&path)?.is_none(),
            "{} {sequence} already exists; the ledger will not overwrite it",
            E::NOUN
        );
        write_private(&path, &serde_json::to_vec(entry)?)
    }

    /// Drop the oldest entries into a signed checkpoint that carries the
    /// chain tip forward. The checkpoint is durable before anything is
    /// removed, so a death in between leaves a ledger that still verifies.
    pub fn rotate(&self) -> Result<()> {
        let sequences = self.sequences()?;
        let checkpoint = self.checkpoint()?;
        let covered = checkpoint.as_ref().map_or(0, |c| c.body.sequence);
        let live: Vec<u64> = sequences.into_iter().filter(|s| *s > covered).collect();
        if live.len() <= E::KEEP {
            return Ok(());
        }
        let drop_through = live[live.len() - E::KEEP - 1];
        let tip = self.read_entry(drop_through)?.digest()?;
        let dropped_now = live.iter().filter(|s| **s <= drop_through).count() as u64;
        let signed = SignedCheckpoint::sign(
            CheckpointBody {
                schema: E::CHECKPOINT_SCHEMA.to_owned(),
                node_fingerprint: self.node.fingerprint().to_owned(),
                sequence: drop_through,
                tip,
                dropped_receipts: checkpoint.as_ref().map_or(0, |c| c.body.dropped_receipts)
                    + dropped_now,
                recorded_at: lillux::time::iso8601_now(),
            },
            self.node.signing_key(),
        )?;
        write_private(
            &self.dir.join(CHECKPOINT_FILE),
            &serde_json::to_vec(&signed)?,
        )?;
        for sequence in live.into_iter().filter(|s| *s <= drop_through) {
            remove(&self.dir.join(entry_name(sequence)))?;
        }
        Ok(())
    }
}

/// What a bounded listing of one directory found.
pub(crate) struct Names {
    /// UTF-8 names with whether each is a regular file (links are not).
    pub entries: Vec<(String, bool)>,
    /// More entries than the bound exist; the listing is the first of them.
    pub truncated: bool,
}

/// List up to `limit` entries without ever failing for having more. A
/// missing directory has none.
pub(crate) fn bounded_names(directory: &Path, limit: usize) -> Result<Names> {
    let reader = match std::fs::read_dir(directory) {
        Ok(reader) => reader,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Names {
                entries: Vec::new(),
                truncated: false,
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("list {}", directory.display()));
        }
    };
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in reader {
        let entry = entry.with_context(|| format!("list {}", directory.display()))?;
        if entries.len() >= limit {
            truncated = true;
            break;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let regular = entry
            .file_type()
            .with_context(|| format!("inspect {}", entry.path().display()))?
            .is_file();
        entries.push((name, regular));
    }
    entries.sort();
    Ok(Names { entries, truncated })
}

/// Durably write a file owner-private, replacing any earlier content.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    lillux::atomic_write_private(path, bytes)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("write {}", path.display()))
}

/// Durably remove a file; absent is already removed.
pub(crate) fn remove(path: &Path) -> Result<()> {
    lillux::remove_file_durable(path)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("remove {}", path.display()))
}
