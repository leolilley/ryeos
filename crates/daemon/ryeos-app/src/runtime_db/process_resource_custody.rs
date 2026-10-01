//! Retention coordinates under the existing resource process owner.
//!
//! These records pin bytes, not permission or cleanup evidence. A trusted
//! executable still requires the private admission, financial and driver joins.
//! Resource-free pooled processes have no row here; this module makes no claim
//! to solve their durable custody or to retain opaque live descriptors by itself.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// A finite first process closure. This is a custody bound, not a cache budget.
pub const MAX_PROCESS_CUSTODY_MATERIALIZATIONS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessMaterializationCache {
    ExternalContent,
    SourceClosure,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessMaterializationCustody {
    pub cache: ProcessMaterializationCache,
    pub manifest_hash: String,
}

/// Physical disposition is separate from process/resource settlement. Every
/// phase remains a CAS/cache root until a future authorized final clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessCustodyPhysicalState {
    Retained,
    CleanupIntent,
    PhysicallyRemoved,
}

/// A workspace name is never physical cleanup authority. Resource-owned
/// scratch belongs to the SAME exact resource reservation/owner journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "owner", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessWorkspaceCustody {
    ExistingExecutionWorkspace {
        workspace_id: String,
        workspace_identity: lillux::secure_fs::PinnedDirectoryIdentity,
    },
    ResourceOwnedScratch {
        runtime_directory_identity: lillux::secure_fs::PinnedDirectoryIdentity,
        parent_directory_identity: lillux::secure_fs::PinnedDirectoryIdentity,
        scratch_name: String,
        workspace_identity: lillux::secure_fs::PinnedDirectoryIdentity,
    },
}

impl ProcessWorkspaceCustody {
    pub(crate) fn is_resource_owned_scratch(&self) -> bool {
        matches!(self, Self::ResourceOwnedScratch { .. })
    }

    fn leaf(&self) -> &str {
        match self {
            Self::ExistingExecutionWorkspace { workspace_id, .. } => workspace_id,
            Self::ResourceOwnedScratch { scratch_name, .. } => scratch_name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessResourceRuntimeCustody {
    pub version: u32,
    /// Persistent-session capsule for a pooled or dedicated session owner.
    /// This first record is never populated for thread launch capsules.
    pub session_capsule_hash: String,
    pub execution_realization_hash: String,
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub source_binding_hash: Option<String>,
    pub materializations: Vec<ProcessMaterializationCustody>,
    pub workspace: ProcessWorkspaceCustody,
    pub physical_state: ProcessCustodyPhysicalState,
}

impl ProcessResourceRuntimeCustody {
    pub const VERSION: u32 = 2;

    pub fn validate(&self) -> Result<()> {
        if self.version != Self::VERSION {
            bail!("process runtime custody version is not current");
        }
        for hash in [&self.session_capsule_hash, &self.execution_realization_hash]
            .into_iter()
            .chain(self.source_binding_hash.iter())
            .chain(
                self.materializations
                    .iter()
                    .map(|entry| &entry.manifest_hash),
            )
        {
            if hash.len() != 64
                || !hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                bail!("process runtime custody has a noncanonical object hash");
            }
        }
        if self.materializations.len() > MAX_PROCESS_CUSTODY_MATERIALIZATIONS
            || self
                .materializations
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            bail!("process runtime custody materializations are not bounded, sorted and unique");
        }
        let leaf = self.workspace.leaf();
        if leaf.is_empty()
            || leaf.len() > 256
            || leaf == "."
            || leaf == ".."
            || !leaf
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            bail!("process runtime custody workspace is not a bounded owner coordinate");
        }
        Ok(())
    }

    pub(crate) fn require_retained(&self) -> Result<()> {
        self.validate()?;
        if self.physical_state != ProcessCustodyPhysicalState::Retained {
            bail!("resource custody physical cleanup fences spawn/attachment");
        }
        Ok(())
    }

    pub(crate) fn validate_owner_kind(&self, owner_kind: &str) -> Result<()> {
        self.validate()?;
        if self.workspace.is_resource_owned_scratch() && owner_kind != "pooled_session" {
            bail!("resource-owned scratch requires its existing pooled resource owner");
        }
        Ok(())
    }

    /// Root collection preserves every named object. Actual closure validation
    /// and small/large traversal belong to the existing verified CAS machinery.
    pub fn cas_roots(&self) -> Result<BTreeSet<String>> {
        self.validate()?;
        Ok(
            [&self.session_capsule_hash, &self.execution_realization_hash]
                .into_iter()
                .chain(self.source_binding_hash.iter())
                .chain(
                    self.materializations
                        .iter()
                        .map(|entry| &entry.manifest_hash),
                )
                .cloned()
                .collect(),
        )
    }
}

