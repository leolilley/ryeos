//! Retained, nonfinancial capacity debit storage. No SQLite is used here.
//!
//! Only stopped-node initialization issues an allowance. The initialization
//! fence binds that issuance, the fixed account inode and the exact redo role before the
//! private live owner may open it. Parsing is not issuance. Failed publication
//! retains the original descriptors and attempted record; reopening an older
//! record is not a retry.

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use lillux::crypto::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use lillux::{
    PinnedDirectory, PinnedDirectoryIdentity, PinnedRegularFile, PinnedRegularFileIdentity,
};
use ryeos_engine::trust::TrustStore;
use ryeos_node_runtime::node_policy::sections::native_capacity::{
    NativeCapacityUnits, NativeCapacityUnitsRefusal, NativeRenewalEnvelope,
    NodeNativeCapacityPolicy,
};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use super::LiveNativeCapacityInputs;
use ryeos_node_runtime::state_lock::{StateLock, StateLockLease};
use std::sync::Mutex;

mod redo;
mod anchor;
pub(super) mod reissue;
#[cfg(all(test, target_os = "linux"))]
mod persistence_controls;

// The account keeps its issued inode. The anchor is an atomically replaced
// bounded redo role in the issued private directory. Only its exact committed
// before/target images authorize repair; no valid-older-record fallback exists.
const RECORD_BYTES: usize = 4096;
const PAYLOAD_BYTES: usize = RECORD_BYTES - 32;
const RECORD_MAGIC: &[u8; 8] = b"RYECAP05";
const RECORD_SCHEMA: u32 = 5;
const PROCESS_START: usize = 269;
const PROCESS_END: usize = PROCESS_START + redo::PROCESS_STORAGE_BYTES;
// Conservative simultaneous storage: fixed account, incumbent/staged/retained
// anchor roles, bounded lower recovery marker and signed issuance document.
// This is charged once at issuance, never refunded as native allocation credit.
const ACCOUNT_CONTROL_STORAGE_BYTES: u64 = (RECORD_BYTES + 3 * redo::ANCHOR_BYTES + 2 * 16 * 1024) as u64;

/// Signed initialization data, never an allocation capability. All identities
/// must be checked against the real retained descriptors by the private owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCapacityAccountBinding {
    schema: u32,
    node_fingerprint: String,
    root_identity: PinnedDirectoryIdentity,
    runtime_identity: PinnedDirectoryIdentity,
    account_directory_identity: PinnedDirectoryIdentity,
    account_identity: PinnedRegularFileIdentity,
    anchor_protocol: String,
    policy: NodeNativeCapacityPolicy,
}

