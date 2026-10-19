//! Guest half of the occurrence channel stream: one WebSocket to the
//! controller's HTTPS origin carrying the same exchange as the
//! request/response route.
//!
//! Each outbound message is exactly one canonical signed supervisor frame and
//! each inbound message is the canonical exchange response for it, so the
//! transport driver runs unchanged. The socket carries no credential of its
//! own: the controller admits it only once its first message verifies against
//! the occurrence binding. The connection is opened lazily, right before a
//! frame is sent, and is discarded after any failure; the next exchange dials
//! again and the driver presents the exact retained bytes.
//!
//! Each exchange ends by its caller's deadline: dialing, the handshake and
//! every read and write on the socket are bounded by it as well as by the
//! socket's own bounds. After a retryable failure the next dial waits a
//! bounded, growing interval, so a held place or a busy controller is not
//! redialed in a tight loop.

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_external_execution::transport::{
    ExternalExecutionChannelTransport, ExternalTransportExchange,
    ExternalTransportStepFailure as Failure,
};
use ryeos_http_transport::{Deadlines, TlsStream, open_tls_stream};
use ryeos_state::external_execution::ExecutionChannelBinding;
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_STREAM_CONTRACT, ExternalCapturedNetworkInputs, ExternalSupervisorBootstrap,
    MAX_EXTERNAL_CHANNEL_STREAM_OUTBOUND_BYTES, decode_external_channel_stream_response,
    external_channel_stream_inbound_frame,
};
use tungstenite::protocol::WebSocketConfig;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::{Message, WebSocket};

/// Route path of the stream under the controller's HTTPS origin; the two
/// occurrence coordinates fill the middle segments.
const STREAM_PATH_PREFIX: &str = "/external-execution/channel";
const STREAM_PATH_SUFFIX: &str = "stream";

/// Wait before the first redial after a retryable failure.
const FIRST_BACKOFF: Duration = Duration::from_millis(100);
/// Longest wait between redials.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// The wait before the next redial, doubling from the first and capped.
fn next_backoff(previous: Duration) -> Duration {
    if previous.is_zero() {
        FIRST_BACKOFF
    } else {
        previous.saturating_mul(2).min(MAX_BACKOFF)
    }
}

/// The guest's stream to one occurrence channel. It owns at most one live
/// socket; nothing is shared with the request/response transport.
pub struct AttachedExternalExecutionStream {
    network: NetworkContext,
    tls_roots_der: Vec<Vec<u8>>,
    origin: url::Url,
    stream_url: String,
    setup_timeout: Duration,
    request_timeout: Duration,
    maximum_response_bytes: usize,
    lifetime: MonotonicDeadline,
    socket: Option<WebSocket<TlsStream>>,
    backoff: Duration,
    redial_after: Option<MonotonicDeadline>,
}

/// Build the stream for an already durably retained exact binding. This
/// contacts nothing; the first exchange dials.
pub fn connect_external_execution_stream(
    bootstrap: &ExternalSupervisorBootstrap,
    supervisor_signing_key: &lillux::crypto::SigningKey,
    binding: &ExecutionChannelBinding,
    network_inputs: &ExternalCapturedNetworkInputs,
) -> Result<AttachedExternalExecutionStream> {
    bootstrap.validate_attached_binding(
        binding,
        &ryeos_state::external_execution::encode_channel_public_key(
            &supervisor_signing_key.verifying_key(),
        )?,
    )?;
    let http = bootstrap.controller.http()?;
    network_inputs.validate_for(&http.network_inputs)?;
    let network = NetworkContext::from_config_bytes(
        &network_inputs.resolver_bytes()?,
        &network_inputs.hosts_bytes()?,
    )?;
    let tls_roots_der = bootstrap
        .tls_root_certificates_der_base64
        .iter()
        .map(|encoded| {
            STANDARD
                .decode(encoded)
                .context("decode admitted external TLS root")
        })
        .collect::<Result<Vec<_>>>()?;
    let now = lillux::time::timestamp_millis();
    let remaining_ms = binding
        .expires_at_ms
        .checked_sub(now)
        .filter(|remaining| *remaining > 0)
        .context("external channel stream binding has already expired")?;
    assemble(
        StreamEndpoint {
            network,
            tls_roots_der,
            https_origin: &http.https_origin,
            placement_thread_id: &bootstrap.placement_thread_id,
            occurrence_id: &bootstrap.occurrence_id,
        },
        StreamBounds {
            setup_timeout: Duration::from_millis(u64::from(http.connect_timeout_ms)),
            request_timeout: Duration::from_millis(u64::from(http.request_timeout_ms)),
            maximum_response_bytes: http.maximum_response_bytes,
            lifetime: Duration::from_millis(u64::try_from(remaining_ms)?),
        },
    )
}

