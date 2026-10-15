//! `channel_stream` response mode: one WebSocket per occurrence channel that
//! carries the same exchange as the request/response route.
//!
//! The HTTP layer authenticates nothing here. Before the upgrade the mode
//! refuses unless the guest offers the stream contract as its WebSocket
//! subprotocol and the occurrence has a live attached binding; after it, the
//! first message must be a supervisor frame that verifies against that
//! binding. Until then the socket is pre-authentication: it holds one of a
//! small number of pre-authentication places and is closed when its deadline
//! passes. The first socket whose frame verifies takes the occurrence's single
//! attached place; any other is told to try again later.
//!
//! Every inbound message is exactly one canonical signed frame, bounded by the
//! frame bound, and every outbound message is the canonical exchange response
//! for it, bounded by the binding's negotiated response bound. Every refusal
//! is named: by a status and code before the upgrade, by a close code and the
//! same kind of code as the close reason after it.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::http::StatusCode;

use crate::route_error::{RouteConfigError, RouteDispatchError};
use crate::routes::compile::{
    CompiledResponseMode, CompiledRoute, ResponseMode, RouteDispatchContext,
};
use ryeos_app::route_raw::{RawRequestBody, RawRouteSpec};
use ryeos_state::external_execution::transport::{
    EXTERNAL_CHANNEL_STREAM_CONTRACT, ExternalChannelExchangeResponse,
    MAX_EXTERNAL_CHANNEL_STREAM_INBOUND_BYTES, MAX_EXTERNAL_CHANNEL_STREAM_OUTBOUND_BYTES,
    encode_external_channel_stream_response, external_channel_stream_inbound_frame,
};

pub const MODE_KEY: &str = "channel_stream";
/// The stream authenticates in band, by signed frames; a route that put
/// another HTTP verifier in front of it would claim an authentication the
/// stream does not use.
pub const REQUIRED_AUTH: &str = "none";
pub const PLACEMENT_CAPTURE: &str = "placement_thread_id";
pub const OCCURRENCE_CAPTURE: &str = "occurrence_id";
/// How long a closing stream waits for its peer to finish the close.
const CLOSE_LINGER: Duration = Duration::from_secs(2);

/// Time, sizes and places that bound every stream on this node.
#[derive(Debug, Clone, Copy)]
pub struct ChannelStreamPolicy {
    /// From upgrade to the first message; the socket is closed when it passes.
    pub pre_auth_deadline: Duration,
    /// Silence allowed on an attached socket before it is closed so its
    /// single place frees for the guest's reconnect.
    pub idle_deadline: Duration,
    pub max_pre_auth_per_occurrence: usize,
    pub max_pre_auth_total: usize,
}

impl ChannelStreamPolicy {
    pub const NODE: Self = Self {
        pre_auth_deadline: Duration::from_secs(10),
        idle_deadline: Duration::from_secs(60),
        max_pre_auth_per_occurrence: 2,
        max_pre_auth_total: 64,
    };
}

/// Named refusals. The code is the public name; it is the JSON `error` of a
/// refused upgrade and the reason of a close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelStreamRefusal {
    NotAnUpgrade,
    ContractNotOffered,
    NoLiveChannel,
    StreamNotSelected,
    PreAuthCapacity,
    PreAuthDeadline,
    FrameNotAuthenticated,
    AlreadyAttached,
    MessageNotBinary,
    MessageBound,
    ResponseBound,
    ExchangeRefused,
    /// The exchange produced no verdict about the lease (the lifecycle
    /// adapter did not answer, or durable state failed transiently). Nothing
    /// was refused, so the guest retries the same frame.
    ExchangeUnavailable,
    /// The occurrence's lease was durably refused; the stream never
    /// reopens for it.
    OccurrenceRefused,
    IdleDeadline,
}

