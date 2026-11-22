//! Real StateStore receiver/publication/CAS/reopen tests. The reused fixture
//! supplies retained process/death and native testimony; no process is launched,
//! no opaque admission/death proof is fabricated, and no live callback is minted.
//! CAS readback here is structural owner readback, not the protected consumer
//! callback API or a semantic qualification/physical writer-exclusion proof.
use super::*;
use ryeos_state::external_execution::ExecutionChannelBinding;
use ryeos_state::objects::{
    QualificationWorkerTranscriptManifest, QualificationWorkerTranscriptStream,
};

struct Harness {
    root: tempfile::TempDir,
    signer: Arc<dyn Signer>,
    trust: Arc<ryeos_state::refs::TrustStore>,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let identity =
            ryeos_node_runtime::identity::NodeIdentity::create(&root.path().join("node-key.pem"))
                .unwrap();
        let signer: Arc<dyn Signer> = Arc::new(NodeIdentitySigner::from_identity(&identity));
        let mut trust = ryeos_state::refs::TrustStore::new();
        trust.insert(
            identity.fingerprint().to_string(),
            *identity.verifying_key(),
        );
        Self {
            root,
            signer,
            trust: Arc::new(trust),
        }
    }

    fn open(&self) -> super::test_native::TestStateStore {
        super::test_native::TestStateStore::open(
            self.root.path(),
            self.signer.clone(),
            self.trust.clone(),
        )
        .unwrap()
    }
}

fn seed(store: &StateStore, with_capture: bool) -> ExecutionChannelBinding {
    // No in-memory replacement DB: the real StateStore owns this SQLite file.
    store
        .lock()
        .unwrap()
        .runtime_db
        .seed_retained_qualification_transcript_test_fixture(with_capture)
}

fn read_stream(
    store: &StateStore,
    hash: &str,
) -> (
    QualificationWorkerTranscriptManifest,
    QualificationWorkerTranscriptStream,
) {
    let authority = store.pinned_state_authority().unwrap();
    let _guard = authority.acquire_shared_guard().unwrap();
    let cas = authority.cas_store().unwrap();
    let value = cas.get_object_bounded(hash, 1024 * 1024).unwrap().unwrap();
    let manifest = QualificationWorkerTranscriptManifest::from_current_value(&value).unwrap();
    assert_eq!(
        ryeos_state::objects::canonical_value_digest(&value).unwrap(),
        hash
    );
    let chunks = manifest
        .chunks
        .iter()
        .map(|chunk| {
            cas.get_blob_bounded(&chunk.blob_hash, chunk.size_bytes)
                .unwrap()
                .unwrap()
        })
        .collect::<Vec<_>>();
    let stream = manifest.verify_chunks(&chunks).unwrap();
    (manifest, stream)
}

fn original_stage(store: &StateStore, placement: &str) -> String {
    store
        .lock()
        .unwrap()
        .runtime_db
        .retained_q_fixture_receiver_plan(placement)
        .unwrap()
        .unwrap()
        .receiver_stage
        .unwrap()
        .stage_id
}

fn damage_capture(
    store: &StateStore,
    placement: &str,
    metadata: bool,
    missing: bool,
) -> (PathBuf, Option<Vec<u8>>) {
    let plan = store
        .lock()
        .unwrap()
        .runtime_db
        .retained_q_fixture_receiver_plan(placement)
        .unwrap()
        .unwrap();
    let reference = &plan.receiver_stage.unwrap().reference;
    let authority = store.pinned_state_authority().unwrap();
    let cas = authority.cas_store().unwrap();
    let path = if metadata {
        lillux::cas::shard_path(cas.root(), "objects", &reference.metadata_hash, ".json")
    } else {
        lillux::cas::shard_path(cas.root(), "blobs", &reference.blob_hash, "")
    };
    assert!(path.is_file());
    // Deliberate corruption of the owned test store's original retained bytes.
    if missing {
        std::fs::remove_file(&path).unwrap();
        (path, None)
    } else {
        let size = usize::try_from(std::fs::metadata(&path).unwrap().len()).unwrap();
        let changed = vec![b'x'; size];
        std::fs::write(&path, &changed).unwrap();
        (path, Some(changed))
    }
}

