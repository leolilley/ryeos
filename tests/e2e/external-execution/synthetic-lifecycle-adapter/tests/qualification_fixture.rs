//! Drives the reusable runtime qualification fixtures against a live test node
//! and checks that the attestations they publish are real and current, and
//! that an activated-content qualification made on one node is usable, as a
//! remote testimony, on another node that activated the same signed release
//! and on no node whose activated content differs.
#![recursion_limit = "256"]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

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
use qualification_node::{
    bind_manifests_to_tool, manifest_hash, operator_context, predict_large_manifest,
    start_bundle_node,
};
use runtime_qualification::{
    ActivatedRuntimeSources, CONTENT_ACTIVATION_REF, CONTENT_POLICY_REF, CONTENT_RUNTIME_CLAIMS,
    CONTENT_VERIFIER_REF, GUEST_RUNTIME_REQUIRED_CLAIMS, RuntimeArchive, activate_runtime,
    archive_root, write_activated_runtime_consumer, write_activated_runtime_sources,
};
use runtime_qualification_fixture::{
    QualifiedActivatedRuntime, QualifiedGuestRuntime, activate_and_qualify_runtime,
    logical_entry_count, qualify_guest_runtime, write_guest_runtime_sources,
};
use ryeos_app::operator_external_content::candidate_program::{
    CandidateProgramRequest, CandidateRemoteTestimony, RemoteContentTestimonyUse,
    check_remote_testimony,
};
use ryeos_app::operator_external_content::qualification_testimony::{
    ContentExportSelection, RemoteContentTestimonyRefusal, accept_testimony_for_owner,
    export_content_testimony_blocking,
};
use ryeos_state::external_content::qualification_subject::ContentQualificationSubject;

const BUNDLE_NAME: &str = "external-candidate-authoring";

#[test]
fn guest_and_activated_runtime_qualifications_publish_real_current_attestations() {
    run_on_test_node(None, |root, mut state| async move {
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
        let _daemon_socket =
            start_bundle_node(&mut state, &bundle.root, BUNDLE_NAME, Some(&grant), None);
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

// ---------------------------------------------------------------------------
// Portable content-lane testimony across three nodes
// ---------------------------------------------------------------------------

const ACCEPTANCE_POLICY_REF: &str = "config:fixtures/runtime-content-acceptance";
const POLICY_SOURCE_PATH: &str = ".ai/config/fixtures/runtime-content-qualification.yaml";

/// What every node of the portable-testimony scenario shares: one publisher
/// signs one Bundle release, and every node trusts that publisher.
struct Release {
    template: PathBuf,
    qualified_tree: PathBuf,
    other_tree: PathBuf,
    verifier_tree: PathBuf,
    verifier_hash: String,
    publisher: ryeos_node_runtime::identity::NodeIdentity,
    signer: ryeos_engine::trust::TrustedSigner,
}

/// What the origin hands to a receiver: its operator's signed testimony and
/// the public facts a receiver's operator configures trust from.
struct Origin {
    testimony: ryeos_state::objects::Attestation,
    node_key: lillux::crypto::VerifyingKey,
    operator_principal: String,
    operator_key: String,
    site_id: String,
    coordinate_id: String,
    attestation_hash: String,
    runtime_manifest_hash: String,
    subject: ContentQualificationSubject,
    verifier_definition_digest: String,
    verifier_artifact_digest: String,
}

/// What one receiver observed.
struct ReceiverRun {
    node_fingerprint: String,
    receipt_hash: String,
    before_activation: RemoteContentTestimonyRefusal,
    outcome: Result<RemoteContentTestimonyUse, RemoteContentTestimonyRefusal>,
    doctor: Vec<&'static str>,
    local_content_qualifications: usize,
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_tree(&entry.path(), &target);
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path()).unwrap(), &target).unwrap();
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn runtime_binaries() -> RuntimeBinaries<'static> {
    RuntimeBinaries {
        candidate: Path::new(env!(
            "CARGO_BIN_EXE_ryeos-synthetic-external-candidate-runtime"
        )),
        provider: Path::new(env!(
            "CARGO_BIN_EXE_ryeos-synthetic-external-provider-runtime"
        )),
    }
}

fn open(path: &Path) -> lillux::PinnedDirectory {
    lillux::PinnedDirectory::open(path).unwrap().unwrap()
}

/// The receiver's signed acceptance policy, naming the origin's operator and
/// site and the verifier identity the origin ran.
fn write_acceptance_policy(bundle_root: &Path, release: &Release, origin: &Origin) {
    let claims = CONTENT_RUNTIME_CLAIMS
        .iter()
        .map(|claim| (*claim).to_owned())
        .collect::<Vec<_>>();
    let mut allowed = origin
        .subject_claims()
        .into_iter()
        .chain(claims.iter().cloned())
        .collect::<Vec<_>>();
    allowed.sort();
    allowed.dedup();
    let document = serde_json::json!({
        "category": "fixtures",
        "version": "1.0.0",
        "description": "Accept the origin operator's activated runtime content testimony",
        "remote_qualification_acceptance_policy": {
            "schema": ryeos_state::external_content::products::qualification_testimony::REMOTE_QUALIFICATION_ACCEPTANCE_POLICY_SCHEMA,
            "qualification_policy_ref": CONTENT_POLICY_REF,
            "trusted_attesters": [{
                "principal": origin.operator_principal,
                "origin_site_id": origin.site_id,
            }],
            "required_claims": claims,
            "allowed_claims": allowed,
            "accepted_verifier_definition_digests": [origin.verifier_definition_digest],
            "accepted_verifier_artifact_digests": [origin.verifier_artifact_digest],
            "allowed_lanes": ["content"],
            "max_testimony_age_seconds": 86400,
            "max_clock_skew_seconds": 300,
        },
    });
    let path = bundle_root.join(".ai/config/fixtures/runtime-content-acceptance.yaml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        lillux::signature::sign_content(
            &serde_yaml::to_string(&document).unwrap(),
            release.publisher.signing_key(),
            "#",
            None,
        ),
    )
    .unwrap();
}

