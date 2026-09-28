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
