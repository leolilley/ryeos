//! Single-connection HTTP/1.1 client with explicit framing and byte bounds.

use crate::{Deadlines, HttpError, Limits};
use lillux::network::{NetworkCancellation, NetworkContext, NetworkStream};
use lillux::time::MonotonicDeadline;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore};
use std::fmt;
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::sync::Arc;
use ureq_proto::client::{Call, RecvResponseResult, SendRequestResult};
use zeroize::{Zeroize, Zeroizing};

const IO_BUFFER_BYTES: usize = 16 * 1024;
const MAX_REQUEST_BODY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REQUEST_HEADER_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_HEADER_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BODY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RESPONSE_BODY_WIRE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TLS_ROOT_CERTIFICATES: usize = 64;
const MAX_TLS_ROOT_BYTES: usize = 1024 * 1024;
const MAX_TLS_ROOT_CERTIFICATE_BYTES: usize = 64 * 1024;
const MAX_REQUEST_HEADER_FIELDS: usize = 128;

/// One HTTP header. Response values preserve their original octets.
#[derive(Clone, PartialEq, Eq)]
pub struct Header {
    /// Header field name.
    pub name: String,
    /// Header value bytes, excluding line framing.
    value: Vec<u8>,
    sensitive: bool,
}

impl Header {
    /// Build a header from a UTF-8 value.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into().into_bytes(),
            sensitive: false,
        }
    }

    /// Build a sensitive header from bytes that are zeroized when dropped.
    ///
    /// The value remains available to the HTTP parser while the request is
    /// prepared. This scrubs RyeOS-owned copies; internal copies made by
    /// `ureq-proto` are outside this type's control.
    pub fn new_sensitive(name: impl Into<String>, mut value: Zeroizing<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            value: std::mem::take(&mut *value),
            sensitive: true,
        }
    }

    /// Read the header value without exposing its owned buffer for replacement.
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Header");
        debug.field("name", &self.name);
        if self.sensitive {
            debug.field("value", &"[REDACTED]");
        } else {
            debug.field("value", &self.value);
        }
        debug.finish()
    }
}

impl Drop for Header {
    fn drop(&mut self) {
        if self.sensitive {
            self.value.zeroize();
        }
    }
}

/// A bounded, exact-length upload captured in host-owned memory.
///
/// A captured regular file is accepted only through Lillux's retained stable
/// snapshot type. No path is reopened and no arbitrary `Read` implementation
/// can stall the lifecycle worker during transmission.
pub enum RequestBodySource {
    /// Finite caller-owned bytes.
    Bytes(Vec<u8>),
    /// Stable byte snapshot captured by Lillux from an admitted regular file.
    CapturedRegularFile(lillux::secure_fs::CapturedRegularFile),
}

impl RequestBodySource {
    /// Retain finite bytes as one exact-length request body.
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes)
    }

    /// Retain a previously captured stable regular-file snapshot.
    pub fn from_captured_regular_file(file: lillux::secure_fs::CapturedRegularFile) -> Self {
        Self::CapturedRegularFile(file)
    }

    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::CapturedRegularFile(file) => file.bytes(),
        }
    }

    /// Return the exact retained byte length used for Content-Length.
    pub fn exact_len(&self) -> u64 {
        u64::try_from(self.as_bytes().len()).unwrap_or(u64::MAX)
    }
}

/// Exact-length request body retained in memory for this one operation.
///
/// The transport writes it in bounded chunks and emits a fixed Content-Length.
/// It accepts no arbitrary reader, reopened path, pipe, or chunked upload.
pub struct HttpRequest {
    /// HTTP method token.
    pub method: String,
    /// Fully qualified URL. Only HTTPS URLs without user-info or fragments are accepted.
    pub url: url::Url,
    /// Caller-provided end-to-end headers.
    pub headers: Vec<Header>,
    /// Finite exact-length source. Its bytes are sent with Content-Length.
    pub body: RequestBodySource,
    /// Explicit DER trust roots for this request's destination only.
    pub tls_roots_der: Vec<Vec<u8>>,
    /// Independent request, response-header, and response-body ceilings.
    pub limits: Limits,
    /// Immutable setup, idle, and absolute operation deadlines.
    pub deadlines: Deadlines,
    /// Caller-owned cancellation signal propagated into every socket I/O.
    pub cancellation: lillux::network::NetworkCancellation,
}

