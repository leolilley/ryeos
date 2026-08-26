use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const WIRE_PROTOCOL: &str = "ryeos.structured-session";
const WIRE_VERSION: u32 = 1;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_APP_SERVER_LINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENTS: usize = 4_096;
const MAX_PENDING_SERVER_REQUESTS: usize = 128;
const APPROVAL_TTL: Duration = Duration::from_secs(15 * 60);
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
    input: ChildStdin,
    incoming: Receiver<Result<Value, String>>,
    responses: HashMap<String, Value>,
    server_requests: HashMap<String, PendingServerRequest>,
    expired_server_requests: VecDeque<ExpiredServerRequest>,
    seen_server_request_ids: HashSet<String>,
    events: Arc<Mutex<EventQueue>>,
    pending_controls: Receiver<PendingControl>,
    control_results: SyncSender<WorkloadCommandResult>,
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
}

struct PendingControl {
    request_id: String,
    body: Value,
}

struct PendingServerRequest {
    message: Value,
    request_digest: String,
    expires_at: Instant,
}

struct ExpiredServerRequest {
    id: String,
    request_digest: String,
}

struct PendingObservationBatch {
    through_sequence: u64,
    digest: String,
    deadline: Instant,
}

type WorkloadCommandResult = (String, std::result::Result<Value, String>);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredSessionProfile {
    schema_version: u32,
    configuration_authority: ConfigurationAuthority,
    workload_realization_id: String,
    workload_executable: String,
    workload_args: Vec<String>,
    workload_home_env: String,
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

/// Mechanical source of workload configuration authority. Immutable argv is
/// retained in the signed profile, verified by its admitted digest, and
/// supplied by the bridge itself; the same-UID workload cannot rewrite it.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ConfigurationAuthority {
    ImmutableArgv,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalRealization {
    id: String,
    kind: String,
    mode: String,
    manifest_hash: String,
    entry_count: usize,
    total_bytes: u64,
    mount: String,
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
    if profile.configuration_authority != ConfigurationAuthority::ImmutableArgv {
        bail!("structured-session configuration authority is not immutable argv");
    }
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
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("inspect admitted schema `{identity}`"))?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_file()
            || metadata.len() > 8 * 1024 * 1024
        {
            bail!("structured-session schema is not a bounded regular file");
        }
        total = total
            .checked_add(usize::try_from(metadata.len())?)
            .ok_or_else(|| anyhow!("structured-session schema bytes overflow"))?;
        if total > 16 * 1024 * 1024 {
            bail!("structured-session schemas exceed their aggregate byte ceiling");
        }
        let bytes = std::fs::read(&path)?;
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
    disable_core_dumps()?;
    let fd = required_fd("RYEOS_SESSION_FD")?;
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
    let profile_bytes = std::fs::read(&profile_path).context("read structured-session profile")?;
    if profile_bytes.is_empty() || profile_bytes.len() > 64 * 1024 {
        bail!("structured-session profile is empty or exceeds its bound");
    }
    let profile: StructuredSessionProfile =
        serde_json::from_slice(&profile_bytes).context("decode structured-session profile")?;
    if profile.schema_version != 1 {
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
    if executable_name.components().count() != 1
        || !matches!(
            executable_name.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        bail!("structured-session workload executable must be one relative file name");
    }
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
    let executable = resolve_pinned_executable(
        std::path::Path::new(&external_root),
        &external_realizations,
        &profile.workload_realization_id,
        executable_name,
    )?;
    reset_compatibility_baseline_config(
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
    // SAFETY: the signed protocol gives this process unique ownership of the
    // inherited descriptor. No other safe Rust owner is constructed.
    let mut channel = unsafe { UnixStream::from_raw_fd(fd) };
    set_close_on_exec(&channel)?;
    let events = Arc::new(Mutex::new(EventQueue::default()));
    let (workload_result_sender, workload_results) = sync_channel::<WorkloadCommandResult>(32);
    let (pending_control_sender, pending_controls) = sync_channel::<PendingControl>(32);
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
    )?;
    app.initialize()?;
    protect_profile_home(std::path::Path::new(&workload_home))?;
    let workload_pid = app.child.id();
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
    set_close_on_exec(&reader)?;
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
    loop {
        while let Ok((request_id, outcome)) = workload_results.try_recv() {
            if cancelled_requests.remove(&request_id) {
                continue;
            }
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
                    deadline: Instant::now() + Duration::from_secs(30),
                });
            }
        }
        if pending_observation
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.deadline)
        {
            bail!("RyeOS did not durably acknowledge the observation batch");
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
                                let result = if control {
                                    workload.handle_control(body)
                                } else {
                                    workload.handle(body, &workspace)
                                };
                                result.map_err(|error| error.to_string())
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
                // SAFETY: `workload_pid` is the child spawned and still owned
                // by `StructuredWorkload`; SIGTERM cannot target another pid
                // until that owned child has been reaped.
                if unsafe { libc::kill(workload_pid as libc::pid_t, libc::SIGTERM) } != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ESRCH) {
                        return Err(error)
                            .context("terminate cancelled structured-session workload");
                    }
                }
                active_request = None;
                cancelled_requests.insert(request_id.to_owned());
                for control_id in active_controls.drain() {
                    cancelled_requests.insert(control_id.clone());
                    write_error(&mut channel, &control_id, "request cancelled")?;
                }
                cancelled_workload = true;
                write_error(&mut channel, request_id, "request cancelled")?;
            }
            FrameKind::ObservationAck => {
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

fn disable_core_dumps() -> Result<()> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setrlimit only changes this worker process. The zero limit is
    // inherited by the pinned structured workload and every descendant it launches.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        return Err(std::io::Error::last_os_error()).context("disable worker core dumps");
    }
    Ok(())
}

