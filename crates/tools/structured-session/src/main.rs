use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, anyhow, bail};
use lillux::time::{Duration, MonotonicDeadline};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

mod workload_client_broker;

const WIRE_PROTOCOL: &str = "ryeos.structured-session";
const WIRE_VERSION: u32 = 2;
const OBSERVATION_ACK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_APP_SERVER_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENTS: usize = 4_096;
const MAX_PENDING_SERVER_REQUESTS: usize = 128;
/// HTTP routes and server-request replies share this finite executor.  A
/// provider cannot turn a backlog of long requests into unbounded bridge
/// threads, while several workers preserve full-duplex event handling.
const HTTP_REQUEST_WORKERS: usize = 4;
const HTTP_REQUEST_QUEUE_CAPACITY: usize = 16;
const APPROVAL_TTL: Duration = Duration::from_secs(15 * 60);
/// A recorded server-request decision must be delivered promptly or fail the
/// session. It must not occupy one of the long-running route workers for the
/// general turn deadline.
const SERVER_REQUEST_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
// A route that is gated by an admitted server request must remain alive long
// enough for that request to expire and send its fail-closed upstream reply.
// The enclosing persistent-session contract admits a one-hour request bound.
const ROUTE_CALL_TIMEOUT: Duration = Duration::from_secs(16 * 60);

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FrameKind {
    Ready,
    Request,
    Control,
    Delta,
    Final,
    Error,
    Cancel,
    ObservationBatch,
    ObservationAck,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    protocol: String,
    version: u32,
    kind: FrameKind,
    request_id: Option<String>,
    body: Option<Value>,
}

struct StructuredWorkload {
    child: Child,
    io: WorkloadIo,
    incoming: Receiver<Result<Value, String>>,
    responses: HashMap<String, Value>,
    server_requests: HashMap<String, PendingServerRequest>,
    workload_channel: Option<workload_client_broker::WorkloadClientChannel>,
    workload_invocations: Vec<PendingWorkloadInvocation>,
    expired_server_requests: VecDeque<ExpiredServerRequest>,
    seen_server_request_ids: HashSet<String>,
    events: Arc<Mutex<EventQueue>>,
    pending_controls: Receiver<PendingControl>,
    control_results: SyncSender<WorkloadCommandResult>,
    command_progress: Option<String>,
    active_progress_notifications: Vec<String>,
    early_command_observations: Vec<Value>,
    next_id: u64,
    fatal: Option<String>,
    workspace: String,
    workload_home: String,
    ceremony_active: bool,
    bound_session_id: Option<String>,
    profile: StructuredSessionProfile,
    route_set: String,
    allowed_effect_classes: HashSet<RouteEffectClass>,
    schemas: HashMap<String, serde_json::Value>,
    outstanding: HashSet<String>,
    response_bytes: usize,
}

#[derive(Default)]
struct EventQueue {
    events: VecDeque<Value>,
    bytes: usize,
    command_progress: Option<CommandProgressBarrier>,
}

struct CommandProgressBarrier {
    request_id: String,
    digest: String,
    acknowledged: bool,
    deadline: MonotonicDeadline,
}

struct PendingControl {
    request_id: String,
    body: Value,
}

struct PendingServerRequest {
    message: Value,
    request_digest: String,
    expires_at: MonotonicDeadline,
}

struct PendingWorkloadInvocation {
    rpc_id: Value,
    response_schema: String,
    response_template: ValueTemplate,
    delivery: WorkloadInvocationDelivery,
}

enum WorkloadInvocationDelivery {
    AwaitingProgress {
        source: ryeos_runtime::workload_client::WorkloadInvocationSource,
        request: ryeos_runtime::workload_client::WorkloadClientRequestFrame,
    },
    Submitted(Receiver<ryeos_runtime::workload_client::WorkloadClientResponseFrame>),
}

/// The admitted workload transport. Stdio exchanges line-delimited
/// JSON-RPC over the child's pipes; HTTP runs a loopback server owned by
/// the child, discovered from its stdout listening line, with requests and
/// server-sent events carrying the same message envelopes.
enum WorkloadIo {
    Stdio(ChildStdin),
    Http(HttpWorkload),
}

struct HttpWorkload {
    base_url: String,
    client: reqwest::blocking::Client,
    server_request_client: reqwest::blocking::Client,
    authorization: String,
    ignored_notification_projection: HttpEventProjection,
    requests: SyncSender<HttpRequest>,
}

/// One admitted loopback exchange. A route request settles through the normal
/// id-correlated incoming stream; a reply failure is fatal because RyeOS can
/// no longer establish the decision it recorded for the upstream workload.
struct HttpRequest {
    client: reqwest::blocking::Client,
    url: String,
    method: String,
    authorization: String,
    body: Value,
    id: Option<Value>,
}

struct ExpiredServerRequest {
    id: String,
    request_digest: String,
}

struct PendingObservationBatch {
    through_sequence: u64,
    digest: String,
    deadline: MonotonicDeadline,
}

#[derive(Debug)]
enum WorkloadCommandOutput {
    Delta(Value),
    Final(Value),
}
type WorkloadCommandResult = (String, std::result::Result<WorkloadCommandOutput, String>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredSessionProfile {
    schema_version: u32,
    transport: ProfileTransport,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    http_sse: Option<HttpSseCredentials>,
    configuration_authority: ConfigurationAuthority,
    workload_realization_id: String,
    workload_executable: String,
    workload_args: Vec<String>,
    workload_home_env: String,
    required_process_environment: Vec<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    workload_client: Option<StructuredSessionWorkloadClient>,
    baseline_config: String,
    baseline_destination: String,
    portable_state: Option<ryeos_state::objects::PortableSessionStateContract>,
    credential_subject: Option<ryeos_state::objects::CredentialSubjectProjectionContract>,
    initialization: Vec<InitializationStep>,
    recovery: Option<RecoveryRule>,
    route_sets: BTreeMap<String, Vec<String>>,
    routes: Vec<RouteRule>,
    notifications: Vec<NotificationRule>,
    #[serde(default)]
    ignored_notifications: BTreeMap<String, String>,
    server_requests: Vec<ServerRequestRule>,
}

/// The workload transport selected by the admitted profile. The set mirrors
/// the admission compiler exactly; the bridge never discovers a transport.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
enum ProfileTransport {
    #[serde(rename = "stdio_jsonrpc")]
    StdioJsonRpc,
    #[serde(rename = "http_sse")]
    HttpSse,
}

/// Environment names through which the bridge supplies per-boot HTTP basic
/// authentication to the workload server. The bridge generates both values;
/// the profile supplies only the names, so no workload-specific spelling
/// lives in this bridge.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpSseCredentials {
    username_env: String,
    password_env: String,
    #[serde(default)]
    seed_path_env: Option<String>,
    listener_stdout_prefix: String,
    readiness_path: String,
    readiness_schema: String,
    event_path: String,
    event_type_pointer: String,
    event_properties_pointer: String,
    ignored_notification_projection: HttpEventProjection,
}

/// Exact signed representation for schemas applied to ignored HTTP/SSE
/// notifications. Normal notification and server-request rules always use
/// the signed properties projection, because their templates are evaluated
/// against `message.params`.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HttpEventProjection {
    Envelope,
    Properties,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredSessionWorkloadClient {
    #[serde(deserialize_with = "deserialize_required_nullable")]
    cli_endpoint_env: Option<String>,
    #[serde(deserialize_with = "deserialize_required_nullable")]
    structured_session:
        Option<ryeos_engine::structured_session_profile::StructuredSessionInvocationMapping>,
}

fn deserialize_required_nullable<'de, D, T>(
    deserializer: D,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