/// Where a stream dials and what it trusts there.
struct StreamEndpoint<'a> {
    network: NetworkContext,
    tls_roots_der: Vec<Vec<u8>>,
    https_origin: &'a str,
    placement_thread_id: &'a str,
    occurrence_id: &'a str,
}

/// Time and size bounds of one stream.
#[derive(Clone, Copy)]
struct StreamBounds {
    setup_timeout: Duration,
    request_timeout: Duration,
    maximum_response_bytes: u64,
    lifetime: Duration,
}

fn assemble(
    endpoint: StreamEndpoint<'_>,
    bounds: StreamBounds,
) -> Result<AttachedExternalExecutionStream> {
    let origin =
        url::Url::parse(endpoint.https_origin).context("parse external controller origin")?;
    let stream_url = stream_url(
        &origin,
        endpoint.placement_thread_id,
        endpoint.occurrence_id,
    )?;
    ensure!(
        !bounds.setup_timeout.is_zero()
            && !bounds.request_timeout.is_zero()
            && bounds.maximum_response_bytes > 0
            && !bounds.lifetime.is_zero(),
        "external channel stream bounds must be nonzero"
    );
    Ok(AttachedExternalExecutionStream {
        network: endpoint.network,
        tls_roots_der: endpoint.tls_roots_der,
        origin,
        stream_url,
        setup_timeout: bounds.setup_timeout,
        request_timeout: bounds.request_timeout,
        maximum_response_bytes: usize::try_from(bounds.maximum_response_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_EXTERNAL_CHANNEL_STREAM_OUTBOUND_BYTES),
        lifetime: MonotonicDeadline::after(bounds.lifetime),
        socket: None,
        backoff: Duration::ZERO,
        redial_after: None,
    })
}

/// Repository-composed transport tests reach a loopback controller without a
/// sealed bootstrap. Not built into production unless `test-support` is
/// selected.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;

    /// Everything a loopback stream needs in place of a sealed bootstrap.
    pub struct LoopbackStream {
        pub https_origin: String,
        pub placement_thread_id: String,
        pub occurrence_id: String,
        pub tls_roots_der: Vec<Vec<u8>>,
        pub network: NetworkContext,
        pub request_timeout: Duration,
        pub maximum_response_bytes: u64,
        pub lifetime: Duration,
    }

    pub fn stream(spec: LoopbackStream) -> Result<AttachedExternalExecutionStream> {
        assemble(
            StreamEndpoint {
                network: spec.network,
                tls_roots_der: spec.tls_roots_der,
                https_origin: &spec.https_origin,
                placement_thread_id: &spec.placement_thread_id,
                occurrence_id: &spec.occurrence_id,
            },
            StreamBounds {
                setup_timeout: spec.request_timeout,
                request_timeout: spec.request_timeout,
                maximum_response_bytes: spec.maximum_response_bytes,
                lifetime: spec.lifetime,
            },
        )
    }
}

/// The stream URL for one occurrence. Route captures are compared as raw path
/// segments, so an identifier that would need percent-encoding is refused
/// here rather than sent in a form the controller cannot match.
fn stream_url(origin: &url::Url, placement_thread_id: &str, occurrence_id: &str) -> Result<String> {
    for identifier in [placement_thread_id, occurrence_id] {
        ensure!(
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)),
            "external channel stream identifier is not a plain path segment"
        );
    }
    ensure!(
        origin.scheme() == "https" && origin.path() == "/",
        "external channel stream requires a canonical HTTPS origin"
    );
    let host = origin
        .host_str()
        .context("external controller origin has no host")?;
    let authority = match origin.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    Ok(format!(
        "wss://{authority}{STREAM_PATH_PREFIX}/{placement_thread_id}/{occurrence_id}/{STREAM_PATH_SUFFIX}"
    ))
}