impl ChannelStreamRefusal {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotAnUpgrade => "external_channel_stream_not_an_upgrade",
            Self::ContractNotOffered => "external_channel_stream_contract_not_offered",
            Self::NoLiveChannel => "external_channel_stream_no_live_channel",
            Self::StreamNotSelected => "external_channel_stream_not_selected",
            Self::PreAuthCapacity => "external_channel_stream_pre_auth_capacity",
            Self::PreAuthDeadline => "external_channel_stream_pre_auth_deadline",
            Self::FrameNotAuthenticated => "external_channel_stream_frame_not_authenticated",
            Self::AlreadyAttached => "external_channel_stream_already_attached",
            Self::MessageNotBinary => "external_channel_stream_message_not_binary",
            Self::MessageBound => "external_channel_stream_message_bound",
            Self::ResponseBound => "external_channel_stream_response_bound",
            Self::ExchangeRefused => "external_channel_stream_exchange_refused",
            Self::OccurrenceRefused => "external_channel_stream_occurrence_refused",
            Self::ExchangeUnavailable => "external_channel_stream_exchange_unavailable",
            Self::IdleDeadline => "external_channel_stream_idle_deadline",
        }
    }

    /// Close code after the upgrade. A busy attached place asks the guest to
    /// try again, and an exchange that reached no verdict is a server-side
    /// error the guest retries; idleness is a plain going-away the guest
    /// recovers from by reconnecting; everything else is a verdict.
    fn close_code(self) -> u16 {
        match self {
            Self::AlreadyAttached => close_code::AGAIN,
            Self::ExchangeUnavailable => close_code::ERROR,
            Self::IdleDeadline => close_code::AWAY,
            Self::MessageNotBinary => close_code::UNSUPPORTED,
            Self::MessageBound => close_code::SIZE,
            Self::NotAnUpgrade
            | Self::ContractNotOffered
            | Self::NoLiveChannel
            | Self::StreamNotSelected
            | Self::PreAuthCapacity
            | Self::PreAuthDeadline
            | Self::FrameNotAuthenticated
            | Self::ResponseBound
            | Self::ExchangeRefused
            | Self::OccurrenceRefused => close_code::POLICY,
        }
    }

    /// Status of a refused upgrade.
    fn status(self) -> StatusCode {
        match self {
            Self::NotAnUpgrade | Self::ContractNotOffered => StatusCode::BAD_REQUEST,
            Self::NoLiveChannel => StatusCode::NOT_FOUND,
            Self::StreamNotSelected => StatusCode::CONFLICT,
            Self::OccurrenceRefused => StatusCode::GONE,
            Self::PreAuthCapacity => StatusCode::SERVICE_UNAVAILABLE,
            Self::PreAuthDeadline
            | Self::FrameNotAuthenticated
            | Self::AlreadyAttached
            | Self::MessageNotBinary
            | Self::MessageBound
            | Self::ResponseBound
            | Self::ExchangeRefused
            | Self::ExchangeUnavailable
            | Self::IdleDeadline => StatusCode::CONFLICT,
        }
    }

    fn upgrade_error(self) -> RouteDispatchError {
        RouteDispatchError::Structured {
            code: self.code().to_owned(),
            status: self.status().as_u16(),
            body: serde_json::json!({ "error": self.code() }),
            thread_id: None,
        }
    }
}

/// The owner side a stream exchanges with. Every call may block on durable
/// state, so the stream runs each one off the async workers.
pub trait ChannelStreamOwner: Send + Sync + 'static {
    /// Refuse unless the occurrence has a live attached binding that selects
    /// the stream; on success, the negotiated bound on one response.
    fn admit(
        &self,
        placement_thread_id: &str,
        occurrence_id: &str,
    ) -> Result<u64, ChannelStreamRefusal>;
    /// Verify one supervisor frame against the occurrence binding without
    /// retaining it.
    fn authenticate(
        &self,
        placement_thread_id: &str,
        occurrence_id: &str,
        frame: &[u8],
    ) -> anyhow::Result<()>;
    /// Authenticate and exchange one supervisor frame.
    fn exchange(
        &self,
        placement_thread_id: &str,
        occurrence_id: &str,
        frame: &[u8],
    ) -> anyhow::Result<ExternalChannelExchangeResponse>;
}

/// The node's owner: the attached channel journal behind the same functions
/// the request/response route uses.
struct NodeChannelStreamOwner {
    state: ryeos_app::state::AppState,
}