/// Atomically reset the workload's compatibility seed before each process
/// generation. This file is deliberately not a same-UID authority boundary:
/// the signed immutable argv is the sole security configuration authority,
/// and an enforced generic isolation backend may additionally overlay the
/// seed read-only. Workload-authored compatible state is discarded at the
/// next launch rather than mistaken for admitted policy.
fn reset_compatibility_baseline_config(
    workload_home: &std::path::Path,
    source: &std::path::Path,
    destination_name: &str,
) -> Result<()> {
    let admitted =
        std::fs::read(source).context("read admitted structured-session baseline config")?;
    if admitted.is_empty() || admitted.len() > 64 * 1024 {
        bail!("admitted structured-session baseline config is empty or exceeds its bound");
    }
    let home = lillux::PinnedDirectory::open(workload_home)?
        .ok_or_else(|| anyhow!("profile workload home is missing"))?;
    let destination_name = std::ffi::OsStr::new(destination_name);
    let incumbent = home
        .open_regular(destination_name, false)
        .context("open compatibility seed through Lillux")?;
    home.atomic_write_if_same(destination_name, incumbent.as_ref(), &admitted, 0o400)
        .context("reset compatibility seed through Lillux")
}

fn resolve_pinned_executable(
    external_root: &std::path::Path,
    sealed_realizations: &str,
    realization_id: &str,
    executable_name: &std::path::Path,
) -> Result<std::path::PathBuf> {
    if realization_id.is_empty()
        || realization_id.len() > 128
        || !realization_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("structured-session workload realization id is not canonical");
    }
    let realizations: Vec<ExternalRealization> =
        serde_json::from_str(sealed_realizations).context("decode sealed external realizations")?;
    let mut matches = realizations
        .iter()
        .filter(|realization| realization.id == realization_id);
    let realization = matches
        .next()
        .ok_or_else(|| anyhow!("workload realization is not present in the admitted set"))?;
    if matches.next().is_some() {
        bail!("workload realization id is ambiguous");
    }
    if realization.mode != "pinned"
        || realization.manifest_hash.len() != 64
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
    let path = match realization.kind.as_str() {
        "file" => {
            if mount.file_name() != executable_name.file_name() {
                bail!("file realization mount does not match the workload executable");
            }
            external_root.join(mount)
        }
        "tree" => external_root.join(mount).join(executable_name),
        _ => bail!("workload realization kind cannot contain an executable"),
    };
    let metadata = std::fs::symlink_metadata(&path)
        .context("inspect pinned structured-session workload executable")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("pinned structured-session workload executable is not an exact regular file");
    }
    let path = path
        .canonicalize()
        .context("canonicalize pinned structured-session workload executable")?;
    require_absolute_normalized(
        "pinned structured-session workload executable",
        path.to_str().ok_or_else(|| {
            anyhow!("pinned structured-session workload executable path is not UTF-8")
        })?,
    )?;
    Ok(path)
}

fn protect_profile_home(root: &std::path::Path) -> Result<()> {
    let home = lillux::PinnedDirectory::open(root)?
        .ok_or_else(|| anyhow!("profile workload home is missing"))?;
    home.tighten_owner_private_directory()
        .context("protect live profile-home root through Lillux")
}

fn set_close_on_exec(stream: &UnixStream) -> Result<()> {
    use std::os::fd::AsRawFd as _;
    let fd = stream.as_raw_fd();
    // SAFETY: fcntl only reads/updates flags on the live descriptor borrowed
    // from `stream`; ownership and lifetime remain with Rust.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error()).context("read session descriptor flags");
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error()).context("fence session descriptor from exec");
    }
    Ok(())
}