/// How a refusal the controller names on close is treated: a capacity or
/// slot refusal, or a server error that reached no verdict, is worth trying
/// again later; every other named refusal is a verdict on this channel.
fn close_failure(code: CloseCode, reason: &str) -> Failure {
    let named = if reason.is_empty() { "unnamed" } else { reason };
    match code {
        CloseCode::Again | CloseCode::Error => Failure::AmbiguousTransport(anyhow::anyhow!(
            "external channel stream asked to try again: {named}"
        )),
        CloseCode::Policy | CloseCode::Unsupported | CloseCode::Size | CloseCode::Invalid => {
            Failure::Fatal(anyhow::anyhow!(
                "external channel stream refused by the controller: {named}"
            ))
        }
        _ => Failure::AmbiguousTransport(anyhow::anyhow!(
            "external channel stream closed by the controller: {named}"
        )),
    }
}

/// How a refused upgrade is treated. The controller refuses an upgrade by
/// HTTP status before any frame crosses; only capacity is temporary.
fn upgrade_status_failure(status: u16) -> Failure {
    match status {
        429 | 503 => Failure::AmbiguousTransport(anyhow::anyhow!(
            "external channel stream upgrade deferred by the controller (HTTP {status})"
        )),
        _ => Failure::Fatal(anyhow::anyhow!(
            "external channel stream upgrade refused by the controller (HTTP {status})"
        )),
    }
}

impl AttachedExternalExecutionStream {
    fn socket_config(&self) -> WebSocketConfig {
        WebSocketConfig {
            max_message_size: Some(self.maximum_response_bytes),
            max_frame_size: Some(self.maximum_response_bytes),
            ..WebSocketConfig::default()
        }
    }

