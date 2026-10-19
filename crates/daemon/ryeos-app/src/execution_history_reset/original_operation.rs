//! Original reset evidence. Staging is inert until State activates its exact
//! reference; this module does not grant allocation or recreate native owners.

use std::ffi::OsStr;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use ryeos_scheduler_contract::{
    SchedulerFireHistoryAccounting, SchedulerRetirementStoppedRolePlan,
    SchedulerRetirementStoppedSource,
};
use ryeos_state::{CasMutationGuard, PinnedStateAuthority, ThreadHistoryDiscardOperationRef};
use serde::{Deserialize, Serialize};

use super::original_quota_claim::OriginalQuotaClaim;
use super::{ExecutionHistoryResetOptions, ExecutionHistorySchemaCut, OfflineResetAuthority};
use ryeos_node_runtime::identity::NodeIdentity;
use crate::runtime_db::{RuntimeDb, SqliteInspectionCopyLease};

pub(super) const PREFIX: &str = "execution-history-reset.operation.";
const MAX_BYTES: usize = 8192;

fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationRecord {
    schema: u32,
    purpose: String,
    operation_id: String,
    owner_fingerprint: String,
    runtime_identity: String,
    app_root_identity: String,
    discard_project_heads: bool,
    #[serde(deserialize_with = "required_nullable")]
    schema_cut: Option<ExecutionHistorySchemaCut>,
    scheduler_selected: bool,
    scheduler_accounting: SchedulerFireHistoryAccounting,
    scheduler_sources: [SchedulerRetirementStoppedRolePlan; 3],
}

/// A signed staged operation file of `operation_id` with no scheduler
/// selected, for recovery tests.
#[cfg(test)]
pub(super) fn fixture_signed_operation(identity: &NodeIdentity, operation_id: &str) -> Vec<u8> {
    let record = OperationRecord {
        schema: 2,
        purpose: "execution_history_reset_operation_v2".to_owned(),
        operation_id: operation_id.to_owned(),
        owner_fingerprint: identity.fingerprint().to_owned(),
        runtime_identity: "fixture".to_owned(),
        app_root_identity: "fixture".to_owned(),
        discard_project_heads: false,
        schema_cut: None,
        scheduler_selected: false,
        scheduler_accounting: SchedulerFireHistoryAccounting::Exact { counts: Default::default() },
        scheduler_sources: std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
            source: SchedulerRetirementStoppedSource::Absent,
            current: None,
        }),
    };
    let body = lillux::canonical_json(&serde_json::to_value(&record).unwrap()).unwrap();
    lillux::signature::sign_content(&body, identity.signing_key(), "#", None).into_bytes()
}

/// The name `name` still holds the inode `pin` was opened on.
fn require_pinned_entry(runtime: &lillux::PinnedDirectory, name: &str, pin: &std::fs::File) -> Result<()> {
    let pinned = lillux::observe_open_file_identity(pin)?;
    let entry = runtime
        .entry_no_follow(OsStr::new(name))?
        .context("staged reset operation disappeared")?;
    ensure!(
        entry.entry_type == lillux::PinnedEntryType::Regular
            && entry.containing_device == pinned.device()
            && entry.inode == pinned.inode(),
        "staged reset operation was replaced"
    );
    Ok(())
}

fn require_same_file(left: &std::fs::File, right: &std::fs::File) -> Result<()> {
    let (left, right) = (lillux::observe_open_file_identity(left)?, lillux::observe_open_file_identity(right)?);
    ensure!(
        (left.device(), left.inode()) == (right.device(), right.inode()),
        "staged reset operation inode changed between stagings"
    );
    Ok(())
}

