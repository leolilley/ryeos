//! Lease operation entry points and the sealed-descriptor helpers every
//! lifecycle operation of a remote lease adapter reads its inputs with.
//!
//! The two lease operations read only their request and the captured signed
//! provider spec, plus, for Ready admission, the guest-frames descriptor. The
//! adapter supplies its id and how to parse its own spec; everything else is
//! shared.

use std::io::Write;

use anyhow::{Context as _, Result, ensure};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_external_execution_contract::qualification_request_binding::QUALIFICATION_REQUEST_BINDING_FD_ENV;
use ryeos_external_execution_contract::relay_file::LIFECYCLE_RELAY_UPLOAD_FD_ENV;
use ryeos_external_execution_contract::{
    LEASE_ADMISSION_FD_ENV, LIFECYCLE_BOOTSTRAP_FD_ENV, LIFECYCLE_CREDENTIAL_FD_ENV,
    LIFECYCLE_GUEST_FRAMES_FD_ENV, LIFECYCLE_GUEST_PACKAGE_FD_ENV, LIFECYCLE_GUEST_RUNTIME_FD_ENV,
    LIFECYCLE_HOSTS_FD_ENV, LIFECYCLE_LAUNCHER_FD_ENV, LIFECYCLE_PROVIDER_SPEC_FD_ENV,
    LIFECYCLE_PROVIDER_SPEC_SHA256_ENV, LIFECYCLE_REMAINING_TIMEOUT_MS_ENV,
    LIFECYCLE_REQUEST_FD_ENV, LIFECYCLE_RESOLVER_FD_ENV, LIFECYCLE_SETTINGS_FD_ENV,
    LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV, LIFECYCLE_SIGNED_IMPORT_FD_ENV,
    LIFECYCLE_SUPERVISOR_FD_ENV, MAX_LIFECYCLE_PROVIDER_SPEC_BYTES, MAX_LIFECYCLE_RESPONSE_BYTES,
    canonical_json, from_json_slice_strict,
};
use serde::Serialize;
use zeroize::Zeroizing;

use crate::facts::LeaseProviderFacts;
use crate::lease::{
    LeaseAdmissionRequest, LeaseReadyRequest, LeaseReadyResponse, MAX_LEASE_REQUEST_BYTES,
    admit_before_spend, await_ready,
};

const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;
/// Bound on one lease Ready request. The request embeds the whole stage-one
/// admission request, which alone may reach the admission bound, beside the
/// retained expectation (channel binding included) and any resume state, so
/// it needs headroom above the admission bound.
pub(crate) const MAX_LEASE_READY_REQUEST_BYTES: usize = 128 * 1024;

// The Ready request embeds a whole admission request, so its bound must
// leave the admission bound headroom.
const _: () = assert!(MAX_LEASE_READY_REQUEST_BYTES >= 2 * MAX_LEASE_REQUEST_BYTES);
const MAX_OPERATION_TIMEOUT_MS: u64 = 60 * 60 * 1000;

/// Every credential, network, settings, activation, relay, lease-admission
/// and qualification descriptor the lifecycle runner can hand an adapter.
/// Workload: the remote lease operations, which read only their request
/// (Ready carries its admitted request inside it) and the captured provider
/// spec, and must refuse every other descriptor by name.
const LEASE_REFUSED_DESCRIPTORS: [&str; 14] = [
    LIFECYCLE_CREDENTIAL_FD_ENV,
    LIFECYCLE_RESOLVER_FD_ENV,
    LIFECYCLE_HOSTS_FD_ENV,
    LIFECYCLE_SETTINGS_FD_ENV,
    LIFECYCLE_BOOTSTRAP_FD_ENV,
    LIFECYCLE_SIGNED_IMPORT_FD_ENV,
    LIFECYCLE_SIGNED_ASSIGNMENT_FD_ENV,
    LIFECYCLE_GUEST_PACKAGE_FD_ENV,
    LIFECYCLE_GUEST_RUNTIME_FD_ENV,
    LIFECYCLE_SUPERVISOR_FD_ENV,
    LIFECYCLE_LAUNCHER_FD_ENV,
    LIFECYCLE_RELAY_UPLOAD_FD_ENV,
    LEASE_ADMISSION_FD_ENV,
    QUALIFICATION_REQUEST_BINDING_FD_ENV,
];