impl NativeCapacityAccountBinding {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.schema == 6, "foreign capacity issuance binding");
        ensure!(
            digest(&self.node_fingerprint),
            "invalid issuance node fingerprint"
        );
        ensure!(
            self.anchor_protocol == anchor::PROTOCOL,
            "foreign capacity anchor protocol"
        );
        self.policy.validate()?;
        Ok(())
    }

    pub(crate) fn require_node(&self, fingerprint: &str) -> Result<()> {
        self.validate()?;
        ensure!(
            self.node_fingerprint == fingerprint,
            "capacity issuance belongs to another node"
        );
        Ok(())
    }

    fn issuance_digest(&self) -> Result<[u8; 32]> {
        self.validate()?;
        // This binding's canonical JSON has fixed, sorted keys; validated
        // fingerprint/nonce strings are ASCII hex. Stream the exact bytes
        // into the hash instead of allocating Value, canonical Vec and hex.
        // Identity serializers also emit containing_device then inode.
        #[derive(Serialize)]
        struct Units {
            allocator_steps: u64,
            cleanup_steps: u64,
            connection_tickets: u64,
            host_control_storage_bytes: u64,
            storage_bytes: u64,
        }
        impl From<NativeCapacityUnits> for Units {
            fn from(value: NativeCapacityUnits) -> Self {
                // Exhaustive destructuring makes a new funding dimension a
                // compile-time protocol change, never a silently omitted hash.
                let NativeCapacityUnits {
                    storage_bytes, host_control_storage_bytes, allocator_steps,
                    connection_tickets, cleanup_steps,
                } = value;
                Self {
                    allocator_steps, cleanup_steps, connection_tickets,
                    host_control_storage_bytes, storage_bytes,
                }
            }
        }
        #[derive(Serialize)]
        struct Incarnation {
            cleanup_occupancy_ms: u64,
            host_release_steps: u64,
            maximum_occupancy_ms: u64,
            outside_cleanup_steps: u64,
            rollover_drain_margin_ms: u64,
            units: Units,
        }
        #[derive(Serialize)]
        struct Policy<'a> {
            allowance: Units,
            incarnation: Incarnation,
            issuance_nonce: &'a str,
            recovery: Recovery,
            renewal: Renewal,
            schema: u32,
        }
        #[derive(Serialize)]
        struct Recovery {
            control_storage_bytes: u64,
            maximum_attempts: u64,
            maximum_duration_ms: u64,
            work_per_attempt: u64,
        }
        // `None` serialises as `null`, exactly as canonical JSON writes it.
        #[derive(Serialize)]
        struct Renewal {
            at_orderly_rollover: bool,
            maximum_epochs: Option<u64>,
            operator_reissue: bool,
        }
        #[derive(Serialize)]
        struct Binding<'a> {
            account_directory_identity: &'a PinnedDirectoryIdentity,
            account_identity: &'a PinnedRegularFileIdentity,
            anchor_protocol: &'a str,
            node_fingerprint: &'a str,
            policy: Policy<'a>,
            root_identity: &'a PinnedDirectoryIdentity,
            runtime_identity: &'a PinnedDirectoryIdentity,
            schema: u32,
        }
        struct HashWriter(sha2::Sha256);
        impl std::io::Write for HashWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let binding = Binding {
            account_directory_identity: &self.account_directory_identity,
            account_identity: &self.account_identity,
            anchor_protocol: &self.anchor_protocol,
            node_fingerprint: &self.node_fingerprint,
            policy: Policy {
                allowance: self.policy.allowance.into(),
                incarnation: Incarnation {
                    cleanup_occupancy_ms: self.policy.incarnation.cleanup_occupancy_ms,
                    host_release_steps: self.policy.incarnation.host_release_steps,
                    outside_cleanup_steps: self.policy.incarnation.outside_cleanup_steps,
                    maximum_occupancy_ms: self.policy.incarnation.maximum_occupancy_ms,
                    rollover_drain_margin_ms: self.policy.incarnation.rollover_drain_margin_ms,
                    units: self.policy.incarnation.units.into(),
                },
                issuance_nonce: &self.policy.issuance_nonce,
                recovery: Recovery {
                    control_storage_bytes: self.policy.recovery.control_storage_bytes,
                    maximum_attempts: self.policy.recovery.maximum_attempts,
                    maximum_duration_ms: self.policy.recovery.maximum_duration_ms,
                    work_per_attempt: self.policy.recovery.work_per_attempt,
                },
                renewal: {
                    let NativeRenewalEnvelope { at_orderly_rollover, operator_reissue, maximum_epochs } =
                        self.policy.renewal;
                    Renewal { at_orderly_rollover, maximum_epochs, operator_reissue }
                },
                schema: self.policy.schema,
            },
            root_identity: &self.root_identity,
            runtime_identity: &self.runtime_identity,
            schema: self.schema,
        };
        let mut writer = HashWriter(sha2::Sha256::new());
        serde_json::to_writer(&mut writer, &binding)?;
        Ok(writer.0.finalize().into())
    }

    fn require_inputs(&self, inputs: &LiveNativeCapacityInputs) -> Result<()> {
        self.require_node(&inputs.node_fingerprint)?;
        ensure!(
            self.root_identity == inputs.root.identity()?,
            "capacity root changed"
        );
        ensure!(
            self.runtime_identity == inputs.runtime.identity()?,
            "capacity runtime changed"
        );
        ensure!(
            self.policy == inputs.policy,
            "immutable capacity issuance changed; policy refresh cannot replenish it"
        );
        Ok(())
    }

    pub(crate) fn require_installed(
        &self,
        app_root: &Path,
        fingerprint: &str,
        policy: &NodeNativeCapacityPolicy,
    ) -> Result<()> {
        self.require_node(fingerprint)?;
        ensure!(
            &self.policy == policy,
            "capacity policy differs from original issuance"
        );
        let root = PinnedDirectory::open(app_root)?.context("capacity root absent")?;
        let runtime = PinnedDirectory::open(&app_root.join(".ai/state"))?
            .context("capacity runtime absent")?;
        ensure!(
            root.identity()? == self.root_identity && runtime.identity()? == self.runtime_identity,
            "capacity root/runtime identities changed"
        );
        root.ensure_path_binding()?;
        runtime.ensure_path_binding()?;
        let directory = PinnedDirectory::open(&app_root.join(".ai/node/native-capacity"))?
            .context("capacity directory absent")?;
        directory.ensure_path_binding()?;
        directory.require_owner_private_directory()?;
        ensure!(
            directory.identity()? == self.account_directory_identity,
            "capacity directory differs from original issuance"
        );
        let account = directory.open_pinned_regular(OsStr::new("account"), false)?
            .context("original capacity account is absent")?;
        account.require_effective_user_owned()?;
        directory.require_pinned_regular_entry_fixed(&account).map_err(anyhow::Error::msg)?;
        ensure!(account.identity()? == self.account_identity, "capacity account replaced");
        // Binding verification is read-only, not recovery or live admission.
        // The lower redo transaction can temporarily have no target name. Only
        // the retained account owner may resolve its exact marker later; absent
        // or malformed redo never supplies a budget here.
        if let Some(anchor) = directory.open_pinned_regular(OsStr::new("anchor"), false)? {
            anchor.require_effective_user_owned()?;
            directory.require_pinned_regular_entry_fixed(&anchor).map_err(anyhow::Error::msg)?;
            ensure!(anchor.identity()? != self.account_identity, "capacity account/anchor alias");
            ensure!(anchor.observation()?.size() == redo::ANCHOR_BYTES as u64, "capacity redo size changed");
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuanceBody {
    schema: String,
    operator_fingerprint: String,
    binding: NativeCapacityAccountBinding,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuanceDocument {
    body: IssuanceBody,
    signature: String,
}

struct IssuancePreparation {
    root: PinnedDirectory,
    runtime: PinnedDirectory,
    onboarding: PinnedDirectory,
    node: PinnedDirectory,
    exclusion: Arc<StateLockLease>,
    directory: Option<PinnedDirectory>,
    account: Option<PinnedRegularFile>,
    anchor: Option<PinnedRegularFile>,
    issuance: Option<PinnedRegularFile>,
    deadline: lillux::time::MonotonicDeadline,
}

struct IssuancePreparationFailure {
    owner: Arc<Mutex<IssuancePreparation>>,
    error: anyhow::Error,
}

/// The initializer keeps successful preparation alive through completion
/// publication, and attaches the same owner to any late completion error.
pub(crate) struct PreparedCapacityIssuance {
    binding: NativeCapacityAccountBinding,
    owner: Option<Arc<Mutex<IssuancePreparation>>>,
}

impl PreparedCapacityIssuance {
    pub(crate) fn binding(&self) -> &NativeCapacityAccountBinding {
        &self.binding
    }

    pub(crate) fn retain_error(&self, error: anyhow::Error) -> anyhow::Error {
        match &self.owner {
            Some(owner) => anyhow::Error::new(IssuancePreparationFailure {
                owner: Arc::clone(owner),
                error,
            }),
            None => error,
        }
    }
}

impl std::fmt::Debug for IssuancePreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuancePreparationFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for IssuancePreparationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "capacity issuance incomplete; original preparation retained: {}",
            self.error
        )
    }
}
impl std::error::Error for IssuancePreparationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// Initial issuance belongs to the actual stopped-node operator ceremony.
/// `operator_created` is the result of that ceremony's create-once private-key
/// operation, never an option supplied by a live caller. Existing operators
/// can preserve a valid issuance but cannot recreate an absent one here.
pub(crate) fn prepare_initial_capacity_binding(
    app_root: &Path,
    state_lock: &StateLock,
    operator_key: &SigningKey,
    operator_created: bool,
    node_fingerprint: &str,
    policy: &NodeNativeCapacityPolicy,
) -> Result<PreparedCapacityIssuance> {
    state_lock.ensure_protects_app_root(app_root)?;
    policy.validate()?;
    let original_deadline = lillux::time::MonotonicDeadline::after(
        std::time::Duration::from_millis(policy.recovery.maximum_duration_ms));
    let operator_fingerprint =
        ryeos_engine::trust::compute_fingerprint(&operator_key.verifying_key());
    let onboarding = PinnedDirectory::open(&app_root.join(".ai/config/onboarding"))?
        .context("operator onboarding absent")?;
    if onboarding
        .open_pinned_regular(OsStr::new("native-capacity-issuance.json"), false)?
        .is_some()
    {
        let binding = load_existing_capacity_binding(
            app_root,
            &operator_fingerprint,
            node_fingerprint,
            policy,
        )?;
        return Ok(PreparedCapacityIssuance {
            binding,
            owner: None,
        });
    }
    ensure!(
        operator_created,
        "capacity issuance absent for existing operator; explicit retained recovery required"
    );
    // Reserve both fixed records and the bounded signed issuance document,
    // in addition to the first native incarnation's storage envelope. Unused
    // document space is not refunded or turned into native allocation credit.
    let control_storage = ACCOUNT_CONTROL_STORAGE_BYTES;
    let required_storage = control_storage
        .checked_add(policy.incarnation.units.storage_bytes)
        .context("capacity control storage overflow")?;
    ensure!(
        policy.allowance.storage_bytes >= required_storage,
        "capacity allowance omits persistent control files"
    );
    let root = PinnedDirectory::open(app_root)?.context("capacity root absent")?;
    let runtime =
        PinnedDirectory::open(&app_root.join(".ai/state"))?.context("capacity runtime absent")?;
    let node =
        PinnedDirectory::open(&app_root.join(".ai/node"))?.context("node namespace absent")?;
    root.ensure_path_binding()?;
    runtime.ensure_path_binding()?;
    node.ensure_path_binding()?;
    onboarding.ensure_path_binding()?;
    ensure!(
        node.open_child_directory(OsStr::new("native-capacity"))?
            .is_none(),
        "orphan capacity namespace requires recovery; do not adopt it"
    );
    let owner = Arc::new(Mutex::new(IssuancePreparation {
        root,
        runtime,
        onboarding,
        node,
        exclusion: Arc::new(state_lock.retain()),
        directory: None,
        account: None,
        anchor: None,
        issuance: None,
        deadline: original_deadline,
    }));
    let result = (|| -> Result<NativeCapacityAccountBinding> {
        let mut state = owner
            .lock()
            .map_err(|_| anyhow::anyhow!("capacity preparation owner poisoned"))?;
        state.exclusion.ensure_protects_app_root(app_root)?;
        state.directory = Some(
            state
                .node
                .create_child(OsStr::new("native-capacity"), 0o700)?,
        );
        let directory = state
            .directory
            .as_ref()
            .context("capacity directory not retained")?;
        directory.require_owner_private_directory()?;
        state.account = Some(
            directory
                .atomic_create_pinned_regular(
                    OsStr::new("account"),
                    &[0; RECORD_BYTES],
                    0o600,
                    crate::NODE_TEMP_PURPOSE,
                )?
                .context("capacity account creation collided")?,
        );
        let directory = state
            .directory
            .as_ref()
            .context("capacity directory not retained")?;
        state.anchor = Some(
            directory
                .atomic_create_pinned_regular(
                    OsStr::new("anchor"),
                    &[0; redo::ANCHOR_BYTES],
                    0o600,
                    crate::NODE_TEMP_PURPOSE,
                )?
                .context("capacity anchor creation collided")?,
        );
        let binding = NativeCapacityAccountBinding {
            schema: 6,
            node_fingerprint: node_fingerprint.to_owned(),
            root_identity: state.root.identity()?,
            runtime_identity: state.runtime.identity()?,
            account_directory_identity: state
                .directory
                .as_ref()
                .context("capacity directory lost")?
                .identity()?,
            account_identity: state.account.as_ref().context("capacity account lost")?.identity()?,
            anchor_protocol: anchor::PROTOCOL.to_owned(),
            policy: policy.clone(),
        };
        binding.validate()?;
        let genesis = CapacityRecord {
            schema: RECORD_SCHEMA,
            epoch: 1,
            issuance_digest: binding.issuance_digest()?,
            sequence: 0,
            consumed: genesis_consumed(),
            outstanding: None,
        };
        let bytes = encode(&genesis)?;
        initialize_control_file(state.account.as_ref().context("capacity account lost")?, &bytes, state.deadline)?;
        let anchor_bytes = redo::AccountRedo::genesis(genesis, policy)?.encode(policy)?;
        initialize_control_file(state.anchor.as_ref().context("capacity anchor lost")?, &anchor_bytes, state.deadline)?;
        state.exclusion.ensure_protects_app_root(app_root)?;
        binding.require_installed(app_root, node_fingerprint, policy)?;
        let body = IssuanceBody {
            schema: "ryeos/native-capacity-issuance/v3".to_owned(),
            operator_fingerprint: operator_fingerprint.clone(),
            binding: binding.clone(),
        };
        let canonical = lillux::canonical_json(&serde_json::to_value(&body)?)?;
        let signature = operator_key.sign(canonical.as_bytes());
        let document = IssuanceDocument {
            body,
            signature: format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())
            ),
        };
        let document_bytes = serde_json::to_vec(&document)?;
        ensure!(
            document_bytes.len() <= 16 * 1024,
            "capacity issuance exceeds control bound"
        );
        state.issuance = Some(
            state
                .onboarding
                .atomic_create_pinned_regular(
                    OsStr::new("native-capacity-issuance.json"),
                    &document_bytes,
                    0o600,
                    crate::NODE_TEMP_PURPOSE,
                )?
                .context("capacity issuance publication collided")?,
        );
        binding.require_installed(app_root, node_fingerprint, policy)?;
        state.onboarding.sync()?;
        Ok(binding)
    })();
    match result {
        Ok(binding) => Ok(PreparedCapacityIssuance {
            binding,
            owner: Some(owner),
        }),
        Err(error) => Err(anyhow::Error::new(IssuancePreparationFailure {
            owner,
            error,
        })),
    }
}

fn initialize_control_file(file: &PinnedRegularFile, bytes: &[u8],
    original: lillux::time::MonotonicDeadline) -> Result<()> {
    ensure!(bytes.len() <= redo::ANCHOR_BYTES, "initial capacity record exceeds fixed bound");
    let zeros = [0; redo::ANCHOR_BYTES];
    let mut scratch = [0; redo::ANCHOR_BYTES];
    file.overwrite_fixed_if_matches(&zeros[..bytes.len()], bytes, &mut scratch[..bytes.len()], original)
}

/// Resealing preserves an already operator-signed issuance. It never adopts
/// unsigned records, creates missing stores, or derives a grant from a ceiling.
pub(crate) fn load_existing_capacity_binding(
    app_root: &Path,
    operator_fingerprint: &str,
    node_fingerprint: &str,
    policy: &NodeNativeCapacityPolicy,
) -> Result<NativeCapacityAccountBinding> {
    let parent = PinnedDirectory::open(&app_root.join(".ai/config/onboarding"))?
        .context("capacity issuance directory absent")?;
    let input = super::RetainedInput::capture(
        parent,
        OsStr::new("native-capacity-issuance.json"),
        16 * 1024,
    )?;
    let document: IssuanceDocument = serde_json::from_slice(&input.bytes)?;
    ensure!(
        document.body.schema == "ryeos/native-capacity-issuance/v3",
        "foreign capacity issuance schema"
    );
    ensure!(
        document.body.operator_fingerprint == operator_fingerprint,
        "capacity issuance operator differs from initialization"
    );
    let trust = TrustStore::load(None, &app_root.join(".ai/config"))?;
    let operator = &trust
        .get(operator_fingerprint)
        .context("capacity operator is not pinned")?
        .verifying_key;
    let encoded = document
        .signature
        .strip_prefix("ed25519:")
        .context("capacity signature encoding invalid")?;
    let signature =
        Signature::from_slice(&base64::engine::general_purpose::STANDARD.decode(encoded)?)?;
    operator.verify(
        lillux::canonical_json(&serde_json::to_value(&document.body)?)?.as_bytes(),
        &signature,
    )?;
    document
        .body
        .binding
        .require_installed(app_root, node_fingerprint, policy)?;
    input.require_original()?;
    Ok(document.body.binding)
}

