use super::*;
use crate::objects::*;

fn fixture() -> (EffectiveSourceBinding, SourceClosureManifest) {
    let manifest = SourceClosureManifest::new(
        vec![LogicalSourceRoot {
            id: "source".to_owned(),
        }],
        vec![
            SourceClosureFile {
                root: "source".to_owned(),
                path: "run.py".to_owned(),
                blob_hash: lillux::sha256_hex(b"run"),
                size: 3,
                mode: SourceFileMode::ReadOnly,
            },
            SourceClosureFile {
                root: "source".to_owned(),
                path: "lib/helper.py".to_owned(),
                blob_hash: lillux::sha256_hex(b"helper"),
                size: 6,
                mode: SourceFileMode::ReadOnly,
            },
        ],
    )
    .unwrap();
    let schema_body = "kind: kind\n".to_owned();
    let binding = EffectiveSourceBinding {
        schema: EFFECTIVE_SOURCE_BINDING_SCHEMA,
        kind: EFFECTIVE_SOURCE_BINDING_KIND.to_owned(),
        owner: SourceOwnerIdentity {
            canonical_ref: "tool:test/run".to_owned(),
            item_kind: "tool".to_owned(),
            source_space: SourceSpaceIdentity::Project,
            source_root: SourceRootIdentity::Project,
            root_source_content_digest: "a".repeat(64),
            root_raw_content_digest: "b".repeat(64),
            signer_fingerprint: "c".repeat(64),
            logical_item_key: "test/run".to_owned(),
        },
        kind_ceiling: SignedKindSourceCeiling {
            schema_ref: "kind:tool".to_owned(),
            source_content_digest: "d".repeat(64),
            raw_content_digest: lillux::signature::content_hash(&schema_body),
            signer_fingerprint: "f".repeat(64),
            signature_header: "signed".to_owned(),
            schema_body,
            schema_document: serde_json::json!({"kind": "kind", "location": {"directory": "tools"}}),
            normalized_declaration: serde_json::json!({
                "derived": SOURCE_CLOSURE_DERIVED_KEY,
                "location": {"type": "item_namespace"}, "testimony": "owner_signed_files",
                "max_files": 8, "max_total_bytes": 1024, "max_file_bytes": 512, "max_depth": 8,
            }),
            root_kind_format: serde_json::json!({"extensions": ["yaml"]}),
            root_signature_envelope: serde_json::json!({"style": "header"}),
        },
        content_manifest_hash: manifest.digest().unwrap(),
        testimony: SourceTestimonyProof::OwnerSignedFiles {
            signer_fingerprint: "c".repeat(64),
            file_count: 2,
            entries_digest: "2".repeat(64),
        },
        execution_policy: SourceExecutionPolicyIdentity::Executor {
            declarer_ref: "tool:ryeos/core/runtimes/python/function".to_owned(),
            signer_fingerprint: "3".repeat(64),
            source_content_digest: "4".repeat(64),
            raw_content_digest: "5".repeat(64),
            policy_digest: "6".repeat(64),
            chain_digest: "7".repeat(64),
        },
        logical_binding: SourceLogicalBinding::Tool {
            loader_roots: vec![SourceLoaderRoot::ItemDirectory],
            root_entry: "run.py".to_owned(),
        },
    };
    (binding, manifest)
}

fn canonical<T: serde::Serialize>(value: &T) -> Vec<u8> {
    lillux::canonical_json(&serde_json::to_value(value).unwrap())
        .unwrap()
        .into_bytes()
}

fn records(
    binding: &EffectiveSourceBinding,
    manifest: &SourceClosureManifest,
) -> anyhow::Result<VerifiedAdmittedSourceRecords> {
    VerifiedAdmittedSourceRecords::from_canonical_bytes(
        &lillux::sha256_hex(&canonical(binding)),
        &lillux::sha256_hex(&canonical(manifest)),
        &canonical(binding),
        &canonical(manifest),
    )
}

