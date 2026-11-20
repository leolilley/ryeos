//! End-to-end: named secrets delivered to a structured session.
//!
//! A worker environment names a vault secret as `delivered_secret`. The daemon
//! reads the value at dedicated-session start under the root's admitted
//! environment authority, with the session owner as recipient, and hands it to
//! the worker process only through a sealed in-memory descriptor named by
//! `RYEOS_SESSION_SECRET_FD`. These tests prove, on a real daemon with
//! isolation disabled:
//!
//! 1. the workload receives the value (a digest of it is reported back);
//! 2. no byte of the value reaches the node's state directory or daemon log;
//! 3. a name the vault lacks refuses with `session_secret_missing`;
//! 4. a recipient the node's name ceiling does not admit refuses with
//!    `session_secret_recipient_refused`;
//! 5. an oversize value, or one holding control characters, refuses with
//!    `session_secret_unbounded`;
//! 6. recovery after the recipient rule is withdrawn refuses with
//!    `session_secret_revoked` (ignored, see its reason);
//! 7. the workload holds no sealed descriptor.
//!
//! The session worker here is a script that plays the bridge's part for this
//! boundary: it adopts and closes the sealed descriptor before anything else,
//! then starts a `sh` child whose environment alone carries the value. The
//! script reports what it and the child observed into a directory owned by
//! the test, outside the node's state.

#![cfg(unix)]
#![recursion_limit = "256"]

use ryeosd::test_support as common;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_app::runtime_db::NewCredentialProfile;
use ryeos_app::state_store::NodeIdentitySigner;
use ryeos_app::test_support::TestStateStore;
use ryeos_node_runtime::identity::NodeIdentity;
use serde_json::{Value, json};

use common::DaemonHarness;

const BUNDLE_NAME: &str = "session-secret-fixture";
const WORKER_REF: &str = "worker:session-secret-fixture/portable";
const EXECUTION_REF: &str = "worker_execution:session-secret-fixture/session";
const ENVIRONMENT_REF: &str = "config:session-secret-fixture/environments/default";
const CREDENTIAL_PROFILE_ID: &str = "credential:session-secret-fixture";

/// Vault name and environment variable name of the delivered secret.
const SECRET_NAME: &str = "SESSION_PROBE_TOKEN";
/// Long and distinctive so that no unrelated byte run can equal it.
const SECRET_VALUE: &str = "sess-secret-9f3c1b7e-Zq7vK3mW9xT2pL8dRf4nB6hJ1cY5aS0eUoXiGgHtNwMkPvQrAz-end-of-distinctive-value";

const MISSING_CODE: &str = "session_secret_missing";
const REFUSED_CODE: &str = "session_secret_recipient_refused";
const UNBOUNDED_CODE: &str = "session_secret_unbounded";
const REVOKED_CODE: &str = "session_secret_revoked";

/// Session worker. It reads and closes the sealed secret descriptor first,
/// starts a `sh` child with the value in that child's environment only, and
/// writes a report of digests and descriptor targets (never the value) to the
/// path substituted for `@@REPORT@@`. It then speaks the structured-session
/// protocol like any other worker.
const SESSION_WORKER_SCRIPT: &str = r##"#!/usr/bin/python3
import fcntl, hashlib, json, os, struct, subprocess
NAME = '@@NAME@@'
REPORT = '@@REPORT@@'

def descriptor_targets():
    targets = []
    for entry in sorted(os.listdir('/proc/self/fd'), key=int):
        try:
            targets.append(os.readlink('/proc/self/fd/' + entry))
        except OSError:
            pass
    return targets

report = {'secret_slot_present': False}
values = {}
slot = os.environ.pop('RYEOS_SESSION_SECRET_FD', None)
if slot is not None:
    sfd = int(slot)
    report['secret_slot_present'] = True
    report['secret_fd_target'] = os.readlink('/proc/self/fd/%d' % sfd)
    seals = fcntl.fcntl(sfd, 1034)  # F_GET_SEALS
    report['secret_fd_sealed'] = (seals & 15) == 15
    payload = os.pread(sfd, os.fstat(sfd).st_size, 0)
    os.close(sfd)
    values = json.loads(payload)['values']
    del payload
report['worker_fds_after_close'] = descriptor_targets()
if NAME in values:
    report['worker_sha256'] = hashlib.sha256(values[NAME].encode()).hexdigest()
    # The shell's own glob-expansion directory descriptor is listed but is
    # already closed when its entry is read, so that readlink fails; the probe
    # reports descriptors, not the listing's exit status.
    script = 'printf %s "$' + NAME + '" | sha256sum | cut -d" " -f1; for f in /proc/self/fd/*; do readlink "$f" || true; done'
    child = subprocess.run(
        ['/bin/sh', '-c', script],
        env={'PATH': '/usr/bin:/bin', NAME: values[NAME]},
        stdin=subprocess.DEVNULL, capture_output=True, text=True, check=True,
    )
    lines = child.stdout.splitlines()
    report['child_sha256'] = lines[0]
    report['child_fds'] = lines[1:]
