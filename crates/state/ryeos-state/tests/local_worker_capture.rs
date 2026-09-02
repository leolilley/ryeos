use std::collections::BTreeMap;
use std::path::Path;

use ryeos_state::objects::{
    LogicalSourceRoot, SourceClosureFile, SourceClosureManifest, SourceFileMode,
};

fn captured_directory_digest(worker_root: &Path) -> String {
    let mut pending = vec![worker_root.to_path_buf()];
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
            let relative = child_path.strip_prefix(worker_root).unwrap();
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
    SourceClosureManifest::new(
        vec![LogicalSourceRoot {
            id: "source".to_owned(),
        }],
        entries,
    )
    .unwrap()
    .digest()
    .unwrap()
}

#[test]
fn shipped_local_worker_pins_the_production_capture_digest() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("ryeos-state lives below the repository root");
    let worker_root =
        repository.join("bundles/local-inference/.ai/workers/local-inference/lib/local-tinygrad");
    let observed = captured_directory_digest(&worker_root);

    let worker_item = std::fs::read_to_string(
        repository.join("bundles/local-inference/.ai/workers/local-inference/local-tinygrad.yaml"),
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
        repository.join("bundles/local-inference/.ai/workers/local-inference/local-tinygrad.yaml"),
    )
    .unwrap();
    let body = lillux::signature::strip_signature_lines(&worker_item);
    let worker: serde_yaml::Value = serde_yaml::from_str(&body).unwrap();
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
            &std::fs::read_to_string(repository.join(
                "bundles/local-inference/.ai/config/ryeos-runtime/local-tinygrad-activation.yaml",
            ))
            .unwrap(),
        ))
        .unwrap();
    assert_eq!(
        fixture["consumer_ref"].as_str(),
        Some("worker:local-inference/local-tinygrad")
    );
    assert!(fixture.get("persistent_session_policy").is_none());
    let release: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            repository.join("scripts/release/local-inference-qwen3-0.6b-v1.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let releases = release["realizations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| (entry["component"].as_str().unwrap(), entry))
        .collect::<BTreeMap<_, _>>();
    let sources = fixture["sources"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|entry| (entry["id"].as_str().unwrap(), entry))
        .collect::<BTreeMap<_, _>>();
    let captured = fixture["components"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|entry| {
            let id = entry["id"].as_str().unwrap();
            let release = releases[id];
            let source_id = format!("{id}_archive");
            let source = sources[source_id.as_str()];
            let shape = &entry["shape"];
            assert_eq!(shape["kind"].as_str(), Some("whole_archive_tree"));
            assert_eq!(shape["source"].as_str(), Some(source_id.as_str()));
            assert_eq!(shape["prefix"].as_str(), release["prefix"].as_str());
            assert_eq!(
                shape["bounds"],
                serde_yaml::to_value(&release["bounds"]).unwrap()
            );
            assert_eq!(entry["storage"].as_str(), release["storage"].as_str());
            assert_eq!(source["url"].as_str(), release["url"].as_str());
            assert_eq!(source["sha256"].as_str(), release["sha256"].as_str());
            assert_eq!(declared[id], release["manifest_hash"].as_str().unwrap());
            (id.to_owned(), declared[id].to_owned())
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(captured, declared);
}