impl Origin {
    fn subject_claims(&self) -> Vec<String> {
        let body = ryeos_state::external_content::products::qualification_testimony::RemoteQualificationTestimonyBody::from_value(&self.testimony.evidence)
            .unwrap();
        body.asserted_claims
    }

    fn request(&self, acceptance_hash: &str) -> CandidateProgramRequest {
        CandidateProgramRequest {
            activation_ref: CONTENT_ACTIVATION_REF.to_owned(),
            coordinate_id: self.coordinate_id.clone(),
            attestation_hash: self.attestation_hash.clone(),
            runtime_manifest_hash: self.runtime_manifest_hash.clone(),
            required_claims: CONTENT_RUNTIME_CLAIMS
                .iter()
                .map(|claim| (*claim).to_owned())
                .collect(),
            remote_testimony: Some(CandidateRemoteTestimony {
                acceptance_hash: acceptance_hash.to_owned(),
                acceptance_policy_ref: ACCEPTANCE_POLICY_REF.to_owned(),
                owner_principal: self.operator_principal.clone(),
            }),
        }
    }
}

/// Node A: activate the release, qualify it with a real verifier run, and
/// export the operator-signed testimony.
fn run_origin(release: Arc<Release>, session_tree: PathBuf) -> Origin {
    let result = Arc::new(Mutex::new(None));
    let out = Arc::clone(&result);
    run_on_test_node(
        Some(release.signer.clone()),
        move |root, mut state| async move {
            let bundle_root = root.join(BUNDLE_NAME);
            copy_tree(&release.template, &bundle_root);
            let runtime_manifest_hash = manifest_hash(&predict_large_manifest(&session_tree));
            let archive = RuntimeArchive::from_tree(&session_tree);
            let archives = archive_root(&root);
            archive.write_into(&archives);
            let grant = archive.activation_grant(&archives);
            let _daemon_socket = start_bundle_node(
                &mut state,
                &bundle_root,
                BUNDLE_NAME,
                Some(&grant),
                Some(&release.signer),
            );
            let verifier = install_large_runtime_content(&state, &open(&release.verifier_tree));
            assert_eq!(manifest_hash(&verifier), release.verifier_hash);
            let state = Arc::new(state);
            let QualifiedActivatedRuntime {
                qualification,
                activation_receipt_hash,
            } = activate_and_qualify_runtime(
                &state,
                &runtime_manifest_hash,
                &release.verifier_hash,
                CONTENT_RUNTIME_CLAIMS,
            )
            .await;
            let operator = ryeos_node_runtime::identity::NodeIdentity::load(
                &state.config.operator_signing_key_path,
            )
            .unwrap();
            let export = {
                let state = Arc::clone(&state);
                let operator = operator.clone();
                let selection = ContentExportSelection {
                    activation_ref: CONTENT_ACTIVATION_REF.to_owned(),
                    coordinate_id: qualification.coordinate_id.clone(),
                    manifest_hash: runtime_manifest_hash.clone(),
                };
                let attestation_hash = qualification.attestation_hash.clone();
                tokio::task::spawn_blocking(move || {
                    export_content_testimony_blocking(
                        &state,
                        &operator_context(&state),
                        &operator,
                        &selection,
                        &attestation_hash,
                        ACCEPTANCE_POLICY_REF,
                        &lillux::time::iso8601_now(),
                    )
                })
                .await
                .unwrap()
                .unwrap()
            };
            assert_eq!(export.coordinate_id, qualification.coordinate_id);
            let body = ryeos_state::external_content::products::qualification_testimony::RemoteQualificationTestimonyBody::from_value(&export.testimony.evidence)
            .unwrap();
            let evidence =
            ryeos_state::external_content::qualification_evidence::ContentQualificationEvidence::from_attestation(
                &body.qualification_attestation,
            )
            .unwrap();
            let ryeos_state::external_content::qualification_purpose::QualificationSubject::ActivatedContent {
            content,
        } = &evidence.purpose.subject
        else {
            panic!("the exported content testimony has no activated subject");
        };
            // The origin's subject names the origin's own records.
            assert_eq!(content.activation_receipt_hash, activation_receipt_hash);
            assert_eq!(
                content.target_node_fingerprint,
                state.identity.fingerprint()
            );
            assert_eq!(content.manifest_hash, runtime_manifest_hash);
            let verifier_artifact_digest = ryeos_state::objects::canonical_value_digest(
                &serde_json::to_value(&evidence.verifier.artifact_identity).unwrap(),
            )
            .unwrap();
            *out.lock().unwrap() = Some(Origin {
                testimony: export.testimony,
                node_key: *state.identity.verifying_key(),
                operator_principal: operator.principal_id(),
                operator_key: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    operator.verifying_key().as_bytes(),
                ),
                site_id: state.threads.site_id().to_owned(),
                coordinate_id: qualification.coordinate_id,
                attestation_hash: qualification.attestation_hash,
                runtime_manifest_hash,
                subject: content.clone(),
                verifier_definition_digest: evidence.verifier.effective_definition_digest.clone(),
                verifier_artifact_digest,
            });
        },
    );
    let origin = result.lock().unwrap().take();
    origin.expect("the origin node exported no testimony")
}