temporary = REPORT + '.tmp'
with open(temporary, 'w', encoding='utf-8') as out:
    json.dump(report, out)
os.replace(temporary, REPORT)
values = None

fd = int(os.environ['RYEOS_SESSION_FD'])
boot = os.environ['RYEOS_SESSION_BOOT_IDENTITY']
home = os.environ['RYEOS_WORKLOAD_HOME']
session_id = 'session-secret-fixture-session'
os.makedirs(os.path.join(home, 'sessions'), exist_ok=True)
with open(os.path.join(home, 'sessions', session_id + '.json'), 'w', encoding='utf-8') as state:
    json.dump({'session_id': session_id, 'sequence': 1}, state, separators=(',', ':'))
def read_exact(size):
    value = b''
    while len(value) < size:
        part = os.read(fd, size - len(value))
        if not part:
            raise SystemExit(0)
        value += part
    return value
def receive():
    return json.loads(read_exact(struct.unpack('>I', read_exact(4))[0]))
def send(kind, request_id, body):
    value = {'protocol':'ryeos.structured-session','version':2,'kind':kind,'request_id':request_id,'body':body}
    raw = json.dumps(value, separators=(',', ':'), allow_nan=False).encode()
    framed = struct.pack('>I', len(raw)) + raw
    offset = 0
    while offset < len(framed):
        offset += os.write(fd, framed[offset:])
send('ready', None, {'boot_identity':boot})
while True:
    frame = receive()
    if frame['kind'] == 'request':
        send('final', frame['request_id'], {
            'response':{'result':{'session_id':session_id}},
            'session_observations':[{'kind':'remote_thread','id':session_id}],
            'result_retention':'durable'
        })
    elif frame['kind'] == 'cancel':
        send('error', frame['request_id'], {'message':'cancelled'})
"##;

fn principal_of_fingerprint(fingerprint: &str) -> String {
    format!("fp:{fingerprint}")
}

/// A well-formed principal that is not the operator and holds no authority.
fn other_principal() -> String {
    principal_of_fingerprint(&lillux::sha256_hex(b"session-secret-other-recipient"))
}

fn source_directory_digest(files: &BTreeMap<String, Vec<u8>>) -> Result<String> {
    let entries = files
        .iter()
        .map(|(path, bytes)| ryeos_state::objects::SourceClosureFile {
            root: "source".to_owned(),
            path: path.clone(),
            blob_hash: lillux::sha256_hex(bytes),
            size: u64::try_from(bytes.len()).expect("fixture source length fits u64"),
            mode: ryeos_state::objects::SourceFileMode::ReadOnly,
        })
        .collect();
    ryeos_state::objects::SourceClosureManifest::new(
        vec![ryeos_state::objects::LogicalSourceRoot {
            id: "source".to_owned(),
        }],
        entries,
    )?
    .digest()
}

