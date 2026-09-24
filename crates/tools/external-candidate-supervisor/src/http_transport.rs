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
        Ok(Self {
            client: HttpClient::new(network),
            tls_roots_der,
            setup_timeout: Duration::from_millis(u64::from(
                bootstrap.controller.connect_timeout_ms,
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
            let message = error.to_string();
            let cause = anyhow::anyhow!(message);
            match error.contact_state() {
                ContactState::NoRequestSent => Failure::Fatal(cause),
                ContactState::RequestMayHaveBeenSent => Failure::AmbiguousTransport(cause),
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
    response.body.read_to_end(&mut body).map_err(|error| {
        Failure::AmbiguousTransport(
            anyhow::Error::new(error).context("read external HTTP response"),
        )
    })?;
    if body.len() as u128 > maximum_response_bytes as u128 {
        return Err(Failure::AmbiguousTransport(anyhow::anyhow!(
            "external response exceeds its bound"
        )));
    }
    Ok(HttpResponse {
        status: response.status,
        body,
    })
}
