//! HTTP/TLS protocol over an owned, bounded Lillux network stream.
//! No redirects, proxy discovery, ambient roots, cookies, retries or pool.

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_external_execution::transport::ExternalTransportStepFailure as Failure;
use ryeos_state::external_execution::transport::ExternalSupervisorBootstrap;
use std::io::{Read, Write};
use std::sync::Arc;
use ureq_proto::client::{Call, RecvResponseResult, SendRequestResult};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const BUFFER_BYTES: usize = 16 * 1024;

pub(crate) struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

pub(crate) struct BoundedHttpClient {
    network: NetworkContext,
    tls: Arc<rustls::ClientConfig>,
    connect_timeout: Duration,
}

impl BoundedHttpClient {
    pub fn new(bootstrap: &ExternalSupervisorBootstrap, network: NetworkContext) -> Result<Self> {
        bootstrap.validate()?;
        let mut roots = rustls::RootCertStore::empty();
        for encoded in &bootstrap.tls_root_certificates_der_base64 {
            let der = STANDARD
                .decode(encoded)
                .context("decode admitted external TLS root")?;
            roots
                .add(rustls::pki_types::CertificateDer::from(der))
                .context("parse admitted external TLS root certificate")?;
        }
        let mut tls = rustls::ClientConfig::builder_with_details(
            crate::tls::provider(),
            Arc::new(crate::tls::HostTime),
        )
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            network,
            tls: Arc::new(tls),
            connect_timeout: Duration::from_millis(u64::from(
                bootstrap.controller.connect_timeout_ms,
            )),
        })
    }

    pub fn post_json(
        &self,
        url: &url::Url,
        bytes: Vec<u8>,
        maximum: u64,
        deadline: MonotonicDeadline,
    ) -> std::result::Result<HttpResponse, Failure> {
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Failure::Fatal(anyhow::anyhow!(
                "external channel requires an uncredentialed HTTPS endpoint"
            )));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Failure::Fatal(anyhow::anyhow!("external channel host is absent")))?
            .trim_start_matches('[')
            .trim_end_matches(']');
        let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
            .map_err(|_| Failure::Fatal(anyhow::anyhow!("invalid external TLS server name")))?;
        let connection = rustls::ClientConnection::new(self.tls.clone(), server_name)
            .map_err(|e| Failure::Fatal(e.into()))?;
        let socket = self
            .network
            .connect(
                host,
                url.port_or_known_default().unwrap(),
                MonotonicDeadline::after(self.connect_timeout),
                deadline,
                NetworkCancellation::default(),
            )
            .map_err(|e| Failure::AmbiguousTransport(e.into()))?;
        let mut tls = rustls::StreamOwned::new(connection, socket);
        while tls.conn.is_handshaking() {
            tls.conn.complete_io(&mut tls.sock).map_err(io_failure)?;
        }
        tls.sock.complete_setup().map_err(io_failure)?;
        exchange_http(&mut tls, url, bytes, maximum)
    }
}

fn io_failure(error: std::io::Error) -> Failure {
    Failure::AmbiguousTransport(error.into())
}
fn protocol_failure(error: impl std::fmt::Display) -> Failure {
    // Do not publish untrusted header values or reflected request credentials.
    let _ = error;
    Failure::Fatal(anyhow::anyhow!("invalid external HTTP framing"))
}