/// The admitted structured-session profile of the fixture worker: one
/// session-binding route plus the inspect and resume routes recovery needs.
fn worker_source() -> Result<BTreeMap<String, Vec<u8>>> {
    let route = |id: &str, method: &str, effect: &str, request: &str, binding: Value| {
        json!({
            "id": id,
            "method": method,
            "effect_class": effect,
            "request_schema": request,
            "response_schema": "schema/response.json",
            "fixed_params": {},
            "workspace_fields": [],
            "forbidden_non_null_fields": [],
            "response_predicates": [],
            "observations": [],
            "result_retention": if id == "session.start" { "durable" } else { "ephemeral" },
            "ceremony": null,
            "session_binding": binding,
        })
    };
    let mut start = route(
        "session.start",
        "fixture/session/start",
        "session_mutation",
        "schema/request.json",
        json!({"action":"bind_new","request_field":null,"response_pointer":"/result/session_id"}),
    );
    let mut inspect = route(
        "session.inspect",
        "fixture/session/inspect",
        "pure_read",
        "schema/session-request.json",
        json!({"action":"require","request_field":"session_id","response_pointer":null}),
    );
    inspect["audience"] = json!("runtime");
    let mut resume = route(
        "session.resume",
        "fixture/session/resume",
        "session_mutation",
        "schema/session-request.json",
        json!({"action":"bind_expected","request_field":"session_id","response_pointer":"/result/session_id"}),
    );
    resume["audience"] = json!("runtime");
    start["audience"] = Value::Null;
    start
        .as_object_mut()
        .expect("route is an object")
        .remove("audience");
    let profile = serde_json::to_vec(&json!({
        "schema_version": ryeos_engine::structured_session_profile::STRUCTURED_SESSION_PROFILE_SCHEMA_VERSION,
        "transport": "stdio_jsonrpc",
        "http_sse": null,
        "configuration_authority": "immutable_argv",
        "workload_realization_id": "session-secret-fixture",
        "workload_executable": "fixture-worker",
        "workload_args": [],
        "workload_home_env": "RYEOS_WORKLOAD_HOME",
        "required_process_environment": [],
        "workload_client": null,
        "baseline_config": "baseline.conf",
        "baseline_destination": "fixture.conf",
        "auxiliary_configs": [],
        "runtime_configs": [],
        "configuration_absence": {"absolute_paths":[],"startup_ancestor_paths":[],"route_ancestor_paths":[]},
        "external_candidate": null,
        "portable_state": {
            "schema": 1,
            "restore_contract": "ryeos.worker_session.restore.v1",
            "max_depth": 8,
            "max_entries": 8,
            "max_file_bytes": 65536,
            "max_total_bytes": 65536,
            "selectors": [
                {"pattern":"fixture.conf","class":"forbidden_or_unknown","max_matches":1},
                {"pattern":"sessions/**","class":"forbidden_or_unknown","max_matches":8},
                {"pattern":"sessions/{session_id}.json","class":"portable_session_state","max_matches":1}
            ]
        },
        "credential_subject": {
            "schema": 1,
            "contract": "session-secret-fixture.account.v1",
            "json_pointers": ["/email", "/type"]
        },
        "initialization": [{
            "method": "initialize",
            "effect_class": "pure_read",
            "params": {},
            "response_schema": "schema/init.json",
            "notification": null
        }],
        "recovery": {
            "resume_route": "session.resume",
            "inspect_route": "session.inspect",
            "route_sets": ["session"]
        },
        "route_sets": {"session": ["session.inspect", "session.resume", "session.start"]},
        "routes": [start, inspect, resume],
        "notifications": [],
        "ignored_notifications": {},
        "server_requests": []
    }))?;
    Ok(BTreeMap::from([
        ("baseline.conf".to_owned(), b"fixture=true\n".to_vec()),
        ("profile.json".to_owned(), profile),
        (
            "schema/init.json".to_owned(),
            br#"{"type":"object"}"#.to_vec(),
        ),
        (
            "schema/request.json".to_owned(),
            br#"{"type":"object","additionalProperties":false}"#.to_vec(),
        ),
        (
            "schema/session-request.json".to_owned(),
            br#"{"type":"object","required":["session_id"],"properties":{"session_id":{"type":"string"}},"additionalProperties":false}"#.to_vec(),
        ),
        (
            "schema/response.json".to_owned(),
            br#"{"type":"object","required":["session_id"],"properties":{"session_id":{"type":"string"}},"additionalProperties":false}"#.to_vec(),
        ),
    ]))
}

fn daemon_identity(state_path: &Path) -> Result<NodeIdentity> {
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    NodeIdentity::load(&identity_path)
        .with_context(|| format!("load daemon identity {}", identity_path.display()))
}

/// Open a stopped node's Store through the native test gate.
fn open_daemon_state(state_path: &Path) -> Result<TestStateStore> {
    let identity = daemon_identity(state_path)?;
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    TestStateStore::open(state_path, signer, Arc::new(head_trust))
}

fn sign_with_publisher(body: &str, fixture: &common::fast_fixture::FastFixture) -> String {
    lillux::signature::sign_content_at(
        body,
        &fixture.publisher,
        "#",
        None,
        common::fast_fixture::FAST_FIXTURE_TIME,
    )
}

fn write_signed(
    path: &Path,
    body: &str,
    fixture: &common::fast_fixture::FastFixture,
) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("fixture path has no parent")?)?;
    std::fs::write(path, sign_with_publisher(body, fixture))?;
    Ok(())
}