impl ChannelStreamOwner for NodeChannelStreamOwner {
    fn admit(
        &self,
        placement_thread_id: &str,
        occurrence_id: &str,
    ) -> Result<u64, ChannelStreamRefusal> {
        use ryeos_app::external_placement::ExternalChannelStreamRefusal as Refused;
        match ryeos_app::external_placement::external_channel_stream_admission(
            &self.state,
            placement_thread_id,
            occurrence_id,
        ) {
            Ok(admitted) => Ok(admitted.maximum_response_bytes()),
            Err(refused) => {
                tracing::warn!(
                    placement = %placement_thread_id,
                    refusal = %refused,
                    "external channel stream not admitted"
                );
                Err(match refused {
                    Refused::NoLiveChannel(_) => ChannelStreamRefusal::NoLiveChannel,
                    Refused::NotSelected => ChannelStreamRefusal::StreamNotSelected,
                    Refused::OccurrenceRefused(_) => ChannelStreamRefusal::OccurrenceRefused,
                })
            }
        }
    }

    fn authenticate(
        &self,
        placement_thread_id: &str,
        occurrence_id: &str,
        frame: &[u8],
    ) -> anyhow::Result<()> {
        ryeos_app::external_placement::authenticate_external_channel_frame(
            &self.state,
            placement_thread_id,
            occurrence_id,
            frame,
        )
        .map(|_| ())
    }

    fn exchange(
        &self,
        placement_thread_id: &str,
        occurrence_id: &str,
        frame: &[u8],
    ) -> anyhow::Result<ExternalChannelExchangeResponse> {
        crate::handlers::external_execution_channel::exchange_frame_response(
            &self.state,
            placement_thread_id,
            occurrence_id,
            frame,
        )
    }
}

type OccurrenceKey = (String, String);

#[derive(Default)]
struct SlotTable {
    pre_auth_total: usize,
    pre_auth: HashMap<OccurrenceKey, usize>,
    attached: HashSet<OccurrenceKey>,
}

/// Pre-authentication and attached places. The node keeps one table for the
/// process, so a route-table reload cannot open a second attached place for
/// the same occurrence.
pub struct ChannelStreamSlots {
    policy: ChannelStreamPolicy,
    table: Mutex<SlotTable>,
}

impl ChannelStreamSlots {
    pub fn new(policy: ChannelStreamPolicy) -> Arc<Self> {
        Arc::new(Self {
            policy,
            table: Mutex::new(SlotTable::default()),
        })
    }

    /// The node's single table.
    pub fn node() -> Arc<Self> {
        static NODE: OnceLock<Arc<ChannelStreamSlots>> = OnceLock::new();
        Arc::clone(NODE.get_or_init(|| Self::new(ChannelStreamPolicy::NODE)))
    }

    fn table(&self) -> std::sync::MutexGuard<'_, SlotTable> {
        // Every critical section is a few counter updates that cannot panic
        // midway, so a poisoned table is still consistent.
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn enter_pre_auth(
        self: &Arc<Self>,
        key: OccurrenceKey,
    ) -> Result<PreAuthPlace, ChannelStreamRefusal> {
        let mut table = self.table();
        let for_occurrence = table.pre_auth.get(&key).copied().unwrap_or(0);
        if table.pre_auth_total >= self.policy.max_pre_auth_total
            || for_occurrence >= self.policy.max_pre_auth_per_occurrence
        {
            return Err(ChannelStreamRefusal::PreAuthCapacity);
        }
        table.pre_auth_total += 1;
        table.pre_auth.insert(key.clone(), for_occurrence + 1);
        Ok(PreAuthPlace {
            held: Some((Arc::clone(self), key)),
        })
    }

    fn leave_pre_auth(&self, key: &OccurrenceKey) {
        let mut table = self.table();
        table.pre_auth_total = table.pre_auth_total.saturating_sub(1);
        let remaining = table
            .pre_auth
            .get(key)
            .copied()
            .unwrap_or(0)
            .saturating_sub(1);
        if remaining == 0 {
            table.pre_auth.remove(key);
        } else {
            table.pre_auth.insert(key.clone(), remaining);
        }
    }

    fn detach(&self, key: &OccurrenceKey) {
        self.table().attached.remove(key);
    }

    #[cfg(test)]
    fn occupancy(&self) -> (usize, usize) {
        let table = self.table();
        (table.pre_auth_total, table.attached.len())
    }
}

/// One pre-authentication place, released when dropped or when it becomes
/// the attached place.
struct PreAuthPlace {
    held: Option<(Arc<ChannelStreamSlots>, OccurrenceKey)>,
}