/// Mechanical source of workload configuration authority. Immutable argv is
/// retained in the signed profile, verified by its admitted digest, and
/// supplied by the bridge itself; the same-UID workload cannot rewrite it.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ConfigurationAuthority {
    ImmutableArgv,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializationStep {
    method: String,
    effect_class: RouteEffectClass,
    params: Value,
    response_schema: Option<String>,
    notification: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteRule {
    id: String,
    method: String,
    #[serde(default)]
    audience: RouteAudience,
    effect_class: RouteEffectClass,
    request_schema: String,
    response_schema: String,
    #[serde(default)]
    fixed_params: BTreeMap<String, Value>,
    #[serde(default)]
    workspace_fields: Vec<String>,
    #[serde(default)]
    forbidden_non_null_fields: Vec<String>,
    #[serde(default)]
    forbidden_fields: Vec<String>,
    #[serde(default)]
    response_predicates: Vec<ValuePredicate>,
    #[serde(default)]
    observations: Vec<ObservationRule>,
    result_retention: ResultRetention,
    #[serde(default)]
    ceremony: Option<CeremonyAction>,
    #[serde(default)]
    session_binding: Option<SessionBindingRule>,
    #[serde(default)]
    post_success_routes: Vec<String>,
    #[serde(default)]
    progress_notifications: Vec<String>,
    #[serde(default)]
    http_method: Option<String>,
    #[serde(default)]
    http_path: Option<String>,
    #[serde(default)]
    http_body_schema: Option<String>,
    #[serde(default)]
    http_path_parameters: BTreeMap<String, HttpPathParameter>,
}

/// Signed source of one HTTP path segment.  The bridge never treats a path
/// template as permission to smuggle arbitrary request JSON into addressing.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HttpPathParameter {
    source: HttpPathParameterSource,
    #[serde(default)]
    field: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HttpPathParameterSource {
    Input,
    BoundSession,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionBindingRule {
    action: SessionBindingAction,
    request_field: Option<String>,
    response_pointer: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryRule {
    resume_route: String,
    inspect_route: String,
    route_sets: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum SessionBindingAction {
    BindNew,
    BindExpected,
    Require,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RouteAudience {
    #[default]
    Public,
    Runtime,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CeremonyAction {
    Start,
    Clear,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
enum RouteEffectClass {
    PureRead,
    SessionMutation,
    ExternalEffect,
    CredentialRead,
    CredentialWrite,
    CredentialDelete,
}

impl RouteEffectClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::PureRead => "pure_read",
            Self::SessionMutation => "session_mutation",
            Self::ExternalEffect => "external_effect",
            Self::CredentialRead => "credential_read",
            Self::CredentialWrite => "credential_write",
            Self::CredentialDelete => "credential_delete",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResultRetention {
    Ephemeral,
    Durable,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationRule {
    method: String,
    schema: String,
    upstream_session_pointer: Option<String>,
    event_type: String,
    durable: bool,
    payload: ValueTemplate,
    #[serde(default)]
    observations: Vec<ObservationRule>,
    #[serde(default)]
    ceremony_clear: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerRequestRule {
    method: String,
    schema: String,
    operation_class: String,
    correlation: ServerRequestCorrelation,
    responses: ApprovalResponses,
    #[serde(default)]
    deny_only: bool,
    #[serde(default)]
    permission_delta_fields: Vec<String>,
    #[serde(default)]
    required_review_fields: Vec<String>,
    #[serde(default)]
    reply_http_path: Option<String>,
    display: ValueTemplate,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerRequestCorrelation {
    upstream_session_pointer: String,
    operation_pointer: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalResponses {
    accept: ValueTemplate,
    cancel: ValueTemplate,
    decline: ValueTemplate,
    expire: ValueTemplate,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationRule {
    #[serde(default)]
    when: Vec<ValuePredicate>,
    value: ValueTemplate,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ValuePredicate {
    pointer: String,
    equals: Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum ValueTemplate {
    Literal {
        value: Value,
    },
    Pointer {
        pointer: String,
        #[serde(default)]
        optional: bool,
        #[serde(default = "default_template_string_limit")]
        max_string_bytes: usize,
    },
    Object {
        fields: BTreeMap<String, ValueTemplate>,
    },
    Array {
        values: Vec<ValueTemplate>,
    },
    Digest {
        pointer: String,
    },
    JsonString {
        pointer: String,
        max_bytes: usize,
    },
}

fn default_template_string_limit() -> usize {
    64 * 1024
}

fn validate_structured_session_profile(profile: &StructuredSessionProfile) -> Result<()> {
    fn identifier(label: &str, value: &str) -> Result<()> {
        if value.is_empty()
            || value.len() > 256
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/')
            })
        {
            bail!("{label} is not a bounded portable identifier");
        }
        Ok(())
    }
    fn file_name(label: &str, value: &str) -> Result<()> {
        let path = std::path::Path::new(value);
        if path.components().count() != 1
            || !matches!(
                path.components().next(),
                Some(std::path::Component::Normal(_))
            )
        {
            bail!("{label} must be one relative file name");
        }
        Ok(())
    }
    ryeos_engine::protocol_vocabulary::validate_env_name(&profile.workload_home_env)
        .map_err(|error| anyhow!(error))?;
    let http_transport = profile.transport == ProfileTransport::HttpSse;
    if http_transport != profile.http_sse.is_some() {
        bail!("structured-session HTTP credential block contradicts its transport");
    }
    if let Some(credentials) = &profile.http_sse {
        for name in [&credentials.username_env, &credentials.password_env] {
            ryeos_engine::protocol_vocabulary::validate_env_name(name)
                .map_err(|error| anyhow!(error))?;
        }
        if credentials.listener_stdout_prefix.is_empty()
            || credentials.listener_stdout_prefix.len() > 512
            || credentials
                .listener_stdout_prefix
                .chars()
                .any(char::is_control)
            || !credentials.readiness_path.starts_with('/')
            || !credentials.event_path.starts_with('/')
            || credentials.readiness_schema.is_empty()
            || credentials.event_type_pointer.is_empty()
            || credentials.event_properties_pointer.is_empty()
        {
            bail!("structured-session HTTP boot contract is invalid");
        }
    }
    if http_transport && !profile.initialization.is_empty() {
        bail!("structured-session HTTP transport admits no initialization handshake");
    }
    for route in &profile.routes {
        if route.http_method.is_some() != http_transport
            || route.http_path.is_some() != http_transport
            || route.http_body_schema.is_some() != http_transport
        {
            bail!("structured-session route HTTP addressing contradicts its transport");
        }
    }
    for request in &profile.server_requests {
        if request.reply_http_path.is_some() != http_transport {
            bail!("structured-session reply HTTP path contradicts its transport");
        }
    }
    if let Some(workload_client) = &profile.workload_client {
        if workload_client
            .cli_endpoint_env
            .as_deref()
            .is_some_and(|endpoint| {
                endpoint != ryeos_runtime::workload_client::WORKLOAD_CLIENT_ENDPOINT_ENV
            })
        {
            bail!("structured-session workload-client endpoint environment is not current");
        }
    }
    if profile.configuration_authority != ConfigurationAuthority::ImmutableArgv {
        bail!("structured-session configuration authority is not immutable argv");
    }
    validate_workload_executable_member(&profile.workload_executable)?;
    if let Some(contract) = &profile.portable_state {
        contract.validate()?;
    }
    if let Some(contract) = &profile.credential_subject {
        contract.validate()?;
    }
    file_name(
        "structured-session baseline destination",
        &profile.baseline_destination,
    )?;
    if profile.workload_args.len() > 64
        || profile
            .workload_args
            .iter()
            .any(|arg| arg.len() > 4096 || arg.chars().any(char::is_control))
        || profile.initialization.len() > 8
        || profile.routes.len() > 128
        || profile.notifications.len() + profile.ignored_notifications.len() > 256
        || profile.server_requests.len() > 32
        || profile.route_sets.len() > 16
    {
        bail!("structured-session profile exceeds a mechanical count bound");
    }
    let mut route_ids = HashSet::new();
    let mut methods = HashSet::new();
    for route in &profile.routes {
        identifier("structured-session route id", &route.id)?;
        identifier("structured-session route method", &route.method)?;
        if !route_ids.insert(route.id.as_str()) {
            bail!("structured-session profile contains a duplicate route id");
        }
        if !methods.insert(route.method.as_str()) {
            bail!("structured-session profile maps one upstream method more than once");
        }
        if route.fixed_params.len() > 32
            || route.workspace_fields.len() > 8
            || route.forbidden_non_null_fields.len() > 32
            || route.forbidden_fields.len() > 32
            || route.observations.len() > 16
            || route.post_success_routes.len() > 8
        {
            bail!("structured-session route exceeds a mapping bound");
        }
    }
    for (route_set, routes) in &profile.route_sets {
        identifier("structured-session route set", route_set)?;
        if routes.is_empty()
            || routes.len() > 128
            || routes
                .iter()
                .any(|route| !route_ids.contains(route.as_str()))
        {
            bail!("structured-session route set contains an unknown or invalid route");
        }
    }
    for route in &profile.routes {
        let mut post_routes = HashSet::new();
        for post_route_id in &route.post_success_routes {
            identifier("structured-session post-success route", post_route_id)?;
            if post_route_id == &route.id || !post_routes.insert(post_route_id.as_str()) {
                bail!("structured-session post-success route graph is not acyclic and unique");
            }
            let post_route = profile
                .routes
                .iter()
                .find(|candidate| &candidate.id == post_route_id)
                .ok_or_else(|| anyhow!("structured-session post-success route is absent"))?;
            if post_route.audience != RouteAudience::Runtime
                || post_route
                    .session_binding
                    .as_ref()
                    .map(|binding| binding.action)
                    != Some(SessionBindingAction::Require)
                || !post_route.post_success_routes.is_empty()
                || !post_route.observations.is_empty()
                || post_route.ceremony.is_some()
                || !matches!(post_route.result_retention, ResultRetention::Ephemeral)
            {
                bail!(
                    "structured-session post-success route is not an inert runtime-only binding operation"
                );
            }
        }
        for routes in profile.route_sets.values() {
            if routes.contains(&route.id)
                && route
                    .post_success_routes
                    .iter()
                    .any(|post_route| !routes.contains(post_route))
            {
                bail!("structured-session post-success route escapes its source route set");
            }
        }
    }
    if let Some(recovery) = &profile.recovery {
        identifier(
            "structured-session recovery resume route",
            &recovery.resume_route,
        )?;
        identifier(
            "structured-session recovery inspect route",
            &recovery.inspect_route,
        )?;
        if recovery.resume_route == recovery.inspect_route
            || recovery.route_sets.is_empty()
            || recovery.route_sets.len() > 16
            || recovery
                .route_sets
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            bail!("structured-session recovery contract is not canonical");
        }
        for route_set in &recovery.route_sets {
            let routes = profile
                .route_sets
                .get(route_set)
                .ok_or_else(|| anyhow!("structured-session recovery route set is absent"))?;
            if !routes.contains(&recovery.resume_route) || !routes.contains(&recovery.inspect_route)
            {
                bail!("structured-session recovery routes are outside their admitted route set");
            }
        }
        for (route_id, action) in [
            (&recovery.resume_route, SessionBindingAction::BindExpected),
            (&recovery.inspect_route, SessionBindingAction::Require),
        ] {
            let route = profile
                .routes
                .iter()
                .find(|route| &route.id == route_id)
                .ok_or_else(|| anyhow!("structured-session recovery route is absent"))?;
            if route.audience != RouteAudience::Runtime
                || route.session_binding.as_ref().map(|binding| binding.action) != Some(action)
            {
                bail!("structured-session recovery route has the wrong audience or binding");
            }
        }
    }
    let mut notification_methods = HashSet::new();
    for notification in &profile.notifications {
        identifier(
            "structured-session notification method",
            &notification.method,
        )?;
        identifier("structured-session event type", &notification.event_type)?;
        if !notification_methods.insert(notification.method.as_str()) {
            bail!("structured-session profile contains a duplicate notification method");
        }
    }
    let mut request_methods = HashSet::new();
    for request in &profile.server_requests {
        identifier("structured-session server-request method", &request.method)?;
        identifier(
            "structured-session operation class",
            &request.operation_class,
        )?;
        if !request_methods.insert(request.method.as_str()) {
            bail!("structured-session profile contains a duplicate server-request method");
        }
    }
    for (method, schema) in &profile.ignored_notifications {
        identifier("structured-session ignored notification method", method)?;
        if schema.is_empty() || notification_methods.contains(method.as_str()) {
            bail!("structured-session ignored notification is invalid or duplicated");
        }
    }
    Ok(())
}

fn load_profile_schemas(
    profile_root: &std::path::Path,
    profile: &StructuredSessionProfile,
) -> Result<HashMap<String, Value>> {
    let mut identities = HashSet::new();
    for step in &profile.initialization {
        if let Some(schema) = &step.response_schema {
            identities.insert(schema.clone());
        }
    }
    for route in &profile.routes {
        identities.insert(route.request_schema.clone());
        identities.insert(route.response_schema.clone());
    }
    for notification in &profile.notifications {
        identities.insert(notification.schema.clone());
    }
    identities.extend(profile.ignored_notifications.values().cloned());
    for request in &profile.server_requests {
        identities.insert(request.schema.clone());
    }
    if let Some(mapping) = profile
        .workload_client
        .as_ref()
        .and_then(|client| client.structured_session.as_ref())
    {
        identities.insert(mapping.request_schema.clone());
        identities.insert(mapping.response_schema.clone());
    }
    if identities.len() > 512 {
        bail!("structured-session profile references too many schemas");
    }
    let mut output = HashMap::new();
    let mut total = 0usize;
    for identity in identities {
        let relative = std::path::Path::new(&identity);
        if relative.is_absolute()
            || relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            bail!("structured-session schema identity is not a safe local path");
        }
        let path = profile_root.join(relative);
        let bytes = lillux::read_regular_file_bounded_no_follow(&path, 8 * 1024 * 1024)
            .with_context(|| format!("read admitted schema `{identity}` through Lillux"))?;
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| anyhow!("structured-session schema bytes overflow"))?;
        if total > 16 * 1024 * 1024 {
            bail!("structured-session schemas exceed their aggregate byte ceiling");
        }
        let schema: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("decode admitted schema `{identity}`"))?;
        reject_nonlocal_schema_refs(&schema, 0)?;
        jsonschema::validator_for(&schema)
            .map_err(|error| anyhow!("compile admitted schema `{identity}`: {error}"))?;
        output.insert(identity, schema);
    }
    Ok(output)
}

fn reject_nonlocal_schema_refs(value: &Value, depth: usize) -> Result<()> {
    if depth > 128 {
        bail!("structured-session schema exceeds nesting bound");
    }
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str)
                && !reference.starts_with("#/")
            {
                bail!("structured-session schema contains a non-local reference");
            }
            for value in object.values() {
                reject_nonlocal_schema_refs(value, depth + 1)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                reject_nonlocal_schema_refs(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ryeos-structured-session-bridge: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    lillux::disable_process_core_dumps().map_err(anyhow::Error::msg)?;
    let workspace = required_env("RYEOS_WORKSPACE")?;
    let workload_home = required_env("RYEOS_WORKLOAD_HOME")?;
    let route_set = required_env("RYEOS_STRUCTURED_SESSION_ROUTE_SET")?;
    let allowed_effect_classes = required_env("RYEOS_STRUCTURED_SESSION_EFFECT_CLASSES")?
        .split(',')
        .map(|effect| match effect {
            "pure_read" => Ok(RouteEffectClass::PureRead),
            "session_mutation" => Ok(RouteEffectClass::SessionMutation),
            "external_effect" => Ok(RouteEffectClass::ExternalEffect),
            "credential_read" => Ok(RouteEffectClass::CredentialRead),
            "credential_write" => Ok(RouteEffectClass::CredentialWrite),
            "credential_delete" => Ok(RouteEffectClass::CredentialDelete),
            _ => bail!("structured-session effect-class ceiling is not canonical"),
        })
        .collect::<Result<HashSet<_>>>()?;
    let boot_identity = required_env("RYEOS_SESSION_BOOT_IDENTITY")?;
    require_absolute_normalized("workspace", &workspace)?;
    require_absolute_normalized("workload home", &workload_home)?;
    if workspace == workload_home || workspace.starts_with(&(workload_home.clone() + "/")) {
        bail!("workspace must not be inside the workload home");
    }
    let profile_path = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .ok_or_else(|| anyhow!("structured-session bridge requires an admitted profile"))?;
    let profile_bytes = lillux::read_regular_file_bounded_no_follow(&profile_path, 64 * 1024)
        .context("read structured-session profile through Lillux")?;
    if profile_bytes.is_empty() || profile_bytes.len() > 64 * 1024 {
        bail!("structured-session profile is empty or exceeds its bound");
    }
    let profile: StructuredSessionProfile =
        serde_json::from_slice(&profile_bytes).context("decode structured-session profile")?;
    if profile.schema_version
        != ryeos_engine::structured_session_profile::STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION
    {
        bail!("unsupported structured-session profile schema");
    }
    validate_structured_session_profile(&profile)?;
    let profile_digest = ryeos_state::objects::canonical_value_digest(&serde_json::from_slice::<
        Value,
    >(&profile_bytes)?)?;
    let expected = required_env("RYEOS_STRUCTURED_SESSION_PROFILE_HASH")?;
    if expected != profile_digest {
        bail!("structured-session profile differs from its admitted digest");
    }
    if !profile.route_sets.contains_key(&route_set) {
        bail!("structured-session route set is not admitted by the profile");
    }
    let executable_name = std::path::Path::new(&profile.workload_executable);
    let baseline_name = std::path::Path::new(&profile.baseline_config);
    if baseline_name.components().count() != 1
        || !matches!(
            baseline_name.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        bail!("structured-session baseline config must be one relative file name");
    }
    let baseline_config = profile_path
        .parent()
        .ok_or_else(|| anyhow!("structured-session profile has no parent"))?
        .join(baseline_name);
    let external_root = required_env("RYEOS_EXTERNAL_ROOT")?;
    let external_realizations = required_env("RYEOS_EXTERNAL_REALIZATIONS")?;
    require_absolute_normalized("external realization root", &external_root)?;
    let (executable, workload_argv0, mut workload_handles) = resolve_pinned_executable(
        std::path::Path::new(&external_root),
        &external_realizations,
        &profile.workload_realization_id,
        executable_name,
    )?;
    let executable_search = optional_env("RYEOS_EXECUTABLE_SEARCH")?;
    let (executable_path, mut inherited_descriptors) = resolve_pinned_executable_search(
        std::path::Path::new(&external_root),
        &external_realizations,
        executable_search.as_deref(),
    )?;
    let session_process_environment =
        optional_env(ryeos_state::objects::SESSION_PROCESS_ENVIRONMENT_ENV)?;
    let (mut session_process_environment, mut environment_descriptors) =
        resolve_session_process_environment(
            std::path::Path::new(&workspace),
            std::path::Path::new(&external_root),
            &external_realizations,
            session_process_environment.as_deref(),
        )?;
    inherited_descriptors.append(&mut environment_descriptors);
    inherited_descriptors.append(&mut workload_handles);
    let workload_client_broker = if std::env::var_os(
        ryeos_runtime::workload_client::WORKLOAD_CLIENT_CHANNEL_ENV,
    )
    .is_some()
    {
        let workload_client = profile.workload_client.as_ref().ok_or_else(|| {
            anyhow!("workload-client channel was supplied to a profile that did not admit it")
        })?;
        // SAFETY: the admitted target-channel plan gives this bridge unique
        // ownership of the named connected descriptor. Lillux owns adoption
        // and immediately prevents inheritance into the untrusted workload.
        let channel = unsafe {
            lillux::take_inherited_duplex_channel_from_env(
                ryeos_runtime::workload_client::WORKLOAD_CLIENT_CHANNEL_ENV,
            )
            .map_err(anyhow::Error::msg)?
        };
        let mut supported = Vec::new();
        if workload_client.cli_endpoint_env.is_some() {
            supported.push(ryeos_runtime::workload_client::WorkloadClientIngress::Cli);
        }
        if workload_client.structured_session.is_some() {
            supported
                .push(ryeos_runtime::workload_client::WorkloadClientIngress::StructuredSession);
        }
        let broker = workload_client_broker::start(channel, &supported)?;
        if let Some(endpoint) = broker.endpoint() {
            let endpoint_env = workload_client
                .cli_endpoint_env
                .as_ref()
                .ok_or_else(|| anyhow!("CLI ingress lacks admitted endpoint environment"))?;
            if session_process_environment
                .insert(endpoint_env.clone(), endpoint.to_owned())
                .is_some()
            {
                bail!("workload-client endpoint collided with admitted process environment");
            }
        }
        Some(broker)
    } else {
        None
    };
    verify_compatibility_baseline_config(
        std::path::Path::new(&workload_home),
        &baseline_config,
        &profile.baseline_destination,
    )?;
    let schemas = load_profile_schemas(
        profile_path
            .parent()
            .ok_or_else(|| anyhow!("structured-session profile has no parent"))?,
        &profile,
    )?;
    // SAFETY: the signed launch protocol gives this process unique ownership
    // of the connected session descriptor minted by the typed isolation
    // authority. Lillux performs the raw ownership conversion and immediately
    // protects the adopted channel from further inheritance.
    let mut channel = unsafe {
        lillux::take_inherited_duplex_channel_from_env("RYEOS_SESSION_FD")
            .map_err(anyhow::Error::msg)?
    };
    let events = Arc::new(Mutex::new(EventQueue::default()));
    let (workload_result_sender, workload_results) = sync_channel::<WorkloadCommandResult>(32);
    let (pending_control_sender, pending_controls) = sync_channel::<PendingControl>(32);
    for name in &profile.required_process_environment {
        if !session_process_environment.contains_key(name) {
            bail!("required process environment `{name}` is absent from admitted session inputs");
        }
    }
    let mut app = StructuredWorkload::start(
        executable.to_str().ok_or_else(|| {
            anyhow!("pinned structured-session workload executable path is not UTF-8")
        })?,
        &workspace,
        &workload_home,
        profile,
        route_set,
        allowed_effect_classes,
        schemas,
        Arc::clone(&events),
        pending_controls,
        workload_result_sender.clone(),
        workload_argv0
            .to_str()
            .ok_or_else(|| anyhow!("descriptor-rooted workload argv[0] is not valid UTF-8"))?,
        executable_path.as_deref(),
        &session_process_environment,
        inherited_descriptors,
    )?;
    // Retain the broker owner for the complete workload lifetime. Its worker
    // threads retain the listener and protected channel; this guard documents
    // that their endpoint is scoped to this bridge boot.
    app.workload_channel = workload_client_broker
        .as_ref()
        .map(|broker| broker.channel());
    let _workload_client_broker = workload_client_broker;
    if let Err(error) = app.initialize() {
        // Upstream stderr can contain credentials and must stay private.
        // Report only the exact child's OS status, never its output. This is
        // diagnostic context, not daemon-owned cleanup/settlement testimony.
        return Err(match app.child.try_wait() {
            Ok(Some(status)) => error.context(format!(
                "structured workload initialization failed; child status: {status}"
            )),
            Ok(None) => error
                .context("structured workload initialization failed; child exit not yet observed"),
            Err(_) => {
                error.context("structured workload initialization failed; child status unavailable")
            }
        });
    }
    protect_profile_home(std::path::Path::new(&workload_home))?;
    let mut workload_termination = Some(
        lillux::CooperativeChildTermination::for_child(&app.child)
            .map_err(anyhow::Error::msg)
            .context("pin structured-session workload termination authority")?,
    );
    let app = Arc::new(Mutex::new(app));
    write_frame(
        &mut channel,
        &Frame {
            protocol: WIRE_PROTOCOL.to_owned(),
            version: WIRE_VERSION,
            kind: FrameKind::Ready,
            request_id: None,
            body: Some(json!({"boot_identity":boot_identity})),
        },
    )?;
    let reader = channel
        .try_clone()
        .context("clone RyeOS session reader descriptor")?;
    let (session_sender, session_incoming) = sync_channel(128);
    thread::Builder::new()
        .name("ryeos-session-control-reader".to_owned())
        .spawn(move || read_session_frames(reader, session_sender))
        .context("start bounded RyeOS session reader")?;
    let mut next_observation_sequence = 1u64;
    let mut previous_observation_digest: Option<String> = None;
    let mut pending_observation: Option<PendingObservationBatch> = None;
    let mut active_request: Option<String> = None;
    let mut active_controls = HashSet::new();
    let mut cancelled_requests = HashSet::new();
    let mut cancelled_workload = false;
    let mut pending_workload_termination: Option<(
        String,
        lillux::PendingCooperativeChildTermination,
    )> = None;
    let mut pending_cancelled_responses: HashSet<String> = HashSet::new();
    let mut pending_cancellation_protocol_error: Option<&'static str> = None;
    loop {
        let workload_terminated = pending_workload_termination
            .as_ref()
            .map(|(_, pending)| pending.has_exited().map_err(anyhow::Error::msg))
            .transpose()
            .context("observe cooperatively terminated structured-session workload")?
            .unwrap_or(false);
        if workload_terminated {
            let (request_id, _) = pending_workload_termination
                .take()
                .expect("checked pending workload termination");
            write_error(&mut channel, &request_id, "request cancelled")?;
            for pending_id in pending_cancelled_responses.drain() {
                write_error(&mut channel, &pending_id, "request cancelled")?;
            }
            if let Some(reason) = pending_cancellation_protocol_error.take() {
                bail!("{reason}");
            }
        }
        if pending_workload_termination.is_some() {
            match session_incoming.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(frame)) => {
                    if validate_incoming_frame(&frame).is_ok()
                        && matches!(frame.kind, FrameKind::Request | FrameKind::Control)
                    {
                        let Some(request_id) = frame.request_id.as_deref() else {
                            pending_cancellation_protocol_error.get_or_insert(
                                "request received during cancellation has no request id",
                            );
                            continue;
                        };
                        let cancelled_request_id = &pending_workload_termination
                            .as_ref()
                            .expect("checked pending workload termination")
                            .0;
                        if request_id == cancelled_request_id
                            || pending_cancelled_responses.contains(request_id)
                        {
                            pending_cancellation_protocol_error.get_or_insert(
                                "daemon reused a pending cancelled structured-session request id",
                            );
                        } else if pending_cancelled_responses.len() >= MAX_PENDING_SERVER_REQUESTS {
                            pending_cancellation_protocol_error.get_or_insert(
                                "structured-session cancellation response backlog is exhausted",
                            );
                        } else {
                            pending_cancelled_responses.insert(request_id.to_owned());
                        }
                    } else {
                        pending_cancellation_protocol_error.get_or_insert(
                            "invalid or unexpected structured-session frame during cancellation",
                        );
                    }
                }
                Ok(Err(_)) => {
                    pending_cancellation_protocol_error
                        .get_or_insert("RyeOS session reader failed during cancellation");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    pending_cancellation_protocol_error
                        .get_or_insert("RyeOS session reader disconnected during cancellation");
                    lillux::time::sleep(Duration::from_millis(50));
                }
            }
            continue;
        }
        while let Ok((request_id, outcome)) = workload_results.try_recv() {
            if cancelled_requests.contains(&request_id) {
                // Progress is not settlement: retain the cancellation marker
                // until this command's terminal output has also been drained.
                if !matches!(&outcome, Ok(WorkloadCommandOutput::Delta(_))) {
                    cancelled_requests.remove(&request_id);
                }
                continue;
            }
            let outcome = match outcome {
                Ok(WorkloadCommandOutput::Delta(body)) => {
                    if active_request.as_deref() != Some(request_id.as_str()) {
                        bail!("structured-session progress names an inactive command");
                    }
                    write_frame(
                        &mut channel,
                        &Frame {
                            protocol: WIRE_PROTOCOL.to_owned(),
                            version: WIRE_VERSION,
                            kind: FrameKind::Delta,
                            request_id: Some(request_id),
                            body: Some(body),
                        },
                    )?;
                    continue;
                }
                Ok(WorkloadCommandOutput::Final(body)) => Ok(body),
                Err(error) => Err(error),
            };
            if active_controls.remove(&request_id) {
                match outcome {
                    Ok(result) => write_final(&mut channel, &request_id, result)?,
                    Err(error) => write_error(&mut channel, &request_id, &error)?,
                }
                continue;
            }
            if active_request.as_deref() != Some(request_id.as_str()) {
                bail!("structured-session workload settled an unknown request");
            }
            active_request = None;
            match outcome {
                Ok(result) => write_final(&mut channel, &request_id, result)?,
                Err(error) => write_error(&mut channel, &request_id, &error)?,
            }
        }
        match app.try_lock() {
            Ok(mut app) => {
                app.drain_incoming()?;
                app.expire_server_requests()?;
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
            Err(std::sync::TryLockError::Poisoned(_)) => {
                bail!("structured-session workload state is poisoned")
            }
        };
        if pending_observation.is_none() {
            let event_batch = events
                .lock()
                .map_err(|_| anyhow!("structured-session event queue is poisoned"))?
                .take_batch(
                    128,
                    next_observation_sequence,
                    previous_observation_digest.as_deref(),
                )?;
            if let Some((body, through_sequence, digest)) = event_batch {
                write_frame(
                    &mut channel,
                    &Frame {
                        protocol: WIRE_PROTOCOL.to_owned(),
                        version: WIRE_VERSION,
                        kind: FrameKind::ObservationBatch,
                        request_id: None,
                        body: Some(body),
                    },
                )?;
                pending_observation = Some(PendingObservationBatch {
                    through_sequence,
                    digest,
                    deadline: MonotonicDeadline::after(OBSERVATION_ACK_TIMEOUT),
                });
            }
        }
        if pending_observation
            .as_ref()
            .is_some_and(|pending| pending.deadline.has_elapsed())
        {
            bail!("RyeOS did not durably acknowledge the observation batch");
        }
        if events
            .lock()
            .map_err(|_| anyhow!("event queue is poisoned"))?
            .command_progress
            .as_ref()
            .is_some_and(|progress| !progress.acknowledged && progress.deadline.has_elapsed())
        {
            bail!("RyeOS did not durably acknowledge command progress");
        }
        let frame = match session_incoming.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(frame)) => frame,
            Ok(Err(reason)) => bail!("RyeOS session reader failed: {reason}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("RyeOS session reader disconnected")
            }
        };
        validate_incoming_frame(&frame)?;
        match frame.kind {
            FrameKind::Request | FrameKind::Control => {
                let request_id = frame
                    .request_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("request frame has no request id"))?;
                let body = frame.body.ok_or_else(|| anyhow!("request has no body"))?;
                if cancelled_workload {
                    write_error(
                        &mut channel,
                        request_id,
                        "structured-session workload was cancelled and is no longer reusable",
                    )?;
                    continue;
                }
                let control = matches!(frame.kind, FrameKind::Control);
                if active_request.is_some() {
                    if !control
                        || body.get("kind").and_then(Value::as_str) != Some("approval_decision")
                    {
                        bail!(
                            "only an approval decision may run full-duplex with an active structured-session request"
                        );
                    }
                    if !active_controls.insert(request_id.to_owned()) {
                        bail!("daemon reused an active structured-session control id");
                    }
                    match pending_control_sender.try_send(PendingControl {
                        request_id: request_id.to_owned(),
                        body,
                    }) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            bail!("structured-session approval control backlog is exhausted")
                        }
                        Err(TrySendError::Disconnected(_)) => {
                            bail!("structured-session command worker stopped accepting controls")
                        }
                    }
                    continue;
                }
                active_request = Some(request_id.to_owned());
                let request_id = request_id.to_owned();
                let workload = Arc::clone(&app);
                let sender = workload_result_sender.clone();
                let workspace = workspace.clone();
                thread::Builder::new()
                    .name("ryeos-structured-session-command".to_owned())
                    .spawn(move || {
                        let outcome = workload
                            .lock()
                            .map_err(|_| "structured-session workload state is poisoned".to_owned())
                            .and_then(|mut workload| {
                                workload.command_progress = Some(request_id.clone());
                                let result = if control {
                                    workload.handle_control(body)
                                } else {
                                    workload.handle(body, &workspace)
                                };
                                workload.command_progress = None;
                                result
                                    .map(WorkloadCommandOutput::Final)
                                    .map_err(|error| error.to_string())
                            });
                        let _ = sender.send((request_id, outcome));
                    })
                    .context("spawn structured-session command worker")?;
            }
            FrameKind::Cancel => {
                let request_id = frame
                    .request_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("cancel frame has no request id"))?;
                if active_request.as_deref() != Some(request_id) {
                    bail!("cancel frame does not name the active request");
                }
                // The generic upstream protocol has no universal cancellation
                // method. Terminating the exact pinned workload is the only
                // provider-neutral prompt cancellation boundary; this worker
                // epoch is not reused afterward.
                let pending = workload_termination
                    .take()
                    .ok_or_else(|| anyhow!("structured-session workload was already cancelled"))?
                    .request()
                    .map_err(anyhow::Error::msg)
                    .context("terminate cancelled structured-session workload")?;
                active_request = None;
                cancelled_requests.insert(request_id.to_owned());
                for control_id in active_controls.drain() {
                    cancelled_requests.insert(control_id.clone());
                    pending_cancelled_responses.insert(control_id);
                }
                pending_workload_termination = Some((request_id.to_owned(), pending));
                cancelled_workload = true;
            }
            FrameKind::ObservationAck => {
                if let Some(request_id) = frame.request_id.as_deref() {
                    let body = frame
                        .body
                        .as_ref()
                        .and_then(Value::as_object)
                        .ok_or_else(|| {
                            anyhow!("command progress acknowledgement is not an object")
                        })?;
                    require_exact_keys(body, &["command_progress_digest"])?;
                    events
                        .lock()
                        .map_err(|_| anyhow!("event queue is poisoned"))?
                        .acknowledge_command_progress(
                            request_id,
                            body["command_progress_digest"].as_str(),
                        )?;
                    continue;
                }
                let pending = pending_observation
                    .take()
                    .ok_or_else(|| anyhow!("unsolicited observation acknowledgement"))?;
                let body = frame
                    .body
                    .as_ref()
                    .and_then(Value::as_object)
                    .ok_or_else(|| anyhow!("observation acknowledgement body is not an object"))?;
                require_exact_keys(body, &["through_sequence", "batch_digest"])?;
                if body.get("through_sequence").and_then(Value::as_u64)
                    != Some(pending.through_sequence)
                    || body.get("batch_digest").and_then(Value::as_str)
                        != Some(pending.digest.as_str())
                {
                    bail!("observation acknowledgement does not match the contacted batch");
                }
                next_observation_sequence = pending
                    .through_sequence
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("observation sequence overflow"))?;
                previous_observation_digest = Some(pending.digest);
            }
            _ => bail!("daemon sent a non-request frame"),
        }
    }
}

/// Verify the seed prepared by the daemon's persistent-session launch owner.
/// Do not repair or replace it here: the enforced backend has already mounted
/// the exact admitted source read-only at this name, so even a same-content
/// rename would fail. All boot modes use the same daemon preparation contract;
/// missing or divergent bytes are a launch error, not a bridge fallback.
/// Immutable argv remains the configuration authority; file mode alone is not
/// a same-UID boundary (the read-only source mount may expose source mode 0644).
fn verify_compatibility_baseline_config(
    workload_home: &std::path::Path,
    source: &std::path::Path,
    destination_name: &str,
) -> Result<()> {
    let admitted = lillux::read_regular_file_bounded_no_follow(source, 64 * 1024)
        .context("read admitted structured-session baseline config through Lillux")?;
    if admitted.is_empty() || admitted.len() > 64 * 1024 {
        bail!("admitted structured-session baseline config is empty or exceeds its bound");
    }
    let home = lillux::PinnedDirectory::open(workload_home)?
        .ok_or_else(|| anyhow!("profile workload home is missing"))?;
    let destination_name = std::ffi::OsStr::new(destination_name);
    let incumbent = home
        .open_pinned_regular(destination_name, false)
        .context("open daemon-prepared compatibility seed through Lillux")?
        .ok_or_else(|| anyhow!("daemon-prepared compatibility seed is missing"))?;
    if incumbent.read_bounded(64 * 1024)? != admitted {
        bail!("daemon-prepared compatibility seed differs from the admitted baseline");
    }
    Ok(())
}

fn resolve_pinned_executable(
    external_root: &std::path::Path,
    sealed_realizations: &str,
    realization_id: &str,
    executable_name: &std::path::Path,
) -> Result<(
    std::path::PathBuf,
    std::path::PathBuf,
    Vec<lillux::InheritedDescriptorAuthority>,
)> {
    let executable_member = executable_name
        .to_str()
        .ok_or_else(|| anyhow!("structured-session workload executable is not UTF-8"))?;
    validate_workload_executable_member(executable_member)?;
    if realization_id.is_empty()
        || realization_id.len() > 128
        || !realization_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("structured-session workload realization id is not canonical");
    }
    let realizations = ryeos_state::objects::ExternalContentRealizationSet::from_value(
        &serde_json::from_str(sealed_realizations)
            .context("decode sealed external realizations")?,
    )?;
    let mut matches = realizations
        .iter()
        .filter(|realization| realization.id == realization_id);
    let realization = matches
        .next()
        .ok_or_else(|| anyhow!("workload realization is not present in the admitted set"))?;
    if matches.next().is_some() {
        bail!("workload realization id is ambiguous");
    }
    if realization.mode != ryeos_state::objects::ExternalContentMode::Pinned
        || !lillux::valid_hash(&realization.manifest_hash)
        || realization.entry_count == 0
        || realization.total_bytes == 0
    {
        bail!("workload realization is not a non-empty pinned closure");
    }
    let mount = std::path::Path::new(&realization.mount);
    if mount.as_os_str().is_empty()
        || mount.is_absolute()
        || mount
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("workload realization mount is not a safe relative path");
    }
    let external_root = realization.mount_root.root(Some(external_root))?;
    // The executable and its argv[0] resource path must share one pinned root,
    // even if the pathname is renamed while the workload is being prepared.
    let root = lillux::PinnedDirectory::open(external_root)?
        .ok_or_else(|| anyhow!("external realization root is unavailable"))?;
    let pinned = match realization.kind {
        ryeos_state::objects::ExternalContentKind::File => {
            if executable_name.components().count() != 1
                || mount.file_name() != Some(executable_name.as_os_str())
            {
                bail!("file realization mount does not match the workload executable");
            }
            open_pinned_regular_file_under(&root, mount)?
        }
        ryeos_state::objects::ExternalContentKind::Tree => {
            let directory = open_pinned_directory_under(&root, mount)?;
            open_pinned_regular_file_under(&directory, executable_name).context(
                "open pinned structured-session workload executable from its tree realization",
            )?
        }
    };
    pinned.require_executable()?;
    // Execution-runtime names are usable by ordinary descendants only after
    // Lillux proves the entire namespace spelling immutable. A readonly leaf
    // mount alone is insufficient: a writable ancestor could be rebound.
    let runtime_argv0 = match realization.mount_root {
        ryeos_state::objects::ExternalContentMountRoot::ExecutionRuntime => {
            Some(pinned.verified_read_only_namespace_path()?)
        }
        ryeos_state::objects::ExternalContentMountRoot::Project => None,
    };
    let executable_handle = pinned.into_inherited_descriptor_path()?;
    let executable = executable_handle.path().to_path_buf();
    if let Some(argv0) = runtime_argv0 {
        return Ok((executable, argv0, vec![executable_handle]));
    }
    let root_handle = root.into_inherited_descriptor_path()?;
    let descriptor_root = root_handle.path();
    let argv0 = match realization.kind {
        ryeos_state::objects::ExternalContentKind::File => descriptor_root.join(mount),
        ryeos_state::objects::ExternalContentKind::Tree => {
            descriptor_root.join(mount).join(executable_name)
        }
    };
    Ok((executable, argv0, vec![executable_handle, root_handle]))
}