/// Install the fixture bundle: the worker, its source profile, a worker
/// execution that selects an environment binding, the signed v7 worker
/// environment naming the delivered secret, and an active credential profile
/// owned by the operator.
fn plant_session_worker(
    state_path: &Path,
    fixture: &common::fast_fixture::FastFixture,
    report_path: &Path,
) -> Result<()> {
    common::fast_fixture::register_workflow_bundles(state_path, fixture)?;
    let bundle_root = state_path.join(".ai/bundles").join(BUNDLE_NAME);
    let script = SESSION_WORKER_SCRIPT
        .replace("@@NAME@@", SECRET_NAME)
        .replace(
            "@@REPORT@@",
            report_path.to_str().context("report path is not UTF-8")?,
        );
    common::fast_fixture::install_signed_bundle_binary(
        &bundle_root,
        "session-secret-worker",
        script.as_bytes(),
        &fixture.publisher,
    )?;
    let source = worker_source()?;
    let source_root = bundle_root.join(".ai/workers/session-secret-fixture/lib/portable");
    for (path, bytes) in &source {
        let destination = source_root.join(path);
        std::fs::create_dir_all(destination.parent().context("source path has no parent")?)?;
        std::fs::write(destination, bytes)?;
    }
    let worker_body = format!(
        r#"category: "session-secret-fixture"
version: "1.0.0"
executor_id: "@subprocess"
description: "Session worker that receives one delivered secret."
execution_protocol: protocol:ryeos/core/structured_session
external_product_slots: []
supported_target:
  os: linux
  arch: x86_64
source:
  root: "lib/portable"
  entry: "profile.json"
  digest: "{}"
external_content: []
config:
  command: "bin:{BUNDLE_NAME}/session-secret-worker"
  args: ["${{source.entry}}"]
  env:
    PATH: ""
    LANG: "C"
    LC_ALL: "C"
  timeout_secs: 0
"#,
        source_directory_digest(&source)?,
    );
    write_signed(
        &bundle_root.join(".ai/workers/session-secret-fixture/portable.yaml"),
        &worker_body,
        fixture,
    )?;
    let execution_body = r#"version: "1.0.0"
category: session-secret-fixture
description: Session whose environment names one delivered secret.
config:
  worker_ref: null
  environment_binding: environment
  required_credential_state: active
  route_set: session
  allowed_effect_classes: [pure_read, session_mutation]
  credential_home_env: RYEOS_WORKLOAD_HOME
  workspace_env: RYEOS_WORKSPACE
  require_pinned_cow: true
  required_terminal_publication: retain_result
  max_lifetime_seconds: 3600
  recover_upstream_session: true
  mode:
    kind: session
  candidate_disposition: owner_decision
  workload_client_delegation_caps: []
limits:
  duration_seconds: 3660
  spend_usd: "1"
requires:
  capabilities:
    declared:
      - ryeos.runtime.dedicated_session.start
      - ryeos.runtime.dedicated_session.command
      - ryeos.runtime.dedicated_session.terminate
"#;
    write_signed(
        &bundle_root.join(".ai/worker-executions/session-secret-fixture/session.yaml"),
        execution_body,
        fixture,
    )?;
    let environment_body = format!(
        r#"category: session-secret-fixture/environments
schema: ryeos.worker_environment.v7
worker_ref: {WORKER_REF}
external_product_slots: []
external_content: []
configuration:
  executable_search: []
  process_environment:
    {SECRET_NAME}: {{kind: delivered_secret, name: {SECRET_NAME}}}
credential_requirement:
  workload_family: session-secret-fixture
  required_state: active
  subject_projection_contract: ryeos.credential.delivered_secret.v1
portable_state_contract: ryeos.worker_session.restore.v1
workload_client: null
"#
    );
    write_signed(
        &bundle_root.join(".ai/config/session-secret-fixture/environments/default.yaml"),
        &environment_body,
        fixture,
    )?;
    common::fast_fixture::register_fixture_bundle(state_path, BUNDLE_NAME, &bundle_root, fixture)?;

    let owner_principal = principal_of_fingerprint(&fixture.user_fp());
    let store = open_daemon_state(state_path)?;
    ryeos_app::private_artifact_home::create(
        &state_path.join(ryeos_engine::AI_DIR).join("state"),
        BUNDLE_NAME,
        &BTreeMap::new(),
    )?;
    store.create_credential_profile(NewCredentialProfile {
        activation: ryeos_app::runtime_db::CredentialActivation::delivered_secret([SECRET_NAME])?,
        profile_id: CREDENTIAL_PROFILE_ID,
        owner_principal: &owner_principal,
        home_id: BUNDLE_NAME,
    })?;
    Ok(())
}

/// Replace the node-signed `sealed_vault_names` section so that `recipients`
/// are the only principals allowed to receive `SECRET_NAME`.
fn write_sealed_vault_names(
    state_path: &Path,
    node: &SigningKey,
    operator: &str,
    recipients: &[&str],
) -> Result<()> {
    let list = |principals: &[&str]| json!({"principals": principals, "capabilities": []});
    let body = serde_yaml::to_string(&json!({
        "schema": 1,
        "names": [{
            "name": SECRET_NAME,
            "recipients": list(recipients),
            "writers": list(&[operator]),
        }],
    }))?;
    let signed = lillux::signature::sign_content(&body, node, "#", None);
    std::fs::write(
        state_path.join(".ai/node/policies/sealed_vault_names.yaml"),
        signed,
    )?;
    Ok(())
}

