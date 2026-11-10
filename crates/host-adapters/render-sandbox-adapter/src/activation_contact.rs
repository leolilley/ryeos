//! One-shot Render guest activation contact, still disabled by the signed
//! provider profile until installed snapshot and lost-run-stream qualification.
//!
//! The daemon has already committed one durable activation claim before this
//! adapter is invoked. Every stage may have reached Render even if its reply
//! is lost. This module never retries and reconciliation must never call it.
//!
//! The run stream is read until the guest owner's terminal report, its exit,
//! loss, or the activation's absolute deadline, and the closed outcome with
//! the owner's last reported phase rides back in the Pending response. Only
//! closed words leave; stderr is never examined or forwarded.

use std::io::Read as _;

use anyhow::{Context as _, Result, ensure};
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_external_execution_contract::canonical_json;
use ryeos_external_execution_contract::guest_owner_report::{
    GuestOwnerActivationReport, GuestOwnerStdout, GuestOwnerTerminal,
};
use ryeos_external_execution_contract::{UncertaintyClass, UncertaintyReason};
use ryeos_http_transport::{
    Deadlines, Header, HttpClient, HttpRequest, HttpResponse, Limits, RequestBodySource, SseLimits,
    SseReader,
};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::provider_spec::{ProviderSpec, RouteName};
use crate::proxy_route::{
    BoundConnectToken, MAX_CONNECT_RESPONSE_BYTES, ProxyOperation, bind_connect_response,
    connect_token_url, validate_proxy_route,
};
use crate::{
    ActivationDeliveryPlan, IDLE_TIMEOUT, SETUP_TIMEOUT, Settings, api_url, has_json_content_type,
    read_credential, send_api_request, tls_roots, validate_api_url,
};

const MAX_UPLOAD_RESPONSE_BYTES: u64 = 16 * 1024;
const MAX_RUN_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
/// Time kept back from the activation deadline to write the response.
const ACTIVATION_RESPONSE_RESERVE: Duration = Duration::from_secs(3);
const OWNER_STREAM_LIMITS: SseLimits = SseLimits {
    line_bytes: 16 * 1024,
    event_bytes: 16 * 1024,
    total_bytes: 1024 * 1024,
    events: 16 * 1024,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunOutputEvent {
    stream: String,
    data: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunExitEvent {
    #[serde(rename = "exit_code")]
    _exit_code: i32,
}

/// Read the run stream until the owner's terminal report. Ending the read is
/// never an owner verdict beyond the closed outcome it returns.
fn observe_owner_report(
    body: impl std::io::Read,
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
) -> GuestOwnerActivationReport {
    let mut stdout = GuestOwnerStdout::new();
    let ended = |stdout: &GuestOwnerStdout| {
        let last_phase = stdout.last_phase();
        if deadline.has_elapsed() {
            GuestOwnerActivationReport::NoReportBeforeDeadline { last_phase }
        } else {
            GuestOwnerActivationReport::StreamLost { last_phase }
        }
    };
    let Ok(mut events) =
        SseReader::with_operation_policy(body, OWNER_STREAM_LIMITS, cancellation.clone(), deadline)
    else {
        return ended(&stdout);
    };
    let maximum = OWNER_STREAM_LIMITS.event_bytes;
    while let Ok(Some(event)) = events.next_event() {
        match event.event.as_deref() {
            Some("output") => {
                let Ok(output) = ryeos_external_execution_contract::from_json_slice_strict::<
                    RunOutputEvent,
                >(event.data.as_bytes(), maximum) else {
                    break;
                };
                if output.stream != "stdout" {
                    continue;
                }
                match stdout.push(&output.data) {
                    Ok(None) => {}
                    Ok(Some(GuestOwnerTerminal::Held)) => {
                        return GuestOwnerActivationReport::Held {
                            last_phase: stdout.last_phase(),
                            supervisor_exit: None,
                        };
                    }
                    Ok(Some(GuestOwnerTerminal::Refused(step))) => {
                        return GuestOwnerActivationReport::Refused {
                            step,
                            last_phase: stdout.last_phase(),
                            detail: stdout.refusal_detail().map(str::to_owned),
                        };
                    }
                    Err(_) => {
                        return GuestOwnerActivationReport::MalformedReport {
                            last_phase: stdout.last_phase(),
                        };
                    }
                }
            }
            Some("exit") => {
                if ryeos_external_execution_contract::from_json_slice_strict::<RunExitEvent>(
                    event.data.as_bytes(),
                    maximum,
                )
                .is_err()
                {
                    break;
                }
                return GuestOwnerActivationReport::ExitedBeforeReport {
                    last_phase: stdout.last_phase(),
                };
            }
            _ => {}
        }
    }
    ended(&stdout)
}

/// The test seam keeps order and at-most-once behavior independent of HTTP.
/// It is not an alternate lifecycle or a provider abstraction above this
/// Render adapter.
trait OneShotContact {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8], sha256: &str) -> Result<()>;
    fn upload_file(
        &mut self,
        path: &str,
        authority: &lillux::InheritedDescriptorAuthority,
        bytes: u64,
        sha256: &str,
    ) -> Result<()>;
    fn run(&mut self, command: &str) -> Result<GuestOwnerActivationReport>;
}