fn resolve_pinned_executable_search(
    external_root: &std::path::Path,
    sealed_realizations: &str,
    encoded: Option<&str>,
) -> Result<(Option<String>, Vec<lillux::InheritedDescriptorAuthority>)> {
    let Some(encoded) = encoded else {
        return Ok((None, Vec::new()));
    };
    let search: Vec<ryeos_state::objects::ExecutableSearchPathEntry> =
        serde_json::from_str(encoded).context("decode admitted executable search")?;
    if search.is_empty() || search.len() > ryeos_state::objects::MAX_EXECUTABLE_SEARCH_PATH_ENTRIES
    {
        bail!("admitted executable search has an invalid entry count");
    }
    let realizations = ryeos_state::objects::ExternalContentRealizationSet::from_value(
        &serde_json::from_str(sealed_realizations)
            .context("decode sealed external realizations for executable search")?,
    )?;
    let mut paths = Vec::with_capacity(search.len());
    let mut handles = Vec::with_capacity(search.len());
    let mut seen = HashSet::new();
    for entry in search {
        entry.validate()?;
        if !seen.insert((
            entry.realization_id.clone(),
            entry.relative_directory.clone(),
        )) {
            bail!("admitted executable search contains a duplicate entry");
        }
        let realization = realizations
            .iter()
            .find(|realization| realization.id == entry.realization_id)
            .ok_or_else(|| anyhow!("executable search names an absent realization"))?;
        if realization.kind != ryeos_state::objects::ExternalContentKind::Tree
            || realization.mode != ryeos_state::objects::ExternalContentMode::Pinned
        {
            bail!("executable search requires a pinned tree realization");
        }
        let mut relative = std::path::PathBuf::from(&realization.mount);
        if entry.relative_directory != "." {
            relative.push(&entry.relative_directory);
        }
        let directory =
            open_pinned_directory(realization.mount_root.root(Some(external_root))?, &relative)?;
        let path = match realization.mount_root {
            ryeos_state::objects::ExternalContentMountRoot::ExecutionRuntime => {
                directory.verified_read_only_namespace_path()?
            }
            ryeos_state::objects::ExternalContentMountRoot::Project => {
                let handle = directory.into_inherited_descriptor_path()?;
                let path = handle.path().to_path_buf();
                handles.push(handle);
                path
            }
        };
        let path = path
            .to_str()
            .ok_or_else(|| anyhow!("admitted executable search path is not UTF-8"))?;
        if path.contains(':') {
            bail!("admitted executable search path contains a separator");
        }
        paths.push(path.to_owned());
    }
    Ok((Some(paths.join(":")), handles))
}

fn resolve_session_process_environment(
    workspace: &std::path::Path,
    external_root: &std::path::Path,
    sealed_realizations: &str,
    encoded: Option<&str>,
) -> Result<(
    BTreeMap<String, String>,
    Vec<lillux::InheritedDescriptorAuthority>,
)> {
    let Some(encoded) = encoded else {
        return Ok((BTreeMap::new(), Vec::new()));
    };
    if encoded.len() > ryeos_state::objects::MAX_PREPARED_SESSION_PROCESS_ENVIRONMENT_BYTES {
        bail!("prepared session process environment exceeds its encoded byte bound");
    }
    let prepared: ryeos_state::objects::PreparedSessionProcessEnvironment =
        serde_json::from_str(encoded).context("decode admitted session process environment")?;
    prepared.validate()?;
    let bindings = prepared.bindings;
    let delivery = prepared.runtime_view_delivery;
    let realizations = ryeos_state::objects::ExternalContentRealizationSet::from_value(
        &serde_json::from_str(sealed_realizations)
            .context("decode sealed external realizations for session process environment")?,
    )?;
    let runtime_view = if bindings.values().any(|value| {
        matches!(
            value,
            ryeos_state::objects::SessionProcessEnvironmentValue::RuntimeViewDirectory { .. }
        )
    }) {
        let mut view = lillux::PinnedDirectory::open(workspace)?
            .ok_or_else(|| anyhow!("worker runtime workspace is unavailable"))?;
        for component in [".ai", "cache", "ryeos-runtime"] {
            let name = std::ffi::OsStr::new(component);
            view = match &delivery {
                ryeos_state::objects::SessionRuntimeViewDelivery::DescriptorWorkspace => {
                    view.open_or_create_child(name, 0o700)?
                }
                ryeos_state::objects::SessionRuntimeViewDelivery::MountedNamespace { .. } => {
                    view.open_child_directory(name)?.ok_or_else(|| {
                        anyhow!("prepared runtime-view component `{component}` is missing")
                    })?
                }
            };
        }
        if matches!(
            &delivery,
            ryeos_state::objects::SessionRuntimeViewDelivery::DescriptorWorkspace
        ) {
            view.tighten_owner_private_directory()?;
        }
        Some(view)
    } else {
        None
    };

    let mut resolved = BTreeMap::new();
    let mut handles = Vec::new();
    for (name, binding) in bindings {
        let value = match binding {
            ryeos_state::objects::SessionProcessEnvironmentValue::Literal { value } => value,
            ryeos_state::objects::SessionProcessEnvironmentValue::RuntimeViewDirectory {
                relative_path,
            } => {
                let mut directory = runtime_view
                    .as_ref()
                    .expect("runtime view was required")
                    .try_clone()?;
                if relative_path != "." {
                    for component in std::path::Path::new(&relative_path).components() {
                        let std::path::Component::Normal(component) = component else {
                            unreachable!("runtime-view path was validated");
                        };
                        directory = match &delivery {
                            ryeos_state::objects::SessionRuntimeViewDelivery::DescriptorWorkspace => {
                                directory.open_or_create_child(component, 0o700)?
                            }
                            ryeos_state::objects::SessionRuntimeViewDelivery::MountedNamespace { .. } => {
                                directory.open_child_directory(component)?
                                    .ok_or_else(|| anyhow!("prepared runtime-view directory is missing"))?
                            }
                        };
                    }
                }
                let path = match &delivery {
                    ryeos_state::objects::SessionRuntimeViewDelivery::DescriptorWorkspace => {
                        directory.tighten_owner_private_directory()?;
                        let handle = directory.into_inherited_descriptor_path()?;
                        let path = handle.path().to_path_buf();
                        handles.push(handle);
                        path
                    }
                    ryeos_state::objects::SessionRuntimeViewDelivery::MountedNamespace {
                        destinations,
                    } => directory.verified_writable_mount_namespace_path(&destinations[&name])?,
                };
                let value = path
                    .to_str()
                    .ok_or_else(|| anyhow!("runtime-view path is not UTF-8"))?
                    .to_owned();
                value
            }
            ryeos_state::objects::SessionProcessEnvironmentValue::RealizationPath {
                realization_id,
                relative_path,
                path_kind,
            } => {
                let realization = realizations
                    .iter()
                    .find(|realization| realization.id == realization_id)
                    .ok_or_else(|| {
                        anyhow!("session process environment names an absent realization")
                    })?;
                if realization.kind != ryeos_state::objects::ExternalContentKind::Tree
                    || realization.mode != ryeos_state::objects::ExternalContentMode::Pinned
                {
                    bail!("session process environment requires a pinned tree realization");
                }
                let mut relative = std::path::PathBuf::from(&realization.mount);
                if relative_path != "." {
                    relative.push(&relative_path);
                }
                let external_root = realization.mount_root.root(Some(external_root))?;
                let (path, handle) = match path_kind {
                    ryeos_state::objects::SessionProcessEnvironmentPathKind::Directory => {
                        let directory = open_pinned_directory(external_root, &relative)?;
                        match realization.mount_root {
                            ryeos_state::objects::ExternalContentMountRoot::ExecutionRuntime => {
                                (directory.verified_read_only_namespace_path()?, None)
                            }
                            ryeos_state::objects::ExternalContentMountRoot::Project => {
                                let handle = directory.into_inherited_descriptor_path()?;
                                (handle.path().to_path_buf(), Some(handle))
                            }
                        }
                    }
                    ryeos_state::objects::SessionProcessEnvironmentPathKind::File => {
                        let file = open_pinned_regular_file(external_root, &relative)?;
                        match realization.mount_root {
                            ryeos_state::objects::ExternalContentMountRoot::ExecutionRuntime => {
                                (file.verified_read_only_namespace_path()?, None)
                            }
                            ryeos_state::objects::ExternalContentMountRoot::Project => {
                                let handle = file.into_inherited_descriptor_path()?;
                                (handle.path().to_path_buf(), Some(handle))
                            }
                        }
                    }
                };
                let value = path
                    .to_str()
                    .ok_or_else(|| anyhow!("admitted realization path is not UTF-8"))?
                    .to_owned();
                if let Some(handle) = handle {
                    handles.push(handle);
                }
                value
            }
        };
        resolved.insert(name, value);
    }
    Ok((resolved, handles))
}

fn open_pinned_directory(
    root: &std::path::Path,
    relative: &std::path::Path,
) -> Result<lillux::PinnedDirectory> {
    let root = lillux::PinnedDirectory::open(root)?
        .ok_or_else(|| anyhow!("external realization root is unavailable"))?;
    open_pinned_directory_under(&root, relative)
}

fn open_pinned_directory_under(
    root: &lillux::PinnedDirectory,
    relative: &std::path::Path,
) -> Result<lillux::PinnedDirectory> {
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("pinned realization directory is not a safe relative path");
    }
    let mut directory = root.try_clone()?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            unreachable!("relative path was validated");
        };
        directory = directory
            .open_child_directory(name)?
            .ok_or_else(|| anyhow!("pinned realization directory is absent"))?;
    }
    Ok(directory)
}

fn open_pinned_regular_file(
    root: &std::path::Path,
    relative: &std::path::Path,
) -> Result<lillux::PinnedRegularFile> {
    let root = lillux::PinnedDirectory::open(root)?
        .ok_or_else(|| anyhow!("external realization root is unavailable"))?;
    open_pinned_regular_file_under(&root, relative)
}

fn open_pinned_regular_file_under(
    root: &lillux::PinnedDirectory,
    relative: &std::path::Path,
) -> Result<lillux::PinnedRegularFile> {
    let name = relative
        .file_name()
        .ok_or_else(|| anyhow!("pinned realization file has no name"))?;
    let parent = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let directory = match parent {
        Some(parent) => open_pinned_directory_under(root, parent)?,
        None => root.try_clone()?,
    };
    directory
        .open_pinned_regular(name, false)?
        .ok_or_else(|| anyhow!("pinned realization file is absent"))
}

fn validate_workload_executable_member(value: &str) -> Result<()> {
    if value.len() > 4096 {
        bail!("structured-session workload executable exceeds its path bound");
    }
    ryeos_state::objects::validate_canonical_project_relative_path(value)
        .context("structured-session workload executable is not a canonical relative member")
}

fn protect_profile_home(root: &std::path::Path) -> Result<()> {
    let home = lillux::PinnedDirectory::open(root)?
        .ok_or_else(|| anyhow!("profile workload home is missing"))?;
    home.tighten_owner_private_directory()
        .context("protect live profile-home root through Lillux")
}