/// One durable account image. `epoch` counts the grants opened under this
/// issuance: genesis is epoch 1, each renewal opens the next, and `sequence`
/// keeps counting debits across epochs so an incarnation number never repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapacityRecord {
    pub(crate) schema: u32,
    pub(crate) epoch: u64,
    pub(crate) issuance_digest: [u8; 32],
    pub(crate) sequence: u64,
    pub(crate) consumed: NativeCapacityUnits,
    pub(crate) outstanding: Option<IncarnationDebit>,
}

/// The consumption every epoch opens with: the fixed control files, charged
/// once at issuance and again by each renewal, never any incarnation.
pub(super) fn genesis_consumed() -> NativeCapacityUnits {
    NativeCapacityUnits {
        storage_bytes: ACCOUNT_CONTROL_STORAGE_BYTES,
        host_control_storage_bytes: 0,
        allocator_steps: 0,
        connection_tickets: 0,
        cleanup_steps: 0,
    }
}

/// Digest a rollover renewal carries: it names the issuance, the epoch being
/// closed, the served incarnation and its sequence, all taken from the
/// owner's own retained record, never from the stop owner's claim.
pub(super) fn rollover_authorization(issuance: &[u8; 32], before: &CapacityRecord) -> Result<[u8; 32]> {
    let served = before.outstanding
        .context("rollover renewal requires the served incarnation in its before image")?;
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"ryeos/native-capacity-rollover-claim/v1");
    hasher.update(issuance);
    hasher.update(before.epoch.to_le_bytes());
    hasher.update(served.process_digest);
    hasher.update(before.sequence.to_le_bytes());
    Ok(hasher.finalize().into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IncarnationDebit {
    process_digest: [u8; 32],
    process: redo::StoredProcess,
    clock_digest: [u8; 32],
    completion_digest: [u8; 32],
    policy_generation_digest: [u8; 32],
    units: NativeCapacityUnits,
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapacityRecordRefusal {
    Schema,
    Epoch,
    Consumption,
    Unsequenced,
    Units(NativeCapacityUnitsRefusal),
    ExceedsConsumption,
    Digest,
    Format,
    Checksum,
    Padding,
    AbsentDebitData,
    Tag,
    Layout,
    Process,
}
impl std::fmt::Display for CapacityRecordRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Schema => "foreign capacity record schema",
            Self::Epoch => "unopened capacity epoch",
            Self::Consumption => "invalid cumulative capacity debit",
            Self::Unsequenced => "unsequenced capacity debit",
            Self::Units(_) => "invalid capacity debit units",
            Self::ExceedsConsumption => "debit exceeds consumption",
            Self::Digest => "noncanonical capacity digest",
            Self::Format => "foreign capacity record format",
            Self::Checksum => "capacity checksum mismatch",
            Self::Padding => "capacity padding is nonzero",
            Self::AbsentDebitData => "absent capacity debit has data",
            Self::Tag => "invalid capacity debit tag",
            Self::Layout => "invalid fixed capacity layout",
            Self::Process => "capacity process coordinates are invalid or disagree with their digest",
        })
    }
}
impl std::error::Error for CapacityRecordRefusal {}
impl From<NativeCapacityUnitsRefusal> for CapacityRecordRefusal {
    fn from(error: NativeCapacityUnitsRefusal) -> Self {
        Self::Units(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapacityDebitRefusal {
    Record(CapacityRecordRefusal),
    Outstanding,
    ConsumptionOverflow,
    AllowanceExhausted,
    SequenceExhausted,
}
impl std::fmt::Display for CapacityDebitRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Record(_) => "invalid original capacity record or debit",
            Self::Outstanding => "original incarnation remains charged",
            Self::ConsumptionOverflow => "capacity debit overflow",
            Self::AllowanceExhausted => "native capacity account exhausted",
            Self::SequenceExhausted => "capacity sequence exhausted",
        })
    }
}
impl std::error::Error for CapacityDebitRefusal {}
impl From<CapacityRecordRefusal> for CapacityDebitRefusal {
    fn from(error: CapacityRecordRefusal) -> Self { Self::Record(error) }
}

impl CapacityRecord {
    // Pure fixed-size preparation, never authentication or debit authority.
    // The actual account owner retains and publishes this record separately.
    fn prepare_debit(
        &self, allowance: NativeCapacityUnits, requested: NativeCapacityUnits,
    ) -> std::result::Result<(u64, NativeCapacityUnits), CapacityDebitRefusal> {
        self.validate()?;
        requested.validate().map_err(CapacityRecordRefusal::from)?;
        if self.outstanding.is_some() { return Err(CapacityDebitRefusal::Outstanding); }
        let add = |used: u64, request: u64| used.checked_add(request)
            .ok_or(CapacityDebitRefusal::ConsumptionOverflow);
        let consumed = NativeCapacityUnits {
            storage_bytes: add(self.consumed.storage_bytes, requested.storage_bytes)?,
            host_control_storage_bytes: add(self.consumed.host_control_storage_bytes, requested.host_control_storage_bytes)?,
            allocator_steps: add(self.consumed.allocator_steps, requested.allocator_steps)?,
            connection_tickets: add(self.consumed.connection_tickets, requested.connection_tickets)?,
            cleanup_steps: add(self.consumed.cleanup_steps, requested.cleanup_steps)?,
        };
        if !allowance.covers(&consumed) { return Err(CapacityDebitRefusal::AllowanceExhausted); }
        let sequence = self.sequence.checked_add(1)
            .ok_or(CapacityDebitRefusal::SequenceExhausted)?;
        Ok((sequence, consumed))
    }

    fn validate(&self) -> std::result::Result<(), CapacityRecordRefusal> {
        if self.schema != RECORD_SCHEMA {
            return Err(CapacityRecordRefusal::Schema);
        }
        if self.epoch == 0 {
            return Err(CapacityRecordRefusal::Epoch);
        }
        for value in [
            self.consumed.storage_bytes,
            self.consumed.host_control_storage_bytes,
            self.consumed.allocator_steps,
            self.consumed.connection_tickets,
            self.consumed.cleanup_steps,
        ] {
            if value > i64::MAX as u64 {
                return Err(CapacityRecordRefusal::Consumption);
            }
        }
        if let Some(debit) = &self.outstanding {
            if self.sequence == 0 {
                return Err(CapacityRecordRefusal::Unsequenced);
            }
            debit.units.validate()?;
            if debit.process.digest().map_err(|_| CapacityRecordRefusal::Process)? != debit.process_digest {
                return Err(CapacityRecordRefusal::Process);
            }
            if !self.consumed.covers(&debit.units) {
                return Err(CapacityRecordRefusal::ExceedsConsumption);
            }
        }
        Ok(())
    }
}

// Fixed account records avoid success-path heap allocations. Capture,
// trust, errors and the actual allocator still require bootstrap custody.
fn fixed_digest(text: &str) -> std::result::Result<[u8; 32], CapacityRecordRefusal> {
    if !digest(text) {
        return Err(CapacityRecordRefusal::Digest);
    }
    let nibble = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
    let mut bytes = [0; 32];
    for (out, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *out = (nibble(pair[0]) << 4) | nibble(pair[1]);
    }
    Ok(bytes)
}

fn encode_units(units: NativeCapacityUnits, bytes: &mut [u8]) {
    for (slot, value) in bytes.chunks_exact_mut(8).zip([
        units.storage_bytes,
        units.host_control_storage_bytes,
        units.allocator_steps,
        units.connection_tickets,
        units.cleanup_steps,
    ]) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

fn decode_units(bytes: &[u8]) -> NativeCapacityUnits {
    let number = |offset: usize| {
        u64::from_le_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("fixed account units"),
        )
    };
    NativeCapacityUnits {
        storage_bytes: number(0),
        host_control_storage_bytes: number(8),
        allocator_steps: number(16),
        connection_tickets: number(24),
        cleanup_steps: number(32),
    }
}

fn encode(
    record: &CapacityRecord,
) -> std::result::Result<[u8; RECORD_BYTES], CapacityRecordRefusal> {
    record.validate()?;
    let mut bytes = [0; RECORD_BYTES];
    bytes[..8].copy_from_slice(RECORD_MAGIC);
    bytes[8..12].copy_from_slice(&record.schema.to_le_bytes());
    bytes[12..20].copy_from_slice(&record.epoch.to_le_bytes());
    bytes[20..28].copy_from_slice(&record.sequence.to_le_bytes());
    bytes[28..60].copy_from_slice(&record.issuance_digest);
    encode_units(record.consumed, &mut bytes[60..100]);
    if let Some(debit) = record.outstanding {
        bytes[100] = 1;
        bytes[101..133].copy_from_slice(&debit.process_digest);
        bytes[133..165].copy_from_slice(&debit.clock_digest);
        bytes[165..197].copy_from_slice(&debit.completion_digest);
        bytes[197..229].copy_from_slice(&debit.policy_generation_digest);
        encode_units(debit.units, &mut bytes[229..269]);
        debit.process.encode_into(&mut bytes[PROCESS_START..PROCESS_END]);
    }
    let checksum: [u8; 32] = sha2::Sha256::digest(&bytes[..PAYLOAD_BYTES]).into();
    bytes[PAYLOAD_BYTES..].copy_from_slice(&checksum);
    Ok(bytes)
}