/// The one-shot activation step a failure happened in. It rides on the error
/// as a context so the reason the controller records can name the step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivationStep {
    UploadSignedImport,
    UploadGuestPackage,
    Run,
}

impl ActivationStep {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::UploadSignedImport => "upload_signed_import",
            Self::UploadGuestPackage => "upload_guest_package",
            Self::Run => "run",
        }
    }
}

impl std::fmt::Display for ActivationStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for ActivationStep {}

/// The bounded, fixed-word reason a failed first contact carries to the
/// controller: the step, then the classified failure when one was
/// classified, else only whether the deadline had passed. Provider text
/// reaches it only through the classification, which bounds and redacts.
pub(crate) fn contact_failure_reason(
    error: &anyhow::Error,
    deadline: MonotonicDeadline,
) -> UncertaintyReason {
    let step = error
        .downcast_ref::<ActivationStep>()
        .map_or("before_contact", |step| step.as_str());
    match error.downcast_ref::<crate::contact_failure::ContactFailure>() {
        Some(classified) => UncertaintyReason::new(
            classified.0.class,
            &format!("{step}: {}", classified.0.detail),
        ),
        None if deadline.has_elapsed() => {
            UncertaintyReason::new(UncertaintyClass::Timeout, &format!("{step}: deadline"))
        }
        None => UncertaintyReason::new(UncertaintyClass::Transport, &format!("{step}: failed")),
    }
}

fn contact_once(
    contact: &mut impl OneShotContact,
    delivery: &ActivationDeliveryPlan,
    signed_import: &[u8],
    package: &lillux::InheritedDescriptorAuthority,
) -> Result<GuestOwnerActivationReport> {
    ensure!(
        signed_import.len() as u64 == delivery.signed_import_bytes
            && lillux::sha256_hex(signed_import) == delivery.signed_import_sha256,
        "signed import changed before Render upload"
    );
    contact
        .upload_bytes(
            delivery.signed_import_remote_path,
            signed_import,
            &delivery.signed_import_sha256,
        )
        .context(ActivationStep::UploadSignedImport)?;
    contact
        .upload_file(
            delivery.guest_package_remote_path,
            package,
            delivery.guest_package_bytes,
            &delivery.guest_package_sha256,
        )
        .context(ActivationStep::UploadGuestPackage)?;
    contact
        .run(&delivery.owner_command)
        .context(ActivationStep::Run)
}

/// This function must only be called from the first claimed activation, never
/// reconciliation. Even a held report remains merely Pending: only the
/// independently authenticated supervisor channel can establish Ready.
pub(crate) fn first_activation_contact(
    network: &NetworkContext,
    provider_spec: &ProviderSpec,
    settings: &Settings,
    occurrence_id: &str,
    delivery: &ActivationDeliveryPlan,
    signed_import: &[u8],
    package: &lillux::InheritedDescriptorAuthority,
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
) -> Result<GuestOwnerActivationReport> {
    preflight_delivery_before_credential(delivery, signed_import, package)?;
    let mut contact = RenderContact::new(
        network,
        provider_spec,
        settings,
        occurrence_id,
        deadline,
        cancellation,
    )?;
    contact_once(&mut contact, delivery, signed_import, package)
}