fn plant_sealed_vault_secrets(state_path: &Path, secrets: &HashMap<String, String>) -> Result<()> {
    let vault_dir = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("vault");
    let sk = lillux::vault::VaultSecretKey::generate();
    lillux::vault::write_secret_key(&vault_dir.join("private_key.pem"), &sk)?;
    let pk = sk.public_key();
    lillux::vault::write_public_key(&vault_dir.join("public_key.pem"), &pk)?;
    let store_path = ryeos_app::vault::default_sealed_store_path(state_path);
    ryeos_app::vault::write_sealed_secrets(&store_path, &pk, secrets)?;
    Ok(())
}

/// What the node is planted with for one case.
struct Case {
    /// Vault contents.
    secrets: HashMap<String, String>,
    /// Principals the name ceiling admits as recipients; `None` admits only
    /// the operator.
    recipients: Option<Vec<String>>,
}

impl Case {
    fn holding(value: &str) -> Self {
        Self {
            secrets: HashMap::from([(SECRET_NAME.to_owned(), value.to_owned())]),
            recipients: None,
        }
    }
}

async fn start_node(case: Case, report_path: PathBuf) -> Result<(DaemonHarness, String)> {
    let plant = move |state_path: &Path,
                      _user: &Path,
                      fixture: &common::fast_fixture::FastFixture|
          -> Result<()> {
        plant_session_worker(state_path, fixture, &report_path)?;
        plant_sealed_vault_secrets(state_path, &case.secrets)?;
        let operator = principal_of_fingerprint(&fixture.user_fp());
        let recipients = case
            .recipients
            .clone()
            .unwrap_or_else(|| vec![operator.clone()]);
        let recipients = recipients.iter().map(String::as_str).collect::<Vec<_>>();
        write_sealed_vault_names(state_path, &fixture.node, &operator, &recipients)
    };
    let (daemon, fixture) = DaemonHarness::start_fast_with(plant, |_| {}).await?;
    Ok((daemon, principal_of_fingerprint(&fixture.user_fp())))
}

enum Launch {
    Started(String),
    /// The host cannot qualify process scopes, so no exclusive session can
    /// start here; the caller skips rather than fails.
    HostUnqualified,
    Rejected(String),
}

async fn launch_session(
    daemon: &mut DaemonHarness,
    project: &Path,
    launch_id: &str,
) -> Result<Launch> {
    let body = json!({
        "launch_id": launch_id,
        "item_ref": EXECUTION_REF,
        "ref_bindings": {"environment": ENVIRONMENT_REF},
        "project_path": project.display().to_string(),
        "parameters": {"credential_profile_id": CREDENTIAL_PROFILE_ID},
        "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::local_pinned_capture(
            ryeos_app::execution_policy::ExecutionResponse::Accepted,
        ),
    });
    let (status, launch) = daemon.post_json("/execute/launch", body).await?;
    if !matches!(
        status,
        reqwest::StatusCode::OK | reqwest::StatusCode::ACCEPTED
    ) {
        let stderr = daemon.drain_stderr_nonblocking().await;
        if stderr.contains("qualified node process-scope support")
            || launch
                .to_string()
                .contains("persistent-session dependency `session_worker`")
        {
            // Fails the test when the real process-scope mode is selected.
            common::process_scopes::skip_or_fail_unqualified(&format!(
                "session secret suite: {launch}\n{stderr}"
            ));
            return Ok(Launch::HostUnqualified);
        }
        return Ok(Launch::Rejected(format!("{status}: {launch}\n{stderr}")));
    }
    let chain_root_id = launch
        .get("thread_id")
        .or_else(|| launch.get("chain_root_id"))
        .or_else(|| launch.pointer("/result/thread_id"))
        .or_else(|| launch.pointer("/result/chain_root_id"))
        .or_else(|| launch.pointer("/thread/chain_root_id"))
        .and_then(Value::as_str)
        .with_context(|| format!("session launch returned no chain root: {launch}"))?;
    Ok(Launch::Started(chain_root_id.to_owned()))
}

fn fixture_project() -> Result<tempfile::TempDir> {
    let project = common::process_scopes::tempdir()?;
    std::fs::create_dir_all(project.path().join(".ai"))?;
    std::fs::write(
        project.path().join("fixture.txt"),
        b"session secret fixture\n",
    )?;
    Ok(project)
}

