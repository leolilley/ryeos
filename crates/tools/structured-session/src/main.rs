use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::UnixStream;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
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
// The durable ledger starts its 15-minute clock after this request is emitted.
// The small skew keeps the adapter-side decline from racing just ahead of the
// daemon's wall-clock expiry CAS.
const APPROVAL_TTL: Duration = Duration::from_secs(15 * 60 + 5);
const MAX_PROFILE_HOME_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_PROFILE_HOME_ENTRIES: usize = 100_000;

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
    events: VecDeque<Value>,
    next_id: u64,
    fatal: Option<String>,
    workspace: String,
    workload_home: String,
    active_login_id: Option<String>,
    bound_session_id: Option<String>,
    admitted_baseline_config: Vec<u8>,
    profile: StructuredSessionProfile,
    route_set: String,
    allowed_effect_classes: HashSet<RouteEffectClass>,
    schemas: HashMap<String, serde_json::Value>,
    outstanding: HashSet<String>,
    response_bytes: usize,
    event_bytes: usize,
}

struct PendingServerRequest {
    message: Value,
    expires_at: Instant,
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
    workload_realization_id: String,
    workload_executable: String,
    workload_args: Vec<String>,
    workload_home_env: String,
    baseline_config: String,
    baseline_destination: String,
    initialization: Vec<InitializationStep>,
    route_sets: BTreeMap<String, Vec<String>>,
    routes: Vec<RouteRule>,
    notifications: Vec<NotificationRule>,
    #[serde(default)]
    ignored_notifications: BTreeMap<String, String>,
    server_requests: Vec<ServerRequestRule>,
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
    response_predicates: Vec<ValuePredicate>,
    #[serde(default)]
    observations: Vec<ObservationRule>,
    result_retention: ResultRetention,
    #[serde(default)]
    ceremony: Option<CeremonyAction>,
    #[serde(default)]
    session_binding: Option<SessionBindingRule>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionBindingRule {
    action: SessionBindingAction,
    request_field: Option<String>,
    response_pointer: Option<String>,
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
    response_style: ApprovalResponseStyle,
    #[serde(default)]
    deny_only: bool,
    #[serde(default)]
    permission_delta_fields: Vec<String>,
    #[serde(default)]
    required_review_fields: Vec<String>,
    display: ValueTemplate,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ApprovalResponseStyle {
    Decision,
    PermissionsDenial,
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
    file_name(
        "structured-session baseline destination",
        &profile.baseline_destination,
    )?;
    if profile.workload_args.len() > 16
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
            || route.observations.len() > 16
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
    if let Ok(expected) = std::env::var("RYEOS_STRUCTURED_SESSION_PROFILE_HASH")
        && expected != profile_digest
    {
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
    install_or_verify_baseline_config(
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
    let admitted_baseline_config = std::fs::read(&baseline_config)
        .context("retain admitted structured-session baseline config")?;
    // SAFETY: the signed protocol gives this process unique ownership of the
    // inherited descriptor. No other safe Rust owner is constructed.
    let mut channel = unsafe { UnixStream::from_raw_fd(fd) };
    set_close_on_exec(&channel)?;
    let mut app = StructuredWorkload::start(
        executable.to_str().ok_or_else(|| {
            anyhow!("pinned structured-session workload executable path is not UTF-8")
        })?,
        &workspace,
        &workload_home,
        admitted_baseline_config,
        profile,
        route_set,
        allowed_effect_classes,
        schemas,
    )?;
    app.initialize()?;
    let workload_pid = app.child.id();
    let app = Arc::new(Mutex::new(app));
    let (workload_result_sender, workload_results) = sync_channel::<WorkloadCommandResult>(32);
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
    let mut cancelled_requests = HashSet::new();
    let mut cancelled_workload = false;
    loop {
        while let Ok((request_id, outcome)) = workload_results.try_recv() {
            if cancelled_requests.remove(&request_id) {
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
        let event_batch = match app.try_lock() {
            Ok(mut app) => {
                app.drain_incoming()?;
                app.expire_server_requests()?;
                if pending_observation.is_none() {
                    app.take_event_batch(128)?
                } else {
                    None
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                bail!("structured-session workload state is poisoned")
            }
        };
        if pending_observation.is_none() {
            if let Some((events, observations)) = event_batch {
                let count = u64::try_from(events.len())?;
                let through_sequence = next_observation_sequence
                    .checked_add(count - 1)
                    .ok_or_else(|| anyhow!("observation sequence overflow"))?;
                let mut body = json!({
                    "first_sequence": next_observation_sequence,
                    "count": count,
                    "previous_digest": previous_observation_digest,
                    "events": events,
                    "session_observations": observations,
                });
                let digest = ryeos_state::objects::canonical_value_digest(&body)?;
                body.as_object_mut()
                    .expect("observation batch is an object")
                    .insert("batch_digest".to_owned(), Value::String(digest.clone()));
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
                if active_request.is_some() {
                    bail!("daemon contacted more than one structured-session request at once");
                }
                active_request = Some(request_id.to_owned());
                let request_id = request_id.to_owned();
                let workload = Arc::clone(&app);
                let sender = workload_result_sender.clone();
                let workspace = workspace.clone();
                let control = matches!(frame.kind, FrameKind::Control);
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

fn install_or_verify_baseline_config(
    workload_home: &std::path::Path,
    source: &std::path::Path,
    destination_name: &str,
) -> Result<()> {
    let admitted =
        std::fs::read(source).context("read admitted structured-session baseline config")?;
    if admitted.is_empty() || admitted.len() > 64 * 1024 {
        bail!("admitted structured-session baseline config is empty or exceeds its bound");
    }
    let destination = workload_home.join(destination_name);
    if !destination.exists() {
        let temporary = workload_home.join(".ryeos-baseline.pending");
        if temporary.exists() {
            let metadata = std::fs::symlink_metadata(&temporary)
                .context("inspect incomplete baseline config")?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                bail!("incomplete baseline config is not a regular file");
            }
            std::fs::remove_file(&temporary).context("remove incomplete baseline config")?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true).mode(0o400);
        let mut file = options
            .open(&temporary)
            .context("create incomplete baseline config")?;
        file.write_all(&admitted)
            .context("write admitted baseline config")?;
        file.sync_all().context("sync admitted baseline config")?;
        std::fs::rename(&temporary, &destination).context("publish admitted baseline config")?;
        let directory =
            std::fs::File::open(workload_home).context("open workload home for sync")?;
        directory.sync_all().context("sync workload home")?;
    }
    let metadata =
        std::fs::symlink_metadata(&destination).context("inspect profile baseline config")?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("profile workload home/config.toml is not a regular file");
    }
    if metadata.permissions().mode() & 0o777 != 0o400 {
        bail!("profile workload home/config.toml is not owner-read-only");
    }
    let actual =
        std::fs::read(&destination).context("read profile structured-session baseline config")?;
    if actual != admitted {
        bail!("profile workload home/config.toml differs from the admitted baseline");
    }
    Ok(())
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

fn require_profile_home_within_limit(root: &std::path::Path) -> Result<u64> {
    fn visit(path: &std::path::Path, entries: &mut usize, bytes: &mut u64) -> Result<()> {
        for entry in std::fs::read_dir(path).context("enumerate profile home")? {
            let entry = entry.context("read profile-home entry")?;
            *entries = entries
                .checked_add(1)
                .ok_or_else(|| anyhow!("profile-home entry count overflow"))?;
            if *entries > MAX_PROFILE_HOME_ENTRIES {
                bail!("profile workload home reached its entry ceiling");
            }
            let metadata = std::fs::symlink_metadata(entry.path())
                .context("inspect profile-home entry without following links")?;
            if metadata.file_type().is_symlink() || !metadata.is_file() && !metadata.is_dir() {
                bail!("profile workload home contains a link or special entry");
            }
            if metadata.permissions().mode() & 0o077 != 0 {
                bail!("profile workload home entry grants group or other permissions");
            }
            if metadata.is_dir() {
                visit(&entry.path(), entries, bytes)?;
            } else {
                *bytes = bytes
                    .checked_add(metadata.len())
                    .ok_or_else(|| anyhow!("profile-home byte count overflow"))?;
                if *bytes > MAX_PROFILE_HOME_BYTES {
                    bail!("profile workload home reached its byte ceiling");
                }
            }
        }
        Ok(())
    }

    let metadata = std::fs::symlink_metadata(root).context("inspect profile workload home")?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("profile workload home is not a directory");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("profile workload home grants group or other permissions");
    }
    let mut entries = 0usize;
    let mut bytes = 0u64;
    visit(root, &mut entries, &mut bytes)?;
    Ok(bytes)
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

impl StructuredWorkload {
    fn start(
        executable: &str,
        workspace: &str,
        workload_home: &str,
        admitted_baseline_config: Vec<u8>,
        profile: StructuredSessionProfile,
        route_set: String,
        allowed_effect_classes: HashSet<RouteEffectClass>,
        schemas: HashMap<String, Value>,
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
            .spawn(move || drain_bounded_stderr(stderr))
            .context("start bounded structured-session stderr drain")?;
        Ok(Self {
            child,
            input,
            incoming,
            responses: HashMap::new(),
            server_requests: HashMap::new(),
            events: VecDeque::new(),
            next_id: 1,
            fatal: None,
            workspace: workspace.to_owned(),
            workload_home: workload_home.to_owned(),
            active_login_id: None,
            bound_session_id: None,
            admitted_baseline_config,
            profile,
            route_set,
            allowed_effect_classes,
            schemas,
            outstanding: HashSet::new(),
            response_bytes: 0,
            event_bytes: 0,
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
        self.verify_baseline_config()?;
        self.drain_incoming()?;
        self.expire_server_requests()?;
        if let Some(reason) = self.fatal.as_deref() {
            bail!("structured-session workload protocol is quarantined: {reason}");
        }
        let object = body
            .as_object()
            .ok_or_else(|| anyhow!("adapter request body must be an object"))?;
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
        self.verify_baseline_config()?;
        result
    }

    fn handle_control(&mut self, body: Value) -> Result<Value> {
        self.verify_baseline_config()?;
        let control = body
            .as_object()
            .ok_or_else(|| anyhow!("RyeOS session control must be an object"))?;
        let result = match control.get("kind").and_then(Value::as_str) {
            Some("approval_decision") => {
                require_exact_keys(
                    control,
                    &["kind", "request_id", "decision", "reservation_token"],
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
            _ => bail!("unsupported RyeOS session control"),
        };
        self.verify_baseline_config()?;
        result
    }

    fn verify_baseline_config(&self) -> Result<()> {
        let destination =
            std::path::Path::new(&self.workload_home).join(&self.profile.baseline_destination);
        let metadata = std::fs::symlink_metadata(&destination)
            .context("inspect retained structured-session baseline")?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_file()
            || metadata.permissions().mode() & 0o777 != 0o400
            || std::fs::read(&destination)? != self.admitted_baseline_config
        {
            bail!("structured-session baseline config changed after admission");
        }
        Ok(())
    }

    fn validate_schema(&self, identity: &str, value: &Value) -> Result<()> {
        let schema = self
            .schemas
            .get(identity)
            .ok_or_else(|| anyhow!("structured-session schema identity is not admitted"))?;
        let validator = jsonschema::validator_for(schema)
            .map_err(|error| anyhow!("compile admitted JSON schema `{identity}`: {error}"))?;
        if let Err(error) = validator.validate(value) {
            bail!("structured-session value failed schema `{identity}`: {error}");
        }
        Ok(())
    }

    fn handle_approval_control(&mut self, control: &Map<String, Value>) -> Result<Value> {
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
        self.handle_approval(&request)
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
        if matches!(route.ceremony, Some(CeremonyAction::Start)) && self.active_login_id.is_some() {
            bail!("one credential enrollment is already active for this worker");
        }
        require_profile_home_within_limit(std::path::Path::new(&self.workload_home))?;
        let expected_binding = prepare_session_binding(
            route.session_binding.as_ref(),
            &self.bound_session_id,
            &mut params,
        )?;
        apply_route_parameters(&route, &mut params, workspace)?;
        self.validate_schema(&route.request_schema, &params)?;
        let timeout_ms = 60_000;
        let observed_params = params.clone();
        let response = self.call_raw(method, params, Duration::from_millis(timeout_ms))?;
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
                Some(CeremonyAction::Start) => {
                    self.active_login_id = response
                        .pointer("/result/loginId")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                Some(CeremonyAction::Clear) => self.active_login_id = None,
                _ => {}
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

    fn take_event_batch(&mut self, limit: usize) -> Result<Option<(Vec<Value>, Vec<Value>)>> {
        if self.events.is_empty() {
            return Ok(None);
        }
        let mut events = Vec::new();
        while events.len() < limit {
            let Some(event) = self.events.pop_front() else {
                break;
            };
            self.event_bytes = self
                .event_bytes
                .saturating_sub(serde_json::to_vec(&event)?.len());
            events.push(event);
        }
        let observations = events
            .iter_mut()
            .flat_map(|event| {
                event
                    .as_object_mut()
                    .and_then(|object| object.remove("session_observations"))
                    .and_then(|value| value.as_array().cloned())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        Ok(Some((events, observations)))
    }

    fn handle_approval(&mut self, request: &Map<String, Value>) -> Result<Value> {
        require_exact_keys(request, &["operation", "requestId", "decision"])?;
        let request_id = request
            .get("requestId")
            .ok_or_else(|| anyhow!("approval has no requestId"))?;
        let key = canonical_id(request_id)?;
        let pending = self
            .server_requests
            .remove(&key)
            .ok_or_else(|| anyhow!("approval is absent, stale, or already resolved"))?
            .message;
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
        let response = match rule.response_style {
            ApprovalResponseStyle::Decision => json!({"decision":decision}),
            ApprovalResponseStyle::PermissionsDenial => {
                if decision == "accept" {
                    bail!("additional-permission requests are denied by the baseline");
                }
                json!({"permissions":{"fileSystem":null,"network":null},
                       "scope":"turn","strictAutoReview":true})
            }
        };
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
            let response = match rule.response_style {
                ApprovalResponseStyle::Decision => json!({"decision":"decline"}),
                ApprovalResponseStyle::PermissionsDenial => json!({
                    "permissions":{"fileSystem":null,"network":null},
                    "scope":"turn","strictAutoReview":true
                }),
            };
            self.send(&json!({"id":request_id,"result":response}))?;
            self.push_event(json!({
                "event_type":"approval.expired",
                "payload":{"request_id":request_id,"operation_class":method}
            }))?;
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
            self.receive_one(remaining.min(Duration::from_millis(250)))?;
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
                if self
                    .server_requests
                    .insert(
                        key,
                        PendingServerRequest {
                            message: message.clone(),
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
                self.push_event(json!({
                    "event_type":"approval.requested",
                    "payload":{
                        "request_id":id,
                        "operation_class":rule.operation_class,
                        "accept_allowed":accept_allowed,
                        "request_digest":ryeos_state::objects::canonical_value_digest(&message)?,
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
                    self.active_login_id = None;
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
        if self.events.len() >= MAX_EVENTS {
            bail!("structured workload event backlog is exhausted");
        }
        let bytes = serde_json::to_vec(&event)?.len();
        self.event_bytes = self
            .event_bytes
            .checked_add(bytes)
            .ok_or_else(|| anyhow!("event byte accounting overflow"))?;
        if self.event_bytes > 8 * 1024 * 1024 {
            bail!("structured-session event backlog byte ceiling is exhausted");
        }
        self.events.push_back(event);
        Ok(())
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

fn drain_bounded_stderr(stderr: impl Read) {
    // Diagnostics are intentionally not forwarded: upstream stderr may
    // contain device material, host paths, prompts, or credentials. Draining
    // prevents child blockage while retaining no second secret-bearing log.
    let mut reader = BufReader::new(stderr);
    let mut buffer = [0u8; 8192];
    let mut total = 0usize;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                total = total.saturating_add(read);
                if total > 8 * 1024 * 1024 {
                    return;
                }
            }
        }
    }
}

fn apply_route_parameters(route: &RouteRule, params: &mut Value, workspace: &str) -> Result<()> {
    let object = params
        .as_object_mut()
        .ok_or_else(|| anyhow!("structured-session route payload must be an object"))?;
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
        bail!("unknown adapter request field `{key}`");
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

    #[test]
    fn approval_without_reviewable_command_is_deny_only() {
        let rule = ServerRequestRule {
            method: "approval".to_owned(),
            schema: "approval.json".to_owned(),
            operation_class: "command".to_owned(),
            response_style: ApprovalResponseStyle::Decision,
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
    fn baseline_is_owner_only_and_detects_drift() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("admitted.conf");
        std::fs::write(&source, b"policy = \"fixed\"\n").unwrap();
        install_or_verify_baseline_config(root.path(), &source, "runtime.conf").unwrap();
        assert_eq!(
            std::fs::metadata(root.path().join("runtime.conf"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o400
        );
        std::fs::write(&source, b"policy = \"changed\"\n").unwrap();
        assert!(install_or_verify_baseline_config(root.path(), &source, "runtime.conf").is_err());
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
    fn session_binding_rejects_cross_thread_and_unbound_routes() {
        let require = SessionBindingRule {
            action: SessionBindingAction::Require,
            request_field: Some("threadId".to_owned()),
            response_pointer: None,
        };
        assert!(prepare_session_binding(Some(&require), &None, &mut json!({})).is_err());
        let bound = Some("thread-one".to_owned());
        assert!(
            prepare_session_binding(
                Some(&require),
                &bound,
                &mut json!({"threadId":"thread-two"}),
            )
            .is_err()
        );
        let mut params = json!({});
        assert_eq!(
            prepare_session_binding(Some(&require), &bound, &mut params).unwrap(),
            Some("thread-one".to_owned())
        );
        assert_eq!(params, json!({"threadId":"thread-one"}));
    }

    #[test]
    fn recovery_binding_requires_the_exact_returned_thread() {
        let recovery = SessionBindingRule {
            action: SessionBindingAction::BindExpected,
            request_field: Some("threadId".to_owned()),
            response_pointer: Some("/result/thread/id".to_owned()),
        };
        let mut params = json!({"threadId":"thread-one"});
        let expected = prepare_session_binding(Some(&recovery), &None, &mut params).unwrap();
        let mut bound = None;
        assert!(
            settle_session_binding(
                Some(&recovery),
                expected.as_deref(),
                &json!({"result":{"thread":{"id":"thread-two"}}}),
                &mut bound,
            )
            .is_err()
        );
        settle_session_binding(
            Some(&recovery),
            expected.as_deref(),
            &json!({"result":{"thread":{"id":"thread-one"}}}),
            &mut bound,
        )
        .unwrap();
        assert_eq!(bound.as_deref(), Some("thread-one"));
    }
}
