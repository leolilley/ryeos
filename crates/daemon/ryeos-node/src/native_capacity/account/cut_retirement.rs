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

/// The cure a refusal names, shared by every start-path refusal that meets a
/// pre-cut capacity file.
pub(super) const PRE_CUT_CURE: &str = "this capacity record was written by a daemon that predates recorded pid namespaces; with the node stopped, run `ryeos node reset execution-history --confirm --schema-cut-from <previous epoch> --schema-cut-to <current epoch>` once to re-record it, then start the node";

/// Inverse of the one re-recording this step makes, so a half-written account
/// can be recognised against the image it was being rewritten from. Only an
/// identity in [`PRE_CUT_PID_NAMESPACE`] has a predecessor image; any other
/// record is returned unchanged.
fn downgrade_record(new: &[u8; RECORD_BYTES]) -> [u8; RECORD_BYTES] {
    let mut out = *new;
    if out[100] != 1 {
        return out;
    }
    let slot = &mut out[PROCESS_START..PROCESS_END];
    let length = usize::from(u16::from_le_bytes([slot[0], slot[1]]));
    if length == 0 || length > PROCESS_STORAGE_BYTES - 2 {
        return *new;
    }
    let Ok(json) = std::str::from_utf8(&slot[2..2 + length]) else {
        return *new;
    };
    let suffix = format!(",\"pid_namespace\":\"{PRE_CUT_PID_NAMESPACE}\"}}");
    let Some(old) = json.strip_suffix(&suffix) else {
        return *new;
    };
    let old = format!("{old}}}");
    slot[..2].copy_from_slice(&(old.len() as u16).to_le_bytes());
    slot[2..].fill(0);
    slot[2..2 + old.len()].copy_from_slice(old.as_bytes());
    let checksum = record_checksum(&out);
    out[PAYLOAD_BYTES..].copy_from_slice(&checksum);
    out
}

/// What the account must be, given the anchor's target image `target`: the
/// image itself, its pre-cut predecessor image, or a torn mixture of the two
/// left by a rewrite that died partway. `None` means it is none of those.
#[derive(Debug, PartialEq, Eq)]
enum AccountState {
    /// Already decodable in the current shape: nothing for the cut to do.
    Current,
    /// The account holds the pre-cut image (or a tear toward the current one)
    /// and must be rewritten to `target`.
    AwaitsCut,
}

fn classify_account(
    account: &[u8; RECORD_BYTES],
    target_old: &[u8; RECORD_BYTES],
    target_new: &[u8; RECORD_BYTES],
) -> Result<AccountState> {
    let mut copy = *account;
    match upgrade_record(&mut copy) {
        Ok(false) => Ok(AccountState::Current),
        Ok(true) => Ok(AccountState::AwaitsCut),
        Err(error) => {
            // Not a whole pre-cut record. It is still ours when it is exactly
            // a prefix of the image being written followed by the suffix of
            // the pre-cut image it replaces: the in-place rewrite's own tear.
            if target_old != target_new
                && super::settlement_transition_bytes(account, target_old, target_new, true)
            {
                return Ok(AccountState::AwaitsCut);
            }
            Err(error)
        }
    }
}

/// Whether the account is waiting for the cut's account rewrite, judged
/// against the anchor's target image. Start uses it to name the cure instead
/// of reporting a generic corrupt account.
pub(super) fn account_awaits_cut(
    account: &[u8; RECORD_BYTES],
    anchor_target: &[u8; RECORD_BYTES],
) -> bool {
    let target_old = downgrade_record(anchor_target);
    matches!(
        classify_account(account, &target_old, anchor_target),
        Ok(AccountState::AwaitsCut)
    ) && account != anchor_target
}

/// The points at which the cut can be interrupted, for the fault parameter of
/// [`retire_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CutStep {
    /// Everything is planned and checked; nothing has been written.
    Planned,
    /// The anchor carries the re-recorded identities; the account is untouched.
    AnchorReplaced,
    /// Both files are final.
    Done,
}

