//! External monotonic financial anchor for the accounting ledger.
//!
//! `accounting.financial-anchor` is a checksummed, fsynced two-slot file that
//! lives in the runtime state directory but OUTSIDE the SQLite backup/restore
//! unit. It exists for exactly one threat: a same-epoch restore of an older
//! `accounting.sqlite3` that is internally consistent and therefore
//! undetectable from the database alone. Every acknowledged irreversible
//! financial transition advances this anchor before the acknowledgement is
//! returned, so a restored older database is provably older than the anchor.
//!
//! Protocol (plan §6.5):
//! - every read/choose/write/fsync cycle runs under an in-process mutex plus
//!   an exclusive OS file lock held for the process lifetime;
//! - slot selection picks the highest structurally valid slot by
//!   `(slot_generation, financial_high_water)` without consulting SQLite;
//! - fallback to the older slot happens ONLY when the newest slot is torn or
//!   checksum-invalid — never because it disagrees with the database;
//! - advancement refuses any decrease and any same-sequence digest conflict;
//!   advancing through a contiguous chain directly to a higher sequence is
//!   legal (a delayed waiter for `N` observes an anchor already at `N+1`);
//! - the inactive slot is written with `slot_generation + 1`, `fdatasync`ed,
//!   and re-read/verified before success is reported.
//!
//! Accepted residual: the two-slot format cannot distinguish a legitimate
//! torn-write crash from post-fsync corruption of the newest slot. If the
//! acknowledged slot at sequence N+1 is corrupted after fsync AND the
//! database is simultaneously restored to exactly sequence N with a
//! matching digest, verification reports agreement and the acknowledged
//! history at N+1 is silently lost. This double fault requires independent
//! corruption of both stores at coordinated points; defending against it
//! needs an external witness (a third replica or remote anchor), which this
//! design intentionally does not require.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Filename of the anchor inside the runtime state directory.
pub const ACCOUNTING_ANCHOR_FILENAME: &str = "accounting.financial-anchor";

const SLOT_MAGIC: &[u8; 8] = b"RYFANC01";
const SLOT_SIZE: usize = 256;
const SLOT_COUNT: usize = 2;
const SITE_ID_MAX: usize = 64;
const DIGEST_LEN: usize = 64;
// magic(8) + slot_generation(8) + ledger_epoch(8) + financial_high_water(8)
// + site_len(1) + site(64) + chain_digest(64) = 161 checksummed bytes.
const CHECKSUM_OFFSET: usize = 161;
const CHECKSUM_LEN: usize = 32;

/// The genesis financial chain digest for a `(site, epoch)` pair. The ledger
/// row and the anchor both start here so they agree at sequence zero.
pub fn genesis_chain_digest(site_id: &str, epoch: u64) -> String {
    lillux::cas::sha256_hex(format!("ryeos-accounting-genesis/{site_id}/{epoch}").as_bytes())
}

fn is_lower_hex_digest(s: &str) -> bool {
    s.len() == DIGEST_LEN
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One decoded, structurally valid anchor slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorRecord {
    pub slot_generation: u64,
    pub budget_authority_site_id: String,
    pub ledger_epoch: u64,
    pub financial_high_water: u64,
    pub financial_chain_digest: String,
}

/// Outcome of comparing the anchor with the database financial chain head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorAgreement {
    /// Anchor and database agree exactly.
    Agrees,
    /// The database committed transitions whose anchor fsync never completed
    /// (crash between COMMIT and anchor advance). Recoverable: the caller
    /// advances the anchor from the complete immutable database hash chain —
    /// no safety-critical acknowledgement was returned for those sequences.
    DbAhead {
        anchor_sequence: u64,
        /// The digest the anchor acknowledged at `anchor_sequence`. The
        /// caller must prove the database chain contains exactly this digest
        /// at that sequence before advancing — a longer DIVERGENT database
        /// must never replace acknowledged history.
        anchor_digest: String,
    },
    /// The anchor acknowledged sequences the database does not contain: the
    /// database was rolled back. Permanently fail-closed for the epoch.
    AnchorAhead {
        anchor_sequence: u64,
        db_sequence: u64,
    },
    /// Same sequence, different chain digest: divergent history. Permanently
    /// fail-closed for the epoch.
    DigestConflict { sequence: u64 },
    /// No structurally valid slot exists for the active site/epoch.
    /// Permanently fail-closed for the epoch.
    MissingForActiveEpoch,
}

