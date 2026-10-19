//! Exact pre-copy source selection. Metadata is evidence, not native admission.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SourceFact {
    device: u64,
    inode: u64,
    byte_length: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InspectionSourcePlan {
    pub(super) purpose: String,
    pub(super) directory_identity: String,
    pub(super) database_name: String,
    pub(super) roles: [Option<SourceFact>; 4],
    /// Set only when this plan is captured for the resumption of an operation
    /// whose signed record shows the main was present when it began. A main
    /// that is now absent with its sidecars left behind is then the earlier
    /// run's own interrupted retirement, which the participant's journals
    /// settle; without it that shape is unknown state and is refused.
    pub(super) orphan_sidecars_recoverable: bool,
}

pub(super) fn role_names(database_name: &str) -> Result<[OsString; 4]> {
    ensure!(
        !database_name.is_empty()
            && database_name != "."
            && database_name != ".."
            && !database_name.contains('/')
            && !database_name.contains('\0')
            && database_name.len() <= 247,
        "invalid fixed inspection database name"
    );
    Ok([
        database_name.to_owned(),
        format!("{database_name}-wal"),
        format!("{database_name}-shm"),
        format!("{database_name}-journal"),
    ]
    .map(OsString::from))
}

impl InspectionSourcePlan {
    pub(super) fn capture(
        source: &lillux::PinnedDirectory,
        name: &OsStr,
        purpose: &str,
        orphan_sidecars_recoverable: bool,
    ) -> Result<(Self, Vec<File>)> {
        source.ensure_path_binding()?;
        let database_name = name
            .to_str()
            .context("inspection database name is not UTF-8")?
            .to_owned();
        let names = role_names(&database_name)?;
        let mut pins = Vec::with_capacity(4);
        let mut roles: [Option<SourceFact>; 4] = std::array::from_fn(|_| None);
        for (index, name) in names.iter().enumerate() {
            if let Some(pin) = source.open_mount_entry(name)? {
                ensure!(
                    pin.metadata()?.is_file(),
                    "inspection source role is not regular"
                );
                let fact = lillux::observe_open_file_identity(&pin)?;
                ensure!(
                    fact.size() <= i64::MAX as u64,
                    "inspection source exceeds native extent range"
                );
                roles[index] = Some(SourceFact {
                    device: fact.device(),
                    inode: fact.inode(),
                    byte_length: fact.size(),
                });
                pins.push(pin);
            }
        }
        let plan = Self {
            purpose: purpose.to_owned(),
            directory_identity: serde_json::to_string(&source.identity()?)?,
            database_name,
            roles,
            orphan_sidecars_recoverable,
        };
        plan.validate()?;
        source.ensure_path_binding()?;
        Ok((plan, pins))
    }

    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.purpose.as_str(), "runtime" | "scheduler"),
            "unknown inspection source purpose"
        );
        role_names(&self.database_name)?;
        ensure!(
            !self.directory_identity.is_empty(),
            "inspection source lacks directory identity"
        );
        if self.roles[0].is_none() {
            ensure!(
                self.orphan_sidecars_recoverable || self.roles.iter().all(Option::is_none),
                "absent inspection main has retained sidecars requiring recovery"
            );
        }
        for fact in self.roles.iter().flatten() {
            ensure!(
                fact.byte_length <= i64::MAX as u64,
                "inspection source extent exceeds native range"
            );
        }
        Ok(())
    }

    /// Match native observed metadata to one fixed original role. This does
    /// not treat a native observation as a signed digest or mutation authority.
    pub(super) fn require_role(
        &self,
        source: &lillux::PinnedDirectory,
        name: &OsStr,
        observed: Option<&lillux_sqlite::SqliteRoleIdentity>,
    ) -> Result<usize> {
        self.require_current(source)?;
        let role = role_names(&self.database_name)?
            .iter()
            .position(|candidate| candidate == name)
            .context("copy selects a role outside the original fixed plan")?;
        ensure!(
            match (&self.roles[role], observed) {
                (None, None) => true,
                (Some(expected), Some(observed)) =>
                    expected.device == observed.device
                        && expected.inode == observed.inode
                        && expected.byte_length == observed.byte_length
                        && observed.content_sha256.is_none(),
                _ => false,
            },
            "native copy source differs from original signed role metadata"
        );
        Ok(role)
    }

    pub(super) fn total_bytes(&self) -> Result<u64> {
        self.roles.iter().flatten().try_fold(0u64, |total, fact| {
            total
                .checked_add(fact.byte_length)
                .context("inspection source extent overflow")
        })
    }

    pub(super) fn require_current(&self, source: &lillux::PinnedDirectory) -> Result<()> {
        self.validate()?;
        source.ensure_path_binding()?;
        ensure!(
            serde_json::to_string(&source.identity()?)? == self.directory_identity,
            "inspection request selects another source directory"
        );
        for (name, expected) in role_names(&self.database_name)?.iter().zip(&self.roles) {
            let current = source.open_mount_entry(name)?;
            match (current, expected) {
                (None, None) => {}
                (Some(pin), Some(expected)) => {
                    ensure!(
                        pin.metadata()?.is_file(),
                        "inspection source role is no longer regular"
                    );
                    let fact = lillux::observe_open_file_identity(&pin)?;
                    ensure!(
                        fact.device() == expected.device
                            && fact.inode() == expected.inode
                            && fact.size() == expected.byte_length,
                        "inspection source differs from original plan"
                    );
                }
                _ => anyhow::bail!("inspection source presence differs from original plan"),
            }
        }
        source.ensure_path_binding()
    }
}

