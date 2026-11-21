//! Typed refusals for a capacity file that cannot be trusted as written.
//!
//! The capacity directory holds two files. The anchor is the durable redo: it
//! is published first and carries the image the account must hold. The account
//! is the live record, overwritten in place after the anchor leads it. A host
//! crash or a bad device can leave either file in bytes no write model of this
//! code produces. Callers (start, the stopped-node verbs, the doctor) must be
//! able to tell such a file from a foreign or wrongly issued one, and to name
//! the one verb that can mend it, so the refusal is a value and not a string.
//!
//! The enums here are mechanical shapes of bytes. Whether the signed policy
//! permits an epoch to be re-issued is read from the policy when the cure is
//! computed and is never encoded here.

use std::fmt;

use ryeos_node_runtime::node_policy::sections::native_capacity::NodeNativeCapacityPolicy;

use super::RECORD_BYTES;

/// Which of the two capacity files the refusal is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityFile {
    Account,
    Anchor,
}

impl CapacityFile {
    fn noun(self) -> &'static str {
        match self {
            Self::Account => "capacity account",
            Self::Anchor => "capacity redo anchor",
        }
    }
}

/// What is wrong with the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TornDefect {
    /// The file is not the fixed size its role requires.
    WrongSize,
    /// The leading format marker is not one this build writes.
    ForeignFormat,
    /// The format marker holds but the trailing checksum does not.
    ChecksumMismatch,
    /// The bytes do not decode as a record although no checksum names why.
    Malformed,
    /// A whole, verifiable record that is neither image of the redo it must
    /// match. It was not torn by a write; something replaced it.
    ReplacedRecord,
}

impl fmt::Display for TornDefect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WrongSize => "wrong size",
            Self::ForeignFormat => "foreign format marker",
            Self::ChecksumMismatch => "checksum mismatch",
            Self::Malformed => "malformed record",
            Self::ReplacedRecord => "verifiable record outside the redo's images",
        })
    }
}

/// The one offline remedy the other file proves available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityCure {
    /// The anchor is sound: the account is derived data and is re-derived from
    /// the anchor's target image.
    RepairAccount,
    /// Only the anchor is bad, the account is a sound record of this issuance
    /// and the signed policy permits operator re-issuance: the next epoch opens
    /// from the account record. The same epoch is never rebuilt, because its
    /// recovery counter and authorization are not in the account.
    ReissueFromAccount,
    /// No verb mends this. Keep the node stopped.
    Hold,
}

/// A capacity file that is torn, with what to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapacityRecordTorn {
    pub file: CapacityFile,
    pub defect: TornDefect,
    pub cure: CapacityCure,
}

impl CapacityRecordTorn {
    /// The cure as an instruction an operator can follow.
    pub fn instruction(&self) -> String {
        match self.cure {
            CapacityCure::RepairAccount => "with the node stopped, run `ryeos node capacity \
                repair-account --reason \"...\"` to re-derive the account from the sound redo \
                anchor, then start the node"
                .to_owned(),
            CapacityCure::ReissueFromAccount => "the account record is sound but the same epoch \
                cannot be rebuilt; with the node stopped, run `ryeos node capacity reissue \
                --reason \"...\"` to open the next epoch from the account record (the unspent \
                allowance of the current epoch is forgiven), then start the node"
                .to_owned(),
            CapacityCure::Hold => "no operator verb can rebuild it: keep the node stopped and \
                restore the node's volume from a backup, or re-provision the node"
                .to_owned(),
        }
    }
}

impl fmt::Display for CapacityRecordTorn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is torn ({}): {}",
            self.file.noun(),
            self.defect,
            self.instruction()
        )
    }
}

impl std::error::Error for CapacityRecordTorn {}

/// The decode refusals of the anchor that carry a meaning beyond their text.
/// Any other decode failure has a valid checksum and is a malformed record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnchorDecodeRefusal {
    Format,
    Checksum,
    OtherIssuance,
}