/// One response head and its bounded streaming body.
pub struct HttpResponse {
    /// HTTP status code. Redirect statuses are returned without following.
    pub status: u16,
    /// Response headers as preserved by the HTTP parser.
    pub headers: Vec<Header>,
    /// Bounded body stream. Read failures after execute are contact-ambiguous.
    pub body: ResponseBody,
}

/// A response body stream enforcing a cumulative response byte ceiling.
pub struct ResponseBody {
    state: BodyState,
}

enum BodyState {
    Empty,
    Streaming(StreamingBody),
}

struct StreamingBody {
    stream: TlsStream,
    recv: Call<ureq_proto::client::state::RecvBody>,
    incoming: Vec<u8>,
    maximum: u64,
    maximum_wire: u64,
    maximum_framing_buffer: usize,
    delivered: u64,
    wire_received: u64,
    close_delimited: bool,
    cancellation: NetworkCancellation,
    absolute_deadline: MonotonicDeadline,
    eof: bool,
    failed: bool,
}

type TlsStream = rustls::StreamOwned<ClientConnection, NetworkStream>;

impl ResponseBody {
    fn empty() -> Self {
        Self {
            state: BodyState::Empty,
        }
    }

    fn streaming(
        stream: TlsStream,
        recv: Call<ureq_proto::client::state::RecvBody>,
        incoming: Vec<u8>,
        maximum: u64,
        maximum_wire: u64,
        maximum_framing_buffer: usize,
        cancellation: NetworkCancellation,
        absolute_deadline: MonotonicDeadline,
    ) -> Self {
        let wire_received = incoming.len() as u64;
        Self {
            state: BodyState::Streaming(StreamingBody {
                stream,
                close_delimited: matches!(recv.body_mode(), ureq_proto::BodyMode::CloseDelimited),
                recv,
                incoming,
                maximum,
                maximum_wire,
                maximum_framing_buffer,
                delivered: 0,
                wire_received,
                cancellation,
                absolute_deadline,
                eof: false,
                failed: false,
            }),
        }
    }
}

impl Read for ResponseBody {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let BodyState::Streaming(body) = &mut self.state else {
            return Ok(0);
        };
        if body.failed {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "response stream is settled",
            ));
        }
        let mut buffer = [0u8; IO_BUFFER_BYTES];
        loop {
            if let Err(error) = body.check_policy() {
                body.failed = true;
                return Err(error);
            }
            if body.eof {
                return Ok(0);
            }
            let remaining = body.maximum.saturating_sub(body.delivered);
            let mut capacity = output.len().min(buffer.len());
            if remaining < capacity as u64 {
                capacity = usize::try_from(remaining).unwrap_or(0).saturating_add(1);
            }
            capacity = capacity.max(1).min(buffer.len());
            let (used, emitted) = match body.recv.read(&body.incoming, &mut buffer[..capacity]) {
                Ok(progress) => progress,
                Err(_) => {
                    body.failed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid HTTP response body framing",
                    ));
                }
            };
            body.incoming.drain(..used);
            if emitted != 0 {
                if let Err(error) = body.check_policy() {
                    body.failed = true;
                    return Err(error);
                }
                if (body.delivered as u128) + (emitted as u128) > body.maximum as u128 {
                    body.failed = true;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "HTTP response body exceeds its bound",
                    ));
                }
                body.delivered += emitted as u64;
                let copied = emitted.min(output.len());
                output[..copied].copy_from_slice(&buffer[..copied]);
                return Ok(copied);
            }
            if !body.close_delimited && body.recv.can_proceed() {
                body.eof = true;
                return Ok(0);
            }
            if used != 0 {
                continue;
            }
            if body.incoming.len() >= body.maximum_framing_buffer {
                body.failed = true;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP response framing exceeds its bound",
                ));
            }
            let received = match body.stream.read(&mut buffer) {
                Ok(count) => count,
                Err(error) => {
                    body.failed = true;
                    return Err(error);
                }
            };
            if received == 0 {
                if body.close_delimited {
                    body.eof = true;
                    return Ok(0);
                }
                body.failed = true;
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "HTTP response body ended before its framing did",
                ));
            }
            if (body.wire_received as u128) + (received as u128) > body.maximum_wire as u128 {
                body.failed = true;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP response wire body exceeds its bound",
                ));
            }
            body.wire_received += received as u64;
            body.incoming.extend_from_slice(&buffer[..received]);
        }
    }
}

