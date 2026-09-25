//! Exact non-serializable authority for one external guest activation.

use anyhow::{Result, ensure};
use ryeos_external_execution_contract::ExternalGuestInputProjection;

/// Keeps every descriptor named by the pure projection alive through the one
/// activation mutation.  It deliberately has no `Clone`: reconciliation may
/// observe the retained operation but can never mint replacement guest input
/// authority after an ambiguous activation.
pub struct ExternalGuestInputAuthority {
    projection: ExternalGuestInputProjection,
    base_snapshot: lillux::InheritedDescriptorAuthority,
    workspace_outputs: Option<lillux::InheritedDescriptorAuthority>,
    inputs: Vec<lillux::InheritedDescriptorAuthority>,
    content_records: Vec<lillux::InheritedDescriptorAuthority>,
    lifelines: Vec<Box<dyn Send + Sync>>,
}

impl std::fmt::Debug for ExternalGuestInputAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExternalGuestInputAuthority")
            .field("projection", &self.projection)
            .field("input_count", &self.inputs.len())
            .field("lifeline_count", &self.lifelines.len())
            .finish_non_exhaustive()
    }
}

impl ExternalGuestInputAuthority {
    pub fn new(
        projection: ExternalGuestInputProjection,
        base_snapshot: lillux::InheritedDescriptorAuthority,
        workspace_outputs: Option<lillux::InheritedDescriptorAuthority>,
        inputs: Vec<lillux::InheritedDescriptorAuthority>,
        content_records: Vec<lillux::InheritedDescriptorAuthority>,
        lifelines: Vec<Box<dyn Send + Sync>>,
    ) -> Result<Self> {
        projection.validate()?;
        ensure!(
            inputs.len() == projection.inputs.len(),
            "external guest input authority count changed"
        );
        ensure!(
            content_records.len() == projection.record_descriptors().count(),
            "external guest retained record authority count changed"
        );
        ensure!(
            base_snapshot
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?
                == projection.base_snapshot.descriptor,
            "external guest base snapshot descriptor changed"
        );
        base_snapshot.directory_identity()?;
        match (&workspace_outputs, &projection.workspace_outputs) {
            (None, None) => {}
            (Some(authority), Some(outputs)) => {
                ensure!(
                    authority
                        .inherited_descriptor()
                        .map_err(anyhow::Error::msg)?
                        == outputs.descriptor,
                    "external guest workspace-output descriptor changed"
                );
                authority.regular_file_observation()?;
            }
            _ => anyhow::bail!("external guest workspace-output authority presence changed"),
        }
        for (authority, input) in inputs.iter().zip(&projection.inputs) {
            ensure!(
                authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    == input.descriptor,
                "external guest input descriptor changed"
            );
            match input.kind {
                ryeos_external_execution_contract::GuestMountKind::Directory => {
                    authority.directory_identity()?;
                }
                ryeos_external_execution_contract::GuestMountKind::RegularFile => {
                    authority.regular_file_observation()?;
                }
            }
        }
        for (authority, (descriptor, hash, bytes)) in
            content_records.iter().zip(projection.record_descriptors())
        {
            ensure!(
                authority
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?
                    == descriptor,
                "external guest retained record descriptor changed"
            );
            let observed = authority.regular_file_observation()?;
            ensure!(
                observed.size() == bytes
                    && authority.digest_regular_file_stable_exact(&observed)? == hash,
                "external guest retained record authority changed"
            );
        }
        Ok(Self {
            projection,
            base_snapshot,
            workspace_outputs,
            inputs,
            content_records,
            lifelines,
        })
    }

    pub fn projection(&self) -> &ExternalGuestInputProjection {
        &self.projection
    }

    pub(crate) fn base_snapshot(&self) -> &lillux::InheritedDescriptorAuthority {
        &self.base_snapshot
    }

    pub(crate) fn workspace_outputs(&self) -> Option<&lillux::InheritedDescriptorAuthority> {
        self.workspace_outputs.as_ref()
    }

    pub(crate) fn input(&self, index: usize) -> Option<&lillux::InheritedDescriptorAuthority> {
        self.inputs.get(index)
    }

    pub(crate) fn content_record(
        &self,
        index: usize,
    ) -> Option<&lillux::InheritedDescriptorAuthority> {
        self.content_records.get(index)
    }

    pub fn identity_digest(&self) -> Result<String> {
        self.projection.identity_digest()
    }

    pub fn retained_descriptors(&self) -> Vec<lillux::InheritedDescriptorAuthority> {
        let mut descriptors =
            Vec::with_capacity(self.inputs.len() + self.content_records.len() + 2);
        descriptors.push(self.base_snapshot.clone());
        descriptors.extend(self.workspace_outputs.iter().cloned());
        descriptors.extend(self.inputs.iter().cloned());
        descriptors.extend(self.content_records.iter().cloned());
        descriptors
    }