fn assert_damage_not_repaired(path: &Path, expected: &Option<Vec<u8>>) {
    match expected {
        Some(bytes) => assert_eq!(std::fs::read(path).unwrap(), *bytes),
        None => assert_eq!(
            std::fs::read(path).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        ),
    }
}

#[test]
fn public_q_publication_requires_applied_native_and_replays_exactly_after_cold_open() {
    for with_capture in [false, true] {
        let harness = Harness::new();
        let store = harness.open();
        let binding = seed(&store, with_capture);
        let placement = &binding.placement_thread_id;
        let roots_before = store.active_resume_snapshot_roots().unwrap();
        // Both null and non-null capture must first have the real Applied join.
        assert!(
            store
                .retain_qualification_worker_transcript(placement)
                .is_err()
        );
        assert_eq!(store.active_resume_snapshot_roots().unwrap(), roots_before);
        assert!(
            store
                .apply_external_qualification_scope_settlement(placement)
                .unwrap()
                .is_some()
        );
        let stage = with_capture.then(|| original_stage(&store, placement));
        let first = store
            .retain_qualification_worker_transcript(placement)
            .unwrap();
        assert!(
            store
                .active_resume_snapshot_roots()
                .unwrap()
                .contains(&first.manifest_hash)
        );
        let (manifest, stream) = read_stream(&store, &first.manifest_hash);
        assert_eq!(manifest.fixture_capture.is_some(), with_capture);
        assert_eq!(stream.fixture_content.is_some(), with_capture);
        if with_capture {
            assert_eq!(
                stream
                    .fixture_content
                    .as_ref()
                    .unwrap()
                    .verify_content_identity(manifest.fixture_capture.as_ref().unwrap())
                    .unwrap(),
                b"after"
            );
        }
        drop(store);
        let reopened = harness.open();
        let replay = reopened
            .retain_qualification_worker_transcript(placement)
            .unwrap();
        assert_eq!(replay, first);
        assert_eq!(
            read_stream(&reopened, &first.manifest_hash),
            (manifest, stream)
        );
        if let Some(stage) = stage {
            assert_eq!(original_stage(&reopened, placement), stage);
        }
    }
}

#[test]
fn public_q_publication_refuses_missing_or_corrupt_original_receiver_cas() {
    for metadata in [false, true] {
        for missing in [false, true] {
            let harness = Harness::new();
            let store = harness.open();
            let binding = seed(&store, true);
            let placement = &binding.placement_thread_id;
            store
                .apply_external_qualification_scope_settlement(placement)
                .unwrap()
                .unwrap();
            let stage = original_stage(&store, placement);
            let roots_before = store.active_resume_snapshot_roots().unwrap();
            let (damaged_path, expected_damage) =
                damage_capture(&store, placement, metadata, missing);
            assert!(
                store
                    .retain_qualification_worker_transcript(placement)
                    .is_err()
            );
            assert_eq!(store.active_resume_snapshot_roots().unwrap(), roots_before);
            assert_eq!(original_stage(&store, placement), stage);
            assert_damage_not_repaired(&damaged_path, &expected_damage);
        }
    }
}

#[test]
fn public_q_cold_replay_does_not_repair_original_receiver_cas_from_signed_content() {
    for metadata in [false, true] {
        for missing in [false, true] {
            let harness = Harness::new();
            let store = harness.open();
            let binding = seed(&store, true);
            let placement = &binding.placement_thread_id;
            store
                .apply_external_qualification_scope_settlement(placement)
                .unwrap()
                .unwrap();
            let first = store
                .retain_qualification_worker_transcript(placement)
                .unwrap();
            let stage = original_stage(&store, placement);
            let (damaged_path, expected_damage) =
                damage_capture(&store, placement, metadata, missing);
            drop(store);
            let reopened = harness.open();
            // Transcript chunks still contain the exact signed capture bundle.
            // Those bytes cannot replace original receiver custody on replay.
            let (manifest, stream) = read_stream(&reopened, &first.manifest_hash);
            assert!(manifest.fixture_capture.is_some() && stream.fixture_content.is_some());
            assert!(
                reopened
                    .retain_qualification_worker_transcript(placement)
                    .is_err()
            );
            assert_eq!(original_stage(&reopened, placement), stage);
            assert_damage_not_repaired(&damaged_path, &expected_damage);
            assert!(
                reopened
                    .active_resume_snapshot_roots()
                    .unwrap()
                    .contains(&first.manifest_hash)
            );
        }
    }
}