fn preflight_delivery_before_credential(
    delivery: &ActivationDeliveryPlan,
    signed_import: &[u8],
    package: &lillux::InheritedDescriptorAuthority,
) -> Result<()> {
    ensure!(
        signed_import.len() as u64 == delivery.signed_import_bytes
            && lillux::sha256_hex(signed_import) == delivery.signed_import_sha256,
        "signed import changed before Render credential access"
    );
    verify_package_before_contact(
        package,
        delivery.guest_package_bytes,
        &delivery.guest_package_sha256,
    )
}

/// Recheck the original sealed package descriptor immediately before any
/// credential access or provider token mint. This is a point observation, not
/// writer exclusion; the streaming body independently checks its exact digest.
pub(crate) fn verify_package_before_contact(
    package: &lillux::InheritedDescriptorAuthority,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<()> {
    package.require_owned_regular()?;
    let observation = package.regular_file_observation()?;
    ensure!(
        observation.full_permission_mode()? == 0o400
            && observation.size() == expected_bytes
            && package.digest_regular_file_stable_exact(&observation)? == expected_sha256,
        "inherited guest package changed its declared delivery identity"
    );
    Ok(())
}

pub(crate) struct RenderContact<'a> {
    network: &'a NetworkContext,
    provider_spec: &'a ProviderSpec,
    settings: &'a Settings,
    occurrence_id: &'a str,
    credential: Zeroizing<String>,
    deadline: MonotonicDeadline,
    cancellation: &'a NetworkCancellation,
}

impl RenderContact<'_> {
    pub(crate) fn new<'a>(
        network: &'a NetworkContext,
        provider_spec: &'a ProviderSpec,
        settings: &'a Settings,
        occurrence_id: &'a str,
        deadline: MonotonicDeadline,
        cancellation: &'a NetworkCancellation,
    ) -> Result<RenderContact<'a>> {
        Ok(RenderContact {
            network,
            provider_spec,
            settings,
            occurrence_id,
            credential: read_credential()?,
            deadline,
            cancellation,
        })
    }

    /// The credential supplied directly, for the live integration test that
    /// runs the real contact path without the sealed-descriptor layer.
    #[cfg(test)]
    pub(crate) fn with_credential<'a>(
        network: &'a NetworkContext,
        provider_spec: &'a ProviderSpec,
        settings: &'a Settings,
        occurrence_id: &'a str,
        deadline: MonotonicDeadline,
        cancellation: &'a NetworkCancellation,
        credential: Zeroizing<String>,
    ) -> RenderContact<'a> {
        RenderContact {
            network,
            provider_spec,
            settings,
            occurrence_id,
            credential,
            deadline,
            cancellation,
        }
    }

    pub(crate) fn mint(
        &self,
        operation: ProxyOperation<'_>,
        command: Option<&str>,
    ) -> Result<BoundConnectToken> {
        let (route, file_path) = match operation {
            ProxyOperation::UploadFile { remote_path } => {
                (RouteName::FileUploadTokenById, Some(remote_path))
            }
            ProxyOperation::RunStream => (RouteName::RunStreamTokenById, None),
            ProxyOperation::DownloadFile { .. } => {
                anyhow::bail!("one-shot activation cannot mint a relay download token")
            }
        };
        let (url, target) = api_url(
            self.provider_spec,
            route,
            Some(self.occurrence_id),
            self.settings,
            file_path,
        )?;
        validate_api_url(
            &url,
            &target.path_segments,
            target.owner_id_query.as_deref(),
            target.file_path_query.as_deref(),
        )?;
        ensure!(
            url == connect_token_url(self.occurrence_id, &self.settings.owner_id, operation)?,
            "signed Render token route differs from the fixed adapter operation"
        );
        let body = command
            .map(|command| canonical_json(&serde_json::json!({ "command": command })))
            .transpose()?;
        let response = send_api_request(
            self.network,
            &url,
            "POST",
            body,
            &self.credential,
            self.deadline,
            self.settings,
            self.cancellation,
        )?;
        if response.status != 201 {
            let detail = crate::contact_failure::response_detail(response.status, response.body);
            return Err(crate::contact_failure::failure(
                ryeos_external_execution_contract::UncertaintyClass::TokenMint,
                &format!("token mint {detail}"),
            ));
        }
        if !has_json_content_type(&response.headers) {
            return Err(crate::contact_failure::failure(
                ryeos_external_execution_contract::UncertaintyClass::ContentType,
                "token mint status 201 without an exact JSON content type",
            ));
        }
        let mut bytes = Zeroizing::new(Vec::new());
        response
            .body
            .take(MAX_CONNECT_RESPONSE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow::anyhow!("Render token mint body is unreadable"))?;
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_CONNECT_RESPONSE_BYTES,
            "Render token mint body exceeds its bound"
        );
        bind_connect_response(
            &bytes,
            self.occurrence_id,
            &self.settings.region,
            operation,
            lillux::time::timestamp_millis(),
        )
    }

    pub(crate) fn send_proxy(
        &self,
        token: BoundConnectToken,
        operation: ProxyOperation<'_>,
        body: RequestBodySource,
        content_type: &'static str,
        accept: &'static str,
        response_limit: u64,
    ) -> Result<HttpResponse> {
        let request = proxy_request(
            self.settings,
            self.occurrence_id,
            token,
            operation,
            body,
            content_type,
            accept,
            response_limit,
            self.deadline,
            self.cancellation,
        )?;
        HttpClient::new(self.network.clone())
            .execute(request)
            .map_err(anyhow::Error::from)
    }
}