/// Retry the first session command until the worker has started and accepted
/// it, which proves the session is up and bound.
async fn first_session_command(
    daemon: &DaemonHarness,
    chain_root_id: &str,
    key: &str,
) -> Result<Value> {
    let mut last = None;
    for _ in 0..300 {
        let response = daemon
            .post_execute(
                "service:worker-executions/command",
                ".",
                json!({
                    "chain_root_id": chain_root_id,
                    "idempotency_key": key,
                    "route_id": "session.start",
                    "payload": {},
                }),
            )
            .await?;
        if response.0 == reqwest::StatusCode::OK {
            return Ok(response.1);
        }
        last = Some(response);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("session never accepted its first command: {last:?}")
}

/// Everything the node shows about a launch that did not start a worker: the
/// launch response, the daemon log, the retained thread and the session
/// status. Typed refusal codes live in the log; the retained records keep
/// closed classes.
async fn refusal_evidence(
    daemon: &mut DaemonHarness,
    chain_root_id: &str,
    launch_text: &str,
) -> String {
    let mut text = launch_text.to_owned();
    text.push_str(&daemon.drain_stderr_nonblocking().await);
    if let Ok(Some(thread)) = daemon.get_thread(chain_root_id).await {
        text.push_str(&thread.to_string());
    }
    if let Ok((_, status)) = daemon
        .post_execute(
            "service:worker-executions/status",
            ".",
            json!({"chain_root_id": chain_root_id}),
        )
        .await
    {
        text.push_str(&status.to_string());
    }
    text
}

/// What the node shows about a session that never accepted its first command:
/// the retained thread, the session status, and the daemon log without its
/// polling noise.
async fn session_start_evidence(daemon: &mut DaemonHarness, chain_root_id: &str) -> String {
    let mut text = String::new();
    if let Ok(Some(thread)) = daemon.get_thread(chain_root_id).await {
        text.push_str(&format!("thread: {thread}\n"));
    }
    if let Ok((_, status)) = daemon
        .post_execute(
            "service:worker-executions/status",
            ".",
            json!({"chain_root_id": chain_root_id}),
        )
        .await
    {
        text.push_str(&format!("status: {status}\n"));
    }
    let stderr = daemon.drain_stderr_nonblocking().await;
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| {
            !line.contains("resolved project execution context") && !line.contains("cache metric")
        })
        .collect();
    text.push_str("daemon log tail:\n");
    text.push_str(&lines[lines.len().saturating_sub(40)..].join("\n"));
    text
}

/// Launch, then wait until the node reports `code` as the reason the session
/// could not start. Returns `false` when the host cannot start sessions.
async fn assert_start_refused(
    daemon: &mut DaemonHarness,
    project: &Path,
    launch_id: &str,
    code: &str,
    report_path: &Path,
) -> Result<bool> {
    let chain_root_id = match launch_session(daemon, project, launch_id).await? {
        Launch::HostUnqualified => return Ok(false),
        Launch::Rejected(text) => {
            anyhow::ensure!(
                text.contains(code),
                "launch was rejected without `{code}`: {text}"
            );
            return Ok(true);
        }
        Launch::Started(chain_root_id) => chain_root_id,
    };
    let mut evidence = refusal_evidence(daemon, &chain_root_id, "").await;
    for _ in 0..240 {
        if evidence.contains(code) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        evidence = refusal_evidence(daemon, &chain_root_id, "").await;
    }
    anyhow::ensure!(
        evidence.contains(code),
        "the session start never reported `{code}`: {evidence}"
    );
    anyhow::ensure!(
        !report_path.exists(),
        "a refused delivery must never reach worker contact, yet the worker reported"
    );
    Ok(true)
}

/// Every regular file under `root` (skipping `skip`) and `extra_text` checked
/// for each needle. Returns the files that hold one.
fn find_needles(root: &Path, skip: &Path, needles: &[Vec<u8>]) -> Result<Vec<PathBuf>> {
    fn holds(haystack: &[u8], needle: &[u8]) -> bool {
        let Some(first) = needle.first() else {
            return false;
        };
        let mut start = 0;
        while let Some(offset) = haystack[start..].iter().position(|byte| byte == first) {
            let at = start + offset;
            if haystack[at..].starts_with(needle) {
                return true;
            }
            start = at + 1;
        }
        false
    }
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if path == skip {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                if needles.iter().any(|needle| holds(&bytes, needle)) {
                    found.push(path);
                }
            }
        }
    }
    Ok(found)
}

/// The node's whole retained state (sqlite files and write-ahead logs, CAS
/// objects, event logs, capsules, workspaces, the user space) and its log must
/// hold none of `values`, in plain or base64 form. The installed bundle
/// payloads are copied in before the node starts and cannot hold a value
/// planted later, so that one subtree is skipped for speed.
fn assert_values_absent(daemon: &DaemonHarness, values: &[&str]) -> Result<()> {
    let mut needles = Vec::new();
    for value in values {
        needles.push(value.as_bytes().to_vec());
        needles.push(
            base64::engine::general_purpose::STANDARD
                .encode(value)
                .into_bytes(),
        );
    }
    let skip = daemon.state_path.join(ryeos_engine::AI_DIR).join("bundles");
    let mut found = find_needles(&daemon.state_path, &skip, &needles)?;
    found.extend(find_needles(daemon.user_space.path(), &skip, &needles)?);
    anyhow::ensure!(
        found.is_empty(),
        "a delivered value reached retained state: {found:?}"
    );
    let log = daemon.stderr_so_far();
    for needle in &needles {
        anyhow::ensure!(
            !log.as_bytes()
                .windows(needle.len())
                .any(|window| window == needle.as_slice()),
            "a delivered value reached the daemon log"
        );
    }
    Ok(())
}