impl fmt::Display for AnchorDecodeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Format => "unsupported capacity redo format",
            Self::Checksum => "capacity redo checksum mismatch",
            Self::OtherIssuance => "capacity redo belongs to another issuance",
        })
    }
}

impl std::error::Error for AnchorDecodeRefusal {}

/// Turn the refusal the anchor decoder produced into the typed torn value
/// when it is one. A foreign issuance is not a tear and keeps its own error,
/// so no cure is ever offered for another node's file.
pub(super) fn classify_anchor_decode(error: anyhow::Error) -> anyhow::Error {
    let defect = match error.downcast_ref::<AnchorDecodeRefusal>() {
        Some(AnchorDecodeRefusal::Format) => TornDefect::ForeignFormat,
        Some(AnchorDecodeRefusal::Checksum) => TornDefect::ChecksumMismatch,
        _ => return error,
    };
    // The account is consulted by the caller that holds it; until then no
    // verb is promised.
    anyhow::Error::new(CapacityRecordTorn {
        file: CapacityFile::Anchor,
        defect,
        cure: CapacityCure::Hold,
    })
}

/// The anchor file's size is wrong.
pub(super) fn anchor_wrong_size() -> anyhow::Error {
    anyhow::Error::new(CapacityRecordTorn {
        file: CapacityFile::Anchor,
        defect: TornDefect::WrongSize,
        cure: CapacityCure::Hold,
    })
}

/// The account file's size is wrong. It is the issued inode and is only ever
/// overwritten at its fixed size, so no verb resizes it.
pub(super) fn account_wrong_size() -> anyhow::Error {
    anyhow::Error::new(CapacityRecordTorn {
        file: CapacityFile::Account,
        defect: TornDefect::WrongSize,
        cure: CapacityCure::Hold,
    })
}

/// Fill in the cure of a torn-anchor refusal from what the account proves.
/// `account` is the account's bytes when they could be read. Any other error
/// is returned unchanged.
pub(super) fn with_anchor_cure(
    error: anyhow::Error,
    account: Option<&[u8; RECORD_BYTES]>,
    policy: &NodeNativeCapacityPolicy,
    issuance: &[u8; 32],
) -> anyhow::Error {
    let Some(torn) = error.downcast_ref::<CapacityRecordTorn>().copied() else {
        return error;
    };
    if torn.file != CapacityFile::Anchor {
        return error;
    }
    let sound_account = account
        .and_then(|bytes| super::decode(bytes).ok())
        .is_some_and(|record| &record.issuance_digest == issuance);
    let cure = if sound_account && policy.renewal.operator_reissue {
        CapacityCure::ReissueFromAccount
    } else {
        CapacityCure::Hold
    };
    anyhow::Error::new(CapacityRecordTorn { cure, ..torn })
}

/// The refusal for an account the sound anchor's redo cannot account for.
/// A record that fails to decode is a tear the anchor mends; a verifiable
/// record is something else and is never rewritten.
pub(super) fn account_outside_redo(account: &[u8; RECORD_BYTES]) -> anyhow::Error {
    let (defect, cure) = match super::decode(account) {
        Ok(_) => (TornDefect::ReplacedRecord, CapacityCure::Hold),
        Err(refusal) => (super::refusal_defect(&refusal), CapacityCure::RepairAccount),
    };
    anyhow::Error::new(CapacityRecordTorn {
        file: CapacityFile::Account,
        defect,
        cure,
    })
}

/// Withdraw any verb from a torn refusal, for a caller that knows the verb
/// cannot run yet (the cut meeting a torn account while the anchor still
/// has the pre-cut shape, which repair does not read).
pub(super) fn without_cure(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<CapacityRecordTorn>().copied() {
        Some(torn) => anyhow::Error::new(CapacityRecordTorn {
            cure: CapacityCure::Hold,
            ..torn
        }),
        None => error,
    }
}