pub(super) fn decode_owner_custody(
    encoded: Option<&str>,
    owner_kind: &str,
    identity: &crate::process::ExecutionProcessIdentity,
) -> Result<Option<ProcessResourceRuntimeCustody>> {
    if encoded.is_some() && !matches!(owner_kind, "pooled_session" | "dedicated_worker") {
        bail!("session runtime custody is not valid for this process owner kind");
    }
    let custody = encoded
        .map(|encoded| {
            let custody: ProcessResourceRuntimeCustody = serde_json::from_str(encoded)?;
            custody.validate_owner_kind(owner_kind)?;
            if lillux::canonical_json(&serde_json::to_value(&custody)?)? != encoded {
                bail!("resource owner runtime custody is not canonical");
            }
            Ok::<_, anyhow::Error>(custody)
        })
        .transpose()?;
    if matches!(
        identity.resource_settlement_authority,
        Some(crate::process::ProcessResourceSettlementAuthority::TrustedProcessGroup { .. })
    ) && custody.is_none()
    {
        bail!("trusted resource owner is missing retained runtime custody");
    }
    Ok(custody)
}

impl super::RuntimeDb {
    fn retained_process_resource_custodies(&self) -> Result<Vec<ProcessResourceRuntimeCustody>> {
        let mut retained = Vec::new();
        for reservation in self.process_resource_reservations()? {
            retained.extend(reservation.runtime_custody);
        }
        for owner in self.process_resource_owners()? {
            retained.extend(owner.runtime_custody);
        }
        Ok(retained)
    }

    /// All states remain roots, including proved process cleanup with pending
    /// workspace cleanup. Resource release alone cannot retire content custody.
    pub fn process_resource_cas_roots(&self) -> Result<Vec<String>> {
        let mut roots = BTreeSet::new();
        for custody in self.retained_process_resource_custodies()? {
            roots.extend(custody.cas_roots()?);
        }
        Ok(roots.into_iter().collect())
    }

    /// A snapshot alone is not permission to evict. A deletion owner must hold
    /// the protected StateStore pin critical section through actual deletion.
    pub fn process_resource_materialization_pins(
        &self,
    ) -> Result<BTreeSet<ProcessMaterializationCustody>> {
        let mut pins = BTreeSet::new();
        for custody in self.retained_process_resource_custodies()? {
            pins.extend(custody.materializations);
        }
        Ok(pins)
    }
}

/// Physical cleanup is fenced by the SAME authoritative reservation row.
/// These methods authorize no issued-process, driver or financial settlement.
impl super::RuntimeDb {
    pub(crate) fn claim_never_issued_scratch_cleanup(
        &self,
        expected: &super::ProcessResourceReservationRecord,
    ) -> Result<super::ProcessResourceReservationRecord> {
        let custody = never_issued_scratch_custody(expected)?;
        if !matches!(
            custody.physical_state,
            ProcessCustodyPhysicalState::Retained | ProcessCustodyPhysicalState::CleanupIntent
        ) {
            bail!("scratch physical cleanup is already recorded; reconcile absence instead");
        }
        let mut next = expected.clone();
        next.runtime_custody
            .as_mut()
            .expect("validated custody")
            .physical_state = ProcessCustodyPhysicalState::CleanupIntent;
        self.compare_never_issued_scratch_transition(expected, &next)?;
        Ok(next)
    }

    /// Only StateStore's exact physical owner can mint this non-serde receipt.
    pub(crate) fn record_never_issued_scratch_removal(
        &self,
        receipt: crate::state_store::ResourceScratchPhysicalReceipt,
    ) -> Result<super::ProcessResourceReservationRecord> {
        let expected = receipt.into_expected();
        let custody = never_issued_scratch_custody(&expected)?;
        if custody.physical_state != ProcessCustodyPhysicalState::CleanupIntent {
            bail!("physical receipt has no retained scratch cleanup intent");
        }
        let mut removed = expected.clone();
        removed
            .runtime_custody
            .as_mut()
            .expect("validated custody")
            .physical_state = ProcessCustodyPhysicalState::PhysicallyRemoved;
        self.compare_never_issued_scratch_transition(&expected, &removed)?;
        // Intentionally retain this row and all roots: no financial/driver or
        // final-clear capability is introduced by a physical receipt.
        Ok(removed)
    }

    fn compare_never_issued_scratch_transition(
        &self,
        expected: &super::ProcessResourceReservationRecord,
        next: &super::ProcessResourceReservationRecord,
    ) -> Result<()> {
        never_issued_scratch_custody(expected)?;
        never_issued_scratch_custody(next)?;
        let birth = super::process_resource_reservation_id(expected)?;
        if birth != super::process_resource_reservation_id(next)? {
            bail!("scratch cleanup changed its immutable reservation birth authority");
        }
        let before = lillux::canonical_json(&serde_json::to_value(expected)?)?;
        let after = lillux::canonical_json(&serde_json::to_value(next)?)?;
        let tx = self.conn.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE process_resource_reservation SET reservation=?5, updated_at_ms=?6
              WHERE reservation_id=?1 AND owner_kind=?2 AND owner_coordinate=?3 AND reservation=?4
                AND NOT EXISTS (SELECT 1 FROM process_resource_owner o
                  WHERE o.owner_kind=?2 AND o.owner_coordinate=?3)",
            rusqlite::params![
                birth,
                expected.owner_kind,
                expected.owner_coordinate,
                before,
                after,
                i64::try_from(lillux::time::timestamp_millis())?
            ],
        )?;
        if changed != 1 {
            bail!("scratch cleanup lost its exact never-issued reservation owner");
        }
        tx.commit().map_err(Into::into)
    }
}