impl StreamingBody {
    fn check_policy(&self) -> io::Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "HTTP response operation was cancelled",
            ));
        }
        if self.absolute_deadline.has_elapsed() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP response operation deadline elapsed",
            ));
        }
        Ok(())
    }
}

pub(crate) fn execute(
    network: &NetworkContext,
    request: HttpRequest,
) -> Result<HttpResponse, HttpError> {
    validate(&request)?;
    let host = request
        .url
        .host_str()
        .ok_or_else(|| HttpError::before(io::ErrorKind::InvalidInput, "HTTPS host is absent"))?;
    let host_without_brackets = host.trim_start_matches('[').trim_end_matches(']');
    let server_name = if let Ok(address) = host_without_brackets.parse::<IpAddr>() {
        ServerName::IpAddress(address.into())
    } else {
        ServerName::try_from(host_without_brackets.to_owned()).map_err(|_| {
            HttpError::before(io::ErrorKind::InvalidInput, "invalid HTTPS server name")
        })?
    };
    let tls_config = build_tls_config(&request.tls_roots_der)?;
    let tls_session = ClientConnection::new(tls_config, server_name)
        .map_err(|_| HttpError::before(io::ErrorKind::InvalidData, "TLS session setup failed"))?;
    let (request_head, send_request) = encode_request_head(
        build_http_call(&request)?,
        request.limits.request_header_bytes,
    )?;

    let setup_deadline = MonotonicDeadline::after(request.deadlines.setup_timeout);
    let socket = network
        .connect(
            host_without_brackets,
            request.url.port_or_known_default().unwrap_or(443),
            setup_deadline,
            request.deadlines.absolute,
            request.deadlines.idle_timeout,
            request.cancellation.clone(),
        )
        .map_err(|error| HttpError::before(error.kind(), "HTTPS connection setup failed"))?;
    if request.deadlines.absolute.has_elapsed() {
        return Err(HttpError::before(
            io::ErrorKind::TimedOut,
            "HTTP operation deadline elapsed before request transmission",
        ));
    }
    let mut stream = rustls::StreamOwned::new(tls_session, socket);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|error| HttpError::before(error.kind(), "TLS setup failed"))?;
    }
    stream
        .sock
        .complete_setup()
        .map_err(|error| HttpError::before(error.kind(), "TLS setup deadline elapsed"))?;

    // Past this point every error is conservatively remote-contact ambiguous.
    if request.cancellation.is_cancelled() || request.deadlines.absolute.has_elapsed() {
        let kind = if request.cancellation.is_cancelled() {
            io::ErrorKind::Interrupted
        } else {
            io::ErrorKind::TimedOut
        };
        return Err(HttpError::before(
            kind,
            "HTTP operation settled before request transmission",
        ));
    }
    stream
        .write_all(&request_head)
        .map_err(|error| HttpError::ambiguous(error.kind(), "HTTP request transmission failed"))?;
    let mut send = match send_request.proceed().map_err(|_| {
        HttpError::ambiguous(io::ErrorKind::InvalidData, "invalid HTTP request state")
    })? {
        Some(SendRequestResult::SendBody(call)) => call,
        Some(SendRequestResult::RecvResponse(call)) => {
            return receive_response(
                stream,
                call,
                Vec::new(),
                request.limits,
                request.cancellation.clone(),
                request.deadlines.absolute,
            );
        }
        Some(SendRequestResult::Await100(_)) => {
            return Err(HttpError::ambiguous(
                io::ErrorKind::InvalidData,
                "unsupported HTTP expect-continue state",
            ));
        }
        None => {
            return Err(HttpError::ambiguous(
                io::ErrorKind::InvalidData,
                "incomplete HTTP request head",
            ));
        }
    };

    let body_bytes = request.body.as_bytes();
    let mut offset = 0usize;
    let mut buffer = [0u8; IO_BUFFER_BYTES];
    while !send.can_proceed() {
        if request.cancellation.is_cancelled() || request.deadlines.absolute.has_elapsed() {
            let kind = if request.cancellation.is_cancelled() {
                io::ErrorKind::Interrupted
            } else {
                io::ErrorKind::TimedOut
            };
            return Err(HttpError::ambiguous(
                kind,
                "HTTP request transmission was interrupted",
            ));
        }
        let (consumed, produced) =
            send.write(&body_bytes[offset..], &mut buffer)
                .map_err(|_| {
                    HttpError::ambiguous(io::ErrorKind::InvalidData, "invalid HTTP request body")
                })?;
        if produced != 0 {
            stream.write_all(&buffer[..produced]).map_err(|error| {
                HttpError::ambiguous(error.kind(), "HTTP request transmission failed")
            })?;
        }
        offset = offset.saturating_add(consumed);
        if consumed == 0 && produced == 0 && !send.can_proceed() {
            return Err(HttpError::ambiguous(
                io::ErrorKind::WriteZero,
                "HTTP request body made no progress",
            ));
        }
    }
    if offset != body_bytes.len() {
        return Err(HttpError::ambiguous(
            io::ErrorKind::InvalidData,
            "HTTP request body length did not match its declaration",
        ));
    }
    stream
        .flush()
        .map_err(|error| HttpError::ambiguous(error.kind(), "HTTP request transmission failed"))?;
    let response = send.proceed().ok_or_else(|| {
        HttpError::ambiguous(io::ErrorKind::InvalidData, "incomplete HTTP request body")
    })?;
    receive_response(
        stream,
        response,
        Vec::new(),
        request.limits,
        request.cancellation,
        request.deadlines.absolute,
    )
}

