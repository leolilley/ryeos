//! Opt-in native probe: runs the guest role's execution against the owner
//! product installed at the fixed runtime root of this host.
//! `RYEOS_GUEST_WIRE_NATIVE=1` plus `RYEOS_GUEST_WIRE_SUPERVISOR` and
//! `RYEOS_GUEST_WIRE_LAUNCHER` (paths to the real executables) on a Linux host
//! (a harness-free test: the mount namespace entry requires a single thread)
//! running as the administrator that has the owner product installed at the
//! guest runtime root. Skipped otherwise.

#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::path::Path;

use ryeos_guest_wire_verifier::evidence::{Expected, GuestWireEvidence, SubjectEvidence};
use ryeos_guest_wire_verifier::guest::{ProbeRequest, RUNTIME_ROOT, execute};

fn main() {
    if std::env::var_os("RYEOS_GUEST_WIRE_NATIVE").is_none() {
        eprintln!("skipped: RYEOS_GUEST_WIRE_NATIVE is not set");
        return;
    }
    let supervisor_path = std::env::var("RYEOS_GUEST_WIRE_SUPERVISOR").unwrap();
    let launcher_path = std::env::var("RYEOS_GUEST_WIRE_LAUNCHER").unwrap();
    let supervisor = std::fs::read(&supervisor_path).unwrap();
    let launcher = std::fs::read(&launcher_path).unwrap();
    let root = lillux::PinnedDirectory::open(Path::new(RUNTIME_ROOT))
        .unwrap()
        .unwrap();
    let identity =
        ryeos_external_execution::guest_import_authorization::ObservedGuestRuntime::observe(&root)
            .unwrap()
            .measure_exact_owner_product()
            .unwrap();
    let subject = SubjectEvidence {
        owner_executable_sha256: identity.owner_executable_sha256,
        manifest_hash: identity.manifest_hash,
        controller_public_root: identity.controller_public_root,
    };
    let member_digests = BTreeMap::from([
        (
            ryeos_guest_wire_verifier::guest::SUPERVISOR_MEMBER.to_owned(),
            lillux::sha256_hex(&supervisor),
        ),
        (
            ryeos_guest_wire_verifier::guest::LAUNCHER_MEMBER.to_owned(),
            lillux::sha256_hex(&launcher),
        ),
    ]);
    let challenge_digest = lillux::sha256_hex(b"native-probe-challenge");
    let bytes = execute(&ProbeRequest {
        challenge_digest: challenge_digest.clone(),
        expected_subject: subject.clone(),
        member_digests: member_digests.clone(),
        supervisor,
        launcher,
    })
    .unwrap();
    let expected = Expected {
        challenge_digest,
        subject,
        member_digests,
        guest_inputs_schema:
            ryeos_external_execution_contract::EXTERNAL_GUEST_INPUT_PROJECTION_SCHEMA,
        launcher_spec_schema:
            ryeos_external_execution_contract::EXTERNAL_CANDIDATE_LAUNCHER_SPEC_SCHEMA,
    };
    GuestWireEvidence::parse_and_verify(&bytes, &expected).unwrap();
}
