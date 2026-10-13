//! Independently measure the exact owner-product bytes in a restored guest.
//!
//! This executable is delivered separately from the provider snapshot. Its
//! output names only a controller challenge and local content. The controller
//! must authenticate the provider run and bind it to its exact restored
//! occurrence, snapshot locator, admitted verifier artifact and product intent.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime;
use ryeos_external_execution_contract::restored_runtime_measurement::{
    MAX_RESTORED_OWNER_CHALLENGE_BYTES, RESTORED_OWNER_MEASUREMENT_PROTOCOL,
    RestoredOwnerChallenge, RestoredOwnerMeasurement,
};

const RUNTIME_ROOT: &str = "/ryeos/guest-runtime";

fn parse_challenge(args: impl IntoIterator<Item = OsString>) -> Result<RestoredOwnerChallenge> {
    let mut args = args.into_iter();
    ensure!(
        args.next().as_deref() == Some(OsStr::new("--challenge-b64")),
        "restoration verifier requires only --challenge-b64"
    );
    let encoded = args.next().context("restoration challenge is absent")?;
    ensure!(
        args.next().is_none(),
        "restoration verifier received extra arguments"
    );
    let encoded = encoded
        .to_str()
        .context("restoration challenge is not UTF-8")?;
    ensure!(
        !encoded.is_empty()
            && encoded.len() <= (MAX_RESTORED_OWNER_CHALLENGE_BYTES * 4).div_ceil(3),
        "restoration challenge exceeds argument bound"
    );
    let bytes = URL_SAFE_NO_PAD.decode(encoded)?;
    ensure!(
        !bytes.is_empty()
            && bytes.len() <= MAX_RESTORED_OWNER_CHALLENGE_BYTES
            && URL_SAFE_NO_PAD.encode(&bytes) == encoded,
        "restoration challenge is not canonical base64url"
    );
    let challenge: RestoredOwnerChallenge =
        ryeos_external_execution_contract::from_json_slice_strict(
            &bytes,
            MAX_RESTORED_OWNER_CHALLENGE_BYTES,
        )?;
    challenge.validate()?;
    ensure!(
        ryeos_external_execution_contract::canonical_json(&challenge)? == bytes,
        "restoration challenge is noncanonical"
    );
    Ok(challenge)
}

fn measure(challenge: &RestoredOwnerChallenge, root: &lillux::PinnedDirectory) -> Result<Vec<u8>> {
    let runtime = ObservedGuestRuntime::observe(root)?;
    let identity = runtime.measure_exact_owner_product()?;
    let result = RestoredOwnerMeasurement {
        schema: 1,
        protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
        challenge_digest: challenge.digest()?,
        manifest_hash: identity.manifest_hash,
        owner_executable_sha256: identity.owner_executable_sha256,
        controller_public_root: identity.controller_public_root,
    };
    let bytes = ryeos_external_execution_contract::canonical_json(&result)?;
    ensure!(
        bytes.len()
            <= ryeos_external_execution_contract::restored_runtime_measurement::MAX_RESTORED_OWNER_RESULT_BYTES,
        "restoration measurement exceeds output bound"
    );
    Ok(bytes)
}

fn run() -> Result<()> {
    let challenge = parse_challenge(std::env::args_os().skip(1))?;
    let root = lillux::PinnedDirectory::open(Path::new(RUNTIME_ROOT))?
        .context("restored guest has no owner-product root")?;
    let bytes = measure(&challenge, &root)?;
    // The provider run transport owns the bounded stdout capture. The guest
    // never prints a credential, provider token, or self-reported snapshot ID.
    println!("{}", String::from_utf8(bytes)?);
    Ok(())
}

fn main() {
    if run().is_err() {
        eprintln!("restoration verifier failed closed");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use lillux::crypto::SigningKey;
    use ryeos_external_execution::guest_import_authorization::GuestOwnerRuntimeProfile;
    use ryeos_external_execution::guest_runtime_product::produce_guest_owner_runtime;
    use ryeos_external_execution_contract::runtime_snapshot::{
        RUNTIME_SNAPSHOT_INTENT_SCHEMA, RuntimeSnapshotIntent, RuntimeSnapshotSource,
    };

    #[test]
    fn same_measurement_code_accepts_distinct_products_and_rejects_substitution() {
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
        let mut products = Vec::new();
        for (name, owner_bytes, timeout) in [
            ("first", b"first-owner-fixture".as_slice(), 600),
            ("second", b"second-owner-fixture".as_slice(), 300),
        ] {
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
                    schema: 2,
                    account: lillux::GuestRuntimeAccount::Unix {
                        uid: 65534,
                        gid: 65534,
                    },
                    private_source_max_bytes: 32 * 1024 * 1024,
                    private_source_max_inodes: 1024,
                    owner_timeout_seconds: timeout,
                },
            )
            .unwrap();
            products.push((product, owner_hash));
        }
        assert_ne!(products[0].0.manifest_hash(), products[1].0.manifest_hash());

        for (index, (product, owner_hash)) in products.iter().enumerate() {
            // Expectations are retained production inputs, never copied from
            // the measurement that is about to be checked.
            let mut intent = RuntimeSnapshotIntent {
                schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
                operation_id: String::new(),
                owner_principal: format!("fp:{}", "1".repeat(64)),
                provider_id: "fixture".into(),
                source_occurrence_id: format!("source-{index}"),
                source_bootstrap_operation_id: None,
                source_created_at: None,
                source_timeout_seconds: None,
                provider_group_id: "fixture-group".into(),
                production_profile_digest: "2".repeat(64),
                adapter_artifact_hash: "3".repeat(64),
                provider_spec_digest: "4".repeat(64),
                settings_digest: "5".repeat(64),
                source: RuntimeSnapshotSource::CapturedProduct {
                    product_witness_hash: "6".repeat(64),
                },
                guest_runtime_manifest_hash: product.manifest_hash().into(),
                owner_executable_sha256: owner_hash.clone(),
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
            let bytes = measure(&challenge, product.root()).unwrap();
            let measured: RestoredOwnerMeasurement = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                ryeos_external_execution_contract::canonical_json(&measured).unwrap(),
                bytes
            );
            measured.validate_for(&challenge, &intent).unwrap();

            // A valid challenge does not make another valid product acceptable.
            let substituted = measure(&challenge, products[1 - index].0.root()).unwrap();
            let substituted: RestoredOwnerMeasurement =
                serde_json::from_slice(&substituted).unwrap();
            assert!(substituted.validate_for(&challenge, &intent).is_err());
        }
    }

    #[test]
    fn challenge_argument_is_canonical_and_exactly_one() {
        let challenge = RestoredOwnerChallenge {
            schema: 1,
            protocol: RESTORED_OWNER_MEASUREMENT_PROTOCOL.into(),
            operation_id: "1".repeat(64),
            snapshot_id: "snp-exact".into(),
            restored_occurrence_id: "sbx-exact".into(),
            nonce_hex: "2".repeat(64),
        };
        let encoded = URL_SAFE_NO_PAD
            .encode(ryeos_external_execution_contract::canonical_json(&challenge).unwrap());
        assert_eq!(
            parse_challenge(["--challenge-b64".into(), encoded.clone().into()]).unwrap(),
            challenge
        );
        assert!(parse_challenge(["--challenge-b64".into(), format!("{encoded}=").into()]).is_err());
        assert!(
            parse_challenge(["--challenge-b64".into(), encoded.into(), "extra".into()]).is_err()
        );
    }
}