fn validate(request: &HttpRequest) -> Result<(), HttpError> {
    if request.url.scheme() != "https"
        || !request.url.username().is_empty()
        || request.url.password().is_some()
        || request.url.fragment().is_some()
    {
        return Err(HttpError::before(
            io::ErrorKind::InvalidInput,
            "request requires an uncredentialed HTTPS URL",
        ));
    }
    if request.deadlines.setup_timeout.is_zero()
        || request.deadlines.idle_timeout.is_zero()
        || request.deadlines.absolute.has_elapsed()
    {
        return Err(HttpError::before(
            io::ErrorKind::TimedOut,
            "HTTP operation has an expired or zero deadline",
        ));
    }
    if request.method.is_empty()
        || request.method.len() > 64
        || request.method.eq_ignore_ascii_case("CONNECT")
    {
        return Err(HttpError::before(
            io::ErrorKind::InvalidInput,
            "HTTP method is empty, oversized, or attempts a tunnel",
        ));
    }
    if request.limits.request_body_bytes > MAX_REQUEST_BODY_BYTES
        || request.limits.request_header_bytes == 0
        || request.limits.request_header_bytes > MAX_REQUEST_HEADER_BYTES
        || request.limits.response_header_bytes == 0
        || request.limits.response_header_bytes > MAX_RESPONSE_HEADER_BYTES
        || request.limits.response_body_bytes == 0
        || request.limits.response_body_bytes > MAX_RESPONSE_BODY_BYTES
        || request.limits.response_body_wire_bytes == 0
        || request.limits.response_body_wire_bytes > MAX_RESPONSE_BODY_WIRE_BYTES
    {
        return Err(HttpError::before(
            io::ErrorKind::InvalidInput,
            "HTTP byte ceilings are invalid or exceed their bound",
        ));
    }
    let body_len = request.body.exact_len();
    if body_len > request.limits.request_body_bytes {
        return Err(HttpError::before(
            io::ErrorKind::InvalidInput,
            "HTTP request body exceeds its bound",
        ));
    }
    if request.url.as_str().len() > request.limits.request_header_bytes {
        return Err(HttpError::before(
            io::ErrorKind::InvalidInput,
            "HTTP request target exceeds its bound",
        ));
    }
    if request.headers.len() > MAX_REQUEST_HEADER_FIELDS {
        return Err(HttpError::before(
            io::ErrorKind::InvalidInput,
            "HTTP request has too many header fields",
        ));
    }
    let mut header_input_bytes = 0usize;
    let mut has_authorization = false;
    for header in &request.headers {
        header_input_bytes = header_input_bytes
            .saturating_add(header.name.len())
            .saturating_add(header.value.len());
        if header_input_bytes > request.limits.request_header_bytes {
            return Err(HttpError::before(
                io::ErrorKind::InvalidInput,
                "HTTP request headers exceed their bound",
            ));
        }
        let name = header.name.to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "host"
                | "connection"
                | "content-length"
                | "transfer-encoding"
                | "te"
                | "trailer"
                | "upgrade"
                | "keep-alive"
                | "proxy-authorization"
                | "proxy-authenticate"
                | "proxy-connection"
                | "expect"
                | "accept-encoding"
        ) {
            return Err(HttpError::before(
                io::ErrorKind::InvalidInput,
                "caller supplied a transport-managed HTTP header",
            ));
        }
        if name == "authorization" {
            if has_authorization {
                return Err(HttpError::before(
                    io::ErrorKind::InvalidInput,
                    "HTTP request repeats its Authorization header",
                ));
            }
            has_authorization = true;
        }
    }
    Ok(())
}