/// The external monotonic financial anchor. Holds an exclusive OS file lock
/// for the process lifetime plus an internal mutex serializing every
/// read/choose/write/fsync cycle.
pub struct AccountingAnchor {
    file: Mutex<File>,
    path: PathBuf,
    site_id: String,
    epoch: u64,
    _lock: lillux::ExclusiveFileLock,
}

fn encode_slot(record: &AnchorRecord) -> Result<[u8; SLOT_SIZE]> {
    let site = record.budget_authority_site_id.as_bytes();
    if site.is_empty() || site.len() > SITE_ID_MAX {
        bail!(
            "anchor site id must be 1..={SITE_ID_MAX} bytes, got {}",
            site.len()
        );
    }
    if !is_lower_hex_digest(&record.financial_chain_digest) {
        bail!(
            "anchor chain digest must be 64 lowercase hex chars: {:?}",
            record.financial_chain_digest
        );
    }
    let mut buf = [0u8; SLOT_SIZE];
    buf[0..8].copy_from_slice(SLOT_MAGIC);
    buf[8..16].copy_from_slice(&record.slot_generation.to_le_bytes());
    buf[16..24].copy_from_slice(&record.ledger_epoch.to_le_bytes());
    buf[24..32].copy_from_slice(&record.financial_high_water.to_le_bytes());
    buf[32] = site.len() as u8;
    buf[33..33 + site.len()].copy_from_slice(site);
    buf[97..97 + DIGEST_LEN].copy_from_slice(record.financial_chain_digest.as_bytes());
    let checksum: [u8; 32] = Sha256::digest(&buf[..CHECKSUM_OFFSET]).into();
    buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN].copy_from_slice(&checksum);
    Ok(buf)
}

/// Decode one slot; `None` means the slot is torn, foreign, or
/// checksum-invalid. Structural validity is judged without consulting SQLite.
fn decode_slot(buf: &[u8; SLOT_SIZE]) -> Option<AnchorRecord> {
    if &buf[0..8] != SLOT_MAGIC {
        return None;
    }
    let expected: [u8; 32] = Sha256::digest(&buf[..CHECKSUM_OFFSET]).into();
    if buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN] != expected[..] {
        return None;
    }
    let site_len = buf[32] as usize;
    if site_len == 0 || site_len > SITE_ID_MAX {
        return None;
    }
    let site = std::str::from_utf8(&buf[33..33 + site_len]).ok()?;
    // Unused site bytes must be zero padding; anything else is torn state
    // that happened to keep a valid checksum prefix (impossible in practice,
    // but the format is strict).
    if buf[33 + site_len..97].iter().any(|b| *b != 0) {
        return None;
    }
    let digest = std::str::from_utf8(&buf[97..97 + DIGEST_LEN]).ok()?;
    if !is_lower_hex_digest(digest) {
        return None;
    }
    let slot_generation = u64::from_le_bytes(buf[8..16].try_into().ok()?);
    let ledger_epoch = u64::from_le_bytes(buf[16..24].try_into().ok()?);
    let financial_high_water = u64::from_le_bytes(buf[24..32].try_into().ok()?);
    Some(AnchorRecord {
        slot_generation,
        budget_authority_site_id: site.to_string(),
        ledger_epoch,
        financial_high_water,
        financial_chain_digest: digest.to_string(),
    })
}

fn read_slot(file: &mut File, index: usize) -> Result<[u8; SLOT_SIZE]> {
    let mut buf = [0u8; SLOT_SIZE];
    file.seek(SeekFrom::Start((index * SLOT_SIZE) as u64))
        .context("seek anchor slot")?;
    file.read_exact(&mut buf).context("read anchor slot")?;
    Ok(buf)
}

fn write_slot(file: &mut File, index: usize, buf: &[u8; SLOT_SIZE]) -> Result<()> {
    file.seek(SeekFrom::Start((index * SLOT_SIZE) as u64))
        .context("seek anchor slot for write")?;
    file.write_all(buf).context("write anchor slot")?;
    Ok(())
}

