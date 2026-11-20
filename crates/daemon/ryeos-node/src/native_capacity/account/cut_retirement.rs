//! The native-capacity step of the execution-history schema cut.
//!
//! A capacity debit or recovery record written by a daemon that was killed
//! before pid namespaces were recorded embeds an exact process identity
//! without `pid_namespace`. Identities now require the field, so that record
//! cannot decode, and nothing else would clear it: the capacity files live
//! outside the runtime database the history reset retires.
//!
//! The cut re-records each such identity, once, as one recorded in an
//! unknown pid namespace ([`PRE_CUT_PID_NAMESPACE`]). That is the truth about
//! it: no observer can compare its numbers. The incarnation digest ignores
//! the namespace, so every digest the records carry still matches. The next
//! start then settles the dead incarnation through the ordinary machinery
//! under the exclusive state lock, where a foreign-namespace incarnation
//! counts as ended: the same recovery charge, the same
//! `DeadIncarnationSettlement`, the same epoch and debit sequence, and the
//! same consumption (a settlement never refunds).
//!
//! This is not a reader of the old shape. It accepts exactly one defect: an
//! identity that is canonical JSON of the predecessor fields and whose only
//! absence is `pid_namespace`, inside a record whose checksum holds and whose
//! carried process digest it reproduces. Any other record that does not
//! decode is refused.

use super::redo::{
    ANCHOR_BYTES, ANCHOR_PAYLOAD_BYTES, BEFORE_START, MAGIC, PROCESS_STORAGE_BYTES,
    RECOVERY_PROCESS_END, RECOVERY_PROCESS_START, SCHEMA, StoredProcess, TARGET_START,
};
use super::{PAYLOAD_BYTES, PROCESS_END, PROCESS_START, RECORD_BYTES, RECORD_MAGIC};
use anyhow::{Context, Result, ensure};
use lillux::process_control::ExactProcessIdentity;
use lillux::time::MonotonicDeadline;
use ryeos_node_runtime::state_lock::StateLock;
use sha2::Digest as _;
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

/// The namespace a pre-cut identity is re-recorded in. No process lives in
/// namespace inode 0, so no observer ever matches it.
pub(crate) const PRE_CUT_PID_NAMESPACE: &str = "pid:[0]";

const MISSING_FIELD: &str = "missing field `pid_namespace`";

/// What the cut step did to the capacity files.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct NativeCapacityCutReport {
    /// The redo anchor carried pre-cut identities and was re-recorded.
    pub anchor_rewritten: bool,
    /// The account record carried a pre-cut identity and was re-recorded.
    pub account_rewritten: bool,
}

impl NativeCapacityCutReport {
    pub fn is_empty(&self) -> bool {
        !self.anchor_rewritten && !self.account_rewritten
    }

    /// The operator-visible line for the cut report.
    pub fn summary(&self) -> String {
        if self.is_empty() {
            return "none".to_owned();
        }
        let files = match (self.anchor_rewritten, self.account_rewritten) {
            (true, true) => "redo anchor and account",
            (true, false) => "redo anchor",
            _ => "account",
        };
        format!(
            "{files} re-recorded in an unknown pid namespace; the next start settles the dead incarnation"
        )
    }
}

/// The identity as predecessor builds recorded it.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PreCutIdentity {
    boot_id: String,
    target_pid: u32,
    target_start_time_ticks: u64,
    group_leader_pid: u32,
    group_leader_start_time_ticks: u64,
    #[serde(default, skip_serializing_if = "lillux::GroupBasis::is_process_group")]
    group_basis: lillux::GroupBasis,
}

/// Re-record one stored identity slot (`u16` length then the fixed payload)
/// if, and only if, it is exactly the pre-cut shape. `Ok(false)` means it is
/// already current. `expected_digest` is the process digest the enclosing
/// record carries, when it carries one.
fn upgrade_process_slot(slot: &mut [u8], expected_digest: Option<[u8; 32]>) -> Result<bool> {
    ensure!(
        slot.len() == PROCESS_STORAGE_BYTES,
        "capacity process layout changed"
    );
    let length = usize::from(u16::from_le_bytes([slot[0], slot[1]]));
    ensure!(
        length > 0 && length <= PROCESS_STORAGE_BYTES - 2,
        "capacity process identity length is invalid"
    );
    ensure!(
        slot[2 + length..].iter().all(|byte| *byte == 0),
        "capacity process identity padding is nonzero"
    );
    let json = slot[2..2 + length].to_vec();
    match serde_json::from_slice::<ExactProcessIdentity>(&json) {
        Ok(_) => return Ok(false),
        Err(error) => ensure!(
            error.to_string().contains(MISSING_FIELD),
            "capacity process identity does not decode and is not the pre-cut shape: {error}"
        ),
    }
    let old: PreCutIdentity = serde_json::from_slice(&json)
        .context("capacity process identity is not the exact pre-cut shape")?;
    ensure!(
        serde_json::to_vec(&old)? == json,
        "pre-cut capacity process identity is not canonically encoded"
    );
    let upgraded = ExactProcessIdentity {
        boot_id: old.boot_id,
        target_pid: old.target_pid,
        target_start_time_ticks: old.target_start_time_ticks,
        group_leader_pid: old.group_leader_pid,
        group_leader_start_time_ticks: old.group_leader_start_time_ticks,
        group_basis: old.group_basis,
        pid_namespace: PRE_CUT_PID_NAMESPACE.to_owned(),
    };
    let digest = upgraded
        .incarnation_digest_bytes()
        .map_err(anyhow::Error::msg)?;
    if let Some(expected) = expected_digest {
        ensure!(
            digest == expected,
            "pre-cut capacity process identity does not reproduce the digest its record carries"
        );
    }
    StoredProcess::capture(&upgraded)?.encode_into(slot);
    Ok(true)
}

