//! Local artifact acceptance only: no provider occurrence or qualification is issued.
//! The same compiled CLI measures two products at its fixed runtime root.

#![cfg(target_os = "linux")]

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use lillux::crypto::SigningKey;
use ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile;
use ryeos_external_execution::guest_runtime_product::produce_guest_owner_runtime;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    MAX_RESTORED_OWNER_RESULT_BYTES, RESTORED_OWNER_MEASUREMENT_PROTOCOL, RestoredOwnerChallenge,
    RestoredOwnerMeasurement,
};
use ryeos_external_execution_contract::runtime_snapshot::RuntimeSnapshotIntent;

fn invoke(checker: &Path, product: &Path, challenge: &str, extra: bool) -> Output {
    // Mount only the verifier, its system loader/libraries and the test product.
    // No network request is made and no network-containment claim is tested.
    let mut command = Command::new("/usr/bin/timeout");
    command.args([
        "--signal=KILL",
        "20",
        "/usr/bin/bwrap",
        "--unshare-user",
        "--unshare-pid",
        "--die-with-parent",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--dir",
        "/ryeos",
        "--ro-bind",
    ]);
    command.arg(product).arg("/ryeos/guest-runtime");
    command.args(["--ro-bind"]).arg(checker).arg("/checker");
    command.args([
        "--proc",
        "/proc",
        "--",
        "/checker",
        "--challenge-b64",
        challenge,
    ]);
    if extra {
        command.arg("unexpected");
    }
    command.env_clear().env("LANG", "C").output().unwrap()
}

#[test]
#[ignore = "local artifact acceptance requires Linux user/mount/PID namespaces, bwrap and GNU timeout"]
fn same_checker_cli_measures_two_products_and_refuses_substitution() {
    let checker = Path::new(env!(
        "CARGO_BIN_EXE_ryeos-external-guest-restoration-verifier"
    ));
    let checker_hash = lillux::sha256_hex(&std::fs::read(checker).unwrap());
    let directory = tempfile::tempdir().unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let parent = lillux::PinnedDirectory::open(directory.path())
        .unwrap()
        .unwrap();
    let key = SigningKey::from_bytes(&[43; 32]).verifying_key();
    let controller_root = format!(
        "ed25519:{}",
        base64::engine::general_purpose::STANDARD.encode(key.to_bytes())
    );
    let mut observed = Vec::new();
    let mut expectations = Vec::new();
    for (index, (name, owner_bytes, timeout)) in [
        ("first", b"first-owner-cli-fixture".as_slice(), 600),
        ("second", b"second-owner-cli-fixture".as_slice(), 300),
    ]
    .into_iter()
    .enumerate()
    {
        let source_name = format!("{name}-owner");
        let source_path = directory.path().join(&source_name);
        std::fs::write(&source_path, owner_bytes).unwrap();
        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let source = parent
            .open_pinned_regular(OsStr::new(&source_name), false)
            .unwrap()
            .unwrap();
        let owner_hash = lillux::sha256_hex(owner_bytes);
        let product = produce_guest_owner_runtime(
            &parent,
            OsStr::new(name),
            &source.inherited_descriptor_authority().unwrap(),
            owner_bytes.len() as u64,
            &owner_hash,
            &key,
            &GuestOwnerRuntimeProfile {
                schema: 1,
                private_source_max_bytes: 32 * 1024 * 1024,
                private_source_max_inodes: 1024,
                owner_timeout_seconds: timeout,
            },
        )
        .unwrap();
        // These expectations precede measurement and come from the producer's
        // independently retained manifest and executable input.
        let mut intent = RuntimeSnapshotIntent {
            schema: 1,
            operation_id: String::new(),
            owner_principal: format!("fp:{}", "1".repeat(64)),
            provider_id: "fixture".into(),
            source_occurrence_id: format!("source-{index}"),
            provider_group_id: "fixture-group".into(),
            production_profile_digest: "2".repeat(64),
            adapter_artifact_hash: "3".repeat(64),
            provider_spec_digest: "4".repeat(64),
            settings_digest: "5".repeat(64),
            product_witness_hash: "6".repeat(64),
            guest_runtime_manifest_hash: product.manifest_hash().into(),
            owner_executable_sha256: owner_hash,
            controller_public_root: controller_root.clone(),
            upload_sha256: "7".repeat(64),
            upload_bytes: 1024,
            attempt_deadline_ms: 42,
        };
        intent.operation_id = intent.derived_operation_id().unwrap();
        let challenge = RestoredOwnerChallenge {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: intent.operation_id.clone(),
            snapshot_id: format!("snapshot-{index}"),
            restored_occurrence_id: format!("restored-{index}"),
            nonce_hex: "8".repeat(64),
        };
        let encoded = URL_SAFE_NO_PAD
            .encode(ryeos_external_execution_contract::canonical_json(&challenge).unwrap());
        let output = invoke(checker, &directory.path().join(name), &encoded, false);
        assert!(
            output.status.success(),
            "CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        assert!(output.stdout.len() <= MAX_RESTORED_OWNER_RESULT_BYTES + 1);
        let bytes = output
            .stdout
            .strip_suffix(b"\n")
            .expect("one output newline");
        let measurement: RestoredOwnerMeasurement = serde_json::from_slice(bytes).unwrap();
        assert_eq!(
            ryeos_external_execution_contract::canonical_json(&measurement).unwrap(),
            bytes
        );
        measurement.validate_for(&challenge, &intent).unwrap();
        assert_eq!(
            lillux::sha256_hex(&std::fs::read(checker).unwrap()),
            checker_hash
        );
        eprintln!(
            "{}",
            serde_json::json!({
                "stage":"local_checker_cli_measurement", "checker_sha256":checker_hash,
                "challenge_digest":challenge.digest().unwrap(),
                "manifest_hash":intent.guest_runtime_manifest_hash,
                "measurement_sha256":lillux::sha256_hex(bytes), "qualification_claims_issued":false
            })
        );
        let rejected = invoke(checker, &directory.path().join(name), &encoded, true);
        assert!(!rejected.status.success());
        assert!(rejected.stdout.is_empty());
        observed.push(measurement);
        expectations.push((challenge, intent));
    }
    assert_ne!(observed[0].manifest_hash, observed[1].manifest_hash);
    for index in 0..2 {
        let (challenge, intent) = &expectations[index];
        assert!(observed[1 - index].validate_for(challenge, intent).is_err());
        // Hold the valid challenge constant to isolate the product mismatch.
        let mut wrong_product = observed[index].clone();
        wrong_product.manifest_hash = observed[1 - index].manifest_hash.clone();
        wrong_product.owner_executable_sha256 = observed[1 - index].owner_executable_sha256.clone();
        assert!(wrong_product.validate_for(challenge, intent).is_err());
    }
}