/// Select the highest structurally valid slot by
/// `(slot_generation, financial_high_water)`. Ties select the lower index
/// deterministically. Returns the slot index alongside the record.
fn select_valid_slot(file: &mut File) -> Result<Option<(usize, AnchorRecord)>> {
    let mut best: Option<(usize, AnchorRecord)> = None;
    for index in 0..SLOT_COUNT {
        let Some(record) = decode_slot(&read_slot(file, index)?) else {
            continue;
        };
        let better = match &best {
            None => true,
            Some((_, current)) => {
                (record.slot_generation, record.financial_high_water)
                    > (current.slot_generation, current.financial_high_water)
            }
        };
        if better {
            best = Some((index, record));
        }
    }
    Ok(best)
}

impl AccountingAnchor {
    /// Open the anchor for `(site_id, epoch)`, creating and fsyncing the file
    /// AND its parent directory on first initialization. Holds an exclusive
    /// OS file lock for the lifetime of the returned value.
    pub fn open_or_init(dir: &Path, site_id: &str, epoch: u64) -> Result<Self> {
        Self::open_with_policy(dir, site_id, epoch, true)
    }

    /// Open the anchor for an established active epoch. A missing anchor here
    /// is NOT recoverable by re-creating genesis — even at sequence zero that
    /// could revive pre-issue holds or old execution allowances — so it fails
    /// closed permanently for the epoch.
    pub fn open_requiring_existing(dir: &Path, site_id: &str, epoch: u64) -> Result<Self> {
        Self::open_with_policy(dir, site_id, epoch, false)
    }