impl StructuredWorkload {
    fn start(
        executable: &str,
        workspace: &str,
        workload_home: &str,
        profile: StructuredSessionProfile,
        route_set: String,
        allowed_effect_classes: HashSet<RouteEffectClass>,
        schemas: HashMap<String, Value>,
        events: Arc<Mutex<EventQueue>>,
        pending_controls: Receiver<PendingControl>,
        control_results: SyncSender<WorkloadCommandResult>,
        workload_argv0: &str,
        executable_path: Option<&str>,
        session_process_environment: &BTreeMap<String, String>,
        inherited_descriptors: Vec<lillux::InheritedDescriptorAuthority>,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        lillux::configure_command_argv0(&mut command, workload_argv0)
            .map_err(anyhow::Error::msg)?;
        // Adopting the session control channel can consume fd 0. Lillux must
        // preserve the newly configured pipes through the child exec; plain
        // Stdio::piped plus a pre-exec hook can otherwise close stdin again.
        lillux::configure_command_piped_stdio(&mut command);
        command
            .args(&profile.workload_args)
            .current_dir(workspace)
            .env_clear()
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env(&profile.workload_home_env, workload_home)
            .env("HOME", workload_home);
        if let Some(path) = executable_path {
            command.env("PATH", path);
        }
        command.envs(session_process_environment);
        let http_boot = match (&profile.transport, &profile.http_sse) {
            (ProfileTransport::HttpSse, Some(credentials)) => {
                let username = hex_encode(&lillux::crypto::generate_random_bytes::<16>());
                let password = hex_encode(&lillux::crypto::generate_random_bytes::<32>());
                let mut generated = vec![
                    (credentials.username_env.clone(), username),
                    (credentials.password_env.clone(), password),
                ];
                // The HTTP transport homes the workload's XDG state inside
                // the private home. XDG roots resolve through the OS user
                // record rather than HOME, so redirection must be explicit.
                for (name, value) in [
                    ("XDG_CONFIG_HOME", format!("{workload_home}/.config")),
                    ("XDG_DATA_HOME", format!("{workload_home}/.local/share")),
                    ("XDG_STATE_HOME", format!("{workload_home}/.local/state")),
                    ("XDG_CACHE_HOME", format!("{workload_home}/.cache")),
                ] {
                    if session_process_environment.contains_key(name) {
                        bail!("admitted session inputs collide with the workload XDG home");
                    }
                    command.env(name, value);
                }
                if let Some(seed_env) = &credentials.seed_path_env {
                    generated.push((
                        seed_env.clone(),
                        format!("{workload_home}/{}", profile.baseline_destination),
                    ));
                }
                for (name, value) in &generated {
                    if session_process_environment.contains_key(name) {
                        bail!("HTTP credential environment collides with admitted session inputs");
                    }
                    command.env(name, value);
                }
                let authorization =
                    base64_standard(format!("{}:{}", generated[0].1, generated[1].1));
                Some(format!("Basic {authorization}"))
            }
            (ProfileTransport::StdioJsonRpc, None) => None,
            _ => bail!("structured-session transport contradicts its credential block"),
        };
        lillux::configure_owner_private_creation_mask(&mut command);
        lillux::configure_inherited_descriptor_authorities(&mut command, &inherited_descriptors)
            .map_err(anyhow::Error::msg)?;
        let mut child = command
            .spawn()
            .with_context(|| format!("start pinned structured-session workload `{executable}`"))?;
        let output = child
            .stdout
            .take()
            .context("capture structured workload stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("capture structured workload stderr")?;
        let (sender, incoming) = sync_channel(1024);
        let io = match http_boot {
            None => {
                let input = child
                    .stdin
                    .take()
                    .context("capture structured workload stdin")?;
                thread::Builder::new()
                    .name("ryeos-structured-session-workload-reader".to_owned())
                    .spawn({
                        let sender = sender.clone();
                        move || read_app_server(output, sender)
                    })
                    .context("start bounded structured workload reader")?;
                WorkloadIo::Stdio(input)
            }
            Some(authorization) => {
                let http_contract = profile
                    .http_sse
                    .as_ref()
                    .expect("HTTP transport has its admitted contract");
                let base_url = wait_for_loopback_listening(
                    output,
                    &http_contract.listener_stdout_prefix,
                    MonotonicDeadline::after(Duration::from_secs(30)),
                )?;
                let client = reqwest::blocking::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(ROUTE_CALL_TIMEOUT)
                    .build()
                    .context("build structured workload HTTP client")?;
                let server_request_client = reqwest::blocking::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(SERVER_REQUEST_REPLY_TIMEOUT)
                    .build()
                    .context("build structured workload server-request client")?;
                let readiness_url = format!("{}{}", base_url, http_contract.readiness_path);
                let readiness = perform_http_request(
                    &client,
                    &readiness_url,
                    "GET",
                    &authorization,
                    &json!({}),
                )?;
                validate_schema_from_map(&schemas, &http_contract.readiness_schema, &readiness)?;
                // The event stream is a long-lived read; it must never inherit
                // the request client's call deadline.
                let sse_client = reqwest::blocking::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .context("build structured workload event client")?;
                let asks: Vec<(String, String)> = profile
                    .server_requests
                    .iter()
                    .map(|rule| {
                        (
                            rule.method.clone(),
                            rule.correlation.operation_pointer.clone(),
                        )
                    })
                    .collect();
                let sse_base = base_url.clone();
                let sse_authorization = authorization.clone();
                let sse_sender = sender.clone();
                let sse_path = http_contract.event_path.clone();
                let event_type_pointer = http_contract.event_type_pointer.clone();
                let event_properties_pointer = http_contract.event_properties_pointer.clone();
                thread::Builder::new()
                    .name("ryeos-structured-session-event-reader".to_owned())
                    .spawn(move || {
                        read_http_events(
                            sse_client,
                            sse_base,
                            sse_authorization,
                            sse_path,
                            event_type_pointer,
                            event_properties_pointer,
                            asks,
                            sse_sender,
                        )
                    })
                    .context("start structured workload event reader")?;
                let (request_sender, request_receiver) =
                    sync_channel::<HttpRequest>(HTTP_REQUEST_QUEUE_CAPACITY);
                let request_receiver = Arc::new(Mutex::new(request_receiver));
                for worker_index in 0..HTTP_REQUEST_WORKERS {
                    let requests = Arc::clone(&request_receiver);
                    let responses = sender.clone();
                    thread::Builder::new()
                        .name(format!("ryeos-structured-session-http-{worker_index}"))
                        .spawn(move || http_request_worker(requests, responses))
                        .context("start bounded structured workload HTTP executor")?;
                }
                WorkloadIo::Http(HttpWorkload {
                    base_url,
                    client,
                    server_request_client,
                    authorization,
                    ignored_notification_projection: http_contract.ignored_notification_projection,
                    requests: request_sender,
                })
            }
        };
        thread::Builder::new()
            .name("ryeos-structured-session-stderr-drain".to_owned())
            .spawn(move || drain_private_stderr(stderr))
            .context("start bounded structured-session stderr drain")?;
        Ok(Self {
            child,
            io,
            incoming,
            responses: HashMap::new(),
            server_requests: HashMap::new(),
            workload_channel: None,
            workload_invocations: Vec::new(),
            expired_server_requests: VecDeque::new(),
            seen_server_request_ids: HashSet::new(),
            events,
            pending_controls,
            control_results,
            command_progress: None,
            active_progress_notifications: Vec::new(),
            early_command_observations: Vec::new(),
            next_id: 1,
            fatal: None,
            workspace: workspace.to_owned(),
            workload_home: workload_home.to_owned(),
            ceremony_active: false,
            bound_session_id: None,
            profile,
            route_set,
            allowed_effect_classes,
            schemas,
            outstanding: HashSet::new(),
            response_bytes: 0,
        })
    }

    fn initialize(&mut self) -> Result<()> {
        for step in self.profile.initialization.clone() {
            if !self.allowed_effect_classes.contains(&step.effect_class) {
                bail!(
                    "structured-session initialization effect `{}` exceeds the root launch ceiling",
                    step.effect_class.as_str()
                );
            }
            if let Some(notification) = step.notification {
                self.send(&json!({"method":notification,"params":step.params}))
                    .context("send admitted initialization notification")?;
                continue;
            }
            let result = self
                .call_raw(&step.method, step.params, Duration::from_secs(30))
                .context("exchange admitted initialization request")?;
            if result.get("error").is_some() {
                bail!("structured-session workload rejected initialization");
            }
            if let Some(schema) = step.response_schema.as_deref() {
                self.validate_schema(schema, result.get("result").unwrap_or(&Value::Null))?;
            }
        }
        Ok(())
    }

    fn handle(&mut self, body: Value, workspace: &str) -> Result<Value> {
        protect_profile_home(std::path::Path::new(&self.workload_home))?;
        self.drain_incoming()?;
        self.expire_server_requests()?;
        if let Some(reason) = self.fatal.as_deref() {
            bail!("structured-session workload protocol is quarantined: {reason}");
        }
        let object = body
            .as_object()
            .ok_or_else(|| anyhow!("structured workload request body must be an object"))?;
        if object.contains_key("ryeos_control") {
            bail!("reserved RyeOS control is not a public session command");
        }
        require_exact_keys(object, &["route_id", "payload"])?;
        let route_id = object
            .get("route_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("structured-session command has no route id"))?;
        let payload = object.get("payload").cloned().unwrap_or_else(|| json!({}));
        let result = self.handle_route(route_id, payload, workspace, RouteAudience::Public);
        protect_profile_home(std::path::Path::new(&self.workload_home))?;
        result
    }

    fn handle_control(&mut self, body: Value) -> Result<Value> {
        protect_profile_home(std::path::Path::new(&self.workload_home))?;
        let control = body
            .as_object()
            .ok_or_else(|| anyhow!("RyeOS session control must be an object"))?;
        let result = match control.get("kind").and_then(Value::as_str) {
            Some("approval_decision") => {
                require_exact_keys(
                    control,
                    &[
                        "kind",
                        "request_id",
                        "request_digest",
                        "decision",
                        "reservation_token",
                    ],
                )?;
                let token = control
                    .get("reservation_token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("approval control has no reservation token"))?;
                if token.is_empty() || token.len() > 256 || token.chars().any(char::is_control) {
                    bail!("approval reservation token is not canonical");
                }
                self.handle_approval_control(control)
            }
            Some("runtime_route") => {
                require_exact_keys(control, &["kind", "route_id", "payload"])?;
                let route_id = control
                    .get("route_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("runtime route control has no route id"))?;
                let payload = control.get("payload").cloned().unwrap_or_else(|| json!({}));
                let workspace = self.workspace.clone();
                self.handle_route(route_id, payload, &workspace, RouteAudience::Runtime)
            }
            Some("runtime_recover") => {
                require_exact_keys(control, &["kind", "upstream_session_id"])?;
                let upstream_session_id = control
                    .get("upstream_session_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty() && value.len() <= 256)
                    .ok_or_else(|| anyhow!("runtime recovery has no bounded upstream session id"))?
                    .to_owned();
                self.handle_runtime_recovery(&upstream_session_id)
            }
            _ => bail!("unsupported RyeOS session control"),
        };
        protect_profile_home(std::path::Path::new(&self.workload_home))?;
        result
    }

    fn validate_schema(&self, identity: &str, value: &Value) -> Result<()> {
        validate_schema_from_map(&self.schemas, identity, value)
    }

    fn handle_approval_control(&mut self, control: &Map<String, Value>) -> Result<Value> {
        let request_digest = control
            .get("request_digest")
            .and_then(Value::as_str)
            .filter(|digest| lillux::valid_hash(digest))
            .ok_or_else(|| anyhow!("approval control has no canonical request digest"))?;
        let request = Map::from_iter([
            (
                "operation".to_string(),
                Value::String("approval".to_string()),
            ),
            (
                "requestId".to_string(),
                control
                    .get("request_id")
                    .cloned()
                    .ok_or_else(|| anyhow!("approval control has no request id"))?,
            ),
            (
                "decision".to_string(),
                control
                    .get("decision")
                    .cloned()
                    .ok_or_else(|| anyhow!("approval control has no decision"))?,
            ),
        ]);
        self.handle_approval(&request, request_digest)
    }

    fn handle_runtime_recovery(&mut self, upstream_session_id: &str) -> Result<Value> {
        let recovery = self
            .profile
            .recovery
            .clone()
            .ok_or_else(|| anyhow!("structured-session profile does not admit recovery"))?;
        if !recovery
            .route_sets
            .iter()
            .any(|route_set| route_set == &self.route_set)
        {
            bail!("active structured-session route set does not admit recovery");
        }
        let resume = self
            .profile
            .routes
            .iter()
            .find(|route| route.id == recovery.resume_route)
            .cloned()
            .ok_or_else(|| anyhow!("admitted recovery resume route disappeared"))?;
        let request_field = resume
            .session_binding
            .as_ref()
            .filter(|binding| binding.action == SessionBindingAction::BindExpected)
            .and_then(|binding| binding.request_field.as_deref())
            .ok_or_else(|| anyhow!("recovery resume route has no expected-session field"))?
            .to_owned();
        let mut resume_payload = Map::new();
        resume_payload.insert(request_field, Value::String(upstream_session_id.to_owned()));
        let workspace = self.workspace.clone();
        let resume_result = self.handle_route(
            &recovery.resume_route,
            Value::Object(resume_payload),
            &workspace,
            RouteAudience::Runtime,
        )?;
        require_successful_internal_route(&resume_result, "resume")?;
        let inspect_result = self.handle_route(
            &recovery.inspect_route,
            json!({}),
            &workspace,
            RouteAudience::Runtime,
        )?;
        require_successful_internal_route(&inspect_result, "inspect")?;
        let mut observations = Vec::new();
        for result in [&resume_result, &inspect_result] {
            let values = result
                .get("session_observations")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("internal recovery route omitted its observations"))?;
            observations.extend(values.iter().cloned());
        }
        Ok(json!({
            "response":{"recovered":true},
            "session_observations":observations,
            "result_retention":"ephemeral"
        }))
    }

    fn handle_route(
        &mut self,
        route_id: &str,
        mut params: Value,
        workspace: &str,
        audience: RouteAudience,
    ) -> Result<Value> {
        let admitted = self
            .profile
            .route_sets
            .get(&self.route_set)
            .ok_or_else(|| anyhow!("active structured-session route set disappeared"))?;
        if !admitted.iter().any(|id| id == route_id) {
            bail!("structured-session route is not admitted for this execution");
        }
        let route = self
            .profile
            .routes
            .iter()
            .find(|route| route.id == route_id)
            .cloned()
            .ok_or_else(|| anyhow!("admitted structured-session route is undefined"))?;
        if route.audience != audience {
            bail!("structured-session route is not available on this command surface");
        }
        if !self.allowed_effect_classes.contains(&route.effect_class) {
            bail!(
                "structured-session route effect `{}` exceeds the root launch ceiling",
                route.effect_class.as_str()
            );
        }
        let method = route.method.as_str();
        if matches!(route.ceremony, Some(CeremonyAction::Start)) && self.ceremony_active {
            bail!("one credential enrollment is already active for this worker");
        }
        let expected_binding = prepare_session_binding(
            route.session_binding.as_ref(),
            &self.bound_session_id,
            &mut params,
            self.profile.transport == ProfileTransport::StdioJsonRpc,
        )?;
        apply_route_parameters(&route, &mut params, workspace)?;
        if let Some(mapping) = self
            .profile
            .workload_client
            .as_ref()
            .and_then(|client| client.structured_session.as_ref())
            .filter(|mapping| mapping.registration_route == route_id)
            && let Some(channel) = self.workload_channel.as_ref().filter(|channel| {
                channel.admits(
                    ryeos_runtime::workload_client::WorkloadClientIngress::StructuredSession,
                )
            })
        {
            // Caller collision was rejected by the route's forbidden-fields
            // contract above. Only this admission-derived presentation may
            // populate the signed registration mapping.
            let template: ValueTemplate = serde_json::from_value(mapping.registration.clone())?;
            let registration = evaluate_template(
                &template,
                &json!({"workload":{"executions":channel.execution_presentation()}}),
            )?;
            params
                .as_object_mut()
                .ok_or_else(|| anyhow!("registration route params are not an object"))?
                .insert(mapping.registration_field.clone(), registration);
        }
        self.validate_schema(&route.request_schema, &params)?;
        let observed_params = params.clone();
        if !self.active_progress_notifications.is_empty() {
            bail!("nested route cannot replace active progress authority");
        }
        self.active_progress_notifications = route.progress_notifications.clone();
        self.early_command_observations.clear();
        let response = self.call_raw(method, params, ROUTE_CALL_TIMEOUT);
        self.active_progress_notifications.clear();
        let response = response?;
        if response.get("error").is_none() {
            self.validate_schema(
                &route.response_schema,
                response.get("result").unwrap_or(&Value::Null),
            )?;
        }
        let context = json!({"params":observed_params,"response":response.clone()});
        if response.get("error").is_none()
            && !route
                .response_predicates
                .iter()
                .all(|predicate| context.pointer(&predicate.pointer) == Some(&predicate.equals))
        {
            bail!("structured workload response violates an admitted conformance predicate");
        }
        let observations = if response.get("error").is_none() {
            evaluate_observations(&route.observations, &context)?
        } else {
            Vec::new()
        };
        if !route.progress_notifications.is_empty()
            && self.early_command_observations.is_empty()
            && !observations.is_empty()
        {
            // A vendor may answer before emitting its progress notification.
            // Correlate the schema-validated response through the same delta
            // acceptance path before any subsequent callback can dispatch.
            self.emit_command_progress(&observations)?;
        }
        if !self
            .early_command_observations
            .iter()
            .all(|early| observations.contains(early))
        {
            bail!("final command response contradicts its earlier lifecycle observations");
        }
        self.early_command_observations.clear();
        if response.get("error").is_none() {
            settle_session_binding(
                route.session_binding.as_ref(),
                expected_binding.as_deref(),
                &response,
                &mut self.bound_session_id,
            )?;
            match route.ceremony {
                Some(CeremonyAction::Start) => self.ceremony_active = true,
                Some(CeremonyAction::Clear) => self.ceremony_active = false,
                _ => {}
            }
            for post_route in &route.post_success_routes {
                let post_result =
                    self.handle_route(post_route, json!({}), workspace, RouteAudience::Runtime)?;
                require_successful_internal_route(&post_result, "post-success")?;
            }
        }
        // structured workload responses are returned to the attached caller but are not
        // a second durable copy of account data, prompts, thread history, or
        // other provider payloads. Durable state is reduced to the typed
        // observations and event journal below.
        let result_retention = match route.result_retention {
            ResultRetention::Ephemeral => "ephemeral",
            ResultRetention::Durable => "durable",
        };
        Ok(
            json!({"response":response,"session_observations":observations,
            "result_retention":result_retention}),
        )
    }

    fn handle_approval(
        &mut self,
        request: &Map<String, Value>,
        expected_request_digest: &str,
    ) -> Result<Value> {
        require_exact_keys(request, &["operation", "requestId", "decision"])?;
        let request_id = request
            .get("requestId")
            .ok_or_else(|| anyhow!("approval has no requestId"))?;
        let key = canonical_id(request_id)?;
        let pending = match self.server_requests.get(&key) {
            Some(pending) if pending.request_digest == expected_request_digest => {
                self.server_requests
                    .remove(&key)
                    .expect("pending server request disappeared")
                    .message
            }
            Some(_) => bail!("approval request digest does not match the pending request"),
            None if self.expired_server_requests.iter().any(|expired| {
                expired.id == key && expired.request_digest == expected_request_digest
            }) =>
            {
                return Ok(json!({
                    "resolved":false,
                    "outcome":"expired",
                    "request_id":request_id,
                    "request_digest":expected_request_digest,
                }));
            }
            None => bail!("approval is absent, stale, or already resolved"),
        };
        let method = pending
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("pending server request has no method"))?;
        let decision = request
            .get("decision")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("approval decision must be a string"))?;
        if !matches!(decision, "accept" | "decline" | "cancel") {
            bail!("approval decision is outside the baseline vocabulary");
        }
        let rule = self
            .profile
            .server_requests
            .iter()
            .find(|rule| rule.method == method)
            .cloned()
            .ok_or_else(|| anyhow!("pending server request has no admitted rule"))?;
        let context = json!({"message":pending});
        if decision == "accept" && !approval_accept_allowed(&rule, &context) {
            bail!("approval with a filesystem, network, or policy delta cannot be accepted");
        }
        let response_template = match decision {
            "accept" => &rule.responses.accept,
            "decline" => &rule.responses.decline,
            "cancel" => &rule.responses.cancel,
            _ => unreachable!("decision vocabulary checked above"),
        };
        let response = evaluate_template(response_template, &context)?;
        self.deliver_server_request_response(request_id, &rule, &response)?;
        Ok(json!({"resolved":true}))
    }

    fn expire_server_requests(&mut self) -> Result<()> {
        let expired = self
            .server_requests
            .iter()
            .filter(|(_, pending)| pending.expires_at.has_elapsed())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in expired {
            let pending = self
                .server_requests
                .remove(&id)
                .ok_or_else(|| anyhow!("expired approval disappeared"))?
                .message;
            let method = pending
                .get("method")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("expired approval has no method"))?;
            let request_id = pending
                .get("id")
                .cloned()
                .ok_or_else(|| anyhow!("expired approval has no id"))?;
            let rule = self
                .profile
                .server_requests
                .iter()
                .find(|rule| rule.method == method)
                .cloned()
                .ok_or_else(|| anyhow!("expired server request has no admitted rule"))?;
            let context = json!({"message":pending});
            let expiry_event = approval_expired_event(&rule, &context)?;
            let response = evaluate_template(&rule.responses.expire, &context)?;
            self.deliver_server_request_response(&request_id, &rule, &response)?;
            self.push_event(expiry_event)?;
            self.expired_server_requests
                .push_back(ExpiredServerRequest {
                    id,
                    request_digest: ryeos_state::objects::canonical_value_digest(&pending)?,
                });
            while self.expired_server_requests.len() > MAX_PENDING_SERVER_REQUESTS {
                self.expired_server_requests.pop_front();
            }
        }
        Ok(())
    }

    fn call_raw(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("structured workload request id overflow"))?;
        self.send(&json!({"id":id,"method":method,"params":params}))?;
        let key = id.to_string();
        if !self.outstanding.insert(key.clone()) {
            bail!("structured-session request id was reused");
        }
        let deadline = MonotonicDeadline::after(timeout);
        loop {
            self.service_pending_controls()?;
            self.expire_server_requests()?;
            if let Some(response) = self.responses.remove(&key) {
                self.response_bytes = self
                    .response_bytes
                    .saturating_sub(serde_json::to_vec(&response)?.len());
                return Ok(response);
            }
            if let Some(reason) = self.fatal.as_deref() {
                bail!("structured-session workload protocol is quarantined: {reason}");
            }
            let remaining = deadline.remaining();
            if remaining.is_zero() {
                self.outstanding.remove(&key);
                bail!("structured-session workload request `{method}` timed out");
            }
            self.receive_one(remaining.min(Duration::from_millis(50)))?;
        }
    }

    fn service_pending_controls(&mut self) -> Result<()> {
        loop {
            let control = match self.pending_controls.try_recv() {
                Ok(control) => control,
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => return Ok(()),
            };
            let outcome = self
                .handle_control(control.body)
                .map(WorkloadCommandOutput::Final)
                .map_err(|error| error.to_string());
            self.control_results
                .send((control.request_id, outcome))
                .map_err(|_| anyhow!("structured-session control result receiver disconnected"))?;
        }
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        if matches!(self.io, WorkloadIo::Http(_)) {
            return self.send_http(message);
        }
        let WorkloadIo::Stdio(input) = &mut self.io else {
            bail!("structured-session transport is not current");
        };
        serde_json::to_writer(&mut *input, message)
            .context("encode structured workload message")?;
        input
            .write_all(b"\n")
            .context("frame structured workload message")?;
        input.flush().context("flush structured workload message")
    }

