use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

use ryeos_state::{
    ExternalCapturePolicy, ExternalContentBlobSink, LaunchCaptureBudget, capture_tree,
};

struct DigestOnlySink;

impl ExternalContentBlobSink for DigestOnlySink {
    fn store_file(
        &mut self,
        mut file: std::fs::File,
        _path: &str,
        expected_size: u64,
    ) -> anyhow::Result<(String, u64)> {
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 == expected_size,
            "fixture changed while read"
        );
        Ok((lillux::sha256_hex(&bytes), expected_size))
    }

    fn store_target(&mut self, target: &[u8], _path: &str) -> anyhow::Result<String> {
        Ok(lillux::sha256_hex(target))
    }
}

#[test]
fn shipped_local_worker_pins_the_production_capture_digest() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("ryeos-state lives below the repository root");
    let worker_root = repository.join("bundles/standard/.ai/workers/lib/tinygrad_qwen");
    let pinned = lillux::PinnedDirectory::open(&worker_root)
        .unwrap()
        .expect("shipped local worker tree exists");
    let ignore = ryeos_state::ignore::matcher_from_builtins();
    let policy =
        ExternalCapturePolicy::new(".ai/workers/lib/tinygrad_qwen".to_owned(), &ignore).unwrap();
    let manifest = capture_tree(
        &pinned,
        &[],
        &policy,
        &mut LaunchCaptureBudget::default(),
        &mut DigestOnlySink,
    )
    .unwrap();
    let observed = lillux::sha256_hex(
        lillux::canonical_json(&serde_json::to_value(&manifest).unwrap())
            .unwrap()
            .as_bytes(),
    );

    let worker_item = std::fs::read_to_string(
        repository.join("bundles/standard/.ai/workers/standard/local-tinygrad.yaml"),
    )
    .unwrap();
    let body = lillux::signature::strip_signature_lines(&worker_item);
    let value: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
    let declared = value["external_content"][0]["digest"]
        .as_str()
        .expect("worker item declares its own source-tree digest");
    assert_eq!(observed, declared);
}

#[test]
fn activation_fixture_matches_every_sourceless_worker_realization() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("ryeos-state lives below the repository root");
    let worker_item = std::fs::read_to_string(
        repository.join("bundles/standard/.ai/workers/standard/local-tinygrad.yaml"),
    )
    .unwrap();
    let body = lillux::signature::strip_signature_lines(&worker_item);
    let worker: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
    let worker_lifecycle =
        serde_yaml::from_str::<serde_yaml::Value>(&lillux::signature::strip_signature_lines(
            &std::fs::read_to_string(
                repository
                    .join("bundles/core/.ai/node/engine/kinds/worker/worker.kind-schema.yaml"),
            )
            .unwrap(),
        ))
        .unwrap();
    let declared = worker["external_content"]
        .as_sequence()
        .unwrap()
        .iter()
        .filter(|entry| entry.get("locator").is_none())
        .map(|entry| {
            (
                entry["id"].as_str().unwrap().to_owned(),
                entry["digest"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let fixture: serde_yaml::Value = serde_yaml::from_str(
        &std::fs::read_to_string(
            repository
                .join("bundles/standard/.ai/workers/lib/tinygrad_qwen/activation-fixture.yaml"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        fixture["consumer_ref"].as_str(),
        Some("worker:standard/local-tinygrad")
    );
    let pool = &fixture["persistent_session_policy"];
    let lifecycle = &worker_lifecycle["execution"]["persistent_session"];
    assert!(
        pool["max_total_processes"].as_u64().unwrap()
            >= lifecycle["max_processes"].as_u64().unwrap()
    );
    assert!(
        pool["max_total_address_space_bytes"].as_u64().unwrap()
            >= lifecycle["max_address_space_bytes"].as_u64().unwrap()
    );
    assert!(
        pool["max_total_cpu_seconds"].as_u64().unwrap()
            >= lifecycle["max_cpu_seconds"].as_u64().unwrap()
    );
    let expected_imports = BTreeMap::from([
        ("runtime", ("runtime", "content", 104_857_600_u64)),
        ("tinygrad", ("tinygrad", "content", 33_554_432_u64)),
        ("toolchain", ("toolchain", "large_content", 335_544_320_u64)),
        ("model", ("model", "large_content", 1_677_721_600_u64)),
    ]);
    let captured = fixture["components"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|entry| {
            assert_eq!(entry["shape"].as_str(), Some("tree"));
            let id = entry["id"].as_str().unwrap();
            let (path, storage, maximum_bytes) = expected_imports[id];
            assert_eq!(entry["path"].as_str(), Some(path));
            assert_eq!(entry["storage"].as_str(), Some(storage));
            assert_eq!(entry["maximum_bytes"].as_u64(), Some(maximum_bytes));
            (
                id.to_owned(),
                entry["expected_manifest_hash"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(captured, declared);
}