/// The two lease operations. Only Ready admission may hold the guest-frames
/// descriptor, on which the controller relays the guest's first frames; it
/// carries no provider contact authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeaseOperation {
    Admission,
    Ready,
}

impl LeaseOperation {
    fn label(self) -> &'static str {
        match self {
            Self::Admission => "lease admission",
            Self::Ready => "lease Ready",
        }
    }

    fn holds_guest_frames(self) -> bool {
        match self {
            Self::Admission => false,
            Self::Ready => true,
        }
    }
}

fn refuse_lease_contact_authority(operation: LeaseOperation) -> Result<()> {
    refuse_received_descriptors(operation, |name| std::env::var_os(name).is_some())
}

fn refuse_received_descriptors(
    operation: LeaseOperation,
    present: impl Fn(&str) -> bool,
) -> Result<()> {
    let received: Vec<&str> = LEASE_REFUSED_DESCRIPTORS
        .into_iter()
        .filter(|name| present(name))
        .collect();
    ensure!(
        received.is_empty(),
        "{} received descriptors it must never hold: {}",
        operation.label(),
        received.join(", ")
    );
    ensure!(
        operation.holds_guest_frames() || !present(LIFECYCLE_GUEST_FRAMES_FD_ENV),
        "{} received the guest-frames descriptor {LIFECYCLE_GUEST_FRAMES_FD_ENV}, which only \
         lease Ready admission may hold",
        operation.label()
    );
    Ok(())
}

/// Adopt the guest-frames descriptor the controller handed over, if any.
fn take_guest_frames_channel() -> Result<Option<lillux::InheritedDuplexChannel>> {
    if std::env::var_os(LIFECYCLE_GUEST_FRAMES_FD_ENV).is_none() {
        return Ok(None);
    }
    // SAFETY: the trusted lifecycle runner installs the guest-frames
    // descriptor exactly once, as one end of a connected byte stream owned by
    // this process alone, before this single-threaded startup boundary.
    unsafe { lillux::take_inherited_duplex_channel_from_env(LIFECYCLE_GUEST_FRAMES_FD_ENV) }
        .map(Some)
        .map_err(anyhow::Error::msg)
}

/// Stage-one admission for one remote lease. It receives no credential,
/// network or activation descriptor, so a refusal here costs no provider
/// contact and no spend. `parse_spec` parses the captured spec bytes under
/// their captured digest.
pub fn admit_lease<F: LeaseProviderFacts>(
    adapter_id: &str,
    parse_spec: impl FnOnce(&[u8], &str) -> Result<F>,
) -> Result<()> {
    refuse_lease_contact_authority(LeaseOperation::Admission)?;
    let request_bytes = read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, MAX_LEASE_REQUEST_BYTES)?;
    let request: LeaseAdmissionRequest =
        from_json_slice_strict(&request_bytes, MAX_LEASE_REQUEST_BYTES)?;
    ensure!(
        canonical_json(&request)? == request_bytes,
        "lease admission request is noncanonical"
    );
    request.validate()?;
    ensure!(
        request.adapter_id == adapter_id,
        "lease admission names a different adapter"
    );
    let provider_spec = read_lease_provider_spec(&request.provider_spec_sha256, parse_spec)?;
    let response = admit_before_spend(
        &provider_spec,
        &request,
        &lillux::sha256_hex(&request_bytes),
    );
    write_response(&response)
}