async fn read_report(path: &Path) -> Result<Value> {
    for _ in 0..300 {
        if let Ok(text) = std::fs::read_to_string(path) {
            return Ok(serde_json::from_str(&text)?);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "the session worker never wrote its report at {}",
        path.display()
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workload_receives_the_value_and_no_state_retains_it() -> Result<()> {
    let reports = common::process_scopes::tempdir()?;
    let report_path = reports.path().join("report.json");
    let project = fixture_project()?;
    let (mut daemon, _operator) =
        start_node(Case::holding(SECRET_VALUE), report_path.clone()).await?;
    let chain_root_id = match launch_session(
        &mut daemon,
        project.path(),
        "L-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1",
    )
    .await?
    {
        Launch::Started(chain_root_id) => chain_root_id,
        Launch::HostUnqualified => return Ok(()),
        Launch::Rejected(text) => anyhow::bail!("session launch was rejected: {text}"),
    };
    if let Err(error) = first_session_command(&daemon, &chain_root_id, "session-secret-start").await
    {
        anyhow::bail!(
            "{error:#}\n{}",
            session_start_evidence(&mut daemon, &chain_root_id).await
        );
    }
    let report = read_report(&report_path).await?;

    // 1. The workload received the value: its digest matches, both as the
    // worker decoded it and as the `sh` child saw it in its environment.
    let expected = lillux::sha256_hex(SECRET_VALUE.as_bytes());
    assert_eq!(report["worker_sha256"], json!(expected), "{report:#}");
    assert_eq!(report["child_sha256"], json!(expected), "{report:#}");

    // The value arrived through a sealed in-memory descriptor, not a file.
    assert_eq!(report["secret_slot_present"], json!(true), "{report:#}");
    assert!(
        report["secret_fd_target"]
            .as_str()
            .is_some_and(|target| target.starts_with("/memfd:")),
        "{report:#}"
    );
    assert_eq!(report["secret_fd_sealed"], json!(true), "{report:#}");

    // 7. Neither the worker after adopting the descriptor nor the workload
    // child holds the sealed secret descriptor. The worker may hold other
    // memfds (on a real host its own program is a descriptor-bound memfd), so
    // the check names the sealed secret's own memfd, taken from the report.
    let sealed = report["secret_fd_target"]
        .as_str()
        .map(|target| target.trim_end_matches(" (deleted)").to_owned())
        .context("the report names no sealed secret descriptor")?;
    assert!(sealed.starts_with("/memfd:"), "{report:#}");
    for holder in ["worker_fds_after_close", "child_fds"] {
        let targets = report[holder]
            .as_array()
            .with_context(|| format!("report has no `{holder}`: {report:#}"))?;
        assert!(!targets.is_empty(), "{holder} listed nothing: {report:#}");
        assert!(
            targets
                .iter()
                .filter_map(Value::as_str)
                .all(|target| target.trim_end_matches(" (deleted)") != sealed),
            "{holder} holds the sealed secret descriptor: {report:#}"
        );
    }
    assert!(
        !report.to_string().contains(SECRET_VALUE),
        "the report must carry digests only"
    );

    // 2. After the session, nothing the node retained holds the value.
    let (status, terminated) = daemon
        .post_execute(
            "service:worker-executions/terminate",
            ".",
            json!({"chain_root_id": chain_root_id, "reason": "cancelled"}),
        )
        .await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "session termination returned {status}: {terminated}"
    );
    daemon.kill_daemon().await?;
    assert_values_absent(&daemon, &[SECRET_VALUE])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_the_vault_lacks_refuses_as_missing() -> Result<()> {
    let reports = common::process_scopes::tempdir()?;
    let report_path = reports.path().join("report.json");
    let project = fixture_project()?;
    let case = Case {
        secrets: HashMap::from([("UNRELATED_VAULT_NAME".to_owned(), SECRET_VALUE.to_owned())]),
        recipients: None,
    };
    let (mut daemon, _operator) = start_node(case, report_path.clone()).await?;
    if !assert_start_refused(
        &mut daemon,
        project.path(),
        "L-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2",
        MISSING_CODE,
        &report_path,
    )
    .await?
    {
        return Ok(());
    }
    daemon.kill_daemon().await?;
    assert_values_absent(&daemon, &[SECRET_VALUE])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipient_the_ceiling_does_not_admit_is_refused() -> Result<()> {
    let reports = common::process_scopes::tempdir()?;
    let report_path = reports.path().join("report.json");
    let project = fixture_project()?;
    let mut case = Case::holding(SECRET_VALUE);
    // The ceiling lists another principal, so it refuses even the operator.
    case.recipients = Some(vec![other_principal()]);
    let (mut daemon, _operator) = start_node(case, report_path.clone()).await?;
    if !assert_start_refused(
        &mut daemon,
        project.path(),
        "L-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3",
        REFUSED_CODE,
        &report_path,
    )
    .await?
    {
        return Ok(());
    }
    daemon.kill_daemon().await?;
    assert_values_absent(&daemon, &[SECRET_VALUE])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_oversize_value_is_refused_as_unbounded() -> Result<()> {
    let reports = common::process_scopes::tempdir()?;
    let report_path = reports.path().join("report.json");
    let project = fixture_project()?;
    // One byte over the 16 KiB bound, built from the distinctive value so the
    // byte search is still meaningful.
    let mut oversize = String::new();
    while oversize.len() <= 16 * 1024 {
        oversize.push_str(SECRET_VALUE);
    }
    let (mut daemon, _operator) = start_node(Case::holding(&oversize), report_path.clone()).await?;
    if !assert_start_refused(
        &mut daemon,
        project.path(),
        "L-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4",
        UNBOUNDED_CODE,
        &report_path,
    )
    .await?
    {
        return Ok(());
    }
    daemon.kill_daemon().await?;
    assert_values_absent(&daemon, &[SECRET_VALUE])?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_value_with_control_characters_is_refused_as_unbounded() -> Result<()> {
    let reports = common::process_scopes::tempdir()?;
    let report_path = reports.path().join("report.json");
    let project = fixture_project()?;
    let with_control = format!("{SECRET_VALUE}\u{7}-after-the-bell");
    let (mut daemon, _operator) =
        start_node(Case::holding(&with_control), report_path.clone()).await?;
    if !assert_start_refused(
        &mut daemon,
        project.path(),
        "L-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa5",
        UNBOUNDED_CODE,
        &report_path,
    )
    .await?
    {
        return Ok(());
    }
    daemon.kill_daemon().await?;
    assert_values_absent(&daemon, &[SECRET_VALUE])?;
    Ok(())
}

/// Recovery re-authorises delivery: after the ceiling stops admitting the
/// owner and the node restarts, the recovering session is refused with
/// `session_secret_revoked` instead of replaying the earlier value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_after_the_recipient_rule_is_withdrawn_is_refused_as_revoked() -> Result<()> {
    let reports = common::process_scopes::tempdir()?;
    let report_path = reports.path().join("report.json");
    let project = fixture_project()?;
    let (mut daemon, operator) =
        start_node(Case::holding(SECRET_VALUE), report_path.clone()).await?;
    let chain_root_id = match launch_session(
        &mut daemon,
        project.path(),
        "L-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6",
    )
    .await?
    {
        Launch::Started(chain_root_id) => chain_root_id,
        Launch::HostUnqualified => return Ok(()),
        Launch::Rejected(text) => anyhow::bail!("session launch was rejected: {text}"),
    };
    first_session_command(&daemon, &chain_root_id, "session-secret-recover-start").await?;
    read_report(&report_path).await?;

    daemon.kill_daemon().await?;
    let node = common::fast_fixture::node_signing_key();
    let other = other_principal();
    write_sealed_vault_names(&daemon.state_path, &node, &operator, &[other.as_str()])?;
    common::fast_fixture::seal_initialized_state(&daemon.state_path)?;
    daemon.respawn_with(|_| {}).await?;

    let mut evidence = String::new();
    for attempt in 0..240 {
        let (_, response) = daemon
            .post_execute(
                "service:worker-executions/command",
                ".",
                json!({
                    "chain_root_id": chain_root_id,
                    "idempotency_key": format!("session-secret-after-revoke-{attempt}"),
                    "route_id": "session.inspect",
                    "payload": {},
                }),
            )
            .await?;
        evidence = refusal_evidence(&mut daemon, &chain_root_id, &response.to_string()).await;
        if evidence.contains(REVOKED_CODE) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    anyhow::ensure!(
        evidence.contains(REVOKED_CODE),
        "recovery never reported `{REVOKED_CODE}`: {evidence}"
    );
    daemon.kill_daemon().await?;
    assert_values_absent(&daemon, &[SECRET_VALUE])?;
    Ok(())
}