#[test]
fn verified_records_retain_original_identity_coordinates_and_protected_environment() {
    let (binding, manifest) = fixture();
    let verified = records(&binding, &manifest).unwrap();
    assert_eq!(verified.binding(), &binding);
    assert_eq!(verified.manifest(), &manifest);
    assert_eq!(verified.binding_hash(), binding.digest().unwrap());
    assert_eq!(verified.content_manifest_hash(), manifest.digest().unwrap());
    assert_eq!(verified.logical_entry(), "run.py");
    assert_eq!(verified.logical_project_mount(), ".ai/tools/test");
    assert_eq!(
        verified.runtime_relative_mount(),
        format!("source-closures/{}", binding.digest().unwrap())
    );
    assert_eq!(
        verified.runtime_entry_path(),
        Path::new(EXECUTION_RUNTIME_REALIZATIONS_ROOT)
            .join(verified.runtime_relative_mount())
            .join("run.py")
    );
    let env: serde_json::Value = serde_json::from_str(verified.sealed_identity_env()).unwrap();
    assert_eq!(
        env,
        serde_json::json!({"schema": 1, "binding_hash": binding.digest().unwrap(),
        "content_manifest_hash": manifest.digest().unwrap(), "owner_key": binding.owner_key().unwrap()})
    );
    let projection = EffectiveSourceClosureProjection {
        schema: 1,
        binding_hash: binding.digest().unwrap(),
        content_manifest_hash: manifest.digest().unwrap(),
        owner_key: binding.owner_key().unwrap(),
        file_count: 2,
        total_bytes: 9,
    };
    verified.validate_projection(&projection).unwrap();
    let mut wrong = projection.clone();
    wrong.owner_key = "f".repeat(64);
    assert!(verified.validate_projection(&wrong).is_err());
    let mut wrong = projection.clone();
    wrong.file_count = 1;
    assert!(verified.validate_projection(&wrong).is_err());
    let mut wrong = projection;
    wrong.total_bytes = 8;
    assert!(verified.validate_projection(&wrong).is_err());
}

#[test]
fn record_bytes_require_exact_bounded_canonical_json_and_admitted_digests() {
    let (binding, manifest) = fixture();
    let binding_bytes = canonical(&binding);
    let manifest_bytes = canonical(&manifest);
    let binding_hash = binding.digest().unwrap();
    let manifest_hash = manifest.digest().unwrap();
    for bytes in [
        vec![],
        b"{".to_vec(),
        b"null".to_vec(),
        vec![b' '; MAX_SOURCE_BINDING_BYTES + 1],
        [binding_bytes.as_slice(), b"\n"].concat(),
        serde_json::to_vec_pretty(&binding).unwrap(),
    ] {
        assert!(
            VerifiedAdmittedSourceRecords::from_canonical_bytes(
                &lillux::sha256_hex(&bytes),
                &manifest_hash,
                &bytes,
                &manifest_bytes
            )
            .is_err()
        );
    }
    assert!(
        VerifiedAdmittedSourceRecords::from_canonical_bytes(
            &"0".repeat(64),
            &manifest_hash,
            &binding_bytes,
            &manifest_bytes
        )
        .is_err()
    );
    assert!(
        VerifiedAdmittedSourceRecords::from_canonical_bytes(
            &binding_hash,
            &"0".repeat(64),
            &binding_bytes,
            &manifest_bytes
        )
        .is_err()
    );
    let noncanonical = [manifest_bytes.as_slice(), b"\n"].concat();
    assert!(
        VerifiedAdmittedSourceRecords::from_canonical_bytes(
            &binding_hash,
            &lillux::sha256_hex(&noncanonical),
            &binding_bytes,
            &noncanonical
        )
        .is_err()
    );
    let too_large = vec![b' '; MAX_SOURCE_MANIFEST_BYTES + 1];
    assert!(
        VerifiedAdmittedSourceRecords::from_canonical_bytes(
            &binding_hash,
            &lillux::sha256_hex(&too_large),
            &binding_bytes,
            &too_large
        )
        .is_err()
    );
    // Parsing duplicate keys is not canonical even if the last value matches.
    let duplicate = format!(
        "{{\"schema\":1,{}",
        std::str::from_utf8(&binding_bytes[1..]).unwrap()
    )
    .into_bytes();
    assert!(
        VerifiedAdmittedSourceRecords::from_canonical_bytes(
            &lillux::sha256_hex(&duplicate),
            &manifest_hash,
            &duplicate,
            &manifest_bytes
        )
        .is_err()
    );
}