/// Authenticate a staged operation file as this node's signed record of
/// `operation_id`, without admitting it as an operation.
pub(super) fn require_own_operation_file(bytes: &[u8], identity: &NodeIdentity, operation_id: &str) -> Result<()> {
    ensure!(bytes.len() <= MAX_BYTES, "staged reset operation exceeds its byte bound");
    let raw = std::str::from_utf8(bytes)?;
    let (body, header) = lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)?;
    let header = header.context("staged reset operation is unsigned")?;
    ensure!(
        lillux::signature::is_valid_signature_for(
            &header.content_hash,
            &header.signature_b64,
            &header.signer_fingerprint,
            &body,
            identity.verifying_key(),
            identity.fingerprint()
        ),
        "staged reset operation signature is invalid"
    );
    let record: OperationRecord = serde_json::from_str(&body)?;
    ensure!(
        record.operation_id == operation_id && record.owner_fingerprint == identity.fingerprint(),
        "staged reset operation names another operation or signer"
    );
    Ok(())
}

pub(super) struct OriginalOperation {
    record: OperationRecord,
    reference: ThreadHistoryDiscardOperationRef,
    signed: Vec<u8>,
    app_root: lillux::PinnedDirectory,
    runtime: lillux::PinnedDirectory,
    // This is the same producer lease, never reconstructed from copied paths.
    // Released once the reset quiesces, so its copy work ends before the
    // claim's lifelines.
    source_lease: std::sync::Mutex<Option<Arc<SqliteInspectionCopyLease>>>,
    // The funded claim this operation runs under. A fresh operation is bound
    // when prepared; a resumed one when its new claim adopts it. Weak, because
    // a resuming claim already holds this operation as its prior one.
    quota_claim: std::sync::OnceLock<std::sync::Weak<OriginalQuotaClaim>>,
    /// Metadata descriptor of the staged operation file, kept from staging
    /// (or from reading an active operation) so its removal acts on the very
    /// inode staged rather than on whatever the name holds later.
    staged: std::sync::OnceLock<std::fs::File>,
}

/// Preserve the same operation and its original descriptor/copy owners on
/// publication or durability failure. A State marker may have been installed
/// before an activation error; this error grants no permission to replay effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicationPhase {
    Staging,
    StateActivation,
    PreActivationAdmission,
}

pub(super) struct OperationPublicationFailure {
    error: anyhow::Error,
    operation: Arc<OriginalOperation>,
    phase: PublicationPhase,
}

impl std::fmt::Debug for OperationPublicationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OperationPublicationFailure")
            .field("error", &self.error)
            .field("phase", &self.phase)
            .field("operation_id", &self.operation.reference.operation_id)
            .finish()
    }
}

impl std::fmt::Display for OperationPublicationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "original reset {:?} retains its exact owner: {}",
            self.phase, self.error
        )
    }
}

impl std::error::Error for OperationPublicationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl OriginalOperation {
    /// Live mutation must run under the exact funded claim bound to this
    /// operation. Signed cold evidence alone cannot renew a resource owner or
    /// attach a fresh allocation to the old operation's effects.
    pub(super) fn require_quota_claim(&self, claim: &Arc<OriginalQuotaClaim>) -> Result<()> {
        ensure!(
            self.quota_claim
                .get()
                .and_then(std::sync::Weak::upgrade)
                .is_some_and(|bound| Arc::ptr_eq(&bound, claim)),
            "original operation is not bound to this exact funded claim; cold recovery admission required"
        );
        Ok(())
    }

    /// Bind a resumed operation to the new claim prepared for it. The claim
    /// must name this exact operation (the same owner and reference) as its
    /// prior one, and the operation must not be bound already.
    pub(super) fn adopt_claim(self: &Arc<Self>, claim: &Arc<OriginalQuotaClaim>) -> Result<()> {
        let prior = claim
            .prior_operation()
            .context("claim was not prepared to resume an operation")?;
        ensure!(
            Arc::ptr_eq(prior, self) && prior.reference() == self.reference(),
            "claim was prepared to resume another operation"
        );
        self.quota_claim
            .set(Arc::downgrade(claim))
            .map_err(|_| anyhow::anyhow!("resumed operation is already bound to a funded claim"))
    }

    /// End the scheduler inspection lease this operation was prepared from.
    /// Its copy work then no longer outlives the release; a second call is a
    /// no-op.
    pub(super) fn release_source_lease(&self) {
        let lease = self
            .source_lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(lease);
    }