    fn dial(&mut self, deadline: MonotonicDeadline) -> std::result::Result<(), Failure> {
        if self.socket.is_some() {
            return Ok(());
        }
        let setup_timeout = self.setup_timeout.min(deadline.remaining());
        if setup_timeout.is_zero() {
            return Err(Failure::AmbiguousTransport(anyhow::anyhow!(
                "external channel stream deadline elapsed before dialing"
            )));
        }
        let deadlines = Deadlines::new(setup_timeout, self.request_timeout, self.lifetime);
        let mut tls = open_tls_stream(
            &self.network,
            &self.origin,
            &self.tls_roots_der,
            deadlines,
            NetworkCancellation::default(),
        )
        .map_err(|error| {
            // Nothing has been sent. A trust failure is a verdict on the
            // controller; anything else is a connection that may succeed later.
            let cause = anyhow::anyhow!("open external channel stream: {error}");
            if error.kind() == std::io::ErrorKind::InvalidData
                || error.kind() == std::io::ErrorKind::InvalidInput
            {
                Failure::Fatal(cause)
            } else {
                Failure::AmbiguousTransport(cause)
            }
        })?;
        let mut request =
            tungstenite::client::IntoClientRequest::into_client_request(self.stream_url.as_str())
                .map_err(|error| {
                Failure::Fatal(anyhow::anyhow!(
                    "compose external channel stream request: {error}"
                ))
            })?;
        request.headers_mut().insert(
            "sec-websocket-protocol",
            tungstenite::http::HeaderValue::from_static(EXTERNAL_CHANNEL_STREAM_CONTRACT),
        );
        tls.sock.bound_operation(Some(deadline));
        let (socket, response) =
            match tungstenite::client::client_with_config(request, tls, Some(self.socket_config()))
            {
                Ok(established) => established,
                Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(response))) => {
                    return Err(upgrade_status_failure(response.status().as_u16()));
                }
                Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Protocol(error))) => {
                    return Err(Failure::Fatal(anyhow::anyhow!(
                        "external channel stream handshake violated the protocol: {error}"
                    )));
                }
                Err(error) => {
                    return Err(Failure::AmbiguousTransport(anyhow::anyhow!(
                        "external channel stream handshake failed: {error}"
                    )));
                }
            };
        let selected = response
            .headers()
            .get("sec-websocket-protocol")
            .map(|value| value.as_bytes());
        if selected != Some(EXTERNAL_CHANNEL_STREAM_CONTRACT.as_bytes()) {
            return Err(Failure::Fatal(anyhow::anyhow!(
                "external channel stream controller did not select {EXTERNAL_CHANNEL_STREAM_CONTRACT}"
            )));
        }
        self.socket = Some(socket);
        Ok(())
    }

    fn send_and_receive(
        &mut self,
        frame: &[u8],
        deadline: MonotonicDeadline,
    ) -> std::result::Result<Vec<u8>, Failure> {
        self.dial(deadline)?;
        let socket = self
            .socket
            .as_mut()
            .ok_or_else(|| Failure::Fatal(anyhow::anyhow!("external channel stream is absent")))?;
        socket.get_mut().sock.bound_operation(Some(deadline));
        socket
            .send(Message::Binary(frame.to_vec()))
            .map_err(|error| {
                Failure::AmbiguousTransport(anyhow::anyhow!(
                    "send external channel stream frame: {error}"
                ))
            })?;
        loop {
            match socket.read() {
                Ok(Message::Binary(bytes)) => return Ok(bytes),
                Ok(Message::Ping(_) | Message::Pong(_)) => {
                    if deadline.has_elapsed() {
                        return Err(Failure::AmbiguousTransport(anyhow::anyhow!(
                            "external channel stream deadline elapsed before its response"
                        )));
                    }
                }
                Ok(Message::Close(close)) => {
                    return Err(match close {
                        Some(close) => close_failure(close.code, &close.reason),
                        None => Failure::AmbiguousTransport(anyhow::anyhow!(
                            "external channel stream closed without a reason"
                        )),
                    });
                }
                Ok(Message::Text(_) | Message::Frame(_)) => {
                    return Err(Failure::Fatal(anyhow::anyhow!(
                        "external channel stream answered with a non-binary message"
                    )));
                }
                Err(tungstenite::Error::Capacity(error)) => {
                    return Err(Failure::Fatal(anyhow::anyhow!(
                        "external channel stream response exceeds its bound: {error}"
                    )));
                }
                Err(error) => {
                    return Err(Failure::AmbiguousTransport(anyhow::anyhow!(
                        "receive external channel stream response: {error}"
                    )));
                }
            }
        }
    }
}

impl ExternalExecutionChannelTransport for AttachedExternalExecutionStream {
    fn exchange(&mut self, canonical_supervisor_frame: &[u8]) -> Result<ExternalTransportExchange> {
        self.exchange_until(
            canonical_supervisor_frame,
            MonotonicDeadline::after(self.request_timeout),
        )
        .map_err(anyhow::Error::new)
    }

