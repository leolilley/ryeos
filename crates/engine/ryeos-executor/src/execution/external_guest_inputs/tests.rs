use super::*;
use ryeos_state::objects::{
    EXTERNAL_CONTENT_MANIFEST_KIND, EXTERNAL_CONTENT_TREE_SCHEMA,
    EXTERNAL_LARGE_CONTENT_MANIFEST_KIND, EXTERNAL_LARGE_CONTENT_SCHEMA,
    ExternalContentManifestObject, ExternalContentMode, ExternalContentMountRoot,
    ExternalContentRealizationSet,
};
use serde_json::json;

fn fixture(large: bool) -> (ExternalContentRealization, serde_json::Value) {
    let value = json!({
        "schema": if large { EXTERNAL_LARGE_CONTENT_SCHEMA } else { EXTERNAL_CONTENT_TREE_SCHEMA },
        "kind": if large { EXTERNAL_LARGE_CONTENT_MANIFEST_KIND } else { EXTERNAL_CONTENT_MANIFEST_KIND },
        "entries": [{"path":"content","kind":"file","mode":493,
            "blob_hash":lillux::sha256_hex(b"product"),"size":7}],
        "entry_count":1,"total_bytes":7,
    });
    let entry = ExternalContentRealization {
        id: "runtime".to_owned(),
        kind: ExternalContentKind::File,
        mode: ExternalContentMode::Pinned,
        manifest_hash: lillux::sha256_hex(lillux::canonical_json(&value).unwrap().as_bytes()),
        entry_count: 1,
        total_bytes: 7,
        mount_root: ExternalContentMountRoot::Project,
        mount: "vendor/runtime".to_owned(),
    };
    (entry, value)
}

#[test]
fn product_manifest_preserves_both_exact_tiers_and_rejects_substituted_counts_hashes_and_shapes() {
    for large in [false, true] {
        let (entry, value) = fixture(large);
        let (kind, canonical) = validated_manifest(&entry, &value).unwrap();
        assert_eq!(
            kind,
            if large {
                GuestProductManifestKind::LargeContent
            } else {
                GuestProductManifestKind::Content
            }
        );
        assert_eq!(
            lillux::sha256_hex(canonical.as_bytes()),
            entry.manifest_hash
        );
        for changed in [
            ExternalContentRealization {
                total_bytes: 8,
                ..entry.clone()
            },
            ExternalContentRealization {
                entry_count: 2,
                ..entry.clone()
            },
            ExternalContentRealization {
                manifest_hash: "f".repeat(64),
                ..entry.clone()
            },
        ] {
            assert!(validated_manifest(&changed, &value).is_err());
        }
        let mut foreign = value.clone();
        foreign["kind"] = json!("foreign_manifest");
        assert!(validated_manifest(&entry, &foreign).is_err());
        let mut non_file = value.clone();
        non_file["entries"][0]["path"] = json!("other");
        let different = ExternalContentRealization {
            manifest_hash: lillux::sha256_hex(
                lillux::canonical_json(&non_file).unwrap().as_bytes(),
            ),
            ..entry.clone()
        };
        assert!(validated_manifest(&different, &non_file).is_err());
        let mut malformed = value;
        malformed["total_bytes"] = json!(8);
        let different = ExternalContentRealization {
            total_bytes: 8,
            manifest_hash: lillux::sha256_hex(
                lillux::canonical_json(&malformed).unwrap().as_bytes(),
            ),
            ..entry
        };
        assert!(validated_manifest(&different, &malformed).is_err());
    }
}

#[test]
fn product_destination_uses_only_selected_override_or_existing_natural_mount() {
    let (mut entry, _) = fixture(false);
    assert_eq!(
        product_destination(&entry, Path::new("/workspace"), None).unwrap(),
        "/workspace/vendor/runtime"
    );
    let admitted = RuntimeDestinationOverride {
        realization_id: "runtime",
        destination: Path::new("/guest/runtime"),
    };
    assert_eq!(
        product_destination(&entry, Path::new("/workspace"), Some(&admitted)).unwrap(),
        "/guest/runtime"
    );
    let other = RuntimeDestinationOverride {
        realization_id: "other",
        destination: Path::new("/guest/other"),
    };
    assert_eq!(
        product_destination(&entry, Path::new("/workspace"), Some(&other)).unwrap(),
        "/workspace/vendor/runtime"
    );
    entry.mount_root = ExternalContentMountRoot::ExecutionRuntime;
    assert_eq!(
        product_destination(&entry, Path::new("/different/project"), None).unwrap(),
        "/ryeos/realizations/vendor/runtime"
    );
    for path in ["relative", "/", "/guest/../escape"] {
        let invalid = RuntimeDestinationOverride {
            realization_id: "runtime",
            destination: Path::new(path),
        };
        assert!(product_destination(&entry, Path::new("/workspace"), Some(&invalid)).is_err());
    }
}