pub(super) fn proxy_request(
    settings: &Settings,
    occurrence_id: &str,
    token: BoundConnectToken,
    operation: ProxyOperation<'_>,
    body: RequestBodySource,
    content_type: &'static str,
    accept: &'static str,
    response_limit: u64,
    deadline: MonotonicDeadline,
    cancellation: &NetworkCancellation,
) -> Result<HttpRequest> {
    ensure!(
        token.expires_at_ms > lillux::time::timestamp_millis(),
        "Render proxy token expired before use"
    );
    validate_proxy_route(
        token.route.as_str(),
        &token.method,
        occurrence_id,
        &settings.region,
        operation,
    )?;
    let mut authorization = Zeroizing::new(b"Bearer ".to_vec());
    authorization.extend_from_slice(token.bearer.as_bytes());
    let mut limits = Limits::control_plane();
    limits.request_body_bytes = body.exact_len();
    limits.response_body_bytes = response_limit;
    limits.response_body_wire_bytes = response_limit.saturating_mul(2);
    let mut deadlines = Deadlines::new(SETUP_TIMEOUT, IDLE_TIMEOUT, deadline);
    if matches!(operation, ProxyOperation::UploadFile { .. }) {
        deadlines = deadlines.with_response_wait(crate::upload_response_wait(body.exact_len()));
    }
    if matches!(operation, ProxyOperation::RunStream) {
        // A run stream carries output only when the program writes. A verifier
        // that is working, such as the consumer scenario's model turn, is
        // silent for longer than any idle bound; the contact's absolute
        // deadline is the one bound that applies while the stream is open.
        deadlines.idle_timeout = deadline.remaining().max(IDLE_TIMEOUT);
    }
    Ok(HttpRequest {
        method: token.method,
        url: token.route,
        headers: vec![
            Header::new_sensitive("Authorization", authorization),
            Header::new("Content-Type", content_type),
            Header::new("Accept", accept),
        ],
        body,
        tls_roots_der: tls_roots(settings)?,
        limits,
        deadlines,
        cancellation: cancellation.clone(),
    })
}