impl PreAuthPlace {
    fn attach(mut self) -> Result<AttachedPlace, ChannelStreamRefusal> {
        let (slots, key) = self
            .held
            .take()
            .ok_or(ChannelStreamRefusal::AlreadyAttached)?;
        slots.leave_pre_auth(&key);
        if !slots.table().attached.insert(key.clone()) {
            return Err(ChannelStreamRefusal::AlreadyAttached);
        }
        Ok(AttachedPlace { slots, key })
    }
}

impl Drop for PreAuthPlace {
    fn drop(&mut self) {
        if let Some((slots, key)) = self.held.take() {
            slots.leave_pre_auth(&key);
        }
    }
}

/// The occurrence's single attached place, released when the socket ends.
struct AttachedPlace {
    slots: Arc<ChannelStreamSlots>,
    key: OccurrenceKey,
}

impl Drop for AttachedPlace {
    fn drop(&mut self) {
        self.slots.detach(&self.key);
    }
}

#[derive(Default)]
pub struct ChannelStreamMode;

struct CompiledChannelStreamMode;

impl ResponseMode for ChannelStreamMode {
    fn key(&self) -> &'static str {
        MODE_KEY
    }

    fn compile(
        &self,
        raw: &RawRouteSpec,
    ) -> Result<Arc<dyn CompiledResponseMode>, RouteConfigError> {
        let refuse = |reason: &str| RouteConfigError::InvalidResponseSpec {
            id: raw.id.clone(),
            mode: MODE_KEY.into(),
            reason: reason.into(),
        };
        if raw.auth != REQUIRED_AUTH {
            return Err(RouteConfigError::SourceAuthRequirement {
                id: raw.id.clone(),
                src: MODE_KEY.into(),
                required: REQUIRED_AUTH.into(),
                got: raw.auth.clone(),
            });
        }
        if raw.methods.len() != 1 || !raw.methods.contains("GET") {
            return Err(refuse("channel_stream accepts exactly GET"));
        }
        if raw.request.body != RawRequestBody::None {
            return Err(refuse("channel_stream accepts no request body"));
        }
        if raw.execute.is_some()
            || raw.response.source.is_some()
            || !raw.response.source_config.is_null()
            || raw.response.status.is_some()
            || raw.response.content_type.is_some()
            || raw.response.body_b64.is_some()
        {
            return Err(refuse(
                "channel_stream takes no execute, source, status, content or body",
            ));
        }
        for capture in [PLACEMENT_CAPTURE, OCCURRENCE_CAPTURE] {
            if !raw.path.contains(&format!("{{{capture}}}")) {
                return Err(refuse(
                    "channel_stream path must capture placement_thread_id and occurrence_id",
                ));
            }
        }
        Ok(Arc::new(CompiledChannelStreamMode))
    }
}

