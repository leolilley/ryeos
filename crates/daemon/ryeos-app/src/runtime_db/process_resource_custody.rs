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
    /// An existing workspace owner coordinate, never a removal pathname.
    pub workspace_id: String,
    /// Captured from the ORIGINAL directory authority before possible spawn.
    /// Retaining/deserializing this value does not recreate that authority.
    pub workspace_identity: lillux::secure_fs::PinnedDirectoryIdentity,
}

impl ProcessResourceRuntimeCustody {
    pub const VERSION: u32 = 1;

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
        if self.workspace_id.is_empty()
            || self.workspace_id.len() > 256
            || self.workspace_id == "."
            || self.workspace_id == ".."
            || !self
                .workspace_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            bail!("process runtime custody workspace is not a bounded owner coordinate");
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
            custody.validate()?;
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
            workspace_id: "workspace-custody".to_owned(),
            workspace_identity: lillux::PinnedDirectory::open(tmp.path())
                .unwrap()
                .unwrap()
                .identity()
                .unwrap(),
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
            changed.workspace_id = name.to_owned();
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