#[test]
fn valid_json_cannot_substitute_manifest_owner_entry_or_kind_ceiling() {
    let (binding, manifest) = fixture();
    let mut substituted = manifest.clone();
    substituted.entries[0].blob_hash = "e".repeat(64);
    assert!(records(&binding, &substituted).is_err());
    let mut wrong = binding.clone();
    wrong.owner.signer_fingerprint = "e".repeat(64);
    assert!(records(&wrong, &manifest).is_err());
    let mut wrong = binding.clone();
    let SourceLogicalBinding::Tool { root_entry, .. } = &mut wrong.logical_binding else {
        unreachable!()
    };
    *root_entry = "absent.py".to_owned();
    assert!(records(&wrong, &manifest).is_err());
    let mut wrong = binding.clone();
    wrong.kind_ceiling.normalized_declaration["max_files"] = serde_json::json!(1);
    assert!(records(&wrong, &manifest).is_err());
    let mut wrong = manifest.clone();
    wrong.totals.total_bytes += 1;
    assert!(records(&binding, &wrong).is_err());
    let mut wrong = binding.clone();
    wrong.kind_ceiling.schema_document["location"]["directory"] = serde_json::json!("../escape");
    assert!(records(&wrong, &manifest).is_err());
}

// These harness operations intentionally create invalid host states. Product
// verification uses only Lillux descriptor authorities, never these path APIs.
#[cfg(unix)]
fn populated_tree() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("lib")).unwrap();
    for (path, bytes) in [
        ("run.py", b"run".as_slice()),
        ("lib/helper.py", b"helper".as_slice()),
    ] {
        std::fs::write(root.path().join(path), bytes).unwrap();
        std::fs::set_permissions(
            root.path().join(path),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }
    root
}

#[test]
#[cfg(unix)]
fn exact_tree_rejects_missing_extra_bytes_size_mode_and_empty_directories() {
    use std::os::unix::fs::PermissionsExt;
    let (binding, manifest) = fixture();
    let verified = records(&binding, &manifest).unwrap();
    let good = populated_tree();
    verified
        .verify_tree(&lillux::PinnedDirectory::open(good.path()).unwrap().unwrap())
        .unwrap();
    for mutation in 0..7 {
        let root = populated_tree();
        let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        match mutation {
            0 => std::fs::remove_file(root.path().join("run.py")).unwrap(),
            1 => std::fs::write(root.path().join("ambient.py"), b"ambient").unwrap(),
            2 => std::fs::write(root.path().join("run.py"), b"bad").unwrap(),
            3 => std::fs::write(root.path().join("run.py"), b"longer").unwrap(),
            4 => std::fs::set_permissions(
                root.path().join("run.py"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap(),
            5 => std::fs::create_dir(root.path().join("undeclared")).unwrap(),
            6 => {
                std::fs::remove_file(root.path().join("lib/helper.py")).unwrap();
                std::fs::remove_dir(root.path().join("lib")).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            verified.verify_tree(&pinned).is_err(),
            "mutation {mutation}"
        );
    }
}

#[test]
#[cfg(unix)]
fn exact_tree_does_not_follow_regular_or_directory_symlinks() {
    use std::os::unix::fs::symlink;
    let (binding, manifest) = fixture();
    let verified = records(&binding, &manifest).unwrap();
    for directory in [false, true] {
        let root = populated_tree();
        let foreign = populated_tree();
        let path = if directory { "lib" } else { "run.py" };
        if directory {
            std::fs::remove_dir_all(root.path().join(path)).unwrap();
        } else {
            std::fs::remove_file(root.path().join(path)).unwrap();
        }
        symlink(foreign.path().join(path), root.path().join(path)).unwrap();
        assert!(
            verified
                .verify_tree(&lillux::PinnedDirectory::open(root.path()).unwrap().unwrap())
                .is_err()
        );
    }
}

#[test]
#[cfg(unix)]
fn tree_verification_uses_pinned_authority_not_diagnostic_path() {
    let (binding, manifest) = fixture();
    let verified = records(&binding, &manifest).unwrap();
    let root = populated_tree();
    let pinned = lillux::PinnedDirectory::from_open_directory(
        PathBuf::from("<inherited-admitted-source>"),
        std::fs::File::open(root.path()).unwrap(),
    )
    .unwrap();
    verified.verify_tree(&pinned).unwrap();
}

#[test]
#[cfg(unix)]
fn executable_tree_mode_is_verified_not_inferred_from_filename() {
    use std::os::unix::fs::PermissionsExt;
    let (mut binding, mut manifest) = fixture();
    manifest
        .entries
        .iter_mut()
        .find(|entry| entry.path == "run.py")
        .unwrap()
        .mode = SourceFileMode::Executable;
    binding.content_manifest_hash = manifest.digest().unwrap();
    let verified = records(&binding, &manifest).unwrap();
    let root = populated_tree();
    let pinned = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
    assert!(verified.verify_tree(&pinned).is_err());
    std::fs::set_permissions(
        root.path().join("run.py"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    verified.verify_tree(&pinned).unwrap();
}
