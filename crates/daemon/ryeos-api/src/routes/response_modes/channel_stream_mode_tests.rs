//! Channel stream tests over real local sockets: the controller half against
//! a raw WebSocket client, and the guest's own transport through a local TLS
//! terminator, as the guest reaches a controller behind a provider edge.

use std::net::SocketAddr;
use std::sync::Mutex;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt as _, StreamExt as _};
use lillux::crypto::SigningKey;
use ryeos_external_execution::transport::{
    ExternalExecutionChannelTransport as _, ExternalTransportStepFailure,
};
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_TRANSPORT_SCHEMA, ExternalChannelResponseFrame,
    decode_external_channel_stream_response,
};
use ryeos_state::external_execution::{
    ChannelDirection, EXECUTION_CHANNEL_BINDING_SCHEMA, ExecutionChannelBinding,
    ExecutionChannelPayload, ExecutionFrame, ExternalExecutionMode, SignedExecutionFrame,
    encode_channel_public_key,
};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use super::*;

const PLACEMENT: &str = "T-stream";
const OCCURRENCE: &str = "occ-stream";

struct Fixture {
    binding: ExecutionChannelBinding,
    owner_key: SigningKey,
    supervisor_key: SigningKey,
}

impl Fixture {
    fn new() -> Self {
        let owner_key = lillux::crypto::generate_signing_key();
        let supervisor_key = lillux::crypto::generate_signing_key();
        let issued_at_ms = lillux::time::timestamp_millis() - 1_000;
        let binding = ExecutionChannelBinding {
            schema: EXECUTION_CHANNEL_BINDING_SCHEMA,
            execution_mode: ExternalExecutionMode::StructuredSession {},
            placement_thread_id: PLACEMENT.into(),
            occurrence_id: OCCURRENCE.into(),
            allocation_request_digest: "a".repeat(64),
            admitted_capsule_hash: "b".repeat(64),
            base_snapshot_hash: "c".repeat(64),
            execution_binding_hash: "d".repeat(64),
            supervisor_runtime_hash: "e".repeat(64),
            candidate_program_digest: "0".repeat(64),
            channel_nonce: "f".repeat(64),
            owner_public_key: encode_channel_public_key(&owner_key.verifying_key()).unwrap(),
            supervisor_public_key: encode_channel_public_key(&supervisor_key.verifying_key())
                .unwrap(),
            issued_at_ms,
            execution_deadline_ms: issued_at_ms + 600_000,
            expires_at_ms: issued_at_ms + 1_200_000,
            candidate_export_max_bytes: 512 * 1024,
            max_frames: 100,
            max_bytes: 1024 * 1024,
        };
        Self {
            binding,
            owner_key,
            supervisor_key,
        }
    }

    fn sign(&self, frame: ExecutionFrame, key: &SigningKey) -> Vec<u8> {
        let signed = SignedExecutionFrame::sign(frame, &self.binding, key).unwrap();
        lillux::canonical_json(&serde_json::to_value(&signed).unwrap())
            .unwrap()
            .into_bytes()
    }

    fn ready_frame(binding: &ExecutionChannelBinding) -> ExecutionFrame {
        ExecutionFrame {
            schema: 1,
            binding_digest: binding.digest().unwrap(),
            direction: ChannelDirection::SupervisorToOwner,
            sequence: 1,
            previous_frame_digest: None,
            acknowledged_peer_sequence: 0,
            payload: ExecutionChannelPayload::Ready {
                supervisor_runtime_hash: binding.supervisor_runtime_hash.clone(),
                base_snapshot_hash: binding.base_snapshot_hash.clone(),
                boot: None,
            },
        }
    }

    fn ready(&self) -> Vec<u8> {
        self.sign(Self::ready_frame(&self.binding), &self.supervisor_key)
    }

    /// A well-formed, correctly signed Ready for a different binding: the
    /// same occurrence coordinates under another supervisor key.
    fn ready_for_another_binding(&self) -> Vec<u8> {
        let stranger = lillux::crypto::generate_signing_key();
        let mut other = self.binding.clone();
        other.supervisor_public_key = encode_channel_public_key(&stranger.verifying_key()).unwrap();
        let signed =
            SignedExecutionFrame::sign(Self::ready_frame(&other), &other, &stranger).unwrap();
        lillux::canonical_json(&serde_json::to_value(&signed).unwrap())
            .unwrap()
            .into_bytes()
    }

    fn owner_bytes_frame(&self) -> Vec<u8> {
        self.sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: self.binding.digest().unwrap(),
                direction: ChannelDirection::OwnerToSupervisor,
                sequence: 1,
                previous_frame_digest: None,
                acknowledged_peer_sequence: 1,
                payload: ExecutionChannelPayload::ProtocolBytes {
                    bytes_base64: STANDARD.encode(b"exec-server hello"),
                },
            },
            &self.owner_key,
        )
    }
}