#[test]
#[cfg(target_os = "linux")]
fn product_input_redemption_retains_sorted_descriptors_exact_manifest_and_original_leases() {
    // Existing test-only storage scaffold supplies a resolution shape, not
    // publisher/launch admission. This test invokes real CAS redemption only.
    let root = tempfile::tempdir().unwrap();
    let state = ryeos_app::state::test_support::build(root.path()).unwrap();
    let (mut first, manifest_value) = fixture(false);
    let manifest = ExternalContentManifestObject::from_value(&manifest_value).unwrap();
    let authority = state.state_store.pinned_state_authority().unwrap();
    let cas = authority.cas_store().unwrap();
    cas.store_blob(b"product").unwrap();
    cas.store_object(&serde_json::to_value(&manifest).unwrap())
        .unwrap();
    first.id = "z-runtime".to_owned();
    let second = ExternalContentRealization {
        id: "a-tool".to_owned(),
        mount: "vendor/tool".to_owned(),
        ..first.clone()
    };
    let set = ExternalContentRealizationSet::new(vec![first, second]).unwrap();
    let invocation = serde_json::to_value(
        ryeos_app::thread_lifecycle::SealedRootExecutionRequest::storage_test_fixture(),
    )
    .unwrap();
    let mut resolution: ryeos_engine::resolution::ResolutionOutput =
        serde_json::from_value(invocation["resolution_output"].clone()).unwrap();
    resolution.composed.derived.insert(
        ryeos_state::objects::EXTERNAL_REALIZATIONS_DERIVED_KEY.to_owned(),
        set.to_value().unwrap(),
    );
    let prepared = prepare_product_inputs(
        &state,
        &resolution,
        Path::new("/workspace"),
        Some(RuntimeDestinationOverride {
            realization_id: "z-runtime",
            destination: Path::new("/runtime/code"),
        }),
    )
    .unwrap();
    assert_eq!(
        prepared
            .inputs
            .iter()
            .map(|input| input.authority_id.as_str())
            .collect::<Vec<_>>(),
        ["a-tool", "z-runtime"]
    );
    assert_eq!(prepared.leases.len(), 2);
    assert_eq!(prepared.destinations["a-tool"], "/workspace/vendor/tool");
    assert_eq!(prepared.destinations["z-runtime"], "/runtime/code");
    for ((input, source), record) in prepared
        .inputs
        .iter()
        .zip(&prepared.authorities)
        .zip(&prepared.manifest_authorities)
    {
        assert_eq!(input.descriptor, source.inherited_descriptor().unwrap());
        assert_eq!(input.normalized_mode, Some(0o755));
        assert_eq!(input.access, GuestMountAccess::ReadOnly);
        let (bytes, _) = source.read_regular_file_stable_bounded(7).unwrap();
        assert_eq!(bytes, b"product");
        let GuestMountContentAuthority::ProductManifest {
            manifest_descriptor,
            manifest_hash,
            manifest_bytes,
            ..
        } = &input.content_authority
        else {
            panic!("lost manifest")
        };
        assert_eq!(*manifest_descriptor, record.inherited_descriptor().unwrap());
        let (bytes, _) = record
            .read_regular_file_stable_bounded(*manifest_bytes)
            .unwrap();
        assert_eq!(lillux::sha256_hex(&bytes), *manifest_hash);
        assert_eq!(
            bytes,
            lillux::canonical_json(&manifest_value).unwrap().as_bytes()
        );
        ryeos_state::external_content::realization_verification::verify_staged_external_realization(
            source,
            record,
            EXTERNAL_CONTENT_MANIFEST_KIND,
            manifest_hash,
            *manifest_bytes,
            ExternalContentKind::File,
            input.bytes,
        )
        .unwrap();
    }
    assert!(
        prepare_product_inputs(
            &state,
            &resolution,
            Path::new("/workspace"),
            Some(RuntimeDestinationOverride {
                realization_id: "missing",
                destination: Path::new("/runtime/code"),
            })
        )
        .is_err()
    );
}