    /// Dispatch one admitted message over the HTTP transport. Requests run
    /// on their own thread and settle through the shared incoming channel,
    /// so `call_raw`'s id correlation and deadline stay transport-neutral.
    fn send_http(&mut self, message: &Value) -> Result<()> {
        let (id, method, params) = match (
            message.get("id").filter(|value| !value.is_null()),
            message.get("method").and_then(Value::as_str),
            message.get("params"),
        ) {
            (Some(id), Some(method), params) => (
                id.clone(),
                method,
                params.cloned().unwrap_or_else(|| json!({})),
            ),
            _ => bail!("structured-session HTTP transport exchanges requests only"),
        };
        let route = self
            .profile
            .routes
            .iter()
            .find(|route| route.method == method)
            .cloned()
            .ok_or_else(|| anyhow!("structured-session HTTP route `{method}` is not admitted"))?;
        let (http_method, http_path, http_body_schema) = match (
            &route.http_method,
            &route.http_path,
            &route.http_body_schema,
        ) {
            (Some(http_method), Some(http_path), Some(http_body_schema)) => (
                http_method.clone(),
                http_path.clone(),
                http_body_schema.clone(),
            ),
            _ => bail!("structured-session HTTP route lacks its addressing"),
        };
        let mut substitutions: Vec<(String, String)> = Vec::new();
        let object = params
            .as_object()
            .ok_or_else(|| anyhow!("structured-session HTTP request input is not an object"))?;
        let mut body = params.clone();
        let body_object = body
            .as_object_mut()
            .expect("input object was established above");
        for (placeholder, projection) in &route.http_path_parameters {
            let value = match projection.source {
                HttpPathParameterSource::BoundSession => self
                    .bound_session_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("structured-session HTTP route is not session bound"))?
                    .to_owned(),
                HttpPathParameterSource::Input => {
                    let field = projection.field.as_deref().ok_or_else(|| {
                        anyhow!("structured-session HTTP input path projection lacks its field")
                    })?;
                    let value = object.get(field).and_then(Value::as_str).ok_or_else(|| {
                        anyhow!("structured-session HTTP path field `{field}` is not bounded text")
                    })?;
                    body_object.remove(field);
                    value.to_owned()
                }
            };
            substitutions.push((placeholder.clone(), value));
        }
        let path = substitute_http_path(&http_path, &substitutions)?;
        self.validate_schema(&http_body_schema, &body)?;
        let WorkloadIo::Http(http) = &mut self.io else {
            bail!("structured-session HTTP send requires the HTTP transport");
        };
        let url = format!("{}{}", http.base_url, path);
        http.requests
            .try_send(HttpRequest {
                client: http.client.clone(),
                url,
                method: http_method,
                authorization: http.authorization.clone(),
                body,
                id: Some(id),
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => {
                    anyhow!("structured-session HTTP request executor is saturated")
                }
                TrySendError::Disconnected(_) => {
                    anyhow!("structured-session HTTP request executor stopped")
                }
            })
    }

    /// Deliver one approval or expiry decision. Stdio workloads receive a
    /// JSON-RPC response; HTTP workloads POST the admitted decision body to
    /// the rule's reply path, correlated by the pending request id.
    fn deliver_server_request_response(
        &mut self,
        request_id: &Value,
        rule: &ServerRequestRule,
        response: &Value,
    ) -> Result<()> {
        if matches!(self.io, WorkloadIo::Stdio(_)) {
            return self.send(&json!({"id":request_id,"result":response}));
        }
        let WorkloadIo::Http(http) = &mut self.io else {
            bail!("structured-session transport is not current");
        };
        let reply_path = rule.reply_http_path.as_deref().ok_or_else(|| {
            anyhow!("structured-session HTTP server request lacks its reply path")
        })?;
        let correlation = canonical_id(request_id)?;
        let mut substitutions = vec![("request_id".to_owned(), correlation.clone())];
        if reply_path.contains("{session_id}") {
            let session = self
                .bound_session_id
                .as_deref()
                .ok_or_else(|| anyhow!("structured-session reply path is not session bound"))?;
            substitutions.push(("session_id".to_owned(), session.to_owned()));
        }
        let path = substitute_http_path(reply_path, &substitutions)?;
        let url = format!("{}{}", http.base_url, path);
        http.requests
            .try_send(HttpRequest {
                client: http.server_request_client.clone(),
                url,
                method: "POST".to_owned(),
                authorization: http.authorization.clone(),
                body: response.clone(),
                id: None,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => {
                    anyhow!("structured-session HTTP reply executor is saturated")
                }
                TrySendError::Disconnected(_) => {
                    anyhow!("structured-session HTTP reply executor stopped")
                }
            })
    }

    fn receive_one(&mut self, timeout: Duration) -> Result<()> {
        self.settle_workload_invocations()?;
        match self.incoming.recv_timeout(timeout) {
            Ok(Ok(message)) => self.route(message),
            Ok(Err(reason)) => {
                self.fatal = Some(reason.clone());
                bail!("structured workload reader failed: {reason}")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("structured workload reader disconnected")
            }
        }
    }

    fn drain_incoming(&mut self) -> Result<()> {
        loop {
            self.settle_workload_invocations()?;
            match self.incoming.try_recv() {
                Ok(Ok(message)) => self.route(message)?,
                Ok(Err(reason)) => {
                    self.fatal = Some(reason.clone());
                    bail!("structured workload reader failed: {reason}")
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => bail!("structured workload reader disconnected"),
            }
        }
    }

    fn route(&mut self, message: Value) -> Result<()> {
        let object = message
            .as_object()
            .ok_or_else(|| anyhow!("structured workload emitted a non-object message"))?;
        let id = object.get("id").filter(|value| !value.is_null());
        let method = object.get("method").and_then(Value::as_str);
        match (id, method) {
            (Some(id), Some(method)) => {
                if let Some(mapping) = self
                    .profile
                    .workload_client
                    .as_ref()
                    .and_then(|client| client.structured_session.as_ref())
                    .filter(|mapping| mapping.method == method)
                    .cloned()
                {
                    return self.handle_workload_invocation(id.clone(), message, mapping);
                }
                let rule = self
                    .profile
                    .server_requests
                    .iter()
                    .find(|rule| rule.method == method)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow!("unknown structured-session server request `{method}`")
                    })?;
                self.validate_schema(&rule.schema, message.get("params").unwrap_or(&Value::Null))?;
                if self.server_requests.len() >= MAX_PENDING_SERVER_REQUESTS {
                    bail!("pending structured workload approval bound is exhausted");
                }
                let key = canonical_id(id)?;
                if self.seen_server_request_ids.len() >= MAX_EVENTS {
                    bail!("structured workload server-request lifetime bound is exhausted");
                }
                if !self.seen_server_request_ids.insert(key.clone()) {
                    bail!("structured workload reused a server-request id");
                }
                let request_digest = ryeos_state::objects::canonical_value_digest(&message)?;
                if self
                    .server_requests
                    .insert(
                        key,
                        PendingServerRequest {
                            message: message.clone(),
                            request_digest: request_digest.clone(),
                            expires_at: MonotonicDeadline::after(APPROVAL_TTL),
                        },
                    )
                    .is_some()
                {
                    bail!("structured workload reused a pending server-request id");
                }
                let context = json!({"message":message});
                let display = evaluate_template(&rule.display, &context)?;
                let accept_allowed = approval_accept_allowed(&rule, &context);
                let upstream_session_id = context
                    .pointer(&rule.correlation.upstream_session_pointer)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty() && value.len() <= 256)
                    .ok_or_else(|| {
                        anyhow!("server request has no bounded upstream session correlation")
                    })?;
                if self.bound_session_id.as_deref() != Some(upstream_session_id) {
                    bail!("server request does not target the bound upstream session");
                }
                let operation_id = context
                    .pointer(&rule.correlation.operation_pointer)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty() && value.len() <= 256)
                    .ok_or_else(|| {
                        anyhow!("server request has no bounded operation correlation")
                    })?;
                self.push_event(json!({
                    "event_type":"approval.requested",
                    "payload":{
                        "request_id":id,
                        "operation_class":rule.operation_class,
                        "upstream_session_id":upstream_session_id,
                        "operation_id":operation_id,
                        "accept_allowed":accept_allowed,
                        "request_digest":request_digest,
                        "display":display
                    }
                }))
            }
            (Some(id), None) => {
                let key = canonical_id(id)?;
                if !self.outstanding.remove(&key) {
                    bail!("structured-session workload emitted an unsolicited response id");
                }
                let bytes = serde_json::to_vec(&message)?.len();
                self.response_bytes = self
                    .response_bytes
                    .checked_add(bytes)
                    .ok_or_else(|| anyhow!("response byte accounting overflow"))?;
                if self.response_bytes > 8 * 1024 * 1024 {
                    bail!("structured-session response backlog byte ceiling is exhausted");
                }
                if self.responses.insert(key, message).is_some() {
                    bail!("structured workload emitted a duplicate response id");
                }
                Ok(())
            }
            (None, Some(method)) => {
                if let Some(schema) = self.profile.ignored_notifications.get(method).cloned() {
                    let value = match &self.io {
                        WorkloadIo::Stdio(_) => message.get("params").unwrap_or(&Value::Null),
                        WorkloadIo::Http(http) => match http.ignored_notification_projection {
                            HttpEventProjection::Envelope => {
                                message.get("envelope").unwrap_or(&Value::Null)
                            }
                            HttpEventProjection::Properties => {
                                message.get("params").unwrap_or(&Value::Null)
                            }
                        },
                    };
                    self.validate_schema(&schema, value)?;
                    return Ok(());
                }
                let rule = self
                    .profile
                    .notifications
                    .iter()
                    .find(|rule| rule.method == method)
                    .cloned()
                    .ok_or_else(|| anyhow!("unknown structured-session notification `{method}`"))?;
                self.validate_schema(&rule.schema, message.get("params").unwrap_or(&Value::Null))?;
                if !rule.durable {
                    return Ok(());
                }
                let context = json!({"message":message});
                if let Some(pointer) = &rule.upstream_session_pointer {
                    let session_id = context
                        .pointer(pointer)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty() && value.len() <= 256)
                        .ok_or_else(|| anyhow!("notification lacks bounded session correlation"))?;
                    if self.bound_session_id.as_deref() != Some(session_id) {
                        bail!("notification does not target the bound upstream session");
                    }
                }
                let payload = evaluate_template(&rule.payload, &context)?;
                let mut observations = evaluate_observations(&rule.observations, &context)?;
                if self
                    .active_progress_notifications
                    .iter()
                    .any(|allowed| allowed == method)
                {
                    self.emit_command_progress(&observations)?;
                    // The notification's lifecycle authority is request-correlated,
                    // not duplicated in the independent pushed-observation stream.
                    return self.push_event(json!({"event_type":rule.event_type,"payload":payload,"session_observations":[]}));
                }
                if self.profile.routes.iter().any(|route| {
                    route
                        .progress_notifications
                        .iter()
                        .any(|allowed| allowed == method)
                }) {
                    // A late progress notification cannot acquire command
                    // authority by becoming an uncorrelated lifecycle batch.
                    observations.clear();
                }
                if rule.ceremony_clear {
                    self.ceremony_active = false;
                }
                self.push_event(json!({
                    "event_type":rule.event_type,
                    "payload":payload,
                    "session_observations":observations
                }))
            }
            (None, None) => bail!("structured workload message has neither id nor method"),
        }
    }

    fn handle_workload_invocation(
        &mut self,
        rpc_id: Value,
        message: Value,
        mapping: ryeos_engine::structured_session_profile::StructuredSessionInvocationMapping,
    ) -> Result<()> {
        use ryeos_runtime::workload_client::{
            WORKLOAD_CLIENT_PROTOCOL, WorkloadClientIngress, WorkloadClientOperation,
            WorkloadClientRequestFrame, WorkloadInvocationSource,
        };
        let _channel = self
            .workload_channel
            .as_ref()
            .filter(|channel| channel.admits(WorkloadClientIngress::StructuredSession))
            .ok_or_else(|| anyhow!("structured workload invocation ingress was not admitted"))?
            .clone();
        self.validate_schema(
            &mapping.request_schema,
            message.get("params").unwrap_or(&Value::Null),
        )?;
        let context = json!({"message": message});
        // A signed null identity explicitly means absent-or-null, never any
        // caller-selected namespace. Non-null assertions remain exact.
        if !invocation_identity_matches(&mapping.required_values, &context) {
            bail!("structured workload invocation violates its exact signed identity");
        }
        let correlation = |pointer: &str| -> Result<String> {
            context
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow!("structured workload invocation lacks correlation"))
        };
        let source = WorkloadInvocationSource::StructuredSession {
            upstream_session_id: correlation(&mapping.session_pointer)?,
            operation_id: correlation(&mapping.operation_pointer)?,
            call_id: correlation(&mapping.call_pointer)?,
        };
        let request_id = source.request_id()?;
        if self.bound_session_id.as_deref()
            != context
                .pointer(&mapping.session_pointer)
                .and_then(Value::as_str)
        {
            bail!("workload invocation does not target the bound upstream session");
        }
        let key = canonical_id(&rpc_id)?;
        if self.seen_server_request_ids.len() >= MAX_EVENTS
            || !self.seen_server_request_ids.insert(key)
        {
            bail!("workload invocation exhausted or reused a server-request identity");
        }
        let request_template: ValueTemplate = serde_json::from_value(mapping.request)?;
        let request = serde_json::from_value(evaluate_template(&request_template, &context)?)?;
        let frame = WorkloadClientRequestFrame {
            protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
            request_id,
            operation: WorkloadClientOperation::Execute(request),
        };
        frame.validate()?;
        let response_template = serde_json::from_value(mapping.response)?;
        if self.workload_invocations.len()
            >= usize::from(ryeos_runtime::workload_client::MAX_WORKLOAD_CLIENT_IN_FLIGHT)
        {
            return self.send_workload_invocation_result(
                &rpc_id,
                &mapping.response_schema,
                &response_template,
                ryeos_runtime::workload_client::WorkloadClientOutcome::Failed {
                    code: "ingress-full".to_owned(),
                    message: "workload ingress result bound exhausted before dispatch".to_owned(),
                    retryable: false,
                },
            );
        }
        // Submit returns immediately. Child execution may freeze this bridge;
        // the daemon owns child settlement and thaw independently of this
        // response consumer. Never wait for a child under the App Server lock.
        self.workload_invocations.push(PendingWorkloadInvocation {
            rpc_id,
            response_schema: mapping.response_schema,
            response_template,
            delivery: WorkloadInvocationDelivery::AwaitingProgress {
                source,
                request: frame,
            },
        });
        Ok(())
    }

    fn settle_workload_invocations(&mut self) -> Result<()> {
        let mut index = 0;
        while index < self.workload_invocations.len() {
            if let WorkloadInvocationDelivery::AwaitingProgress { source, request } =
                &self.workload_invocations[index].delivery
            {
                let acknowledged = self
                    .events
                    .lock()
                    .map_err(|_| anyhow!("event queue is poisoned"))?
                    .command_progress
                    .as_ref()
                    .is_none_or(|progress| progress.acknowledged);
                if !acknowledged
                    || (!self.active_progress_notifications.is_empty()
                        && self.early_command_observations.is_empty())
                {
                    index += 1;
                    continue;
                }
                let channel = self
                    .workload_channel
                    .as_ref()
                    .ok_or_else(|| anyhow!("pending invocation lost its admitted channel"))?;
                match channel.submit(source.clone(), request.clone()) {
                    Ok(receiver) => {
                        self.workload_invocations[index].delivery =
                            WorkloadInvocationDelivery::Submitted(receiver)
                    }
                    Err(_) => {
                        let pending = self.workload_invocations.swap_remove(index);
                        self.send_workload_invocation_result(
                            &pending.rpc_id,
                            &pending.response_schema,
                            &pending.response_template,
                            ryeos_runtime::workload_client::WorkloadClientOutcome::Failed {
                                code: "ingress-refused".to_owned(),
                                message: "workload ingress refused this request before dispatch"
                                    .to_owned(),
                                retryable: false,
                            },
                        )?;
                        continue;
                    }
                }
            }
            let WorkloadInvocationDelivery::Submitted(receiver) =
                &self.workload_invocations[index].delivery
            else {
                unreachable!()
            };
            let frame = match receiver.try_recv() {
                Ok(frame) => frame,
                Err(TryRecvError::Empty) => {
                    index += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    bail!("workload invocation lost its outcome channel")
                }
            };
            frame.validate()?;
            let pending = self.workload_invocations.swap_remove(index);
            self.send_workload_invocation_result(
                &pending.rpc_id,
                &pending.response_schema,
                &pending.response_template,
                frame.outcome,
            )?;
        }
        Ok(())
    }

    fn emit_command_progress(&mut self, observations: &[Value]) -> Result<()> {
        let request_id = self
            .command_progress
            .as_ref()
            .ok_or_else(|| anyhow!("progress has no active command coordinate"))?;
        if observations.len() != 1 || !self.early_command_observations.is_empty() {
            bail!("command progress must contain one unique lifecycle observation");
        }
        let body = json!({"events":[],"session_observations":observations});
        let digest = ryeos_state::objects::canonical_value_digest(&body)?;
        let mut events = self
            .events
            .lock()
            .map_err(|_| anyhow!("event queue is poisoned"))?;
        if events
            .command_progress
            .as_ref()
            .is_some_and(|progress| !progress.acknowledged)
        {
            bail!("previous command progress has not been acknowledged");
        }
        events.command_progress = Some(CommandProgressBarrier {
            request_id: request_id.clone(),
            digest,
            acknowledged: false,
            deadline: MonotonicDeadline::after(OBSERVATION_ACK_TIMEOUT),
        });
        self.early_command_observations = observations.to_vec();
        self.control_results
            .try_send((request_id.clone(), Ok(WorkloadCommandOutput::Delta(body))))
            .map_err(|_| anyhow!("bounded command progress channel is unavailable"))
    }

    fn send_workload_invocation_result(
        &mut self,
        rpc_id: &Value,
        schema: &str,
        template: &ValueTemplate,
        outcome: ryeos_runtime::workload_client::WorkloadClientOutcome,
    ) -> Result<()> {
        let result = render_workload_invocation_result(template, outcome)?;
        self.validate_schema(schema, &result)?;
        self.send(&json!({"id":rpc_id,"result":result}))
    }

    fn push_event(&mut self, event: Value) -> Result<()> {
        self.events
            .lock()
            .map_err(|_| anyhow!("structured-session event queue is poisoned"))?
            .push(event)
    }
}

fn render_workload_invocation_result(
    template: &ValueTemplate,
    outcome: ryeos_runtime::workload_client::WorkloadClientOutcome,
) -> Result<Value> {
    use ryeos_runtime::workload_client::WorkloadClientOutcome;
    let success = outcome.succeeded();
    let result = match outcome {
        WorkloadClientOutcome::Dispatched { response } => response.result,
        WorkloadClientOutcome::Failed {
            code,
            message,
            retryable,
        } => json!({"code":code,"message":message,"retryable":retryable}),
    };
    match evaluate_template(
        template,
        &json!({"outcome":{"success":success,"result":result}}),
    ) {
        Ok(response) => Ok(response),
        // The child may already have committed. A presentation-byte refusal
        // must neither kill the parent session nor imply it is safe to rerun.
        // Keep the authoritative result in its ordinary execution owner.
        Err(_) => evaluate_template(
            template,
            &json!({"outcome":{
                "success":false,
                "result":{"code":"result-unavailable","retryable":false,
                    "execution_may_have_completed":true}
            }}),
        ),
    }
}

fn invocation_identity_matches(required: &BTreeMap<String, Value>, context: &Value) -> bool {
    required
        .iter()
        .all(|(pointer, expected)| context.pointer(pointer).unwrap_or(&Value::Null) == expected)
}

fn approval_expired_event(rule: &ServerRequestRule, context: &Value) -> Result<Value> {
    let message = context
        .get("message")
        .ok_or_else(|| anyhow!("approval expiry context has no pending message"))?;
    let request_id = message
        .get("id")
        .filter(|value| !value.is_null())
        .ok_or_else(|| anyhow!("expired approval has no request id"))?;
    let request_digest = ryeos_state::objects::canonical_value_digest(message)?;
    let upstream_session_id = context
        .pointer(&rule.correlation.upstream_session_pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| anyhow!("expired approval lost its upstream-session correlation"))?;
    let operation_id = context
        .pointer(&rule.correlation.operation_pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| anyhow!("expired approval lost its operation correlation"))?;
    Ok(json!({
        "event_type":"approval.expired",
        "payload":{
            "request_id":request_id,
            "operation_class":rule.operation_class,
            "upstream_session_id":upstream_session_id,
            "operation_id":operation_id,
            "request_digest":request_digest
        }
    }))
}

fn require_successful_internal_route(result: &Value, stage: &str) -> Result<()> {
    let response = result
        .get("response")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("internal recovery {stage} route omitted its response"))?;
    if response.get("error").is_some_and(|value| !value.is_null()) {
        bail!("structured workload rejected internal recovery {stage} route");
    }
    Ok(())
}

impl EventQueue {
    fn acknowledge_command_progress(
        &mut self,
        request_id: &str,
        digest: Option<&str>,
    ) -> Result<()> {
        let pending = self
            .command_progress
            .as_mut()
            .ok_or_else(|| anyhow!("unsolicited command progress acknowledgement"))?;
        if pending.acknowledged
            || pending.request_id != request_id
            || digest != Some(pending.digest.as_str())
        {
            bail!("command progress acknowledgement contradicts its exact request/batch");
        }
        pending.acknowledged = true;
        Ok(())
    }
    fn push(&mut self, event: Value) -> Result<()> {
        if self.events.len() >= MAX_EVENTS {
            bail!("structured workload event backlog is exhausted");
        }
        let bytes = serde_json::to_vec(&event)?.len();
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow!("event byte accounting overflow"))?;
        if self.bytes > 8 * 1024 * 1024 {
            bail!("structured-session event backlog byte ceiling is exhausted");
        }
        self.events.push_back(event);
        Ok(())
    }

    fn take_batch(
        &mut self,
        event_limit: usize,
        first_sequence: u64,
        previous_digest: Option<&str>,
    ) -> Result<Option<(Value, u64, String)>> {
        // The independent pushed-event channel must not outrun an earlier
        // request-correlated lifecycle commit (for example fast completion).
        if self
            .command_progress
            .as_ref()
            .is_some_and(|progress| !progress.acknowledged)
        {
            return Ok(None);
        }
        if self.events.is_empty() {
            return Ok(None);
        }
        let mut events = Vec::new();
        let mut observations = Vec::new();
        let mut accepted: Option<(Value, u64, String)> = None;
        for queued in self.events.iter().take(event_limit) {
            let mut event = queued.clone();
            observations.extend(
                event
                    .as_object_mut()
                    .and_then(|object| object.remove("session_observations"))
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default(),
            );
            events.push(event);
            let count = u64::try_from(events.len())?;
            let through_sequence = first_sequence
                .checked_add(count - 1)
                .ok_or_else(|| anyhow!("observation sequence overflow"))?;
            let mut body = json!({
                "first_sequence": first_sequence,
                "count": count,
                "previous_digest": previous_digest,
                "events": events,
                "session_observations": observations,
            });
            let digest = ryeos_state::objects::canonical_value_digest(&body)?;
            body.as_object_mut()
                .expect("observation batch is an object")
                .insert("batch_digest".to_owned(), Value::String(digest.clone()));
            if serde_json::to_vec(&body)?.len()
                > ryeos_state::objects::MAX_STRUCTURED_OBSERVATION_BATCH_BYTES
            {
                if accepted.is_none() {
                    bail!(
                        "one structured-session event exceeds the observation batch byte ceiling"
                    );
                }
                break;
            }
            accepted = Some((body, through_sequence, digest));
        }
        let accepted_count = accepted
            .as_ref()
            .and_then(|(body, _, _)| body.get("count"))
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("observation batch selection produced no event"))?;
        for _ in 0..usize::try_from(accepted_count)? {
            let event = self
                .events
                .pop_front()
                .ok_or_else(|| anyhow!("observation event queue changed during selection"))?;
            self.bytes = self.bytes.saturating_sub(serde_json::to_vec(&event)?.len());
        }
        Ok(accepted)
    }
}

fn evaluate_observations(rules: &[ObservationRule], context: &Value) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for rule in rules {
        if rule
            .when
            .iter()
            .all(|predicate| context.pointer(&predicate.pointer) == Some(&predicate.equals))
        {
            output.push(evaluate_template(&rule.value, context)?);
        }
    }
    Ok(output)
}

fn evaluate_template(template: &ValueTemplate, context: &Value) -> Result<Value> {
    match template {
        ValueTemplate::Literal { value } => Ok(value.clone()),
        ValueTemplate::Pointer {
            pointer,
            optional,
            max_string_bytes,
        } => match context.pointer(pointer) {
            Some(value) => {
                validate_template_value(value, *max_string_bytes)?;
                Ok(value.clone())
            }
            None if *optional => Ok(Value::Null),
            None => bail!("structured-session mapping pointer `{pointer}` is absent"),
        },
        ValueTemplate::Object { fields } => fields
            .iter()
            .map(|(key, value)| Ok((key.clone(), evaluate_template(value, context)?)))
            .collect::<Result<Map<String, Value>>>()
            .map(Value::Object),
        ValueTemplate::Array { values } => values
            .iter()
            .map(|value| evaluate_template(value, context))
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        ValueTemplate::Digest { pointer } => {
            let value = context.pointer(pointer).ok_or_else(|| {
                anyhow!("structured-session digest pointer `{pointer}` is absent")
            })?;
            Ok(Value::String(ryeos_state::objects::canonical_value_digest(
                value,
            )?))
        }
        ValueTemplate::JsonString { pointer, max_bytes } => {
            let value = context
                .pointer(pointer)
                .ok_or_else(|| anyhow!("JSON string mapping pointer is absent"))?;
            let encoded = lillux::canonical_json(value)?;
            if encoded.len() > *max_bytes {
                bail!("JSON string mapping exceeds its admitted byte bound");
            }
            Ok(Value::String(encoded))
        }
    }
}