/// Owner fixture: verifies every frame against the binding exactly as the
/// node does and keeps the exchange idempotent per frame digest.
struct FixtureOwner {
    fixture: Fixture,
    maximum_response_bytes: u64,
    seen: Mutex<Vec<String>>,
    exchanges: Mutex<usize>,
    /// Cleared to stand for a terminated allocation.
    live: std::sync::atomic::AtomicBool,
    /// Cleared to stand for a binding that selects the request/response route.
    selected: std::sync::atomic::AtomicBool,
    /// Set to stand for an occurrence whose lease was durably refused.
    refused: std::sync::atomic::AtomicBool,
    /// Set to make the next exchange record a refusal and fail, as a frame
    /// that refuses the lease does.
    refuse_in_exchange: std::sync::atomic::AtomicBool,
    /// Set to answer every frame as a held one: not new, nothing outbound.
    held: std::sync::atomic::AtomicBool,
    /// Set to hold only the first frame of the channel, as the controller
    /// holds a leased guest's Ready until it has admitted the guest.
    hold_first_frame: std::sync::atomic::AtomicBool,
    /// The number of coming exchanges that reach no verdict, as an adapter
    /// timeout does; each fails with the unavailable context.
    unavailable_exchanges: std::sync::atomic::AtomicUsize,
    exchange_delay: Duration,
}

impl FixtureOwner {
    fn new(fixture: Fixture, maximum_response_bytes: u64) -> Arc<Self> {
        Self::slow(fixture, maximum_response_bytes, Duration::ZERO)
    }

    /// An owner whose every exchange takes `exchange_delay`.
    fn slow(fixture: Fixture, maximum_response_bytes: u64, exchange_delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            fixture,
            maximum_response_bytes,
            seen: Mutex::new(Vec::new()),
            exchanges: Mutex::new(0),
            live: std::sync::atomic::AtomicBool::new(true),
            selected: std::sync::atomic::AtomicBool::new(true),
            refused: std::sync::atomic::AtomicBool::new(false),
            refuse_in_exchange: std::sync::atomic::AtomicBool::new(false),
            held: std::sync::atomic::AtomicBool::new(false),
            hold_first_frame: std::sync::atomic::AtomicBool::new(false),
            unavailable_exchanges: std::sync::atomic::AtomicUsize::new(0),
            exchange_delay,
        })
    }

    fn terminate(&self) {
        self.live.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    fn exchanges(&self) -> usize {
        *self.exchanges.lock().unwrap()
    }

    fn verify(
        &self,
        placement: &str,
        occurrence: &str,
        frame: &[u8],
    ) -> anyhow::Result<ryeos_state::external_execution::AuthenticatedExecutionFrame> {
        anyhow::ensure!(
            placement == self.fixture.binding.placement_thread_id
                && occurrence == self.fixture.binding.occurrence_id,
            "no channel for this occurrence"
        );
        let verified = SignedExecutionFrame::decode_and_verify(
            frame,
            &self.fixture.binding,
            lillux::time::timestamp_millis(),
        )?;
        anyhow::ensure!(
            verified.frame().direction == ChannelDirection::SupervisorToOwner,
            "not a supervisor frame"
        );
        Ok(verified)
    }
}

impl ChannelStreamOwner for FixtureOwner {
    fn admit(&self, placement: &str, occurrence: &str) -> Result<u64, ChannelStreamRefusal> {
        if self.refused.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ChannelStreamRefusal::OccurrenceRefused);
        }
        if placement == PLACEMENT
            && occurrence == OCCURRENCE
            && self.live.load(std::sync::atomic::Ordering::SeqCst)
        {
            if self.selected.load(std::sync::atomic::Ordering::SeqCst) {
                Ok(self.maximum_response_bytes)
            } else {
                Err(ChannelStreamRefusal::StreamNotSelected)
            }
        } else {
            Err(ChannelStreamRefusal::NoLiveChannel)
        }
    }

    fn authenticate(&self, placement: &str, occurrence: &str, frame: &[u8]) -> anyhow::Result<()> {
        self.verify(placement, occurrence, frame).map(|_| ())
    }

    fn exchange(
        &self,
        placement: &str,
        occurrence: &str,
        frame: &[u8],
    ) -> anyhow::Result<ExternalChannelExchangeResponse> {
        let verified = self.verify(placement, occurrence, frame)?;
        if self
            .refuse_in_exchange
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.refused.store(true, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("external channel refused: the lease was refused: test");
        }
        if self
            .unavailable_exchanges
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |left| left.checked_sub(1),
            )
            .is_ok()
        {
            return Err(anyhow::anyhow!("the lifecycle adapter timed out")
                .context(ryeos_app::external_placement::ExternalChannelExchangeUnavailable));
        }
        if self.held.load(std::sync::atomic::Ordering::SeqCst)
            || (verified.frame().sequence == 1
                && self
                    .hold_first_frame
                    .load(std::sync::atomic::Ordering::SeqCst))
        {
            return Ok(ExternalChannelExchangeResponse {
                schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
                incoming_new: false,
                incoming_sequence: verified.frame().sequence,
                incoming_frame_digest: verified.digest().to_owned(),
                acknowledgement_frame_digest: None,
                outbound_frames: Vec::new(),
                urgent_revocation_frame: None,
            });
        }
        std::thread::sleep(self.exchange_delay);
        *self.exchanges.lock().unwrap() += 1;
        let digest = verified.digest().to_owned();
        let incoming_new = {
            let mut seen = self.seen.lock().unwrap();
            let new = !seen.contains(&digest);
            if new {
                seen.push(digest.clone());
            }
            new
        };
        let owner_frame = self.fixture.owner_bytes_frame();
        let outbound = vec![ExternalChannelResponseFrame::new(
            1,
            lillux::sha256_hex(&owner_frame),
            &owner_frame,
        )?];
        Ok(ExternalChannelExchangeResponse {
            schema: EXTERNAL_CHANNEL_TRANSPORT_SCHEMA,
            incoming_new,
            incoming_sequence: verified.frame().sequence,
            incoming_frame_digest: digest,
            acknowledgement_frame_digest: None,
            outbound_frames: outbound,
            urgent_revocation_frame: None,
        })
    }
}