/// A receiver: trust the origin's operator and node, accept the testimony,
/// then use it before and after activating its own copy of the content.
/// `prepare` adjusts the receiver's copy of the signed release.
fn run_receiver(
    release: Arc<Release>,
    origin: Arc<Origin>,
    session_tree: PathBuf,
    prepare: fn(&Release, &Path),
) -> ReceiverRun {
    let result = Arc::new(Mutex::new(None));
    let out = Arc::clone(&result);
    run_on_test_node(
        Some(release.signer.clone()),
        move |root, mut state| async move {
            let bundle_root = root.join(BUNDLE_NAME);
            copy_tree(&release.template, &bundle_root);
            prepare(&release, &bundle_root);
            write_acceptance_policy(&bundle_root, &release, &origin);
            let runtime_manifest_hash = manifest_hash(&predict_large_manifest(&session_tree));
            let archive = RuntimeArchive::from_tree(&session_tree);
            let archives = archive_root(&root);
            archive.write_into(&archives);
            let grant = archive.activation_grant(&archives);
            let _daemon_socket = start_bundle_node(
                &mut state,
                &bundle_root,
                BUNDLE_NAME,
                Some(&grant),
                Some(&release.signer),
            );
            let verifier = install_large_runtime_content(&state, &open(&release.verifier_tree));
            assert_eq!(manifest_hash(&verifier), release.verifier_hash);
            // The receiver's operator trusts the origin's operator as a remote
            // operator of its site, and the origin's node as a remote node of it.
            let now = lillux::time::iso8601_now();
            let operator_fingerprint = origin.operator_principal.strip_prefix("fp:").unwrap();
            ryeos_node_runtime::identity::reconcile_authorized_key_toml_scopes(
                &state.config.authorized_keys_dir,
                operator_fingerprint,
                &origin.operator_key,
                &[
                    "ryeos.execute.service.external-content/accept-qualification-testimony"
                        .to_owned(),
                    "ryeos.execute.service.external-execution/admit-candidate-program".to_owned(),
                ],
                "portable content testimony origin operator",
                state.identity.fingerprint(),
                &now,
                &ryeos_node_runtime::identity::GrantTerms::for_tests(),
                &state.identity,
                ryeos_node_runtime::identity::WildcardPolicy::Reject,
                false,
                Some(origin.site_id.as_str()),
                false,
            )
            .unwrap();
            ryeos_node_runtime::identity::write_authorized_remote_node_key_toml(
                &state.config.authorized_keys_dir,
                &lillux::crypto::fingerprint(&origin.node_key),
                &base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    origin.node_key.as_bytes(),
                ),
                &[
                    "ryeos.execute.service.external-content/accept-qualification-testimony"
                        .to_owned(),
                ],
                "portable content testimony origin node",
                state.identity.fingerprint(),
                &now,
                &ryeos_node_runtime::identity::GrantTerms::for_tests(),
                &origin.site_id,
                state.identity.signing_key(),
            )
            .unwrap();
            let state = Arc::new(state);

            // Acceptance needs no local activation.
            let acceptance = {
                let state = Arc::clone(&state);
                let origin = Arc::clone(&origin);
                tokio::task::spawn_blocking(move || {
                    accept_testimony_for_owner(
                        &state,
                        &origin.operator_principal,
                        &origin.node_key,
                        origin.testimony.clone(),
                        &lillux::time::iso8601_now(),
                    )
                })
                .await
                .unwrap()
                .unwrap()
            };
            assert_eq!(acceptance.coordinate_id, origin.coordinate_id);
            assert!(!acceptance.reused_existing);
            let request = origin.request(&acceptance.acceptance_hash);
            let check = |request: CandidateProgramRequest| {
                let state = Arc::clone(&state);
                let origin = Arc::clone(&origin);
                tokio::task::spawn_blocking(move || {
                    let attester =
                        ryeos_app::operator_authority::admitted_operator_authority_for_principal(
                            &state,
                            &origin.operator_principal,
                        )?;
                    check_remote_testimony(&state, &attester.handler_context(), &request)
                })
            };
            let typed = |error: anyhow::Error| {
                error
                    .chain()
                    .find_map(|cause| cause.downcast_ref::<RemoteContentTestimonyRefusal>())
                    .cloned()
                    .unwrap_or_else(|| panic!("expected a typed refusal, got {error:#}"))
            };
            let before_activation = typed(check(request.clone()).await.unwrap().unwrap_err());

            let receipt_hash = activate_runtime(&state).await;
            bind_manifests_to_tool(
                &state,
                CONTENT_VERIFIER_REF,
                &[
                    runtime_manifest_hash.as_str(),
                    release.verifier_hash.as_str(),
                ],
            );
            let outcome = check(request).await.unwrap().map_err(typed);
            let doctor = ryeos_app::operator_external_content::remote_testimony_inspection::inspect_remote_content_testimonies(
            &state.config.app_root,
        )
        .unwrap()
        .findings
        .into_iter()
        .map(|finding| finding.code)
        .collect();
            let local_content_qualifications = state
            .state_store
            .with_state_db(|db| {
                db.list_generic_head_refs(
                    ryeos_state::external_content::qualification_publication::CONTENT_QUALIFICATION_HEAD_NAMESPACE,
                )
            })
            .unwrap()
            .len();
            *out.lock().unwrap() = Some(ReceiverRun {
                node_fingerprint: state.identity.fingerprint().to_owned(),
                receipt_hash,
                before_activation,
                outcome,
                doctor,
                local_content_qualifications,
            });
        },
    );
    let run = result.lock().unwrap().take();
    run.expect("the receiver node recorded no outcome")
}

