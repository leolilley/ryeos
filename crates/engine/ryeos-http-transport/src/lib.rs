//! Bounded HTTP/1.1 over explicit Lillux network and TLS authorities.
//!
//! This crate does not choose provider routes, credential policy, or retry
//! behavior. Callers must authorize the URL and provide the exact TLS roots
//! for that authority. It never follows redirects or discovers proxies, roots,
//! credentials, or endpoints from the process environment.

mod http1;
mod sse;
mod tls;

use lillux::network::NetworkContext;
use lillux::time::{Duration, MonotonicDeadline};
use std::io;

pub use http1::{Header, HttpRequest, HttpResponse, RequestBodySource, ResponseBody};
pub use sse::{SseEvent, SseLimits, SseReader};

/// Whether the HTTP mutator may have reached the remote peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContactState {
    /// No HTTP request bytes were submitted to the TLS stream.
    NoRequestSent,
    /// Request transmission began; remote commit is unknown.
    RequestMayHaveBeenSent,
}

/// Redacted transport failure that retains remote-contact state.
#[derive(Debug)]
pub struct HttpError {
    contact: ContactState,
    kind: io::ErrorKind,
    message: &'static str,
}

impl HttpError {
    pub(crate) fn before(kind: io::ErrorKind, message: &'static str) -> Self {
        Self {
            contact: ContactState::NoRequestSent,
            kind,
            message,
        }
    }

    pub(crate) fn ambiguous(kind: io::ErrorKind, message: &'static str) -> Self {
        Self {
            contact: ContactState::RequestMayHaveBeenSent,
            kind,
            message,
        }
    }

    /// Get the request's remote-contact classification.
    pub fn contact_state(&self) -> ContactState {
        self.contact
    }

    /// Get the underlying I/O error category without exposing transport data.
    pub fn kind(&self) -> io::ErrorKind {
        self.kind
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for HttpError {}

/// Immutable operation deadlines and the per-I/O idle bound.
#[derive(Clone, Copy, Debug)]
pub struct Deadlines {
    /// Shared DNS, TCP connect, and TLS setup window.
    pub setup_timeout: Duration,
    /// Maximum silence for one underlying network I/O operation.
    pub idle_timeout: Duration,
    /// Absolute request deadline. Progress never extends it.
    pub absolute: MonotonicDeadline,
}

impl Deadlines {
    /// Construct a deadline policy from Lillux clock authorities.
    pub fn new(
        setup_timeout: Duration,
        idle_timeout: Duration,
        absolute: MonotonicDeadline,
    ) -> Self {
        Self {
            setup_timeout,
            idle_timeout,
            absolute,
        }
    }
}

/// Independent byte ceilings for one exchange.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum exact-length request body bytes.
    pub request_body_bytes: u64,
    /// Maximum serialized request line and header bytes.
    pub request_header_bytes: usize,
    /// Maximum cumulative response status line and header bytes.
    pub response_header_bytes: usize,
    /// Maximum cumulative decoded response body bytes.
    pub response_body_bytes: u64,
    /// Maximum transferred response-body bytes, including HTTP chunk framing.
    pub response_body_wire_bytes: u64,
}

impl Limits {
    /// Finite defaults suitable for small control-plane requests.
    pub const fn control_plane() -> Self {
        Self {
            request_body_bytes: 1024 * 1024,
            request_header_bytes: 64 * 1024,
            response_header_bytes: 64 * 1024,
            response_body_bytes: 1024 * 1024,
            response_body_wire_bytes: 2 * 1024 * 1024,
        }
    }
}

/// A bounded HTTPS exchange with explicit network inputs, roots and budgets.
pub struct HttpClient {
    network: NetworkContext,
}

impl HttpClient {
    /// Create a client over captured Lillux resolver/hosts inputs.
    pub fn new(network: NetworkContext) -> Self {
        Self { network }
    }

    /// Execute one request. This method performs no retry and returns a
    /// streaming body; callers must keep their operation pending if its read
    /// fails after `RequestMayHaveBeenSent`.
    pub fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        http1::execute(&self.network, request)
    }
}
