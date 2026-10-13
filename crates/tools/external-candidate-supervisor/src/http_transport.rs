//! Narrow supervisor adaptation to the shared bounded HTTP transport.

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_external_execution::transport::ExternalTransportStepFailure as Failure;
use ryeos_http_transport::{
    ContactState, Deadlines, Header, HttpClient, HttpRequest, HttpResponse as SharedResponse,
    Limits, RequestBodySource,
};
use ryeos_state::external_execution::transport::ExternalSupervisorBootstrap;
use std::io::Read;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub(crate) struct BoundedHttpClient {
    client: HttpClient,
    tls_roots_der: Vec<Vec<u8>>,
    setup_timeout: Duration,
}

impl BoundedHttpClient {
    pub fn new(bootstrap: &ExternalSupervisorBootstrap, network: NetworkContext) -> Result<Self> {
        bootstrap.validate()?;
        let tls_roots_der = bootstrap
            .tls_root_certificates_der_base64
            .iter()
            .map(|encoded| {
                STANDARD
                    .decode(encoded)
                    .context("decode admitted external TLS root")
            })
            .collect::<Result<Vec<_>>>()?;
        // Refuse an unusable admitted root before any request exists, so a
        // malformed trust bundle can never reach the network path.
        let mut roots = rustls::RootCertStore::empty();
        for der in &tls_roots_der {
            roots
                .add(rustls::pki_types::CertificateDer::from(der.as_slice()))
                .context("parse admitted external TLS root certificate")?;
        }
        Ok(Self {
            client: HttpClient::new(network),
            tls_roots_der,
            setup_timeout: Duration::from_millis(u64::from(
                bootstrap.controller.http()?.connect_timeout_ms,
            )),
        })
    }

    pub fn post_json(
        &self,
        url: &url::Url,
        bytes: Vec<u8>,
        maximum_response_bytes: u64,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<HttpResponse, Failure> {
        let mut limits = Limits::control_plane();
        limits.request_header_bytes = MAX_HEADER_BYTES;
        limits.response_header_bytes = MAX_HEADER_BYTES;
        limits.response_body_bytes = maximum_response_bytes;
        limits.response_body_wire_bytes = maximum_response_bytes
            .saturating_add(MAX_HEADER_BYTES as u64)
            .saturating_add(64 * 1024);
        limits.request_body_bytes = 1024 * 1024;
        let request = HttpRequest {
            method: "POST".to_owned(),
            url: url.clone(),
            headers: vec![Header::new("content-type", "application/json")],
            body: RequestBodySource::from_bytes(bytes),
            tls_roots_der: self.tls_roots_der.clone(),
            limits,
            deadlines: Deadlines::new(self.setup_timeout, IDLE_TIMEOUT, deadline),
            cancellation: NetworkCancellation::default(),
        };
        let response = self.client.execute(request).map_err(|error| {
            let cause = anyhow::anyhow!(error.to_string());
            match error.contact_state() {
                ContactState::NoRequestSent => Failure::Fatal(cause),
                ContactState::RequestMayHaveBeenSent => {
                    after_contact_failure(error.response_refused(), cause)
                }
            }
        })?;
        collect_response(response, maximum_response_bytes)
    }
}

fn collect_response(
    mut response: SharedResponse,
    maximum_response_bytes: u64,
) -> std::result::Result<HttpResponse, Failure> {
    if !(200..300).contains(&response.status) {
        return Ok(HttpResponse {
            status: response.status,
            body: Vec::new(),
        });
    }
    let reserve = usize::try_from(maximum_response_bytes.min(64 * 1024)).unwrap_or(64 * 1024);
    let mut body = Vec::with_capacity(reserve);
    response
        .body
        .read_to_end(&mut body)
        .map_err(body_read_failure)?;
    if body.len() as u128 > maximum_response_bytes as u128 {
        return Err(Failure::Fatal(anyhow::anyhow!(
            "external response exceeds its bound"
        )));
    }
    Ok(HttpResponse {
        status: response.status,
        body,
    })
}

fn body_read_failure(error: std::io::Error) -> Failure {
    let refused = ryeos_http_transport::is_response_refusal(&error);
    after_contact_failure(
        refused,
        anyhow::Error::new(error).context("read external HTTP response"),
    )
}

/// Classify a failure after request bytes may have reached the controller.
/// Only a response the transport received and refused (its status, framing,
/// header or body bounds, or its parse) is a protocol refusal that repeating
/// the exchange cannot cure. Every other failure, including request-side and
/// TLS-record failures that share the invalid-data kind, truncation, reset,
/// timeout and cancellation, leaves the controller's commit unknown and stays
/// ambiguous.
fn after_contact_failure(response_refused: bool, cause: anyhow::Error) -> Failure {
    if response_refused {
        Failure::Fatal(cause)
    } else {
        Failure::AmbiguousTransport(cause)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_data_without_a_response_refusal_stays_ambiguous() {
        // A TLS-record failure such as a bad record MAC surfaces as plain
        // invalid data on the body stream; the controller's commit is unknown.
        let tls_record = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "received corrupt message of type ApplicationData",
        );
        assert!(matches!(
            body_read_failure(tls_record),
            Failure::AmbiguousTransport(_)
        ));
        assert!(matches!(
            after_contact_failure(false, anyhow::anyhow!("invalid HTTP request state")),
            Failure::AmbiguousTransport(_)
        ));
        assert!(matches!(
            after_contact_failure(true, anyhow::anyhow!("invalid HTTP response headers")),
            Failure::Fatal(_)
        ));
    }
}