fn test_policy() -> ChannelStreamPolicy {
    ChannelStreamPolicy {
        pre_auth_deadline: Duration::from_millis(400),
        idle_deadline: Duration::from_secs(30),
        max_pre_auth_per_occurrence: 2,
        max_pre_auth_total: 64,
    }
}

async fn serve(owner: Arc<FixtureOwner>, slots: Arc<ChannelStreamSlots>) -> SocketAddr {
    let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
        let owner = Arc::clone(&owner);
        let slots = Arc::clone(&slots);
        async move {
            use axum::response::IntoResponse as _;
            let (parts, _body) = request.into_parts();
            let segments: Vec<String> = parts
                .uri
                .path()
                .split('/')
                .filter(|segment| !segment.is_empty())
                .map(str::to_owned)
                .collect();
            let [_, _, placement, occurrence, _] = segments.as_slice() else {
                return StatusCode::NOT_FOUND.into_response();
            };
            match channel_stream_upgrade(parts, owner, slots, placement.clone(), occurrence.clone())
                .await
            {
                Ok(response) => response,
                Err(error) => error.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    address
}

type ClientSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

async fn dial(
    address: SocketAddr,
    occurrence: &str,
    offer_contract: bool,
) -> Result<ClientSocket, tungstenite::Error> {
    let mut request =
        format!("ws://{address}/external-execution/channel/{PLACEMENT}/{occurrence}/stream")
            .into_client_request()
            .unwrap();
    if offer_contract {
        request.headers_mut().insert(
            "sec-websocket-protocol",
            tungstenite::http::HeaderValue::from_static(EXTERNAL_CHANNEL_STREAM_CONTRACT),
        );
    }
    let tcp = tokio::net::TcpStream::connect(address).await.unwrap();
    let config = tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(8 * 1024 * 1024),
        max_frame_size: Some(8 * 1024 * 1024),
        ..tungstenite::protocol::WebSocketConfig::default()
    };
    tokio_tungstenite::client_async_with_config(request, tcp, Some(config))
        .await
        .map(|(socket, _)| socket)
}

fn refused_status(error: tungstenite::Error) -> u16 {
    match error {
        tungstenite::Error::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other}"),
    }
}

/// Read until the controller closes; return its close code and reason.
async fn closed_with(socket: &mut ClientSocket) -> (u16, String) {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("controller did not close in time")
        {
            Some(Ok(tungstenite::Message::Close(Some(frame)))) => {
                return (u16::from(frame.code), frame.reason.into_owned());
            }
            Some(Ok(tungstenite::Message::Close(None))) | None => {
                return (0, String::new());
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => panic!("stream failed before its close: {error}"),
        }
    }
}

async fn binary_reply(socket: &mut ClientSocket) -> Vec<u8> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("controller did not answer in time")
        {
            Some(Ok(tungstenite::Message::Binary(bytes))) => return bytes,
            Some(Ok(tungstenite::Message::Close(frame))) => {
                panic!("controller closed instead of answering: {frame:?}")
            }
            Some(Ok(_)) => {}
            other => panic!("stream ended before its answer: {other:?}"),
        }
    }
}

async fn wait_for_occupancy(slots: &ChannelStreamSlots, expected: (usize, usize)) {
    for _ in 0..200 {
        if slots.occupancy() == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "stream places never reached {expected:?}; now {:?}",
        slots.occupancy()
    );
}

#[tokio::test]
async fn upgrade_without_the_stream_contract_is_refused_by_name() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let error = dial(address, OCCURRENCE, false).await.err().unwrap();
    assert_eq!(refused_status(error), 400);
    assert_eq!(slots.occupancy(), (0, 0));
}

#[tokio::test]
async fn upgrade_without_a_live_channel_is_refused_by_name() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(owner, Arc::clone(&slots)).await;
    let error = dial(address, "occ-other", true).await.err().unwrap();
    assert_eq!(refused_status(error), 404);
    assert_eq!(slots.occupancy(), (0, 0));
    assert_eq!(
        ChannelStreamRefusal::NoLiveChannel.code(),
        "external_channel_stream_no_live_channel"
    );
}

#[tokio::test]
async fn a_frame_from_another_binding_never_reaches_the_exchange() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let forged = owner.fixture.ready_for_another_binding();
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(forged))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::POLICY);
    assert_eq!(reason, "external_channel_stream_frame_not_authenticated");
    assert_eq!(owner.exchanges(), 0);
    wait_for_occupancy(&slots, (0, 0)).await;

    // A frame of the right binding but the owner's direction is refused too.
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(
            owner.fixture.owner_bytes_frame(),
        ))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::POLICY);
    assert_eq!(reason, "external_channel_stream_frame_not_authenticated");
    assert_eq!(owner.exchanges(), 0);
}