    fn exchange_until(
        &mut self,
        canonical_supervisor_frame: &[u8],
        deadline: MonotonicDeadline,
    ) -> std::result::Result<ExternalTransportExchange, Failure> {
        external_channel_stream_inbound_frame(canonical_supervisor_frame)
            .map_err(Failure::Fatal)?;
        let deadline = deadline.min(self.lifetime);
        if deadline.has_elapsed() {
            return Err(Failure::AmbiguousTransport(anyhow::anyhow!(
                "external channel stream deadline elapsed before contact"
            )));
        }
        if let Some(redial_after) = self.redial_after {
            let wait = redial_after.remaining();
            if wait >= deadline.remaining() {
                lillux::time::sleep(deadline.remaining());
                return Err(Failure::AmbiguousTransport(anyhow::anyhow!(
                    "external channel stream deadline elapsed while waiting to redial"
                )));
            }
            lillux::time::sleep(wait);
            self.redial_after = None;
        }
        let message = match self.send_and_receive(canonical_supervisor_frame, deadline) {
            Ok(message) => message,
            Err(failure) => {
                // A socket that failed once is never reused: its framing
                // state is unknown. The next exchange dials a fresh one, after
                // a wait when the failure may clear by itself.
                self.socket = None;
                if matches!(failure, Failure::AmbiguousTransport(_)) {
                    self.backoff = next_backoff(self.backoff);
                    self.redial_after = Some(MonotonicDeadline::after(self.backoff));
                }
                return Err(failure);
            }
        };
        self.backoff = Duration::ZERO;
        if let Some(socket) = self.socket.as_mut() {
            socket.get_mut().sock.bound_operation(None);
        }
        if message.len() > self.maximum_response_bytes {
            self.socket = None;
            return Err(Failure::Fatal(anyhow::anyhow!(
                "external channel stream response exceeds its negotiated bound"
            )));
        }
        let response = decode_external_channel_stream_response(&message).map_err(|error| {
            self.socket = None;
            Failure::Fatal(error.context("decode external channel stream response"))
        })?;
        crate::exchange_from_response(response).map_err(Failure::Fatal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_url_names_the_occurrence_under_the_origin() {
        let origin = url::Url::parse("https://controller.example:8443").unwrap();
        assert_eq!(
            stream_url(&origin, "T-placement", "occ-0123").unwrap(),
            "wss://controller.example:8443/external-execution/channel/T-placement/occ-0123/stream"
        );
        let origin = url::Url::parse("https://controller.example").unwrap();
        assert_eq!(
            stream_url(&origin, "T-placement", "occ-0123").unwrap(),
            "wss://controller.example/external-execution/channel/T-placement/occ-0123/stream"
        );
    }

    #[test]
    fn identifiers_that_need_encoding_are_refused_before_contact() {
        let origin = url::Url::parse("https://controller.example").unwrap();
        for (placement, occurrence) in [
            ("T/placement", "occ"),
            ("T-placement", "occ?x"),
            ("T-placement", ""),
            ("T placement", "occ"),
            ("T-placement", "occ%2F"),
        ] {
            assert!(stream_url(&origin, placement, occurrence).is_err());
        }
        let not_https = url::Url::parse("http://controller.example").unwrap();
        assert!(stream_url(&not_https, "T-placement", "occ").is_err());
    }

    #[test]
    fn redial_waits_grow_from_the_first_and_stop_at_the_cap() {
        assert_eq!(next_backoff(Duration::ZERO), FIRST_BACKOFF);
        assert_eq!(next_backoff(FIRST_BACKOFF), FIRST_BACKOFF * 2);
        let mut wait = Duration::ZERO;
        for _ in 0..32 {
            wait = next_backoff(wait);
            assert!(wait <= MAX_BACKOFF);
        }
        assert_eq!(wait, MAX_BACKOFF);
    }

    #[test]
    fn named_refusals_are_fatal_and_capacity_is_retried() {
        assert!(matches!(
            close_failure(
                CloseCode::Policy,
                "external_channel_stream_frame_not_authenticated"
            ),
            Failure::Fatal(_)
        ));
        assert!(matches!(
            close_failure(CloseCode::Size, "external_channel_stream_message_bound"),
            Failure::Fatal(_)
        ));
        assert!(matches!(
            close_failure(CloseCode::Again, "external_channel_stream_already_attached"),
            Failure::AmbiguousTransport(_)
        ));
        assert!(matches!(
            close_failure(
                CloseCode::Error,
                "external_channel_stream_exchange_unavailable"
            ),
            Failure::AmbiguousTransport(_)
        ));
        assert!(matches!(
            close_failure(CloseCode::Away, ""),
            Failure::AmbiguousTransport(_)
        ));
        assert!(matches!(
            upgrade_status_failure(503),
            Failure::AmbiguousTransport(_)
        ));
        assert!(matches!(upgrade_status_failure(404), Failure::Fatal(_)));
        assert!(matches!(upgrade_status_failure(410), Failure::Fatal(_)));
        let message = format!(
            "{:#}",
            match close_failure(
                CloseCode::Policy,
                "external_channel_stream_exchange_refused"
            ) {
                Failure::Fatal(error) => error,
                Failure::AmbiguousTransport(error) => error,
            }
        );
        assert!(message.contains("external_channel_stream_exchange_refused"));
    }
}