fn build_tls_config(roots_der: &[Vec<u8>]) -> Result<Arc<ClientConfig>, HttpError> {
    let root_bytes = roots_der
        .iter()
        .fold(0usize, |total, root| total.saturating_add(root.len()));
    if roots_der.is_empty()
        || roots_der.len() > MAX_TLS_ROOT_CERTIFICATES
        || root_bytes > MAX_TLS_ROOT_BYTES
        || roots_der
            .iter()
            .any(|root| root.is_empty() || root.len() > MAX_TLS_ROOT_CERTIFICATE_BYTES)
    {
        return Err(HttpError::before(
            io::ErrorKind::InvalidInput,
            "request TLS trust roots are absent or exceed their bound",
        ));
    }
    let mut roots = RootCertStore::empty();
    for der in roots_der {
        roots.add(CertificateDer::from(der.clone())).map_err(|_| {
            HttpError::before(
                io::ErrorKind::InvalidData,
                "invalid explicit TLS trust root",
            )
        })?;
    }
    let mut config =
        ClientConfig::builder_with_details(crate::tls::provider(), Arc::new(crate::tls::HostTime))
            .with_safe_default_protocol_versions()
            .map_err(|_| {
                HttpError::before(io::ErrorKind::InvalidData, "TLS protocol setup failed")
            })?
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

fn build_http_call(
    request: &HttpRequest,
) -> Result<Call<ureq_proto::client::state::Prepare>, HttpError> {
    let body_len = request.body.exact_len().to_string();
    let mut builder = ureq_proto::http::Request::builder()
        .method(request.method.as_str())
        .uri(request.url.as_str());
    for header in &request.headers {
        let value = ureq_proto::http::HeaderValue::from_bytes(&header.value).map_err(|_| {
            HttpError::before(
                io::ErrorKind::InvalidInput,
                "invalid HTTP request header value",
            )
        })?;
        builder = builder.header(header.name.as_str(), value);
    }
    builder = builder
        .header("content-length", body_len)
        .header("connection", "close")
        .header("accept-encoding", "identity");
    let request = builder.body(()).map_err(|_| {
        HttpError::before(io::ErrorKind::InvalidInput, "invalid HTTP request metadata")
    })?;
    let mut call = Call::new(request)
        .map_err(|_| HttpError::before(io::ErrorKind::InvalidInput, "invalid HTTP request"))?;
    call.force_send_body();
    Ok(call)
}

fn encode_request_head(
    call: Call<ureq_proto::client::state::Prepare>,
    maximum: usize,
) -> Result<(Zeroizing<Vec<u8>>, Call<ureq_proto::client::state::SendRequest>), HttpError> {
    let mut request = call.proceed();
    let mut encoded = Zeroizing::new(Vec::new());
    encoded
        .try_reserve_exact(maximum)
        .map_err(|_| {
            HttpError::before(
                io::ErrorKind::OutOfMemory,
                "HTTP request head allocation failed",
            )
        })?;
    let mut buffer = Zeroizing::new([0u8; IO_BUFFER_BYTES]);
    while !request.can_proceed() {
        let count = request.write(&mut buffer[..]).map_err(|_| {
            HttpError::before(io::ErrorKind::InvalidData, "invalid HTTP request framing")
        })?;
        if count == 0 || encoded.len().saturating_add(count) > maximum {
            return Err(HttpError::before(
                io::ErrorKind::InvalidInput,
                "HTTP request headers exceed their bound",
            ));
        }
        encoded.extend_from_slice(&buffer[..count]);
    }
    Ok((encoded, request))
}

fn receive_response(
    mut stream: TlsStream,
    mut response: Call<ureq_proto::client::state::RecvResponse>,
    mut incoming: Vec<u8>,
    limits: Limits,
    cancellation: NetworkCancellation,
    absolute_deadline: MonotonicDeadline,
) -> Result<HttpResponse, HttpError> {
    let mut consumed_headers = 0usize;
    let mut buffer = [0u8; IO_BUFFER_BYTES];
    let (head_bytes, head) = loop {
        check_response_policy(&cancellation, absolute_deadline)?;
        let (used, parsed) = response.try_response(&incoming, false).map_err(|_| {
            HttpError::ambiguous(io::ErrorKind::InvalidData, "invalid HTTP response headers")
        })?;
        consumed_headers = consumed_headers.saturating_add(used);
        if consumed_headers > limits.response_header_bytes {
            return Err(HttpError::ambiguous(
                io::ErrorKind::InvalidData,
                "HTTP response headers exceed their bound",
            ));
        }
        if let Some(head) = parsed {
            break (used, head);
        }
        if used != 0 {
            incoming.drain(..used);
            continue;
        }
        let buffered = consumed_headers.saturating_add(incoming.len());
        if buffered >= limits.response_header_bytes {
            return Err(HttpError::ambiguous(
                io::ErrorKind::InvalidData,
                "HTTP response headers exceed their bound",
            ));
        }
        let capacity = buffer
            .len()
            .min(limits.response_header_bytes.saturating_sub(buffered));
        let received = stream
            .read(&mut buffer[..capacity])
            .map_err(|error| HttpError::ambiguous(error.kind(), "HTTP response read failed"))?;
        if received == 0 {
            return Err(HttpError::ambiguous(
                io::ErrorKind::UnexpectedEof,
                "HTTP response headers ended early",
            ));
        }
        incoming.extend_from_slice(&buffer[..received]);
    };

    check_response_policy(&cancellation, absolute_deadline)?;
    let status = head.status().as_u16();
    validate_response_framing(
        head.headers(),
        status,
        consumed_headers,
        limits.response_body_bytes,
        limits.response_body_wire_bytes,
    )?;
    let headers = head
        .headers()
        .iter()
        .map(|(name, value)| Header {
            name: name.as_str().to_owned(),
            value: value.as_bytes().to_vec(),
            sensitive: false,
        })
        .collect();
    incoming.drain(..head_bytes);
    if incoming.len() as u128 > limits.response_body_wire_bytes as u128 {
        return Err(HttpError::ambiguous(
            io::ErrorKind::InvalidData,
            "HTTP response wire body exceeds its bound",
        ));
    }
    match response.proceed() {
        Some(RecvResponseResult::RecvBody(recv)) => Ok(HttpResponse {
            status,
            headers,
            body: ResponseBody::streaming(
                stream,
                recv,
                incoming,
                limits.response_body_bytes,
                limits.response_body_wire_bytes,
                limits.response_header_bytes,
                cancellation,
                absolute_deadline,
            ),
        }),
        Some(RecvResponseResult::Cleanup(_)) | Some(RecvResponseResult::Redirect(_)) => {
            Ok(HttpResponse {
                status,
                headers,
                body: ResponseBody::empty(),
            })
        }
        None => Err(HttpError::ambiguous(
            io::ErrorKind::InvalidData,
            "incomplete HTTP response state",
        )),
    }
}

fn check_response_policy(
    cancellation: &NetworkCancellation,
    absolute_deadline: MonotonicDeadline,
) -> Result<(), HttpError> {
    if cancellation.is_cancelled() {
        return Err(HttpError::ambiguous(
            io::ErrorKind::Interrupted,
            "HTTP response operation was cancelled",
        ));
    }
    if absolute_deadline.has_elapsed() {
        return Err(HttpError::ambiguous(
            io::ErrorKind::TimedOut,
            "HTTP response operation deadline elapsed",
        ));
    }
    Ok(())
}

fn validate_response_framing(
    headers: &ureq_proto::http::HeaderMap,
    status: u16,
    header_len: usize,
    max_body: u64,
    max_wire: u64,
) -> Result<(), HttpError> {
    if status == 101 {
        return Err(HttpError::ambiguous(
            io::ErrorKind::InvalidData,
            "HTTP protocol switching is unsupported",
        ));
    }
    if header_len == 0 {
        return Err(HttpError::ambiguous(
            io::ErrorKind::InvalidData,
            "HTTP response header framing is empty",
        ));
    }
    let content_lengths = headers.get_all("content-length");
    let transfer_encodings = headers.get_all("transfer-encoding");
    if content_lengths.iter().next().is_some() && transfer_encodings.iter().next().is_some() {
        return Err(HttpError::ambiguous(
            io::ErrorKind::InvalidData,
            "HTTP response has conflicting body framing",
        ));
    }
    if transfer_encodings.iter().count() > 1
        || transfer_encodings.iter().any(|value| {
            !value
                .to_str()
                .is_ok_and(|encoding| encoding.eq_ignore_ascii_case("chunked"))
        })
    {
        return Err(HttpError::ambiguous(
            io::ErrorKind::InvalidData,
            "HTTP response uses unsupported transfer encoding",
        ));
    }
    if content_lengths.iter().count() > 1 {
        return Err(HttpError::ambiguous(
            io::ErrorKind::InvalidData,
            "HTTP response repeats Content-Length",
        ));
    }
    if let Some(value) = content_lengths.iter().next() {
        let length = value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                HttpError::ambiguous(io::ErrorKind::InvalidData, "invalid HTTP Content-Length")
            })?;
        if length > max_body || length > max_wire {
            return Err(HttpError::ambiguous(
                io::ErrorKind::InvalidData,
                "HTTP response body exceeds its bound",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Deadlines, Limits};
    use lillux::time::Duration;

    fn request(body: Vec<u8>) -> HttpRequest {
        HttpRequest {
            method: "POST".to_owned(),
            url: url::Url::parse("https://api.example.invalid/v1/operation").unwrap(),
            headers: vec![Header::new("content-type", "application/json")],
            body: RequestBodySource::from_bytes(body),
            tls_roots_der: vec![],
            limits: Limits::control_plane(),
            deadlines: Deadlines::new(
                Duration::from_secs(5),
                Duration::from_secs(2),
                MonotonicDeadline::after(Duration::from_secs(10)),
            ),
            cancellation: lillux::network::NetworkCancellation::default(),
        }
    }

    #[test]
    fn request_uses_derived_content_length_and_transport_owned_headers() {
        let request = request(b"{\"x\":1}".to_vec());
        let (head, send) = encode_request_head(build_http_call(&request).unwrap(), 4096).unwrap();
        let text = std::str::from_utf8(&head).unwrap().to_ascii_lowercase();
        assert!(text.starts_with("post /v1/operation http/1.1\r\n"));
        assert!(text.contains("content-length: 7\r\n"));
        assert!(text.contains("connection: close\r\n"));
        assert!(text.contains("accept-encoding: identity\r\n"));
        assert!(send.can_proceed());
    }

    #[test]
    fn request_limits_and_transport_headers_refuse_before_contact() {
        let mut oversized_request = request(vec![b'x'; 8]);
        oversized_request.limits.request_body_bytes = 7;
        assert_eq!(
            validate(&oversized_request).unwrap_err().contact_state(),
            crate::ContactState::NoRequestSent
        );

        let mut restricted_request = request(vec![]);
        restricted_request
            .headers
            .push(Header::new("connection", "keep-alive"));
        assert_eq!(
            validate(&restricted_request).unwrap_err().contact_state(),
            crate::ContactState::NoRequestSent
        );
    }

    #[test]
    fn configured_byte_ceilings_cannot_exceed_transport_maxima() {
        let mut oversized_request_body = request(vec![]);
        oversized_request_body.limits.request_body_bytes = MAX_REQUEST_BODY_BYTES + 1;
        assert_eq!(
            validate(&oversized_request_body)
                .unwrap_err()
                .contact_state(),
            crate::ContactState::NoRequestSent
        );

        let mut oversized_request_headers = request(vec![]);
        oversized_request_headers.limits.request_header_bytes = MAX_REQUEST_HEADER_BYTES + 1;
        assert_eq!(
            validate(&oversized_request_headers)
                .unwrap_err()
                .contact_state(),
            crate::ContactState::NoRequestSent
        );

        let mut oversized_response_headers = request(vec![]);
        oversized_response_headers.limits.response_header_bytes = MAX_RESPONSE_HEADER_BYTES + 1;
        assert_eq!(
            validate(&oversized_response_headers)
                .unwrap_err()
                .contact_state(),
            crate::ContactState::NoRequestSent
        );

        let mut oversized_response_body = request(vec![]);
        oversized_response_body.limits.response_body_bytes = MAX_RESPONSE_BODY_BYTES + 1;
        assert_eq!(
            validate(&oversized_response_body)
                .unwrap_err()
                .contact_state(),
            crate::ContactState::NoRequestSent
        );

        let mut oversized_response_wire_body = request(vec![]);
        oversized_response_wire_body.limits.response_body_wire_bytes =
            MAX_RESPONSE_BODY_WIRE_BYTES + 1;
        assert_eq!(
            validate(&oversized_response_wire_body)
                .unwrap_err()
                .contact_state(),
            crate::ContactState::NoRequestSent
        );
    }

    #[test]
    fn declared_response_body_over_bound_is_ambiguous_after_request() {
        let mut headers = ureq_proto::http::HeaderMap::new();
        headers.insert(
            "content-length",
            (MAX_RESPONSE_BODY_BYTES + 1).to_string().parse().unwrap(),
        );
        let error = validate_response_framing(
            &headers,
            200,
            20,
            MAX_RESPONSE_BODY_BYTES,
            MAX_RESPONSE_BODY_WIRE_BYTES,
        )
        .unwrap_err();
        assert_eq!(
            error.contact_state(),
            crate::ContactState::RequestMayHaveBeenSent
        );
    }

    #[test]
    fn sensitive_header_debug_redacts_value() {
        let header = Header::new_sensitive(
            "authorization",
            zeroize::Zeroizing::new(b"secret-token".to_vec()),
        );
        let debug = format!("{header:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret-token"));
    }

    /// Exercise the streaming body with a local TLS peer and captured localhost resolution.
    #[cfg(unix)]
    fn streamed_response_read_error(
        response_headers: &'static [u8],
        response_body: &'static [u8],
        decoded_limit: u64,
        wire_limit: u64,
    ) -> io::ErrorKind {
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::time::Duration as StdDuration;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release_body, wait_for_release) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(StdDuration::from_secs(5)))
                .unwrap();
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    include_bytes!("../tests/fixtures/test-server.der").to_vec(),
                )],
                rustls::pki_types::PrivateKeyDer::Pkcs8(
                    rustls::pki_types::PrivatePkcs8KeyDer::from(
                        include_bytes!("../tests/fixtures/test-server-key.der").to_vec(),
                    ),
                ),
            )
            .unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            let mut tls = rustls::StreamOwned::new(connection, socket);
            let mut request_head = Vec::new();
            loop {
                assert!(
                    request_head.len() < 64 * 1024,
                    "test request headers exceeded bound"
                );
                let mut buffer = [0u8; 1024];
                let received = tls.read(&mut buffer).unwrap();
                assert_ne!(received, 0, "test request ended before its headers");
                request_head.extend_from_slice(&buffer[..received]);
                if request_head.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                    break;
                }
            }
            tls.write_all(response_headers).unwrap();
            tls.flush().unwrap();
            wait_for_release
                .recv_timeout(StdDuration::from_secs(10))
                .unwrap();
            tls.write_all(response_body).unwrap();
            tls.flush().unwrap();
        });

        let network =
            NetworkContext::from_config_bytes(b"nameserver 127.0.0.1\n", b"127.0.0.1 localhost\n")
                .unwrap();
        let mut http_request = request(vec![]);
        http_request.url =
            url::Url::parse(&format!("https://localhost:{port}/bounded-response")).unwrap();
        http_request.tls_roots_der = vec![include_bytes!("../tests/fixtures/test-ca.der").to_vec()];
        http_request.limits.response_body_bytes = decoded_limit;
        http_request.limits.response_body_wire_bytes = wire_limit;

        let mut response = execute(&network, http_request).unwrap();
        assert_eq!(response.status, 200);
        release_body.send(()).unwrap();
        let mut delivered = 0u64;
        let mut output = [0u8; 32];
        let error = loop {
            match response.body.read(&mut output) {
                Ok(0) => panic!("stream ended before exceeding its configured body limit"),
                Ok(count) => {
                    delivered = delivered.saturating_add(count as u64);
                    assert!(
                        delivered <= decoded_limit,
                        "transport exposed bytes past the decoded-body limit"
                    );
                }
                Err(error) => break error,
            }
        };
        server.join().unwrap();
        error.kind()
    }

    #[cfg(unix)]
    #[test]
    fn streamed_chunked_response_wire_overflow_is_reported_as_read_error() {
        let error = streamed_response_read_error(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
            b"5\r\nhello\r\n0\r\n\r\n",
            64,
            8,
        );
        assert_eq!(error, io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn streamed_close_delimited_response_decoded_overflow_is_reported_as_read_error() {
        let error = streamed_response_read_error(
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n",
            b"hello",
            4,
            64,
        );
        assert_eq!(error, io::ErrorKind::InvalidData);
    }

    #[test]
    fn response_conflicting_framing_is_ambiguous_after_request() {
        let mut headers = ureq_proto::http::HeaderMap::new();
        headers.insert("content-length", "4".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        let error = validate_response_framing(&headers, 200, 20, 100, 200).unwrap_err();
        assert_eq!(
            error.contact_state(),
            crate::ContactState::RequestMayHaveBeenSent
        );
    }
}