#[tokio::test]
async fn text_and_oversized_messages_are_refused_by_name() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;

    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Text("{}".into()))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::UNSUPPORTED);
    assert_eq!(reason, "external_channel_stream_message_not_binary");

    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(vec![
            b'{';
            MAX_EXTERNAL_CHANNEL_STREAM_INBOUND_BYTES
                + 1
        ]))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::SIZE);
    assert_eq!(reason, "external_channel_stream_message_bound");
    assert_eq!(owner.exchanges(), 0);
    wait_for_occupancy(&slots, (0, 0)).await;
}

#[tokio::test]
async fn a_silent_socket_loses_its_pre_auth_place_at_the_deadline() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(owner, Arc::clone(&slots)).await;
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    wait_for_occupancy(&slots, (1, 0)).await;
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::POLICY);
    assert_eq!(reason, "external_channel_stream_pre_auth_deadline");
    wait_for_occupancy(&slots, (0, 0)).await;
}

#[tokio::test]
async fn pre_auth_places_are_capped_per_occurrence_and_in_total() {
    let mut policy = test_policy();
    policy.pre_auth_deadline = Duration::from_secs(30);
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(policy);
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let first = dial(address, OCCURRENCE, true).await.unwrap();
    let _second = dial(address, OCCURRENCE, true).await.unwrap();
    let error = dial(address, OCCURRENCE, true).await.err().unwrap();
    assert_eq!(refused_status(error), 503);
    drop(first);
    wait_for_occupancy(&slots, (1, 0)).await;
    let _third = dial(address, OCCURRENCE, true).await.unwrap();

    let mut policy = test_policy();
    policy.pre_auth_deadline = Duration::from_secs(30);
    policy.max_pre_auth_per_occurrence = 8;
    policy.max_pre_auth_total = 1;
    let slots = ChannelStreamSlots::new(policy);
    let address = serve(owner, Arc::clone(&slots)).await;
    let _only = dial(address, OCCURRENCE, true).await.unwrap();
    let error = dial(address, OCCURRENCE, true).await.err().unwrap();
    assert_eq!(refused_status(error), 503);
}

#[tokio::test]
async fn one_verified_socket_holds_the_occurrence_and_a_reconnect_resumes() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let ready = owner.fixture.ready();

    let mut first = dial(address, OCCURRENCE, true).await.unwrap();
    first
        .send(tungstenite::Message::Binary(ready.clone()))
        .await
        .unwrap();
    let reply = decode_external_channel_stream_response(&binary_reply(&mut first).await).unwrap();
    assert!(reply.incoming_new);
    assert_eq!(reply.incoming_sequence, 1);
    assert_eq!(reply.incoming_frame_digest, lillux::sha256_hex(&ready));
    assert_eq!(
        reply.outbound_frames[0].decode_frame().unwrap(),
        owner.fixture.owner_bytes_frame()
    );
    wait_for_occupancy(&slots, (0, 1)).await;

    // A second socket whose frame verifies is told to try again, before its
    // frame reaches the exchange.
    let exchanged = owner.exchanges();
    let mut second = dial(address, OCCURRENCE, true).await.unwrap();
    second
        .send(tungstenite::Message::Binary(ready.clone()))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut second).await;
    assert_eq!(code, close_code::AGAIN);
    assert_eq!(reason, "external_channel_stream_already_attached");
    assert_eq!(owner.exchanges(), exchanged);

    // The same socket keeps exchanging; a replay is not new.
    first
        .send(tungstenite::Message::Binary(ready.clone()))
        .await
        .unwrap();
    let replay = decode_external_channel_stream_response(&binary_reply(&mut first).await).unwrap();
    assert!(!replay.incoming_new);

    // Once the first socket ends, the guest reconnects and resumes by
    // presenting its retained frame again.
    first.close(None).await.unwrap();
    wait_for_occupancy(&slots, (0, 0)).await;
    let mut resumed = dial(address, OCCURRENCE, true).await.unwrap();
    resumed
        .send(tungstenite::Message::Binary(ready.clone()))
        .await
        .unwrap();
    let resumed_reply =
        decode_external_channel_stream_response(&binary_reply(&mut resumed).await).unwrap();
    assert!(!resumed_reply.incoming_new);
    assert_eq!(
        resumed_reply.incoming_frame_digest,
        lillux::sha256_hex(&ready)
    );
}

#[tokio::test]
async fn a_response_over_the_negotiated_bound_is_refused_by_name() {
    let owner = FixtureOwner::new(Fixture::new(), 256);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), slots).await;
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(owner.fixture.ready()))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::POLICY);
    assert_eq!(reason, "external_channel_stream_response_bound");
}

#[tokio::test]
async fn an_idle_attached_socket_is_closed_so_its_place_frees() {
    let mut policy = test_policy();
    policy.idle_deadline = Duration::from_millis(300);
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(policy);
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(owner.fixture.ready()))
        .await
        .unwrap();
    binary_reply(&mut socket).await;
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::AWAY);
    assert_eq!(reason, "external_channel_stream_idle_deadline");
    wait_for_occupancy(&slots, (0, 0)).await;
}