fn decode(
    bytes: &[u8; RECORD_BYTES],
) -> std::result::Result<CapacityRecord, CapacityRecordRefusal> {
    if &bytes[..8] != RECORD_MAGIC { return Err(CapacityRecordRefusal::Format); }
    let checksum: [u8; 32] = sha2::Sha256::digest(&bytes[..PAYLOAD_BYTES]).into();
    if bytes[PAYLOAD_BYTES..] != checksum {
        return Err(CapacityRecordRefusal::Checksum);
    }
    if bytes[PROCESS_END..PAYLOAD_BYTES].iter().any(|b| *b != 0) {
        return Err(CapacityRecordRefusal::Padding);
    }
    let outstanding = match bytes[100] {
        0 => {
            if bytes[101..PROCESS_END].iter().any(|b| *b != 0) {
                return Err(CapacityRecordRefusal::AbsentDebitData);
            }
            None
        }
        1 => Some(IncarnationDebit {
            process: redo::StoredProcess::decode(&bytes[PROCESS_START..PROCESS_END])
                .map_err(|_| CapacityRecordRefusal::Process)?,
            process_digest: bytes[101..133]
                .try_into()
                .map_err(|_| CapacityRecordRefusal::Layout)?,
            clock_digest: bytes[133..165]
                .try_into()
                .map_err(|_| CapacityRecordRefusal::Layout)?,
            completion_digest: bytes[165..197]
                .try_into()
                .map_err(|_| CapacityRecordRefusal::Layout)?,
            policy_generation_digest: bytes[197..229]
                .try_into()
                .map_err(|_| CapacityRecordRefusal::Layout)?,
            units: decode_units(&bytes[229..269]),
        }),
        _ => return Err(CapacityRecordRefusal::Tag),
    };
    let record = CapacityRecord {
        schema: u32::from_le_bytes(
            bytes[8..12]
                .try_into()
                .map_err(|_| CapacityRecordRefusal::Layout)?,
        ),
        epoch: u64::from_le_bytes(
            bytes[12..20]
                .try_into()
                .map_err(|_| CapacityRecordRefusal::Layout)?,
        ),
        issuance_digest: bytes[28..60]
            .try_into()
            .map_err(|_| CapacityRecordRefusal::Layout)?,
        sequence: u64::from_le_bytes(
            bytes[20..28]
                .try_into()
                .map_err(|_| CapacityRecordRefusal::Layout)?,
        ),
        consumed: decode_units(&bytes[60..100]),
        outstanding,
    };
    record.validate()?;
    Ok(record)
}

struct RetainedStore {
    parent: PinnedDirectory,
    pinned: PinnedRegularFile,
    identity: PinnedRegularFileIdentity,
}

const ACCOUNT_READ_WORK: u64 = RECORD_BYTES as u64 / 64 + 16;
const ACCOUNT_WRITE_WORK: u64 = 3 * RECORD_BYTES as u64 / 64 + 32;
const PROCESS_OBSERVATION_WORK: u64 = 512;
// The selected node's enclosing bootstrap scope pays these fixed, finite
// admission operations before a durable recovery attempt exists. It is not
// native-region credit or retroactive funding from the attempt being issued.
// One extra anchor read funds the operator re-issuance document check.
const BOOTSTRAP_WORK: u64 = 16 * anchor::PUBLICATION_WORK + 17 * anchor::READ_WORK;

pub(crate) const REISSUANCE_DOCUMENT_NAME: &str = "native-capacity-reissuance.json";
pub(crate) const REISSUANCE_DOCUMENT_BOUND: u64 = 16 * 1024;
pub(crate) const REISSUANCE_SCHEMA: &str = "ryeos/native-capacity-reissuance/v1";

/// What the operator signed to open one epoch on a stopped node. The anchor
/// digest and the epoch pair bind it to exactly one redo transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReissuanceBody {
    pub(crate) schema: String,
    pub(crate) operator_fingerprint: String,
    pub(crate) node_fingerprint: String,
    pub(crate) issuance_digest: String,
    pub(crate) from_epoch: u64,
    pub(crate) to_epoch: u64,
    pub(crate) anchor_digest: String,
    pub(crate) previous_reissuance_digest: Option<String>,
    pub(crate) reason: String,
    pub(crate) issued_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReissuanceDocument {
    pub(crate) body: ReissuanceBody,
    pub(crate) signature: String,
}

/// Parse and verify a re-issuance document against the node it must name and
/// the pinned operator key. Shared by the live open and the stopped-node
/// command so both refuse the same foreign documents.
pub(crate) fn verify_reissuance_document(
    bytes: &[u8],
    operator: &VerifyingKey,
    operator_fingerprint: &str,
    node_fingerprint: &str,
    issuance: &[u8; 32],
) -> Result<ReissuanceBody> {
    ensure!(u64::try_from(bytes.len())? <= REISSUANCE_DOCUMENT_BOUND,
        "operator re-issuance document exceeds its bound");
    let document: ReissuanceDocument = serde_json::from_slice(bytes)?;
    let body = document.body;
    ensure!(body.schema == REISSUANCE_SCHEMA, "foreign operator re-issuance schema");
    ensure!(body.operator_fingerprint == operator_fingerprint,
        "operator re-issuance was signed for another operator");
    ensure!(body.node_fingerprint == node_fingerprint,
        "operator re-issuance belongs to another node");
    ensure!(body.issuance_digest == hex_digest(issuance),
        "operator re-issuance belongs to another issuance");
    ensure!(body.from_epoch.checked_add(1) == Some(body.to_epoch),
        "operator re-issuance does not open the next epoch");
    ensure!(digest(&body.anchor_digest), "operator re-issuance anchor digest is not canonical");
    ensure!(body.previous_reissuance_digest.as_deref().is_none_or(digest),
        "operator re-issuance previous digest is not canonical");
    ensure!(body.reason.chars().count() <= 1024, "operator re-issuance reason exceeds its bound");
    let encoded = document.signature.strip_prefix("ed25519:")
        .context("operator re-issuance signature encoding invalid")?;
    let signature = Signature::from_slice(&base64::engine::general_purpose::STANDARD.decode(encoded)?)?;
    operator.verify(lillux::canonical_json(&serde_json::to_value(&body)?)?.as_bytes(), &signature)?;
    Ok(body)
}

/// The document an `OperatorReissue` anchor names must still be the one the
/// operator signed for exactly this anchor: present in `onboarding`, bytes
/// hashing to the anchor's authorization, naming this operator, node,
/// issuance and the epoch the anchor opened, and signed by `operator`. The
/// retained input is returned so the caller can pin it after further checks.
fn verify_reissue_anchor_document(
    onboarding: PinnedDirectory,
    operator: &VerifyingKey,
    operator_fingerprint: &str,
    node_fingerprint: &str,
    issuance: &[u8; 32],
    current: &redo::AccountRedo,
) -> Result<super::RetainedInput> {
    ensure!(current.operation() == redo::Operation::OperatorReissue,
        "capacity anchor is not an operator re-issuance");
    ensure!(onboarding.open_pinned_regular(OsStr::new(REISSUANCE_DOCUMENT_NAME), false)?.is_some(),
        "operator re-issuance anchor has no document");
    let input = super::RetainedInput::capture(onboarding, OsStr::new(REISSUANCE_DOCUMENT_NAME),
        REISSUANCE_DOCUMENT_BOUND)?;
    let digest: [u8; 32] = sha2::Sha256::digest(&input.bytes).into();
    ensure!(digest == current.authorization(), "operator re-issuance document is stale");
    let body = verify_reissuance_document(&input.bytes, operator, operator_fingerprint,
        node_fingerprint, issuance)?;
    ensure!(body.to_epoch == current.epoch(), "operator re-issuance names another epoch");
    Ok(input)
}

fn hex_digest(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl RetainedStore {
    fn open(parent: PinnedDirectory, expected: PinnedRegularFileIdentity) -> Result<Self> {
        let pinned = parent.open_pinned_regular(OsStr::new("account"), true)?
            .context("original capacity account is absent; startup cannot issue it")?;
        let result = Self { parent, pinned, identity: expected };
        result.require_original()?;
        Ok(result)
    }

    fn require_original(&self) -> Result<()> {
        self.parent.ensure_path_binding_fixed().map_err(anyhow::Error::msg)?;
        self.parent.require_owner_private_directory_fixed().map_err(anyhow::Error::msg)?;
        self.parent.require_pinned_regular_entry_fixed(&self.pinned).map_err(anyhow::Error::msg)?;
        self.pinned.require_effective_user_owned()?;
        ensure!(self.pinned.identity()? == self.identity, "capacity account differs from issued inode");
        ensure!(self.pinned.observation()?.size() == RECORD_BYTES as u64, "capacity account size changed");
        Ok(())
    }

    fn read_bytes(&self, deadline: lillux::time::MonotonicDeadline,
        pay: &dyn Fn(u64) -> Result<()>) -> Result<[u8; RECORD_BYTES]> {
        pay(ACCOUNT_READ_WORK)?;
        self.require_original()?;
        let mut bytes = [0; RECORD_BYTES];
        self.pinned.read_exact_stable_into(&mut bytes, deadline)?;
        self.require_original()?;
        Ok(bytes)
    }

    fn apply_redo(&self, redo: &redo::AccountRedo, deadline: lillux::time::MonotonicDeadline,
        pay: &dyn Fn(u64) -> Result<()>) -> Result<()> {
        let before = self.read_bytes(deadline, pay)?;
        ensure!(redo.accepts_account_bytes(&before)?, "account is outside its exact durable redo transition");
        let target = encode(redo.target())?;
        pay(ACCOUNT_WRITE_WORK)?;
        self.require_original()?;
        let mut scratch = [0; RECORD_BYTES];
        self.pinned.overwrite_fixed_if_matches(&before, &target, &mut scratch, deadline)?;
        self.require_original()?;
        Ok(())
    }
}

fn settlement_transition_bytes(current: &[u8; RECORD_BYTES], before: &[u8; RECORD_BYTES],
    target: &[u8; RECORD_BYTES], write_attempted: bool) -> bool {
    if current == before || current == target { return true; }
    if !write_attempted { return false; }
    // The durable redo is published before the first account write. Accept
    // exactly a target prefix followed by its predecessor suffix, including
    // prefixes crossing the checksum. Arbitrary corruption is never repaired.
    let mut predecessor_suffix = false;
    for index in 0..RECORD_BYTES {
        if current[index] != target[index] { predecessor_suffix = true; }
        if predecessor_suffix && current[index] != before[index] { return false; }
    }
    true
}

struct AccountControlBudget {
    // Atomic so the retained account stays `Sync`: boundaries share it
    // through a read lock while settlement holds it exclusively.
    remaining: std::sync::atomic::AtomicU64,
    process: [u8; 32],
    clock: lillux::time::OccupancyLimit,
    deadline: lillux::time::MonotonicDeadline,
}
impl AccountControlBudget {
    fn original(inputs: &LiveNativeCapacityInputs, steps: u64) -> Result<Self> {
        let clock = lillux::time::OccupancyLimit::new(inputs.clock.start().clone(),
            inputs.policy.recovery.maximum_duration_ms.checked_mul(1_000_000)
                .context("capacity control interval overflow")?).map_err(anyhow::Error::msg)?;
        // Derive from the SAME retained start, not a fresh duration at retry.
        // Lower positional I/O uses this endpoint as well as each work check.
        let (deadline, _) = inputs.clock.original_local_deadlines(&clock, std::time::Duration::ZERO)
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            remaining: std::sync::atomic::AtomicU64::new(steps),
            process: inputs.process.incarnation_digest_bytes().map_err(anyhow::Error::msg)?,
            clock,
            deadline: deadline.min(inputs.cleanup_deadline),
        })
    }
    fn spend(&self, inputs: &LiveNativeCapacityInputs, steps: u64) -> Result<()> {
        inputs.require_original(true)?;
        ensure!(!self.deadline.has_elapsed(), "original capacity control deadline exhausted");
        ensure!(inputs.process.incarnation_digest_bytes().map_err(anyhow::Error::msg)? == self.process,
            "capacity control belongs to another original process");
        ensure!(matches!(inputs.clock.window_fixed(&self.clock, std::time::Duration::ZERO)
            .map_err(anyhow::Error::msg)?, lillux::time::OccupancyWindowState::Service { .. }),
            "original capacity control interval exhausted");
        self.remaining.fetch_update(std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst, |remaining| remaining.checked_sub(steps))
            .map_err(|_| anyhow::anyhow!("original capacity control work exhausted"))?;
        Ok(())
    }
}