fn validate_template_value(value: &Value, max_string_bytes: usize) -> Result<()> {
    fn visit(value: &Value, depth: usize, strings: &mut usize, max: usize) -> Result<()> {
        if depth > 32 {
            bail!("structured-session mapped value exceeds nesting bound");
        }
        match value {
            Value::String(value) => {
                *strings = strings
                    .checked_add(value.len())
                    .ok_or_else(|| anyhow!("mapped string byte count overflow"))?;
                if *strings > max {
                    bail!("structured-session mapped strings exceed byte bound");
                }
            }
            Value::Array(values) => {
                if values.len() > 4096 {
                    bail!("structured-session mapped array exceeds element bound");
                }
                for value in values {
                    visit(value, depth + 1, strings, max)?;
                }
            }
            Value::Object(values) => {
                if values.len() > 4096 {
                    bail!("structured-session mapped object exceeds field bound");
                }
                for value in values.values() {
                    visit(value, depth + 1, strings, max)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(value, 0, &mut 0, max_string_bytes)
}

impl Drop for StructuredWorkload {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_app_server(output: impl Read, sender: SyncSender<Result<Value, String>>) {
    let mut reader = BufReader::new(output);
    loop {
        let mut line = Vec::new();
        match (&mut reader)
            .take((MAX_APP_SERVER_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => {
                let _ = sender.send(Err("structured workload stdout closed".to_owned()));
                return;
            }
            Ok(_) if line.len() > MAX_APP_SERVER_LINE_BYTES => {
                let _ = sender.send(Err("structured workload line exceeds bound".to_owned()));
                return;
            }
            Ok(_) => {
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                let message = serde_json::from_slice(&line)
                    .map_err(|error| format!("invalid structured workload JSON: {error}"));
                if sender.send(message).is_err() {
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(format!("read structured workload stdout: {error}")));
                return;
            }
        }
    }
}

fn drain_private_stderr(stderr: impl Read) {
    // Diagnostics are intentionally not forwarded: upstream stderr may
    // contain device material, host paths, prompts, or credentials. Draining
    // prevents child blockage while retaining no second secret-bearing log.
    let mut reader = BufReader::new(stderr);
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// Wait until the workload announces its signed loopback listener contract on
/// stdout.  The provider owns the prefix in its admitted profile; Core only
/// accepts a decimal port appended to that prefix and constructs an exact
/// loopback endpoint.  A reader thread owns the pipe for its whole lifetime:
/// after reporting readiness it drains stdout, so a chatty workload cannot
/// deadlock on a full pipe.  Waiting for the first report is deadline-bounded.
fn wait_for_loopback_listening<R: Read + Send + 'static>(
    output: R,
    prefix: &str,
    deadline: MonotonicDeadline,
) -> Result<String> {
    let prefix = prefix.to_owned();
    let (announced, receiver) = sync_channel(1);
    thread::Builder::new()
        .name("ryeos-structured-session-http-stdout".to_owned())
        .spawn(move || {
            let mut reader = BufReader::new(output);
            let mut line = Vec::new();
            loop {
                line.clear();
                let read = (&mut reader)
                    .take((MAX_APP_SERVER_LINE_BYTES + 1) as u64)
                    .read_until(b'\n', &mut line);
                let announcement = match read {
                    Ok(0) => Err(
                        "structured workload stdout closed before announcing its listener"
                            .to_owned(),
                    ),
                    Ok(_) if line.len() > MAX_APP_SERVER_LINE_BYTES => {
                        Err("structured workload listening line exceeds its bound".to_owned())
                    }
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line);
                        let port = text.find(&prefix).and_then(|start| {
                            let port = text[start + prefix.len()..]
                                .trim_end()
                                .trim_end_matches(['\r', '\n'])
                                .trim();
                            (port.len() <= 5
                                && !port.is_empty()
                                && port.bytes().all(|byte| byte.is_ascii_digit()))
                            .then(|| format!("http://127.0.0.1:{port}"))
                        });
                        let Some(endpoint) = port else {
                            continue;
                        };
                        Ok(endpoint)
                    }
                    Err(error) => Err(format!(
                        "read structured workload listener announcement: {error}"
                    )),
                };
                let ready = announcement.is_ok();
                if announced.send(announcement).is_err() {
                    return;
                }
                if ready {
                    drain_discard(reader);
                    return;
                }
                return;
            }
        })
        .context("start bounded structured workload listener reader")?;
    receiver
        .recv_timeout(deadline.remaining())
        .map_err(|error| {
            anyhow!("structured workload did not announce its loopback listener: {error}")
        })?
        .map_err(|error| anyhow!(error))
}

fn drain_discard(output: impl Read) {
    let mut reader = BufReader::new(output);
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

/// Read the workload's server-sent event stream and deliver each event into
/// the shared incoming channel using the stdio message envelopes: an event
/// whose type matches a server-request rule arrives as that pending request
/// (its operation pointer supplies the reply correlation id), everything
/// else arrives as a notification. Notification matching, schema checks and
/// fail-closed handling of unknown types stay in `route`.
fn read_http_events(
    client: reqwest::blocking::Client,
    base_url: String,
    authorization: String,
    event_path: String,
    event_type_pointer: String,
    event_properties_pointer: String,
    asks: Vec<(String, String)>,
    sender: SyncSender<Result<Value, String>>,
) {
    let deliver = |message: Result<Value, String>| {
        if sender.send(message).is_err() {
            return false;
        }
        true
    };
    let response = client
        .get(format!("{base_url}{event_path}"))
        .header("Accept", "text/event-stream")
        .header("Authorization", &authorization)
        .send();
    let mut response = match response {
        Ok(response) => response,
        Err(error) => {
            deliver(Err(format!(
                "open structured workload event stream: {error}"
            )));
            return;
        }
    };
    if !response.status().is_success() {
        deliver(Err(format!(
            "structured workload refused the event stream: {}",
            response.status()
        )));
        return;
    }
    let is_event_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"))
        });
    if !is_event_stream {
        deliver(Err(
            "structured workload event stream has no text/event-stream content type".to_owned(),
        ));
        return;
    }
    let mut reader = BufReader::new(&mut response);
    let mut event = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader)
            .take((MAX_APP_SERVER_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => {
                let message = if event.is_empty() {
                    "structured workload event stream closed"
                } else {
                    "structured workload event stream closed with an unterminated event"
                };
                deliver(Err(message.to_owned()));
                return;
            }
            Ok(_) if line.len() > MAX_APP_SERVER_LINE_BYTES => {
                deliver(Err(
                    "structured workload event line exceeds bound".to_owned()
                ));
                return;
            }
            Ok(_) => {
                while matches!(line.last(), Some(b'\n' | b'\r')) {
                    line.pop();
                }
                if line.is_empty() {
                    if event.is_empty() {
                        continue;
                    }
                    let decoded = match serde_json::from_slice::<Value>(&event) {
                        Ok(decoded) => decoded,
                        Err(error) => {
                            deliver(Err(format!(
                                "invalid structured workload event JSON: {error}"
                            )));
                            return;
                        }
                    };
                    let event_type = decoded
                        .pointer(&event_type_pointer)
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned);
                    let properties = decoded.pointer(&event_properties_pointer).cloned();
                    let (Some(event_type), Some(properties)) = (event_type, properties) else {
                        deliver(Err("structured workload event contradicts its admitted envelope projection".to_owned()));
                        return;
                    };
                    if let Some((_, pointer)) =
                        asks.iter().find(|(method, _)| *method == event_type)
                    {
                        let message = json!({"method": event_type, "params": properties});
                        let root = json!({"message": message});
                        let Some(id) = pointer
                            .strip_prefix("/message")
                            .and_then(|rest| root.pointer(rest))
                            .filter(|value| !value.is_null())
                        else {
                            deliver(Err(format!(
                                "structured workload ask `{event_type}` lacks its correlation id"
                            )));
                            return;
                        };
                        let ask = json!({"id": id, "method": event_type, "params": properties, "envelope": decoded});
                        if !deliver(Ok(ask)) {
                            return;
                        }
                    } else {
                        let notification = json!({"method": event_type, "params": properties, "envelope": decoded});
                        if !deliver(Ok(notification)) {
                            return;
                        }
                    }
                    event.clear();
                    continue;
                }
                if let Some(data) = line.strip_prefix(b"data: ") {
                    if event.len().saturating_add(data.len()) > MAX_APP_SERVER_LINE_BYTES {
                        deliver(Err(
                            "structured workload aggregate event exceeds bound".to_owned()
                        ));
                        return;
                    }
                    event.extend_from_slice(data);
                } else if line == b"data:" {
                    // An empty data frame still separates frames; nothing to append.
                } else if let Some(field) = line.strip_prefix(b"data:") {
                    if event.len().saturating_add(field.len()) > MAX_APP_SERVER_LINE_BYTES {
                        deliver(Err(
                            "structured workload aggregate event exceeds bound".to_owned()
                        ));
                        return;
                    }
                    event.extend_from_slice(field);
                }
            }
            Err(error) => {
                deliver(Err(format!(
                    "read structured workload event stream: {error}"
                )));
                return;
            }
        }
    }
}

fn validate_schema_from_map(
    schemas: &HashMap<String, Value>,
    identity: &str,
    value: &Value,
) -> Result<()> {
    let schema = schemas
        .get(identity)
        .ok_or_else(|| anyhow!("structured-session schema identity is not admitted"))?;
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| anyhow!("compile admitted JSON schema `{identity}`: {error}"))?;
    if validator.validate(value).is_err() {
        let instance_digest = ryeos_state::objects::canonical_value_digest(value)?;
        bail!(
            "structured-session value failed schema `{identity}` (instance digest {instance_digest})"
        );
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn base64_standard(value: String) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(value)
}

/// Percent-encode one substituted path segment for unreserved characters
/// only, so an upstream session or permission id can never reshape the path.
fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn substitute_http_path(path: &str, substitutions: &[(String, String)]) -> Result<String> {
    let mut resolved = String::new();
    let mut remainder = path;
    while let Some(start) = remainder.find('{') {
        let end = remainder[start..]
            .find('}')
            .ok_or_else(|| anyhow!("structured-session HTTP path placeholder is unterminated"))?;
        resolved.push_str(&remainder[..start]);
        let name = &remainder[start + 1..start + end];
        let value = substitutions
            .iter()
            .find(|(placeholder, _)| placeholder == name)
            .map(|(_, value)| value.as_str())
            .ok_or_else(|| anyhow!("structured-session HTTP path placeholder is unbound"))?;
        resolved.push_str(&encode_path_segment(value));
        remainder = &remainder[start + end + 1..];
    }
    resolved.push_str(remainder);
    Ok(resolved)
}

/// Run the bounded HTTP executor. The receiver lock is held only while taking
/// the next job; actual network contact is concurrent and never blocks the
/// bridge's event/control loop.
fn http_request_worker(
    requests: Arc<Mutex<Receiver<HttpRequest>>>,
    sender: SyncSender<Result<Value, String>>,
) {
    loop {
        let request = match requests
            .lock()
            .expect("HTTP request receiver lock poisoned")
            .recv()
        {
            Ok(request) => request,
            Err(_) => return,
        };
        let result = perform_http_request(
            &request.client,
            &request.url,
            &request.method,
            &request.authorization,
            &request.body,
        );
        let delivered = match request.id {
            Some(id) => match result {
                Ok(result) => Ok(json!({"id": id, "result": result})),
                Err(error) => Ok(json!({"id": id, "error": {"message": error.to_string()}})),
            },
            None => match result {
                Ok(_) => continue,
                Err(error) => Err(format!(
                    "deliver structured workload server-request reply: {error}"
                )),
            },
        };
        if sender.send(delivered).is_err() {
            return;
        }
    }
}

/// Perform one bounded HTTP exchange with the loopback workload. Body
/// methods carry the admitted params as JSON; bodyless methods carry flat
/// scalar params as query entries. Empty and 204 responses settle as null
/// so asynchronous routes can complete through durable events.
fn perform_http_request(
    client: &reqwest::blocking::Client,
    url: &str,
    method: &str,
    authorization: &str,
    params: &Value,
) -> Result<Value> {
    let bodyless = matches!(method, "GET" | "DELETE");
    let mut request = client
        .request(
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|_| anyhow!("structured-session HTTP method is not canonical"))?,
            url,
        )
        .header("Authorization", authorization);
    if bodyless {
        if let Some(query) = params.as_object() {
            for (key, value) in query {
                let value = value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string());
                request = request.query(&[(key, value)]);
            }
        }
    } else {
        request = request.json(params);
    }
    let response = request
        .send()
        .with_context(|| format!("contact structured workload `{method} {url}`"))?;
    if !response.status().is_success() {
        bail!(
            "structured workload refused `{method} {url}` with {}",
            response.status()
        );
    }
    if matches!(response.status().as_u16(), 204) {
        return Ok(Value::Null);
    }
    let mut body = Vec::new();
    response
        .take((MAX_APP_SERVER_LINE_BYTES + 1) as u64)
        .read_to_end(&mut body)
        .context("read structured workload HTTP response")?;
    if body.len() > MAX_APP_SERVER_LINE_BYTES {
        bail!("structured workload HTTP response exceeds its bound");
    }
    if body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&body).context("decode structured workload HTTP response")
}

fn apply_route_parameters(route: &RouteRule, params: &mut Value, workspace: &str) -> Result<()> {
    let object = params
        .as_object_mut()
        .ok_or_else(|| anyhow!("structured-session route payload must be an object"))?;
    for field in &route.forbidden_fields {
        if object.contains_key(field) {
            bail!("structured-session route field `{field}` is not admitted");
        }
    }
    for field in &route.forbidden_non_null_fields {
        if object.get(field).is_some_and(|value| !value.is_null()) {
            bail!("structured-session route field `{field}` is not admitted");
        }
    }
    for field in route.fixed_params.keys().chain(&route.workspace_fields) {
        if object.contains_key(field) {
            bail!("structured-session route cannot override admitted field `{field}`");
        }
    }
    for (field, value) in &route.fixed_params {
        object.insert(field.clone(), value.clone());
    }
    for field in &route.workspace_fields {
        object.insert(field.clone(), Value::String(workspace.to_owned()));
    }
    Ok(())
}

fn prepare_session_binding(
    binding: Option<&SessionBindingRule>,
    bound_session_id: &Option<String>,
    params: &mut Value,
    inject_into_upstream_params: bool,
) -> Result<Option<String>> {
    let Some(binding) = binding else {
        return Ok(None);
    };
    let object = params
        .as_object_mut()
        .ok_or_else(|| anyhow!("structured-session route payload must be an object"))?;
    match binding.action {
        SessionBindingAction::BindNew => {
            if bound_session_id.is_some() {
                bail!("structured-session start is single-use for one bound session");
            }
            Ok(None)
        }
        SessionBindingAction::BindExpected => {
            if bound_session_id.is_some() {
                bail!("structured-session recovery cannot replace an existing binding");
            }
            let field = binding
                .request_field
                .as_deref()
                .ok_or_else(|| anyhow!("bind_expected route has no request field"))?;
            let expected = object
                .get(field)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow!("structured-session recovery has no expected session id"))?
                .to_owned();
            Ok(Some(expected))
        }
        SessionBindingAction::Require => {
            let bound = bound_session_id
                .as_deref()
                .ok_or_else(|| anyhow!("structured-session route requires a bound session"))?;
            let field = binding
                .request_field
                .as_deref()
                .ok_or_else(|| anyhow!("bound route has no request field"))?;
            if object
                .get(field)
                .is_some_and(|value| value.as_str() != Some(bound))
            {
                bail!("structured-session route attempted to target another session");
            }
            // Stdio uses the binding field in the upstream message. HTTP
            // routes address the bound session through their separately
            // admitted path projection; injecting it into vendor JSON would
            // violate a body schema that deliberately does not expose it.
            if inject_into_upstream_params {
                object.insert(field.to_owned(), Value::String(bound.to_owned()));
            }
            Ok(Some(bound.to_owned()))
        }
    }
}

fn settle_session_binding(
    binding: Option<&SessionBindingRule>,
    expected: Option<&str>,
    response: &Value,
    bound_session_id: &mut Option<String>,
) -> Result<()> {
    let Some(binding) = binding else {
        return Ok(());
    };
    if binding.action == SessionBindingAction::Require {
        return Ok(());
    }
    let pointer = binding
        .response_pointer
        .as_deref()
        .ok_or_else(|| anyhow!("session-binding route has no response pointer"))?;
    let observed = response
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("structured workload did not return a bound session id"))?;
    if expected.is_some_and(|expected| observed != expected) {
        bail!("structured workload recovery returned a different session id");
    }
    *bound_session_id = Some(observed.to_owned());
    Ok(())
}

fn approval_accept_allowed(rule: &ServerRequestRule, context: &Value) -> bool {
    !rule.deny_only
        && !rule.permission_delta_fields.iter().any(|pointer| {
            context
                .pointer(pointer)
                .is_none_or(|value| !value.is_null())
        })
        && rule.required_review_fields.iter().all(|pointer| {
            context.pointer(pointer).is_some_and(|value| match value {
                Value::String(value) => !value.trim().is_empty(),
                Value::Array(value) => !value.is_empty(),
                Value::Null => false,
                _ => true,
            })
        })
}

fn require_exact_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        bail!("unknown structured-session request field `{key}`");
    }
    Ok(())
}

fn canonical_id(value: &Value) -> Result<String> {
    if !value.is_string() && !value.is_number() {
        bail!("JSON-RPC id must be a string or number");
    }
    serde_json::to_string(value).context("encode JSON-RPC id")
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("missing environment {name}"))?;
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        bail!("environment {name} is not canonical and bounded");
    }
    Ok(value)
}

fn optional_env(name: &str) -> Result<Option<String>> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(None);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow!("environment {name} is not valid UTF-8"))?;
    if value.is_empty() || value.len() > 128 * 1024 || value.chars().any(char::is_control) {
        bail!("environment {name} is not canonical and bounded");
    }
    Ok(Some(value))
}

fn require_absolute_normalized(label: &str, value: &str) -> Result<()> {
    let path = std::path::Path::new(value);
    if !path.is_absolute()
        || path.components().enumerate().any(|(index, component)| {
            !matches!(
                (index, component),
                (0, std::path::Component::RootDir) | (_, std::path::Component::Normal(_))
            )
        })
    {
        bail!("{label} must be an absolute normalized path");
    }
    Ok(())
}

fn validate_incoming_frame(frame: &Frame) -> Result<()> {
    if frame.protocol != WIRE_PROTOCOL || frame.version != WIRE_VERSION {
        bail!("RyeOS dedicated-session wire identity mismatch");
    }
    match frame.kind {
        FrameKind::Request | FrameKind::Control
            if frame.request_id.as_deref().is_some_and(|id| !id.is_empty())
                && frame.body.is_some() =>
        {
            Ok(())
        }
        FrameKind::Cancel
            if frame.request_id.as_deref().is_some_and(|id| !id.is_empty())
                && frame.body.is_none() =>
        {
            Ok(())
        }
        FrameKind::ObservationAck if frame.body.is_some() => Ok(()),
        _ => bail!("daemon sent an invalid frame shape"),
    }
}

fn read_session_frames(
    mut stream: lillux::InheritedDuplexChannel,
    sender: SyncSender<std::result::Result<Frame, String>>,
) {
    loop {
        let frame = read_frame(&mut stream).map_err(|error| format!("{error:#}"));
        let terminal = frame.is_err();
        if sender.send(frame).is_err() || terminal {
            return;
        }
    }
}

fn read_frame(stream: &mut lillux::InheritedDuplexChannel) -> Result<Frame> {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .context("read RyeOS frame length")?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        bail!("RyeOS frame exceeds bound");
    }
    let mut body = vec![0u8; length];
    stream
        .read_exact(&mut body)
        .context("read RyeOS frame body")?;
    serde_json::from_slice(&body).context("decode RyeOS frame")
}

fn write_frame(stream: &mut lillux::InheritedDuplexChannel, frame: &Frame) -> Result<()> {
    let encoded = serde_json::to_vec(frame).context("encode RyeOS frame")?;
    if encoded.is_empty() || encoded.len() > MAX_FRAME_BYTES {
        bail!("RyeOS output frame exceeds bound");
    }
    stream.write_all(&(encoded.len() as u32).to_be_bytes())?;
    stream.write_all(&encoded)?;
    stream.flush().context("flush RyeOS frame")
}

fn write_final(
    stream: &mut lillux::InheritedDuplexChannel,
    request_id: &str,
    body: Value,
) -> Result<()> {
    write_frame(
        stream,
        &Frame {
            protocol: WIRE_PROTOCOL.to_owned(),
            version: WIRE_VERSION,
            kind: FrameKind::Final,
            request_id: Some(request_id.to_owned()),
            body: Some(body),
        },
    )
}