    pub(super) fn reference(&self) -> &ThreadHistoryDiscardOperationRef {
        &self.reference
    }

    pub(super) fn accounting(&self) -> &SchedulerFireHistoryAccounting {
        &self.record.scheduler_accounting
    }

    pub(super) fn signed_bytes(&self) -> &[u8] {
        &self.signed
    }

    pub(super) fn scheduler_sources(&self) -> &[SchedulerRetirementStoppedRolePlan; 3] {
        &self.record.scheduler_sources
    }

    pub(super) fn initial_generation(&self) -> Result<[u8; 16]> {
        self.reference.validate()?;
        let mut bytes = [0; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&self.reference.operation_id[index * 2..index * 2 + 2], 16)?;
        }
        Ok(bytes)
    }

    fn require_app_root(&self) -> Result<()> {
        let current = lillux::PinnedDirectory::open(self.app_root.path())?
            .context("original reset app root is absent")?;
        ensure!(
            current.is_same_directory(&self.app_root)?
                && self.record.app_root_identity
                    == serde_json::to_string(&self.app_root.identity()?)?,
            "original reset operation selects a different app root"
        );
        Ok(())
    }

    fn check_record(
        &self,
        runtime: &lillux::PinnedDirectory,
        identity: &NodeIdentity,
        options: &ExecutionHistoryResetOptions,
        scheduler_selected: bool,
    ) -> Result<()> {
        self.reference.validate()?;
        self.require_app_root()?;
        ensure!(
            self.record.schema == 2
                && self.record.purpose == "execution_history_reset_operation_v2",
            "unsupported original execution-history reset operation"
        );
        ensure!(
            self.record.operation_id == self.reference.operation_id
                && lillux::sha256_hex(&self.signed) == self.reference.sha256,
            "original reset reference differs from its exact signed bytes"
        );
        ensure!(
            self.record.owner_fingerprint == identity.fingerprint()
                && self.record.runtime_identity == serde_json::to_string(&runtime.identity()?)?,
            "original reset operation selects a different signer or runtime"
        );
        ensure!(
            self.record.discard_project_heads == options.discard_project_heads
                && self.record.schema_cut == options.schema_cut
                && self.record.scheduler_selected == scheduler_selected,
            "reset options or scheduler selection differ from the original operation"
        );
        let mut physical_sources = std::collections::BTreeSet::new();
        for source in &self.record.scheduler_sources {
            match (&source.source, &source.current) {
                (SchedulerRetirementStoppedSource::Absent, None) => {}
                (SchedulerRetirementStoppedSource::Canonical, Some(identity)) => {
                    identity.validate()?;
                    ensure!(
                        physical_sources.insert((identity.device, identity.inode)),
                        "original scheduler roles alias the same physical source"
                    );
                    ensure!(
                        identity.content_sha256.is_some(),
                        "original source lacks its copied digest"
                    );
                }
                _ => anyhow::bail!("original reset source is not canonical or absent"),
            }
        }
        if self.record.scheduler_sources[0].current.is_none() {
            ensure!(
                self.record
                    .scheduler_sources
                    .iter()
                    .all(|source| source.current.is_none()),
                "original scheduler main is absent with retained sidecars"
            );
            ensure!(
                self.record.scheduler_accounting
                    == SchedulerFireHistoryAccounting::Exact {
                        counts: Default::default(),
                    },
                "absent original scheduler cannot supply nonzero or unavailable accounting"
            );
        }
        ensure!(
            self.record.scheduler_selected || physical_sources.is_empty(),
            "unselected scheduler has retained original sources"
        );
        if let SchedulerFireHistoryAccounting::Exact { counts } = &self.record.scheduler_accounting
        {
            ensure!(
                counts.total_rows().is_some(),
                "original scheduler accounting overflows"
            );
        }
        Ok(())
    }

    /// Called before scheduler preview. Only the original signed accounting
    /// is returned on re-entry; missing canonical P never manufactures zero.
    pub(super) fn read_active(
        app_root: &std::path::Path,
        state: &PinnedStateAuthority,
        guard: &CasMutationGuard,
        runtime: &lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        identity: &NodeIdentity,
        options: &ExecutionHistoryResetOptions,
        scheduler_selected: bool,
    ) -> Result<Option<Arc<Self>>> {
        state.ensure_guard(guard)?;
        namespace.ensure_protects(runtime)?;
        ensure!(
            state.runtime_directory().is_same_directory(runtime)?,
            "original reset State owner differs from runtime"
        );
        let Some(reference) = state
            .require_recovery()?
            .thread_history_discard_operation()?
        else {
            return Ok(None);
        };
        reference.validate()?;
        let name = format!("{PREFIX}{}", reference.operation_id);
        let staged = runtime
            .open_mount_entry(OsStr::new(&name))?
            .context("active reset marker lacks its staged signed operation")?;
        let signed = lillux_sqlite::read_regular_without_sqlite_alias(
            runtime,
            OsStr::new(&name),
            MAX_BYTES,
        )?
        .context("active reset marker lacks its staged signed operation")?;
        require_pinned_entry(runtime, &name, &staged)?;
        ensure!(
            lillux::sha256_hex(&signed) == reference.sha256,
            "active reset operation digest mismatch"
        );
        let raw = std::str::from_utf8(&signed)?;
        let (body, header) =
            lillux::signature::strip_canonical_signature_with_envelope(raw, "#", None, false)?;
        let header = header.context("original reset operation is unsigned")?;
        ensure!(
            lillux::signature::is_valid_signature_for(
                &header.content_hash,
                &header.signature_b64,
                &header.signer_fingerprint,
                &body,
                identity.verifying_key(),
                identity.fingerprint()
            ),
            "original reset operation signature is invalid"
        );
        let record: OperationRecord = serde_json::from_str(&body)?;
        ensure!(
            lillux::canonical_json(&serde_json::to_value(&record)?)? == body,
            "original reset operation is not canonical"
        );
        let app_root =
            lillux::PinnedDirectory::open(app_root)?.context("active reset app root is absent")?;
        let operation = Self {
            record,
            reference,
            signed,
            app_root,
            runtime: runtime.try_clone()?,
            source_lease: std::sync::Mutex::new(None),
            quota_claim: std::sync::OnceLock::new(),
            staged: std::sync::OnceLock::from(staged),
        };
        operation.check_record(runtime, identity, options, scheduler_selected)?;
        Ok(Some(Arc::new(operation)))
    }

    /// Private issuer called in the actual pre-effect frame. Signing/staging
    /// does not activate the record and supplies no resource-funding grant.
    pub(super) fn prepare(
        authority: &OfflineResetAuthority<'_>,
        db: &RuntimeDb,
        state: &PinnedStateAuthority,
        guard: &CasMutationGuard,
        identity: &NodeIdentity,
        options: &ExecutionHistoryResetOptions,
        scheduler_selected: bool,
        accounting: &SchedulerFireHistoryAccounting,
        source_lease: Option<Arc<SqliteInspectionCopyLease>>,
        quota_claim: &Arc<OriginalQuotaClaim>,
    ) -> Result<Arc<Self>> {
        authority.require_held(db)?;
        state.ensure_guard(guard)?;
        ensure!(
            guard.is_exclusive()
                && state
                    .runtime_directory()
                    .is_same_directory(authority.runtime_directory)?,
            "original reset preparation requires its actual exclusive State owner"
        );
        let runtime = authority.runtime_directory;
        let names = [
            "scheduler.sqlite3",
            "scheduler.sqlite3-wal",
            "scheduler.sqlite3-shm",
            "scheduler.sqlite3-journal",
        ];
        let sources = if let Some(lease) = &source_lease {
            let receipts = lease
                .source_receipts
                .get()
                .context("original copy receipts are incomplete")?;
            ensure!(
                receipts.len() == names.len()
                    && receipts
                        .iter()
                        .zip(names)
                        .all(|((name, _), expected)| name == OsStr::new(expected))
                    && receipts[3].1.is_none(),
                "original scheduler source role grammar differs"
            );
            std::array::from_fn(|index| match &receipts[index].1 {
                None => SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Absent,
                    current: None,
                },
                Some(receipt) => SchedulerRetirementStoppedRolePlan {
                    source: SchedulerRetirementStoppedSource::Canonical,
                    current: Some(ryeos_scheduler_contract::SchedulerRetirementRoleIdentity {
                        device: receipt.source_identity.device,
                        inode: receipt.source_identity.inode,
                        byte_length: receipt.source_identity.byte_length,
                        content_sha256: Some(receipt.copied_sha256.clone()),
                    }),
                },
            })
        } else {
            for name in names {
                ensure!(
                    runtime.entry_no_follow(OsStr::new(name))?.is_none(),
                    "original scheduler accounting lacks source-copy provenance"
                );
            }
            std::array::from_fn(|_| SchedulerRetirementStoppedRolePlan {
                source: SchedulerRetirementStoppedSource::Absent,
                current: None,
            })
        };
        let app_root = lillux::PinnedDirectory::open(authority.app_root)?
            .context("original reset preparation app root is absent")?;
        let operation_id = quota_claim
            .require_original_identity(runtime, identity, authority.app_root)?
            .to_owned();
        let record = OperationRecord {
            schema: 2,
            purpose: "execution_history_reset_operation_v2".to_owned(),
            operation_id: operation_id.clone(),
            owner_fingerprint: identity.fingerprint().to_owned(),
            runtime_identity: serde_json::to_string(&runtime.identity()?)?,
            app_root_identity: serde_json::to_string(&app_root.identity()?)?,
            discard_project_heads: options.discard_project_heads,
            schema_cut: options.schema_cut,
            scheduler_selected,
            scheduler_accounting: accounting.clone(),
            scheduler_sources: sources,
        };
        let body = lillux::canonical_json(&serde_json::to_value(&record)?)?;
        let signed =
            lillux::signature::sign_content(&body, identity.signing_key(), "#", None).into_bytes();
        ensure!(
            signed.len() <= MAX_BYTES,
            "original signed reset operation exceeds its byte bound"
        );
        let reference = ThreadHistoryDiscardOperationRef {
            operation_id,
            sha256: lillux::sha256_hex(&signed),
        };
        let operation = Self {
            record,
            reference,
            signed,
            app_root,
            runtime: runtime.try_clone()?,
            source_lease: std::sync::Mutex::new(source_lease),
            quota_claim: std::sync::OnceLock::from(Arc::downgrade(quota_claim)),
            staged: std::sync::OnceLock::new(),
        };
        operation.check_record(runtime, identity, options, scheduler_selected)?;
        Ok(Arc::new(operation))
    }

    /// Exact bytes are memoized before installation. Retrying never chooses
    /// another operation ID or signs/re-previews the original accounting.
    pub(super) fn stage(self: &Arc<Self>, runtime: &lillux::PinnedDirectory) -> Result<()> {
        self.stage_retained(runtime).map_err(|error| {
            anyhow::Error::new(OperationPublicationFailure {
                error,
                operation: Arc::clone(self),
                phase: PublicationPhase::Staging,
            })
        })
    }

    fn stage_retained(&self, runtime: &lillux::PinnedDirectory) -> Result<()> {
        ensure!(
            self.runtime.is_same_directory(runtime)?,
            "original reset staging requires its retained runtime descriptor"
        );
        self.require_app_root()?;
        self.reference.validate()?;
        ensure!(
            self.record.runtime_identity == serde_json::to_string(&runtime.identity()?)?
                && lillux::sha256_hex(&self.signed) == self.reference.sha256,
            "staged operation differs from its original runtime or bytes"
        );
        let name = format!("{PREFIX}{}", self.reference.operation_id);
        if runtime
            .atomic_create_regular(
                OsStr::new(&name),
                &self.signed,
                0o600,
                crate::execution_history_reset::RESET_TEMP_PURPOSE,
            )?
            .is_none()
        {
            let existing = lillux_sqlite::read_regular_without_sqlite_alias(
                runtime,
                OsStr::new(&name),
                MAX_BYTES,
            )?
            .context("staged original reset operation disappeared")?;
            ensure!(
                existing == self.signed,
                "staged reset operation conflicts with retained bytes"
            );
        }
        runtime
            .sync()
            .context("sync staged original reset operation")?;
        let pin = runtime
            .open_mount_entry(OsStr::new(&name))?
            .context("staged original reset operation disappeared")?;
        let current = lillux_sqlite::read_regular_without_sqlite_alias(runtime, OsStr::new(&name), MAX_BYTES)?
            .context("staged original reset operation disappeared")?;
        ensure!(current == self.signed, "staged reset operation changed after staging");
        require_pinned_entry(runtime, &name, &pin)?;
        match self.staged.get() {
            Some(retained) => require_same_file(retained, &pin),
            None => {
                let _ = self.staged.set(pin);
                Ok(())
            }
        }
    }

    /// Hold this exact candidate through namespace/reservation refusal. The
    /// callback validates domain admission; it does not activate State.
    pub(super) fn validate_pre_activation(
        self: &Arc<Self>,
        state: &PinnedStateAuthority,
        guard: &CasMutationGuard,
        validate: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            state.ensure_guard(guard)?;
            self.require_app_root()?;
            ensure!(
                guard.is_exclusive()
                    && self.runtime.is_same_directory(state.runtime_directory())?,
                "reset pre-activation admission requires its actual State runtime"
            );
            self.require_staged(state.runtime_directory())?;
            if let Some(active) = state
                .require_recovery()?
                .thread_history_discard_operation()?
            {
                ensure!(
                    active == self.reference,
                    "pre-activation marker selects another original operation"
                );
            }
            validate()
        })();
        result.map_err(|error| {
            anyhow::Error::new(OperationPublicationFailure {
                error,
                operation: Arc::clone(self),
                phase: PublicationPhase::PreActivationAdmission,
            })
        })
    }

    /// Activation failure can follow a successful marker install and failed
    /// sync. Keep the exact operation, pins and lease rather than returning an
    /// error which discards its owner or prepares replacement signed bytes.
    pub(super) fn activate(
        self: &Arc<Self>,
        state_db: &ryeos_state::StateDb,
        state: &PinnedStateAuthority,
        guard: &CasMutationGuard,
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            state.ensure_guard(guard)?;
            self.require_app_root()?;
            ensure!(
                guard.is_exclusive()
                    && self.runtime.is_same_directory(state.runtime_directory())?,
                "original operation activation requires its actual exclusive State runtime"
            );
            self.require_staged(state.runtime_directory())?;
            state_db.begin_thread_history_discard_admitted(guard, &self.reference)?;
            self.require_active(state, guard)
        })();
        result.map_err(|error| {
            anyhow::Error::new(OperationPublicationFailure {
                error,
                operation: Arc::clone(self),
                phase: PublicationPhase::StateActivation,
            })
        })
    }

    pub(super) fn require_staged(&self, runtime: &lillux::PinnedDirectory) -> Result<()> {
        self.require_staged_in(None, runtime)
    }

    /// With the reset's native factory the guarded read runs in that
    /// factory's scope, so projection leases the factory holds answer for
    /// their owner; without one it refuses whenever any lease is listed.
    fn require_staged_in(
        &self,
        factory: Option<&lillux_sqlite::OriginalNativeFactory>,
        runtime: &lillux::PinnedDirectory,
    ) -> Result<()> {
        self.require_app_root()?;
        ensure!(
            self.runtime.is_same_directory(runtime)?,
            "original operation staged read requires its retained runtime"
        );
        let name = format!("{PREFIX}{}", self.reference.operation_id);
        let current = match factory {
            Some(factory) => factory.read_regular_without_sqlite_alias(runtime, OsStr::new(&name), MAX_BYTES),
            None => lillux_sqlite::read_regular_without_sqlite_alias(runtime, OsStr::new(&name), MAX_BYTES),
        }?
        .context("original staged operation is missing")?;
        ensure!(
            current == self.signed,
            "original staged operation bytes changed"
        );
        Ok(())
    }

    /// Remove the staged operation file once State has acknowledged this
    /// operation, so a completed reset leaves no inert operation behind. The
    /// file is removed only while its bytes are this operation's own and no
    /// Active marker names it, through a planned quarantine whose name is
    /// known before the effect. An absent file was removed by an earlier
    /// attempt.
    pub(super) fn remove_staged_after_acknowledgement(
        &self,
        state: &PinnedStateAuthority,
        guard: &CasMutationGuard,
    ) -> Result<()> {
        state.ensure_guard(guard)?;
        self.require_app_root()?;
        let runtime = state.runtime_directory();
        ensure!(
            guard.is_exclusive() && self.runtime.is_same_directory(runtime)?,
            "staged operation removal requires its actual exclusive State runtime"
        );
        let recovery = state.require_recovery()?;
        ensure!(
            recovery.thread_history_discard_operation()?.is_none()
                && recovery.last_acknowledged_thread_history_discard()?.as_ref() == Some(&self.reference),
            "staged operation removal requires State to have acknowledged this exact operation"
        );
        let name = format!("{PREFIX}{}", self.reference.operation_id);
        let quarantine = format!("cleanup.{name}");
        let staged = self
            .staged
            .get()
            .context("staged operation removal lacks the descriptor retained at staging")?;
        if runtime.entry_no_follow(OsStr::new(&name))?.is_none() {
            // An earlier attempt may have stopped after its quarantine
            // rename; only the retained inode is unlinked there.
            if runtime.entry_no_follow(OsStr::new(&quarantine))?.is_some() {
                runtime
                    .unlink_regular_if_same(OsStr::new(&quarantine), staged)
                    .context("unlink staged operation quarantine")?;
            }
            return Ok(());
        }
        require_pinned_entry(runtime, &name, staged)?;
        let current = lillux_sqlite::read_regular_without_sqlite_alias(runtime, OsStr::new(&name), MAX_BYTES)?
            .context("staged operation disappeared before removal")?;
        ensure!(current == self.signed, "staged operation bytes changed before removal");
        runtime
            .remove_metadata_regular_if_same_planned_atomic(OsStr::new(&name), staged, OsStr::new(&quarantine))
            .map_err(anyhow::Error::new)
            .context("remove staged operation file")?;
        Ok(())
    }

    pub(super) fn require_active(
        &self,
        state: &PinnedStateAuthority,
        guard: &CasMutationGuard,
    ) -> Result<()> {
        self.require_active_in(None, state, guard)
    }

    /// [`Self::require_active`] with its staged read in `factory`'s scope.
    pub(super) fn require_active_in(
        &self,
        factory: Option<&lillux_sqlite::OriginalNativeFactory>,
        state: &PinnedStateAuthority,
        guard: &CasMutationGuard,
    ) -> Result<()> {
        state.ensure_guard(guard)?;
        self.require_app_root()?;
        ensure!(
            guard.is_exclusive()
                && self.record.runtime_identity
                    == serde_json::to_string(&state.runtime_directory().identity()?)?,
            "active original operation requires its actual exclusive State runtime"
        );
        // With the owning factory the marker is read in its scope, so the
        // projection leases that factory holds do not refuse the read.
        let recovery = state.require_recovery()?;
        let active = match factory {
            Some(factory) => recovery.thread_history_discard_operation_scoped(factory)?,
            None => recovery.thread_history_discard_operation()?,
        };
        ensure!(
            active.as_ref() == Some(&self.reference),
            "original reset operation is not activated by its exact State marker"
        );
        self.require_staged_in(factory, state.runtime_directory())?;
        Ok(())
    }
}