#[axum::async_trait]
impl CompiledResponseMode for CompiledChannelStreamMode {
    fn is_streaming(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn handle(
        &self,
        _compiled: &CompiledRoute,
        ctx: RouteDispatchContext,
    ) -> Result<axum::response::Response, RouteDispatchError> {
        let placement = ctx
            .captures
            .get(PLACEMENT_CAPTURE)
            .cloned()
            .ok_or_else(|| ChannelStreamRefusal::NoLiveChannel.upgrade_error())?;
        let occurrence = ctx
            .captures
            .get(OCCURRENCE_CAPTURE)
            .cloned()
            .ok_or_else(|| ChannelStreamRefusal::NoLiveChannel.upgrade_error())?;
        let owner = Arc::new(NodeChannelStreamOwner { state: ctx.state });
        channel_stream_upgrade(
            ctx.request_parts,
            owner,
            ChannelStreamSlots::node(),
            placement,
            occurrence,
        )
        .await
    }
}

fn offers_contract(parts: &axum::http::request::Parts) -> bool {
    parts
        .headers
        .get_all(axum::http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|offered| offered.trim() == EXTERNAL_CHANNEL_STREAM_CONTRACT)
}

/// Admit and upgrade one stream request, or refuse it by name. The socket
/// then runs on its own task until it closes.
pub async fn channel_stream_upgrade<O: ChannelStreamOwner>(
    mut parts: axum::http::request::Parts,
    owner: Arc<O>,
    slots: Arc<ChannelStreamSlots>,
    placement_thread_id: String,
    occurrence_id: String,
) -> Result<axum::response::Response, RouteDispatchError> {
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| ChannelStreamRefusal::NotAnUpgrade.upgrade_error())?;
    if !offers_contract(&parts) {
        return Err(ChannelStreamRefusal::ContractNotOffered.upgrade_error());
    }
    let admitted = {
        let owner = Arc::clone(&owner);
        let placement = placement_thread_id.clone();
        let occurrence = occurrence_id.clone();
        tokio::task::spawn_blocking(move || owner.admit(&placement, &occurrence))
            .await
            .map_err(|error| RouteDispatchError::Internal(error.to_string()))?
    };
    let maximum_response_bytes = match admitted {
        Ok(bytes) => usize::try_from(bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_EXTERNAL_CHANNEL_STREAM_OUTBOUND_BYTES),
        Err(refusal) => return Err(refusal.upgrade_error()),
    };
    let place = slots
        .enter_pre_auth((placement_thread_id.clone(), occurrence_id.clone()))
        .map_err(ChannelStreamRefusal::upgrade_error)?;
    let policy = slots.policy;
    let session = ChannelStreamSession {
        owner,
        placement_thread_id,
        occurrence_id,
        maximum_response_bytes,
        policy,
    };
    Ok(upgrade
        .protocols([EXTERNAL_CHANNEL_STREAM_CONTRACT])
        .max_message_size(MAX_EXTERNAL_CHANNEL_STREAM_INBOUND_BYTES)
        .max_frame_size(MAX_EXTERNAL_CHANNEL_STREAM_INBOUND_BYTES)
        .on_upgrade(move |socket| session.run(socket, place)))
}

struct ChannelStreamSession<O> {
    owner: Arc<O>,
    placement_thread_id: String,
    occurrence_id: String,
    maximum_response_bytes: usize,
    policy: ChannelStreamPolicy,
}

/// What reading the next message produced.
enum Inbound {
    Frame(Vec<u8>),
    Closed,
    Refused(ChannelStreamRefusal),
}

async fn next_frame(socket: &mut WebSocket) -> Inbound {
    loop {
        match socket.recv().await {
            Some(Ok(Message::Binary(bytes))) => {
                return match external_channel_stream_inbound_frame(&bytes) {
                    Ok(_) => Inbound::Frame(bytes),
                    Err(_) => Inbound::Refused(ChannelStreamRefusal::MessageBound),
                };
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Text(_))) => {
                return Inbound::Refused(ChannelStreamRefusal::MessageNotBinary);
            }
            Some(Ok(Message::Close(_))) | None => return Inbound::Closed,
            Some(Err(error)) => {
                let over_bound = error
                    .into_inner()
                    .downcast::<tungstenite::Error>()
                    .is_ok_and(|error| matches!(*error, tungstenite::Error::Capacity(_)));
                return if over_bound {
                    Inbound::Refused(ChannelStreamRefusal::MessageBound)
                } else {
                    Inbound::Closed
                };
            }
        }
    }
}

async fn close(mut socket: WebSocket, refusal: ChannelStreamRefusal) {
    // The peer may already be gone; a failed close only means it is.
    let sent = socket
        .send(Message::Close(Some(CloseFrame {
            code: refusal.close_code(),
            reason: Cow::Borrowed(refusal.code()),
        })))
        .await;
    if let Err(error) = sent {
        tracing::debug!(error = %error, "external channel stream close was not delivered");
        return;
    }
    // Let the peer read the close before the connection is dropped: a drop
    // with unread input resets the connection, and a reset can discard the
    // close and its named reason before the guest reads them.
    let drained = tokio::time::timeout(CLOSE_LINGER, async {
        while let Some(Ok(_)) = socket.recv().await {}
    })
    .await;
    if drained.is_err() {
        tracing::debug!("external channel stream peer did not finish closing");
    }
}