/// The receiver installs the origin's release unchanged.
fn same_release(_: &Release, _: &Path) {}

/// The receiver's release pins a runtime archive whose tree differs from the
/// qualified one by one byte, under the same signed policy.
fn one_byte_other_release(release: &Release, bundle_root: &Path) {
    let tree = &release.other_tree;
    let archive = RuntimeArchive::from_tree(tree);
    let parameters = portable_testimony_parameters();
    write_activated_runtime_sources(
        bundle_root,
        &release.publisher,
        &ActivatedRuntimeSources {
            runtime_manifest_hash: &manifest_hash(&predict_large_manifest(tree)),
            verifier_runtime_manifest_hash: &release.verifier_hash,
            archive: &archive,
            verifier_parameters: &parameters,
            required_claims: CONTENT_RUNTIME_CLAIMS,
        },
    );
    // The same signed policy file as the origin's, byte for byte.
    std::fs::copy(
        release.template.join(POLICY_SOURCE_PATH),
        bundle_root.join(POLICY_SOURCE_PATH),
    )
    .unwrap();
}

/// The receiver's release mounts the same archive elsewhere.
fn other_mount_release(release: &Release, bundle_root: &Path) {
    write_activated_runtime_consumer(
        bundle_root,
        &release.publisher,
        &manifest_hash(&predict_large_manifest(&release.qualified_tree)),
        CONTENT_RUNTIME_CLAIMS,
        "runtime-alt",
    );
}