fn exchange_http(
    stream: &mut (impl Read + Write),
    url: &url::Url,
    bytes: Vec<u8>,
    maximum: u64,
) -> std::result::Result<HttpResponse, Failure> {
    let request = ureq_proto::http::Request::post(url.as_str())
        .header("content-type", "application/json")
        .header("content-length", bytes.len())
        .header("connection", "close")
        .header("user-agent", "ryeos-external-candidate-supervisor/1")
        .body(())
        .map_err(protocol_failure)?;
    let mut call = Call::new(request).map_err(protocol_failure)?.proceed();
    let mut buffer = [0; BUFFER_BYTES];
    while !call.can_proceed() {
        let n = call.write(&mut buffer).map_err(protocol_failure)?;
        if n == 0 {
            return Err(protocol_failure("request headers exceed buffer"));
        }
        stream.write_all(&buffer[..n]).map_err(io_failure)?;
    }
    let mut send = match call.proceed().map_err(protocol_failure)? {
        Some(SendRequestResult::SendBody(call)) => call,
        _ => return Err(protocol_failure("unexpected request state")),
    };
    let mut sent = 0;
    while !send.can_proceed() {
        let (consumed, output) = send
            .write(&bytes[sent..], &mut buffer)
            .map_err(protocol_failure)?;
        stream.write_all(&buffer[..output]).map_err(io_failure)?;
        sent += consumed;
        if consumed == 0 && output == 0 && !send.can_proceed() {
            return Err(protocol_failure("body made no progress"));
        }
    }
    stream.flush().map_err(io_failure)?;
    let mut response = send
        .proceed()
        .ok_or_else(|| protocol_failure("incomplete request body"))?;
    let mut incoming = Vec::new();
    let mut headers_consumed = 0;
    let (used, head) = loop {
        let (used, head) = response
            .try_response(&incoming, false)
            .map_err(protocol_failure)?;
        headers_consumed += used;
        if headers_consumed > MAX_HEADER_BYTES {
            return Err(protocol_failure("headers exceed cumulative bound"));
        }
        if let Some(head) = head {
            break (used, head);
        }
        if used != 0 {
            incoming.drain(..used);
            continue;
        }
        if incoming.len() + headers_consumed >= MAX_HEADER_BYTES {
            return Err(protocol_failure("headers exceed bound"));
        }
        let capacity = buffer
            .len()
            .min(MAX_HEADER_BYTES - incoming.len() - headers_consumed);
        let n = stream.read(&mut buffer[..capacity]).map_err(io_failure)?;
        if n == 0 {
            return Err(io_failure(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "external response header truncated",
            )));
        }
        incoming.extend_from_slice(&buffer[..n]);
    };
    let status = head.status().as_u16();
    if !head.status().is_success() {
        return Ok(HttpResponse {
            status,
            body: vec![],
        });
    }
    if head.headers().contains_key("transfer-encoding")
        && head.headers().contains_key("content-length")
    {
        return Err(protocol_failure("conflicting framing"));
    }
    if let Some(length) = head.headers().get("content-length") {
        let length: u64 = length
            .to_str()
            .map_err(protocol_failure)?
            .parse()
            .map_err(protocol_failure)?;
        if length > maximum {
            return Err(Failure::Fatal(anyhow::anyhow!(
                "external response exceeds its bound"
            )));
        }
    }
    incoming.drain(..used);
    let mut recv = match response.proceed() {
        Some(RecvResponseResult::RecvBody(call)) => call,
        Some(RecvResponseResult::Cleanup(_)) => {
            return Ok(HttpResponse {
                status,
                body: vec![],
            });
        }
        _ => return Err(protocol_failure("unexpected response state")),
    };
    let close_delimited = matches!(recv.body_mode(), ureq_proto::BodyMode::CloseDelimited);
    let mut body = Vec::new();
    loop {
        let (used, emitted) = recv
            .read(&incoming, &mut buffer)
            .map_err(protocol_failure)?;
        if (body.len() as u64).saturating_add(emitted as u64) > maximum {
            return Err(Failure::Fatal(anyhow::anyhow!(
                "external response exceeds its bound"
            )));
        }
        body.extend_from_slice(&buffer[..emitted]);
        incoming.drain(..used);
        if !close_delimited && recv.can_proceed() {
            break;
        }
        if used != 0 || emitted != 0 {
            continue;
        }
        if incoming.len() >= MAX_HEADER_BYTES {
            return Err(protocol_failure("body framing exceeds bound"));
        }
        let n = stream.read(&mut buffer).map_err(io_failure)?;
        if n == 0 {
            if close_delimited {
                break;
            }
            return Err(io_failure(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "external response body truncated",
            )));
        }
        incoming.extend_from_slice(&buffer[..n]);
    }
    Ok(HttpResponse { status, body })
}