struct AccountRecoveryAttempt {
    number: u64,
    budget: AccountControlBudget,
}

#[derive(Clone, Copy)]
enum AccountPhase {
    Opening,
    Ready,
    DebitPending(redo::AccountRedo),
    Debited,
    SettlementPending(redo::AccountRedo),
    Settled,
}

/// Actual account and redo descriptors are retained before the first mutation.
/// No Drop settlement, allowance refund or DTO-to-budget conversion exists.
/// Recovery shares these original record owners; its finite control capability
/// is consumed before those owners proceed to live debit, without reopening.
pub(super) struct RetainedCapacityAccount {
    original_incarnation: IncarnationDebit,
    issuance: [u8; 32],
    account: RetainedStore,
    anchor: anchor::RetainedAnchor,
    record: Option<CapacityRecord>,
    phase: AccountPhase,
    bootstrap: AccountControlBudget,
    recovery: Option<AccountRecoveryAttempt>,
    recovery_admission: Option<redo::AccountRedo>,
    cold_settlement: Option<redo::AccountRedo>,
    completed_recovery: Option<u64>,
    // The incarnation whose settlement this account's own debit followed,
    // captured from the durable anchor immediately before that debit.
    predecessor: Option<[u8; 32]>,
}

/// Exact account and redo source pins after durable settlement. Every field is
/// retained with `ManuallyDrop` until the consuming node finalizer registers
/// it in the already prepared physical inventory; ordinary Drop is not close
/// evidence for this post-settlement phase.
pub(super) struct RetainedCapacityFinalizationSources {
    pub(super) account_parent: Option<super::FinalizerDirectorySource>,
    pub(super) account_file: Option<super::FinalizerControlSource>,
    pub(super) anchor_parent: Option<super::FinalizerDirectorySource>,
    pub(super) anchor_file: Option<super::FinalizerControlSource>,
}

pub(super) fn quote_capacity_finalization_sources() -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    let directory_owner = std::mem::size_of::<PinnedDirectory>()
        .checked_add(2 * std::mem::size_of::<usize>())
        .and_then(|bytes| bytes.checked_add(std::mem::align_of::<PinnedDirectory>() - 1))
        .context("capacity source directory allocation quote overflow")?;
    let bytes = std::mem::size_of::<RetainedCapacityFinalizationSources>()
        .checked_add(directory_owner.checked_mul(2)
            .context("capacity source directory allocation quote overflow")?)
        .context("capacity source frame quote overflow")?;
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: u64::try_from(bytes)?,
        steps: 4,
    })
}

/// Work charged while checking settled account/redo bytes before their source
/// frame can move into finalizer custody. A refused transfer repeats this
/// validation on the next shared finalizer pass; the successful transfer is
/// cached and does not charge again.
pub(super) fn quote_capacity_finalization_validation()
    -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: 0,
        steps: ACCOUNT_READ_WORK
            .checked_add(anchor::READ_WORK)
            .context("capacity finalization validation work quote overflow")?,
    })
}

/// Work charged by one mapped-release settlement attempt. The anchor owner
/// quotes its worst bounded publication/recovery route; the account owner then
/// performs one charged predecessor read and target write. `reserve` multiplies
/// this by the shared finalizer-attempt limit.
pub(super) fn quote_capacity_settlement_attempt()
    -> Result<lillux_sqlite::original_physical_custody::OutsideWork> {
    Ok(lillux_sqlite::original_physical_custody::OutsideWork {
        bytes: 0,
        steps: anchor::maximum_publication_work()?
            .checked_add(ACCOUNT_READ_WORK)
            .and_then(|steps| steps.checked_add(ACCOUNT_WRITE_WORK))
            .context("capacity settlement work quote overflow")?,
    })
}

impl RetainedCapacityAccount {
    pub(super) fn open(inputs: &LiveNativeCapacityInputs) -> Result<Self> {
        inputs.require_original(false)?;
        let binding = &inputs.capacity_account;
        binding.require_inputs(inputs)?;
        // The recovery protocol uses bounded fixed records, one lower marker
        // (16 KiB), and eight bounded namespace entries. Include the retained
        // frame and simultaneous encode/read/verification scratch. These are
        // enclosing-node control bytes, never native or C allocator backing.
        let required_bytes = std::mem::size_of::<Self>().checked_add(8 * redo::ANCHOR_BYTES)
            .and_then(|n| n.checked_add(8 * RECORD_BYTES + 16 * 1024 + 8 * 4096))
            .context("capacity recovery layout overflow")?;
        ensure!(inputs.policy.recovery.control_storage_bytes >= u64::try_from(required_bytes)?,
            "capacity recovery control allowance cannot cover actual bounded layout");
        let required_work = 4 * anchor::PUBLICATION_WORK + 8 * anchor::READ_WORK
            + 4 * ACCOUNT_WRITE_WORK + 8 * ACCOUNT_READ_WORK + 4 * PROCESS_OBSERVATION_WORK;
        ensure!(inputs.policy.recovery.work_per_attempt >= required_work,
            "capacity recovery work allowance cannot cover one complete transition");
        let bootstrap = AccountControlBudget::original(inputs, BOOTSTRAP_WORK)?;
        bootstrap.spend(inputs, ACCOUNT_READ_WORK)?;
        let ai = inputs.root.open_child_directory(OsStr::new(".ai"))?
            .context("initialized capacity .ai directory is absent")?;
        let node = ai.open_child_directory(OsStr::new("node"))?
            .context("initialized capacity node directory is absent")?;
        let directory = node.open_child_directory(OsStr::new("native-capacity"))?
            .context("initialized capacity directory is absent")?;
        ensure!(directory.identity()? == binding.account_directory_identity, "capacity directory changed");
        let account = RetainedStore::open(directory.try_clone()?, binding.account_identity)?;
        let anchor = anchor::RetainedAnchor::retain(directory, binding.account_directory_identity)?;
        // Neither account parsing nor lower namespace recovery ran yet. The
        // reservation caches this whole owner before calling debit/recovery.
        Ok(Self {
            original_incarnation: Self::incarnation(inputs)?, issuance: binding.issuance_digest()?,
            account, anchor, record: None, phase: AccountPhase::Opening, bootstrap,
            recovery: None, recovery_admission: None, cold_settlement: None, completed_recovery: None,
            predecessor: None,
        })
    }

    fn incarnation(inputs: &LiveNativeCapacityInputs) -> Result<IncarnationDebit> {
        Ok(IncarnationDebit {
            process_digest: inputs.process.incarnation_digest_bytes().map_err(anyhow::Error::msg)?,
            process: redo::StoredProcess::capture(&inputs.process)?,
            clock_digest: fixed_digest(inputs.clock.binding_digest())?,
            completion_digest: sha2::Sha256::digest(&inputs.completion.bytes).into(),
            policy_generation_digest: fixed_digest(inputs.snapshot.generation_digest())?,
            units: inputs.policy.incarnation.units,
        })
    }

    fn require_inputs(&self, inputs: &LiveNativeCapacityInputs, cleanup: bool) -> Result<()> {
        inputs.require_original(cleanup)?;
        inputs.capacity_account.require_inputs(inputs)?;
        ensure!(Self::incarnation(inputs)? == self.original_incarnation,
            "capacity account borrowed inputs changed incarnation");
        ensure!(inputs.capacity_account.issuance_digest()? == self.issuance
            && inputs.capacity_account.account_identity == self.account.identity
            && inputs.capacity_account.account_directory_identity == self.account.parent.identity()?
            && self.anchor.directory_identity() == inputs.capacity_account.account_directory_identity,
            "capacity account borrowed inputs changed retained issuance/stores");
        self.account.require_original()
    }

    fn recovery_budget(&self) -> Result<&AccountControlBudget> {
        Ok(&self.recovery.as_ref().context("durable capacity recovery grant absent")?.budget)
    }

    fn ensure_recovery_attempt(&mut self, inputs: &LiveNativeCapacityInputs) -> Result<()> {
        self.require_inputs(inputs, true)?;
        if let Some(attempt) = &self.recovery {
            attempt.budget.spend(inputs, 1)?;
            return Ok(());
        }
        if self.recovery_admission.is_none() {
            let current = *self.anchor.current()?;
            if let Some(previous) = current.recovery_process() {
                self.bootstrap.spend(inputs, PROCESS_OBSERVATION_WORK)?;
                let exact = previous.exact()?;
                let completed_here = exact == inputs.process
                    && self.completed_recovery == Some(current.recovery_attempts());
                ensure!(completed_here || exact.has_ended().map_err(anyhow::Error::msg)?,
                    "previous recovery process remains live; retain its original owner");
            }
            self.recovery_admission = Some(current.charge_recovery(&inputs.process, &inputs.policy)?);
        }
        let admission = self.recovery_admission.expect("retained recovery admission");
        let budget = &self.bootstrap;
        self.anchor.publish(admission, &inputs.policy, &self.issuance, budget.deadline,
            &|steps| budget.spend(inputs, steps))?;
        // Install before any further fallible work. A crash has burned this
        // durable attempt; reopening cannot reconstruct a fresh same-process
        // counter from its record. No descriptor or original clock is cloned.
        self.recovery = Some(AccountRecoveryAttempt {
            number: admission.recovery_attempts(),
            budget: AccountControlBudget::original(inputs, inputs.policy.recovery.work_per_attempt)?,
        });
        self.recovery_admission = None;
        self.recovery_budget()?.spend(inputs, 1)
    }