fn raw_route(auth: &str, methods: &[&str], path: &str) -> RawRouteSpec {
    RawRouteSpec {
        id: "external-execution/channel-stream".into(),
        path: path.into(),
        methods: methods.iter().map(|method| (*method).to_owned()).collect(),
        auth: auth.into(),
        auth_config: None,
        limits: ryeos_app::route_raw::RawLimits::default(),
        response: ryeos_app::route_raw::RawResponseSpec {
            mode: MODE_KEY.into(),
            source: None,
            source_config: serde_json::Value::Null,
            status: None,
            content_type: None,
            body_b64: None,
        },
        execute: None,
        request: ryeos_app::route_raw::RawRequest::default(),
        source_file: std::path::PathBuf::new(),
    }
}

#[test]
fn the_route_must_be_an_unverified_get_that_captures_the_occurrence() {
    let path = "/external-execution/channel/{placement_thread_id}/{occurrence_id}/stream";
    assert!(
        ChannelStreamMode
            .compile(&raw_route("none", &["GET"], path))
            .is_ok()
    );
    assert!(
        ChannelStreamMode
            .compile(&raw_route("ryeos_signed", &["GET"], path))
            .is_err()
    );
    assert!(
        ChannelStreamMode
            .compile(&raw_route("none", &["POST"], path))
            .is_err()
    );
    assert!(
        ChannelStreamMode
            .compile(&raw_route("none", &["GET", "POST"], path))
            .is_err()
    );
    assert!(
        ChannelStreamMode
            .compile(&raw_route(
                "none",
                &["GET"],
                "/external-execution/channel/{placement_thread_id}/stream"
            ))
            .is_err()
    );
    let mut with_source = raw_route("none", &["GET"], path);
    with_source.response.source = Some("service:anything".into());
    assert!(ChannelStreamMode.compile(&with_source).is_err());
}

// ── Guest transport end to end ─────────────────────────────────────────

const TEST_CA_DER: &str = ryeos_external_candidate_supervisor::test_support::TEST_CA_DER_BASE64;
const TEST_SERVER_DER: &str =
    ryeos_external_candidate_supervisor::test_support::TEST_SERVER_DER_BASE64;
const TEST_SERVER_KEY_DER: &str =
    ryeos_external_candidate_supervisor::test_support::TEST_SERVER_KEY_DER_BASE64;

/// Terminate TLS for `localhost` and forward bytes to the plain controller,
/// as a provider edge does in front of the node.
async fn tls_edge(controller: SocketAddr) -> SocketAddr {
    let certificate =
        rustls::pki_types::CertificateDer::from(STANDARD.decode(TEST_SERVER_DER).unwrap());
    let private_key = rustls::pki_types::PrivateKeyDer::Pkcs8(
        rustls::pki_types::PrivatePkcs8KeyDer::from(STANDARD.decode(TEST_SERVER_KEY_DER).unwrap()),
    );
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate], private_key)
    .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(socket).await else {
                    return;
                };
                let Ok(mut upstream) = tokio::net::TcpStream::connect(controller).await else {
                    return;
                };
                let copied = tokio::io::copy_bidirectional(&mut tls, &mut upstream).await;
                drop(copied);
            });
        }
    });
    address
}

fn guest_stream(
    edge: SocketAddr,
    occurrence: &str,
) -> ryeos_external_candidate_supervisor::stream_transport::AttachedExternalExecutionStream {
    use ryeos_external_candidate_supervisor::stream_transport::test_support::{
        LoopbackStream, stream,
    };
    stream(LoopbackStream {
        https_origin: format!("https://localhost:{}", edge.port()),
        placement_thread_id: PLACEMENT.into(),
        occurrence_id: occurrence.into(),
        tls_roots_der: vec![STANDARD.decode(TEST_CA_DER).unwrap()],
        network: lillux::network::NetworkContext::from_config_bytes(
            b"nameserver 127.0.0.1\n",
            b"127.0.0.1 localhost\n",
        )
        .unwrap(),
        request_timeout: lillux::time::Duration::from_secs(5),
        maximum_response_bytes: 1024 * 1024,
        lifetime: lillux::time::Duration::from_secs(120),
    })
    .unwrap()
}

