//! Drives the reusable runtime qualification fixtures against a live test node
//! and checks that the attestations they publish are real and current.
#![recursion_limit = "256"]

use std::path::Path;
use std::sync::Arc;

#[path = "../../support/candidate_authoring.rs"]
mod candidate_authoring;
#[path = "../../support/candidate_requirement.rs"]
mod candidate_requirement;
#[path = "../../support/qualification_fixture_node.rs"]
mod qualification_fixture_node;
#[path = "../../support/qualification_node.rs"]
mod qualification_node;
#[path = "../../support/runtime_qualification.rs"]
mod runtime_qualification;
#[path = "../../support/runtime_qualification_fixture.rs"]
mod runtime_qualification_fixture;

use qualification_fixture_node::{
    RuntimeBinaries, install_large_runtime_content, install_runtime_files, private_directory,
    run_on_test_node,
};
use qualification_node::{manifest_hash, predict_large_manifest, start_bundle_node};
use runtime_qualification::{
    ActivatedRuntimeSources, CONTENT_RUNTIME_CLAIMS, GUEST_RUNTIME_REQUIRED_CLAIMS, RuntimeArchive,
    archive_root, write_activated_runtime_sources,
};
use runtime_qualification_fixture::{
    QualifiedActivatedRuntime, QualifiedGuestRuntime, activate_and_qualify_runtime,
    logical_entry_count, qualify_guest_runtime, write_guest_runtime_sources,
};

const BUNDLE_NAME: &str = "external-candidate-authoring";

#[test]
fn guest_and_activated_runtime_qualifications_publish_real_current_attestations() {
    run_on_test_node(|root, mut state| async move {
        let binaries = RuntimeBinaries {
            candidate: Path::new(env!(
                "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-runtime"
            )),
            provider: Path::new(env!(
                "CARGO_BIN_EXE_ryeos-synthetic-external-provider-runtime"
            )),
        };
        let session = private_directory();
        install_runtime_files(session.path(), &binaries, None);
        let guest = private_directory();
        install_runtime_files(guest.path(), &binaries, Some("guest-runtime"));
        let verifier = private_directory();
        install_runtime_files(verifier.path(), &binaries, None);
        let open = |path: &Path| lillux::PinnedDirectory::open(path).unwrap().unwrap();

        // The verifier runtime and the guest runtime are resident before their
        // verifiers run. The session runtime is not: only its archive exists.
        let verifier_manifest = install_large_runtime_content(&state, &open(verifier.path()));
        let verifier_hash = manifest_hash(&verifier_manifest);
        let guest_manifest = install_large_runtime_content(&state, &open(guest.path()));
        let guest_hash = manifest_hash(&guest_manifest);
        let session_hash = manifest_hash(&predict_large_manifest(session.path()));
        assert_ne!(session_hash, guest_hash);

        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(4)
            .unwrap();
        let (bundle, _) = candidate_authoring::write_candidate_authoring_bundle(
            repository,
            &root,
            &state.identity,
            &session_hash,
            &verifier_hash,
            false,
            true,
            None,
            None,
        );
        write_guest_runtime_sources(
            &bundle.root,
            &state.identity,
            &guest_hash,
            &verifier_hash,
            &GUEST_RUNTIME_REQUIRED_CLAIMS,
        );
        let archive = RuntimeArchive::from_tree(session.path());
        let archives = archive_root(&root);
        archive.write_into(&archives);
        let verifier_parameters = serde_json::json!({"support_test": "activated_runtime"});
        write_activated_runtime_sources(
            &bundle.root,
            &state.identity,
            &ActivatedRuntimeSources {
                runtime_manifest_hash: &session_hash,
                verifier_runtime_manifest_hash: &verifier_hash,
                archive: &archive,
                verifier_parameters: &verifier_parameters,
                required_claims: CONTENT_RUNTIME_CLAIMS,
            },
        );
        let grant = archive.activation_grant(&archives);
        let _daemon_socket = start_bundle_node(&mut state, &bundle.root, BUNDLE_NAME, Some(&grant));
        let state = Arc::new(state);

        let QualifiedGuestRuntime {
            qualification: guest_qualification,
            product_witness_hash,
        } = qualify_guest_runtime(
            &state,
            &guest_manifest,
            &verifier_hash,
            &GUEST_RUNTIME_REQUIRED_CLAIMS,
        )
        .await;
        assert_eq!(guest_qualification.guest_runtime_manifest_hash, guest_hash);
        assert_eq!(
            guest_qualification.policy_ref,
            runtime_qualification::GUEST_RUNTIME_POLICY_REF
        );
        assert!(guest_qualification.owner_principal.starts_with("fp:"));
        assert!(lillux::valid_hash(&product_witness_hash));
        assert_eq!(
            guest_qualification.required_claims,
            GUEST_RUNTIME_REQUIRED_CLAIMS
        );

        let QualifiedActivatedRuntime {
            qualification: content_qualification,
            activation_receipt_hash,
        } = activate_and_qualify_runtime(
            &state,
            &session_hash,
            &verifier_hash,
            CONTENT_RUNTIME_CLAIMS,
        )
        .await;
        assert_eq!(
            content_qualification.activation_ref,
            runtime_qualification::CONTENT_ACTIVATION_REF
        );
        assert_eq!(
            content_qualification.owner_principal,
            guest_qualification.owner_principal
        );
        assert!(lillux::valid_hash(&content_qualification.coordinate_id));
        assert!(lillux::valid_hash(&activation_receipt_hash));
        assert_eq!(
            content_qualification.required_claims,
            CONTENT_RUNTIME_CLAIMS
        );
        assert_eq!(logical_entry_count(session.path()), 10);
        assert_ne!(
            content_qualification.attestation_hash,
            guest_qualification.attestation_hash
        );
        assert_eq!(Arc::strong_count(&state), 1);
    });
}