    fn complete_recovery_control(&mut self) {
        if let Some(attempt) = self.recovery.take() {
            // Only affine counters/clock coordinates live in this value. The
            // actual original clock and record descriptors stay in their single
            // enclosing owners; no recovery alias escapes this transition.
            self.completed_recovery = Some(attempt.number);
        }
    }

    /// Spend one read from the opening budget and verify the document the
    /// `OperatorReissue` anchor names, with the operator key pinned under the
    /// initialized operator fingerprint. Read-only; nothing is repaired.
    fn require_reissue_document(&self, inputs: &LiveNativeCapacityInputs,
        current: &redo::AccountRedo, budget: &AccountControlBudget) -> Result<()> {
        budget.spend(inputs, anchor::READ_WORK)?;
        let onboarding = inputs.root.open_child_directory(OsStr::new(".ai"))?
            .context("initialized capacity .ai directory is absent")?
            .open_child_directory(OsStr::new("config"))?
            .context("initialized capacity config directory is absent")?
            .open_child_directory(OsStr::new("onboarding"))?
            .context("initialized capacity onboarding directory is absent")?;
        let trust = TrustStore::load(None, &inputs.root.path().join(".ai/config"))?;
        let operator = &trust.get(&inputs.operator_fingerprint)
            .context("capacity operator is not pinned")?.verifying_key;
        let input = verify_reissue_anchor_document(onboarding, operator, &inputs.operator_fingerprint,
            &inputs.node_fingerprint, &self.issuance, current)?;
        input.require_original()?;
        inputs.require_original(true)
    }

    fn prepare_open(&mut self, inputs: &LiveNativeCapacityInputs) -> Result<()> {
        ensure!(matches!(self.phase, AccountPhase::Opening), "capacity account is no longer opening");
        self.require_inputs(inputs, true)?;
        if self.recovery_admission.is_some() { self.ensure_recovery_attempt(inputs)?; }
        if let Some(settlement) = self.cold_settlement {
            let budget = self.recovery.as_ref().context("cold settlement lost recovery owner")?;
            self.anchor.publish(settlement, &inputs.policy, &self.issuance, budget.budget.deadline,
                &|steps| budget.budget.spend(inputs, steps))?;
            self.account.apply_redo(self.anchor.current()?, budget.budget.deadline,
                &|steps| budget.budget.spend(inputs, steps))?;
            self.record = Some(*self.anchor.current()?.target());
            self.phase = AccountPhase::Ready;
            self.cold_settlement = None;
            self.complete_recovery_control();
            return Ok(());
        }
        let budget = self.recovery.as_ref().map(|a| &a.budget).unwrap_or(&self.bootstrap);
        self.anchor.load(&inputs.policy, &self.issuance, budget.deadline,
            &|steps| budget.spend(inputs, steps))?;
        let current = *self.anchor.current()?;
        // An operator re-issuance anchor is admissible only while the document
        // that authorised it is present and verifies; this runs before any
        // repair so an absent or foreign document refuses the whole open.
        if current.operation() == redo::Operation::OperatorReissue {
            self.require_reissue_document(inputs, &current, budget)?;
        }
        let bytes = self.account.read_bytes(budget.deadline, &|steps| budget.spend(inputs, steps))?;
        ensure!(current.accepts_account_bytes(&bytes)?, "capacity account corruption has no permitted redo");
        let needs_repair = bytes != encode(current.target())?;
        if !needs_repair && current.target().outstanding.is_none() {
            self.record = Some(*current.target());
            self.phase = AccountPhase::Ready;
            self.complete_recovery_control();
            return Ok(());
        }
        // A new opening owner cannot adopt a still-live incarnation, including
        // an earlier attempt in this same process. The original live reservation
        // has its own never-mapped/release route below.
        if let Some(debit) = current.target().outstanding {
            budget.spend(inputs, PROCESS_OBSERVATION_WORK)?;
            ensure!(debit.process.exact()?.has_ended().map_err(anyhow::Error::msg)?,
                "outstanding native incarnation remains live or unproved");
        }
        self.ensure_recovery_attempt(inputs)?;
        let budget = self.recovery.as_ref().expect("durable original recovery owner");
        self.account.apply_redo(self.anchor.current()?, budget.budget.deadline,
            &|steps| budget.budget.spend(inputs, steps))?;
        let repaired = *self.anchor.current()?.target();
        if let Some(debit) = repaired.outstanding {
            budget.budget.spend(inputs, PROCESS_OBSERVATION_WORK)?;
            ensure!(debit.process.exact()?.has_ended().map_err(anyhow::Error::msg)?,
                "native predecessor disposition no longer established");
            let target = CapacityRecord { outstanding: None, ..repaired };
            let next = self.anchor.current()?.successor(redo::Operation::DeadIncarnationSettlement, target, &inputs.policy)?;
            self.cold_settlement = Some(next);
            self.anchor.publish(next, &inputs.policy, &self.issuance, budget.budget.deadline,
                &|steps| budget.budget.spend(inputs, steps))?;
            self.account.apply_redo(self.anchor.current()?, budget.budget.deadline,
                &|steps| budget.budget.spend(inputs, steps))?;
            self.cold_settlement = None;
        }
        self.record = Some(*self.anchor.current()?.target());
        self.phase = AccountPhase::Ready;
        self.complete_recovery_control();
        Ok(())
    }

    pub(super) fn debit(&mut self, inputs: &LiveNativeCapacityInputs) -> Result<()> {
        self.require_inputs(inputs, false)?;
        if matches!(self.phase, AccountPhase::Opening) { self.prepare_open(inputs)?; }
        ensure!(matches!(self.phase, AccountPhase::Ready), "capacity debit requires its retained opening owner");
        let before = self.record.context("capacity opening target absent")?;
        self.predecessor = self.anchor.current()?.settled_incarnation();
        let (sequence, consumed) = before.prepare_debit(inputs.policy.allowance, inputs.policy.incarnation.units)?;
        let next = CapacityRecord { sequence, consumed, outstanding: Some(self.original_incarnation), ..before };
        let redo = self.anchor.current()?.successor(redo::Operation::Debit, next, &inputs.policy)?;
        self.record = Some(next);
        self.phase = AccountPhase::DebitPending(redo);
        let budget = &self.bootstrap;
        self.anchor.publish(redo, &inputs.policy, &self.issuance, budget.deadline,
            &|steps| budget.spend(inputs, steps))?;
        self.account.apply_redo(self.anchor.current()?, budget.deadline,
            &|steps| budget.spend(inputs, steps))?;
        inputs.require_original(false)?;
        self.phase = AccountPhase::Debited;
        Ok(())
    }

    pub(super) fn settle_before_region(&mut self, inputs: &LiveNativeCapacityInputs,
        release: &lillux_sqlite::original_host_owner::backing::HostStorageReleased) -> Result<()> {
        release.require_original_process().map_err(anyhow::Error::msg)?;
        self.require_inputs(inputs, true)?;
        if matches!(self.phase, AccountPhase::Settled) { return Ok(()); }
        if matches!(self.phase, AccountPhase::Opening) { self.prepare_open(inputs)?; }
        if matches!(self.phase, AccountPhase::Ready) {
            ensure!(self.record.context("unacquired account record absent")?.outstanding.is_none(),
                "unacquired reservation retains another incarnation");
            self.phase = AccountPhase::Settled;
            return Ok(());
        }
        if let AccountPhase::DebitPending(target) = self.phase {
            // Resolve debit publication only until recovery admission begins.
            // That admission retains the same account images but advances the
            // recovery charge. An interrupted retry must finish its own target,
            // never publish the earlier debit over it or refund that charge.
            if self.recovery_admission.is_none() && self.recovery.is_none() {
                let budget = &self.bootstrap;
                self.anchor.publish(target, &inputs.policy, &self.issuance, budget.deadline,
                    &|steps| budget.spend(inputs, steps))?;
            }
        }
        // Actual retained pre-region release/never-mapped custody, not a report,
        // permits live pre-acquisition reconciliation under charged recovery.
        self.ensure_recovery_attempt(inputs)?;
        if matches!(self.phase, AccountPhase::DebitPending(_)) {
            let budget = self.recovery.as_ref().expect("original recovery owner");
            self.account.apply_redo(self.anchor.current()?, budget.budget.deadline,
                &|steps| budget.budget.spend(inputs, steps))?;
            self.phase = AccountPhase::Debited;
        }
        // A before-region release never served a window, so it never renews.
        self.prepare_settlement(inputs, None)?;
        let AccountPhase::SettlementPending(target) = self.phase else {
            anyhow::bail!("pre-region settlement lost its original transition");
        };
        let budget = self.recovery.as_ref().expect("original pre-region recovery owner");
        self.anchor.publish(target, &inputs.policy, &self.issuance, budget.budget.deadline,
            &|steps| budget.budget.spend(inputs, steps))?;
        self.account.apply_redo(self.anchor.current()?, budget.budget.deadline,
            &|steps| budget.budget.spend(inputs, steps))?;
        self.phase = AccountPhase::Settled;
        Ok(())
    }

    /// A pending settlement is retried as published; a renewal decided once
    /// cannot downgrade to a plain settlement, nor the reverse.
    fn prepare_settlement(&mut self, inputs: &LiveNativeCapacityInputs,
        renewal: Option<(CapacityRecord, [u8; 32])>) -> Result<()> {
        if matches!(self.phase, AccountPhase::SettlementPending(_)) { return Ok(()); }
        ensure!(matches!(self.phase, AccountPhase::Debited), "native settlement lacks completed original debit");
        let before = self.record.context("original native debit record absent")?;
        ensure!(before.outstanding == Some(self.original_incarnation), "native settlement changed its original incarnation");
        ensure!(self.anchor.current()?.target() == &before, "native settlement changed redo target");
        let redo = match renewal {
            Some((target, authorization)) => self.anchor.current()?
                .renew(redo::Operation::RolloverRenewal, target, authorization, &inputs.policy)?,
            None => {
                let target = CapacityRecord { outstanding: None, ..before };
                self.anchor.current()?.successor(redo::Operation::ReleaseSettlement, target, &inputs.policy)?
            }
        };
        self.record = Some(*redo.target());
        self.phase = AccountPhase::SettlementPending(redo);
        Ok(())
    }