impl OneShotContact for RenderContact<'_> {
    fn upload_bytes(&mut self, path: &str, bytes: &[u8], sha256: &str) -> Result<()> {
        ensure!(
            bytes.len()
                <= ryeos_external_execution_contract::guest_import_authorization::MAX_GUEST_IMPORT_AUTHORIZATION_BYTES
                    + 256
                && lillux::sha256_hex(bytes) == sha256,
            "Render signed-import upload changed its exact bytes"
        );
        let operation = ProxyOperation::UploadFile { remote_path: path };
        let token = self.mint(operation, None)?;
        let response = self.send_proxy(
            token,
            operation,
            RequestBodySource::from_bytes(bytes.to_vec()),
            "application/octet-stream",
            "application/json",
            MAX_UPLOAD_RESPONSE_BYTES,
        )?;
        ensure!(
            (200..300).contains(&response.status),
            "Render signed-import upload did not succeed"
        );
        Ok(())
    }

    fn upload_file(
        &mut self,
        path: &str,
        authority: &lillux::InheritedDescriptorAuthority,
        bytes: u64,
        sha256: &str,
    ) -> Result<()> {
        let operation = ProxyOperation::UploadFile { remote_path: path };
        let token = self.mint(operation, None)?;
        let response = self.send_proxy(
            token,
            operation,
            RequestBodySource::from_inherited_regular_file(authority.clone(), bytes, sha256.into()),
            "application/octet-stream",
            "application/json",
            MAX_UPLOAD_RESPONSE_BYTES,
        )?;
        ensure!(
            (200..300).contains(&response.status),
            "Render exact guest-package upload did not succeed"
        );
        Ok(())
    }

    fn run(&mut self, command: &str) -> Result<GuestOwnerActivationReport> {
        let operation = ProxyOperation::RunStream;
        let token = self.mint(operation, Some(command))?;
        let body = canonical_json(&serde_json::json!({ "command": command }))?;
        let stream_deadline = MonotonicDeadline::after(
            self.deadline
                .remaining()
                .saturating_sub(ACTIVATION_RESPONSE_RESERVE),
        );
        let mut request = proxy_request(
            self.settings,
            self.occurrence_id,
            token,
            operation,
            RequestBodySource::from_bytes(body),
            "application/json",
            "text/event-stream",
            MAX_RUN_RESPONSE_BYTES,
            stream_deadline,
            self.cancellation,
        )?;
        // The owner is silent between progress reports; only the absolute
        // deadline bounds the stream.
        request.deadlines.idle_timeout = stream_deadline.remaining().max(IDLE_TIMEOUT);
        let response = HttpClient::new(self.network.clone()).execute(request)?;
        ensure!(
            response.status == 200,
            "Render run proxy did not return the expected stream status"
        );
        // The stream is dropped after the terminal report; the owner survives
        // a lost client stream by design.
        Ok(observe_owner_report(
            response.body,
            stream_deadline,
            self.cancellation,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use std::os::unix::fs::PermissionsExt as _;

    fn proxy_settings() -> Settings {
        Settings {
            schema: 2,
            owner_id: "tea-owner".into(),
            plan: crate::RenderPlan::Starter,
            region: "oregon".into(),
            snapshot_id: "snp-fixture".into(),
            tls_roots_der_base64: vec![
                base64::engine::general_purpose::STANDARD.encode(b"exact-test-root"),
            ],
        }
    }

    fn upload_token(uri: &str) -> BoundConnectToken {
        BoundConnectToken {
            execution_id: "exe-fixture".into(),
            expires_at_ms: lillux::time::timestamp_millis() + 60_000,
            method: "PUT".into(),
            route: url::Url::parse(uri).unwrap(),
            bearer: Zeroizing::new("private-token-sentinel".into()),
        }
    }

    #[test]
    fn signed_token_routes_match_fixed_render_operations() {
        let settings = proxy_settings();
        let schema_digest = lillux::sha256_hex(include_bytes!("../fixtures/settings.schema.json"));
        let spec = ProviderSpec::parse(
            include_bytes!("../fixtures/provider-spec.json"),
            &schema_digest,
        )
        .unwrap();
        for (route, operation, file_path) in [
            (
                RouteName::FileUploadTokenById,
                ProxyOperation::UploadFile {
                    remote_path: "/ryeos/activation/guest-package",
                },
                Some("/ryeos/activation/guest-package"),
            ),
            (
                RouteName::FileDownloadTokenById,
                ProxyOperation::DownloadFile {
                    remote_path: "/ryeos/relay/attach-request",
                },
                Some("/ryeos/relay/attach-request"),
            ),
            (
                RouteName::RunStreamTokenById,
                ProxyOperation::RunStream,
                None,
            ),
        ] {
            let (url, target) =
                api_url(&spec, route, Some("sbx-fixture"), &settings, file_path).unwrap();
            validate_api_url(
                &url,
                &target.path_segments,
                target.owner_id_query.as_deref(),
                target.file_path_query.as_deref(),
            )
            .unwrap();
            assert_eq!(
                url,
                connect_token_url("sbx-fixture", &settings.owner_id, operation).unwrap()
            );
        }
    }

    #[test]
    fn proxy_request_binds_bearer_to_exact_sandbox_and_body() {
        let settings = proxy_settings();
        let cancellation = NetworkCancellation::default();
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(30));
        let path = "/ryeos/activation/guest-package";
        let uri = "https://sbx-fixture.oregon.sandbox.onrender.com/files/upload?path=%2Fryeos%2Factivation%2Fguest-package";
        let request = proxy_request(
            &settings,
            "sbx-fixture",
            upload_token(uri),
            ProxyOperation::UploadFile { remote_path: path },
            RequestBodySource::from_bytes(b"exact-package".to_vec()),
            "application/octet-stream",
            "application/json",
            MAX_UPLOAD_RESPONSE_BYTES,
            deadline,
            &cancellation,
        )
        .unwrap();
        assert_eq!(request.method, "PUT");
        assert_eq!(request.url.as_str(), uri);
        assert_eq!(request.body.exact_len(), 13);
        assert_eq!(request.limits.request_body_bytes, 13);
        assert_eq!(
            request.limits.response_body_bytes,
            MAX_UPLOAD_RESPONSE_BYTES
        );
        assert_eq!(request.headers[0].value(), b"Bearer private-token-sentinel");
        assert!(!format!("{:?}", request.headers[0]).contains("private-token-sentinel"));
        assert_eq!(request.tls_roots_der, vec![b"exact-test-root".to_vec()]);
        for (occurrence, route) in [
            ("sbx-other", uri),
            (
                "sbx-fixture",
                "https://sbx-other.oregon.sandbox.onrender.com/files/upload?path=%2Fryeos%2Factivation%2Fguest-package",
            ),
        ] {
            assert!(
                proxy_request(
                    &settings,
                    occurrence,
                    upload_token(route),
                    ProxyOperation::UploadFile { remote_path: path },
                    RequestBodySource::from_bytes(b"exact-package".to_vec()),
                    "application/octet-stream",
                    "application/json",
                    MAX_UPLOAD_RESPONSE_BYTES,
                    deadline,
                    &cancellation,
                )
                .is_err()
            );
        }
    }

    struct CountingContact {
        calls: Vec<&'static str>,
        fail_at: Option<&'static str>,
    }

    impl CountingContact {
        fn record(&mut self, stage: &'static str) -> Result<()> {
            self.calls.push(stage);
            ensure!(self.fail_at != Some(stage), "simulated lost response");
            Ok(())
        }
    }

    impl OneShotContact for CountingContact {
        fn upload_bytes(&mut self, _: &str, _: &[u8], _: &str) -> Result<()> {
            self.record("import")
        }
        fn upload_file(
            &mut self,
            _: &str,
            _: &lillux::InheritedDescriptorAuthority,
            _: u64,
            _: &str,
        ) -> Result<()> {
            self.record("package")
        }
        fn run(&mut self, _: &str) -> Result<GuestOwnerActivationReport> {
            self.record("run")?;
            Ok(GuestOwnerActivationReport::Held {
                last_phase: None,
                supervisor_exit: None,
            })
        }
    }

    #[test]
    fn a_failed_first_contact_names_its_step_in_fixed_words() {
        let deadline = MonotonicDeadline::after(Duration::from_secs(60));
        let classified = crate::contact_failure::failure(
            UncertaintyClass::UploadStatus,
            "status 503; message: Bearer abc",
        )
        .context(ActivationStep::UploadGuestPackage);
        let reason = contact_failure_reason(&classified, deadline);
        assert_eq!(reason.class, UncertaintyClass::UploadStatus);
        assert!(
            reason
                .detail
                .starts_with("upload_guest_package: status 503"),
            "{}",
            reason.detail
        );

        let plain = anyhow::anyhow!("socket closed by https://provider.example/secret-path")
            .context(ActivationStep::Run);
        let reason = contact_failure_reason(&plain, deadline);
        assert_eq!(reason.class, UncertaintyClass::Transport);
        assert_eq!(reason.detail, "run: failed");

        let elapsed = MonotonicDeadline::after(Duration::ZERO);
        let late = anyhow::anyhow!("read timed out").context(ActivationStep::UploadGuestPackage);
        let reason = contact_failure_reason(&late, elapsed);
        assert_eq!(reason.class, UncertaintyClass::Timeout);
        assert_eq!(reason.detail, "upload_guest_package: deadline");

        let unstepped = anyhow::anyhow!("signed import changed before Render upload");
        assert_eq!(
            contact_failure_reason(&unstepped, deadline).detail,
            "before_contact: failed"
        );
    }

    #[test]
    fn one_shot_sequence_stops_at_first_uncertain_contact() {
        let bytes = b"signed-import";
        let plan = ActivationDeliveryPlan {
            signed_import_remote_path: "/ryeos/activation/signed-import.json",
            signed_import_sha256: lillux::sha256_hex(bytes),
            signed_import_bytes: bytes.len() as u64,
            guest_package_remote_path: "/ryeos/activation/guest-package",
            guest_package_sha256: "a".repeat(64),
            guest_package_bytes: 1,
            owner_command: "exec /ryeos/guest-runtime/bin/owner --assignment-b64 abc".into(),
        };
        let parent = tempfile::tempdir().unwrap();
        let source = lillux::PinnedDirectory::open(parent.path())
            .unwrap()
            .unwrap();
        std::fs::write(parent.path().join("package"), b"x").unwrap();
        let package = source
            .open_pinned_regular(std::ffi::OsStr::new("package"), false)
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        for (fail_at, expected) in [
            (Some("import"), vec!["import"]),
            (Some("package"), vec!["import", "package"]),
            (Some("run"), vec!["import", "package", "run"]),
            (None, vec!["import", "package", "run"]),
        ] {
            let mut contact = CountingContact {
                calls: Vec::new(),
                fail_at,
            };
            let result = contact_once(&mut contact, &plan, bytes, &package);
            assert_eq!(result.is_err(), fail_at.is_some());
            assert_eq!(contact.calls, expected);
        }
        let mut contact = CountingContact {
            calls: Vec::new(),
            fail_at: None,
        };
        assert!(contact_once(&mut contact, &plan, b"changed", &package).is_err());
        assert!(contact.calls.is_empty());
    }

    fn sse(events: &[(&str, serde_json::Value)]) -> Vec<u8> {
        events
            .iter()
            .map(|(name, data)| format!("event: {name}\ndata: {data}\n\n"))
            .collect::<String>()
            .into_bytes()
    }

    fn output(stream: &str, data: &str) -> (&'static str, serde_json::Value) {
        (
            "output",
            serde_json::json!({ "stream": stream, "data": data }),
        )
    }

    fn observed(body: &[u8]) -> GuestOwnerActivationReport {
        let deadline = MonotonicDeadline::after(lillux::time::Duration::from_secs(30));
        observe_owner_report(body, deadline, &NetworkCancellation::default())
    }

    #[test]
    fn activation_stream_reports_each_owner_outcome_with_its_last_phase() {
        use ryeos_external_execution_contract::guest_owner_report::{
            GuestOwnerReport, GuestOwnerStep as Step,
        };
        let phase = |step| GuestOwnerReport::Phase(step).line();
        let held = GuestOwnerReport::Held.line();
        let refused = GuestOwnerReport::Refused(Step::Install).line();
        let exit = ("exit", serde_json::json!({ "exit_code": 1 }));
        let staged = [
            output("stdout", &phase(Step::ImportVerify)),
            output("stderr", "progress noise\n"),
            output("stdout", &phase(Step::Stage)),
        ];
        let with = |tail: &[(&'static str, serde_json::Value)]| {
            let mut events = staged.to_vec();
            events.extend_from_slice(tail);
            sse(&events)
        };
        assert_eq!(
            observed(&with(&[output("stdout", &held)])),
            GuestOwnerActivationReport::Held {
                last_phase: Some(Step::Stage),
                supervisor_exit: None
            }
        );
        // The terminal line ends the read; later events are never examined.
        assert_eq!(
            observed(&with(&[
                output("stdout", &held),
                output("stdout", "garbage\n"),
                exit.clone()
            ])),
            GuestOwnerActivationReport::Held {
                last_phase: Some(Step::Stage),
                supervisor_exit: None
            }
        );
        assert_eq!(
            observed(&with(&[
                output("stdout", &phase(Step::Install)[..10]),
                output("stdout", &phase(Step::Install)[10..]),
                output("stdout", &refused),
            ])),
            GuestOwnerActivationReport::Refused {
                step: Step::Install,
                last_phase: Some(Step::Install),
                detail: None,
            }
        );
        assert_eq!(
            observed(&with(&[
                output("stderr", "Error: private text\n"),
                exit.clone()
            ])),
            GuestOwnerActivationReport::ExitedBeforeReport {
                last_phase: Some(Step::Stage)
            }
        );
        // Stderr is never read as a report, even when it looks like one.
        assert_eq!(
            observed(&sse(&[output("stderr", &refused)])),
            GuestOwnerActivationReport::StreamLost { last_phase: None }
        );
        assert_eq!(
            observed(&with(&[])),
            GuestOwnerActivationReport::StreamLost {
                last_phase: Some(Step::Stage)
            }
        );
        for garbage in ["thread 'main' panicked\n", &"x".repeat(4096)] {
            assert_eq!(
                observed(&with(&[output("stdout", garbage), exit.clone()])),
                GuestOwnerActivationReport::MalformedReport {
                    last_phase: Some(Step::Stage)
                }
            );
        }
    }

    #[test]
    fn activation_stream_ending_without_a_terminal_report_is_lost_or_late() {
        assert_eq!(
            observed(b""),
            GuestOwnerActivationReport::StreamLost { last_phase: None }
        );
        for body in [
            sse(&[("keepalive", serde_json::json!({}))]),
            sse(&[(
                "output",
                serde_json::json!({ "stream": "stdout", "data": "x", "extra": 1 }),
            )]),
            sse(&[("exit", serde_json::json!({}))]),
            sse(&[output("stderr", &"a".repeat(32 * 1024))]),
            sse(&(0..160)
                .map(|_| output("stderr", &"e".repeat(8 * 1024)))
                .collect::<Vec<_>>()),
        ] {
            assert_eq!(
                observed(&body),
                GuestOwnerActivationReport::StreamLost { last_phase: None }
            );
        }
        let refused = sse(&[output(
            "stdout",
            &ryeos_external_execution_contract::guest_owner_report::GuestOwnerReport::Refused(
                ryeos_external_execution_contract::guest_owner_report::GuestOwnerStep::Stage,
            )
            .line(),
        )]);
        let cancellation = NetworkCancellation::default();
        cancellation.cancel();
        let live = MonotonicDeadline::after(lillux::time::Duration::from_secs(30));
        assert_eq!(
            observe_owner_report(&refused[..], live, &cancellation),
            GuestOwnerActivationReport::StreamLost { last_phase: None }
        );
        let expired = MonotonicDeadline::after(lillux::time::Duration::ZERO);
        assert_eq!(
            observe_owner_report(&refused[..], expired, &NetworkCancellation::default()),
            GuestOwnerActivationReport::NoReportBeforeDeadline { last_phase: None }
        );
    }

    #[test]
    fn source_drift_refuses_before_credential_or_token_mint() {
        let parent = tempfile::tempdir().unwrap();
        let path = parent.path().join("package");
        std::fs::write(&path, b"exact-package").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        let root = lillux::PinnedDirectory::open(parent.path())
            .unwrap()
            .unwrap();
        let package = root
            .open_pinned_regular(std::ffi::OsStr::new("package"), false)
            .unwrap()
            .unwrap()
            .inherited_descriptor_authority()
            .unwrap();
        let signed_import = b"signed-import";
        let plan = ActivationDeliveryPlan {
            signed_import_remote_path: "/ryeos/activation/signed-import.json",
            signed_import_sha256: lillux::sha256_hex(signed_import),
            signed_import_bytes: signed_import.len() as u64,
            guest_package_remote_path: "/ryeos/activation/guest-package",
            guest_package_sha256: lillux::sha256_hex(b"exact-package"),
            guest_package_bytes: b"exact-package".len() as u64,
            owner_command: "fixed-owner-command".into(),
        };
        preflight_delivery_before_credential(&plan, signed_import, &package).unwrap();
        assert!(preflight_delivery_before_credential(&plan, b"changed", &package).is_err());
        let mut changed_digest = plan;
        changed_digest.guest_package_sha256 = "a".repeat(64);
        assert!(
            preflight_delivery_before_credential(&changed_digest, signed_import, &package).is_err()
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            preflight_delivery_before_credential(
                &ActivationDeliveryPlan {
                    guest_package_sha256: lillux::sha256_hex(b"exact-package"),
                    ..changed_digest
                },
                signed_import,
                &package,
            )
            .is_err()
        );
    }
}