fn write_error(
    stream: &mut lillux::InheritedDuplexChannel,
    request_id: &str,
    message: &str,
) -> Result<()> {
    let bounded = message.chars().take(2_048).collect::<String>();
    write_frame(
        stream,
        &Frame {
            protocol: WIRE_PROTOCOL.to_owned(),
            version: WIRE_VERSION,
            kind: FrameKind::Error,
            request_id: Some(request_id.to_owned()),
            body: Some(json!({"message":bounded})),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt as _;

    #[test]
    fn invocation_identity_and_progress_ack_are_exact() {
        let required = BTreeMap::from([
            ("/message/params/tool".to_owned(), json!("execute")),
            ("/message/params/namespace".to_owned(), Value::Null),
        ]);
        for params in [
            json!({"tool":"execute"}),
            json!({"tool":"execute","namespace":null}),
        ] {
            assert!(invocation_identity_matches(
                &required,
                &json!({"message":{"params":params}})
            ));
        }
        for params in [
            json!({"tool":"other"}),
            json!({"tool":"execute","namespace":"other"}),
        ] {
            assert!(!invocation_identity_matches(
                &required,
                &json!({"message":{"params":params}})
            ));
        }
        let mut queue = EventQueue::default();
        queue.command_progress = Some(CommandProgressBarrier {
            request_id: "command-one".to_owned(),
            digest: "a".repeat(64),
            acknowledged: false,
            deadline: MonotonicDeadline::after(OBSERVATION_ACK_TIMEOUT),
        });
        assert!(
            queue
                .acknowledge_command_progress("command-two", Some(&"a".repeat(64)))
                .is_err()
        );
        assert!(
            queue
                .acknowledge_command_progress("command-one", Some(&"b".repeat(64)))
                .is_err()
        );
        assert!(!queue.command_progress.as_ref().unwrap().acknowledged);
        queue
            .push(json!({"event_type":"turn.completed","payload":{},"session_observations":[]}))
            .unwrap();
        assert!(queue.take_batch(128, 1, None).unwrap().is_none());
        assert_eq!(queue.events.len(), 1);
        queue
            .acknowledge_command_progress("command-one", Some(&"a".repeat(64)))
            .unwrap();
        assert!(queue.take_batch(128, 1, None).unwrap().is_some());
        assert!(
            queue
                .acknowledge_command_progress("command-one", Some(&"a".repeat(64)))
                .is_err()
        );
    }

    #[test]
    fn oversized_invocation_presentation_is_failure_not_rerun_permission() {
        let template: ValueTemplate = serde_json::from_value(json!({"op":"object","fields":{
            "success":{"op":"pointer","pointer":"/outcome/success"},
            "text":{"op":"json_string","pointer":"/outcome/result","max_bytes":256}
        }}))
        .unwrap();
        let response = render_workload_invocation_result(
            &template,
            ryeos_runtime::workload_client::WorkloadClientOutcome::Failed {
                code: "child-failure".to_owned(),
                message: "x".repeat(1024),
                retryable: false,
            },
        )
        .unwrap();
        assert_eq!(response["success"], false);
        let result: Value = serde_json::from_str(response["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            result,
            json!({"code":"result-unavailable","retryable":false,"execution_may_have_completed":true})
        );
    }

    #[test]
    fn forbidden_fields_reject_presence_including_null() {
        let rule: RouteRule = serde_json::from_value(json!({
            "id":"record.open",
            "method":"record/open",
            "effect_class":"session_mutation",
            "request_schema":"request.json",
            "response_schema":"response.json",
            "fixed_params":{},
            "workspace_fields":[],
            "forbidden_non_null_fields":[],
            "forbidden_fields":["authorityOverride"],
            "response_predicates":[],
            "observations":[],
            "result_retention":"ephemeral",
            "ceremony":null
        }))
        .unwrap();
        for value in [Value::Null, Value::String("disabled".to_owned())] {
            let mut params = json!({"authorityOverride": value});
            assert!(apply_route_parameters(&rule, &mut params, "/workspace").is_err());
        }
    }

    #[test]
    fn observation_batches_respect_the_exact_serialized_byte_ceiling() {
        let mut queue = EventQueue::default();
        for index in 0..8 {
            queue
                .push(json!({
                    "event_type":"delta",
                    "payload":{"index":index,"text":"x".repeat(64 * 1024)},
                    "session_observations":[]
                }))
                .unwrap();
        }
        let (body, through, _) = queue.take_batch(128, 1, None).unwrap().unwrap();
        assert!(
            serde_json::to_vec(&body).unwrap().len()
                <= ryeos_state::objects::MAX_STRUCTURED_OBSERVATION_BATCH_BYTES
        );
        let count = body["count"].as_u64().unwrap();
        assert!(count > 0 && count < 8);
        assert_eq!(through, count);
        assert_eq!(queue.events.len(), 8 - usize::try_from(count).unwrap());
    }

    fn gating_approval_profile() -> StructuredSessionProfile {
        serde_json::from_value(json!({
            "schema_version":ryeos_engine::structured_session_profile::STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION,
            "transport":"stdio_jsonrpc",
            "http_sse":null,
            "configuration_authority":"immutable_argv",
            "workload_realization_id":"test-realization",
            "workload_executable":"sh",
            "workload_args":[
                "-c",
                "IFS= read -r request; printf '%s\\n' '{\"id\":\"approval-one\",\"method\":\"approval/request\",\"params\":{\"session\":\"session-one\",\"operation\":\"operation-one\",\"command\":\"true\"}}'; IFS= read -r decision; printf '%s\\n' '{\"id\":1,\"result\":{\"ok\":true}}'"
            ],
            "workload_home_env":"TEST_WORKLOAD_HOME",
            "required_process_environment":[],
            "workload_client":null,
            "baseline_config":"baseline.conf",
            "baseline_destination":"config.toml",
            "portable_state":null,
            "credential_subject":null,
            "initialization":[],
            "recovery":null,
            "route_sets":{"session":["operation.run"]},
            "routes":[{
                "id":"operation.run",
                "method":"operation/run",
                "effect_class":"session_mutation",
                "request_schema":"request.json",
                "response_schema":"response.json",
                "fixed_params":{},
                "workspace_fields":[],
                "forbidden_non_null_fields":[],
                "forbidden_fields":[],
                "response_predicates":[],
                "observations":[],
                "result_retention":"ephemeral",
                "ceremony":null,
                "session_binding":null
            }],
            "notifications":[],
            "ignored_notifications":{},
            "server_requests":[{
                "method":"approval/request",
                "schema":"approval.json",
                "operation_class":"command",
                "correlation":{
                    "upstream_session_pointer":"/message/params/session",
                    "operation_pointer":"/message/params/operation"
                },
                "responses":{
                    "accept":{"op":"literal","value":{"decision":"accept"}},
                    "cancel":{"op":"literal","value":{"decision":"cancel"}},
                    "decline":{"op":"literal","value":{"decision":"decline"}},
                    "expire":{"op":"literal","value":{"decision":"decline"}}
                },
                "deny_only":false,
                "permission_delta_fields":[],
                "required_review_fields":["/message/params/command"],
                "display":{"op":"literal","value":{"command":"true"}}
            }]
        }))
        .unwrap()
    }

    #[test]
    fn protocol_invocation_subprocess_entry() {
        const CHANNEL_ENV: &str = "RYEOS_TEST_INVOCATION_CHANNEL";
        if std::env::var_os(CHANNEL_ENV).is_none() {
            return;
        }
        // SAFETY: the parent test binds and transfers this unique endpoint
        // through Lillux before exec. No borrowed or ambient FD is adopted.
        let channel =
            unsafe { lillux::take_inherited_duplex_channel_from_env(CHANNEL_ENV) }.unwrap();
        let broker = workload_client_broker::start(
            channel,
            &[ryeos_runtime::workload_client::WorkloadClientIngress::StructuredSession],
        )
        .unwrap();
        assert!(broker.endpoint().is_none());
        let root = tempfile::tempdir().unwrap();
        let mut profile = gating_approval_profile();
        let progress_test = std::env::var_os("RYEOS_TEST_INVOCATION_PROGRESS").is_some();
        profile.workload_args = vec!["-c".to_owned(),
            "IFS= read -r request; printf '%s\\n' '{\"id\":\"rpc-one\",\"method\":\"operation/execute\",\"params\":{\"session\":\"session-one\",\"turn\":\"turn-one\",\"call\":\"call-one\",\"name\":\"execute\",\"arguments\":{\"item_ref\":\"tool:fixture/check\",\"ref_bindings\":{},\"params\":{}}}}'; IFS= read -r reply; case \"$reply\" in *'\"success\":false'*) printf '%s\\n' '{\"id\":1,\"result\":{\"ok\":true}}';; *) exit 9;; esac".to_owned()];
        let mapping = serde_json::from_value(json!({
            "registration_route":"session.start","registration_field":"tools",
            "registration":{"op":"literal","value":[]},
            "method":"operation/execute","request_schema":"invoke.json","response_schema":"invoked.json",
            "session_pointer":"/message/params/session","operation_pointer":"/message/params/turn","call_pointer":"/message/params/call",
            "required_values":{"/message/params/name":"execute"},
            "request":{"op":"pointer","pointer":"/message/params/arguments"},
            "response":{"op":"object","fields":{"success":{"op":"pointer","pointer":"/outcome/success"}}}
        })).unwrap();
        profile.workload_client = Some(StructuredSessionWorkloadClient {
            cli_endpoint_env: None,
            structured_session: Some(mapping),
        });
        if progress_test {
            profile.workload_args[1] = profile.workload_args[1].replacen("IFS= read -r request;", r#"IFS= read -r request; printf '%s\n' '{"method":"operation/started","params":{"session":"session-one","turn":"turn-one"}}';"#, 1);
            profile.notifications.push(serde_json::from_value(json!({
                "method":"operation/started","schema":"progress.json","event_type":"operation.started","durable":true,
                "upstream_session_pointer":"/message/params/session",
                "payload":{"op":"literal","value":{}},"ceremony_clear":false,
                "observations":[{"when":[],"value":{"op":"literal","value":{
                    "kind":"state","expected":"idle","next":"turn_running","turn_id":"turn-one"
                }}}]
            })).unwrap());
        }
        let (_, controls) = sync_channel(1);
        let (results, progress_results) = sync_channel(1);
        let mut workload = StructuredWorkload::start("/bin/sh", root.path().to_str().unwrap(), root.path().to_str().unwrap(),
            profile, "session".to_owned(), HashSet::from([RouteEffectClass::SessionMutation]),
            HashMap::from([("invoke.json".to_owned(),json!({"type":"object"})),
                ("invoked.json".to_owned(),json!({"type":"object","required":["success"],"properties":{"success":{"type":"boolean"}}}))]),
            Arc::new(Mutex::new(EventQueue::default())), controls, results, "/bin/sh", None, &BTreeMap::new(), Vec::new()).unwrap();
        workload.bound_session_id = Some("session-one".to_owned());
        workload.workload_channel = Some(broker.channel());
        let reply = if progress_test {
            workload
                .schemas
                .insert("progress.json".to_owned(), json!({"type":"object"}));
            workload.command_progress = Some("command-one".to_owned());
            workload.active_progress_notifications = vec!["operation/started".to_owned()];
            assert!(
                workload
                    .route(json!({"method":"operation/started",
                "params":{"session":"different-session","turn":"turn-one"}}))
                    .is_err()
            );
            assert!(progress_results.try_recv().is_err());
            assert!(workload.early_command_observations.is_empty());
            let key = canonical_id(&json!(1)).unwrap();
            workload.outstanding.insert(key.clone());
            workload
                .send(&json!({"id":1,"method":"operation/run","params":{}}))
                .unwrap();
            workload.receive_one(Duration::from_secs(1)).unwrap();
            let (request_id, progress) = progress_results
                .recv_timeout(Duration::from_secs(1))
                .unwrap();
            let WorkloadCommandOutput::Delta(progress) = progress.unwrap() else {
                panic!("expected progress delta")
            };
            workload.receive_one(Duration::from_secs(1)).unwrap();
            workload.settle_workload_invocations().unwrap();
            assert!(matches!(
                workload.workload_invocations[0].delivery,
                WorkloadInvocationDelivery::AwaitingProgress { .. }
            ));
            {
                let mut events = workload.events.lock().unwrap();
                assert!(events.take_batch(128, 1, None).unwrap().is_none());
                events
                    .acknowledge_command_progress(
                        &request_id,
                        Some(&ryeos_state::objects::canonical_value_digest(&progress).unwrap()),
                    )
                    .unwrap();
            }
            let deadline = MonotonicDeadline::after(Duration::from_secs(5));
            while !workload.responses.contains_key(&key) {
                assert!(!deadline.has_elapsed());
                workload.receive_one(Duration::from_millis(10)).unwrap();
            }
            workload.responses.remove(&key).unwrap()
        } else {
            workload
                .call_raw("operation/run", json!({}), Duration::from_secs(5))
                .unwrap()
        };
        assert_eq!(reply["result"]["ok"], true);
        assert!(workload.workload_invocations.is_empty());
        workload.child.wait().unwrap();
    }

    #[test]
    fn in_flight_upstream_call_services_a_gating_workload_invocation() {
        run_gating_invocation(false);
    }

    #[test]
    fn invocation_waits_for_exact_progress_acceptance_before_dispatch() {
        run_gating_invocation(true);
    }

    fn run_gating_invocation(progress: bool) {
        use ryeos_runtime::workload_client::*;
        let (mut channel, child_channel) = lillux::inherited_duplex_channel_pair().unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        if progress {
            command.env("RYEOS_TEST_INVOCATION_PROGRESS", "1");
        }
        command.args([
            "--exact",
            "tests::protocol_invocation_subprocess_entry",
            "--nocapture",
        ]);
        child_channel
            .bind_to_command(&mut command, "RYEOS_TEST_INVOCATION_CHANNEL")
            .unwrap();
        let mut child = command.spawn().unwrap();
        drop(command);
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        let mut channel = channel.with_deadline(deadline);
        write_frame(
            &mut channel,
            &WorkloadClientBootFrame {
                protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
                grant_digest: "a".repeat(64),
                ingresses: vec![WorkloadClientIngress::StructuredSession],
                execution_presentation: json!([{}]),
                max_in_flight: 1,
                max_request_bytes: 4096,
                max_lifetime_seconds: 10,
            },
        )
        .unwrap();
        let ready: WorkloadClientReadyFrame = read_frame(&mut channel).unwrap();
        ready.validate().unwrap();
        let request: WorkloadClientDispatchFrame = read_frame(&mut channel).unwrap();
        request.validate().unwrap();
        assert_eq!(
            request.source,
            WorkloadInvocationSource::StructuredSession {
                upstream_session_id: "session-one".to_owned(),
                operation_id: "turn-one".to_owned(),
                call_id: "call-one".to_owned(),
            }
        );
        write_frame(
            &mut channel,
            &WorkloadClientResponseFrame {
                protocol: WORKLOAD_CLIENT_PROTOCOL.to_owned(),
                request_id: request.request.request_id,
                outcome: WorkloadClientOutcome::Failed {
                    code: "test-child-failed".to_owned(),
                    message: "deliberately failed child".to_owned(),
                    retryable: false,
                },
            },
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn in_flight_upstream_call_services_a_gating_approval() {
        let root = tempfile::tempdir().unwrap();
        let workload_home = root.path().join("home");
        std::fs::create_dir(&workload_home).unwrap();
        let profile = gating_approval_profile();
        let schemas = HashMap::from([
            (
                "request.json".to_owned(),
                json!({"type":"object","additionalProperties":false}),
            ),
            (
                "response.json".to_owned(),
                json!({
                    "type":"object",
                    "required":["ok"],
                    "properties":{"ok":{"const":true}},
                    "additionalProperties":false
                }),
            ),
            (
                "approval.json".to_owned(),
                json!({
                    "type":"object",
                    "required":["session","operation","command"],
                    "properties":{
                        "session":{"type":"string"},
                        "operation":{"type":"string"},
                        "command":{"type":"string"}
                    },
                    "additionalProperties":false
                }),
            ),
        ]);
        let events = Arc::new(Mutex::new(EventQueue::default()));
        let (control_sender, controls) = sync_channel(1);
        let (result_sender, results) = sync_channel(1);
        let observed_events = Arc::clone(&events);
        let controller = thread::spawn(move || {
            let deadline = MonotonicDeadline::after(Duration::from_secs(2));
            loop {
                if observed_events
                    .lock()
                    .unwrap()
                    .events
                    .iter()
                    .any(|event| event["event_type"] == "approval.requested")
                {
                    control_sender
                        .send(PendingControl {
                            request_id: "control-one".to_owned(),
                            body: json!({
                                "kind":"approval_decision",
                                "request_id":"approval-one",
                                "request_digest":ryeos_state::objects::canonical_value_digest(&json!({
                                    "id":"approval-one",
                                    "method":"approval/request",
                                    "params":{"session":"session-one","operation":"operation-one","command":"true"}
                                })).unwrap(),
                                "decision":"accept",
                                "reservation_token":"reservation-one"
                            }),
                        })
                        .unwrap();
                    return;
                }
                assert!(!deadline.has_elapsed(), "approval event was not surfaced");
                lillux::time::sleep(Duration::from_millis(1));
            }
        });
        let mut workload = StructuredWorkload::start(
            "/bin/sh",
            root.path().to_str().unwrap(),
            workload_home.to_str().unwrap(),
            profile,
            "session".to_owned(),
            HashSet::from([RouteEffectClass::SessionMutation]),
            schemas,
            events,
            controls,
            result_sender,
            "/bin/sh",
            None,
            &BTreeMap::new(),
            Vec::new(),
        )
        .unwrap();
        workload.bound_session_id = Some("session-one".to_owned());

        let result = workload
            .handle(
                json!({"route_id":"operation.run","payload":{}}),
                root.path().to_str().unwrap(),
            )
            .unwrap();

        controller.join().unwrap();
        assert_eq!(result["response"]["result"], json!({"ok":true}));
        let (control_id, control_result) = results.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(control_id, "control-one");
        let WorkloadCommandOutput::Final(control_result) = control_result.unwrap() else {
            panic!("expected final control result")
        };
        assert_eq!(control_result, json!({"resolved":true}));

        let sentinel = "DEVICE-CREDENTIAL-SENTINEL";
        let schema_error = workload
            .validate_schema("response.json", &json!({"ok":sentinel}))
            .unwrap_err()
            .to_string();
        assert!(!schema_error.contains(sentinel));
        assert!(schema_error.contains("instance digest"));

        let expired_digest = "a".repeat(64);
        workload
            .expired_server_requests
            .push_back(ExpiredServerRequest {
                id: canonical_id(&json!("approval-expired")).unwrap(),
                request_digest: expired_digest.clone(),
            });
        let expired_request = json!({
            "operation":"approval",
            "requestId":"approval-expired",
            "decision":"decline",
        });
        let expired = workload
            .handle_approval(expired_request.as_object().unwrap(), &expired_digest)
            .unwrap();
        assert_eq!(
            expired,
            json!({
                "resolved":false,
                "outcome":"expired",
                "request_id":"approval-expired",
                "request_digest":expired_digest,
            })
        );

        let reused_key = canonical_id(&json!("approval-reused")).unwrap();
        let old_digest = "b".repeat(64);
        let new_digest = "c".repeat(64);
        workload.server_requests.insert(
            reused_key.clone(),
            PendingServerRequest {
                message: json!({
                    "id":"approval-reused",
                    "method":"approval/request",
                    "params":{"session":"session-one","operation":"operation-new","command":"false"}
                }),
                request_digest: new_digest,
                expires_at: MonotonicDeadline::after(APPROVAL_TTL),
            },
        );
        let late_old_decision = json!({
            "operation":"approval",
            "requestId":"approval-reused",
            "decision":"decline",
        });
        assert!(
            workload
                .handle_approval(late_old_decision.as_object().unwrap(), &old_digest)
                .is_err()
        );
        assert!(workload.server_requests.contains_key(&reused_key));
    }

    const HTTP_FIXTURE_SERVER: &str = r#"
import base64, json, os, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

expected = "Basic " + base64.b64encode(
    ("%s:%s" % (os.environ["FX_HTTP_USER"], os.environ["FX_HTTP_PASSWORD"])).encode()
).decode()

for xdg in ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME", "XDG_CACHE_HOME"):
    if not os.environ.get(xdg, "").strip():
        raise SystemExit("workload XDG home was not supplied: " + xdg)

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def authorized(self):
        if self.headers.get("Authorization") != expected:
            self.send_error(401)
            return False
        return True

    def reply(self, status, value):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if not self.authorized():
            return
        if self.path == "/session":
            self.reply(200, {"session_id": "ses_fixture"})
        else:
            self.send_error(404)

    def do_GET(self):
        if not self.authorized():
            return
        if self.path == "/event":
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(
                b'data: {"id":"evt_1","type":"session.created","properties":{"sessionID":"ses_fixture"}}\n\n'
            )
            self.wfile.flush()
            threading.Event().wait(300)
        elif self.path == "/health":
            self.reply(200, {"ok": True})
        elif self.path.startswith("/session/"):
            self.reply(200, {"ok": True, "path": self.path})
        else:
            self.send_error(404)

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
print("listening on http://127.0.0.1:%d" % server.server_port, flush=True)
server.serve_forever()
"#;

    fn http_sse_profile() -> StructuredSessionProfile {
        serde_json::from_value(json!({
            "schema_version":ryeos_engine::structured_session_profile::STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION,
            "transport":"http_sse",
            "http_sse":{
                "username_env":"FX_HTTP_USER","password_env":"FX_HTTP_PASSWORD",
                "listener_stdout_prefix":"listening on http://127.0.0.1:",
                "readiness_path":"/health","readiness_schema":"health.json",
            "event_path":"/event","event_type_pointer":"/type","event_properties_pointer":"/properties",
            "ignored_notification_projection":"properties"
            },
            "configuration_authority":"immutable_argv",
            "workload_realization_id":"fixture-http",
            "workload_executable":"python3",
            "workload_args":["-c",HTTP_FIXTURE_SERVER],
            "workload_home_env":"FIXTURE_HOME",
            "required_process_environment":[],
            "workload_client":null,
            "baseline_config":"baseline.conf",
            "baseline_destination":"fixture.conf",
            "portable_state":null,
            "credential_subject":null,
            "initialization":[],
            "recovery":null,
            "route_sets":{"session":["session.read","session.start"]},
            "routes":[{
                "id":"session.start",
                "method":"session.start",
                "effect_class":"session_mutation",
                "http_method":"POST",
                "http_path":"/session",
                "http_body_schema":"empty.json",
                "http_path_parameters":{},
                "request_schema":"empty.json",
                "response_schema":"start.json",
                "fixed_params":{},
                "workspace_fields":[],
                "forbidden_non_null_fields":[],
                "response_predicates":[],
                "observations":[],
                "result_retention":"ephemeral",
                "ceremony":null,
                "session_binding":{
                    "action":"bind_new",
                    "request_field":null,
                    "response_pointer":"/result/session_id"
                }
            },{
                "id":"session.read",
                "method":"session.read",
                "audience":"runtime",
                "effect_class":"pure_read",
                "http_method":"GET",
                "http_path":"/session/{session_id}",
                "http_body_schema":"empty.json",
                "http_path_parameters":{"session_id":{"source":"bound_session"}},
                "request_schema":"empty.json",
                "response_schema":"read.json",
                "fixed_params":{},
                "workspace_fields":[],
                "forbidden_non_null_fields":[],
                "response_predicates":[],
                "observations":[],
                "result_retention":"ephemeral",
                "ceremony":null,
                "session_binding":{
                    "action":"require",
                    "request_field":"session_id",
                    "response_pointer":null
                }
            }],
            "notifications":[{
                "method":"session.created",
                "schema":"event.json",
                "upstream_session_pointer":null,
                "event_type":"session.created",
                "durable":true,
                "payload":{"op":"object","fields":{
                    "session_id":{"op":"pointer","pointer":"/message/params/sessionID"}
                }},
                "observations":[],
                "ceremony_clear":false
            }],
            "ignored_notifications":{},
            "server_requests":[]
        }))
        .unwrap()
    }

    #[test]
    fn http_sse_transport_binds_sessions_and_streams_events() {
        let root = tempfile::tempdir().unwrap();
        let workload_home = root.path().join("home");
        std::fs::create_dir(&workload_home).unwrap();
        let profile = http_sse_profile();
        let schemas = HashMap::from([
            ("empty.json".to_owned(), json!({"type":"object"})),
            (
                "start.json".to_owned(),
                json!({"type":"object","required":["session_id"],"properties":{
                    "session_id":{"type":"string"}
                },"additionalProperties":false}),
            ),
            (
                "read.json".to_owned(),
                json!({"type":"object","required":["ok","path"],"properties":{
                    "ok":{"const":true},"path":{"type":"string"}
                },"additionalProperties":false}),
            ),
            ("event.json".to_owned(), json!({"type":"object"})),
            (
                "health.json".to_owned(),
                json!({"type":"object","additionalProperties":true}),
            ),
        ]);
        let events = Arc::new(Mutex::new(EventQueue::default()));
        let (control_sender, controls) = sync_channel(1);
        let (result_sender, results) = sync_channel(1);
        let _results = results;
        let _control_keepalive = control_sender;
        let mut workload = StructuredWorkload::start(
            "/usr/bin/python3",
            root.path().to_str().unwrap(),
            workload_home.to_str().unwrap(),
            profile,
            "session".to_owned(),
            HashSet::from([
                RouteEffectClass::SessionMutation,
                RouteEffectClass::PureRead,
            ]),
            schemas,
            Arc::clone(&events),
            controls,
            result_sender,
            "python3",
            None,
            &BTreeMap::new(),
            Vec::new(),
        )
        .unwrap();
        let workspace = root.path().to_str().unwrap().to_owned();
        let started = workload
            .handle(json!({"route_id":"session.start","payload":{}}), &workspace)
            .unwrap();
        assert_eq!(started["response"]["result"]["session_id"], "ses_fixture");
        assert_eq!(workload.bound_session_id.as_deref(), Some("ses_fixture"));
        let deadline = MonotonicDeadline::after(Duration::from_secs(10));
        loop {
            workload.drain_incoming().unwrap();
            let observed = events.lock().unwrap().events.iter().any(|event| {
                event["event_type"] == "session.created"
                    && event["payload"]["session_id"] == "ses_fixture"
            });
            if observed {
                break;
            }
            assert!(
                !deadline.has_elapsed(),
                "structured workload event stream did not deliver the durable session event"
            );
            lillux::time::sleep(Duration::from_millis(50));
        }
        let read = workload
            .handle_control(json!({
                "kind":"runtime_route",
                "route_id":"session.read",
                "payload":{}
            }))
            .unwrap();
        assert_eq!(read["response"]["result"]["path"], "/session/ses_fixture");
    }

    #[test]
    fn approval_without_reviewable_command_is_deny_only() {
        let rule = ServerRequestRule {
            method: "approval".to_owned(),
            schema: "approval.json".to_owned(),
            operation_class: "command".to_owned(),
            correlation: ServerRequestCorrelation {
                upstream_session_pointer: "/message/session".to_owned(),
                operation_pointer: "/message/operation".to_owned(),
            },
            responses: ApprovalResponses {
                accept: ValueTemplate::Literal { value: Value::Null },
                cancel: ValueTemplate::Literal { value: Value::Null },
                decline: ValueTemplate::Literal { value: Value::Null },
                expire: ValueTemplate::Literal { value: Value::Null },
            },
            deny_only: false,
            permission_delta_fields: Vec::new(),
            reply_http_path: None,
            required_review_fields: vec!["/message/params/command".to_owned()],
            display: ValueTemplate::Literal { value: Value::Null },
        };
        assert!(!approval_accept_allowed(
            &rule,
            &json!({"message":{"params":{"command":null}}})
        ));
        assert!(approval_accept_allowed(
            &rule,
            &json!({"message":{"params":{"command":"cargo test"}}})
        ));
    }

    #[test]
    fn approval_expiry_is_inside_route_deadline_and_retains_exact_correlation() {
        assert!(ROUTE_CALL_TIMEOUT > APPROVAL_TTL);
        assert!(SERVER_REQUEST_REPLY_TIMEOUT < ROUTE_CALL_TIMEOUT);
        assert!(SERVER_REQUEST_REPLY_TIMEOUT < APPROVAL_TTL);
        let rule = ServerRequestRule {
            method: "approval/request".to_owned(),
            schema: "approval.json".to_owned(),
            operation_class: "command".to_owned(),
            correlation: ServerRequestCorrelation {
                upstream_session_pointer: "/message/params/session".to_owned(),
                operation_pointer: "/message/params/operation".to_owned(),
            },
            responses: ApprovalResponses {
                accept: ValueTemplate::Literal { value: Value::Null },
                cancel: ValueTemplate::Literal { value: Value::Null },
                decline: ValueTemplate::Literal { value: Value::Null },
                expire: ValueTemplate::Literal { value: Value::Null },
            },
            deny_only: false,
            permission_delta_fields: Vec::new(),
            reply_http_path: None,
            required_review_fields: Vec::new(),
            display: ValueTemplate::Literal { value: Value::Null },
        };
        let message = json!({
            "id":"approval-one",
            "method":"approval/request",
            "params":{"session":"session-one","operation":"operation-one"}
        });
        let event = approval_expired_event(&rule, &json!({"message":message.clone()})).unwrap();
        assert_eq!(event["event_type"], "approval.expired");
        assert_eq!(event["payload"]["request_id"], "approval-one");
        assert_eq!(event["payload"]["upstream_session_id"], "session-one");
        assert_eq!(event["payload"]["operation_id"], "operation-one");
        assert_eq!(
            event["payload"]["request_digest"],
            ryeos_state::objects::canonical_value_digest(&message).unwrap()
        );
    }

    #[test]
    fn workload_executable_comes_only_from_the_selected_sealed_realization() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("fixture-worker");
        std::fs::write(&executable, b"fixture").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o500)).unwrap();
        let sealed = serde_json::to_string(&json!([{
            "id":"fixture",
            "kind":"file",
            "mode":"pinned",
            "manifest_hash":"a".repeat(64),
            "entry_count":1,
            "total_bytes":7,
            "mount_root": "project",
            "mount":"fixture-worker"
        }]))
        .unwrap();
        let (descriptor, argv0, authorities) = resolve_pinned_executable(
            root.path(),
            &sealed,
            "fixture",
            std::path::Path::new("fixture-worker"),
        )
        .unwrap();
        assert!(descriptor.starts_with("/proc/self/fd"));
        assert!(argv0.starts_with("/proc/self/fd"));
        assert_eq!(argv0.file_name().unwrap(), "fixture-worker");
        assert_eq!(authorities.len(), 2);
        assert!(
            resolve_pinned_executable(
                root.path(),
                &sealed,
                "other",
                std::path::Path::new("fixture-worker")
            )
            .is_err()
        );
        assert!(
            resolve_pinned_executable(
                root.path(),
                &sealed,
                "fixture",
                std::path::Path::new("nested/fixture-worker")
            )
            .is_err(),
            "a file realization must not discard an executable-member prefix"
        );
    }

    #[test]
    fn tree_workload_executable_uses_a_canonical_descriptor_walk() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let runtime = project.join("runtime");
        std::fs::create_dir_all(runtime.join("bin")).unwrap();
        let executable = runtime.join("bin/program");
        std::fs::write(&executable, b"nested executable").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o500)).unwrap();
        symlink("program", runtime.join("bin/program-link")).unwrap();
        let sealed = pinned_content_fixture("runtime", "project").to_string();

        let (descriptor, argv0, authorities) = resolve_pinned_executable(
            &project,
            &sealed,
            "fixture",
            std::path::Path::new("bin/program"),
        )
        .unwrap();
        assert_eq!(std::fs::read(descriptor).unwrap(), b"nested executable");
        assert!(argv0.ends_with("runtime/bin/program"));
        assert_eq!(authorities.len(), 2);
        assert!(
            resolve_pinned_executable(
                &project,
                &sealed,
                "fixture",
                std::path::Path::new("program")
            )
            .is_err(),
            "tree lookup must not flatten a nested executable member"
        );
        assert!(
            resolve_pinned_executable(
                &project,
                &sealed,
                "fixture",
                std::path::Path::new("bin/program-link")
            )
            .is_err(),
            "tree lookup must not follow an executable symlink"
        );

        for invalid in [
            "/bin/program",
            "../program",
            "bin/../program",
            "bin//program",
            "bin/./program",
            "bin\\program",
        ] {
            assert!(
                resolve_pinned_executable(
                    &project,
                    &sealed,
                    "fixture",
                    std::path::Path::new(invalid)
                )
                .is_err(),
                "non-canonical executable member reached the descriptor walk: {invalid:?}"
            );
        }
    }

    fn pinned_content_fixture(mount: &str, mount_root: &str) -> Value {
        json!([{
            "id":"fixture", "kind":"tree", "mode":"pinned",
            "manifest_hash":"a".repeat(64), "entry_count":4, "total_bytes":16,
            "mount_root":mount_root, "mount":mount
        }])
    }

    #[test]
    fn pinned_content_paths_preserve_search_order_and_retain_resource_authority() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let mount = project.join("runtime");
        std::fs::create_dir_all(mount.join("bin")).unwrap();
        std::fs::create_dir(mount.join("helpers")).unwrap();
        std::fs::write(mount.join("worker"), b"worker").unwrap();
        std::fs::set_permissions(mount.join("worker"), std::fs::Permissions::from_mode(0o500))
            .unwrap();
        std::fs::write(mount.join("resource"), b"retained").unwrap();
        let sealed = pinned_content_fixture("runtime", "project").to_string();
        let (executable, argv0, workload_handles) =
            resolve_pinned_executable(&project, &sealed, "fixture", std::path::Path::new("worker"))
                .unwrap();
        let search = json!([
            {"realization_id":"fixture", "relative_directory":"helpers"},
            {"realization_id":"fixture", "relative_directory":"bin"}
        ])
        .to_string();
        let (path, search_handles) =
            resolve_pinned_executable_search(&project, &sealed, Some(&search)).unwrap();
        let bindings = descriptor_environment_fixture(json!({
            "FIXTURE_RESOURCE":{
                "kind":"realization_path", "realization_id":"fixture",
                "relative_path":"resource", "path_kind":"file"
            }
        }));
        let (environment, environment_handles) =
            resolve_session_process_environment(&project, &project, &sealed, Some(&bindings))
                .unwrap();
        // All returned paths remain tied to the original admitted handles,
        // including argv[0]'s adjacent resources, not a replacement pathname.
        std::fs::rename(&project, root.path().join("retained")).unwrap();
        std::fs::create_dir(&project).unwrap();
        assert_eq!(std::fs::read(executable).unwrap(), b"worker");
        assert_eq!(
            std::fs::read(argv0.parent().unwrap().join("resource")).unwrap(),
            b"retained"
        );
        assert_eq!(
            std::fs::read(&environment["FIXTURE_RESOURCE"]).unwrap(),
            b"retained"
        );
        let directories = path
            .as_ref()
            .unwrap()
            .split(':')
            .map(|path| std::fs::canonicalize(path).unwrap())
            .collect::<Vec<_>>();
        assert!(directories[0].ends_with("runtime/helpers"));
        assert!(directories[1].ends_with("runtime/bin"));
        assert_eq!(
            (
                workload_handles.len(),
                search_handles.len(),
                environment_handles.len()
            ),
            (2, 2, 1)
        );
    }

    #[test]
    fn pinned_content_paths_never_substitute_project_content_for_runtime_mounts() {
        let project = tempfile::tempdir().unwrap();
        // A unique mount avoids relying on whether the machine already has a
        // runtime root. This fixture deliberately exists only in the project.
        let mount = project.path().file_name().unwrap().to_str().unwrap();
        std::fs::create_dir(project.path().join(mount)).unwrap();
        std::fs::write(project.path().join(mount).join("worker"), b"worker").unwrap();
        std::fs::set_permissions(
            project.path().join(mount).join("worker"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        let sealed = pinned_content_fixture(mount, "execution_runtime").to_string();
        let search = json!([{"realization_id":"fixture", "relative_directory":"."}]).to_string();
        let bindings = descriptor_environment_fixture(json!({"FIXTURE_RESOURCE":{
            "kind":"realization_path", "realization_id":"fixture",
            "relative_path":"worker", "path_kind":"file"
        }}));
        assert!(
            resolve_pinned_executable(
                project.path(),
                &sealed,
                "fixture",
                std::path::Path::new("worker")
            )
            .is_err()
        );
        assert!(resolve_pinned_executable_search(project.path(), &sealed, Some(&search)).is_err());
        assert!(
            resolve_session_process_environment(
                project.path(),
                project.path(),
                &sealed,
                Some(&bindings)
            )
            .is_err()
        );
    }

    #[test]
    fn pinned_content_paths_refuse_unsafe_search_and_environment_endpoints() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join("runtime")).unwrap();
        std::fs::write(project.path().join("runtime/file"), b"file").unwrap();
        std::os::unix::fs::symlink("file", project.path().join("runtime/link")).unwrap();
        let sealed = pinned_content_fixture("runtime", "project");
        for relative in ["../runtime", "/runtime", "file", "link"] {
            let search =
                json!([{"realization_id":"fixture", "relative_directory":relative}]).to_string();
            assert!(
                resolve_pinned_executable_search(
                    project.path(),
                    &sealed.to_string(),
                    Some(&search)
                )
                .is_err(),
                "{relative}"
            );
        }
        let search = json!([{"realization_id":"fixture", "relative_directory":"."}]);
        let duplicates = json!([search[0], search[0]]).to_string();
        assert!(
            resolve_pinned_executable_search(
                project.path(),
                &sealed.to_string(),
                Some(&duplicates)
            )
            .is_err()
        );
        for (field, value) in [("kind", "file"), ("mode", "captured"), ("id", "absent")] {
            let mut changed = sealed.clone();
            changed[0][field] = json!(value);
            assert!(
                resolve_pinned_executable_search(
                    project.path(),
                    &changed.to_string(),
                    Some(&search.to_string())
                )
                .is_err()
            );
        }
        for (relative, kind) in [
            ("link", "file"),
            ("file", "directory"),
            (".", "file"),
            ("../file", "file"),
        ] {
            let bindings = descriptor_environment_fixture(json!({"FIXTURE_RESOURCE":{
                "kind":"realization_path", "realization_id":"fixture",
                "relative_path":relative, "path_kind":kind
            }}));
            assert!(
                resolve_session_process_environment(
                    project.path(),
                    project.path(),
                    &sealed.to_string(),
                    Some(&bindings)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn templates_copy_only_explicit_bounded_fields() {
        let template = ValueTemplate::Object {
            fields: BTreeMap::from([
                (
                    "id".to_owned(),
                    ValueTemplate::Pointer {
                        pointer: "/result/id".to_owned(),
                        optional: false,
                        max_string_bytes: 16,
                    },
                ),
                (
                    "kind".to_owned(),
                    ValueTemplate::Literal {
                        value: json!("fixture"),
                    },
                ),
            ]),
        };
        let output = evaluate_template(
            &template,
            &json!({"result":{"id":"one","secret":"not-copied"}}),
        )
        .unwrap();
        assert_eq!(output, json!({"id":"one","kind":"fixture"}));
        assert!(!output.to_string().contains("not-copied"));
    }

    #[test]
    fn compatibility_baseline_verification_preserves_the_daemon_prepared_file() {
        use std::os::unix::fs::MetadataExt as _;

        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("admitted.conf");
        let destination = root.path().join("runtime.conf");
        std::fs::write(&source, b"policy = \"fixed\"\n").unwrap();
        std::fs::write(&destination, b"policy = \"fixed\"\n").unwrap();
        // Source-overlay mode and private-seed mode are both valid. Checking
        // the inode also catches unnecessary atomic replacement without
        // requiring this unit test to create a kernel mount namespace.
        for mode in [0o644, 0o400] {
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(mode)).unwrap();
            let before = std::fs::metadata(&destination).unwrap();
            verify_compatibility_baseline_config(root.path(), &source, "runtime.conf").unwrap();
            let after = std::fs::metadata(&destination).unwrap();
            assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
            assert_eq!(after.permissions().mode() & 0o777, mode);
        }
    }

    #[test]
    fn compatibility_baseline_verification_refuses_missing_divergent_or_oversized_state() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("admitted.conf");
        let destination = root.path().join("runtime.conf");
        std::fs::write(&source, b"policy = \"fixed\"\n").unwrap();
        assert!(
            verify_compatibility_baseline_config(root.path(), &source, "runtime.conf").is_err()
        );
        assert!(!destination.exists());
        std::fs::write(&destination, b"workload = \"state\"\n").unwrap();
        assert!(
            verify_compatibility_baseline_config(root.path(), &source, "runtime.conf").is_err()
        );
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"workload = \"state\"\n"
        );
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&destination, vec![b'x'; 64 * 1024 + 1]).unwrap();
        assert!(
            verify_compatibility_baseline_config(root.path(), &source, "runtime.conf").is_err()
        );
        assert_eq!(
            std::fs::metadata(&destination).unwrap().len(),
            64 * 1024 + 1
        );
    }

    #[test]
    fn compatibility_baseline_rejects_a_link_without_touching_its_target() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("admitted.conf");
        let target = root.path().join("workload.conf");
        std::fs::write(&source, b"policy = \"fixed\"\n").unwrap();
        std::fs::write(&target, b"workload = \"state\"\n").unwrap();
        std::os::unix::fs::symlink("workload.conf", root.path().join("runtime.conf")).unwrap();

        assert!(
            verify_compatibility_baseline_config(root.path(), &source, "runtime.conf").is_err()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"workload = \"state\"\n");
    }

    #[test]
    fn structured_workload_creates_owner_only_files_and_directories() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("touch child-file && mkdir child-dir")
            .current_dir(root.path());
        // Prove the structured-session hook wins over a permissive mask
        // installed by an earlier child-only hook.
        unsafe {
            command.pre_exec(|| {
                libc::umask(0o000);
                Ok(())
            });
        }
        lillux::configure_owner_private_creation_mask(&mut command);
        assert!(command.status().unwrap().success());
        assert_eq!(
            std::fs::metadata(root.path().join("child-file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(root.path().join("child-dir"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn live_profile_home_protection_uses_the_pinned_root_as_the_boundary() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("child");
        std::fs::create_dir(&child).unwrap();
        let writable = child.join("state");
        let read_only = root.path().join("baseline");
        std::fs::write(&writable, b"state").unwrap();
        std::fs::write(&read_only, b"baseline").unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&child, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&read_only, std::fs::Permissions::from_mode(0o444)).unwrap();

        protect_profile_home(root.path()).unwrap();
        assert_eq!(
            std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&child).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::metadata(&writable).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(&read_only).unwrap().permissions().mode() & 0o777,
            0o444
        );
    }

    #[test]
    fn live_profile_home_protection_never_follows_links() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("home");
        let target = parent.path().join("target");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&target, b"outside").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("../target", root.join("link")).unwrap();

        protect_profile_home(&root).unwrap();
        assert!(
            std::fs::symlink_metadata(root.join("link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn control_descriptor_is_close_on_exec() {
        use std::os::fd::AsRawFd as _;
        let (left, _right) = UnixStream::pair().unwrap();
        lillux::protect_descriptor_from_exec(&left).unwrap();
        // SAFETY: F_GETFD only observes the borrowed descriptor.
        let flags = unsafe { libc::fcntl(left.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
    }

    #[test]
    fn private_stderr_drain_continues_beyond_retention_limits() {
        struct CountingReader {
            remaining: usize,
            consumed: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl Read for CountingReader {
            fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
                let count = output.len().min(self.remaining);
                output[..count].fill(b'x');
                self.remaining -= count;
                self.consumed
                    .fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                Ok(count)
            }
        }
        let expected = 10 * 1024 * 1024;
        let consumed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        drain_private_stderr(CountingReader {
            remaining: expected,
            consumed: Arc::clone(&consumed),
        });
        assert_eq!(
            consumed.load(std::sync::atomic::Ordering::Relaxed),
            expected
        );
    }

    #[test]
    fn session_binding_rejects_cross_session_and_unbound_routes() {
        let require = SessionBindingRule {
            action: SessionBindingAction::Require,
            request_field: Some("sessionKey".to_owned()),
            response_pointer: None,
        };
        assert!(prepare_session_binding(Some(&require), &None, &mut json!({}), true).is_err());
        let bound = Some("session-one".to_owned());
        assert!(
            prepare_session_binding(
                Some(&require),
                &bound,
                &mut json!({"sessionKey":"session-two"}),
                true,
            )
            .is_err()
        );
        let mut params = json!({});
        assert_eq!(
            prepare_session_binding(Some(&require), &bound, &mut params, true).unwrap(),
            Some("session-one".to_owned())
        );
        assert_eq!(params, json!({"sessionKey":"session-one"}));
    }

    #[test]
    fn recovery_binding_requires_the_exact_returned_session() {
        let recovery = SessionBindingRule {
            action: SessionBindingAction::BindExpected,
            request_field: Some("sessionKey".to_owned()),
            response_pointer: Some("/result/session/key".to_owned()),
        };
        let mut params = json!({"sessionKey":"session-one"});
        let expected = prepare_session_binding(Some(&recovery), &None, &mut params, true).unwrap();
        let mut bound = None;
        assert!(
            settle_session_binding(
                Some(&recovery),
                expected.as_deref(),
                &json!({"result":{"session":{"key":"session-two"}}}),
                &mut bound,
            )
            .is_err()
        );
        settle_session_binding(
            Some(&recovery),
            expected.as_deref(),
            &json!({"result":{"session":{"key":"session-one"}}}),
            &mut bound,
        )
        .unwrap();
        assert_eq!(bound.as_deref(), Some("session-one"));
    }

    #[test]
    fn session_process_environment_materializes_only_the_owned_runtime_view() {
        let root = tempfile::tempdir().unwrap();
        let bindings = BTreeMap::from([
            (
                "CARGO_HOME".to_owned(),
                ryeos_state::objects::SessionProcessEnvironmentValue::RuntimeViewDirectory {
                    relative_path: "cargo/home".to_owned(),
                },
            ),
            (
                "CARGO_NET_OFFLINE".to_owned(),
                ryeos_state::objects::SessionProcessEnvironmentValue::Literal {
                    value: "true".to_owned(),
                },
            ),
        ]);
        let encoded = descriptor_environment_fixture(serde_json::to_value(bindings).unwrap());
        let (resolved, handles) =
            resolve_session_process_environment(root.path(), root.path(), "[]", Some(&encoded))
                .unwrap();
        assert_eq!(
            resolved.get("CARGO_NET_OFFLINE").map(String::as_str),
            Some("true")
        );
        assert!(
            resolved
                .get("CARGO_HOME")
                .is_some_and(|path| path.starts_with("/proc/self/fd/"))
        );
        assert_eq!(handles.len(), 1);
        assert!(
            root.path()
                .join(".ai/cache/ryeos-runtime/cargo/home")
                .is_dir()
        );
    }

    fn descriptor_environment_fixture(bindings: Value) -> String {
        json!({"bindings": bindings, "runtime_view_delivery": {"kind":"descriptor_workspace"}})
            .to_string()
    }

    #[test]
    fn mounted_runtime_view_delivery_never_creates_a_missing_prepared_source() {
        let root = tempfile::tempdir().unwrap();
        let encoded = json!({
            "bindings":{"CARGO_HOME":{"kind":"runtime_view_directory","relative_path":"cargo/home"}},
            "runtime_view_delivery":{"kind":"mounted_namespace","destinations":{"CARGO_HOME":"/ryeos/runtime-views/CARGO_HOME"}}
        }).to_string();
        let error =
            resolve_session_process_environment(root.path(), root.path(), "[]", Some(&encoded))
                .unwrap_err();
        assert!(
            error.to_string().contains("prepared runtime-view"),
            "{error:#}"
        );
        assert!(!root.path().join(".ai").exists());
        assert!(
            resolve_session_process_environment(root.path(), root.path(), "[]", Some("{}"))
                .is_err(),
            "raw authored bindings are not prepared launch delivery"
        );
    }
}