fn record_checksum(bytes: &[u8; RECORD_BYTES]) -> [u8; 32] {
    sha2::Sha256::digest(&bytes[..PAYLOAD_BYTES]).into()
}

/// Re-record the identity of one fixed capacity record, if it is pre-cut.
fn upgrade_record(bytes: &mut [u8; RECORD_BYTES]) -> Result<bool> {
    ensure!(
        &bytes[..8] == RECORD_MAGIC,
        "capacity record has an unknown format"
    );
    ensure!(
        bytes[PAYLOAD_BYTES..] == record_checksum(bytes),
        "capacity record checksum mismatch"
    );
    match bytes[100] {
        0 => return Ok(false),
        1 => {}
        _ => anyhow::bail!("capacity record has an unknown debit tag"),
    }
    let digest: [u8; 32] = bytes[101..133].try_into()?;
    if !upgrade_process_slot(&mut bytes[PROCESS_START..PROCESS_END], Some(digest))? {
        return Ok(false);
    }
    let checksum = record_checksum(bytes);
    bytes[PAYLOAD_BYTES..].copy_from_slice(&checksum);
    super::decode(bytes).map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    Ok(true)
}

fn anchor_checksum(bytes: &[u8; ANCHOR_BYTES]) -> [u8; 32] {
    sha2::Sha256::digest(&bytes[..ANCHOR_PAYLOAD_BYTES]).into()
}

/// Re-record every identity the redo anchor carries (its recovery process
/// and the debit of its before and target images), if any is pre-cut.
fn upgrade_anchor(bytes: &mut [u8; ANCHOR_BYTES]) -> Result<bool> {
    ensure!(
        &bytes[..8] == MAGIC && bytes[8..12] == SCHEMA.to_le_bytes(),
        "capacity redo has an unknown format"
    );
    ensure!(
        bytes[ANCHOR_PAYLOAD_BYTES..] == anchor_checksum(bytes),
        "capacity redo checksum mismatch"
    );
    let mut changed = false;
    match bytes[53] {
        0 => {}
        1 => {
            changed |= upgrade_process_slot(
                &mut bytes[RECOVERY_PROCESS_START..RECOVERY_PROCESS_END],
                None,
            )?
        }
        _ => anyhow::bail!("invalid capacity recovery process tag"),
    }
    for start in [BEFORE_START, TARGET_START] {
        let mut record: [u8; RECORD_BYTES] = bytes[start..start + RECORD_BYTES].try_into()?;
        if upgrade_record(&mut record)? {
            bytes[start..start + RECORD_BYTES].copy_from_slice(&record);
            changed = true;
        }
    }
    if changed {
        let checksum = anchor_checksum(bytes);
        bytes[ANCHOR_PAYLOAD_BYTES..].copy_from_slice(&checksum);
    }
    Ok(changed)
}

/// Whether `bytes` are a redo anchor in exactly the pre-cut shape. Used to
/// name the cure when a start meets one.
pub(super) fn is_pre_cut_anchor(bytes: &[u8; ANCHOR_BYTES]) -> bool {
    let mut copy = *bytes;
    upgrade_anchor(&mut copy).unwrap_or(false)
}