impl<O: ChannelStreamOwner> ChannelStreamSession<O> {
    async fn run(self, mut socket: WebSocket, place: PreAuthPlace) {
        let first = match tokio::time::timeout(
            self.policy.pre_auth_deadline,
            next_frame(&mut socket),
        )
        .await
        {
            Err(_) => return close(socket, ChannelStreamRefusal::PreAuthDeadline).await,
            Ok(Inbound::Closed) => return,
            Ok(Inbound::Refused(refusal)) => return close(socket, refusal).await,
            Ok(Inbound::Frame(frame)) => frame,
        };
        // The channel may have ended while the socket waited for its first
        // frame, so admission is checked again right before the place is
        // taken, not only before the upgrade.
        let checked = {
            let owner = Arc::clone(&self.owner);
            let placement = self.placement_thread_id.clone();
            let occurrence = self.occurrence_id.clone();
            let frame = first.clone();
            tokio::task::spawn_blocking(move || {
                if owner.authenticate(&placement, &occurrence, &frame).is_err() {
                    return Err(ChannelStreamRefusal::FrameNotAuthenticated);
                }
                owner.admit(&placement, &occurrence).map(|_| ())
            })
            .await
            .unwrap_or(Err(ChannelStreamRefusal::FrameNotAuthenticated))
        };
        if let Err(refusal) = checked {
            tracing::warn!(
                placement = %self.placement_thread_id,
                refusal = refusal.code(),
                "external channel stream first frame refused"
            );
            return close(socket, refusal).await;
        }
        let _attached = match place.attach() {
            Ok(attached) => attached,
            Err(refusal) => return close(socket, refusal).await,
        };
        let mut frame = first;
        loop {
            if let Err(refusal) = self.exchange(&mut socket, frame).await {
                return close(socket, refusal).await;
            }
            frame = match tokio::time::timeout(self.policy.idle_deadline, next_frame(&mut socket))
                .await
            {
                Err(_) => return close(socket, ChannelStreamRefusal::IdleDeadline).await,
                Ok(Inbound::Closed) => return,
                Ok(Inbound::Refused(refusal)) => return close(socket, refusal).await,
                Ok(Inbound::Frame(frame)) => frame,
            };
        }
    }

    async fn exchange(
        &self,
        socket: &mut WebSocket,
        frame: Vec<u8>,
    ) -> Result<(), ChannelStreamRefusal> {
        let owner = Arc::clone(&self.owner);
        let placement = self.placement_thread_id.clone();
        let occurrence = self.occurrence_id.clone();
        // Every exchange first confirms the channel is still live, so a
        // terminated allocation ends the stream at its next frame.
        let exchanged = tokio::task::spawn_blocking(move || {
            if let Err(refusal) = owner.admit(&placement, &occurrence) {
                return Ok(Err(refusal));
            }
            let exchanged = owner
                .exchange(&placement, &occurrence, &frame)
                .and_then(|response| encode_external_channel_stream_response(&response));
            match exchanged {
                Ok(message) => Ok(Ok(message)),
                Err(error) => {
                    // A frame that refuses the lease records the refusal
                    // durably; the standing it left names the verdict.
                    if let Err(ChannelStreamRefusal::OccurrenceRefused) =
                        owner.admit(&placement, &occurrence)
                    {
                        return Ok(Err(ChannelStreamRefusal::OccurrenceRefused));
                    }
                    Err(error)
                }
            }
        })
        .await;
        let message = match exchanged {
            Ok(Ok(Ok(message))) => message,
            Ok(Ok(Err(refusal))) => return Err(refusal),
            Ok(Err(error)) => {
                tracing::warn!(
                    placement = %self.placement_thread_id,
                    error = %format!("{error:#}"),
                    "external channel stream exchange refused"
                );
                return Err(if ryeos_app::external_placement::ExternalChannelExchangeUnavailable::marks(&error) {
                    ChannelStreamRefusal::ExchangeUnavailable
                } else {
                    ChannelStreamRefusal::ExchangeRefused
                });
            }
            Err(error) => {
                tracing::warn!(error = %error, "external channel stream exchange task failed");
                return Err(ChannelStreamRefusal::ExchangeRefused);
            }
        };
        if message.len() > self.maximum_response_bytes {
            return Err(ChannelStreamRefusal::ResponseBound);
        }
        // A failed send means the peer is gone; the read that follows sees it.
        if let Err(error) = socket.send(Message::Binary(message)).await {
            tracing::debug!(error = %error, "external channel stream response was not delivered");
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "channel_stream_mode_tests.rs"]
mod tests;