#[cfg(test)]
impl super::RuntimeDb {
    /// Test-only staging of a previous daemon's ORIGINAL birth row, before
    /// recovery. No production generation rewrite or wire constructor exists.
    pub(crate) fn fixture_previous_scratch_daemon_birth(
        &self,
        current: &super::ProcessResourceReservationRecord,
        old_generation: &str,
    ) -> Result<super::ProcessResourceReservationRecord> {
        never_issued_scratch_custody(current)?;
        let mut old = current.clone();
        old.daemon_generation_id = old_generation.to_owned();
        let changed = self.conn.execute(
            "UPDATE process_resource_reservation SET reservation_id=?1, reservation=?2
             WHERE reservation_id=?3 AND owner_kind=?4 AND owner_coordinate=?5 AND reservation=?6",
            rusqlite::params![
                super::process_resource_reservation_id(&old)?,
                lillux::canonical_json(&serde_json::to_value(&old)?)?,
                super::process_resource_reservation_id(current)?,
                current.owner_kind,
                current.owner_coordinate,
                lillux::canonical_json(&serde_json::to_value(current)?)?
            ],
        )?;
        if changed != 1 {
            bail!("old-daemon fixture lost its exact original reservation");
        }
        Ok(old)
    }
}

pub(crate) fn never_issued_scratch_custody(
    reservation: &super::ProcessResourceReservationRecord,
) -> Result<&ProcessResourceRuntimeCustody> {
    reservation.validate()?;
    if reservation.owner_kind != "pooled_session"
        || !matches!(
            reservation.launch_authority,
            super::ProcessResourceLaunchAuthority::TrustedProcessGroup {
                phase: super::TrustedResourceLaunchPhase::Reserved,
                ..
            }
        )
    {
        bail!("scratch removal requires an authoritative never-issued pooled reservation");
    }
    let custody = reservation
        .runtime_custody
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("scratch removal lost retained custody"))?;
    if !custody.workspace.is_resource_owned_scratch() {
        bail!("scratch removal cannot borrow an execution-workspace journal coordinate");
    }
    Ok(custody)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn custody() -> ProcessResourceRuntimeCustody {
        let tmp = tempfile::tempdir().unwrap();
        ProcessResourceRuntimeCustody {
            version: ProcessResourceRuntimeCustody::VERSION,
            session_capsule_hash: "1".repeat(64),
            execution_realization_hash: "2".repeat(64),
            source_binding_hash: Some("3".repeat(64)),
            materializations: vec![
                ProcessMaterializationCustody {
                    cache: ProcessMaterializationCache::ExternalContent,
                    manifest_hash: "4".repeat(64),
                },
                ProcessMaterializationCustody {
                    cache: ProcessMaterializationCache::SourceClosure,
                    manifest_hash: "5".repeat(64),
                },
            ],
            physical_state: ProcessCustodyPhysicalState::Retained,
            workspace: ProcessWorkspaceCustody::ExistingExecutionWorkspace {
                workspace_id: "workspace-custody".to_owned(),
                workspace_identity: lillux::PinnedDirectory::open(tmp.path())
                    .unwrap()
                    .unwrap()
                    .identity()
                    .unwrap(),
            },
        }
    }

    #[test]
    fn custody_rejects_omitted_nullable_hash_and_drifted_coordinates() {
        let original = custody();
        let mut value = serde_json::to_value(&original).unwrap();
        value.as_object_mut().unwrap().remove("source_binding_hash");
        assert!(serde_json::from_value::<ProcessResourceRuntimeCustody>(value).is_err());
        let mut changed = original.clone();
        changed.session_capsule_hash = "A".repeat(64);
        assert!(changed.validate().is_err());
        for name in ["..", "root/child", "/tmp/root", "root\n"] {
            changed = original.clone();
            let ProcessWorkspaceCustody::ExistingExecutionWorkspace { workspace_id, .. } =
                &mut changed.workspace
            else {
                unreachable!()
            };
            *workspace_id = name.to_owned();
            assert!(changed.validate().is_err());
        }
    }

    #[test]
    fn custody_roots_cover_the_exact_bounded_inventory() {
        let original = custody();
        assert_eq!(
            original.cas_roots().unwrap(),
            (1..=5).map(|n| n.to_string().repeat(64)).collect()
        );
        let mut changed = original.clone();
        changed.materializations.reverse();
        assert!(changed.validate().is_err());
        changed = original.clone();
        changed
            .materializations
            .insert(0, changed.materializations[0].clone());
        assert!(changed.validate().is_err());
        changed = original;
        changed.materializations =
            vec![changed.materializations[0].clone(); MAX_PROCESS_CUSTODY_MATERIALIZATIONS + 1];
        assert!(changed.validate().is_err());
    }
}