/// The cut step. With the node stopped and its exclusive state lock held,
/// re-record every pre-cut identity in the capacity redo anchor and then the
/// account. The anchor goes FIRST, because once it is current-shaped the
/// account is derived from it: the anchor's target image is what the account
/// must hold, and its pre-cut form is recomputable, so a rewrite that dies
/// anywhere (before the anchor, between the two files, or inside the
/// account's in-place write) is recognised and finished by a rerun, and a
/// start that meets the in-between state names this command as its cure.
///
/// Both files are checked before the first write, so a capacity file that is
/// not the pre-cut shape is refused with nothing changed. Idempotent:
/// current files are left untouched.
pub fn retire_pre_cut_identity_records(
    app_root: &Path,
    state_lock: &StateLock,
) -> Result<NativeCapacityCutReport> {
    retire_with(app_root, state_lock, &mut |_| Ok(()))
}

pub(crate) fn retire_with(
    app_root: &Path,
    state_lock: &StateLock,
    fault: &mut dyn FnMut(CutStep) -> Result<()>,
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

    // Read and plan both files before writing either.
    let anchor = directory
        .open_pinned_regular(OsStr::new("anchor"), false)?
        .context("capacity redo is absent")?;
    anchor.require_effective_user_owned()?;
    let anchor_identity = anchor.identity()?;
    let mut anchor_before = [0u8; ANCHOR_BYTES];
    anchor.read_exact_stable_into(&mut anchor_before, deadline)?;
    let mut anchor_after = anchor_before;
    let anchor_changed = upgrade_anchor(&mut anchor_after)?;
    let target_of = |bytes: &[u8; ANCHOR_BYTES]| -> Result<[u8; RECORD_BYTES]> {
        Ok(bytes[TARGET_START..TARGET_START + RECORD_BYTES].try_into()?)
    };
    let target_new = target_of(&anchor_after)?;
    // The image the account was being rewritten from: the old anchor's own
    // target when it is still pre-cut, else its recomputed predecessor.
    let target_old = if anchor_changed {
        target_of(&anchor_before)?
    } else {
        downgrade_record(&target_new)
    };

    // Opened writable: the account is rewritten in place.
    let account = directory
        .open_pinned_regular(OsStr::new("account"), true)?
        .context("capacity account is absent")?;
    account.require_effective_user_owned()?;
    let mut account_before = [0u8; RECORD_BYTES];
    account.read_exact_stable_into(&mut account_before, deadline)?;
    let rewrite_account = match classify_account(&account_before, &target_old, &target_new)? {
        AccountState::Current => false,
        AccountState::AwaitsCut => true,
    };
    fault(CutStep::Planned)?;

    let mut report = NativeCapacityCutReport::default();
    if anchor_changed {
        directory.replace_pinned_bytes_if_matches_atomic(
            OsStr::new("anchor"),
            Some(&anchor),
            |incumbent| {
                incumbent.require_effective_user_owned()?;
                ensure!(
                    incumbent.identity()? == anchor_identity,
                    "capacity redo incumbent replaced"
                );
                let mut now = [0u8; ANCHOR_BYTES];
                incumbent.read_exact_stable_into(&mut now, deadline)?;
                ensure!(
                    now == anchor_before,
                    "capacity redo incumbent bytes changed"
                );
                Ok(())
            },
            &anchor_after,
            0o600,
        )?;
        let published = directory
            .open_pinned_regular(OsStr::new("anchor"), false)?
            .context("capacity redo is absent after re-recording")?;
        published.sync_exact(deadline)?;
        directory.sync()?;
        report.anchor_rewritten = true;
    }
    fault(CutStep::AnchorReplaced)?;

    if rewrite_account {
        // A whole pre-cut record keeps its own image (the redo model already
        // repairs a record at the anchor's before image); only a tear toward
        // the anchor's target is finished to that target.
        let mut converted = account_before;
        let target = if upgrade_record(&mut converted).unwrap_or(false) {
            converted
        } else {
            target_new
        };
        let mut scratch = [0u8; RECORD_BYTES];
        account.overwrite_fixed_if_matches(&account_before, &target, &mut scratch, deadline)?;
        account.sync_exact(deadline)?;
        report.account_rewritten = true;
    }
    fault(CutStep::Done)?;
    Ok(report)
}