fn configure_private_creation_mask(command: &mut Command) {
    // The structured workload owns opaque credential/state bytes under its
    // node-private home. Install the creation mask only in the forked child:
    // changing the daemon/bridge process-wide umask would race unrelated
    // threads. The workload and all descendants inherit this owner-only mask.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o077);
            Ok(())
        });
    }
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
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command
            .args(&profile.workload_args)
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear()
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env(&profile.workload_home_env, workload_home)
            .env("HOME", workload_home);
        configure_private_creation_mask(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("start pinned structured-session workload `{executable}`"))?;
        let input = child
            .stdin
            .take()
            .context("capture structured workload stdin")?;
        let output = child
            .stdout
            .take()
            .context("capture structured workload stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("capture structured workload stderr")?;
        let (sender, incoming) = sync_channel(1024);
        thread::Builder::new()
            .name("ryeos-structured-session-workload-reader".to_owned())
            .spawn(move || read_app_server(output, sender))
            .context("start bounded structured workload reader")?;
        thread::Builder::new()
            .name("ryeos-structured-session-stderr-drain".to_owned())
            .spawn(move || drain_private_stderr(stderr))
            .context("start bounded structured-session stderr drain")?;
        Ok(Self {
            child,
            input,
            incoming,
            responses: HashMap::new(),
            server_requests: HashMap::new(),
            expired_server_requests: VecDeque::new(),
            seen_server_request_ids: HashSet::new(),
            events,
            pending_controls,
            control_results,
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
                self.send(&json!({"method":notification,"params":step.params}))?;
                continue;
            }
            let result = self.call_raw(&step.method, step.params, Duration::from_secs(30))?;
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
        let schema = self
            .schemas
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
        )?;
        apply_route_parameters(&route, &mut params, workspace)?;
        self.validate_schema(&route.request_schema, &params)?;
        let observed_params = params.clone();
        let response = self.call_raw(method, params, ROUTE_CALL_TIMEOUT)?;
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
            .ok_or_else(|| anyhow!("pending server request has no admitted rule"))?;
        let context = json!({"message":pending});
        if decision == "accept" && !approval_accept_allowed(rule, &context) {
            bail!("approval with a filesystem, network, or policy delta cannot be accepted");
        }
        let response_template = match decision {
            "accept" => &rule.responses.accept,
            "decline" => &rule.responses.decline,
            "cancel" => &rule.responses.cancel,
            _ => unreachable!("decision vocabulary checked above"),
        };
        let response = evaluate_template(response_template, &context)?;
        self.send(&json!({"id":request_id,"result":response}))?;
        Ok(json!({"resolved":true}))
    }

    fn expire_server_requests(&mut self) -> Result<()> {
        let now = Instant::now();
        let expired = self
            .server_requests
            .iter()
            .filter(|(_, pending)| pending.expires_at <= now)
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
                .ok_or_else(|| anyhow!("expired server request has no admitted rule"))?;
            let context = json!({"message":pending});
            let expiry_event = approval_expired_event(rule, &context)?;
            let response = evaluate_template(&rule.responses.expire, &context)?;
            self.send(&json!({"id":request_id,"result":response}))?;
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
        let deadline = Instant::now() + timeout;
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
            let remaining = deadline.saturating_duration_since(Instant::now());
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
                .map_err(|error| error.to_string());
            self.control_results
                .send((control.request_id, outcome))
                .map_err(|_| anyhow!("structured-session control result receiver disconnected"))?;
        }
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.input, message)
            .context("encode structured workload message")?;
        self.input
            .write_all(b"\n")
            .context("frame structured workload message")?;
        self.input
            .flush()
            .context("flush structured workload message")
    }

    fn receive_one(&mut self, timeout: Duration) -> Result<()> {
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
                            expires_at: Instant::now() + APPROVAL_TTL,
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
                    self.validate_schema(&schema, message.get("params").unwrap_or(&Value::Null))?;
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
                let payload = evaluate_template(&rule.payload, &context)?;
                let observations = evaluate_observations(&rule.observations, &context)?;
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

    fn push_event(&mut self, event: Value) -> Result<()> {
        self.events
            .lock()
            .map_err(|_| anyhow!("structured-session event queue is poisoned"))?
            .push(event)
    }
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
            object.insert(field.to_owned(), Value::String(bound.to_owned()));
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

fn required_fd(name: &str) -> Result<RawFd> {
    let fd = required_env(name)?
        .parse::<RawFd>()
        .with_context(|| format!("parse inherited descriptor {name}"))?;
    if fd < 3 {
        bail!("inherited descriptor {name} is not canonical");
    }
    Ok(fd)
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("missing environment {name}"))?;
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        bail!("environment {name} is not canonical and bounded");
    }
    Ok(value)
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
        FrameKind::ObservationAck if frame.request_id.is_none() && frame.body.is_some() => Ok(()),
        _ => bail!("daemon sent an invalid frame shape"),
    }
}