/// Receive and authenticate the guest Ready and probe report of one activated
/// remote lease. Every failure to admit the guest is a `terminate` verdict; a
/// malformed controller request fails closed with no verdict. The only
/// descriptor beyond its request and the captured spec is the guest-frames
/// descriptor the controller relays the guest's frames on, and only a first
/// admission may hold it: a resumed admission reads no frame and refuses the
/// descriptor as a malformed request. This operation holds no contact
/// authority either; a transport that needs some is a reviewed change to this
/// guard.
pub fn await_lease_ready<F: LeaseProviderFacts>(
    adapter_id: &str,
    parse_spec: impl FnOnce(&[u8], &str) -> Result<F>,
) -> Result<()> {
    refuse_lease_contact_authority(LeaseOperation::Ready)?;
    let guest_frames = take_guest_frames_channel()?;
    let deadline = operation_deadline()?;
    let request_bytes = read_sealed_env(LIFECYCLE_REQUEST_FD_ENV, MAX_LEASE_READY_REQUEST_BYTES)?;
    let request: LeaseReadyRequest =
        from_json_slice_strict(&request_bytes, MAX_LEASE_READY_REQUEST_BYTES)?;
    ensure!(
        canonical_json(&request)? == request_bytes,
        "lease Ready request is noncanonical"
    );
    // The lease id's form is a provider fact, so the spec is read before the
    // request is validated.
    let provider_spec =
        read_lease_provider_spec(&request.admission.provider_spec_sha256, parse_spec)?;
    request.validate(&provider_spec)?;
    ensure!(
        request.adapter_id == adapter_id && request.admission.adapter_id == adapter_id,
        "lease Ready request names a different adapter"
    );
    let response = match request_deadline(request.receive_deadline_ms(), deadline) {
        Ok(deadline) => await_ready(&provider_spec, &request, deadline, guest_frames)?,
        Err(_) => LeaseReadyResponse::terminate(
            &request.expectation.lease_id,
            "the lease deadline elapsed before the guest frame was awaited",
        ),
    };
    write_response(&response)
}

/// Read the captured provider spec, check it is the one the lease request
/// names, and parse it with the adapter's own parser, which is handed the
/// spec bytes and their captured digest.
fn read_lease_provider_spec<F>(
    expected_sha256: &str,
    parse_spec: impl FnOnce(&[u8], &str) -> Result<F>,
) -> Result<F> {
    let provider_spec_bytes = read_sealed_env(
        LIFECYCLE_PROVIDER_SPEC_FD_ENV,
        usize::try_from(MAX_LIFECYCLE_PROVIDER_SPEC_BYTES)?,
    )?;
    let captured_sha256 = std::env::var(LIFECYCLE_PROVIDER_SPEC_SHA256_ENV)
        .context("missing captured provider spec digest")?;
    ensure!(
        captured_sha256 == expected_sha256,
        "lease request names a different provider spec than the captured one"
    );
    parse_spec(&provider_spec_bytes, &captured_sha256)
}

/// Read the provider credential descriptor. `provider_label` names the
/// provider in refusals.
pub fn read_credential(provider_label: &str) -> Result<Zeroizing<String>> {
    let bytes = read_sealed_env(LIFECYCLE_CREDENTIAL_FD_ENV, MAX_CREDENTIAL_BYTES)?;
    let bytes = Zeroizing::new(bytes);
    let credential = std::str::from_utf8(&bytes)
        .with_context(|| format!("{provider_label} credential is not UTF-8"))?;
    ensure!(
        !credential.is_empty()
            && credential.len() <= MAX_CREDENTIAL_BYTES
            && !credential.bytes().any(|byte| byte.is_ascii_control()),
        "{provider_label} credential format is invalid"
    );
    Ok(Zeroizing::new(credential.to_owned()))
}

/// The earlier of `enclosing` and the wall-clock `deadline_ms`, refused once
/// that deadline has passed.
pub fn request_deadline(
    deadline_ms: i64,
    enclosing: MonotonicDeadline,
) -> Result<MonotonicDeadline> {
    let now_ms = lillux::time::timestamp_millis();
    let remaining = deadline_ms.saturating_sub(now_ms);
    ensure!(remaining > 0, "lifecycle contact deadline elapsed");
    Ok(
        enclosing.min(MonotonicDeadline::after(Duration::from_millis(
            u64::try_from(remaining)?,
        ))),
    )
}

/// The whole operation's deadline, from the remaining time the lifecycle
/// runner hands over.
pub fn operation_deadline() -> Result<MonotonicDeadline> {
    let raw = std::env::var(LIFECYCLE_REMAINING_TIMEOUT_MS_ENV)
        .context("missing remaining lifecycle deadline")?;
    let milliseconds: u64 = raw
        .parse()
        .context("invalid remaining lifecycle deadline")?;
    ensure!(
        (1..=MAX_OPERATION_TIMEOUT_MS).contains(&milliseconds),
        "remaining lifecycle deadline is zero or exceeds its bound"
    );
    Ok(MonotonicDeadline::after(Duration::from_millis(
        milliseconds,
    )))
}