    fn open_with_policy(
        dir: &Path,
        site_id: &str,
        epoch: u64,
        allow_initialize: bool,
    ) -> Result<Self> {
        if site_id.is_empty() || site_id.len() > SITE_ID_MAX {
            bail!("anchor site id must be 1..={SITE_ID_MAX} bytes");
        }
        let path = dir.join(ACCOUNTING_ANCHOR_FILENAME);
        let lock = lillux::ExclusiveFileLock::acquire(&path)
            .with_context(|| format!("lock financial anchor {}", path.display()))?;

        let mut open_existing = OpenOptions::new();
        open_existing.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_existing.custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = match open_existing.open(&path) {
            Ok(file) => {
                let metadata = file
                    .metadata()
                    .with_context(|| format!("stat financial anchor {}", path.display()))?;
                if !metadata.file_type().is_file() {
                    bail!(
                        "financial anchor must be a regular file: {}",
                        path.display()
                    );
                }
                if metadata.len() != (SLOT_COUNT * SLOT_SIZE) as u64 {
                    bail!(
                        "financial anchor has unexpected size {} (expected {}): {}",
                        metadata.len(),
                        SLOT_COUNT * SLOT_SIZE,
                        path.display()
                    );
                }
                file
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !allow_initialize {
                    bail!(
                        "financial anchor is missing for established active epoch {epoch} of \
                         site {site_id}; the epoch is unverifiable and hard admission is \
                         permanently fail-closed: {}",
                        path.display()
                    );
                }
                let mut create = OpenOptions::new();
                create.read(true).write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    create.mode(0o600);
                    create.custom_flags(libc::O_NOFOLLOW);
                }
                let mut file = create
                    .open(&path)
                    .with_context(|| format!("create financial anchor {}", path.display()))?;
                let genesis = AnchorRecord {
                    slot_generation: 1,
                    budget_authority_site_id: site_id.to_string(),
                    ledger_epoch: epoch,
                    financial_high_water: 0,
                    financial_chain_digest: genesis_chain_digest(site_id, epoch),
                };
                let encoded = encode_slot(&genesis)?;
                for index in 0..SLOT_COUNT {
                    write_slot(&mut file, index, &encoded)?;
                }
                file.sync_all()
                    .with_context(|| format!("fsync new financial anchor {}", path.display()))?;
                File::open(dir)
                    .and_then(|d| d.sync_all())
                    .with_context(|| format!("fsync financial anchor parent {}", dir.display()))?;
                file
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("open financial anchor {}", path.display()));
            }
        };

        let Some((_, record)) = select_valid_slot(&mut file)? else {
            bail!(
                "financial anchor has no structurally valid slot: {}",
                path.display()
            );
        };
        if record.budget_authority_site_id != site_id || record.ledger_epoch != epoch {
            bail!(
                "financial anchor belongs to site {} epoch {} but the ledger is site {site_id} \
                 epoch {epoch}; hard admission is fail-closed for this epoch: {}",
                record.budget_authority_site_id,
                record.ledger_epoch,
                path.display()
            );
        }

        Ok(Self {
            file: Mutex::new(file),
            path,
            site_id: site_id.to_string(),
            epoch,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn site_identity(&self) -> (&str, u64) {
        (&self.site_id, self.epoch)
    }

    /// Read the currently authoritative record: the highest structurally
    /// valid slot by `(slot_generation, financial_high_water)`. Falls back
    /// to the older slot only when the newest is torn or checksum-invalid.
    pub fn read_valid(&self) -> Result<AnchorRecord> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("financial anchor mutex poisoned"))?;
        let record = Self::select_for(&mut file, &self.site_id, self.epoch, &self.path)?;
        Ok(record.1)
    }

    fn select_for(
        file: &mut File,
        site_id: &str,
        epoch: u64,
        path: &Path,
    ) -> Result<(usize, AnchorRecord)> {
        let Some((index, record)) = select_valid_slot(file)? else {
            bail!(
                "financial anchor has no structurally valid slot: {}",
                path.display()
            );
        };
        if record.budget_authority_site_id != site_id || record.ledger_epoch != epoch {
            bail!(
                "financial anchor site/epoch mismatch: anchor is {}:{} but ledger is \
                 {site_id}:{epoch}: {}",
                record.budget_authority_site_id,
                record.ledger_epoch,
                path.display()
            );
        }
        Ok((index, record))
    }

    /// Monotonic serialized compare-and-advance. Under the process-lifetime
    /// file lock and the internal mutex: read/validate both slots, refuse any
    /// decrease and any same-sequence digest conflict, write the INACTIVE
    /// slot with `slot_generation + 1`, `fdatasync`, and re-read/verify
    /// before returning. Advancing through a contiguous chain directly to a
    /// higher sequence is legal; an equal sequence with an equal digest is an
    /// idempotent no-op.
    pub fn compare_and_advance(
        &self,
        expected_site: &str,
        expected_epoch: u64,
        target_sequence: u64,
        target_digest: &str,
    ) -> Result<()> {
        if expected_site != self.site_id || expected_epoch != self.epoch {
            bail!(
                "anchor advance for site {expected_site} epoch {expected_epoch} does not match \
                 the opened anchor {}:{}",
                self.site_id,
                self.epoch
            );
        }
        if !is_lower_hex_digest(target_digest) {
            bail!("anchor target digest must be 64 lowercase hex chars");
        }
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("financial anchor mutex poisoned"))?;
        let (active_index, current) =
            Self::select_for(&mut file, &self.site_id, self.epoch, &self.path)?;

        if target_sequence < current.financial_high_water {
            bail!(
                "refusing financial anchor regression: anchor is at sequence {} but the caller \
                 requested {} ({})",
                current.financial_high_water,
                target_sequence,
                self.path.display()
            );
        }
        if target_sequence == current.financial_high_water {
            if target_digest != current.financial_chain_digest {
                bail!(
                    "financial anchor digest conflict at sequence {}: anchor {} caller {} ({})",
                    target_sequence,
                    current.financial_chain_digest,
                    target_digest,
                    self.path.display()
                );
            }
            // Idempotent: a delayed waiter observed a higher-or-equal valid
            // anchor and never overwrites it.
            return Ok(());
        }

        let next = AnchorRecord {
            slot_generation: current
                .slot_generation
                .checked_add(1)
                .context("financial anchor slot generation overflow")?,
            budget_authority_site_id: self.site_id.clone(),
            ledger_epoch: self.epoch,
            financial_high_water: target_sequence,
            financial_chain_digest: target_digest.to_string(),
        };
        let encoded = encode_slot(&next)?;
        let inactive_index = 1 - active_index;
        write_slot(&mut file, inactive_index, &encoded)?;
        file.sync_data()
            .with_context(|| format!("fdatasync financial anchor {}", self.path.display()))?;

        // Re-read and verify the durable slot before waking acknowledgements.
        let reread = decode_slot(&read_slot(&mut file, inactive_index)?).ok_or_else(|| {
            anyhow::anyhow!(
                "financial anchor slot verification failed after write: {}",
                self.path.display()
            )
        })?;
        if reread != next {
            bail!(
                "financial anchor readback mismatch after advance: {}",
                self.path.display()
            );
        }
        Ok(())
    }

    /// Compare the independently selected anchor with the database chain
    /// head. `db_chain_digest` of `None` means the database is at genesis.
    /// The anchor slot is selected without consulting SQLite; disagreement
    /// is classified, never repaired here.
    pub fn verify_against_db(
        &self,
        db_high_water: u64,
        db_chain_digest: Option<&str>,
    ) -> Result<AnchorAgreement> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| anyhow::anyhow!("financial anchor mutex poisoned"))?;
        let record = match select_valid_slot(&mut file)? {
            Some((_, record))
                if record.budget_authority_site_id == self.site_id
                    && record.ledger_epoch == self.epoch =>
            {
                record
            }
            _ => return Ok(AnchorAgreement::MissingForActiveEpoch),
        };
        let genesis = genesis_chain_digest(&self.site_id, self.epoch);
        let db_digest = db_chain_digest.unwrap_or(genesis.as_str());
        if record.financial_high_water == db_high_water {
            if record.financial_chain_digest == db_digest {
                Ok(AnchorAgreement::Agrees)
            } else {
                Ok(AnchorAgreement::DigestConflict {
                    sequence: db_high_water,
                })
            }
        } else if record.financial_high_water < db_high_water {
            Ok(AnchorAgreement::DbAhead {
                anchor_sequence: record.financial_high_water,
                anchor_digest: record.financial_chain_digest,
            })
        } else {
            Ok(AnchorAgreement::AnchorAhead {
                anchor_sequence: record.financial_high_water,
                db_sequence: db_high_water,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: &str = "S-test0000000001";
    const EPOCH: u64 = 1;

    fn digest_of(tag: &str) -> String {
        lillux::cas::sha256_hex(tag.as_bytes())
    }

    fn open(dir: &Path) -> AccountingAnchor {
        AccountingAnchor::open_or_init(dir, SITE, EPOCH).unwrap()
    }

    #[test]
    fn slot_round_trip() {
        let record = AnchorRecord {
            slot_generation: 7,
            budget_authority_site_id: SITE.to_string(),
            ledger_epoch: 3,
            financial_high_water: 42,
            financial_chain_digest: digest_of("chain"),
        };
        let encoded = encode_slot(&record).unwrap();
        assert_eq!(decode_slot(&encoded).unwrap(), record);
    }

    #[test]
    fn init_creates_genesis_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let anchor = open(dir.path());
            let record = anchor.read_valid().unwrap();
            assert_eq!(record.financial_high_water, 0);
            assert_eq!(
                record.financial_chain_digest,
                genesis_chain_digest(SITE, EPOCH)
            );
            anchor
                .compare_and_advance(SITE, EPOCH, 1, &digest_of("c1"))
                .unwrap();
        }
        let anchor = open(dir.path());
        let record = anchor.read_valid().unwrap();
        assert_eq!(record.financial_high_water, 1);
        assert_eq!(record.financial_chain_digest, digest_of("c1"));
    }

    #[test]
    fn checksum_validation_rejects_corrupt_slot() {
        let record = AnchorRecord {
            slot_generation: 1,
            budget_authority_site_id: SITE.to_string(),
            ledger_epoch: 1,
            financial_high_water: 5,
            financial_chain_digest: digest_of("x"),
        };
        let mut encoded = encode_slot(&record).unwrap();
        encoded[24] ^= 0xff; // corrupt the high-water field
        assert!(decode_slot(&encoded).is_none());
        let mut torn = encode_slot(&record).unwrap();
        torn[CHECKSUM_OFFSET] ^= 0x01; // corrupt the checksum itself
        assert!(decode_slot(&torn).is_none());
    }

    #[test]
    fn torn_newest_slot_falls_back_to_older_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = {
            let anchor = open(dir.path());
            anchor
                .compare_and_advance(SITE, EPOCH, 3, &digest_of("c3"))
                .unwrap();
            anchor.path().to_path_buf()
        };
        // Find and corrupt the newest slot (the one carrying high water 3).
        let mut bytes = std::fs::read(&path).unwrap();
        let mut newest: Option<usize> = None;
        for index in 0..SLOT_COUNT {
            let slot: [u8; SLOT_SIZE] = bytes[index * SLOT_SIZE..(index + 1) * SLOT_SIZE]
                .try_into()
                .unwrap();
            if let Some(record) = decode_slot(&slot) {
                if record.financial_high_water == 3 {
                    newest = Some(index);
                }
            }
        }
        let newest = newest.expect("advanced slot present");
        bytes[newest * SLOT_SIZE + 30] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let anchor = open(dir.path());
        let record = anchor.read_valid().unwrap();
        assert_eq!(
            record.financial_high_water, 0,
            "fallback must select the older valid slot"
        );
        // Recovery may then advance from the database chain.
        anchor
            .compare_and_advance(SITE, EPOCH, 3, &digest_of("c3"))
            .unwrap();
        assert_eq!(anchor.read_valid().unwrap().financial_high_water, 3);
    }

    #[test]
    fn refuses_regression() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = open(dir.path());
        anchor
            .compare_and_advance(SITE, EPOCH, 5, &digest_of("c5"))
            .unwrap();
        let error = anchor
            .compare_and_advance(SITE, EPOCH, 3, &digest_of("c3"))
            .unwrap_err();
        assert!(format!("{error:#}").contains("regression"));
        assert_eq!(anchor.read_valid().unwrap().financial_high_water, 5);
    }

    #[test]
    fn refuses_same_sequence_digest_conflict_and_accepts_idempotent_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = open(dir.path());
        anchor
            .compare_and_advance(SITE, EPOCH, 2, &digest_of("c2"))
            .unwrap();
        // Exact repeat is an idempotent no-op.
        anchor
            .compare_and_advance(SITE, EPOCH, 2, &digest_of("c2"))
            .unwrap();
        let error = anchor
            .compare_and_advance(SITE, EPOCH, 2, &digest_of("other"))
            .unwrap_err();
        assert!(format!("{error:#}").contains("digest conflict"));
    }

    #[test]
    fn advances_through_contiguous_chain_directly() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = open(dir.path());
        // Callbacks for sequences 1 and 2 both committed; whichever acquires
        // the lock advances directly to 2.
        anchor
            .compare_and_advance(SITE, EPOCH, 2, &digest_of("c2"))
            .unwrap();
        assert_eq!(anchor.read_valid().unwrap().financial_high_water, 2);
        // The delayed sequence-1 waiter observes the higher anchor and must
        // not overwrite it (regression refusal).
        assert!(anchor
            .compare_and_advance(SITE, EPOCH, 1, &digest_of("c1"))
            .is_err());
    }

    #[test]
    fn verify_against_db_classifies_agreement() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = open(dir.path());
        assert_eq!(
            anchor.verify_against_db(0, None).unwrap(),
            AnchorAgreement::Agrees
        );
        // Database committed sequence 1 before the anchor advanced: DbAhead.
        assert_eq!(
            anchor.verify_against_db(1, Some(&digest_of("c1"))).unwrap(),
            AnchorAgreement::DbAhead {
                anchor_sequence: 0,
                anchor_digest: genesis_chain_digest(SITE, EPOCH),
            }
        );
        anchor
            .compare_and_advance(SITE, EPOCH, 2, &digest_of("c2"))
            .unwrap();
        assert_eq!(
            anchor.verify_against_db(2, Some(&digest_of("c2"))).unwrap(),
            AnchorAgreement::Agrees
        );
        assert_eq!(
            anchor
                .verify_against_db(2, Some(&digest_of("divergent")))
                .unwrap(),
            AnchorAgreement::DigestConflict { sequence: 2 }
        );
        // A database behind the anchor is a rollback: fail closed.
        assert_eq!(
            anchor.verify_against_db(1, Some(&digest_of("c1"))).unwrap(),
            AnchorAgreement::AnchorAhead {
                anchor_sequence: 2,
                db_sequence: 1
            }
        );
    }

    #[test]
    fn wrong_site_or_epoch_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        drop(open(dir.path()));
        assert!(AccountingAnchor::open_or_init(dir.path(), "S-other", EPOCH).is_err());
        assert!(AccountingAnchor::open_or_init(dir.path(), SITE, 2).is_err());
    }
}