/// The guest transport is synchronous and refuses to run inside a reactor,
/// so it runs on its own thread as it does in the guest.
async fn on_guest_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    let guest = std::thread::spawn(work);
    tokio::task::spawn_blocking(move || guest.join().unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_transport_exchanges_and_resumes_over_tls_and_a_real_websocket() {
    let mut policy = test_policy();
    policy.idle_deadline = Duration::from_millis(400);
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(policy);
    let controller = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let edge = tls_edge(controller).await;
    let ready = owner.fixture.ready();
    let owner_frame = owner.fixture.owner_bytes_frame();

    on_guest_thread(move || {
        let mut stream = guest_stream(edge, OCCURRENCE);
        let first = stream.exchange(&ready).unwrap();
        assert!(first.incoming_new);
        assert_eq!(first.incoming_sequence, 1);
        assert_eq!(first.incoming_frame_digest, lillux::sha256_hex(&ready));
        assert_eq!(first.outbound_frames.len(), 1);
        assert_eq!(first.outbound_frames[0].canonical_wire, owner_frame);

        let replay = stream.exchange(&ready).unwrap();
        assert!(!replay.incoming_new);

        // The controller closes the idle socket; the next exchange finds it
        // gone and is ambiguous, and the one after dials again and resumes
        // with the same retained bytes.
        std::thread::sleep(std::time::Duration::from_millis(900));
        let lost = stream.exchange_until(
            &ready,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
        );
        assert!(
            matches!(
                lost,
                Err(ExternalTransportStepFailure::AmbiguousTransport(_))
            ),
            "an idle-closed socket must be an ambiguous outcome"
        );
        let resumed = stream.exchange(&ready).unwrap();
        assert!(!resumed.incoming_new);
        assert_eq!(resumed.incoming_frame_digest, lillux::sha256_hex(&ready));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_transport_surfaces_named_refusals_as_fatal() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let controller = serve(Arc::clone(&owner), slots).await;
    let edge = tls_edge(controller).await;
    let forged = owner.fixture.ready_for_another_binding();

    on_guest_thread(move || {
        let mut stream = guest_stream(edge, OCCURRENCE);
        match stream.exchange_until(
            &forged,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
        ) {
            Err(ExternalTransportStepFailure::Fatal(error)) => assert!(
                format!("{error:#}").contains("external_channel_stream_frame_not_authenticated"),
                "{error:#}"
            ),
            other => panic!("a forged first frame must be a named fatal refusal: {other:?}"),
        }

        let mut elsewhere = guest_stream(edge, "occ-other");
        match elsewhere.exchange_until(
            &forged,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5)),
        ) {
            Err(ExternalTransportStepFailure::Fatal(error)) => {
                assert!(format!("{error:#}").contains("HTTP 404"), "{error:#}")
            }
            other => panic!("an occurrence without a live channel must be fatal: {other:?}"),
        }
    })
    .await;
    assert_eq!(owner.exchanges(), 0);
}

#[tokio::test]
async fn a_channel_that_ends_closes_its_stream_at_the_next_frame() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let ready = owner.fixture.ready();
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(ready.clone()))
        .await
        .unwrap();
    binary_reply(&mut socket).await;
    let exchanged = owner.exchanges();
    owner.terminate();
    socket
        .send(tungstenite::Message::Binary(ready))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::POLICY);
    assert_eq!(reason, "external_channel_stream_no_live_channel");
    assert_eq!(owner.exchanges(), exchanged);
    wait_for_occupancy(&slots, (0, 0)).await;
}

#[tokio::test]
async fn a_channel_that_ends_before_the_first_frame_never_takes_the_place() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    owner.terminate();
    socket
        .send(tungstenite::Message::Binary(owner.fixture.ready()))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::POLICY);
    assert_eq!(reason, "external_channel_stream_no_live_channel");
    assert_eq!(owner.exchanges(), 0);
    wait_for_occupancy(&slots, (0, 0)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_exchange_ends_by_its_deadline_after_contact() {
    let owner = FixtureOwner::slow(Fixture::new(), 1024 * 1024, Duration::from_secs(3));
    let slots = ChannelStreamSlots::new(test_policy());
    let controller = serve(Arc::clone(&owner), slots).await;
    let edge = tls_edge(controller).await;
    let ready = owner.fixture.ready();
    on_guest_thread(move || {
        // The stream's own read bound is five seconds; the exchange's
        // deadline is far shorter and must end it.
        let mut stream = guest_stream(edge, OCCURRENCE);
        let started = std::time::Instant::now();
        let outcome = stream.exchange_until(
            &ready,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(800)),
        );
        assert!(
            matches!(
                outcome,
                Err(ExternalTransportStepFailure::AmbiguousTransport(_))
            ),
            "a response later than the deadline is an ambiguous outcome"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(2_500),
            "the exchange outlived its deadline: {:?}",
            started.elapsed()
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_waits_before_redialing_a_busy_occurrence() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let controller = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let edge = tls_edge(controller).await;
    let ready = owner.fixture.ready();

    // Another socket holds the occurrence's attached place.
    let mut holder = dial(controller, OCCURRENCE, true).await.unwrap();
    holder
        .send(tungstenite::Message::Binary(ready.clone()))
        .await
        .unwrap();
    binary_reply(&mut holder).await;
    wait_for_occupancy(&slots, (0, 1)).await;

    let exchanged = owner.exchanges();
    on_guest_thread(move || {
        let mut stream = guest_stream(edge, OCCURRENCE);
        let deadline =
            || lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5));
        match stream.exchange_until(&ready, deadline()) {
            Err(ExternalTransportStepFailure::AmbiguousTransport(error)) => assert!(
                format!("{error:#}").contains("external_channel_stream_already_attached"),
                "{error:#}"
            ),
            other => panic!("a held place must ask the guest to try again: {other:?}"),
        }
        // Inside the redial wait, a short deadline ends without dialing.
        let started = std::time::Instant::now();
        match stream.exchange_until(
            &ready,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_millis(20)),
        ) {
            Err(ExternalTransportStepFailure::AmbiguousTransport(error)) => assert!(
                format!("{error:#}").contains("waiting to redial"),
                "{error:#}"
            ),
            other => panic!("the redial wait must hold the guest back: {other:?}"),
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    })
    .await;
    assert_eq!(owner.exchanges(), exchanged);
    drop(holder);
}

#[tokio::test]
async fn a_binding_that_selects_the_request_response_route_has_no_stream() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    owner
        .selected
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let error = dial(address, OCCURRENCE, true).await.err().unwrap();
    assert_eq!(refused_status(error), 409);
    assert_eq!(slots.occupancy(), (0, 0));
    assert_eq!(
        ChannelStreamRefusal::StreamNotSelected.code(),
        "external_channel_stream_not_selected"
    );
}