#[cfg(test)]
pub(crate) fn retire_with_fault_for_test(
    app_root: &Path,
    state_lock: &StateLock,
    fault: &mut dyn FnMut(CutStep) -> Result<()>,
) -> Result<NativeCapacityCutReport> {
    retire_with(app_root, state_lock, fault)
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

#[cfg(test)]
mod interruption_tests {
    use super::super::redo::{AccountRedo, Operation, controls};
    use super::*;
    use ryeos_node_runtime::state_lock::{StateLock, default_lock_path};

    struct Node {
        _temporary: tempfile::TempDir,
        root: std::path::PathBuf,
        lock: StateLock,
        policy:
            ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy,
        issuance: [u8; 32],
    }

    impl Node {
        /// A capacity directory holding a debit's redo anchor and account,
        /// as a killed daemon of the previous schema left them.
        fn pre_cut() -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().to_path_buf();
            let directory = root.join(".ai/node/native-capacity");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            let (policy, genesis, debit_target) = controls::fixture();
            let debit = genesis
                .successor(Operation::Debit, debit_target, &policy)
                .unwrap();
            for (name, bytes) in [
                ("anchor", debit.encode(&policy).unwrap().to_vec()),
                (
                    "account",
                    super::super::encode(debit.target()).unwrap().to_vec(),
                ),
            ] {
                let path = directory.join(name);
                std::fs::write(&path, bytes).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let lock = StateLock::acquire(&default_lock_path(&root)).unwrap();
            let issuance = debit.target().issuance_digest;
            downgrade_to_pre_cut_for_test(&root, &|json| json).unwrap();
            Self {
                _temporary: temporary,
                root,
                lock,
                policy,
                issuance,
            }
        }

        fn files(&self) -> (Vec<u8>, Vec<u8>) {
            let directory = self.root.join(".ai/node/native-capacity");
            (
                std::fs::read(directory.join("anchor")).unwrap(),
                std::fs::read(directory.join("account")).unwrap(),
            )
        }

        fn write_account(&self, bytes: &[u8]) {
            std::fs::write(self.root.join(".ai/node/native-capacity/account"), bytes).unwrap();
        }

        fn cut(&self) -> Result<NativeCapacityCutReport> {
            retire_pre_cut_identity_records(&self.root, &self.lock)
        }

        fn cut_failing_at(&self, step: CutStep) -> anyhow::Error {
            retire_with(&self.root, &self.lock, &mut |at| {
                ensure!(at != step, "injected failure at {at:?}");
                Ok(())
            })
            .expect_err("the injected failure interrupts the cut")
        }

        /// The anchor's decoded redo and the account's acceptance by the
        /// existing redo model, which is all a start has to repair with.
        fn start_view(&self) -> (AccountRedo, bool) {
            let (anchor, account) = self.files();
            let redo =
                AccountRedo::decode(&anchor.try_into().unwrap(), &self.policy, &self.issuance)
                    .unwrap();
            let accepted = redo
                .accepts_account_bytes(&account.try_into().unwrap())
                .unwrap();
            (redo, accepted)
        }
    }

    /// The final files of an uninterrupted cut over the same fixture.
    fn uninterrupted() -> (Vec<u8>, Vec<u8>) {
        let node = Node::pre_cut();
        let report = node.cut().unwrap();
        assert_eq!(
            report,
            NativeCapacityCutReport {
                anchor_rewritten: true,
                account_rewritten: true
            }
        );
        node.files()
    }

    #[test]
    fn an_uninterrupted_cut_leaves_decodable_files_with_the_identity_in_an_unknown_namespace() {
        let node = Node::pre_cut();
        // Before the cut the anchor does not decode.
        let (anchor, _) = node.files();
        assert!(
            AccountRedo::decode(&anchor.try_into().unwrap(), &node.policy, &node.issuance).is_err()
        );
        node.cut().unwrap();
        let (redo, accepted) = node.start_view();
        assert!(accepted);
        let debit = redo.target().outstanding.expect("the debit is retained");
        assert_eq!(
            debit.process.exact().unwrap().pid_namespace,
            PRE_CUT_PID_NAMESPACE
        );
        // Nothing left to do on a rerun.
        let files = node.files();
        assert!(node.cut().unwrap().is_empty());
        assert_eq!(node.files(), files);
    }

    #[test]
    fn a_cut_interrupted_before_any_write_changes_nothing_and_the_rerun_converges() {
        let reference = uninterrupted();
        let node = Node::pre_cut();
        let before = node.files();
        let error = node.cut_failing_at(CutStep::Planned);
        assert!(format!("{error:#}").contains("injected failure"));
        assert_eq!(node.files(), before);
        node.cut().unwrap();
        assert_eq!(node.files(), reference);
    }

    #[test]
    fn a_cut_interrupted_between_its_two_writes_is_named_to_start_and_the_rerun_converges() {
        let reference = uninterrupted();
        let node = Node::pre_cut();
        node.cut_failing_at(CutStep::AnchorReplaced);
        // The anchor is current, the account is still the pre-cut image.
        let (redo, accepted) = node.start_view();
        // The existing redo model cannot repair that account, so a start
        // would refuse it as corruption; instead it is recognised as waiting
        // for this cut and the refusal names the cure.
        assert!(!accepted);
        let (_, account) = node.files();
        let target = super::super::encode(redo.target()).unwrap();
        assert!(account_awaits_cut(&account.try_into().unwrap(), &target));
        let report = node.cut().unwrap();
        assert_eq!(
            report,
            NativeCapacityCutReport {
                anchor_rewritten: false,
                account_rewritten: true
            }
        );
        assert_eq!(node.files(), reference);
    }

    #[test]
    fn a_cut_that_dies_inside_the_account_write_is_recognised_and_finished() {
        let reference = uninterrupted();
        let node = Node::pre_cut();
        let (_, old_account) = node.files();
        let root = node.root.clone();
        let tear_offset = PROCESS_START + 8;
        let error = retire_with(&node.root, &node.lock, &mut |step| {
            if step == CutStep::AnchorReplaced {
                // What the account looks like when its in-place rewrite
                // stopped partway: a prefix of the target, then the old tail.
                let (anchor, _) = {
                    let directory = root.join(".ai/node/native-capacity");
                    (std::fs::read(directory.join("anchor")).unwrap(), ())
                };
                let target = &anchor[TARGET_START..TARGET_START + RECORD_BYTES];
                let mut torn = old_account.clone();
                torn[..tear_offset].copy_from_slice(&target[..tear_offset]);
                std::fs::write(root.join(".ai/node/native-capacity/account"), torn).unwrap();
                anyhow::bail!("injected tear");
            }
            Ok(())
        })
        .expect_err("the tear interrupts the cut");
        assert!(format!("{error:#}").contains("injected tear"));
        let (redo, accepted) = node.start_view();
        assert!(!accepted, "the redo model cannot repair this tear");
        let (_, torn) = node.files();
        assert_ne!(torn, old_account);
        let target = super::super::encode(redo.target()).unwrap();
        assert!(account_awaits_cut(&torn.try_into().unwrap(), &target));
        node.cut().unwrap();
        assert_eq!(node.files(), reference);
    }

    #[test]
    fn a_cut_interrupted_after_both_writes_is_complete_and_the_rerun_is_a_no_op() {
        let reference = uninterrupted();
        let node = Node::pre_cut();
        node.cut_failing_at(CutStep::Done);
        assert_eq!(node.files(), reference);
        assert!(node.cut().unwrap().is_empty());
        assert_eq!(node.files(), reference);
    }

    #[test]
    fn an_account_that_is_not_the_pre_cut_image_is_refused_with_both_files_untouched() {
        let node = Node::pre_cut();
        let (_, mut account) = node.files();
        // Damage outside the identity: the checksum no longer holds and the
        // bytes are neither the pre-cut image nor a tear toward the target.
        account[40] ^= 0xff;
        node.write_account(&account);
        let before = node.files();
        let error = node.cut().expect_err("not the pre-cut shape");
        assert!(format!("{error:#}").contains("checksum"), "{error:#}");
        assert_eq!(node.files(), before);
    }
}