/// Stable Operational provenance is not a disposable inspection destination.
/// All coordinates are protocol-owned; metadata alone issues no native grant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OperationalSourcePlan {
    directory_identity: String,
    roles: [Option<SourceFact>; 4],
    marker: SourceFact,
}

/// The actual original metadata pins, separately retained from signed facts.
/// No drop/metadata comparison constitutes disposal or funding settlement.
pub(super) struct OperationalSourcePins {
    roles: [Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 4],
    marker: Arc<lillux_sqlite::RetainedOriginalMetadataPin>,
}
impl OperationalSourcePins {
    pub(super) fn main(&self) -> Result<&Arc<lillux_sqlite::RetainedOriginalMetadataPin>> {
        self.roles[0]
            .as_ref()
            .context("original Operational main pin absent")
    }
    pub(super) fn marker(&self) -> &Arc<lillux_sqlite::RetainedOriginalMetadataPin> {
        &self.marker
    }
    /// Exact original present-role inventory, followed by the marker. These
    /// Arc clones retain the same physical owners; no descriptor is duplicated.
    pub(super) fn retirement_pins(
        &self,
    ) -> Vec<(u8, Arc<lillux_sqlite::RetainedOriginalMetadataPin>)> {
        self.roles
            .iter()
            .enumerate()
            .filter_map(|(index, pin)| pin.as_ref().map(|pin| (index as u8, pin.clone())))
            .chain(std::iter::once((4, self.marker.clone())))
            .collect()
    }
    pub(super) fn retained_count(&self) -> usize {
        // Inventory size only, never evidence of physical retirement.
        self.roles.iter().flatten().count() + 1
    }
}
impl SourceFact {
    fn capture_pin(pin: &File) -> Result<Self> {
        ensure!(pin.metadata()?.is_file(), "source role is not regular");
        let fact = lillux::observe_open_file_identity(pin)?;
        ensure!(
            fact.size() <= i64::MAX as u64,
            "source extent exceeds native range"
        );
        Ok(Self {
            device: fact.device(),
            inode: fact.inode(),
            byte_length: fact.size(),
        })
    }
    fn require_retained_pin(
        &self,
        pin: &lillux_sqlite::RetainedOriginalMetadataPin,
        require_extent: bool,
    ) -> Result<()> {
        let current = pin.observed_identity()?;
        ensure!(
            current.device() == self.device
                && current.inode() == self.inode
                && (!require_extent || current.size() == self.byte_length),
            "actual retained source pin differs from signed original identity"
        );
        Ok(())
    }
    fn require_pin(&self, pin: &File, require_extent: bool) -> Result<()> {
        let current = Self::capture_pin(pin)?;
        ensure!(
            current.device == self.device
                && current.inode == self.inode
                && (!require_extent || current.byte_length == self.byte_length),
            "actual source pin differs from signed original identity"
        );
        Ok(())
    }
}
impl OperationalSourcePlan {
    pub(super) fn capture(
        runtime: &lillux::PinnedDirectory,
        namespace: &lillux::PinnedDirectoryLock,
        copies: &[InspectionSourcePlan; 2],
    ) -> Result<(Self, OperationalSourcePins)> {
        runtime.ensure_path_binding()?;
        namespace.ensure_protects(runtime)?;
        let names = role_names(ryeos_state::operational::OPERATIONAL_DB_FILENAME)?;
        let mut roles: [Option<SourceFact>; 4] = std::array::from_fn(|_| None);
        let mut pins: [Option<Arc<lillux_sqlite::RetainedOriginalMetadataPin>>; 4] =
            std::array::from_fn(|_| None);
        for (index, name) in names.iter().enumerate() {
            if let Some(pin) = runtime.open_mount_entry(name)? {
                roles[index] = Some(SourceFact::capture_pin(&pin)?);
                // SAFETY: this is the actual regular O_PATH pin captured
                // above from the locked namespace, not a descriptor clone.
                pins[index] =
                    Some(unsafe { lillux_sqlite::RetainedOriginalMetadataPin::issued(pin) });
            }
        }
        let marker_pin = runtime
            .open_mount_entry(OsStr::new(
                ryeos_state::operational::OPERATIONAL_INITIALIZED_FILENAME,
            ))?
            .context("established Operational initialization marker absent")?;
        let marker = SourceFact::capture_pin(&marker_pin)?;
        let plan = Self {
            directory_identity: serde_json::to_string(&runtime.identity()?)?,
            roles,
            marker,
        };
        plan.validate_with_copies(copies)?;
        // SAFETY: actual regular metadata-only marker pin already validated
        // against the canonical original fact, under this same namespace.
        let pins = OperationalSourcePins {
            roles: pins,
            marker: unsafe { lillux_sqlite::RetainedOriginalMetadataPin::issued(marker_pin) },
        };
        plan.require_current(runtime, &pins)?;
        namespace.ensure_protects(runtime)?;
        Ok((plan, pins))
    }
    pub(super) fn validate(&self) -> Result<()> {
        ensure!(
            !self.directory_identity.is_empty(),
            "Operational source directory absent"
        );
        ensure!(
            self.roles[0].is_some(),
            "established Operational main is absent"
        );
        for fact in self
            .roles
            .iter()
            .flatten()
            .chain(std::iter::once(&self.marker))
        {
            ensure!(
                fact.byte_length <= i64::MAX as u64,
                "Operational source extent exceeds native range"
            );
        }
        ensure!(
            self.marker.byte_length
                == ryeos_state::operational::OPERATIONAL_INITIALIZED_CONTENT.len() as u64,
            "Operational initialization marker has invalid extent"
        );
        Ok(())
    }
    pub(super) fn validate_with_copies(&self, copies: &[InspectionSourcePlan; 2]) -> Result<()> {
        self.validate()?;
        validate_sources(copies)?;
        ensure!(
            self.directory_identity == copies[1].directory_identity,
            "Operational provenance selects another runtime namespace"
        );
        let mut coordinates = std::collections::BTreeSet::new();
        let mut physical = std::collections::BTreeSet::new();
        for copy in copies {
            for (name, fact) in role_names(&copy.database_name)?
                .into_iter()
                .zip(&copy.roles)
            {
                ensure!(
                    coordinates.insert((copy.directory_identity.as_str(), name)),
                    "duplicate source coordinate"
                );
                if let Some(fact) = fact {
                    ensure!(
                        physical.insert((fact.device, fact.inode)),
                        "aliased source inode"
                    );
                }
            }
        }
        let names = role_names(ryeos_state::operational::OPERATIONAL_DB_FILENAME)?;
        for (name, fact) in names
            .into_iter()
            .zip(&self.roles)
            .map(|(name, fact)| (name, fact.as_ref()))
            .chain(std::iter::once((
                OsString::from(ryeos_state::operational::OPERATIONAL_INITIALIZED_FILENAME),
                Some(&self.marker),
            )))
        {
            ensure!(
                coordinates.insert((self.directory_identity.as_str(), name)),
                "Operational source coordinate aliases another source"
            );
            if let Some(fact) = fact {
                ensure!(
                    physical.insert((fact.device, fact.inode)),
                    "Operational source inode aliases another source"
                );
            }
        }
        Ok(())
    }
    fn require_directory(&self, runtime: &lillux::PinnedDirectory) -> Result<()> {
        self.validate()?;
        runtime.ensure_path_binding()?;
        ensure!(
            serde_json::to_string(&runtime.identity()?)? == self.directory_identity,
            "Operational source runtime identity changed"
        );
        Ok(())
    }
    /// Initial admission only: exact original presence, inode and extents.
    /// Do not repeat these stale size/absence requirements after activation.
    pub(super) fn require_current(
        &self,
        runtime: &lillux::PinnedDirectory,
        pins: &OperationalSourcePins,
    ) -> Result<()> {
        self.require_directory(runtime)?;
        for ((name, expected), pin) in
            role_names(ryeos_state::operational::OPERATIONAL_DB_FILENAME)?
                .iter()
                .zip(&self.roles)
                .zip(&pins.roles)
        {
            let current = runtime.open_mount_entry(name)?;
            match (expected, pin, current) {
                (None, None, None) => {}
                (Some(fact), Some(original), Some(current)) => {
                    fact.require_retained_pin(original, true)?;
                    fact.require_pin(&current, true)?;
                }
                _ => anyhow::bail!("Operational source presence changed before admission"),
            }
        }
        self.require_marker_identity(runtime, pins)?;
        runtime.ensure_path_binding()
    }
    /// Stable main inode custody after legitimate mutation; no old size test.
    pub(super) fn require_main_identity(
        &self,
        runtime: &lillux::PinnedDirectory,
        pins: &OperationalSourcePins,
    ) -> Result<()> {
        self.require_directory(runtime)?;
        let expected = self.roles[0]
            .as_ref()
            .context("Operational main fact absent")?;
        expected.require_retained_pin(pins.main()?, false)?;
        let current = runtime
            .open_mount_entry(OsStr::new(
                ryeos_state::operational::OPERATIONAL_DB_FILENAME,
            ))?
            .context("Operational main coordinate disappeared")?;
        expected.require_pin(&current, false)
    }
    fn require_marker_identity(
        &self,
        runtime: &lillux::PinnedDirectory,
        pins: &OperationalSourcePins,
    ) -> Result<()> {
        self.require_directory(runtime)?;
        self.marker.require_retained_pin(pins.marker(), true)?;
        let current = runtime
            .open_mount_entry(OsStr::new(
                ryeos_state::operational::OPERATIONAL_INITIALIZED_FILENAME,
            ))?
            .context("Operational marker coordinate disappeared")?;
        self.marker.require_pin(&current, true)
    }
    /// Caller must irreversibly debit the real original read/memory dimensions
    /// before the actual alias-excluding reader. No default debit is supplied.
    pub(super) fn require_marker(
        &self,
        runtime: &lillux::PinnedDirectory,
        pins: &OperationalSourcePins,
        debit_original_read: impl FnOnce(u64, u64) -> Result<()>,
    ) -> Result<()> {
        self.require_marker_identity(runtime, pins)?;
        let expected = ryeos_state::operational::OPERATIONAL_INITIALIZED_CONTENT;
        // The guarded reader allocates and may read one extra growth byte.
        let maximum = u64::try_from(expected.len())?
            .checked_add(1)
            .context("Operational marker read bound overflow")?;
        debit_original_read(maximum, maximum)?;
        let bytes = lillux_sqlite::read_regular_without_sqlite_alias(
            runtime,
            OsStr::new(ryeos_state::operational::OPERATIONAL_INITIALIZED_FILENAME),
            expected.len(),
        )?
        .context("Operational initialization marker disappeared during read")?;
        ensure!(
            bytes.as_slice() == expected,
            "Operational initialization marker content invalid"
        );
        self.require_marker_identity(runtime, pins)
    }
}