    pub fn retained_lifeline_count(&self) -> usize {
        self.lifelines.len()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use ryeos_external_execution_contract::{
        EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA, GuestBaseSnapshotInput, GuestMountAccess,
        GuestMountContentAuthority, GuestMountInput, GuestMountKind, GuestMountRole,
    };
    use std::ffi::OsStr;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Fixture {
        _temporary: tempfile::TempDir,
        projection: ExternalGuestInputProjection,
        base: lillux::InheritedDescriptorAuthority,
        source: lillux::InheritedDescriptorAuthority,
        records: Vec<lillux::InheritedDescriptorAuthority>,
    }

    impl Fixture {
        fn new() -> Self {
            // Harness-only fixture construction. The product constructor below
            // performs descriptor observation and hashing through Lillux.
            let temporary = tempfile::tempdir().unwrap();
            std::fs::create_dir(temporary.path().join("source")).unwrap();
            std::fs::write(temporary.path().join("binding"), b"exact binding record").unwrap();
            std::fs::write(
                temporary.path().join("manifest"),
                b"exact source manifest record",
            )
            .unwrap();
            let root = lillux::PinnedDirectory::open(temporary.path())
                .unwrap()
                .unwrap();
            let base = root.inherited_descriptor_authority().unwrap();
            let source = lillux::PinnedDirectory::open(&temporary.path().join("source"))
                .unwrap()
                .unwrap()
                .inherited_descriptor_authority()
                .unwrap();
            let records: Vec<_> = ["binding", "manifest"]
                .into_iter()
                .map(|name| {
                    root.open_inherited_regular(OsStr::new(name), false)
                        .unwrap()
                        .unwrap()
                })
                .collect();
            let observations: Vec<_> = records
                .iter()
                .map(|record| {
                    let observed = record.regular_file_observation().unwrap();
                    (
                        record.inherited_descriptor().unwrap(),
                        record.digest_regular_file_stable_exact(&observed).unwrap(),
                        observed.size(),
                    )
                })
                .collect();
            let projection = ExternalGuestInputProjection {
                schema: EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
                base_snapshot: GuestBaseSnapshotInput {
                    descriptor: base.inherited_descriptor().unwrap(),
                    snapshot_hash: "1".repeat(64),
                    closure_digest: "2".repeat(64),
                    object_count: 3,
                    blob_count: 0,
                    total_bytes: 0,
                },
                workspace_outputs: None,
                inputs: vec![GuestMountInput {
                    role: GuestMountRole::Source,
                    authority_id: "evaluator".into(),
                    descriptor: source.inherited_descriptor().unwrap(),
                    destination: "/source/evaluator".into(),
                    kind: GuestMountKind::Directory,
                    access: GuestMountAccess::ReadOnly,
                    normalized_mode: None,
                    bytes: 0,
                    content_authority: GuestMountContentAuthority::SourceClosure {
                        binding_descriptor: observations[0].0,
                        binding_hash: observations[0].1.clone(),
                        binding_bytes: observations[0].2,
                        manifest_descriptor: observations[1].0,
                        manifest_hash: observations[1].1.clone(),
                        manifest_bytes: observations[1].2,
                    },
                }],
                executable_search: Vec::new(),
                environment: Default::default(),
            };
            Self {
                _temporary: temporary,
                projection,
                base,
                source,
                records,
            }
        }

        fn authority(
            &self,
            projection: ExternalGuestInputProjection,
            records: Vec<lillux::InheritedDescriptorAuthority>,
            lifelines: Vec<Box<dyn Send + Sync>>,
        ) -> Result<ExternalGuestInputAuthority> {
            ExternalGuestInputAuthority::new(
                projection,
                self.base.clone(),
                None,
                vec![self.source.clone()],
                records,
                lifelines,
            )
        }
    }

    #[test]
    fn retained_source_records_require_exact_count_order_length_and_hash() {
        let fixture = Fixture::new();
        let accepted = fixture
            .authority(fixture.projection.clone(), fixture.records.clone(), vec![])
            .unwrap();
        assert_eq!(accepted.retained_descriptors().len(), 4);
        for records in [
            vec![],
            vec![fixture.records[0].clone()],
            vec![fixture.records[1].clone(), fixture.records[0].clone()],
            vec![
                fixture.records[0].clone(),
                fixture.records[1].clone(),
                fixture.records[0].clone(),
            ],
        ] {
            assert!(
                fixture
                    .authority(fixture.projection.clone(), records, vec![])
                    .is_err()
            );
        }
        for binding in [false, true] {
            for size in [false, true] {
                let mut projection = fixture.projection.clone();
                if let GuestMountContentAuthority::SourceClosure {
                    binding_hash,
                    binding_bytes,
                    manifest_hash,
                    manifest_bytes,
                    ..
                } = &mut projection.inputs[0].content_authority
                {
                    match (binding, size) {
                        (true, true) => *binding_bytes += 1,
                        (false, true) => *manifest_bytes += 1,
                        (true, false) => *binding_hash = "a".repeat(64),
                        (false, false) => *manifest_hash = "b".repeat(64),
                    }
                }
                assert!(
                    fixture
                        .authority(projection, fixture.records.clone(), vec![])
                        .unwrap_err()
                        .to_string()
                        .contains("record authority changed")
                );
            }
        }
    }

    #[test]
    fn retained_source_authority_keeps_original_lifelines_until_drop() {
        struct Lifeline(Arc<AtomicUsize>);
        impl Drop for Lifeline {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let fixture = Fixture::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let authority = fixture
            .authority(
                fixture.projection.clone(),
                fixture.records.clone(),
                vec![Box::new(Lifeline(drops.clone()))],
            )
            .unwrap();
        assert_eq!(authority.retained_lifeline_count(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let descriptors = authority.retained_descriptors();
        assert_eq!(
            descriptors[2].inherited_descriptor().unwrap(),
            fixture.records[0].inherited_descriptor().unwrap()
        );
        assert_eq!(
            descriptors[3].inherited_descriptor().unwrap(),
            fixture.records[1].inherited_descriptor().unwrap()
        );
        drop(authority);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