/// The cut step. With the node stopped and its exclusive state lock held,
/// re-record every pre-cut identity in the capacity account and redo anchor.
/// Idempotent: current files are left untouched, and a rerun after an
/// interruption finishes whichever file remains. Refuses any capacity file
/// that does not decode for another reason.
pub fn retire_pre_cut_identity_records(
    app_root: &Path,
    state_lock: &StateLock,
) -> Result<NativeCapacityCutReport> {
    state_lock.ensure_protects_app_root(app_root)?;
    let Some(directory) =
        lillux::PinnedDirectory::open(&app_root.join(".ai/node/native-capacity"))?
    else {
        return Ok(NativeCapacityCutReport::default());
    };
    directory
        .require_owner_private_directory_fixed()
        .map_err(anyhow::Error::msg)?;
    let deadline = MonotonicDeadline::after(Duration::from_secs(60));
    directory.recover_conditional_byte_replacement_atomic(OsStr::new("anchor"))?;
    let entries = directory.entries_no_follow_bounded(8)?;
    ensure!(
        entries.len() == 2
            && entries.iter().all(|entry| matches!(
                entry.name.to_str(),
                Some("account" | "anchor")
            ) && entry.entry_type
                == lillux::PinnedEntryType::Regular),
        "capacity namespace retains unexpected entries; stopped operator resolution required"
    );
    let mut report = NativeCapacityCutReport::default();

    let account = directory
        .open_pinned_regular(OsStr::new("account"), false)?
        .context("capacity account is absent")?;
    account.require_effective_user_owned()?;
    let mut before = [0u8; RECORD_BYTES];
    account.read_exact_stable_into(&mut before, deadline)?;
    let mut after = before;
    if upgrade_record(&mut after)? {
        let mut scratch = [0u8; RECORD_BYTES];
        account.overwrite_fixed_if_matches(&before, &after, &mut scratch, deadline)?;
        account.sync_exact(deadline)?;
        report.account_rewritten = true;
    }

    let anchor = directory
        .open_pinned_regular(OsStr::new("anchor"), false)?
        .context("capacity redo is absent")?;
    anchor.require_effective_user_owned()?;
    let identity = anchor.identity()?;
    let mut before = [0u8; ANCHOR_BYTES];
    anchor.read_exact_stable_into(&mut before, deadline)?;
    let mut after = before;
    if upgrade_anchor(&mut after)? {
        directory.replace_pinned_bytes_if_matches_atomic(
            OsStr::new("anchor"),
            Some(&anchor),
            |incumbent| {
                incumbent.require_effective_user_owned()?;
                ensure!(
                    incumbent.identity()? == identity,
                    "capacity redo incumbent replaced"
                );
                let mut now = [0u8; ANCHOR_BYTES];
                incumbent.read_exact_stable_into(&mut now, deadline)?;
                ensure!(now == before, "capacity redo incumbent bytes changed");
                Ok(())
            },
            &after,
            0o600,
        )?;
        let published = directory
            .open_pinned_regular(OsStr::new("anchor"), false)?
            .context("capacity redo is absent after re-recording")?;
        published.sync_exact(deadline)?;
        directory.sync()?;
        report.anchor_rewritten = true;
    }
    Ok(report)
}

/// Write the predecessor shape for tests: every stored identity loses its
/// `pid_namespace` (then `mutate` may alter the JSON further), exactly as a
/// daemon that predates the field encoded it, with every checksum remade.
#[cfg(test)]
pub(crate) fn downgrade_to_pre_cut_for_test(
    app_root: &Path,
    mutate: &dyn Fn(String) -> String,
) -> Result<()> {
    fn slot(slot: &mut [u8], mutate: &dyn Fn(String) -> String) -> Result<()> {
        let length = usize::from(u16::from_le_bytes([slot[0], slot[1]]));
        let json = String::from_utf8(slot[2..2 + length].to_vec())?;
        let cut = json
            .rfind(",\"pid_namespace\":")
            .context("identity already lacks pid_namespace")?;
        let old = mutate(format!("{}}}", &json[..cut]));
        ensure!(old.len() <= PROCESS_STORAGE_BYTES - 2, "identity too long");
        slot[..2].copy_from_slice(&u16::try_from(old.len())?.to_le_bytes());
        slot[2..].fill(0);
        slot[2..2 + old.len()].copy_from_slice(old.as_bytes());
        Ok(())
    }
    fn record(bytes: &mut [u8; RECORD_BYTES], mutate: &dyn Fn(String) -> String) -> Result<()> {
        if bytes[100] == 1 {
            slot(&mut bytes[PROCESS_START..PROCESS_END], mutate)?;
            let checksum = record_checksum(bytes);
            bytes[PAYLOAD_BYTES..].copy_from_slice(&checksum);
        }
        Ok(())
    }
    let directory = app_root.join(".ai/node/native-capacity");
    let account_path = directory.join("account");
    let mut account: [u8; RECORD_BYTES] = std::fs::read(&account_path)?.try_into().unwrap();
    record(&mut account, mutate)?;
    std::fs::write(&account_path, account)?;
    let anchor_path = directory.join("anchor");
    let mut anchor: [u8; ANCHOR_BYTES] = std::fs::read(&anchor_path)?.try_into().unwrap();
    if anchor[53] == 1 {
        slot(
            &mut anchor[RECOVERY_PROCESS_START..RECOVERY_PROCESS_END],
            mutate,
        )?;
    }
    for start in [BEFORE_START, TARGET_START] {
        let mut part: [u8; RECORD_BYTES] = anchor[start..start + RECORD_BYTES].try_into()?;
        record(&mut part, mutate)?;
        anchor[start..start + RECORD_BYTES].copy_from_slice(&part);
    }
    let checksum = anchor_checksum(&anchor);
    anchor[ANCHOR_PAYLOAD_BYTES..].copy_from_slice(&checksum);
    std::fs::write(&anchor_path, anchor)?;
    Ok(())
}