fn read_session_frames(
    mut stream: UnixStream,
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

fn read_frame(stream: &mut UnixStream) -> Result<Frame> {
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

fn write_frame(stream: &mut UnixStream, frame: &Frame) -> Result<()> {
    let encoded = serde_json::to_vec(frame).context("encode RyeOS frame")?;
    if encoded.is_empty() || encoded.len() > MAX_FRAME_BYTES {
        bail!("RyeOS output frame exceeds bound");
    }
    stream.write_all(&(encoded.len() as u32).to_be_bytes())?;
    stream.write_all(&encoded)?;
    stream.flush().context("flush RyeOS frame")
}

fn write_final(stream: &mut UnixStream, request_id: &str, body: Value) -> Result<()> {
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

fn write_error(stream: &mut UnixStream, request_id: &str, message: &str) -> Result<()> {
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

    #[test]
    fn in_flight_upstream_call_services_a_gating_approval() {
        let root = tempfile::tempdir().unwrap();
        let workload_home = root.path().join("home");
        std::fs::create_dir(&workload_home).unwrap();
        let profile: StructuredSessionProfile = serde_json::from_value(json!({
            "schema_version":1,
            "configuration_authority":"immutable_argv",
            "workload_realization_id":"test-realization",
            "workload_executable":"sh",
            "workload_args":[
                "-c",
                "IFS= read -r request; printf '%s\\n' '{\"id\":\"approval-one\",\"method\":\"approval/request\",\"params\":{\"session\":\"session-one\",\"operation\":\"operation-one\",\"command\":\"true\"}}'; IFS= read -r decision; printf '%s\\n' '{\"id\":1,\"result\":{\"ok\":true}}'"
            ],
            "workload_home_env":"TEST_WORKLOAD_HOME",
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
        .unwrap();
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
            let deadline = Instant::now() + Duration::from_secs(2);
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
                assert!(Instant::now() < deadline, "approval event was not surfaced");
                thread::sleep(Duration::from_millis(1));
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
        assert_eq!(control_result.unwrap(), json!({"resolved":true}));

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
                expires_at: Instant::now() + APPROVAL_TTL,
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
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("fixture-worker");
        std::fs::write(&executable, b"fixture").unwrap();
        let sealed = serde_json::to_string(&json!([{
            "id":"fixture",
            "kind":"file",
            "mode":"pinned",
            "manifest_hash":"a".repeat(64),
            "entry_count":1,
            "total_bytes":7,
            "mount":"fixture-worker"
        }]))
        .unwrap();
        assert_eq!(
            resolve_pinned_executable(
                root.path(),
                &sealed,
                "fixture",
                std::path::Path::new("fixture-worker")
            )
            .unwrap(),
            executable.canonicalize().unwrap()
        );
        assert!(
            resolve_pinned_executable(
                root.path(),
                &sealed,
                "other",
                std::path::Path::new("fixture-worker")
            )
            .is_err()
        );
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
    fn compatibility_baseline_is_owner_only_and_resets_workload_state() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("admitted.conf");
        std::fs::write(&source, b"policy = \"fixed\"\n").unwrap();
        reset_compatibility_baseline_config(root.path(), &source, "runtime.conf").unwrap();
        assert_eq!(
            std::fs::metadata(root.path().join("runtime.conf"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
        std::fs::set_permissions(
            root.path().join("runtime.conf"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        std::fs::write(root.path().join("runtime.conf"), b"workload = \"state\"\n").unwrap();
        reset_compatibility_baseline_config(root.path(), &source, "runtime.conf").unwrap();
        assert_eq!(
            std::fs::read(root.path().join("runtime.conf")).unwrap(),
            b"policy = \"fixed\"\n"
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

        assert!(reset_compatibility_baseline_config(root.path(), &source, "runtime.conf").is_err());
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
        configure_private_creation_mask(&mut command);
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
        set_close_on_exec(&left).unwrap();
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
        assert!(prepare_session_binding(Some(&require), &None, &mut json!({})).is_err());
        let bound = Some("session-one".to_owned());
        assert!(
            prepare_session_binding(
                Some(&require),
                &bound,
                &mut json!({"sessionKey":"session-two"}),
            )
            .is_err()
        );
        let mut params = json!({});
        assert_eq!(
            prepare_session_binding(Some(&require), &bound, &mut params).unwrap(),
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
        let expected = prepare_session_binding(Some(&recovery), &None, &mut params).unwrap();
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
}