pub(super) fn capture_sources(
    runtime: &lillux::PinnedDirectory,
    db_target: &Path,
    scheduler_main_was_retired_by_prior: bool,
) -> Result<(
    [InspectionSourcePlan; 2],
    [lillux::PinnedDirectory; 2],
    Vec<File>,
)> {
    let parent = db_target
        .parent()
        .context("runtime inspection target has no parent")?;
    let runtime_source = lillux::PinnedDirectory::open(parent)?
        .context("runtime inspection source parent is absent")?;
    let name = db_target
        .file_name()
        .context("runtime inspection target has no basename")?;
    let (runtime_plan, mut pins) =
        InspectionSourcePlan::capture(&runtime_source, name, "runtime", false)?;
    let (scheduler_plan, scheduler_pins) = InspectionSourcePlan::capture(
        runtime,
        OsStr::new("scheduler.sqlite3"),
        "scheduler",
        scheduler_main_was_retired_by_prior,
    )?;
    pins.extend(scheduler_pins);
    let plans = [runtime_plan, scheduler_plan];
    validate_sources(&plans)?;
    Ok((plans, [runtime_source, runtime.try_clone()?], pins))
}

pub(super) fn validate_sources(plans: &[InspectionSourcePlan; 2]) -> Result<()> {
    ensure!(
        plans[0].purpose == "runtime"
            && plans[1].purpose == "scheduler"
            && plans[1].database_name == "scheduler.sqlite3",
        "inspection source ordering differs"
    );
    let mut coordinates = std::collections::BTreeSet::new();
    let mut physical = std::collections::BTreeSet::new();
    for plan in plans {
        plan.validate()?;
        for (name, fact) in role_names(&plan.database_name)?.iter().zip(&plan.roles) {
            ensure!(
                coordinates.insert((plan.directory_identity.as_str(), name.clone())),
                "inspection source roles select the same coordinate"
            );
            if let Some(fact) = fact {
                ensure!(
                    physical.insert((fact.device, fact.inode)),
                    "inspection source roles alias the same inode"
                );
            }
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod controls {
    use super::*;

    fn operational_fixture() -> (
        tempfile::TempDir,
        lillux::PinnedDirectory,
        lillux::PinnedDirectoryLock,
        [InspectionSourcePlan; 2],
    ) {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("runtime.sqlite3"),
            ryeos_state::operational::OPERATIONAL_INITIALIZED_CONTENT,
        )
        .unwrap();
        std::fs::write(
            temp.path()
                .join(ryeos_state::operational::OPERATIONAL_DB_FILENAME),
            b"initial",
        )
        .unwrap();
        std::fs::write(
            temp.path()
                .join(ryeos_state::operational::OPERATIONAL_INITIALIZED_FILENAME),
            ryeos_state::operational::OPERATIONAL_INITIALIZED_CONTENT,
        )
        .unwrap();
        let directory = lillux::PinnedDirectory::open(temp.path()).unwrap().unwrap();
        let namespace = directory.lock_exclusive().unwrap();
        let (copies, _, _) =
            capture_sources(&directory, &temp.path().join("runtime.sqlite3"), false).unwrap();
        (temp, directory, namespace, copies)
    }

    #[test]
    fn operational_initial_extents_do_not_become_stale_mutation_identity() {
        let (temp, directory, namespace, copies) = operational_fixture();
        let (plan, pins) = OperationalSourcePlan::capture(&directory, &namespace, &copies).unwrap();
        assert_eq!(pins.retained_count(), 2);
        plan.require_current(&directory, &pins).unwrap();
        std::fs::write(
            temp.path()
                .join(ryeos_state::operational::OPERATIONAL_DB_FILENAME),
            b"legitimate same-inode growth after activation",
        )
        .unwrap();
        assert!(plan.require_current(&directory, &pins).is_err());
        plan.require_main_identity(&directory, &pins).unwrap();
        let replacement = temp.path().join("replacement");
        std::fs::write(&replacement, b"different inode").unwrap();
        std::fs::rename(
            replacement,
            temp.path()
                .join(ryeos_state::operational::OPERATIONAL_DB_FILENAME),
        )
        .unwrap();
        assert!(plan.require_main_identity(&directory, &pins).is_err());
    }

    #[test]
    fn operational_provenance_refuses_cross_source_inode_aliases() {
        for marker_alias in [false, true] {
            let (temp, directory, namespace, copies) = operational_fixture();
            let name = if marker_alias {
                ryeos_state::operational::OPERATIONAL_INITIALIZED_FILENAME
            } else {
                ryeos_state::operational::OPERATIONAL_DB_FILENAME
            };
            std::fs::remove_file(temp.path().join(name)).unwrap();
            std::fs::hard_link(temp.path().join("runtime.sqlite3"), temp.path().join(name))
                .unwrap();
            assert!(OperationalSourcePlan::capture(&directory, &namespace, &copies).is_err());
        }
    }

    #[test]
    fn operational_marker_debit_refusal_precedes_actual_read() {
        let (_temp, directory, namespace, copies) = operational_fixture();
        let (plan, pins) = OperationalSourcePlan::capture(&directory, &namespace, &copies).unwrap();
        let called = std::cell::Cell::new(false);
        assert!(
            plan.require_marker(&directory, &pins, |read, memory| {
                let bound =
                    ryeos_state::operational::OPERATIONAL_INITIALIZED_CONTENT.len() as u64 + 1;
                assert_eq!((read, memory), (bound, bound));
                called.set(true);
                anyhow::bail!("injected original debit refusal")
            })
            .is_err()
        );
        assert!(called.get());
    }

    #[test]
    fn pinned_plan_rejects_changed_extent_replacement_and_new_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.sqlite3");
        std::fs::write(&path, b"original").unwrap();
        let root = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        let (plan, pins) =
            InspectionSourcePlan::capture(&root, OsStr::new("runtime.sqlite3"), "runtime", false)
                .unwrap();
        plan.require_current(&root).unwrap();
        std::fs::write(&path, b"changed extent").unwrap();
        assert!(plan.require_current(&root).is_err());
        std::fs::write(&path, b"original").unwrap();
        plan.require_current(&root).unwrap();
        std::fs::write(tmp.path().join("runtime.sqlite3-wal"), b"new").unwrap();
        assert!(plan.require_current(&root).is_err());
        std::fs::remove_file(tmp.path().join("runtime.sqlite3-wal")).unwrap();
        std::fs::rename(&path, tmp.path().join("retained-original")).unwrap();
        std::fs::write(&path, b"original").unwrap();
        assert!(plan.require_current(&root).is_err());
        assert!(!pins.is_empty(), "original O_PATH pins prevent inode reuse");
    }

    #[test]
    fn source_set_rejects_aliases_and_absent_main_with_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.sqlite3");
        std::fs::write(&path, b"original").unwrap();
        std::fs::hard_link(&path, tmp.path().join("scheduler.sqlite3")).unwrap();
        let root = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        assert!(capture_sources(&root, &path, false).is_err());
        std::fs::remove_file(tmp.path().join("scheduler.sqlite3")).unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3-wal"), b"orphan").unwrap();
        assert!(capture_sources(&root, &path, false).is_err());
        std::fs::remove_file(tmp.path().join("scheduler.sqlite3-wal")).unwrap();
        let (plans, roots, pins) = capture_sources(&root, &path, false).unwrap();
        assert_eq!(plans[0].total_bytes().unwrap(), 8);
        assert_eq!(plans[1].total_bytes().unwrap(), 0);
        plans[0].require_current(&roots[0]).unwrap();
        plans[1].require_current(&roots[1]).unwrap();
        assert_eq!(pins.len(), 1);
        assert!(role_names("../foreign").is_err());
        assert!(role_names(&"a".repeat(248)).is_err());
    }

    #[test]
    fn prior_retirement_alone_admits_absent_scheduler_main_with_sidecars() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("runtime.sqlite3");
        std::fs::write(&path, b"original").unwrap();
        std::fs::write(tmp.path().join("scheduler.sqlite3-wal"), b"retired").unwrap();
        let root = lillux::PinnedDirectory::open(tmp.path()).unwrap().unwrap();
        assert!(capture_sources(&root, &path, false).is_err());
        let (plans, roots, _pins) = capture_sources(&root, &path, true).unwrap();
        plans[1].require_current(&roots[1]).unwrap();
    }
}