    /// The account owner's own proof that this release ends a served window.
    /// The claim is advisory: every fact comes from the retained inputs and
    /// record, and any failed part yields `None`, a plain settlement, never a
    /// refusal that would strand an otherwise correct stop.
    fn rollover_renewal_target(&self, inputs: &LiveNativeCapacityInputs,
        claim: Option<super::RolloverClaim>) -> Result<Option<(CapacityRecord, [u8; 32])>> {
        let Some(super::RolloverClaim { latch_bits }) = claim else { return Ok(None); };
        if latch_bits & super::service_refusal::FACT != 0 { return Ok(None); }
        if !inputs.policy.renewal.at_orderly_rollover { return Ok(None); }
        let Some(before) = self.record else { return Ok(None); };
        if before.outstanding != Some(self.original_incarnation) { return Ok(None); }
        let window = inputs.clock.window_fixed(&inputs.occupancy,
            Duration::from_millis(inputs.policy.incarnation.cleanup_occupancy_ms))
            .map_err(anyhow::Error::msg)?;
        let drained = match window {
            lillux::time::OccupancyWindowState::Cleanup { .. }
            | lillux::time::OccupancyWindowState::Expired => true,
            lillux::time::OccupancyWindowState::Service { remaining } =>
                remaining <= Duration::from_millis(inputs.policy.incarnation.rollover_drain_margin_ms),
        };
        if !drained { return Ok(None); }
        let Some(epoch) = before.epoch.checked_add(1) else { return Ok(None); };
        if inputs.policy.renewal.maximum_epochs.is_some_and(|maximum| epoch > maximum) {
            return Ok(None);
        }
        let target = CapacityRecord { epoch, consumed: genesis_consumed(), outstanding: None, ..before };
        Ok(Some((target, rollover_authorization(&self.issuance, &before)?)))
    }

    pub(super) fn settle_released(&mut self, inputs: &LiveNativeCapacityInputs,
        release: &lillux_sqlite::original_host_owner::backing::HostStorageReleased,
        claim: Option<super::RolloverClaim>,
        pay: &dyn Fn(u64) -> Result<()>) -> Result<()> {
        release.require_original_process().map_err(anyhow::Error::msg)?;
        self.require_inputs(inputs, true)?;
        if matches!(self.phase, AccountPhase::Settled) { return Ok(()); }
        let renewal = self.rollover_renewal_target(inputs, claim)?;
        self.prepare_settlement(inputs, renewal)?;
        let AccountPhase::SettlementPending(target) = self.phase else { unreachable!("prepared settlement") };
        self.anchor.publish(target, &inputs.policy, &self.issuance, inputs.cleanup_deadline, pay)?;
        self.account.apply_redo(self.anchor.current()?, inputs.cleanup_deadline, pay)?;
        self.phase = AccountPhase::Settled;
        Ok(())
    }

    /// Verify the durable settled images one last time, then move their actual
    /// pins into an affine source frame for explicit descriptor retirement.
    /// A refusal returns the same complete account/anchor owner untouched.
    pub(super) fn into_finalization_sources(
        self,
        inputs: &LiveNativeCapacityInputs,
        pay: &dyn Fn(u64) -> Result<()>,
        outside_prepay: &dyn Fn(u64, u64) -> Result<()>,
    ) -> std::result::Result<RetainedCapacityFinalizationSources, (Self, anyhow::Error)> {
        let checked = (|| -> Result<()> {
            self.require_inputs(inputs, true)?;
            ensure!(matches!(self.phase, AccountPhase::Settled),
                "capacity account finalization requires durable settlement");
            let record = self.record.context("settled capacity record absent")?;
            ensure!(record.outstanding.is_none(), "settled capacity record retains native debt");
            let account_bytes = self.account.read_bytes(inputs.cleanup_deadline, pay)?;
            ensure!(account_bytes == encode(&record)?,
                "settled capacity account bytes changed before descriptor transfer");
            let redo = *self.anchor.current()?;
            ensure!(redo.target() == &record,
                "settled capacity anchor target differs from account record");
            self.anchor.require_finalization_source_ready(&redo, inputs.cleanup_deadline, pay)?;
            Ok(())
        })();
        if let Err(error) = checked {
            return Err((self, error));
        }
        let quote = match quote_capacity_finalization_sources() {
            Ok(quote) => quote,
            Err(error) => return Err((self, error)),
        };
        if let Err(error) = outside_prepay(quote.bytes, quote.steps) {
            return Err((self, error));
        }
        if let Err(error) = inputs.require_original(true) {
            return Err((self, error));
        }

        let RetainedCapacityAccount { account, anchor, .. } = self;
        let RetainedStore { parent: account_parent, pinned: account_file, .. } = account;
        let (anchor_parent, anchor_file) = anchor.into_finalization_parts();
        Ok(RetainedCapacityFinalizationSources {
            account_parent: Some(super::FinalizerDirectorySource::retained(Arc::new(account_parent))),
            account_file: Some(super::FinalizerControlSource::pinned_file(account_file)),
            anchor_parent: Some(super::FinalizerDirectorySource::retained(Arc::new(anchor_parent))),
            anchor_file: Some(super::FinalizerControlSource::pinned_file(anchor_file)),
        })
    }

    pub(super) fn resident_generation(&mut self, inputs: &LiveNativeCapacityInputs) -> Result<u64> {
        self.require_original_debit(inputs, false)?;
        let sequence = self.record.context("original native record absent")?.sequence;
        ensure!(sequence != 0, "native resident generation is unsequenced");
        Ok(sequence)
    }

    /// This account's own incarnation digest and the digest of the settled
    /// incarnation its debit followed, once the debit is durable.
    pub(super) fn incarnation_lineage(&self) -> Result<([u8; 32], Option<[u8; 32]>)> {
        ensure!(matches!(self.phase, AccountPhase::Debited), "original capacity debit is not durably active");
        Ok((self.original_incarnation.process_digest, self.predecessor))
    }

    /// Read-only view of the durable anchor for the boot log and status.
    pub(super) fn summary(&self, policy: &NodeNativeCapacityPolicy) -> Result<CapacityAccountSummary> {
        summarize(self.anchor.current()?, policy)
    }

    pub(super) fn require_original_debit(&self, inputs: &LiveNativeCapacityInputs, cleanup: bool) -> Result<()> {
        self.require_inputs(inputs, cleanup)?;
        ensure!(matches!(self.phase, AccountPhase::Debited), "original capacity debit is not durably active");
        let record = self.record.context("original native debit absent")?;
        ensure!(record.outstanding == Some(self.original_incarnation), "original capacity incarnation changed");
        // Fixed read-only admission observations run within the caller's existing
        // node/native operation. They issue no repair, cleanup or allocation grant.
        self.anchor.require_current(inputs.cleanup_deadline, &|_| Ok(()))?;
        ensure!(self.anchor.current()?.target() == &record
            && self.account.read_bytes(inputs.cleanup_deadline, &|_| Ok(()))? == encode(&record)?,
            "original capacity debit no longer matches durable account/redo");
        inputs.require_original(cleanup)
    }
}

/// Read-only view of the account for status and doctor. Decoding is not
/// recovery: a torn or unresolved anchor is reported, never repaired.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityAccountSummary {
    pub epoch: u64,
    pub maximum_epochs: Option<u64>,
    pub sequence: u64,
    pub outstanding_incarnation: bool,
    pub recovery_attempts_used: u64,
    pub recovery_attempts_maximum: u64,
    /// Allowance minus consumption, saturating per dimension.
    pub remaining: NativeCapacityUnits,
    /// Whole incarnations the remaining allowance still funds.
    pub incarnations_remaining: u64,
    pub last_operation: String,
    pub renewal: NativeRenewalEnvelope,
}

fn summarize(redo: &redo::AccountRedo, policy: &NodeNativeCapacityPolicy) -> Result<CapacityAccountSummary> {
    let target = redo.target();
    let NativeCapacityUnits {
        storage_bytes, host_control_storage_bytes, allocator_steps, connection_tickets, cleanup_steps,
    } = policy.allowance;
    let remaining = NativeCapacityUnits {
        storage_bytes: storage_bytes.saturating_sub(target.consumed.storage_bytes),
        host_control_storage_bytes: host_control_storage_bytes
            .saturating_sub(target.consumed.host_control_storage_bytes),
        allocator_steps: allocator_steps.saturating_sub(target.consumed.allocator_steps),
        connection_tickets: connection_tickets.saturating_sub(target.consumed.connection_tickets),
        cleanup_steps: cleanup_steps.saturating_sub(target.consumed.cleanup_steps),
    };
    let per = policy.incarnation.units;
    // A zero-sized dimension funds any number of incarnations; the minimum
    // over the sized dimensions is what the next debits can still cover.
    let incarnations_remaining = [
        (remaining.storage_bytes, per.storage_bytes),
        (remaining.host_control_storage_bytes, per.host_control_storage_bytes),
        (remaining.allocator_steps, per.allocator_steps),
        (remaining.connection_tickets, per.connection_tickets),
        (remaining.cleanup_steps, per.cleanup_steps),
    ].into_iter().filter_map(|(left, unit)| left.checked_div(unit)).min().unwrap_or(0);
    Ok(CapacityAccountSummary {
        epoch: target.epoch,
        maximum_epochs: policy.renewal.maximum_epochs,
        sequence: target.sequence,
        outstanding_incarnation: target.outstanding.is_some(),
        recovery_attempts_used: redo.recovery_attempts(),
        recovery_attempts_maximum: policy.recovery.maximum_attempts,
        remaining,
        incarnations_remaining,
        last_operation: redo.operation().name().to_owned(),
        renewal: policy.renewal,
    })
}

/// Decode the durable anchor of an initialized root without opening the
/// record owners, taking the lock or writing. `None` before initialization;
/// an absent or unresolved anchor is an error the caller reports as such.
pub fn inspect_capacity_account(app_root: &Path) -> Result<Option<CapacityAccountSummary>> {
    let Some(report) = crate::init::verify_init_completion(app_root)? else { return Ok(None); };
    let binding = report.capacity_account;
    binding.validate()?;
    let policy = &binding.policy;
    let issuance = binding.issuance_digest()?;
    let directory = PinnedDirectory::open(&app_root.join(".ai/node/native-capacity"))?
        .context("capacity directory absent")?;
    ensure!(directory.identity()? == binding.account_directory_identity,
        "capacity directory differs from original issuance");
    let anchor = directory.open_pinned_regular(OsStr::new("anchor"), false)?
        .context("capacity redo is unresolved; only its live owner recovers it")?;
    anchor.require_effective_user_owned()?;
    ensure!(anchor.observation()?.size() == redo::ANCHOR_BYTES as u64, "capacity redo size changed");
    let deadline = lillux::time::MonotonicDeadline::after(
        Duration::from_millis(policy.recovery.maximum_duration_ms));
    let mut bytes = [0; redo::ANCHOR_BYTES];
    anchor.read_exact_stable_into(&mut bytes, deadline)?;
    let current = redo::AccountRedo::decode(&bytes, policy, &issuance)?;
    Ok(Some(summarize(&current, policy)?))
}