fn portable_testimony_parameters() -> serde_json::Value {
    serde_json::json!({"support_test": "portable_content_testimony"})
}

#[test]
fn a_content_testimony_is_usable_exactly_where_the_same_release_is_activated() {
    let binaries = runtime_binaries();
    let workspace = tempfile::tempdir().unwrap();
    // The qualified runtime, a runtime one byte away from it, and the
    // verifier's own runtime.
    let qualified = workspace.path().join("qualified-runtime");
    let one_byte_other = workspace.path().join("one-byte-other-runtime");
    let verifier_tree = workspace.path().join("verifier-runtime");
    for (tree, marker) in [
        (&qualified, Some("runtime-a")),
        (&one_byte_other, Some("runtime-c")),
        (&verifier_tree, None),
    ] {
        std::fs::create_dir_all(tree).unwrap();
        open(tree).tighten_owner_private_directory().unwrap();
        install_runtime_files(tree, &binaries, marker);
    }
    let qualified_hash = manifest_hash(&predict_large_manifest(&qualified));
    let other_hash = manifest_hash(&predict_large_manifest(&one_byte_other));
    let verifier_hash = manifest_hash(&predict_large_manifest(&verifier_tree));
    assert_ne!(qualified_hash, other_hash);

    // One publisher signs one release; every node trusts it.
    let publisher = ryeos_node_runtime::identity::NodeIdentity::create(
        &workspace.path().join("publisher/publisher-key.pem"),
    )
    .unwrap();
    let signer = ryeos_engine::trust::TrustedSigner {
        fingerprint: publisher.fingerprint().to_owned(),
        verifying_key: *publisher.verifying_key(),
        label: Some("portable content testimony release publisher".into()),
    };
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(4)
        .unwrap();
    let (template, _) = candidate_authoring::write_candidate_authoring_bundle(
        repository,
        workspace.path(),
        &publisher,
        &qualified_hash,
        &verifier_hash,
        false,
        true,
        None,
        None,
    );
    let parameters = portable_testimony_parameters();
    write_activated_runtime_sources(
        &template.root,
        &publisher,
        &ActivatedRuntimeSources {
            runtime_manifest_hash: &qualified_hash,
            verifier_runtime_manifest_hash: &verifier_hash,
            archive: &RuntimeArchive::from_tree(&qualified),
            verifier_parameters: &parameters,
            required_claims: CONTENT_RUNTIME_CLAIMS,
        },
    );
    let release = Arc::new(Release {
        template: template.root.clone(),
        qualified_tree: qualified.clone(),
        other_tree: one_byte_other.clone(),
        verifier_tree,
        verifier_hash,
        publisher,
        signer,
    });

    // Node A activates, qualifies with a real verifier run, and exports.
    let origin = Arc::new(run_origin(Arc::clone(&release), qualified.clone()));

    // Node B activates the same release: the testimony is usable there.
    let same = run_receiver(
        Arc::clone(&release),
        Arc::clone(&origin),
        qualified.clone(),
        same_release,
    );
    assert_eq!(
        same.before_activation.code(),
        "remote_content_no_local_activation"
    );
    let used = same
        .outcome
        .unwrap_or_else(|refusal| panic!("the same release was refused: {refusal}"));
    assert_eq!(used.origin_subject, origin.subject);
    let receiver = &used.receiver_subject;
    // B's local join is its own ...
    assert_eq!(receiver.activation_receipt_hash, same.receipt_hash);
    assert_eq!(receiver.target_node_fingerprint, same.node_fingerprint);
    assert_ne!(
        receiver.activation_receipt_hash,
        origin.subject.activation_receipt_hash
    );
    assert_ne!(receiver.binding_hash, origin.subject.binding_hash);
    assert_ne!(
        receiver.target_node_fingerprint,
        origin.subject.target_node_fingerprint
    );
    // ... while the content identity is the origin's: the activation program
    // digest is node-independent.
    assert_eq!(receiver.portable(), origin.subject.portable());
    assert_eq!(
        receiver.activation_program_digest,
        origin.subject.activation_program_digest
    );
    assert_eq!(same.doctor, vec!["ok"]);
    assert_eq!(same.local_content_qualifications, 0);

    // Node C: one runtime byte differs, under the same signed policy.
    let other = run_receiver(
        Arc::clone(&release),
        Arc::clone(&origin),
        one_byte_other.clone(),
        one_byte_other_release,
    );
    assert_eq!(
        other.before_activation.code(),
        "remote_content_no_local_activation"
    );
    let refusal = match other.outcome {
        Ok(_) => panic!("content one byte away from the qualified content was admitted"),
        Err(refusal) => refusal,
    };
    assert_eq!(refusal.code(), "remote_content_identity_differs");
    let RemoteContentTestimonyRefusal::IdentityDiffers {
        fields,
        origin_activation_program_digest,
        receiver_activation_program_digest,
        origin_manifest_hash,
        receiver_manifest_hash,
    } = &refusal
    else {
        panic!("expected an identity refusal, got {refusal}");
    };
    assert!(fields.contains(&"activation_program_digest"), "{refusal}");
    assert!(fields.contains(&"manifest_hash"), "{refusal}");
    assert_eq!(
        origin_activation_program_digest,
        &origin.subject.activation_program_digest
    );
    assert_ne!(
        receiver_activation_program_digest,
        origin_activation_program_digest
    );
    assert_eq!(origin_manifest_hash, &qualified_hash);
    assert_eq!(receiver_manifest_hash, &other_hash);
    assert_eq!(other.doctor, vec!["local_activation_differs"]);

    // Node C': the same archive, mounted elsewhere by its consumer.
    let mounted = run_receiver(
        Arc::clone(&release),
        Arc::clone(&origin),
        qualified,
        other_mount_release,
    );
    let refusal = match mounted.outcome {
        Ok(_) => panic!("the qualified content mounted elsewhere was admitted"),
        Err(refusal) => refusal,
    };
    let RemoteContentTestimonyRefusal::IdentityDiffers { fields, .. } = &refusal else {
        panic!("expected an identity refusal, got {refusal}");
    };
    assert!(fields.contains(&"realization"), "{refusal}");
    assert!(fields.contains(&"activation_program_digest"), "{refusal}");
    assert!(!fields.contains(&"manifest_hash"), "{refusal}");
    assert_eq!(mounted.doctor, vec!["local_activation_differs"]);
}