/// Read one sealed inherited descriptor named by `name`, at most `maximum`
/// bytes.
pub fn read_sealed_env(name: &str, maximum: usize) -> Result<Vec<u8>> {
    // SAFETY: the trusted lifecycle runner installs each sealed descriptor
    // exactly once before this single-threaded startup boundary.
    unsafe { lillux::read_sealed_inherited_descriptor_from_env(name, maximum) }
        .map_err(anyhow::Error::msg)
}

pub fn write_response<T: Serialize>(response: &T) -> Result<()> {
    write_response_bounded(response, MAX_LIFECYCLE_RESPONSE_BYTES)
}

pub fn write_response_bounded<T: Serialize>(response: &T, maximum_bytes: usize) -> Result<()> {
    let bytes = canonical_json(response)?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= maximum_bytes,
        "lifecycle response exceeds its byte bound"
    );
    std::io::stdout().write_all(&bytes)?;
    std::io::stdout().write_all(b"\n")?;
    std::io::stdout().flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_request_bound_holds_a_maximal_admission_with_headroom() {
        // Only the Ready request embeds another request; admission keeps its
        // own bound.
        assert_eq!(MAX_LEASE_REQUEST_BYTES, 64 * 1024);
        assert_eq!(MAX_LEASE_READY_REQUEST_BYTES, 128 * 1024);
    }

    #[test]
    fn lease_operations_refuse_every_activation_descriptor_by_name() {
        refuse_received_descriptors(LeaseOperation::Admission, |_| false).unwrap();
        refuse_received_descriptors(LeaseOperation::Ready, |_| false).unwrap();
        for operation in [LeaseOperation::Admission, LeaseOperation::Ready] {
            for descriptor in LEASE_REFUSED_DESCRIPTORS {
                let error = refuse_received_descriptors(operation, |name| {
                    name == descriptor || name == LIFECYCLE_GUEST_FRAMES_FD_ENV
                })
                .err()
                .unwrap();
                assert!(format!("{error:#}").contains(descriptor));
            }
        }
        // Only Ready admission may hold the guest-frames descriptor.
        assert!(!LEASE_REFUSED_DESCRIPTORS.contains(&LIFECYCLE_GUEST_FRAMES_FD_ENV));
        refuse_received_descriptors(LeaseOperation::Ready, |name| {
            name == LIFECYCLE_GUEST_FRAMES_FD_ENV
        })
        .unwrap();
        let error = refuse_received_descriptors(LeaseOperation::Admission, |name| {
            name == LIFECYCLE_GUEST_FRAMES_FD_ENV
        })
        .err()
        .unwrap();
        let reason = format!("{error:#}");
        assert!(reason.contains("lease admission"), "{reason}");
        assert!(reason.contains(LIFECYCLE_GUEST_FRAMES_FD_ENV), "{reason}");
        for activation in [
            LIFECYCLE_GUEST_PACKAGE_FD_ENV,
            LIFECYCLE_GUEST_RUNTIME_FD_ENV,
            LIFECYCLE_SUPERVISOR_FD_ENV,
            LIFECYCLE_LAUNCHER_FD_ENV,
        ] {
            assert!(LEASE_REFUSED_DESCRIPTORS.contains(&activation));
        }
        // Neither lease operation reads a relay upload, a separately handed
        // lease admission or a qualification binding.
        for operation in [LeaseOperation::Admission, LeaseOperation::Ready] {
            for descriptor in [
                LIFECYCLE_RELAY_UPLOAD_FD_ENV,
                LEASE_ADMISSION_FD_ENV,
                QUALIFICATION_REQUEST_BINDING_FD_ENV,
            ] {
                let error = refuse_received_descriptors(operation, |name| name == descriptor)
                    .err()
                    .unwrap();
                let reason = format!("{error:#}");
                assert!(reason.contains(operation.label()), "{reason}");
                assert!(reason.contains(descriptor), "{reason}");
            }
        }
    }
}