const OCCURRENCE_REFUSED: &str = "external_channel_stream_occurrence_refused";

#[tokio::test]
async fn a_refused_occurrence_has_no_stream_and_is_gone_before_the_upgrade() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    owner
        .refused
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let error = dial(address, OCCURRENCE, true).await.err().unwrap();
    assert_eq!(refused_status(error), 410);
    assert_eq!(slots.occupancy(), (0, 0));
    assert_eq!(ChannelStreamRefusal::OccurrenceRefused.code(), OCCURRENCE_REFUSED);
}

#[tokio::test]
async fn an_occurrence_refused_before_the_first_frame_closes_the_socket() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    owner
        .refused
        .store(true, std::sync::atomic::Ordering::SeqCst);
    socket
        .send(tungstenite::Message::Binary(owner.fixture.ready()))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::POLICY);
    assert_eq!(reason, OCCURRENCE_REFUSED);
    assert_eq!(owner.exchanges(), 0);
    wait_for_occupancy(&slots, (0, 0)).await;
}

#[tokio::test]
async fn an_occurrence_refused_mid_stream_closes_at_the_next_frame() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let ready = owner.fixture.ready();
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(ready.clone()))
        .await
        .unwrap();
    binary_reply(&mut socket).await;
    let exchanged = owner.exchanges();
    owner
        .refused
        .store(true, std::sync::atomic::Ordering::SeqCst);
    socket
        .send(tungstenite::Message::Binary(ready))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::POLICY);
    assert_eq!(reason, OCCURRENCE_REFUSED);
    assert_eq!(owner.exchanges(), exchanged);
    wait_for_occupancy(&slots, (0, 0)).await;
}

#[tokio::test]
async fn a_frame_that_refuses_the_lease_closes_with_the_occurrence_refusal() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    owner
        .refuse_in_exchange
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(owner.fixture.ready()))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::POLICY);
    assert_eq!(reason, OCCURRENCE_REFUSED);
    wait_for_occupancy(&slots, (0, 0)).await;
}

#[tokio::test]
async fn a_held_reply_passes_through_to_the_guest_unchanged() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    owner.held.store(true, std::sync::atomic::Ordering::SeqCst);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(owner.fixture.ready()))
        .await
        .unwrap();
    let reply = binary_reply(&mut socket).await;
    let response: ExternalChannelExchangeResponse = serde_json::from_slice(&reply).unwrap();
    assert!(!response.incoming_new);
    assert!(response.outbound_frames.is_empty());
}

#[tokio::test]
async fn an_exchange_with_no_verdict_closes_retryably_and_a_retry_is_served() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    owner
        .unavailable_exchanges
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let slots = ChannelStreamSlots::new(test_policy());
    let address = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(owner.fixture.ready()))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::ERROR, "a missing verdict is not a policy refusal");
    assert_eq!(reason, "external_channel_stream_exchange_unavailable");
    wait_for_occupancy(&slots, (0, 0)).await;

    // The occurrence is not refused: the same frame on a new socket is served.
    let mut retry = dial(address, OCCURRENCE, true).await.unwrap();
    retry
        .send(tungstenite::Message::Binary(owner.fixture.ready()))
        .await
        .unwrap();
    let response: ExternalChannelExchangeResponse =
        serde_json::from_slice(&binary_reply(&mut retry).await).unwrap();
    assert!(response.incoming_new);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guest_retries_a_frame_whose_exchange_reached_no_verdict() {
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    owner
        .unavailable_exchanges
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let slots = ChannelStreamSlots::new(test_policy());
    let controller = serve(Arc::clone(&owner), slots).await;
    let edge = tls_edge(controller).await;
    let ready = owner.fixture.ready();

    on_guest_thread(move || {
        let mut stream = guest_stream(edge, OCCURRENCE);
        let deadline =
            || lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(5));
        match stream.exchange_until(&ready, deadline()) {
            Err(ExternalTransportStepFailure::AmbiguousTransport(error)) => assert!(
                format!("{error:#}").contains("external_channel_stream_exchange_unavailable"),
                "{error:#}"
            ),
            other => panic!("a missing verdict must be retryable, not fatal: {other:?}"),
        }
        // After the redial wait, the same frame is served.
        match stream.exchange_until(&ready, deadline()) {
            Ok(_) => {}
            other => panic!("the retried frame must be served: {other:?}"),
        }
    })
    .await;
    assert_eq!(owner.exchanges(), 1);
}