#[cfg(test)]
mod issuance_digest_tests {
    use super::*;

    #[test]
    fn outside_account_quotes_cover_settlement_and_finalization_checks() {
        let publication = anchor::maximum_publication_work().unwrap();
        assert_eq!(publication,
            3 * anchor::READ_WORK + 3 * anchor::NAMESPACE_WORK
                + 3 * anchor::PUBLICATION_WORK + 17);

        let validation = quote_capacity_finalization_validation().unwrap();
        assert_eq!(validation.bytes, 0);
        assert_eq!(validation.steps, ACCOUNT_READ_WORK + anchor::READ_WORK);

        let settlement = quote_capacity_settlement_attempt().unwrap();
        assert_eq!(settlement.bytes, 0);
        assert_eq!(settlement.steps,
            publication + ACCOUNT_READ_WORK + ACCOUNT_WRITE_WORK);

        let sources = quote_capacity_finalization_sources().unwrap();
        assert_eq!(sources.steps, 4);
        assert!(sources.bytes > 0);
    }

    #[test]
    fn settlement_retry_accepts_only_its_retained_write_prefix() {
        let before = [0x19; RECORD_BYTES];
        let target = [0xa4; RECORD_BYTES];
        for cut in 0..=RECORD_BYTES {
            let mut interrupted = before;
            interrupted[..cut].copy_from_slice(&target[..cut]);
            assert!(settlement_transition_bytes(&interrupted, &before, &target, true));
            assert_eq!(settlement_transition_bytes(&interrupted, &before, &target, false),
                cut == 0 || cut == RECORD_BYTES);
        }
        let mut foreign = before;
        foreign[RECORD_BYTES / 2] = 0x33;
        assert!(!settlement_transition_bytes(&foreign, &before, &target, true));
        let mut reordered = before;
        reordered[0] = target[0];
        reordered[2] = target[2];
        assert!(!settlement_transition_bytes(&reordered, &before, &target, true));
    }

    #[test]
    fn previous_record_format_is_foreign_even_with_a_valid_checksum() {
        let (_, genesis, _) = redo::controls::fixture();
        let mut previous = encode(genesis.target()).unwrap();
        previous[..8].copy_from_slice(b"RYECAP04");
        previous[8..12].copy_from_slice(&4u32.to_le_bytes());
        let checksum: [u8; 32] = sha2::Sha256::digest(&previous[..PAYLOAD_BYTES]).into();
        previous[PAYLOAD_BYTES..].copy_from_slice(&checksum);
        assert_eq!(decode(&previous).unwrap_err(), CapacityRecordRefusal::Format);
        // An epoch of zero is not an opened grant, whatever the checksum says.
        let unopened = CapacityRecord { epoch: 0, ..*genesis.target() };
        assert_eq!(encode(&unopened).unwrap_err(), CapacityRecordRefusal::Epoch);
        let mut zeroed = encode(genesis.target()).unwrap();
        zeroed[12..20].copy_from_slice(&0u64.to_le_bytes());
        let checksum: [u8; 32] = sha2::Sha256::digest(&zeroed[..PAYLOAD_BYTES]).into();
        zeroed[PAYLOAD_BYTES..].copy_from_slice(&checksum);
        assert_eq!(decode(&zeroed).unwrap_err(), CapacityRecordRefusal::Epoch);
    }

    #[test]
    fn rollover_authorization_names_the_served_incarnation_from_the_record() {
        let (_, genesis, debited) = redo::controls::fixture();
        assert!(rollover_authorization(&[7; 32], genesis.target()).is_err());
        let digest = rollover_authorization(&[7; 32], &debited).unwrap();
        assert_ne!(digest, [0; 32]);
        assert_eq!(digest, rollover_authorization(&[7; 32], &debited).unwrap());
        assert_ne!(digest, rollover_authorization(&[8; 32], &debited).unwrap());
        assert_ne!(digest, rollover_authorization(&[7; 32], &CapacityRecord { epoch: 2, ..debited }).unwrap());
        assert_ne!(digest, rollover_authorization(&[7; 32], &CapacityRecord { sequence: 2, ..debited }).unwrap());
    }

    #[test]
    fn reissuance_document_refuses_tampered_foreign_and_unsigned_bodies() {
        let operator = SigningKey::from_bytes(&[5; 32]);
        let fingerprint = ryeos_engine::trust::compute_fingerprint(&operator.verifying_key());
        let issuance = [0x31; 32];
        let body = ReissuanceBody {
            schema: REISSUANCE_SCHEMA.to_owned(),
            operator_fingerprint: fingerprint.clone(),
            node_fingerprint: "e".repeat(64),
            issuance_digest: hex_digest(&issuance),
            from_epoch: 3,
            to_epoch: 4,
            anchor_digest: "0".repeat(64),
            previous_reissuance_digest: None,
            reason: "exhausted".to_owned(),
            issued_at: "2026-10-14T00:00:00Z".to_owned(),
        };
        let sign = |body: &ReissuanceBody, key: &SigningKey| {
            let canonical = lillux::canonical_json(&serde_json::to_value(body).unwrap()).unwrap();
            let signature = key.sign(canonical.as_bytes());
            serde_json::to_vec(&ReissuanceDocument {
                body: body.clone(),
                signature: format!("ed25519:{}",
                    base64::engine::general_purpose::STANDARD.encode(signature.to_bytes())),
            }).unwrap()
        };
        let verify = |bytes: &[u8]| verify_reissuance_document(bytes, &operator.verifying_key(),
            &fingerprint, &"e".repeat(64), &issuance);
        let good = sign(&body, &operator);
        assert_eq!(verify(&good).unwrap(), body);
        assert!(verify(&sign(&body, &SigningKey::from_bytes(&[6; 32]))).is_err(), "foreign key");
        let mut tampered: ReissuanceDocument = serde_json::from_slice(&good).unwrap();
        tampered.body.to_epoch = 5;
        assert!(verify(&serde_json::to_vec(&tampered).unwrap()).is_err(), "tampered body");
        for (label, edit) in [
            ("schema", Box::new(|b: &mut ReissuanceBody| b.schema = "ryeos/native-capacity-issuance/v3".into()) as Box<dyn Fn(&mut ReissuanceBody)>),
            ("node", Box::new(|b: &mut ReissuanceBody| b.node_fingerprint = "f".repeat(64))),
            ("issuance", Box::new(|b: &mut ReissuanceBody| b.issuance_digest = "0".repeat(64))),
            ("operator", Box::new(|b: &mut ReissuanceBody| b.operator_fingerprint = "a".repeat(64))),
            ("epoch", Box::new(|b: &mut ReissuanceBody| b.to_epoch = 6)),
            ("anchor digest", Box::new(|b: &mut ReissuanceBody| b.anchor_digest = "xyz".into())),
            ("reason", Box::new(|b: &mut ReissuanceBody| b.reason = "r".repeat(1025))),
        ] {
            let mut foreign = body.clone();
            edit(&mut foreign);
            assert!(verify(&sign(&foreign, &operator)).is_err(), "{label}");
        }
        assert!(verify(b"{}").is_err());
    }

    #[test]
    fn streamed_binding_digest_matches_canonical_protocol() {
        for (maximum, maximum_epochs) in [(false, None), (true, Some(8))] {
            let units = NativeCapacityUnits {
                storage_bytes: if maximum { i64::MAX as u64 } else { 65_537 },
                host_control_storage_bytes: if maximum { i64::MAX as u64 } else { 4_096 },
                allocator_steps: if maximum { i64::MAX as u64 } else { 1_000 },
                connection_tickets: if maximum { 64 } else { 1 },
                cleanup_steps: if maximum { i64::MAX as u64 - 1 } else { 8 },
            };
            let directory: PinnedDirectoryIdentity = serde_json::from_value(serde_json::json!({
                "containing_device": u64::MAX, "inode": 1,
            })).unwrap();
            let account: PinnedRegularFileIdentity = serde_json::from_value(serde_json::json!({
                "containing_device": 2, "inode": u64::MAX,
            })).unwrap();
            let mut binding = NativeCapacityAccountBinding {
                schema: 6,
                node_fingerprint: "f".repeat(64),
                root_identity: directory,
                runtime_identity: directory,
                account_directory_identity: directory,
                account_identity: account,
                anchor_protocol: anchor::PROTOCOL.to_owned(),
                policy: NodeNativeCapacityPolicy {
                    schema: 6,
                    issuance_nonce: "a".repeat(32),
                    allowance: units,
                    incarnation: ryeos_node_runtime::node_policy::sections::native_capacity::NativeIncarnationEnvelope {
                        units, maximum_occupancy_ms: 3, cleanup_occupancy_ms: 1, rollover_drain_margin_ms: 1,
                        host_release_steps: 2, outside_cleanup_steps: 1,
                    },
                    recovery: ryeos_node_runtime::node_policy::sections::native_capacity::NativeRecoveryEnvelope {
                        maximum_attempts: 32, work_per_attempt: 65_536,
                        control_storage_bytes: 262_144, maximum_duration_ms: 120_000,
                    },
                    renewal: NativeRenewalEnvelope {
                        at_orderly_rollover: maximum, operator_reissue: !maximum, maximum_epochs,
                    },
                },
            };
            binding.policy.validate().unwrap();
            let canonical = lillux::canonical_json(&serde_json::to_value(&binding).unwrap()).unwrap();
            let expected: [u8; 32] = sha2::Sha256::digest(canonical.as_bytes()).into();
            assert_eq!(binding.issuance_digest().unwrap(), expected);
            binding.policy.incarnation.outside_cleanup_steps += 1;
            assert_ne!(binding.issuance_digest().unwrap(), expected,
                "outside cleanup allocation must bind to the original issuance");
        }
    }
}
