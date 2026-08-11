use std::collections::BTreeMap;
use std::path::Path;

use ryeos_state::objects::{
    LogicalSourceRoot, SourceClosureFile, SourceClosureManifest, SourceFileMode,
};

#[test]
fn shipped_local_worker_pins_the_production_capture_digest() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("ryeos-state lives below the repository root");
    let worker_root = repository.join("bundles/standard/.ai/workers/standard/lib/local-tinygrad");
    let mut pending = vec![worker_root.clone()];
    let mut entries = Vec::new();
    while let Some(directory) = pending.pop() {
        let mut children = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect::<Vec<_>>();
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let file_type = child.file_type().unwrap();
            if file_type.is_dir() {
                pending.push(child.path());
                continue;
            }
            assert!(
                file_type.is_file(),
                "worker source contains only regular files"
            );
            let child_path = child.path();
            let bytes = std::fs::read(&child_path).unwrap();
            let relative = child_path.strip_prefix(&worker_root).unwrap();
            let path = relative
                .to_str()
                .expect("worker source paths are UTF-8")
                .replace(std::path::MAIN_SEPARATOR, "/");
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt as _;
                if child.metadata().unwrap().permissions().mode() & 0o111 == 0 {
                    SourceFileMode::ReadOnly
                } else {
                    SourceFileMode::Executable
                }
            };
            #[cfg(not(unix))]
            let mode = SourceFileMode::ReadOnly;
            entries.push(SourceClosureFile {
                root: "source".to_owned(),
                path,
                blob_hash: lillux::sha256_hex(&bytes),
                size: bytes.len() as u64,
                mode,
            });
        }
    }
    let observed = SourceClosureManifest::new(
        vec![LogicalSourceRoot {
            id: "source".to_owned(),
        }],
        entries,
    )
    .unwrap()
    .digest()
    .unwrap();

    let worker_item = std::fs::read_to_string(
        repository.join("bundles/standard/.ai/workers/standard/local-tinygrad.yaml"),
    )
    .unwrap();
    let body = lillux::signature::strip_signature_lines(&worker_item);
    let value: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
    let declared = value["source"]["digest"]
        .as_str()
        .expect("worker item declares its adjacent source digest");
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

    let fixture: serde_yaml::Value =
        serde_yaml::from_str(&lillux::signature::strip_signature_lines(
            &std::fs::read_to_string(
                repository.join(
                    "bundles/standard/.ai/config/ryeos-runtime/local-tinygrad-activation.yaml",
                ),
            )
            .unwrap(),
        ))
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
            let expected_schema = match storage {
                "content" => ryeos_state::objects::EXTERNAL_CONTENT_TREE_SCHEMA,
                "large_content" => ryeos_state::objects::EXTERNAL_LARGE_CONTENT_SCHEMA,
                _ => unreachable!("fixture storage was validated above"),
            };
            assert_eq!(entry["manifest_schema"].as_str(), Some(expected_schema));
            assert_eq!(entry["maximum_bytes"].as_u64(), Some(maximum_bytes));
            (
                id.to_owned(),
                entry["expected_manifest_hash"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(captured, declared);
}