/// A leased guest's Ready naming its boot and the capability report that
/// follows it, signed by the supervisor key of `fixture`.
fn lease_handshake_frames(fixture: &Fixture) -> (Vec<u8>, Vec<u8>) {
    let boot_id = "01234567-89ab-cdef-0123-456789abcdef".to_owned();
    let author = |sequence: u64, previous: Option<String>, payload: ExecutionChannelPayload| {
        let signed = SignedExecutionFrame::sign(
            ExecutionFrame {
                schema: 1,
                binding_digest: fixture.binding.digest().unwrap(),
                direction: ChannelDirection::SupervisorToOwner,
                sequence,
                previous_frame_digest: previous,
                acknowledged_peer_sequence: 0,
                payload,
            },
            &fixture.binding,
            &fixture.supervisor_key,
        )
        .unwrap();
        let wire = lillux::canonical_json(&serde_json::to_value(&signed).unwrap())
            .unwrap()
            .into_bytes();
        let digest = SignedExecutionFrame::decode_and_verify(
            &wire,
            &fixture.binding,
            lillux::time::timestamp_millis(),
        )
        .unwrap()
        .digest()
        .to_owned();
        (wire, digest)
    };
    let (ready, ready_digest) = author(
        1,
        None,
        ExecutionChannelPayload::Ready {
            supervisor_runtime_hash: fixture.binding.supervisor_runtime_hash.clone(),
            base_snapshot_hash: fixture.binding.base_snapshot_hash.clone(),
            boot: Some(ryeos_state::external_execution::SupervisorReadyBoot {
                boot_id: boot_id.clone(),
                verified_input_digests: vec!["1".repeat(64), "2".repeat(64), "3".repeat(64)],
                ready_at_ms: lillux::time::timestamp_millis(),
            }),
        },
    );
    let (report, _) = author(
        2,
        Some(ready_digest),
        ExecutionChannelPayload::CapabilityReport {
            boot_id,
            provided: std::collections::BTreeMap::new(),
            missing: std::collections::BTreeMap::new(),
        },
    );
    (ready, report)
}

/// The guest's own `lease_handshake` over the guest's TLS WebSocket transport
/// against the real stream route. The controller holds the Ready, answers the
/// report once it has admitted the guest, and a transient no-verdict closes
/// 1011 and is retried while a durably refused occurrence closes 1008 and is
/// fatal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_guests_lease_handshake_runs_against_the_real_stream_route() {
    // Held Ready, then Admitted: the handshake proceeds.
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    owner
        .hold_first_frame
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let slots = ChannelStreamSlots::new(test_policy());
    let controller = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let edge = tls_edge(controller).await;
    let (ready, report) = lease_handshake_frames(&owner.fixture);
    let binding = owner.fixture.binding.clone();
    on_guest_thread(move || {
        let mut stream = guest_stream(edge, OCCURRENCE);
        ryeos_external_candidate_supervisor::runtime::lease_handshake_for_test(
            &mut stream,
            &binding,
            &ready,
            &report,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10)),
        )
        .unwrap();
    })
    .await;
    // Only the report reached a verdict; the held Ready is not counted.
    assert_eq!(owner.exchanges(), 1);

    // A transient no-verdict closes 1011 and the guest retries the same
    // frame. The 1011 close itself is checked on a raw socket below.
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    owner
        .hold_first_frame
        .store(true, std::sync::atomic::Ordering::SeqCst);
    owner
        .unavailable_exchanges
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let slots = ChannelStreamSlots::new(test_policy());
    let controller = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let edge = tls_edge(controller).await;
    let (ready, report) = lease_handshake_frames(&owner.fixture);
    let binding = owner.fixture.binding.clone();
    on_guest_thread(move || {
        let mut stream = guest_stream(edge, OCCURRENCE);
        ryeos_external_candidate_supervisor::runtime::lease_handshake_for_test(
            &mut stream,
            &binding,
            &ready,
            &report,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(20)),
        )
        .unwrap();
    })
    .await;
    assert_eq!(owner.exchanges(), 1);
    assert_eq!(
        owner
            .unavailable_exchanges
            .load(std::sync::atomic::Ordering::SeqCst),
        0,
        "both no-verdict exchanges were consumed and retried"
    );
    let probe = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    probe
        .unavailable_exchanges
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let address = serve(Arc::clone(&probe), ChannelStreamSlots::new(test_policy())).await;
    let mut socket = dial(address, OCCURRENCE, true).await.unwrap();
    socket
        .send(tungstenite::Message::Binary(
            lease_handshake_frames(&probe.fixture).0,
        ))
        .await
        .unwrap();
    let (code, reason) = closed_with(&mut socket).await;
    assert_eq!(code, close_code::ERROR);
    assert_eq!(reason, "external_channel_stream_exchange_unavailable");

    // A durably refused occurrence closes 1008 and the handshake is fatal.
    let owner = FixtureOwner::new(Fixture::new(), 1024 * 1024);
    owner
        .refuse_in_exchange
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let slots = ChannelStreamSlots::new(test_policy());
    let controller = serve(Arc::clone(&owner), Arc::clone(&slots)).await;
    let edge = tls_edge(controller).await;
    let (ready, report) = lease_handshake_frames(&owner.fixture);
    let binding = owner.fixture.binding.clone();
    let refusal = on_guest_thread(move || {
        let mut stream = guest_stream(edge, OCCURRENCE);
        ryeos_external_candidate_supervisor::runtime::lease_handshake_for_test(
            &mut stream,
            &binding,
            &ready,
            &report,
            lillux::time::MonotonicDeadline::after(lillux::time::Duration::from_secs(10)),
        )
        .unwrap_err()
    })
    .await;
    assert!(format!("{refusal:#}").contains(OCCURRENCE_REFUSED), "{refusal:#}");
    // The refusal is verdict-final: the occurrence no longer upgrades.
    assert_eq!(
        refused_status(dial(controller, OCCURRENCE, true).await.err().unwrap()),
        410
    );
}
