//! Process-crash qualification for the durable worker-handoff recovery path.
//!
//! Source-recovery fixtures write the exact durable job and signed chain while
//! the daemon is stopped. Target-request fixtures use a separately signed
//! source authority and the real authenticated remote-node request boundary.
//! The parent observes each named boundary over the inherited test-only Lillux
//! channel
//! and SIGKILLs the process, so no Rust unwinding or request cleanup can
//! manufacture the outcome.

#![cfg(all(unix, feature = "handoff-test-support"))]

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::routing::post;
use base64::Engine as _;
use lillux::crypto::SigningKey;
use lillux::time::{Duration, MonotonicTimer};
use ryeos_app::identity::NodeIdentity;
use ryeos_app::runtime_db::{NewCredentialProfile, NewCredentialProfileReservation};
use ryeos_app::state_store::{NewEventRecord, NewThreadRecord, NodeIdentitySigner, StateStore};
use ryeos_app::worker_handoff::test_support::{
    AppendAuthorityLane, CredentialDisposition, DurableJobExpectation, DurableJobPhase,
    DurableJobResult, DurableJobState, ExpectedHeadSigner, HANDOFF_ACCEPTANCE_MATRIX,
    HandoffAcceptanceCase, HandoffCrashBoundary, HandoffDurableSnapshot, HandoffMeasurementRecord,
    HandoffMeasurementReport, HandoffNode, HandoffObservedMeasurements, HandoffPhaseCutEvidence,
    OperatorOutcome, PortableStateDisposition, ProcessDisposition, RecoveryTrigger,
    RequestOutcomeAtCut, RetryDisposition, SourcePlacementState, StagingRootDisposition,
    SuccessorPlacementState, WorkspaceDisposition,
};
use ryeos_app::worker_handoff::{
    WORKER_PLACEMENT_ABORT_SERVICE, WORKER_PLACEMENT_ADOPT_SERVICE,
    WORKER_SESSION_HANDOFF_MAX_ATTEMPTS, WORKER_SESSION_HANDOFF_OPERATION,
    WorkerHandoffAbortFenceEvidence, WorkerHandoffAdoptionReceiptEvidence, WorkerHandoffJobRole,
    WorkerHandoffPhase, WorkerHandoffTerminalFailureEvidence, WorkerPlacementAbortRequest,
    WorkerPlacementAbortResponse, WorkerPlacementAbortResult, WorkerPlacementAdmissionEvidence,
    WorkerPlacementAdoptRequest, WorkerPlacementAdoptResult, WorkerSessionHandoffJobOperation,
    WorkerSessionHandoffProgress,
};
use ryeos_app::write_barrier::WriteBarrier;
use ryeos_state::{
    FinishSyncJobAttempt, NewSyncJob, NewSyncJobAttempt,
    SYNC_JOB_UNBOUNDED_RETAINED_TERMINAL_ATTEMPTS, SyncJobAttemptState, SyncJobState,
    SyncJobUpdate,
};
use tokio::net::TcpListener;

use common::DaemonHarness;

const DAEMON_SOURCE_SITE_ID: &str = "site:testhost";
const REMOTE_TARGET_SITE_ID: &str = "site:handoff-target";
const REMOTE_SOURCE_SITE_ID: &str = "site:handoff-source";
const DAEMON_TARGET_SITE_ID: &str = "site:testhost";
const TARGET_REMOTE_NAME: &str = "handoff-source";
const TARGET_CREDENTIAL_PROFILE_ID: &str = "credential:handoff-fixture";
const TARGET_CREDENTIAL_RESERVATION_ID: &str = "reservation:handoff-fixture";

fn target_node_signing_key() -> SigningKey {
    // Deliberately differs from every standard fast-fixture role. Cross-site
    // qualification must never accidentally authenticate both daemons as the
    // same deterministic node.
    SigningKey::from_bytes(&[61_u8; 32])
}

const PORTABLE_WORKER_REF: &str = "worker:handoff-fixture/portable";
const PORTABLE_EXECUTION_REF: &str = "worker_execution:handoff-fixture/session";
const PORTABLE_CREDENTIAL_PROFILE_ID: &str = "credential:handoff-portable";
const PORTABLE_SOURCE_SITE_ID: &str = "site:handoff-source";
const PORTABLE_TARGET_SITE_ID: &str = "site:handoff-target";
const PORTABLE_TARGET_REMOTE: &str = "handoff-target";
const PORTABLE_SOURCE_REMOTE: &str = "handoff-source";

struct PortableCheckpoint {
    chain_root_id: String,
    manifest_ref: String,
}

fn remote_config(
    name: &str,
    url: String,
    remote: &common::fast_fixture::FastFixture,
    site_id: &str,
    project_binding: Option<(&Path, &Path)>,
) -> Result<ryeos_api::remote::config::RemoteConfig> {
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(remote.node.verifying_key().as_bytes());
    let project_bindings = if let Some((local, target)) = project_binding {
        HashMap::from([(
            ryeos_api::remote::config::local_project_identity(
                &ryeos_api::remote::config::canonical_local_project_path(local)?,
            )?
            .to_owned(),
            ryeos_api::remote::config::RemoteProjectBinding {
                remote_project_path: target.display().to_string(),
                sync_scope: ryeos_api::remote::config::ProjectSyncScope::FullProject,
            },
        )])
    } else {
        HashMap::new()
    };
    Ok(ryeos_api::remote::config::RemoteConfig {
        name: name.to_owned(),
        url,
        principal_id: format!("fp:{}", remote.node_fp()),
        signing_key: format!("ed25519:{public_key}"),
        site_id: site_id.to_owned(),
        vault_fingerprint: format!("sha256:{name}-fixture-vault"),
        ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
        project_bindings,
    })
}

fn install_single_remote(
    state_path: &Path,
    remote: ryeos_api::remote::config::RemoteConfig,
) -> Result<()> {
    ryeos_api::remote::config::save_remotes(
        state_path,
        &HashMap::from([(remote.name.clone(), remote)]),
    )
}

fn authorize_remote_node(
    state_path: &Path,
    local: &common::fast_fixture::FastFixture,
    remote: &common::fast_fixture::FastFixture,
    remote_site_id: &str,
    scopes: &[&str],
) -> Result<()> {
    let public_key =
        base64::engine::general_purpose::STANDARD.encode(remote.node.verifying_key().as_bytes());
    ryeos_app::identity::write_authorized_remote_node_key_toml(
        &state_path
            .join(ryeos_engine::AI_DIR)
            .join("node/auth/authorized_keys"),
        &remote.node_fp(),
        &public_key,
        &scopes
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect::<Vec<_>>(),
        "portable-handoff-peer",
        &local.node_fp(),
        &lillux::time::iso8601_now(),
        remote_site_id,
        &local.node,
    )?;
    Ok(())
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

fn portable_worker_source() -> Result<BTreeMap<String, Vec<u8>>> {
    let profile = serde_json::to_vec(&serde_json::json!({
        "schema_version":1,
        "configuration_authority":"immutable_argv",
        "workload_realization_id":"handoff-fixture",
        "workload_executable":"fixture-worker",
        "workload_args":[],
        "workload_home_env":"RYEOS_WORKLOAD_HOME",
        "baseline_config":"baseline.conf",
        "baseline_destination":"fixture.conf",
        "portable_state":{
            "schema":1,
            "restore_contract":"ryeos.worker_session.restore.v1",
            "max_depth":8,
            "max_entries":8,
            "max_file_bytes":65536,
            "max_total_bytes":65536,
            "selectors":[{
                "pattern":"fixture.conf",
                "class":"forbidden_or_unknown",
                "max_matches":1
            },{
                "pattern":"sessions/**",
                "class":"forbidden_or_unknown",
                "max_matches":8
            },{
                "pattern":"sessions/{session_id}.json",
                "class":"portable_session_state",
                "max_matches":1
            }]
        },
        "credential_subject":{
            "schema":1,
            "contract":"handoff-fixture.account.v1",
            "json_pointers":["/email","/type"]
        },
        "initialization":[{
            "method":"initialize",
            "effect_class":"pure_read",
            "params":{},
            "response_schema":"schema/init.json",
            "notification":null
        }],
        "recovery":{
            "resume_route":"session.resume",
            "inspect_route":"session.inspect",
            "route_sets":["session"]
        },
        "route_sets":{"session":["session.inspect","session.resume","session.start"]},
        "routes":[{
            "id":"session.start",
            "method":"fixture/session/start",
            "effect_class":"session_mutation",
            "request_schema":"schema/request.json",
            "response_schema":"schema/response.json",
            "fixed_params":{},
            "workspace_fields":[],
            "forbidden_non_null_fields":[],
            "response_predicates":[],
            "observations":[],
            "result_retention":"durable",
            "ceremony":null,
            "session_binding":{
                "action":"bind_new",
                "request_field":null,
                "response_pointer":"/result/session_id"
            }
        },{
            "id":"session.inspect",
            "method":"fixture/session/inspect",
            "effect_class":"pure_read",
            "request_schema":"schema/session-request.json",
            "response_schema":"schema/response.json",
            "fixed_params":{},
            "workspace_fields":[],
            "forbidden_non_null_fields":[],
            "response_predicates":[],
            "observations":[],
            "result_retention":"ephemeral",
            "ceremony":null,
            "audience":"runtime",
            "session_binding":{
                "action":"require",
                "request_field":"session_id",
                "response_pointer":null
            }
        },{
            "id":"session.resume",
            "method":"fixture/session/resume",
            "effect_class":"session_mutation",
            "request_schema":"schema/session-request.json",
            "response_schema":"schema/response.json",
            "fixed_params":{},
            "workspace_fields":[],
            "forbidden_non_null_fields":[],
            "response_predicates":[],
            "observations":[],
            "result_retention":"ephemeral",
            "ceremony":null,
            "audience":"runtime",
            "session_binding":{
                "action":"bind_expected",
                "request_field":"session_id",
                "response_pointer":"/result/session_id"
            }
        }],
        "notifications":[],
        "ignored_notifications":{},
        "server_requests":[]
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

const PORTABLE_WORKER_SCRIPT: &[u8] = br#"#!/usr/bin/python3
import json, os, struct
fd = int(os.environ['RYEOS_SESSION_FD'])
boot = os.environ['RYEOS_SESSION_BOOT_IDENTITY']
home = os.environ['RYEOS_WORKLOAD_HOME']
session_id = 'handoff-fixture-session'
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
    value = {'protocol':'ryeos.structured-session','version':1,'kind':kind,'request_id':request_id,'body':body}
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
"#;

fn plant_portable_worker(
    state_path: &Path,
    fixture: &common::fast_fixture::FastFixture,
) -> Result<()> {
    common::fast_fixture::register_standard_bundle(state_path, fixture)?;
    let bundle_root = state_path.join(".ai/bundles/handoff-portable-fixture");
    common::fast_fixture::install_signed_bundle_binary(
        &bundle_root,
        "handoff-portable-worker",
        PORTABLE_WORKER_SCRIPT,
        &fixture.publisher,
    )?;
    let source = portable_worker_source()?;
    let source_root = bundle_root.join(".ai/workers/handoff-fixture/lib/portable");
    for (path, bytes) in &source {
        let destination = source_root.join(path);
        std::fs::create_dir_all(destination.parent().context("source path has no parent")?)?;
        std::fs::write(destination, bytes)?;
    }
    let worker_body = format!(
        r#"category: "handoff-fixture"
version: "1.0.0"
executor_id: "@subprocess"
description: "Portable handoff qualification worker."
execution_protocol: protocol:ryeos/core/structured_session
supported_target:
  os: linux
  arch: x86_64
source:
  root: "lib/portable"
  entry: "profile.json"
  digest: "{}"
external_content: []
config:
  command: "bin:handoff-portable-fixture/handoff-portable-worker"
  args: ["${{source.entry}}"]
  env:
    PATH: ""
    LANG: "C"
    LC_ALL: "C"
  timeout_secs: 0
"#,
        source_directory_digest(&source)?,
    );
    let worker_path = bundle_root.join(".ai/workers/handoff-fixture/portable.yaml");
    std::fs::create_dir_all(worker_path.parent().context("worker path has no parent")?)?;
    std::fs::write(
        worker_path,
        lillux::signature::sign_content_at(
            &worker_body,
            &fixture.publisher,
            "#",
            None,
            common::fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    let execution_body = format!(
        r#"version: "1.0.0"
category: handoff-fixture
description: Portable handoff qualification session.
config:
  worker_ref: {PORTABLE_WORKER_REF}
  environment_binding: null
  required_credential_state: active
  route_set: session
  allowed_effect_classes: [pure_read, session_mutation]
  credential_home_env: RYEOS_WORKLOAD_HOME
  workspace_env: RYEOS_WORKSPACE
  require_pinned_cow: true
  required_terminal_publication: retain_result
  max_lifetime_seconds: 3600
  recover_upstream_session: true
limits:
  duration_seconds: 3660
requires:
  capabilities:
    declared:
      - ryeos.runtime.dedicated_session.start
      - ryeos.runtime.dedicated_session.command
      - ryeos.runtime.dedicated_session.terminate
"#
    );
    let execution_path = bundle_root.join(".ai/worker-executions/handoff-fixture/session.yaml");
    std::fs::create_dir_all(
        execution_path
            .parent()
            .context("worker execution path has no parent")?,
    )?;
    std::fs::write(
        execution_path,
        lillux::signature::sign_content_at(
            &execution_body,
            &fixture.publisher,
            "#",
            None,
            common::fast_fixture::FAST_FIXTURE_TIME,
        ),
    )?;
    common::fast_fixture::register_fixture_bundle(
        state_path,
        "handoff-portable-fixture",
        &bundle_root,
        fixture,
    )?;

    let store = open_daemon_state(state_path)?;
    ryeos_app::private_artifact_home::create(
        &state_path.join(ryeos_engine::AI_DIR).join("state"),
        "handoff-portable-fixture",
        &BTreeMap::new(),
    )?;
    store.create_credential_profile(NewCredentialProfile {
        profile_id: PORTABLE_CREDENTIAL_PROFILE_ID,
        owner_principal: &format!("fp:{}", fixture.user_fp()),
        home_id: "handoff-portable-fixture",
    })?;
    let lock_id = "credential-enrollment:handoff-portable-fixture";
    store.acquire_credential_profile(
        PORTABLE_CREDENTIAL_PROFILE_ID,
        &format!("fp:{}", fixture.user_fp()),
        lock_id,
    )?;
    let login_id = "credential-login:handoff-portable-fixture";
    let epoch = store.begin_credential_enrollment(
        PORTABLE_CREDENTIAL_PROFILE_ID,
        lock_id,
        login_id,
        i64::try_from(lillux::time::timestamp_millis())? + 60_000,
    )?;
    store.complete_credential_enrollment(
        PORTABLE_CREDENTIAL_PROFILE_ID,
        lock_id,
        login_id,
        epoch,
        &serde_json::json!({"email":"fixture@example.test","type":"fixture"}),
    )?;
    store.release_credential_profile(PORTABLE_CREDENTIAL_PROFILE_ID, lock_id)?;
    Ok(())
}

struct SeededSourceHandoff {
    operation: WorkerSessionHandoffJobOperation,
    job_id: String,
    source_head_hash: String,
}

struct SourceAbortAuthority {
    _state_root: tempfile::TempDir,
    signing_key: SigningKey,
    operation: WorkerSessionHandoffJobOperation,
    source_head_payload: ryeos_state::sync::ExportPayload,
    abort_payload: ryeos_state::sync::ExportPayload,
    abort_head_hash: String,
}

#[derive(Clone, Copy)]
struct TargetAbortCutExpectation {
    phase: WorkerHandoffPhase,
    state: SyncJobState,
    abort_root_retained: bool,
    reservation_state: &'static str,
    fence_terminal_disposition: &'static str,
}

fn fixture_hash(label: &str) -> String {
    lillux::sha256_hex(format!("handoff-crash-recovery-fixture:{label}").as_bytes())
}

fn captured_policy(item_ref: &str) -> ryeos_state::objects::CapturedThreadHistoryPolicy {
    let hash = fixture_hash("captured-policy");
    ryeos_state::objects::CapturedThreadHistoryPolicy {
        retention: ryeos_state::objects::ThreadHistoryRetention::Durable,
        canonical_item_ref: item_ref.to_owned(),
        item_content_hash: hash.clone(),
        item_signer_fingerprint: Some(hash.clone()),
        item_trust_class: ryeos_state::objects::CapturedItemTrustClass::Trusted,
        kind_schema_content_hash: hash,
        resolved_from: ryeos_state::objects::CapturedPolicyProvenance::NodeDefault {
            node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::test_policy(),
        },
    }
}

fn daemon_identity(state_path: &Path) -> Result<NodeIdentity> {
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    NodeIdentity::load(&identity_path)
        .with_context(|| format!("load daemon identity {}", identity_path.display()))
}

fn open_daemon_state(state_path: &Path) -> Result<StateStore> {
    open_daemon_state_with_trusted_nodes(state_path, &[])
}

fn open_daemon_state_with_trusted_nodes(
    state_path: &Path,
    trusted_nodes: &[&SigningKey],
) -> Result<StateStore> {
    let runtime_state_dir = state_path.join(ryeos_engine::AI_DIR).join("state");
    let runtime_db_path = runtime_state_dir.join("runtime.sqlite3");
    let identity = daemon_identity(state_path)?;
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    for key in trusted_nodes {
        let verifying_key = key.verifying_key();
        head_trust.insert(
            lillux::signature::compute_fingerprint(&verifying_key),
            verifying_key,
        );
    }
    StateStore::new_with_head_trust(
        state_path.to_path_buf(),
        runtime_state_dir,
        runtime_db_path,
        signer,
        WriteBarrier::new(),
        Arc::new(head_trust),
    )
}

/// Open the exact published projection for concurrent qualification reads.
/// Unlike `open_daemon_state_with_trusted_nodes`, this never claims the live
/// daemon's exclusive runtime-state namespace or constructs mutable runtime
/// authority.
fn open_daemon_projection_state_with_trusted_nodes(
    state_path: &Path,
    trusted_nodes: &[&SigningKey],
) -> Result<StateStore> {
    let runtime_state_dir = state_path.join(ryeos_engine::AI_DIR).join("state");
    let identity = daemon_identity(state_path)?;
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    for key in trusted_nodes {
        let verifying_key = key.verifying_key();
        head_trust.insert(
            lillux::signature::compute_fingerprint(&verifying_key),
            verifying_key,
        );
    }
    StateStore::new_for_projection_verification(
        runtime_state_dir,
        signer,
        WriteBarrier::new(),
        Arc::new(head_trust),
    )
}

#[derive(Debug)]
struct ObservedHandoffJob {
    expectation: DurableJobExpectation,
    phase: Option<WorkerHandoffPhase>,
    staging_roots: StagingRootDisposition,
}

fn durable_job_state(state: SyncJobState) -> DurableJobState {
    match state {
        SyncJobState::Planned => DurableJobState::Planned,
        SyncJobState::Running => DurableJobState::Running,
        SyncJobState::Retryable => DurableJobState::Retryable,
        SyncJobState::Completed => DurableJobState::Completed,
        SyncJobState::Failed => DurableJobState::Failed,
        SyncJobState::Cancelled => DurableJobState::Cancelled,
    }
}

fn durable_job_phase(phase: &str) -> Result<DurableJobPhase> {
    Ok(match phase {
        "planned" => DurableJobPhase::Planned,
        "source_exported" => DurableJobPhase::SourceExported,
        "target_prepare" => DurableJobPhase::TargetPrepare,
        "placement_admission" => DurableJobPhase::PlacementAdmission,
        "target_prepared" => DurableJobPhase::TargetPrepared,
        "abort_authorized" => DurableJobPhase::AbortAuthorized,
        "target_abort" => DurableJobPhase::TargetAbort,
        ryeos_app::worker_handoff::WORKER_HANDOFF_TARGET_ABORT_CLAIM_PHASE => {
            DurableJobPhase::TargetAbortClaimed
        }
        "source_committed" => DurableJobPhase::SourceCommitted,
        "target_adopt" => DurableJobPhase::TargetAdopt,
        "target_adopted" => DurableJobPhase::TargetAdopted,
        "state_installed" => DurableJobPhase::StateInstalled,
        "process_attached" => DurableJobPhase::ProcessAttached,
        "completed" => DurableJobPhase::Completed,
        "aborted" => DurableJobPhase::Aborted,
        other => anyhow::bail!("unknown durable handoff job phase `{other}`"),
    })
}

fn find_handoff_job(
    store: &StateStore,
    chain_root_id: &str,
    role: WorkerHandoffJobRole,
) -> Result<Option<ryeos_state::SyncJobRecord>> {
    let matches = store
        .with_state_db(|db| {
            db.list_sync_jobs_by_operation_type_before(WORKER_SESSION_HANDOFF_OPERATION, None, 128)
        })?
        .into_iter()
        .filter(|job| {
            WorkerSessionHandoffJobOperation::from_value(job.operation.clone()).is_ok_and(
                |operation| operation.chain_root_id == chain_root_id && operation.role == role,
            )
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        matches.len() <= 1,
        "handoff qualification found duplicate {role:?} jobs for `{chain_root_id}`"
    );
    Ok(matches.into_iter().next())
}

fn observe_handoff_job(
    store: &StateStore,
    job: Option<&ryeos_state::SyncJobRecord>,
) -> Result<ObservedHandoffJob> {
    let Some(job) = job else {
        return Ok(ObservedHandoffJob {
            expectation: DurableJobExpectation {
                state: DurableJobState::Absent,
                phase: DurableJobPhase::Absent,
                result: DurableJobResult::Absent,
                active_attempt: false,
            },
            phase: None,
            staging_roots: StagingRootDisposition::None,
        });
    };
    let active_attempt = store
        .with_state_db(|db| db.list_sync_job_attempts(&job.job_id))?
        .iter()
        .any(|attempt| attempt.state == SyncJobAttemptState::Running);
    let (result, phase) = match job.result.clone() {
        Some(value) => {
            if let Ok(progress) = WorkerSessionHandoffProgress::from_value(value.clone()) {
                (
                    DurableJobResult::Progress(progress.phase),
                    Some(progress.phase),
                )
            } else if serde_json::from_value::<
                ryeos_app::worker_handoff::WorkerPlacementAdoptResponse,
            >(value.clone())
            .is_ok()
            {
                (
                    DurableJobResult::AdoptionReceipt,
                    Some(WorkerHandoffPhase::Completed),
                )
            } else if serde_json::from_value::<WorkerPlacementAbortResponse>(value).is_ok() {
                (
                    DurableJobResult::AbortReceipt,
                    Some(WorkerHandoffPhase::AbortAuthorized),
                )
            } else {
                anyhow::bail!("handoff job `{}` retained an unknown result", job.job_id);
            }
        }
        None => {
            let phase = matches!(job.phase.as_str(), "planned" | "placement_admission")
                .then_some(WorkerHandoffPhase::Planned);
            (DurableJobResult::Absent, phase)
        }
    };
    let terminal = matches!(
        job.state,
        SyncJobState::Completed | SyncJobState::Cancelled | SyncJobState::Failed
    );
    Ok(ObservedHandoffJob {
        expectation: DurableJobExpectation {
            state: durable_job_state(job.state),
            phase: durable_job_phase(&job.phase)?,
            result,
            active_attempt,
        },
        phase,
        staging_roots: if job.roots.is_empty() {
            StagingRootDisposition::None
        } else if terminal {
            StagingRootDisposition::TerminalJobOwned
        } else {
            StagingRootDisposition::ActiveJobOwned
        },
    })
}

fn portable_fixture_state_is_installed(state_path: &Path) -> Result<bool> {
    let home_path = ryeos_app::private_artifact_home::home_path(
        &state_path.join(ryeos_engine::AI_DIR).join("state"),
        "handoff-portable-fixture",
    )?;
    let Some(home) = lillux::PinnedDirectory::open(&home_path)? else {
        return Ok(false);
    };
    let Some(sessions) = home.open_child_directory("sessions".as_ref())? else {
        return Ok(false);
    };
    Ok(sessions
        .open_regular("handoff-fixture-session.json".as_ref(), false)?
        .is_some())
}

fn observe_real_handoff_snapshot(
    handoff: &RealPortableHandoff,
    successor_id: &str,
) -> Result<HandoffDurableSnapshot> {
    let trusted = [&handoff.source_fixture.node, &handoff.target_fixture.node];
    let source = open_daemon_state_with_trusted_nodes(&handoff.source.state_path, &trusted)?;
    let target = open_daemon_state_with_trusted_nodes(&handoff.target.state_path, &trusted)?;
    let chain_root_id = &handoff.checkpoint.chain_root_id;
    let source_job = find_handoff_job(&source, chain_root_id, WorkerHandoffJobRole::Source)?;
    let target_job = find_handoff_job(&target, chain_root_id, WorkerHandoffJobRole::Target)?;
    if let (Some(source_job), Some(target_job)) = (&source_job, &target_job) {
        let source_operation =
            WorkerSessionHandoffJobOperation::from_value(source_job.operation.clone())?;
        let target_operation =
            WorkerSessionHandoffJobOperation::from_value(target_job.operation.clone())?;
        anyhow::ensure!(
            source_operation.target_projection(target_operation.peer_remote_name.clone())?
                == target_operation,
            "source and target retained different handoff operations"
        );
    }
    let source_observed = observe_handoff_job(&source, source_job.as_ref())?;
    let target_observed = observe_handoff_job(&target, target_job.as_ref())?;

    let source_fingerprint = handoff.source_fixture.node_fp();
    let target_fingerprint = handoff.target_fixture.node_fp();
    let source_head = source
        .with_state_db(|db| db.read_generic_head_ref("chains", chain_root_id))?
        .context("source qualification chain head disappeared")?;
    let target_head =
        target.with_state_db(|db| db.read_generic_head_ref("chains", chain_root_id))?;
    let head_signer = if target_head
        .as_ref()
        .is_some_and(|head| head.signer == target_fingerprint)
    {
        ExpectedHeadSigner::TargetNode
    } else {
        anyhow::ensure!(
            source_head.signer == source_fingerprint,
            "handoff chain head is signed by neither admitted node"
        );
        ExpectedHeadSigner::SourceNode
    };

    let current_placement = source.current_chain_placement_thread_id(chain_root_id)?;
    let append_authority = if current_placement.as_deref() == Some(successor_id) {
        AppendAuthorityLane::ExactSuccessorGrant
    } else {
        anyhow::ensure!(
            current_placement.as_deref() == Some(chain_root_id.as_str()),
            "handoff chain points at an unexpected placement: {current_placement:?}"
        );
        AppendAuthorityLane::SourcePlacement
    };
    let source_thread = source
        .get_thread(chain_root_id)?
        .context("source placement disappeared")?;
    let source_placement =
        if source_thread.status == ryeos_state::objects::ThreadStatus::Continued.as_str() {
            SourcePlacementState::Fenced
        } else {
            anyhow::ensure!(
                source_thread.status == ryeos_state::objects::ThreadStatus::Running.as_str(),
                "source placement has unexpected status `{}`",
                source_thread.status
            );
            let abort_recorded = source
                .latest_thread_events(chain_root_id, 64)?
                .iter()
                .any(|event| event.event_type == "worker_session.handoff_aborted");
            if abort_recorded {
                SourcePlacementState::AbortRecordedCurrentWriter
            } else {
                SourcePlacementState::CurrentWriter
            }
        };

    let reservation = target.credential_profile_reservation_for_successor(successor_id)?;
    let target_credential = match reservation.as_ref().map(|record| record.state.as_str()) {
        None => CredentialDisposition::NotReserved,
        Some("reserved") => CredentialDisposition::Reserved,
        Some("released") => CredentialDisposition::Released,
        Some("consumed") => CredentialDisposition::Consumed,
        Some(other) => anyhow::bail!("unknown credential reservation state `{other}`"),
    };
    let target_thread = target.get_thread(successor_id)?;
    let session = target.dedicated_session(successor_id)?;
    let live_workers = target
        .live_worker_processes()?
        .into_iter()
        .filter(|worker| worker.placement_thread_id == successor_id)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        live_workers.len() <= 1,
        "handoff retained duplicate target process authorities"
    );
    let process = if let Some(session) = &session {
        if let Some(worker_id) = session.worker_instance_id.as_deref() {
            let worker = target
                .worker_process(worker_id)?
                .context("attached session lost its worker process")?;
            anyhow::ensure!(
                worker.placement_thread_id == successor_id
                    && worker.boot_epoch
                        == session
                            .worker_boot_epoch
                            .context("attached session lost boot epoch")?
                    && worker.state == ryeos_app::runtime_db::WorkerProcessState::Live
                    && worker.cleanup_state == "owned",
                "target process authority is not exact: {worker:?}"
            );
            ProcessDisposition::ExactTargetAttached
        } else {
            ProcessDisposition::None
        }
    } else {
        anyhow::ensure!(
            live_workers.is_empty(),
            "unattached successor retained a live worker"
        );
        ProcessDisposition::None
    };
    let successor_placement = if process == ProcessDisposition::ExactTargetAttached {
        SuccessorPlacementState::Attached
    } else if target_thread.is_some() {
        SuccessorPlacementState::AdoptedUnattached
    } else if append_authority == AppendAuthorityLane::ExactSuccessorGrant {
        SuccessorPlacementState::AuthorizedUnadopted
    } else if reservation
        .as_ref()
        .is_some_and(|reservation| matches!(reservation.state.as_str(), "reserved" | "consumed"))
    {
        SuccessorPlacementState::PreparedOnly
    } else {
        SuccessorPlacementState::Absent
    };
    let workspace = if session.is_some() {
        WorkspaceDisposition::Attached
    } else if target_job
        .as_ref()
        .is_some_and(|job| job.state == SyncJobState::Running && job.phase == "placement_admission")
    {
        WorkspaceDisposition::EphemeralPreparation
    } else if target_job.as_ref().is_some_and(|job| {
        !matches!(job.state, SyncJobState::Cancelled | SyncJobState::Failed) && job.result.is_some()
    }) {
        WorkspaceDisposition::PreparedReconstructible
    } else {
        WorkspaceDisposition::None
    };
    let portable_state = if portable_fixture_state_is_installed(&handoff.target.state_path)? {
        PortableStateDisposition::Installed
    } else {
        PortableStateDisposition::Absent
    };

    Ok(HandoffDurableSnapshot {
        source_phase: source_observed.phase,
        target_phase: target_observed.phase,
        source_job: source_observed.expectation,
        target_job: target_observed.expectation,
        append_authority,
        head_signer,
        source_placement,
        successor_placement,
        target_credential,
        source_staging_roots: source_observed.staging_roots,
        target_staging_roots: target_observed.staging_roots,
        portable_state,
        workspace,
        process,
    })
}

fn assert_real_handoff_snapshot(
    handoff: &RealPortableHandoff,
    successor_id: &str,
    expected: HandoffDurableSnapshot,
    point: &str,
) -> Result<()> {
    let observed = observe_real_handoff_snapshot(handoff, successor_id)?;
    anyhow::ensure!(
        observed == expected,
        "handoff {point} snapshot differs from the executable oracle\nexpected: {expected:#?}\nobserved: {observed:#?}"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct HandoffAttemptKey {
    role: &'static str,
    attempt_id: String,
    attempt_number: u64,
    worker_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HandoffAttemptObservation {
    state: String,
    phase: String,
    error: Option<String>,
    result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HandoffRetryJobEvidence {
    job_id: String,
    operation: WorkerSessionHandoffJobOperation,
    state: String,
    phase: String,
    roots: Vec<String>,
    heads: Vec<String>,
    attempt_count: u64,
    attempts: BTreeMap<HandoffAttemptKey, HandoffAttemptObservation>,
    result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CredentialReservationCoordinate {
    reservation_id: String,
    operation_id: String,
    successor_thread_id: String,
    profile_id: String,
    owner_principal: String,
    credential_generation: u64,
    subject_contract_digest: String,
    subject_digest: String,
    checkpoint_manifest_hash: String,
    upstream_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GenericHeadCoordinate {
    target_hash: String,
    signer: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HandoffRetryEvidence {
    source_job: Option<HandoffRetryJobEvidence>,
    target_job: Option<HandoffRetryJobEvidence>,
    writer_grant_hash: Option<String>,
    credential_reservation: Option<CredentialReservationCoordinate>,
    adoption_receipt: Option<WorkerHandoffAdoptionReceiptEvidence>,
    abort_fence: Option<WorkerHandoffAbortFenceEvidence>,
    terminal_failure: Option<WorkerHandoffTerminalFailureEvidence>,
    target_terminal_attestation_hash: Option<String>,
    source_chain_head: GenericHeadCoordinate,
    target_chain_head: Option<GenericHeadCoordinate>,
    source_remote_continuation: Option<ryeos_state::objects::RemoteContinuationAuthority>,
    signed_placement: Option<WorkerPlacementAdmissionEvidence>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveSyncJobInspectResponse {
    status: String,
    #[serde(default)]
    job_id: Option<String>,
    #[serde(default)]
    job: Option<LiveSyncJobRecord>,
    #[serde(default)]
    attempt_retention: Option<LiveSyncJobAttemptRetention>,
    #[serde(default)]
    attempts: Vec<LiveSyncJobAttemptRecord>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveSyncJobAttemptRetention {
    mode: String,
    cumulative_count: u64,
    retained_count: u64,
    terminal_row_limit: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveSyncJobRecord {
    job_id: String,
    operation_type: String,
    operation: serde_json::Value,
    peer: Option<String>,
    state: String,
    phase: String,
    roots: Vec<String>,
    heads: Vec<String>,
    uploaded_hashes: Vec<String>,
    fetched_hashes: Vec<String>,
    attempt_count: u64,
    max_attempts: u64,
    last_error: Option<String>,
    result: Option<serde_json::Value>,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveSyncJobAttemptRecord {
    attempt_id: String,
    job_id: String,
    attempt_number: u64,
    worker_id: Option<String>,
    state: String,
    phase: String,
    started_at: String,
    updated_at: String,
    finished_at: Option<String>,
    error: Option<String>,
    result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy)]
enum RetryObservationMode {
    Offline,
    Live,
}

fn validate_retained_attempt_ledger(
    attempt_count: u64,
    max_attempts: u64,
    observed_numbers: &[u64],
    running_attempt_numbers: &[u64],
) -> Result<()> {
    anyhow::ensure!(
        running_attempt_numbers.len() <= 1
            && running_attempt_numbers
                .first()
                .is_none_or(|number| *number == attempt_count),
        "sync job retained another running-attempt coordinate"
    );
    if max_attempts == ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS {
        let running_count = u64::try_from(running_attempt_numbers.len())?;
        let terminal_count = u64::try_from(observed_numbers.len())?
            .checked_sub(running_count)
            .context("retained sync-job attempt counts underflowed")?;
        let expected_terminal_count = attempt_count
            .checked_sub(running_count)
            .context("cumulative sync-job attempt counts underflowed")?
            .min(SYNC_JOB_UNBOUNDED_RETAINED_TERMINAL_ATTEMPTS);
        anyhow::ensure!(
            terminal_count == expected_terminal_count
                && u64::try_from(observed_numbers.len())?
                    == expected_terminal_count + running_count,
            "unbounded sync job did not retain its exact bounded diagnostic suffix"
        );
        if let Some((&first, &last)) = observed_numbers.first().zip(observed_numbers.last()) {
            let retained_len = u64::try_from(observed_numbers.len())?;
            let expected_first = attempt_count
                .checked_sub(retained_len)
                .and_then(|value| value.checked_add(1));
            anyhow::ensure!(
                last == attempt_count
                    && expected_first == Some(first)
                    && observed_numbers
                        .windows(2)
                        .all(|pair| pair[1] == pair[0] + 1),
                "unbounded sync job did not retain the exact newest attempt suffix: {observed_numbers:?}"
            );
        } else {
            anyhow::ensure!(
                attempt_count == 0,
                "unbounded sync job lost every retained attempt diagnostic"
            );
        }
    } else {
        anyhow::ensure!(
            observed_numbers
                .iter()
                .copied()
                .eq(1..=u64::try_from(observed_numbers.len())?)
                && attempt_count == u64::try_from(observed_numbers.len())?,
            "bounded sync job attempt counter differs from its complete ledger"
        );
    }
    Ok(())
}

fn retry_job_evidence(
    store: &StateStore,
    job: Option<ryeos_state::SyncJobRecord>,
) -> Result<Option<HandoffRetryJobEvidence>> {
    let Some(job) = job else {
        return Ok(None);
    };
    let operation = WorkerSessionHandoffJobOperation::from_value(job.operation)?;
    let mut attempts = BTreeMap::new();
    for attempt in store.with_state_db(|db| db.list_sync_job_attempts(&job.job_id))? {
        let key = HandoffAttemptKey {
            role: match operation.role {
                WorkerHandoffJobRole::Source => "source",
                WorkerHandoffJobRole::Target => "target",
            },
            attempt_id: attempt.attempt_id,
            attempt_number: attempt.attempt_number,
            worker_id: attempt.worker_id,
        };
        let observation = HandoffAttemptObservation {
            state: attempt.state.as_str().to_owned(),
            phase: attempt.phase,
            error: attempt.error,
            result: attempt.result,
        };
        anyhow::ensure!(
            attempts.insert(key, observation).is_none(),
            "handoff job retained a duplicate attempt coordinate"
        );
    }
    let mut observed_numbers = attempts
        .keys()
        .map(|attempt| attempt.attempt_number)
        .collect::<Vec<_>>();
    observed_numbers.sort_unstable();
    let running_attempt_numbers = attempts
        .iter()
        .filter(|(_, observation)| observation.state == "running")
        .map(|(attempt, _)| attempt.attempt_number)
        .collect::<Vec<_>>();
    validate_retained_attempt_ledger(
        job.attempt_count,
        job.max_attempts,
        &observed_numbers,
        &running_attempt_numbers,
    )?;
    Ok(Some(HandoffRetryJobEvidence {
        job_id: job.job_id,
        operation,
        state: job.state.as_str().to_owned(),
        phase: job.phase,
        roots: job.roots,
        heads: job.heads,
        attempt_count: job.attempt_count,
        attempts,
        result: job.result,
    }))
}

async fn live_retry_job_evidence(
    daemon: &DaemonHarness,
    job_id: &str,
    role: WorkerHandoffJobRole,
) -> Result<Option<HandoffRetryJobEvidence>> {
    let (status, response) = daemon
        .post_execute(
            "service:sync/jobs/inspect",
            ".",
            serde_json::json!({"job_id":job_id}),
        )
        .await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "live sync-job inspection failed for `{job_id}`: {status} {response}"
    );
    let inspected: LiveSyncJobInspectResponse = serde_json::from_value(
        response
            .get("result")
            .cloned()
            .context("live sync-job inspection returned no result")?,
    )?;
    match inspected.status.as_str() {
        "missing" => {
            anyhow::ensure!(
                inspected.job_id.as_deref() == Some(job_id)
                    && inspected.job.is_none()
                    && inspected.attempt_retention.is_none()
                    && inspected.attempts.is_empty(),
                "missing live sync-job response changed its requested coordinate"
            );
            Ok(None)
        }
        "found" => {
            anyhow::ensure!(
                inspected.job_id.is_none(),
                "found live sync-job response duplicated its coordinate"
            );
            let job = inspected
                .job
                .context("found live sync-job response retained no job")?;
            let retention = inspected
                .attempt_retention
                .context("found live sync-job response omitted attempt retention")?;
            anyhow::ensure!(
                job.job_id == job_id && job.operation_type == WORKER_SESSION_HANDOFF_OPERATION,
                "live sync-job inspection returned another durable operation"
            );
            let operation = WorkerSessionHandoffJobOperation::from_value(job.operation)?;
            anyhow::ensure!(
                operation.role == role,
                "live sync-job inspection returned the opposite handoff role"
            );
            let role = match role {
                WorkerHandoffJobRole::Source => "source",
                WorkerHandoffJobRole::Target => "target",
            };
            let mut attempts = BTreeMap::new();
            for attempt in inspected.attempts {
                anyhow::ensure!(
                    attempt.job_id == job_id
                        && matches!(
                            attempt.state.as_str(),
                            "running" | "completed" | "failed" | "cancelled"
                        ),
                    "live sync-job inspection returned an invalid attempt"
                );
                let key = HandoffAttemptKey {
                    role,
                    attempt_id: attempt.attempt_id,
                    attempt_number: attempt.attempt_number,
                    worker_id: attempt.worker_id,
                };
                let observation = HandoffAttemptObservation {
                    state: attempt.state,
                    phase: attempt.phase,
                    error: attempt.error,
                    result: attempt.result,
                };
                anyhow::ensure!(
                    attempts.insert(key, observation).is_none(),
                    "live sync-job inspection returned a duplicate attempt coordinate"
                );
            }
            let mut observed_numbers = attempts
                .keys()
                .map(|attempt| attempt.attempt_number)
                .collect::<Vec<_>>();
            observed_numbers.sort_unstable();
            let running_attempt_numbers = attempts
                .iter()
                .filter(|(_, observation)| observation.state == "running")
                .map(|(attempt, _)| attempt.attempt_number)
                .collect::<Vec<_>>();
            validate_retained_attempt_ledger(
                job.attempt_count,
                job.max_attempts,
                &observed_numbers,
                &running_attempt_numbers,
            )?;
            anyhow::ensure!(
                retention.cumulative_count == job.attempt_count
                    && retention.retained_count == u64::try_from(attempts.len())?
                    && if job.max_attempts == ryeos_state::SYNC_JOB_UNBOUNDED_ATTEMPTS {
                        retention.mode == "bounded_terminal_suffix"
                            && retention.terminal_row_limit
                                == Some(SYNC_JOB_UNBOUNDED_RETAINED_TERMINAL_ATTEMPTS)
                    } else {
                        retention.mode == "complete" && retention.terminal_row_limit.is_none()
                    },
                "live sync-job attempt-retention testimony disagrees with its ledger"
            );
            anyhow::ensure!(
                matches!(
                    job.state.as_str(),
                    "planned" | "running" | "completed" | "failed" | "retryable" | "cancelled"
                ),
                "live sync-job inspection returned unknown state `{}`",
                job.state
            );
            Ok(Some(HandoffRetryJobEvidence {
                job_id: job.job_id,
                operation,
                state: job.state,
                phase: job.phase,
                roots: job.roots,
                heads: job.heads,
                attempt_count: job.attempt_count,
                attempts,
                result: job.result,
            }))
        }
        other => anyhow::bail!("live sync-job inspection returned unknown status `{other}`"),
    }
}

fn progress_writer_grant(job: Option<&HandoffRetryJobEvidence>) -> Option<String> {
    job.and_then(|job| job.result.clone())
        .and_then(|value| WorkerSessionHandoffProgress::from_value(value).ok())
        .and_then(|progress| progress.writer_grant_hash)
}

fn progress_placement_attestation(job: Option<&HandoffRetryJobEvidence>) -> Option<String> {
    job.and_then(|job| job.result.clone())
        .and_then(|value| WorkerSessionHandoffProgress::from_value(value).ok())
        .and_then(|progress| progress.placement_attestation_hash)
}

fn verified_signed_placement(
    target: &StateStore,
    target_job: Option<&HandoffRetryJobEvidence>,
    adoption_receipt: Option<&WorkerHandoffAdoptionReceiptEvidence>,
    terminal_failure: Option<&WorkerHandoffTerminalFailureEvidence>,
    target_key: &SigningKey,
) -> Result<Option<WorkerPlacementAdmissionEvidence>> {
    let mut asserted_hashes = [
        progress_placement_attestation(target_job),
        adoption_receipt.map(|receipt| receipt.request.placement_attestation_hash.clone()),
        terminal_failure.map(|receipt| receipt.request.placement_attestation_hash.clone()),
    ]
    .into_iter()
    .flatten()
    .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        asserted_hashes.len() <= 1,
        "handoff evidence names divergent placement attestations: {asserted_hashes:?}"
    );
    let asserted_hash = asserted_hashes.pop_first();
    let Some(target_job) = target_job else {
        anyhow::ensure!(
            asserted_hash.is_none(),
            "placement attestation was asserted without a target handoff job"
        );
        return Ok(None);
    };

    let authority = target.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let mut found = Vec::new();
    for root in &target_job.roots {
        let Some(value) = cas.get_object(root)? else {
            continue;
        };
        let Ok(attestation) = ryeos_state::objects::Attestation::from_value(&value) else {
            continue;
        };
        if attestation.policy != ryeos_app::worker_handoff::WORKER_PLACEMENT_POLICY
            || attestation.claim != ryeos_app::worker_handoff::WORKER_PLACEMENT_CLAIM
        {
            continue;
        }
        anyhow::ensure!(
            ryeos_state::objects::canonical_value_digest(&value)? == *root,
            "signed placement root changed its canonical digest"
        );
        attestation.verify_with_key(&target_key.verifying_key())?;
        anyhow::ensure!(
            !attestation.is_expired_at(&lillux::time::iso8601_now())?,
            "signed placement expired during qualification"
        );
        let placement = WorkerPlacementAdmissionEvidence::from_attestation(&attestation)?;
        anyhow::ensure!(
            placement.operation_id == target_job.operation.operation_id,
            "signed placement belongs to another handoff operation"
        );
        found.push((root.clone(), placement));
    }
    drop(guard);
    anyhow::ensure!(
        found.len() <= 1,
        "target handoff roots retain multiple signed placement authorities"
    );
    let found = found.pop();
    if let Some(asserted_hash) = asserted_hash {
        anyhow::ensure!(
            found.as_ref().map(|(hash, _)| hash) == Some(&asserted_hash),
            "handoff progress or receipt names an unrooted signed placement"
        );
    }
    Ok(found.map(|(_, placement)| placement))
}

fn observe_target_handoff_branch(
    store: &StateStore,
    operation_id: &str,
) -> Result<(
    Option<String>,
    Option<WorkerHandoffAdoptionReceiptEvidence>,
    Option<WorkerHandoffAbortFenceEvidence>,
    Option<WorkerHandoffTerminalFailureEvidence>,
)> {
    let Some(head) = store.with_state_db(|db| {
        db.read_generic_head_ref(
            ryeos_app::worker_handoff::WORKER_HANDOFF_TARGET_BRANCH_HEAD_NAMESPACE,
            operation_id,
        )
    })?
    else {
        return Ok((None, None, None, None));
    };
    let authority = store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let value = authority
        .cas_store()?
        .get_object(&head.target_hash)?
        .context("target handoff branch head object is absent")?;
    let attestation = ryeos_state::objects::Attestation::from_value(&value)?;
    drop(guard);
    match (attestation.policy.as_str(), attestation.claim.as_str()) {
        (
            ryeos_app::worker_handoff::WORKER_HANDOFF_ADOPTION_RECEIPT_POLICY,
            ryeos_app::worker_handoff::WORKER_HANDOFF_ADOPTION_RECEIPT_CLAIM,
        ) => Ok((
            Some(head.target_hash),
            Some(
                store
                    .worker_handoff_adoption_receipt(operation_id)?
                    .context("classified adoption branch disappeared")?,
            ),
            None,
            None,
        )),
        (
            ryeos_app::worker_handoff::WORKER_HANDOFF_ABORT_FENCE_POLICY,
            ryeos_app::worker_handoff::WORKER_HANDOFF_ABORT_FENCE_CLAIM,
        ) => Ok((
            Some(head.target_hash),
            None,
            Some(
                store
                    .worker_handoff_abort_fence(operation_id)?
                    .context("classified abort branch disappeared")?,
            ),
            None,
        )),
        (
            ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_FAILURE_POLICY,
            ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_FAILURE_CLAIM,
        ) => Ok((
            Some(head.target_hash),
            None,
            None,
            Some(
                store
                    .worker_handoff_terminal_failure(operation_id)?
                    .context("classified terminal-failure branch disappeared")?,
            ),
        )),
        _ => anyhow::bail!(
            "target handoff branch has an unknown or contradictory signed evidence type"
        ),
    }
}

async fn observe_handoff_retry_evidence(
    handoff: &RealPortableHandoff,
    successor_id: &str,
    operation_id: &str,
    mode: RetryObservationMode,
) -> Result<HandoffRetryEvidence> {
    let trusted = [&handoff.source_fixture.node, &handoff.target_fixture.node];
    let chain_root_id = &handoff.checkpoint.chain_root_id;
    let (source, target, source_job, target_job) = match mode {
        RetryObservationMode::Offline => {
            let source =
                open_daemon_state_with_trusted_nodes(&handoff.source.state_path, &trusted)?;
            let target =
                open_daemon_state_with_trusted_nodes(&handoff.target.state_path, &trusted)?;
            let source_job = retry_job_evidence(
                &source,
                find_handoff_job(&source, chain_root_id, WorkerHandoffJobRole::Source)?,
            )?;
            let target_job = retry_job_evidence(
                &target,
                find_handoff_job(&target, chain_root_id, WorkerHandoffJobRole::Target)?,
            )?;
            (source, target, source_job, target_job)
        }
        RetryObservationMode::Live => {
            let source = open_daemon_projection_state_with_trusted_nodes(
                &handoff.source.state_path,
                &trusted,
            )?;
            let target = open_daemon_projection_state_with_trusted_nodes(
                &handoff.target.state_path,
                &trusted,
            )?;
            let source_id = format!("worker-handoff-source:{operation_id}");
            let target_id = format!("worker-handoff-target:{operation_id}");
            let source_job =
                live_retry_job_evidence(&handoff.source, &source_id, WorkerHandoffJobRole::Source)
                    .await?;
            let target_job =
                live_retry_job_evidence(&handoff.target, &target_id, WorkerHandoffJobRole::Target)
                    .await?;
            (source, target, source_job, target_job)
        }
    };
    if let (Some(source_job), Some(target_job)) = (&source_job, &target_job) {
        anyhow::ensure!(
            source_job
                .operation
                .target_projection(target_job.operation.peer_remote_name.clone())?
                == target_job.operation,
            "retry evidence found divergent source and target operations"
        );
    }
    let observed_operation_id = source_job
        .as_ref()
        .or(target_job.as_ref())
        .map(|job| job.operation.operation_id.as_str());
    anyhow::ensure!(
        observed_operation_id.is_none_or(|observed| observed == operation_id),
        "retry observation returned another handoff operation"
    );
    let (target_terminal_attestation_hash, adoption_receipt, abort_fence, terminal_failure) =
        observed_operation_id
            .map(|observed| observe_target_handoff_branch(&target, observed))
            .transpose()?
            .unwrap_or((None, None, None, None));
    let signed_placement = verified_signed_placement(
        &target,
        target_job.as_ref(),
        adoption_receipt.as_ref(),
        terminal_failure.as_ref(),
        &handoff.target_fixture.node,
    )?;
    let mut writer_grants = [
        progress_writer_grant(source_job.as_ref()),
        progress_writer_grant(target_job.as_ref()),
        adoption_receipt
            .as_ref()
            .map(|receipt| receipt.request.writer_grant_hash.clone()),
        terminal_failure
            .as_ref()
            .map(|receipt| receipt.request.writer_grant_hash.clone()),
    ]
    .into_iter()
    .flatten()
    .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        writer_grants.len() <= 1,
        "retry evidence retained divergent writer-grant coordinates: {writer_grants:?}"
    );
    let writer_grant_hash = writer_grants.pop_first();
    let credential_reservation = signed_placement.as_ref().map(|placement| {
        let reservation = &placement.credential_reservation;
        CredentialReservationCoordinate {
            reservation_id: reservation.reservation_id.clone(),
            operation_id: placement.operation_id.clone(),
            successor_thread_id: placement.successor_placement_thread_id.clone(),
            profile_id: reservation.profile_id.clone(),
            owner_principal: reservation.owner_principal.clone(),
            credential_generation: reservation.generation,
            subject_contract_digest: reservation.subject_contract_digest.clone(),
            subject_digest: reservation.subject_digest.clone(),
            checkpoint_manifest_hash: placement.checkpoint_manifest_hash.clone(),
            upstream_session_id: reservation.upstream_session_id.clone(),
        }
    });
    let source_chain_head = source
        .with_state_db(|db| db.read_generic_head_ref("chains", chain_root_id))?
        .context("source retry evidence found no authoritative chain head")?;
    let target_chain_head = target
        .with_state_db(|db| db.read_generic_head_ref("chains", chain_root_id))?
        .map(|head| GenericHeadCoordinate {
            target_hash: head.target_hash,
            signer: head.signer,
        });
    if let (Some(receipt), Some(head)) = (&adoption_receipt, &target_chain_head) {
        let authority = target.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        ryeos_state::sync::verify_chain_closure_anchored_pinned(
            &authority.cas_store()?,
            chain_root_id,
            &head.target_hash,
            &receipt.request.target_chain_head_hash,
        )?;
        drop(guard);
    }
    let successor_exists = source
        .with_state_db(|db| db.read_authoritative_thread_snapshot(chain_root_id, successor_id))?
        .is_some();
    let source_remote_continuation = if successor_exists {
        source.remote_continuation_authority(chain_root_id, successor_id)?
    } else {
        None
    };
    Ok(HandoffRetryEvidence {
        source_job,
        target_job,
        writer_grant_hash,
        credential_reservation,
        adoption_receipt,
        abort_fence,
        terminal_failure,
        target_terminal_attestation_hash,
        source_chain_head: GenericHeadCoordinate {
            target_hash: source_chain_head.target_hash,
            signer: source_chain_head.signer,
        },
        target_chain_head,
        source_remote_continuation,
        signed_placement,
    })
}

fn assert_canonical_source_operation(operation: &WorkerSessionHandoffJobOperation) -> Result<()> {
    anyhow::ensure!(
        operation.role == WorkerHandoffJobRole::Source,
        "retry oracle expected a source-role operation"
    );
    let expected = ryeos_state::objects::canonical_value_digest(&serde_json::json!({
        "schema":"ryeos.worker_session_handoff_operation.v1",
        "preflight_id":operation.preflight_id,
        "preflight_attestation_hash":operation.preflight_attestation_hash,
        "owner_principal":operation.owner_principal,
        "chain_root_id":operation.chain_root_id,
        "source_site_id":operation.source_site_id,
        "target_site_id":operation.target_site_id,
        "source_placement_thread_id":operation.source_placement_thread_id,
        "source_chain_head_hash":operation.source_chain_head_hash,
        "source_last_event_hash":operation.source_last_event_hash,
        "checkpoint_manifest_hash":operation.checkpoint_manifest_hash,
        "project_route_digest":operation.project_route_digest,
        "target_credential_profile_id":operation.target_credential_profile_id,
        "follow_delivery_reservation_attestation_hash":operation.follow_delivery_reservation_attestation_hash,
    }))?;
    anyhow::ensure!(
        operation.operation_id == expected,
        "handoff retry used a replacement operation identity: expected {expected}, observed {}",
        operation.operation_id
    );
    Ok(())
}

fn combined_attempts(
    evidence: &HandoffRetryEvidence,
) -> Result<BTreeMap<HandoffAttemptKey, HandoffAttemptObservation>> {
    let mut attempts = BTreeMap::new();
    for (key, observation) in evidence
        .source_job
        .iter()
        .chain(evidence.target_job.iter())
        .flat_map(|job| job.attempts.iter())
    {
        anyhow::ensure!(
            attempts.insert(key.clone(), observation.clone()).is_none(),
            "source and target reused one handoff attempt identity"
        );
    }
    let unique_ids = attempts
        .keys()
        .map(|attempt| attempt.attempt_id.as_str())
        .collect::<BTreeSet<_>>();
    anyhow::ensure!(
        unique_ids.len() == attempts.len(),
        "source and target reused one handoff attempt id"
    );
    Ok(attempts)
}

fn assert_attempt_ledger_transition(
    point: &str,
    previous: &BTreeMap<HandoffAttemptKey, HandoffAttemptObservation>,
    current: &BTreeMap<HandoffAttemptKey, HandoffAttemptObservation>,
) -> Result<()> {
    for (key, prior) in previous {
        let observed = current
            .get(key)
            .with_context(|| format!("{point} removed handoff attempt {key:?}"))?;
        if prior.state == "running" {
            let expected = HandoffAttemptObservation {
                state: "failed".to_owned(),
                phase: prior.phase.clone(),
                error: Some("daemon restarted before this attempt settled".to_owned()),
                result: prior.result.clone(),
            };
            anyhow::ensure!(
                *observed == expected,
                "{point} did not settle interrupted attempt {key:?} through the exact restart transition\nexpected: {expected:#?}\nobserved: {observed:#?}"
            );
        } else {
            anyhow::ensure!(
                observed == prior,
                "{point} rewrote terminal attempt {key:?}\nprior: {prior:#?}\nobserved: {observed:#?}"
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct HandoffContactLane {
    role: &'static str,
    worker_id: String,
    state: String,
    phase: String,
}

fn contact_lane(
    role: &'static str,
    worker_id: &'static str,
    state: &'static str,
    phase: &'static str,
) -> HandoffContactLane {
    HandoffContactLane {
        role,
        worker_id: worker_id.to_owned(),
        state: state.to_owned(),
        phase: phase.to_owned(),
    }
}

fn new_contact_lanes(
    previous: &BTreeMap<HandoffAttemptKey, HandoffAttemptObservation>,
    current: &BTreeMap<HandoffAttemptKey, HandoffAttemptObservation>,
) -> Result<BTreeMap<HandoffContactLane, usize>> {
    let mut lanes = BTreeMap::new();
    for (key, observation) in current {
        if previous.contains_key(key) {
            continue;
        }
        let worker_id = key
            .worker_id
            .clone()
            .context("handoff contact attempt omitted its worker identity")?;
        anyhow::ensure!(
            observation.state != "running" && observation.error.is_none(),
            "new handoff contact did not reach one clean terminal lane: {key:?} {observation:#?}"
        );
        *lanes
            .entry(HandoffContactLane {
                role: key.role,
                worker_id,
                state: observation.state.clone(),
                phase: observation.phase.clone(),
            })
            .or_default() += 1;
    }
    Ok(lanes)
}

fn expected_contact_lanes(
    lanes: impl IntoIterator<Item = HandoffContactLane>,
) -> BTreeMap<HandoffContactLane, usize> {
    let mut expected = BTreeMap::new();
    for lane in lanes {
        *expected.entry(lane).or_default() += 1;
    }
    expected
}

fn assert_job_operation_continuity(
    point: &str,
    previous: Option<&HandoffRetryJobEvidence>,
    current: Option<&HandoffRetryJobEvidence>,
) -> Result<()> {
    if let Some(previous) = previous {
        let current = current.with_context(|| format!("{point} removed a durable handoff job"))?;
        anyhow::ensure!(
            previous.job_id == current.job_id && previous.operation == current.operation,
            "{point} replaced a durable handoff operation"
        );
        let previous_roots = previous.roots.iter().collect::<BTreeSet<_>>();
        let current_roots = current.roots.iter().collect::<BTreeSet<_>>();
        anyhow::ensure!(
            previous_roots.is_subset(&current_roots),
            "{point} dropped a durable handoff GC root"
        );
    }
    Ok(())
}

fn assert_abort_fence_transition(
    point: &str,
    previous: Option<&WorkerHandoffAbortFenceEvidence>,
    current: Option<&WorkerHandoffAbortFenceEvidence>,
) -> Result<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let current = current.with_context(|| format!("{point} removed the target abort fence"))?;
    anyhow::ensure!(
        previous.schema == current.schema
            && previous.target_operation == current.target_operation
            && previous.abort_chain_head_hash == current.abort_chain_head_hash,
        "{point} changed the target abort-fence authority coordinates"
    );
    match previous.terminal_disposition.as_deref() {
        Some(disposition) => anyhow::ensure!(
            current.terminal_disposition.as_deref() == Some(disposition),
            "{point} changed an already-terminal abort disposition"
        ),
        None => anyhow::ensure!(
            current
                .terminal_disposition
                .as_deref()
                .is_some_and(|value| matches!(value, "reservation_released" | "target_absent")),
            "{point} did not monotonically promote the provisional abort fence"
        ),
    }
    Ok(())
}

fn assert_adoption_response_matches_receipt(
    response: &serde_json::Value,
    receipt: &WorkerHandoffAdoptionReceiptEvidence,
) -> Result<()> {
    let result = response
        .get("result")
        .context("successful handoff response omitted its result")?;
    anyhow::ensure!(
        result
            .get("operation_id")
            .and_then(serde_json::Value::as_str)
            == Some(receipt.response.operation_id.as_str())
            && result
                .get("chain_root_id")
                .and_then(serde_json::Value::as_str)
                == Some(receipt.response.chain_root_id.as_str())
            && result
                .get("placement_thread_id")
                .and_then(serde_json::Value::as_str)
                == Some(receipt.response.placement_thread_id.as_str())
            && result
                .get("target_chain_head_hash")
                .and_then(serde_json::Value::as_str)
                == Some(receipt.response.target_chain_head_hash.as_str())
            && result.get("delivery").and_then(serde_json::Value::as_str)
                == Some(receipt.response.delivery.as_str()),
        "handoff response did not replay the exact durable adoption receipt: {response}"
    );
    Ok(())
}

fn assert_exact_adoption_authority(
    evidence: &HandoffRetryEvidence,
    handoff: &RealPortableHandoff,
    source: &HandoffRetryJobEvidence,
    target: &HandoffRetryJobEvidence,
    receipt: &WorkerHandoffAdoptionReceiptEvidence,
) -> Result<()> {
    receipt.validate()?;
    let expected_target = source
        .operation
        .target_projection(PORTABLE_SOURCE_REMOTE.to_owned())?;
    anyhow::ensure!(
        target.operation == expected_target && receipt.target_operation == target.operation,
        "adoption receipt did not retain the exact projected target operation"
    );
    let remote = evidence
        .source_remote_continuation
        .as_ref()
        .context("completed handoff retained no signed remote-continuation authority")?;
    let placement = evidence
        .signed_placement
        .as_ref()
        .context("completed handoff retained no verified signed placement")?;
    let reservation = evidence
        .credential_reservation
        .as_ref()
        .context("completed handoff retained no credential reservation coordinate")?;
    let signed_credential = &placement.credential_reservation;
    let target_chain_head = evidence
        .target_chain_head
        .as_ref()
        .context("completed handoff retained no target-site chain head")?;
    let terminal_attestation_hash = evidence
        .target_terminal_attestation_hash
        .as_ref()
        .context("completed handoff retained no target terminal attestation hash")?;
    anyhow::ensure!(
        remote.operation_id == source.operation.operation_id
            && remote.preflight_id == source.operation.preflight_id
            && remote.preflight_attestation_hash == source.operation.preflight_attestation_hash
            && remote.follow_delivery_reservation_attestation_hash
                == source
                    .operation
                    .follow_delivery_reservation_attestation_hash
            && remote.source_chain_head_hash == source.operation.source_chain_head_hash
            && remote.source_last_event_hash == source.operation.source_last_event_hash
            && remote.checkpoint_manifest_hash == source.operation.checkpoint_manifest_hash
            && remote.target_placement_attestation_hash
                == receipt.request.placement_attestation_hash
            && remote.chain_writer_grant_hash == receipt.request.writer_grant_hash
            && remote.source_site_id == source.operation.source_site_id
            && remote.target_site_id == source.operation.target_site_id
            && remote.successor_thread_id == source.operation.successor_placement_thread_id,
        "remote-continuation authority differs from the source operation: {remote:#?}"
    );
    anyhow::ensure!(
        evidence.source_chain_head.target_hash == receipt.request.target_chain_head_hash
            && evidence.source_chain_head.signer == handoff.source_fixture.node_fp()
            && target_chain_head.signer == handoff.target_fixture.node_fp()
            && remote.target_node_signer_fingerprint == handoff.target_fixture.node_fp()
            && evidence.writer_grant_hash.as_deref()
                == Some(receipt.request.writer_grant_hash.as_str())
            && source.heads == [receipt.request.target_chain_head_hash.clone()]
            && source
                .roots
                .iter()
                .any(|root| root == terminal_attestation_hash)
            && target.heads == [receipt.request.target_chain_head_hash.clone()]
            && target
                .roots
                .iter()
                .any(|root| root == &receipt.request.placement_attestation_hash)
            && target
                .roots
                .iter()
                .any(|root| root == &receipt.request.writer_grant_hash)
            && target
                .roots
                .iter()
                .any(|root| root == &receipt.request.target_chain_head_hash),
        "adoption receipt is not the exact signed head/job authority: source_head={:#?} target_head={:#?} expected_target_signer={} observed_writer_grant={:?} source_heads={:#?} target_roots={:#?} target_heads={:#?} request={:#?}",
        evidence.source_chain_head,
        target_chain_head,
        remote.target_node_signer_fingerprint,
        evidence.writer_grant_hash,
        source.heads,
        target.roots,
        target.heads,
        receipt.request,
    );
    anyhow::ensure!(
        placement.operation_id == source.operation.operation_id
            && placement.preflight_id == source.operation.preflight_id
            && placement.preflight_attestation_hash == source.operation.preflight_attestation_hash
            && placement.follow_delivery_reservation_attestation_hash
                == source
                    .operation
                    .follow_delivery_reservation_attestation_hash
            && placement.owner_principal == source.operation.owner_principal
            && placement.chain_root_id == source.operation.chain_root_id
            && placement.origin_site_id == source.operation.origin_site_id
            && placement.source_site_id == source.operation.source_site_id
            && placement.target_site_id == source.operation.target_site_id
            && placement.source_placement_thread_id == source.operation.source_placement_thread_id
            && placement.successor_placement_thread_id
                == source.operation.successor_placement_thread_id
            && placement.source_chain_head_hash == source.operation.source_chain_head_hash
            && placement.source_last_event_hash == source.operation.source_last_event_hash
            && placement.checkpoint_manifest_hash == source.operation.checkpoint_manifest_hash,
        "signed placement differs from the source operation: placement={placement:#?} source={:#?}",
        source.operation,
    );
    anyhow::ensure!(
        reservation.reservation_id == signed_credential.reservation_id
            && reservation.operation_id == source.operation.operation_id
            && reservation.successor_thread_id == source.operation.successor_placement_thread_id
            && reservation.profile_id == source.operation.target_credential_profile_id
            && reservation.profile_id == signed_credential.profile_id
            && reservation.owner_principal == source.operation.owner_principal
            && reservation.owner_principal == signed_credential.owner_principal
            && reservation.credential_generation == signed_credential.generation
            && reservation.subject_contract_digest == signed_credential.subject_contract_digest
            && reservation.subject_digest == signed_credential.subject_digest
            && reservation.checkpoint_manifest_hash == source.operation.checkpoint_manifest_hash
            && reservation.upstream_session_id == signed_credential.upstream_session_id,
        "credential coordinate differs from the signed placement: coordinate={reservation:#?} signed={signed_credential:#?}"
    );
    let response = serde_json::to_value(&receipt.response)?;
    anyhow::ensure!(
        source.state == "completed"
            && source.phase == "completed"
            && source.result.as_ref() == Some(&response)
            && target.state == "completed"
            && target.phase == "completed"
            && target.result.as_ref() == Some(&response),
        "completed source/target jobs do not fold the exact adoption receipt response"
    );
    Ok(())
}

fn assert_exact_abort_authority(
    case: &HandoffAcceptanceCase,
    evidence: &HandoffRetryEvidence,
    handoff: &RealPortableHandoff,
    source: &HandoffRetryJobEvidence,
    target: &HandoffRetryJobEvidence,
    fence: &WorkerHandoffAbortFenceEvidence,
) -> Result<()> {
    fence.validate()?;
    let expected_target = source
        .operation
        .target_projection(PORTABLE_SOURCE_REMOTE.to_owned())?;
    let disposition = fence
        .terminal_disposition
        .as_deref()
        .context("terminal abort fence omitted its disposition")?;
    let expected_disposition =
        if case.at_cut.target_credential == CredentialDisposition::NotReserved {
            "target_absent"
        } else {
            "reservation_released"
        };
    let terminal_attestation_hash = evidence
        .target_terminal_attestation_hash
        .as_ref()
        .context("aborted handoff retained no target terminal attestation hash")?;
    let response = WorkerPlacementAbortResponse {
        operation_id: source.operation.operation_id.clone(),
        chain_root_id: source.operation.chain_root_id.clone(),
        disposition: disposition.to_owned(),
    };
    let request = WorkerPlacementAbortRequest {
        operation: source.operation.clone(),
        abort_chain_head_hash: fence.abort_chain_head_hash.clone(),
    };
    response.validate_against(&request)?;
    let result = serde_json::to_value(&response)?;
    anyhow::ensure!(
        target.operation == expected_target
            && fence.target_operation == target.operation
            && disposition == expected_disposition
            && evidence.source_remote_continuation.is_none()
            && evidence.source_chain_head.target_hash == fence.abort_chain_head_hash
            && evidence.source_chain_head.signer == handoff.source_fixture.node_fp()
            && source.heads == [fence.abort_chain_head_hash.clone()]
            && source
                .roots
                .iter()
                .any(|root| root == terminal_attestation_hash)
            && target.heads == [fence.abort_chain_head_hash.clone()]
            && target
                .roots
                .iter()
                .any(|root| root == &fence.abort_chain_head_hash)
            && source.state == "cancelled"
            && source.phase == "aborted"
            && source.result.as_ref() == Some(&result)
            && target.state == "cancelled"
            && target.phase == "aborted"
            && target.result.as_ref() == Some(&result),
        "source/target jobs do not fold the exact terminal abort authority"
    );
    Ok(())
}

fn assert_retry_disposition(
    case: &HandoffAcceptanceCase,
    at_cut: &HandoffRetryEvidence,
    before_retry: &HandoffRetryEvidence,
    after_retry: &HandoffRetryEvidence,
    status: reqwest::StatusCode,
    response: &serde_json::Value,
    cut_evidence: &HandoffPhaseCutEvidence,
    handoff: &RealPortableHandoff,
    expected_preflight_id: &str,
    expected_successor_id: &str,
) -> Result<()> {
    let final_source = after_retry
        .source_job
        .as_ref()
        .context("handoff retry retained no source operation")?;
    let final_target = after_retry
        .target_job
        .as_ref()
        .context("handoff retry retained no target operation")?;
    assert_canonical_source_operation(&final_source.operation)?;
    let cut_operation = match case.interrupted_node {
        HandoffNode::Source => &final_source.operation,
        HandoffNode::Target => &final_target.operation,
    };
    let cut_operation_digest =
        ryeos_state::objects::canonical_value_digest(&cut_operation.to_value()?)?;
    anyhow::ensure!(
        cut_evidence.operation_id == final_source.operation.operation_id
            && cut_evidence.operation_digest == cut_operation_digest
            && final_source.operation.preflight_id == expected_preflight_id
            && final_source.operation.chain_root_id == handoff.checkpoint.chain_root_id
            && final_source.operation.origin_site_id == PORTABLE_SOURCE_SITE_ID
            && final_source.operation.source_site_id == PORTABLE_SOURCE_SITE_ID
            && final_source.operation.target_site_id == PORTABLE_TARGET_SITE_ID
            && final_source.operation.successor_placement_thread_id == expected_successor_id
            && final_source.operation.target_credential_profile_id
                == PORTABLE_CREDENTIAL_PROFILE_ID
            && final_source.operation.peer_remote_name == PORTABLE_TARGET_REMOTE,
        "handoff retry operation differs from the exact live request and fixture coordinates"
    );
    anyhow::ensure!(
        final_source.job_id == format!("worker-handoff-source:{}", cut_evidence.operation_id)
            && final_target.job_id
                == format!("worker-handoff-target:{}", cut_evidence.operation_id)
            && final_source
                .operation
                .target_projection(PORTABLE_SOURCE_REMOTE.to_owned())?
                == final_target.operation,
        "handoff retry moved or changed the exact projected source/target operation"
    );

    assert_job_operation_continuity(
        "startup recovery",
        at_cut.source_job.as_ref(),
        before_retry.source_job.as_ref(),
    )?;
    assert_job_operation_continuity(
        "startup recovery",
        at_cut.target_job.as_ref(),
        before_retry.target_job.as_ref(),
    )?;
    assert_job_operation_continuity(
        "explicit retry",
        before_retry.source_job.as_ref(),
        after_retry.source_job.as_ref(),
    )?;
    assert_job_operation_continuity(
        "explicit retry",
        before_retry.target_job.as_ref(),
        after_retry.target_job.as_ref(),
    )?;

    let cut_attempts = combined_attempts(at_cut)?;
    let before_attempts = combined_attempts(before_retry)?;
    let after_attempts = combined_attempts(after_retry)?;
    assert_attempt_ledger_transition("startup recovery", &cut_attempts, &before_attempts)?;
    assert_attempt_ledger_transition("explicit retry", &before_attempts, &after_attempts)?;
    anyhow::ensure!(
        before_attempts
            .values()
            .chain(after_attempts.values())
            .all(|attempt| attempt.state != "running"),
        "retry oracle sampled an unsettled startup or explicit-retry contact"
    );

    let restart_lanes = new_contact_lanes(&cut_attempts, &before_attempts)?;
    let explicit_retry_lanes = new_contact_lanes(&before_attempts, &after_attempts)?;
    let expected_restart_lanes = match case.retry_disposition {
        RetryDisposition::ReusesExactOperation => expected_contact_lanes([]),
        RetryDisposition::RejectedByAbortAuthority => expected_contact_lanes([contact_lane(
            "source",
            "source-handoff-recovery",
            "cancelled",
            "aborted",
        )]),
        RetryDisposition::ResumesExactOperation => {
            let mut lanes = vec![contact_lane(
                "source",
                "source-handoff-recovery",
                "completed",
                "completed",
            )];
            // Once attachment itself was committed, target startup folds the
            // retained worker/session/profile transaction before clearing the
            // old daemon generation. No new worker-contact attempt is needed
            // to project or receipt that already-observed fact.
            if !matches!(
                case.boundary,
                HandoffCrashBoundary::TargetProcessAttachmentObserved
                    | HandoffCrashBoundary::TargetProcessAttachmentProjected
            ) {
                lanes.push(contact_lane(
                    "target",
                    "target-handoff-adopt",
                    "completed",
                    "completed",
                ));
            }
            expected_contact_lanes(lanes)
        }
        RetryDisposition::ResumesSourceFromTargetReceipt => expected_contact_lanes([contact_lane(
            "source",
            "source-handoff-recovery",
            "completed",
            "completed",
        )]),
        RetryDisposition::ObservesCompletedReceipt => expected_contact_lanes([]),
    };
    let expected_explicit_retry_lanes = match case.retry_disposition {
        RetryDisposition::ReusesExactOperation => expected_contact_lanes([
            contact_lane("source", "source-handoff", "completed", "target_prepared"),
            contact_lane(
                "target",
                "target-handoff-prepare",
                "completed",
                "target_prepared",
            ),
            contact_lane("source", "source-handoff", "completed", "completed"),
            contact_lane("target", "target-handoff-adopt", "completed", "completed"),
        ]),
        _ => expected_contact_lanes([]),
    };
    anyhow::ensure!(
        restart_lanes == expected_restart_lanes,
        "case `{}` used the wrong startup-recovery contact lanes\nexpected: {expected_restart_lanes:#?}\nobserved: {restart_lanes:#?}",
        case.case_id
    );
    anyhow::ensure!(
        explicit_retry_lanes == expected_explicit_retry_lanes,
        "case `{}` used the wrong explicit-retry contact lanes\nexpected: {expected_explicit_retry_lanes:#?}\nobserved: {explicit_retry_lanes:#?}",
        case.case_id
    );

    if let Some(cut_reservation) = &at_cut.credential_reservation {
        anyhow::ensure!(
            before_retry.credential_reservation.as_ref() == Some(cut_reservation)
                && after_retry.credential_reservation.as_ref() == Some(cut_reservation),
            "handoff retry changed the credential reservation coordinate after `{}`",
            case.case_id
        );
    } else if let Some(before_reservation) = &before_retry.credential_reservation {
        anyhow::ensure!(
            after_retry.credential_reservation.as_ref() == Some(before_reservation),
            "explicit retry changed the credential reservation coordinate after `{}`",
            case.case_id
        );
    }
    if let Some(cut_writer_grant) = &at_cut.writer_grant_hash {
        anyhow::ensure!(
            before_retry.writer_grant_hash.as_ref() == Some(cut_writer_grant)
                && after_retry.writer_grant_hash.as_ref() == Some(cut_writer_grant),
            "handoff retry changed the one-shot writer-grant coordinate after `{}`",
            case.case_id
        );
    } else if let Some(before_writer_grant) = &before_retry.writer_grant_hash {
        anyhow::ensure!(
            after_retry.writer_grant_hash.as_ref() == Some(before_writer_grant),
            "explicit retry changed the one-shot writer grant after `{}`",
            case.case_id
        );
    }
    if let Some(receipt) = &at_cut.adoption_receipt {
        anyhow::ensure!(
            before_retry.adoption_receipt.as_ref() == Some(receipt)
                && after_retry.adoption_receipt.as_ref() == Some(receipt),
            "restart or retry changed the permanent adoption receipt"
        );
    } else if let Some(receipt) = &before_retry.adoption_receipt {
        anyhow::ensure!(
            after_retry.adoption_receipt.as_ref() == Some(receipt),
            "explicit retry changed the permanent adoption receipt"
        );
    }
    assert_abort_fence_transition(
        "startup recovery",
        at_cut.abort_fence.as_ref(),
        before_retry.abort_fence.as_ref(),
    )?;
    assert_abort_fence_transition(
        "explicit retry",
        before_retry.abort_fence.as_ref(),
        after_retry.abort_fence.as_ref(),
    )?;

    match case.retry_disposition {
        RetryDisposition::ReusesExactOperation => {
            anyhow::ensure!(
                at_cut.source_job.is_none()
                    && at_cut.target_job.is_none()
                    && before_retry.source_job.is_none()
                    && before_retry.target_job.is_none()
                    && at_cut.adoption_receipt.is_none()
                    && before_retry.adoption_receipt.is_none(),
                "reuse case `{}` had already published a durable operation",
                case.case_id
            );
            anyhow::ensure!(
                status == reqwest::StatusCode::OK,
                "reuse case `{}` did not execute the canonical operation: {status} {response}",
                case.case_id
            );
            let receipt = after_retry
                .adoption_receipt
                .as_ref()
                .context("reuse case retained no adoption receipt")?;
            assert_exact_adoption_authority(
                after_retry,
                handoff,
                final_source,
                final_target,
                receipt,
            )?;
            assert_adoption_response_matches_receipt(response, receipt)?;
        }
        RetryDisposition::ResumesExactOperation => {
            anyhow::ensure!(
                at_cut.source_job.is_some()
                    && at_cut.adoption_receipt.is_none()
                    && at_cut.abort_fence.is_none()
                    && status == reqwest::StatusCode::OK,
                "resume case `{}` did not start from an incomplete durable operation",
                case.case_id
            );
            let receipt = after_retry
                .adoption_receipt
                .as_ref()
                .context("resumed operation retained no adoption receipt")?;
            assert_exact_adoption_authority(
                after_retry,
                handoff,
                final_source,
                final_target,
                receipt,
            )?;
            assert_adoption_response_matches_receipt(response, receipt)?;
        }
        RetryDisposition::ResumesSourceFromTargetReceipt => {
            let cut_receipt = at_cut
                .adoption_receipt
                .as_ref()
                .context("source-from-target-receipt case had no target receipt at its cut")?;
            anyhow::ensure!(
                at_cut.source_job.is_some()
                    && at_cut.abort_fence.is_none()
                    && status == reqwest::StatusCode::OK
                    && before_retry.adoption_receipt.as_ref() == Some(cut_receipt)
                    && after_retry.adoption_receipt.as_ref() == Some(cut_receipt),
                "source-from-target-receipt case `{}` did not fold the immutable target receipt",
                case.case_id
            );
            assert_exact_adoption_authority(
                after_retry,
                handoff,
                final_source,
                final_target,
                cut_receipt,
            )?;
            assert_adoption_response_matches_receipt(response, cut_receipt)?;
        }
        RetryDisposition::ObservesCompletedReceipt => {
            let cut_receipt = at_cut
                .adoption_receipt
                .as_ref()
                .context("receipt-observation case had no receipt at its crash cut")?;
            anyhow::ensure!(
                status == reqwest::StatusCode::OK
                    && before_retry.adoption_receipt.as_ref() == Some(cut_receipt)
                    && after_retry.adoption_receipt.as_ref() == Some(cut_receipt),
                "receipt-observation case `{}` contacted a worker or changed its receipt: {status} {response}",
                case.case_id
            );
            assert_exact_adoption_authority(
                after_retry,
                handoff,
                final_source,
                final_target,
                cut_receipt,
            )?;
            assert_adoption_response_matches_receipt(response, cut_receipt)?;
        }
        RetryDisposition::RejectedByAbortAuthority => {
            let abort_fence = after_retry
                .abort_fence
                .as_ref()
                .context("abort-rejection case retained no target abort fence")?;
            anyhow::ensure!(
                status == reqwest::StatusCode::CONFLICT && after_retry.adoption_receipt.is_none(),
                "abort-rejection case `{}` did not reject through its exact durable abort authority: {status} {response}",
                case.case_id
            );
            assert_exact_abort_authority(
                case,
                after_retry,
                handoff,
                final_source,
                final_target,
                abort_fence,
            )?;
        }
    }
    Ok(())
}

type CasEntrySnapshot = BTreeMap<(String, String), u64>;

fn snapshot_cas_entries(state_path: &Path) -> Result<CasEntrySnapshot> {
    let store = open_daemon_state(state_path)?;
    let authority = store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let root = lillux::PinnedDirectory::open(cas.root())?
        .with_context(|| format!("open pinned CAS root {}", cas.root().display()))?;
    let mut entries = BTreeMap::new();
    for (namespace, extension) in [("objects", ".json"), ("blobs", "")] {
        let Some(namespace_dir) = root.open_child_directory(namespace.as_ref())? else {
            continue;
        };
        for first_name in namespace_dir.entry_names()? {
            let Some(first) = namespace_dir.open_child_directory(&first_name)? else {
                anyhow::bail!("CAS namespace contains a non-directory first shard");
            };
            for second_name in first.entry_names()? {
                let Some(second) = first.open_child_directory(&second_name)? else {
                    anyhow::bail!("CAS namespace contains a non-directory second shard");
                };
                for entry_name in second.entry_names()? {
                    let text = entry_name.to_str().context("CAS entry name is not UTF-8")?;
                    let hash = text.strip_suffix(extension).unwrap_or(text);
                    anyhow::ensure!(
                        lillux::valid_hash(hash)
                            && hash.bytes().all(|byte| !byte.is_ascii_uppercase()),
                        "CAS snapshot found a non-canonical entry `{text}`"
                    );
                    let file = second
                        .open_regular(&entry_name, false)?
                        .context("CAS snapshot entry is not a regular file")?;
                    let key = (namespace.to_owned(), hash.to_owned());
                    anyhow::ensure!(
                        entries.insert(key, file.metadata()?.len()).is_none(),
                        "CAS snapshot contains a duplicate typed entry"
                    );
                }
            }
        }
    }
    authority.ensure_guard(&guard)?;
    Ok(entries)
}

fn seed_source_exported_handoff(
    state_path: &Path,
    source_site_id: &str,
    target_site_id: &str,
) -> Result<SeededSourceHandoff> {
    seed_source_exported_handoff_with_kind(
        state_path,
        source_site_id,
        target_site_id,
        "system_task",
    )
}

fn seed_source_exported_handoff_with_kind(
    state_path: &Path,
    source_site_id: &str,
    target_site_id: &str,
    kind: &str,
) -> Result<SeededSourceHandoff> {
    let store = open_daemon_state(state_path)?;
    let source_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    let successor_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    let item_ref = "system:handoff-crash-recovery-fixture";
    store.create_thread_for_test(&NewThreadRecord {
        thread_id: source_thread_id.clone(),
        chain_root_id: source_thread_id.clone(),
        // Seam-only cases pass the daemon-owned bookkeeping kind so unrelated
        // process recovery stays out of their oracle. The dedicated startup
        // regression passes the real executable worker kind to prove the
        // durable handoff fence wins generic reconciliation.
        kind: kind.to_owned(),
        item_ref: item_ref.to_owned(),
        executor_ref: "daemon:handoff-crash-recovery-fixture".to_owned(),
        launch_mode: "wait".to_owned(),
        current_site_id: source_site_id.to_owned(),
        origin_site_id: source_site_id.to_owned(),
        upstream_thread_id: None,
        requested_by: Some("fp:handoff-fixture-owner".to_owned()),
        project_root: None,
        project_authority: ryeos_state::objects::ExecutionProjectAuthority::PROJECTLESS,
        base_project_snapshot_hash: None,
        usage_subject: None,
        usage_subject_asserted_by: None,
        captured_history_policy: Some(captured_policy(item_ref)),
    })?;
    store.mark_thread_running(&source_thread_id, None)?;

    let (_, source_event, source_head_hash) = store
        .get_authoritative_thread_snapshot_with_last_event(&source_thread_id, &source_thread_id)?
        .context("seeded source root has no authoritative snapshot")?;
    let source_last_event_hash = source_event
        .and_then(|event| event.event_hash)
        .context("seeded running source has no authoritative last event")?;
    let operation_id = fixture_hash("operation");
    let transfer_manifest_hash = fixture_hash("transfer-manifest");
    let operation = WorkerSessionHandoffJobOperation::new(
        WorkerHandoffJobRole::Source,
        operation_id.clone(),
        fixture_hash("preflight"),
        fixture_hash("preflight-attestation"),
        "fp:handoff-fixture-owner".to_owned(),
        source_thread_id.clone(),
        source_site_id.to_owned(),
        source_site_id.to_owned(),
        target_site_id.to_owned(),
        source_thread_id,
        successor_thread_id,
        source_head_hash.clone(),
        source_last_event_hash,
        fixture_hash("checkpoint-manifest"),
        transfer_manifest_hash.clone(),
        "missing-handoff-target".to_owned(),
        state_path.join("source-project").display().to_string(),
        state_path.join("target-project").display().to_string(),
        fixture_hash("project-route"),
        TARGET_CREDENTIAL_PROFILE_ID.to_owned(),
        None,
    )?;
    let progress = WorkerSessionHandoffProgress::source_exported(operation_id.clone())?;
    let progress_value = progress.to_value()?;
    let job_id = format!("worker-handoff-source:{operation_id}");
    store.with_state_db(|db| {
        db.create_sync_job_with_initial_progress(
            &NewSyncJob {
                job_id: job_id.clone(),
                operation_type: WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
                operation: operation.to_value()?,
                peer: Some("missing-handoff-target".to_owned()),
                roots: vec![
                    transfer_manifest_hash,
                    operation.preflight_attestation_hash.clone(),
                ],
                heads: vec![source_head_hash.clone()],
                max_attempts: WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
            },
            SyncJobState::Running,
            WorkerHandoffPhase::SourceExported.as_str(),
            Some(&progress_value),
        )
    })?;

    Ok(SeededSourceHandoff {
        operation,
        job_id,
        source_head_hash,
    })
}

fn assert_source_recovery_state(
    state_path: &Path,
    seeded: &SeededSourceHandoff,
    expected_phase: WorkerHandoffPhase,
    expected_head_hash: &str,
    expected_abort_events: usize,
) -> Result<()> {
    let store = open_daemon_state(state_path)?;
    let job = store
        .with_state_db(|db| db.get_sync_job(&seeded.job_id))?
        .context("source handoff job disappeared")?;
    let progress = WorkerSessionHandoffProgress::from_value(
        job.result.context("source handoff job lost progress")?,
    )?;
    anyhow::ensure!(
        progress.phase == expected_phase,
        "expected source phase {}, observed {}",
        expected_phase.as_str(),
        progress.phase.as_str()
    );
    anyhow::ensure!(
        job.state == SyncJobState::Running,
        "source job unexpectedly changed state to {:?}",
        job.state
    );
    let head = store
        .with_state_db(|db| db.read_generic_head_ref("chains", &seeded.operation.chain_root_id))?
        .context("source chain head disappeared")?;
    anyhow::ensure!(
        head.target_hash == expected_head_hash,
        "expected source head {expected_head_hash}, observed {}",
        head.target_hash
    );
    let expected_signer = daemon_identity(state_path)?.fingerprint().to_owned();
    anyhow::ensure!(
        head.signer == expected_signer,
        "source chain head is no longer locally signed"
    );
    anyhow::ensure!(
        store.current_chain_placement_thread_id(&seeded.operation.chain_root_id)?
            == Some(seeded.operation.source_placement_thread_id.clone()),
        "pre-cut recovery changed the current placement"
    );
    let abort_events = store
        .latest_thread_events(&seeded.operation.source_placement_thread_id, 32)?
        .into_iter()
        .filter(|event| event.event_type == "worker_session.handoff_aborted")
        .count();
    anyhow::ensure!(
        abort_events == expected_abort_events,
        "expected {expected_abort_events} abort fact(s), observed {abort_events}"
    );
    if expected_abort_events == 1 {
        let authority = store.pinned_state_authority()?;
        let guard = authority.acquire_shared_guard()?;
        authority.ensure_guard(&guard)?;
        ryeos_app::worker_handoff::validate_handoff_abort_authority(
            &authority.cas_store()?,
            &seeded.operation,
            expected_head_hash,
        )?;
    }
    Ok(())
}

fn export_exact_chain_head(
    store: &StateStore,
    chain_root_id: &str,
    head_hash: &str,
) -> Result<ryeos_state::sync::ExportPayload> {
    let authority = store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    ryeos_state::sync::export_exact_chain_head_pinned(&authority, chain_root_id, head_hash, &guard)
}

fn create_source_abort_authority() -> Result<SourceAbortAuthority> {
    let state_root = tempfile::tempdir()?;
    let identity_path = state_root
        .path()
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    std::fs::create_dir_all(
        identity_path
            .parent()
            .context("source identity path has no parent")?,
    )?;
    let identity = NodeIdentity::create(&identity_path)?;
    let signing_key = identity.signing_key().clone();
    drop(identity);

    let seeded = seed_source_exported_handoff(
        state_root.path(),
        REMOTE_SOURCE_SITE_ID,
        DAEMON_TARGET_SITE_ID,
    )?;
    let store = open_daemon_state(state_root.path())?;
    let source_head_payload = export_exact_chain_head(
        &store,
        &seeded.operation.chain_root_id,
        &seeded.source_head_hash,
    )?;
    store.append_events(
        &seeded.operation.chain_root_id,
        &seeded.operation.source_placement_thread_id,
        &[NewEventRecord {
            event_type: "worker_session.handoff_aborted".to_owned(),
            storage_class: "indexed".to_owned(),
            payload: serde_json::json!({
                "schema":"ryeos.worker_session_handoff_abort.v1",
                "operation_id":seeded.operation.operation_id,
                "chain_root_id":seeded.operation.chain_root_id,
                "source_placement_thread_id":seeded.operation.source_placement_thread_id,
                "source_site_id":seeded.operation.source_site_id,
                "target_site_id":seeded.operation.target_site_id,
                "source_chain_head_hash":seeded.operation.source_chain_head_hash,
                "source_last_event_hash":seeded.operation.source_last_event_hash,
            }),
        }],
    )?;
    let abort_head_hash = store
        .with_state_db(|db| db.read_generic_head_ref("chains", &seeded.operation.chain_root_id))?
        .context("source abort publication produced no chain head")?
        .target_hash;
    ryeos_app::worker_handoff::validate_handoff_abort_authority(
        &store.pinned_state_authority()?.cas_store()?,
        &seeded.operation,
        &abort_head_hash,
    )?;
    let abort_payload =
        export_exact_chain_head(&store, &seeded.operation.chain_root_id, &abort_head_hash)?;

    Ok(SourceAbortAuthority {
        _state_root: state_root,
        signing_key,
        operation: seeded.operation,
        source_head_payload,
        abort_payload,
        abort_head_hash,
    })
}

fn closure_response(payload: &ryeos_state::sync::ExportPayload) -> Result<serde_json::Value> {
    let mut object_hashes = Vec::new();
    let mut blob_hashes = Vec::new();
    let mut object_bytes = 0_u64;
    let mut blob_bytes = 0_u64;
    let mut entries = Vec::with_capacity(payload.entries.len());
    for entry in &payload.entries {
        if entry.is_blob {
            blob_hashes.push(entry.hash.clone());
            blob_bytes = blob_bytes.saturating_add(u64::try_from(entry.data.len())?);
            entries.push(serde_json::json!({
                "hash":entry.hash,
                "kind":"blob",
                "data":base64::engine::general_purpose::STANDARD.encode(&entry.data),
            }));
        } else {
            object_hashes.push(entry.hash.clone());
            object_bytes = object_bytes.saturating_add(u64::try_from(entry.data.len())?);
            let value: serde_json::Value = serde_json::from_slice(&entry.data)
                .with_context(|| format!("decode source closure object {}", entry.hash))?;
            entries.push(serde_json::json!({
                "hash":entry.hash,
                "kind":"object",
                "value":value,
            }));
        }
    }
    Ok(serde_json::json!({
        "closure":{
            "roots":[payload.chain_head_hash],
            "complete":true,
            "object_hashes":object_hashes,
            "blob_hashes":blob_hashes,
            "large_object_hashes":[],
            "missing_objects":[],
            "missing_blobs":[],
            "malformed_objects":[],
            "unsupported_objects":[],
        },
        "object_bytes":object_bytes,
        "blob_bytes":blob_bytes,
        "entries":entries,
    }))
}

async fn start_source_closure_server(
    payload: &ryeos_state::sync::ExportPayload,
) -> Result<(String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)> {
    let response = Arc::new(closure_response(payload)?);
    let requests = Arc::new(AtomicUsize::new(0));
    let observed_requests = Arc::clone(&requests);
    let app = Router::new().route(
        "/objects/closure/get",
        post(move || {
            let response = Arc::clone(&response);
            let observed_requests = Arc::clone(&observed_requests);
            async move {
                observed_requests.fetch_add(1, Ordering::SeqCst);
                Json((*response).clone())
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), requests, task))
}

fn target_job_id(operation_id: &str) -> String {
    format!("worker-handoff-target:{operation_id}")
}

fn plant_target_abort_state(
    state_path: &Path,
    target_fixture: &common::fast_fixture::FastFixture,
    source: &SourceAbortAuthority,
    source_url: &str,
) -> Result<()> {
    let source_verifying_key = source.signing_key.verifying_key();
    let source_fingerprint = lillux::signature::compute_fingerprint(&source_verifying_key);
    let source_principal = format!("fp:{source_fingerprint}");
    let source_public_key =
        base64::engine::general_purpose::STANDARD.encode(source_verifying_key.as_bytes());
    let mut remotes = HashMap::new();
    remotes.insert(
        TARGET_REMOTE_NAME.to_owned(),
        ryeos_api::remote::config::RemoteConfig {
            name: TARGET_REMOTE_NAME.to_owned(),
            url: source_url.to_owned(),
            principal_id: source_principal.clone(),
            signing_key: format!("ed25519:{source_public_key}"),
            site_id: REMOTE_SOURCE_SITE_ID.to_owned(),
            vault_fingerprint: "sha256:handoff-source-fixture".to_owned(),
            ingest_ignore: ryeos_app::ignore::IgnoreConfig { patterns: vec![] },
            project_bindings: HashMap::new(),
        },
    );
    ryeos_api::remote::config::save_remotes(state_path, &remotes)?;

    let auth_dir = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/auth/authorized_keys");
    ryeos_app::identity::write_authorized_remote_node_key_toml(
        &auth_dir,
        &source_fingerprint,
        &source_public_key,
        &["ryeos.execute.service.worker-placements/abort".to_owned()],
        "handoff-source-fixture",
        &target_fixture.node_fp(),
        &lillux::time::iso8601_now(),
        REMOTE_SOURCE_SITE_ID,
        &target_fixture.node,
    )?;

    let store = open_daemon_state(state_path)?;
    store.create_credential_profile(NewCredentialProfile {
        profile_id: TARGET_CREDENTIAL_PROFILE_ID,
        owner_principal: &source.operation.owner_principal,
        home_id: "home:handoff-target-fixture",
    })?;
    let enrollment_lock = "credential-enrollment:handoff-target-fixture";
    store.acquire_credential_profile(
        TARGET_CREDENTIAL_PROFILE_ID,
        &source.operation.owner_principal,
        enrollment_lock,
    )?;
    let login_id = "credential-login:handoff-target-fixture";
    let login_epoch = store.begin_credential_enrollment(
        TARGET_CREDENTIAL_PROFILE_ID,
        enrollment_lock,
        login_id,
        i64::try_from(lillux::time::timestamp_millis())? + 60_000,
    )?;
    let credential_generation = store.complete_credential_enrollment(
        TARGET_CREDENTIAL_PROFILE_ID,
        enrollment_lock,
        login_id,
        login_epoch,
        &serde_json::json!({"account":"handoff-target-fixture"}),
    )?;
    store.release_credential_profile(TARGET_CREDENTIAL_PROFILE_ID, enrollment_lock)?;
    let subject_contract_digest = fixture_hash("credential-subject-contract");
    let subject_digest = fixture_hash("credential-subject");
    store.reserve_credential_profile_generation(NewCredentialProfileReservation {
        reservation_id: TARGET_CREDENTIAL_RESERVATION_ID,
        operation_id: &source.operation.operation_id,
        successor_thread_id: &source.operation.successor_placement_thread_id,
        profile_id: TARGET_CREDENTIAL_PROFILE_ID,
        owner_principal: &source.operation.owner_principal,
        credential_generation,
        subject_contract_digest: &subject_contract_digest,
        subject_digest: &subject_digest,
        checkpoint_manifest_hash: &source.operation.checkpoint_manifest_hash,
        upstream_session_id: "upstream-session:handoff-fixture",
    })?;

    let authority = store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let placement_attestation_hash = cas.store_object(&serde_json::json!({
        "schema":"ryeos.test.worker_placement_attestation.v1",
        "operation_id":source.operation.operation_id,
    }))?;
    let target_runtime_seed_hash = cas.store_object(&serde_json::json!({
        "schema":"ryeos.test.placement_runtime_seed.v1",
        "operation_id":source.operation.operation_id,
    }))?;
    drop(guard);

    let mut target_operation = source.operation.clone();
    target_operation.role = WorkerHandoffJobRole::Target;
    target_operation.peer_remote_name = TARGET_REMOTE_NAME.to_owned();
    target_operation.validate()?;
    let progress = WorkerSessionHandoffProgress {
        schema: "ryeos.worker_session_handoff_progress.v1".to_owned(),
        operation_id: target_operation.operation_id.clone(),
        phase: WorkerHandoffPhase::TargetPrepared,
        placement_attestation_hash: Some(placement_attestation_hash.clone()),
        target_runtime_seed_hash: Some(target_runtime_seed_hash.clone()),
        writer_grant_hash: None,
        target_chain_head_hash: None,
        credential_reservation_id: Some(TARGET_CREDENTIAL_RESERVATION_ID.to_owned()),
        abort_chain_head_hash: None,
    };
    progress.validate()?;
    let job_id = target_job_id(&target_operation.operation_id);
    store.stage_sync_payload_and_create_job(
        &source.source_head_payload,
        &ryeos_state::sync::ImportAttribution {
            source_principal: Some(source_principal),
            source_peer: Some(TARGET_REMOTE_NAME.to_owned()),
            job_id: Some(job_id.clone()),
        },
        &NewSyncJob {
            job_id: job_id.clone(),
            operation_type: WORKER_SESSION_HANDOFF_OPERATION.to_owned(),
            operation: target_operation.to_value()?,
            peer: Some(TARGET_REMOTE_NAME.to_owned()),
            roots: vec![source.operation.source_chain_head_hash.clone()],
            heads: vec![source.operation.source_chain_head_hash.clone()],
            max_attempts: WORKER_SESSION_HANDOFF_MAX_ATTEMPTS,
        },
    )?;
    store.with_state_db(|db| {
        db.update_sync_job(
            &job_id,
            &SyncJobUpdate {
                state: SyncJobState::Running,
                phase: WorkerHandoffPhase::TargetPrepared.as_str().to_owned(),
                roots: Some(vec![
                    source.operation.source_chain_head_hash.clone(),
                    placement_attestation_hash,
                    target_runtime_seed_hash,
                ]),
                heads: None,
                uploaded_hashes: Vec::new(),
                fetched_hashes: Vec::new(),
                last_error: None,
                result: Some(progress.to_value()?),
            },
        )
    })?;
    Ok(())
}

fn assert_target_abort_state(
    state_path: &Path,
    source: &SourceAbortAuthority,
    expected: TargetAbortCutExpectation,
) -> Result<()> {
    let store = open_daemon_state(state_path)?;
    let job = store
        .with_state_db(|db| db.get_sync_job(&target_job_id(&source.operation.operation_id)))?
        .context("target handoff job disappeared")?;
    anyhow::ensure!(
        job.state == expected.state,
        "expected target job state {}, observed {}",
        expected.state.as_str(),
        job.state.as_str()
    );
    anyhow::ensure!(
        job.roots.iter().any(|root| root == &source.abort_head_hash)
            == expected.abort_root_retained,
        "target abort root retention differs from the crash oracle"
    );
    anyhow::ensure!(
        job.heads == [source.abort_head_hash.clone()],
        "target abort claim did not retain the exact source abort head"
    );
    if expected.state == SyncJobState::Cancelled {
        anyhow::ensure!(
            job.phase == "aborted",
            "cancelled target job is not aborted"
        );
        let response: WorkerPlacementAbortResponse = serde_json::from_value(
            job.result
                .context("cancelled target job lost its abort receipt")?,
        )?;
        response.validate_against(&WorkerPlacementAbortRequest {
            operation: source.operation.clone(),
            abort_chain_head_hash: source.abort_head_hash.clone(),
        })?;
    } else {
        let progress = WorkerSessionHandoffProgress::from_value(
            job.result.context("active target job lost its progress")?,
        )?;
        anyhow::ensure!(
            progress.phase == expected.phase,
            "expected target phase {}, observed {}",
            expected.phase.as_str(),
            progress.phase.as_str()
        );
    }
    let reservation = store
        .credential_profile_reservation_for_successor(
            &source.operation.successor_placement_thread_id,
        )?
        .context("target credential reservation disappeared")?;
    anyhow::ensure!(
        reservation.reservation_id == TARGET_CREDENTIAL_RESERVATION_ID
            && reservation.operation_id == source.operation.operation_id
            && reservation.state == expected.reservation_state,
        "target credential reservation differs from the crash oracle: {reservation:?}"
    );
    if expected.abort_root_retained {
        ryeos_app::worker_handoff::validate_handoff_abort_authority(
            &store.pinned_state_authority()?.cas_store()?,
            &source.operation,
            &source.abort_head_hash,
        )?;
    }
    let fence = store
        .worker_handoff_abort_fence(&source.operation.operation_id)?
        .context("target abort fence disappeared")?;
    anyhow::ensure!(
        fence.terminal_disposition.as_deref() == Some(expected.fence_terminal_disposition),
        "target abort fence differs from the crash oracle"
    );
    Ok(())
}

async fn post_remote_abort(
    bind: std::net::SocketAddr,
    source_key: &SigningKey,
    target_node_key: &SigningKey,
    operation: &WorkerSessionHandoffJobOperation,
    abort_head_hash: &str,
) -> Result<(reqwest::StatusCode, serde_json::Value)> {
    let request = WorkerPlacementAbortRequest {
        operation: operation.clone(),
        abort_chain_head_hash: abort_head_hash.to_owned(),
    };
    let body = serde_json::json!({
        "item_ref":WORKER_PLACEMENT_ABORT_SERVICE,
        "ref_bindings":{},
        "project_path":null,
        "parameters":request,
        "execution_policy":ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let body_bytes = serde_json::to_vec(&body)?;
    let mut request = reqwest::Client::new()
        .post(format!("http://{bind}/execute"))
        .header("content-type", "application/json")
        .body(body_bytes.clone());
    for (name, value) in common::build_signed_headers_for_bytes(
        source_key,
        target_node_key,
        "POST",
        "/execute",
        &body_bytes,
    ) {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response
        .json()
        .await
        .unwrap_or_else(|_| serde_json::json!({}));
    Ok((status, body))
}

async fn post_remote_adopt(
    bind: std::net::SocketAddr,
    source_key: &SigningKey,
    target_node_key: &SigningKey,
    request: &WorkerPlacementAdoptRequest,
) -> Result<(reqwest::StatusCode, serde_json::Value)> {
    let body = serde_json::json!({
        "item_ref":WORKER_PLACEMENT_ADOPT_SERVICE,
        "ref_bindings":{},
        "project_path":null,
        "parameters":request,
        "execution_policy":ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let body_bytes = serde_json::to_vec(&body)?;
    let mut request = reqwest::Client::new()
        .post(format!("http://{bind}/execute"))
        .header("content-type", "application/json")
        .body(body_bytes.clone());
    for (name, value) in common::build_signed_headers_for_bytes(
        source_key,
        target_node_key,
        "POST",
        "/execute",
        &body_bytes,
    ) {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    let body = response
        .json()
        .await
        .unwrap_or_else(|_| serde_json::json!({}));
    Ok((status, body))
}

async fn qualify_target_abort_boundary(
    source: &SourceAbortAuthority,
    source_url: &str,
    source_requests: &AtomicUsize,
    boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    expected_cut: TargetAbortCutExpectation,
) -> Result<u64> {
    let source_url = source_url.to_owned();
    let (mut target, target_fixture, mut gate) =
        DaemonHarness::start_fast_with_node_key_and_handoff_crash_gate(
            target_node_signing_key(),
            |state_path, _user_space, target_fixture| {
                plant_target_abort_state(state_path, target_fixture, source, &source_url)
            },
            boundary,
        )
        .await?;
    anyhow::ensure!(
        daemon_identity(&target.state_path)?.fingerprint() == target_fixture.node_fp(),
        "target daemon identity differs from its request audience"
    );
    let bind = target.bind;
    let source_key = source.signing_key.clone();
    let target_node_key = target_fixture.node.clone();
    let operation = source.operation.clone();
    let abort_head_hash = source.abort_head_hash.clone();
    let request_task = tokio::spawn(async move {
        post_remote_abort(
            bind,
            &source_key,
            &target_node_key,
            &operation,
            &abort_head_hash,
        )
        .await
    });
    gate.wait_reached().await?;
    target.kill_daemon().await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), request_task).await;
    assert_target_abort_state(&target.state_path, source, expected_cut)?;

    // Recovery timing starts at the durable crash cut. Fixture construction
    // and the request used to reach the cut are qualification setup, not
    // recovery work.
    let recovery_timer = lillux::time::MonotonicTimer::start();
    target.respawn_with(|_| {}).await?;
    let (status, body) = post_remote_abort(
        target.bind,
        &source.signing_key,
        &target_fixture.node,
        &source.operation,
        &source.abort_head_hash,
    )
    .await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "target abort retry returned {status}: {body}"
    );
    let terminal_result: WorkerPlacementAbortResult = serde_json::from_value(
        body.get("result")
            .cloned()
            .context("target abort retry returned no typed result")?,
    )?;
    terminal_result.validate_against(&WorkerPlacementAbortRequest {
        operation: source.operation.clone(),
        abort_chain_head_hash: source.abort_head_hash.clone(),
    })?;
    target.kill_daemon().await?;
    assert_target_abort_state(
        &target.state_path,
        source,
        TargetAbortCutExpectation {
            phase: WorkerHandoffPhase::AbortAuthorized,
            state: SyncJobState::Cancelled,
            abort_root_retained: true,
            reservation_state: "released",
            fence_terminal_disposition: "reservation_released",
        },
    )?;
    let recovery_elapsed = recovery_timer.elapsed_millis();

    let target_store = open_daemon_state(&target.state_path)?;
    let (deleted_jobs, _) = target_store
        .with_state_db(|db| db.delete_terminal_sync_jobs_before("9999-12-31T23:59:59Z"))?;
    anyhow::ensure!(
        deleted_jobs > 0
            && target_store
                .with_state_db(|db| {
                    db.get_sync_job(&target_job_id(&source.operation.operation_id))
                })?
                .is_none(),
        "target abort job survived terminal retention"
    );
    anyhow::ensure!(
        target_store
            .worker_handoff_abort_fence(&source.operation.operation_id)?
            .is_some_and(
                |fence| fence.terminal_disposition.as_deref() == Some("reservation_released")
            ),
        "target abort receipt did not survive terminal retention"
    );
    drop(target_store);
    let requests_before_replay = source_requests.load(Ordering::SeqCst);
    target.respawn_with(|_| {}).await?;
    let (status, body) = post_remote_abort(
        target.bind,
        &source.signing_key,
        &target_fixture.node,
        &source.operation,
        &source.abort_head_hash,
    )
    .await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "retained target abort retry returned {status}: {body}"
    );
    let retained_terminal_result: WorkerPlacementAbortResult = serde_json::from_value(
        body.get("result")
            .cloned()
            .context("retained target abort replay returned no typed result")?,
    )?;
    retained_terminal_result.validate_against(&WorkerPlacementAbortRequest {
        operation: source.operation.clone(),
        abort_chain_head_hash: source.abort_head_hash.clone(),
    })?;
    anyhow::ensure!(
        retained_terminal_result == terminal_result,
        "retained target abort replay changed its terminal attestation"
    );
    anyhow::ensure!(
        source_requests.load(Ordering::SeqCst) == requests_before_replay,
        "retained target abort replay contacted the source closure server"
    );
    let opposite_request = WorkerPlacementAdoptRequest {
        operation_id: source.operation.operation_id.clone(),
        chain_root_id: source.operation.chain_root_id.clone(),
        target_chain_head_hash: "9".repeat(64),
        placement_attestation_hash: "a".repeat(64),
        writer_grant_hash: "b".repeat(64),
    };
    let (opposite_status, _) = post_remote_adopt(
        target.bind,
        &source.signing_key,
        &target_fixture.node,
        &opposite_request,
    )
    .await?;
    anyhow::ensure!(
        !opposite_status.is_success(),
        "retained abort branch admitted the competing adoption branch"
    );
    target.kill_daemon().await?;
    Ok(recovery_elapsed)
}

async fn launch_and_checkpoint_portable_worker(
    daemon: &mut DaemonHarness,
    project: &Path,
    launch_id: &str,
) -> Result<PortableCheckpoint> {
    let project_path = project.display().to_string();
    let launch_body = serde_json::json!({
        "launch_id":launch_id,
        "item_ref":PORTABLE_EXECUTION_REF,
        "ref_bindings":{},
        "project_path":project_path,
        "parameters":{"credential_profile_id":PORTABLE_CREDENTIAL_PROFILE_ID},
        "execution_policy":ryeos_app::execution_policy::ExecutionPolicy::local_pinned_capture(
            ryeos_app::execution_policy::ExecutionResponse::Accepted,
        ),
    });
    let (status, launch) = daemon.post_json("/execute/launch", launch_body).await?;
    anyhow::ensure!(
        matches!(
            status,
            reqwest::StatusCode::OK | reqwest::StatusCode::ACCEPTED
        ),
        "portable worker launch returned {status}: {launch}"
    );
    let chain_root_id = launch
        .get("thread_id")
        .or_else(|| launch.get("chain_root_id"))
        .or_else(|| launch.pointer("/result/thread_id"))
        .or_else(|| launch.pointer("/result/chain_root_id"))
        .or_else(|| launch.pointer("/thread/chain_root_id"))
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("portable worker launch returned no chain root: {launch}"))?
        .to_owned();
    let mut command_result = None;
    let mut last_command = None;
    for _ in 0..120 {
        let response = daemon
            .post_execute(
                "service:worker-executions/command",
                ".",
                serde_json::json!({
                    "chain_root_id":chain_root_id,
                    "idempotency_key":format!("{launch_id}-session-start"),
                    "route_id":"session.start",
                    "payload":{},
                }),
            )
            .await?;
        if response.0 == reqwest::StatusCode::OK {
            command_result = Some(response.1);
            break;
        }
        last_command = Some(response);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    command_result.with_context(|| {
        format!("portable worker never accepted its first command: {last_command:?}")
    })?;
    let (status, terminated) = daemon
        .post_execute(
            "service:worker-executions/terminate",
            ".",
            serde_json::json!({"chain_root_id":chain_root_id,"reason":"completed"}),
        )
        .await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "portable worker termination returned {status}: {terminated}"
    );
    let mut frozen = false;
    let mut last_status = None;
    for _ in 0..120 {
        let response = daemon
            .post_execute(
                "service:worker-executions/status",
                ".",
                serde_json::json!({"chain_root_id":chain_root_id}),
            )
            .await?;
        if response.0 == reqwest::StatusCode::OK
            && response
                .1
                .pointer("/result/state")
                .and_then(serde_json::Value::as_str)
                == Some("frozen")
        {
            frozen = true;
            break;
        }
        last_status = Some(response);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !frozen {
        let stderr = daemon.drain_stderr_nonblocking().await;
        daemon.kill_daemon().await?;
        let store = open_daemon_state(&daemon.state_path)?;
        let current_placement = store.current_chain_placement_thread_id(&chain_root_id)?;
        let direct_session = store.dedicated_session(&chain_root_id)?.map(|session| {
            serde_json::json!({
                "placement_thread_id":session.placement_thread_id,
                "owner_principal":session.owner_principal,
                "state":session.state,
                "candidate_snapshot_hash":session.candidate_snapshot_hash,
                "terminal_reason":session.terminal_reason,
            })
        });
        let thread = store.get_thread(&chain_root_id)?.map(|thread| {
            serde_json::json!({
                "thread_id":thread.thread_id,
                "chain_root_id":thread.chain_root_id,
                "status":thread.status,
                "requested_by":thread.requested_by,
            })
        });
        let events = store
            .latest_thread_events(&chain_root_id, 32)?
            .into_iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>();
        let mut diagnostic_lines = stderr
            .lines()
            .filter(|line| {
                line.contains(&chain_root_id)
                    || line.contains("candidate")
                    || line.contains("dedicated")
                    || line.contains("worker process")
                    || line.contains("ERROR")
            })
            .rev()
            .take(128)
            .collect::<Vec<_>>();
        diagnostic_lines.reverse();
        anyhow::bail!(
            "portable worker candidate never froze: {last_status:?}; termination={terminated}; current_placement={current_placement:?}; direct_session={direct_session:?}; thread={thread:?}; events={events:?}; filtered daemon stderr:\n{}",
            diagnostic_lines.join("\n")
        );
    }
    let (status, checkpoint) = daemon
        .post_execute(
            "service:worker-executions/checkpoint",
            ".",
            serde_json::json!({"chain_root_id":chain_root_id}),
        )
        .await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "portable worker checkpoint returned {status}: {checkpoint}"
    );
    let manifest_ref = checkpoint
        .pointer("/result/manifest_ref")
        .and_then(serde_json::Value::as_str)
        .filter(|value| value.starts_with("cas:"))
        .with_context(|| format!("checkpoint returned no manifest authority: {checkpoint}"))?
        .to_owned();
    Ok(PortableCheckpoint {
        chain_root_id,
        manifest_ref,
    })
}

fn install_target_project_head(
    source_state_path: &Path,
    target_state_path: &Path,
    target_project: &Path,
    owner_principal: &str,
    snapshot_hash: &str,
) -> Result<()> {
    let source = open_daemon_state(source_state_path)?;
    let source_authority = source.pinned_state_authority()?;
    let source_guard = source_authority.acquire_shared_guard()?;
    source_authority.ensure_guard(&source_guard)?;
    let source_cas = source_authority.cas_store()?;
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &source_cas,
        [snapshot_hash.to_owned()],
        ryeos_state::object_closure::ObjectClosureLimits::for_project_snapshot_transport(),
    )?;
    anyhow::ensure!(
        closure.is_complete() && closure.large_object_hashes.is_empty(),
        "portable project candidate closure is not locally transportable: {closure:?}"
    );
    let objects = closure
        .object_hashes
        .iter()
        .map(|hash| {
            Ok((
                hash.clone(),
                source_cas
                    .get_object(hash)?
                    .with_context(|| format!("source project closure lost object {hash}"))?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let blobs = closure
        .blob_hashes
        .iter()
        .map(|hash| {
            Ok((
                hash.clone(),
                source_cas
                    .get_blob(hash)?
                    .with_context(|| format!("source project closure lost blob {hash}"))?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    drop(source_cas);
    drop(source_guard);
    drop(source_authority);
    drop(source);

    let target = open_daemon_state(target_state_path)?;
    let target_authority = target.pinned_state_authority()?;
    let target_guard = target_authority.acquire_shared_guard()?;
    target_authority.ensure_guard(&target_guard)?;
    let target_cas = target_authority.cas_store()?;
    for (hash, value) in objects {
        anyhow::ensure!(target_cas.store_object(&value)? == hash);
    }
    for (hash, value) in blobs {
        anyhow::ensure!(target_cas.store_blob(&value)? == hash);
    }
    target.verify_project_snapshot_closure(snapshot_hash)?;
    let canonical = ryeos_api::remote::config::canonical_local_project_path(target_project)?;
    let project_ref = ryeos_executor::execution::project_source::canonical_project_ref(
        ryeos_api::remote::config::local_project_identity(&canonical)?,
    )?;
    let project_hash = lillux::sha256_hex(project_ref.as_bytes());
    let principal_key = ryeos_state::refs::principal_storage_key(owner_principal)?.to_owned();
    let signer = NodeIdentitySigner::from_identity(&daemon_identity(target_state_path)?);
    target.with_state_db(|db| {
        db.write_project_head_ref(
            &principal_key,
            &project_hash,
            snapshot_hash,
            &signer,
            &target_guard,
        )
    })?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_abort_recovery_survives_sigkill_at_each_durable_seam() -> Result<()> {
    let (mut daemon, _fixture) = DaemonHarness::start_fast().await?;
    daemon.kill_daemon().await?;
    let seeded = seed_source_exported_handoff(
        &daemon.state_path,
        DAEMON_SOURCE_SITE_ID,
        REMOTE_TARGET_SITE_ID,
    )?;

    let boundaries = matrix_boundaries(
        HandoffNode::Source,
        OperatorOutcome::AbortedSourceContinues,
        true,
    );
    let mut abort_head_hash = None;
    for boundary in boundaries {
        daemon
            .respawn_until_handoff_crash_boundary(boundary)
            .await
            .with_context(|| format!("reach source recovery boundary `{boundary}`"))?;
        daemon.kill_daemon().await?;
        match boundary {
            HandoffCrashBoundary::SourceBeforeAbortPublication => {
                assert_source_recovery_state(
                    &daemon.state_path,
                    &seeded,
                    WorkerHandoffPhase::SourceExported,
                    &seeded.source_head_hash,
                    0,
                )?;
            }
            HandoffCrashBoundary::SourceAbortPublished => {
                let store = open_daemon_state(&daemon.state_path)?;
                let observed = store
                    .with_state_db(|db| {
                        db.read_generic_head_ref("chains", &seeded.operation.chain_root_id)
                    })?
                    .context("source abort publication produced no chain head")?
                    .target_hash;
                drop(store);
                anyhow::ensure!(
                    observed != seeded.source_head_hash,
                    "source abort publication did not advance the chain"
                );
                assert_source_recovery_state(
                    &daemon.state_path,
                    &seeded,
                    WorkerHandoffPhase::SourceExported,
                    &observed,
                    1,
                )?;
                abort_head_hash = Some(observed);
            }
            HandoffCrashBoundary::SourceAbortProjected => {
                assert_source_recovery_state(
                    &daemon.state_path,
                    &seeded,
                    WorkerHandoffPhase::AbortAuthorized,
                    abort_head_hash
                        .as_deref()
                        .context("source abort projection ran before publication")?,
                    1,
                )?;
            }
            other => anyhow::bail!("matrix selected non-abort source boundary `{other}`"),
        }
    }

    // One ungated boot runs the ordinary recovery path through its missing
    // remote configuration failure. That expected external failure must not
    // duplicate the signed abort or roll the job back to source_exported.
    daemon.respawn_with(|_| {}).await?;
    daemon.kill_daemon().await?;
    assert_source_recovery_state(
        &daemon.state_path,
        &seeded,
        WorkerHandoffPhase::AbortAuthorized,
        abort_head_hash
            .as_deref()
            .context("source abort matrix published no abort head")?,
        1,
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_reconcile_leaves_real_source_placement_to_handoff_recovery() -> Result<()> {
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let (mut daemon, _fixture) = DaemonHarness::start_fast().await?;
    daemon.kill_daemon().await?;
    let seeded = seed_source_exported_handoff_with_kind(
        &daemon.state_path,
        DAEMON_SOURCE_SITE_ID,
        REMOTE_TARGET_SITE_ID,
        "worker_execution",
    )?;

    // The generic startup reconciler sees an executable, nonterminal thread
    // with no live process and would normally settle it. It must instead leave
    // the placement untouched until the exact handoff recovery owner reaches
    // its first source-abort boundary.
    daemon
        .respawn_until_handoff_crash_boundary(HandoffCrashBoundary::SourceBeforeAbortPublication)
        .await?;
    daemon.kill_daemon().await?;
    let store = open_daemon_state(&daemon.state_path)?;
    let thread = store
        .get_thread(&seeded.operation.source_placement_thread_id)?
        .context("source placement disappeared during startup reconciliation")?;
    anyhow::ensure!(
        thread.status == ryeos_state::objects::ThreadStatus::Running.as_str(),
        "generic startup reconciliation terminalized an active handoff source"
    );
    drop(store);
    assert_source_recovery_state(
        &daemon.state_path,
        &seeded,
        WorkerHandoffPhase::SourceExported,
        &seeded.source_head_hash,
        0,
    )?;
    Ok(())
}

struct RealPortableHandoff {
    _project: tempfile::TempDir,
    source: DaemonHarness,
    source_fixture: common::fast_fixture::FastFixture,
    target: DaemonHarness,
    target_fixture: common::fast_fixture::FastFixture,
    checkpoint: PortableCheckpoint,
}

impl RealPortableHandoff {
    fn refresh_routes(&self) -> Result<()> {
        install_single_remote(
            &self.target.state_path,
            remote_config(
                PORTABLE_SOURCE_REMOTE,
                format!("http://{}", self.source.bind),
                &self.source_fixture,
                PORTABLE_SOURCE_SITE_ID,
                None,
            )?,
        )?;
        install_single_remote(
            &self.source.state_path,
            remote_config(
                PORTABLE_TARGET_REMOTE,
                format!("http://{}", self.target.bind),
                &self.target_fixture,
                PORTABLE_TARGET_SITE_ID,
                Some((self._project.path(), self._project.path())),
            )?,
        )
    }

    async fn preflight(&self) -> Result<(String, String)> {
        let (status, response) = self
            .source
            .post_execute(
                "service:worker-executions/handoff-preflight",
                ".",
                serde_json::json!({
                    "chain_root_id":self.checkpoint.chain_root_id,
                    "remote":PORTABLE_TARGET_REMOTE,
                    "target_credential_profile_id":PORTABLE_CREDENTIAL_PROFILE_ID,
                }),
            )
            .await?;
        anyhow::ensure!(
            status == reqwest::StatusCode::OK,
            "handoff preflight returned {status}: {response}"
        );
        Ok((
            response
                .pointer("/result/preflight_id")
                .and_then(serde_json::Value::as_str)
                .context("handoff preflight returned no id")?
                .to_owned(),
            response
                .pointer("/result/successor_placement_thread_id")
                .and_then(serde_json::Value::as_str)
                .context("handoff preflight returned no successor")?
                .to_owned(),
        ))
    }

    async fn handoff(
        &self,
        preflight_id: &str,
    ) -> Result<(reqwest::StatusCode, serde_json::Value)> {
        self.source
            .post_execute(
                "service:worker-executions/handoff",
                ".",
                serde_json::json!({
                    "chain_root_id":self.checkpoint.chain_root_id,
                    "manifest_ref":self.checkpoint.manifest_ref,
                    "remote":PORTABLE_TARGET_REMOTE,
                    "target_credential_profile_id":PORTABLE_CREDENTIAL_PROFILE_ID,
                    "preflight_id":preflight_id,
                }),
            )
            .await
    }
}

async fn start_real_portable_handoff() -> Result<RealPortableHandoff> {
    let project = tempfile::tempdir()?;
    std::fs::create_dir_all(project.path().join(".ai"))?;
    std::fs::write(
        project.path().join("fixture.txt"),
        b"portable handoff fixture\n",
    )?;

    let (mut target, target_fixture) = DaemonHarness::start_fast_with_node_key(
        target_node_signing_key(),
        |state_path, _user_space, fixture| plant_portable_worker(state_path, fixture),
        |command| {
            command.env("HOSTNAME", "handoff-target");
        },
    )
    .await?;
    let initial_target_url = format!("http://{}", target.bind);
    let target_project = project.path().to_path_buf();
    let (mut source, source_fixture) = DaemonHarness::start_fast_with(
        |state_path, _user_space, fixture| {
            plant_portable_worker(state_path, fixture)?;
            authorize_remote_node(
                state_path,
                fixture,
                &target_fixture,
                PORTABLE_TARGET_SITE_ID,
                &["ryeos.execute.service.objects/closure/get"],
            )?;
            install_single_remote(
                state_path,
                remote_config(
                    PORTABLE_TARGET_REMOTE,
                    initial_target_url.clone(),
                    &target_fixture,
                    PORTABLE_TARGET_SITE_ID,
                    Some((target_project.as_path(), target_project.as_path())),
                )?,
            )
        },
        |command| {
            command.env("HOSTNAME", "handoff-source");
        },
    )
    .await?;
    let checkpoint = launch_and_checkpoint_portable_worker(
        &mut source,
        project.path(),
        "L-22222222222222222222222222222222",
    )
    .await?;

    source.kill_daemon().await?;
    let source_store = open_daemon_state(&source.state_path)?;
    let source_placement = source_store
        .current_chain_placement_thread_id(&checkpoint.chain_root_id)?
        .context("portable worker chain has no placement")?;
    let source_session = source_store
        .dedicated_session(&source_placement)?
        .context("portable worker has no dedicated session")?;
    anyhow::ensure!(
        source_session.remote_thread_id.as_deref() == Some("handoff-fixture-session")
            && source_session.state == "frozen",
        "portable worker did not retain its exact frozen upstream session"
    );
    let candidate_snapshot_hash = source_session
        .candidate_snapshot_hash
        .context("portable worker checkpoint retained no project candidate")?;
    drop(source_store);
    target.kill_daemon().await?;
    authorize_remote_node(
        &target.state_path,
        &target_fixture,
        &source_fixture,
        PORTABLE_SOURCE_SITE_ID,
        &[
            "ryeos.execute.service.objects/get",
            "ryeos.execute.service.objects/closure/get",
            "ryeos.execute.service.worker-placements/preflight",
            "ryeos.execute.service.worker-placements/prepare",
            "ryeos.execute.service.worker-placements/adopt",
            "ryeos.execute.service.worker-placements/abort",
        ],
    )?;
    install_target_project_head(
        &source.state_path,
        &target.state_path,
        project.path(),
        &format!("fp:{}", source_fixture.user_fp()),
        &candidate_snapshot_hash,
    )?;
    source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    install_single_remote(
        &target.state_path,
        remote_config(
            PORTABLE_SOURCE_REMOTE,
            format!("http://{}", source.bind),
            &source_fixture,
            PORTABLE_SOURCE_SITE_ID,
            None,
        )?,
    )?;
    target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    install_single_remote(
        &source.state_path,
        remote_config(
            PORTABLE_TARGET_REMOTE,
            format!("http://{}", target.bind),
            &target_fixture,
            PORTABLE_TARGET_SITE_ID,
            Some((project.path(), project.path())),
        )?,
    )?;

    Ok(RealPortableHandoff {
        _project: project,
        source,
        source_fixture,
        target,
        target_fixture,
        checkpoint,
    })
}

async fn assert_real_handoff_completed(
    handoff: &mut RealPortableHandoff,
    successor_id: &str,
) -> Result<()> {
    handoff.source.kill_daemon().await?;
    handoff.target.kill_daemon().await?;
    let source_store = open_daemon_state(&handoff.source.state_path)?;
    anyhow::ensure!(
        source_store.current_chain_placement_thread_id(&handoff.checkpoint.chain_root_id)?
            == Some(successor_id.to_owned()),
        "source did not retain the exact successor placement"
    );
    let source_thread = source_store
        .get_thread(&handoff.checkpoint.chain_root_id)?
        .context("source lost its original placement")?;
    anyhow::ensure!(
        source_thread.origin_site_id == PORTABLE_SOURCE_SITE_ID
            && source_thread.status == "continued",
        "source placement was not durably fenced: {source_thread:?}"
    );
    let source_head = source_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &handoff.checkpoint.chain_root_id))?
        .context("source lost its committed chain head")?
        .target_hash;
    anyhow::ensure!(
        source_store
            .append_events_if_thread_running(
                &handoff.checkpoint.chain_root_id,
                &handoff.checkpoint.chain_root_id,
                &[NewEventRecord {
                    event_type: "worker_session.stale_source_probe".to_owned(),
                    storage_class: "indexed".to_owned(),
                    payload: serde_json::json!({"must":"fail"}),
                }],
            )?
            .is_none(),
        "fenced source runtime accepted a stale append"
    );
    anyhow::ensure!(
        source_store
            .with_state_db(
                |db| db.read_generic_head_ref("chains", &handoff.checkpoint.chain_root_id)
            )?
            .is_some_and(|head| head.target_hash == source_head),
        "rejected stale source append still advanced the signed chain head"
    );
    let target_store = open_daemon_state(&handoff.target.state_path)?;
    anyhow::ensure!(
        target_store.current_chain_placement_thread_id(&handoff.checkpoint.chain_root_id)?
            == Some(successor_id.to_owned()),
        "target did not retain the exact adopted placement"
    );
    let adopted = target_store
        .get_thread(successor_id)?
        .context("target lost its adopted successor")?;
    anyhow::ensure!(
        adopted.chain_root_id == handoff.checkpoint.chain_root_id
            && adopted.origin_site_id == PORTABLE_SOURCE_SITE_ID
            && adopted.current_site_id == PORTABLE_TARGET_SITE_ID,
        "adopted placement changed stable or provenance identity: {adopted:?}"
    );
    let reservation = target_store
        .credential_profile_reservation_for_successor(successor_id)?
        .context("completed target lost its credential reservation evidence")?;
    anyhow::ensure!(
        reservation.state == "consumed"
            && reservation.successor_thread_id == successor_id
            && reservation.profile_id == PORTABLE_CREDENTIAL_PROFILE_ID,
        "completed target retained a live or substituted credential lease: {reservation:?}"
    );
    let session = target_store
        .dedicated_session(successor_id)?
        .context("completed handoff has no adopted dedicated session")?;
    let worker_instance_id = session
        .worker_instance_id
        .as_deref()
        .context("adopted session has no attached worker")?;
    let worker = target_store
        .worker_process(worker_instance_id)?
        .context("adopted worker projection disappeared")?;
    anyhow::ensure!(
        worker.placement_thread_id == successor_id
            && worker.boot_epoch
                == session
                    .worker_boot_epoch
                    .context("session lost boot epoch")?
            && worker.state == ryeos_app::runtime_db::WorkerProcessState::Live
            && worker.cleanup_state == "owned",
        "adopted worker authority is not exact: {worker:?}"
    );
    let live_workers = target_store.live_worker_processes()?;
    anyhow::ensure!(
        live_workers
            .iter()
            .filter(|candidate| candidate.placement_thread_id == successor_id)
            .count()
            == 1,
        "completed handoff retained duplicate live worker authority: {live_workers:?}"
    );
    Ok(())
}

async fn assert_real_handoff_aborted(
    handoff: &mut RealPortableHandoff,
    successor_id: &str,
) -> Result<()> {
    handoff.source.kill_daemon().await?;
    handoff.target.kill_daemon().await?;
    let source_store = open_daemon_state(&handoff.source.state_path)?;
    anyhow::ensure!(
        source_store.current_chain_placement_thread_id(&handoff.checkpoint.chain_root_id)?
            == Some(handoff.checkpoint.chain_root_id.clone()),
        "pre-cut recovery did not preserve the source as the exact writer"
    );
    let source_thread = source_store
        .get_thread(&handoff.checkpoint.chain_root_id)?
        .context("aborted handoff lost its source placement")?;
    anyhow::ensure!(
        source_thread.status == ryeos_state::objects::ThreadStatus::Running.as_str(),
        "aborted handoff terminalized the continuing source: {source_thread:?}"
    );
    let cancelled = source_store.with_state_db(|db| {
        db.list_sync_jobs_by_operation_type_and_state(
            WORKER_SESSION_HANDOFF_OPERATION,
            SyncJobState::Cancelled,
            64,
        )
    })?;
    let source_job = cancelled
        .into_iter()
        .find(|job| {
            WorkerSessionHandoffJobOperation::from_value(job.operation.clone()).is_ok_and(
                |operation| {
                    operation.role == WorkerHandoffJobRole::Source
                        && operation.chain_root_id == handoff.checkpoint.chain_root_id
                },
            )
        })
        .context("pre-cut recovery retained no cancelled source handoff job")?;
    anyhow::ensure!(
        source_job.phase == "aborted"
            && serde_json::from_value::<WorkerPlacementAbortResponse>(
                source_job
                    .result
                    .clone()
                    .context("aborted source job retained no receipt")?,
            )
            .is_ok(),
        "pre-cut source job did not settle to its typed abort receipt: {source_job:?}"
    );
    anyhow::ensure!(
        source_store
            .append_events_if_thread_running(
                &handoff.checkpoint.chain_root_id,
                &handoff.checkpoint.chain_root_id,
                &[NewEventRecord {
                    event_type: "worker_session.abort_continuation_probe".to_owned(),
                    storage_class: "indexed".to_owned(),
                    payload: serde_json::json!({"must":"succeed"}),
                }],
            )?
            .is_some(),
        "aborted source could not continue its sole append authority"
    );
    drop(source_store);

    let target_store = open_daemon_state(&handoff.target.state_path)?;
    anyhow::ensure!(
        target_store.get_thread(successor_id)?.is_none()
            && !target_store
                .live_worker_processes()?
                .iter()
                .any(|worker| worker.placement_thread_id == successor_id),
        "aborted target retained successor placement or process authority"
    );
    if let Some(reservation) =
        target_store.credential_profile_reservation_for_successor(successor_id)?
    {
        anyhow::ensure!(
            reservation.state == "released",
            "aborted target retained a live credential reservation: {reservation:?}"
        );
    }
    Ok(())
}

async fn wait_for_handoff_terminal_state(
    daemon: &DaemonHarness,
    chain_root_id: &str,
    expected_state: &str,
) -> Result<serde_json::Value> {
    let terminal_wait = if std::env::var_os("RYEOS_HANDOFF_QUALIFICATION_CASE").is_some() {
        Duration::from_secs(25)
    } else {
        Duration::from_secs(90)
    };
    const OBSERVATION_INTERVAL: Duration = Duration::from_millis(250);

    let mut last_status = None;
    let timer = MonotonicTimer::start();
    while timer.elapsed() < terminal_wait {
        let (status, response) = daemon
            .post_execute(
                "service:worker-executions/status",
                ".",
                serde_json::json!({"chain_root_id":chain_root_id}),
            )
            .await?;
        let observed = response
            .pointer("/result/handoff/state")
            .and_then(serde_json::Value::as_str);
        if status == reqwest::StatusCode::OK && observed == Some(expected_state) {
            return Ok(response);
        }
        if matches!(observed, Some("completed" | "cancelled" | "failed")) {
            anyhow::bail!(
                "handoff recovery settled to {observed:?}, expected `{expected_state}`: {response}"
            );
        }
        last_status = Some((status, response));
        tokio::time::sleep(OBSERVATION_INTERVAL).await;
    }
    anyhow::bail!(
        "handoff recovery never reached terminal state `{expected_state}`: {last_status:?}"
    )
}

async fn await_declared_startup_recovery(
    case: &HandoffAcceptanceCase,
    handoff: &mut RealPortableHandoff,
) -> Result<()> {
    if !matches!(
        case.recovery_trigger,
        RecoveryTrigger::RestartSource
            | RecoveryTrigger::RestartTargetThenRetrySource
            | RecoveryTrigger::RestartTargetThenSourceRecovery
    ) {
        return Ok(());
    }
    let expected_state = match case.operator_outcome {
        OperatorOutcome::Completed => "completed",
        OperatorOutcome::AbortedSourceContinues => "cancelled",
    };
    if let Err(error) = wait_for_handoff_terminal_state(
        &handoff.source,
        &handoff.checkpoint.chain_root_id,
        expected_state,
    )
    .await
    {
        let source_stderr = stderr_tail(&handoff.source.drain_stderr_nonblocking().await, 200);
        let target_stderr = stderr_tail(&handoff.target.drain_stderr_nonblocking().await, 200);
        anyhow::bail!(
            "{error:#}\nsource daemon stderr:\n{source_stderr}\ntarget daemon stderr:\n{target_stderr}"
        );
    }
    Ok(())
}

fn stderr_tail(stderr: &str, max_lines: usize) -> String {
    let lines = stderr.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(max_lines)..].join("\n")
}

async fn post_handoff_request(
    bind: std::net::SocketAddr,
    user_key: &SigningKey,
    node_key: &SigningKey,
    params: serde_json::Value,
) -> Result<(reqwest::StatusCode, serde_json::Value)> {
    let body = serde_json::json!({
        "item_ref":"service:worker-executions/handoff",
        "ref_bindings":{},
        "project_path":null,
        "parameters":params,
        "execution_policy":ryeos_app::execution_policy::ExecutionPolicy::projectless(
            ryeos_app::execution_policy::ExecutionResponse::Wait,
        ),
    });
    let bytes = serde_json::to_vec(&body)?;
    let mut request = reqwest::Client::new()
        .post(format!("http://{bind}/execute"))
        .header("content-type", "application/json")
        .body(bytes.clone());
    for (name, value) in
        common::build_signed_headers_for_bytes(user_key, node_key, "POST", "/execute", &bytes)
    {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    let status = response.status();
    Ok((
        status,
        response
            .json()
            .await
            .unwrap_or_else(|_| serde_json::json!({})),
    ))
}

fn matrix_boundaries(
    node: HandoffNode,
    operator_outcome: OperatorOutcome,
    recovery_request: bool,
) -> Vec<HandoffCrashBoundary> {
    HANDOFF_ACCEPTANCE_MATRIX
        .iter()
        .filter(|case| {
            case.interrupted_node == node
                && case.operator_outcome == operator_outcome
                && matches!(
                    case.request_outcome_at_cut,
                    RequestOutcomeAtCut::RecoveryInterrupted
                ) == recovery_request
        })
        .map(|case| case.boundary)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn measured_millis(measurements: &serde_json::Value, field: &str) -> Result<u64> {
    measurements
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .with_context(|| format!("handoff stage measurements omitted `{field}`"))
}

fn optional_measured_millis(
    measurements: Option<&serde_json::Value>,
    field: &str,
) -> Result<Option<u64>> {
    match measurements.and_then(|value| value.get(field)) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .with_context(|| format!("handoff stage measurement `{field}` is not u64 or null")),
    }
}

fn source_handoff_operation_id(state_path: &Path, chain_root_id: &str) -> Result<String> {
    let store = open_daemon_state(state_path)?;
    let jobs = store.with_state_db(|db| {
        db.list_sync_jobs_by_operation_type_before(WORKER_SESSION_HANDOFF_OPERATION, None, 128)
    })?;
    jobs.into_iter()
        .find_map(|job| {
            WorkerSessionHandoffJobOperation::from_value(job.operation)
                .ok()
                .filter(|operation| {
                    operation.role == WorkerHandoffJobRole::Source
                        && operation.chain_root_id == chain_root_id
                })
                .map(|operation| operation.operation_id)
        })
        .context("qualification source handoff operation disappeared")
}

fn object_schema_version(value: &serde_json::Value) -> Option<(String, u32)> {
    let schema = value.get("schema");
    let schema_name = schema.and_then(serde_json::Value::as_str);
    let name = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .or(schema_name)?;
    let version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| schema.and_then(serde_json::Value::as_u64))
        .and_then(|version| u32::try_from(version).ok())
        .or_else(|| {
            schema_name
                .and_then(|schema| schema.rsplit_once(".v"))
                .and_then(|(_, version)| version.parse::<u32>().ok())
        })?;
    Some((name.to_owned(), version))
}

fn build_handoff_measurement_record(
    source_state_path: &Path,
    operation_id: &str,
    target_before_transfer: &CasEntrySnapshot,
    case_id: &str,
    failure_cut: Option<HandoffCrashBoundary>,
    measurements: Option<&serde_json::Value>,
    total_handoff_recovery_ms: u64,
) -> Result<HandoffMeasurementRecord> {
    let source = open_daemon_state(source_state_path)?;
    let job = source
        .with_state_db(|db| db.get_sync_job(&format!("worker-handoff-source:{operation_id}")))?
        .context("measurement source job disappeared")?;
    let operation = WorkerSessionHandoffJobOperation::from_value(job.operation)?;
    let authority = source.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    authority.ensure_guard(&guard)?;
    let cas = authority.cas_store()?;
    let closure_timer = lillux::time::MonotonicTimer::start();
    let closure = ryeos_state::object_closure::collect_object_closure_with_cas_and_limits(
        &cas,
        [operation.transfer_manifest_hash.clone()],
        ryeos_state::object_closure::ObjectClosureLimits::default(),
    )?;
    let closure_calculation_ms = closure_timer.elapsed_millis();
    anyhow::ensure!(
        closure.is_complete() && closure.large_object_hashes.is_empty(),
        "measured portable handoff closure is incomplete: {closure:?}"
    );

    let mut object_schema_versions = BTreeMap::new();
    let mut link_count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut largest_entry_bytes = 0_u64;
    let mut target_present_entries = 0_u64;
    let mut target_present_bytes = 0_u64;
    for hash in &closure.object_hashes {
        let value = cas
            .get_object(hash)?
            .with_context(|| format!("measured closure object {hash} disappeared"))?;
        let bytes = u64::try_from(lillux::canonical_json(&value)?.len())?;
        let links =
            ryeos_state::object_closure::object_links(&value).map_err(anyhow::Error::msg)?;
        link_count = link_count.saturating_add(u64::try_from(
            links.object_hashes.len() + links.blob_hashes.len() + links.large_object_hashes.len(),
        )?);
        if let Some((schema, version)) = object_schema_version(&value) {
            if let Some(previous) = object_schema_versions.insert(schema.clone(), version) {
                anyhow::ensure!(
                    previous == version,
                    "measured closure mixed versions for schema `{schema}`"
                );
            }
        }
        total_bytes = total_bytes.saturating_add(bytes);
        largest_entry_bytes = largest_entry_bytes.max(bytes);
        if target_before_transfer.contains_key(&("objects".to_owned(), hash.clone())) {
            target_present_entries = target_present_entries.saturating_add(1);
            target_present_bytes = target_present_bytes.saturating_add(bytes);
        }
    }
    for hash in &closure.blob_hashes {
        let bytes = u64::try_from(
            cas.get_blob(hash)?
                .with_context(|| format!("measured closure blob {hash} disappeared"))?
                .len(),
        )?;
        total_bytes = total_bytes.saturating_add(bytes);
        largest_entry_bytes = largest_entry_bytes.max(bytes);
        if target_before_transfer.contains_key(&("blobs".to_owned(), hash.clone())) {
            target_present_entries = target_present_entries.saturating_add(1);
            target_present_bytes = target_present_bytes.saturating_add(bytes);
        }
    }
    authority.ensure_guard(&guard)?;

    Ok(HandoffMeasurementRecord {
        schema: "ryeos.worker_handoff_qualification_record.v1".to_owned(),
        case_id: case_id.to_owned(),
        workload_profile_id: PORTABLE_WORKER_REF.to_owned(),
        source_site_id: PORTABLE_SOURCE_SITE_ID.to_owned(),
        target_site_id: PORTABLE_TARGET_SITE_ID.to_owned(),
        object_schema_versions,
        failure_cut,
        cache_state: "measured_before_transfer".to_owned(),
        object_count: u64::try_from(closure.object_hashes.len())?,
        blob_count: u64::try_from(closure.blob_hashes.len())?,
        link_count,
        total_bytes,
        largest_entry_bytes,
        target_present_entries,
        target_present_bytes,
        observed: HandoffObservedMeasurements {
            closure_calculation_ms: Some(closure_calculation_ms),
            staging_and_transfer_ms: optional_measured_millis(measurements, "target_prepare_ms")?,
            closure_verification_ms: optional_measured_millis(
                measurements,
                "closure_verification_ms",
            )?,
            source_publication_ms: optional_measured_millis(measurements, "source_publication_ms")?,
            target_adoption_ms: optional_measured_millis(measurements, "target_adoption_ms")?,
            checkpoint_load_ms: optional_measured_millis(measurements, "checkpoint_load_ms")?,
            event_replay_ms: optional_measured_millis(measurements, "event_replay_ms")?,
            project_materialization_ms: optional_measured_millis(
                measurements,
                "project_materialization_ms",
            )?,
            worker_attach_recovery_ms: optional_measured_millis(
                measurements,
                "worker_attach_recovery_ms",
            )?,
            total_handoff_recovery_ms: Some(total_handoff_recovery_ms),
        },
    })
}

fn build_abort_payload_measurement_record(
    case_id: &str,
    boundary: HandoffCrashBoundary,
    source: &SourceAbortAuthority,
    total_handoff_recovery_ms: u64,
) -> Result<HandoffMeasurementRecord> {
    let timer = lillux::time::MonotonicTimer::start();
    let target_present = source
        .source_head_payload
        .entries
        .iter()
        .map(|entry| (entry.is_blob, entry.hash.as_str()))
        .collect::<BTreeSet<_>>();
    let mut object_schema_versions = BTreeMap::new();
    let mut object_count = 0_u64;
    let mut blob_count = 0_u64;
    let mut link_count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut largest_entry_bytes = 0_u64;
    let mut target_present_entries = 0_u64;
    let mut target_present_bytes = 0_u64;
    for entry in &source.abort_payload.entries {
        let bytes = u64::try_from(entry.data.len())?;
        total_bytes = total_bytes.saturating_add(bytes);
        largest_entry_bytes = largest_entry_bytes.max(bytes);
        if entry.is_blob {
            blob_count = blob_count.saturating_add(1);
        } else {
            object_count = object_count.saturating_add(1);
            let value: serde_json::Value = serde_json::from_slice(&entry.data)?;
            let links =
                ryeos_state::object_closure::object_links(&value).map_err(anyhow::Error::msg)?;
            link_count = link_count.saturating_add(u64::try_from(
                links.object_hashes.len()
                    + links.blob_hashes.len()
                    + links.large_object_hashes.len(),
            )?);
            if let Some((schema, version)) = object_schema_version(&value) {
                if let Some(previous) = object_schema_versions.insert(schema.clone(), version) {
                    anyhow::ensure!(
                        previous == version,
                        "abort closure mixed versions for schema `{schema}`"
                    );
                }
            }
        }
        if target_present.contains(&(entry.is_blob, entry.hash.as_str())) {
            target_present_entries = target_present_entries.saturating_add(1);
            target_present_bytes = target_present_bytes.saturating_add(bytes);
        }
    }
    Ok(HandoffMeasurementRecord {
        schema: "ryeos.worker_handoff_qualification_record.v1".to_owned(),
        case_id: case_id.to_owned(),
        workload_profile_id: "worker:handoff-fixture/target-abort".to_owned(),
        source_site_id: REMOTE_SOURCE_SITE_ID.to_owned(),
        target_site_id: DAEMON_TARGET_SITE_ID.to_owned(),
        object_schema_versions,
        failure_cut: Some(boundary),
        cache_state: "measured_pre_abort_import".to_owned(),
        object_count,
        blob_count,
        link_count,
        total_bytes,
        largest_entry_bytes,
        target_present_entries,
        target_present_bytes,
        observed: HandoffObservedMeasurements {
            closure_calculation_ms: Some(timer.elapsed_millis()),
            staging_and_transfer_ms: None,
            closure_verification_ms: None,
            source_publication_ms: None,
            target_adoption_ms: None,
            checkpoint_load_ms: None,
            event_replay_ms: None,
            project_materialization_ms: None,
            worker_attach_recovery_ms: None,
            total_handoff_recovery_ms: Some(total_handoff_recovery_ms),
        },
    })
}

async fn crash_real_source_handoff_at(
    boundary: HandoffCrashBoundary,
) -> Result<(
    RealPortableHandoff,
    String,
    String,
    CasEntrySnapshot,
    HandoffPhaseCutEvidence,
)> {
    let mut handoff = start_real_portable_handoff().await?;
    let (preflight_id, successor_id) = handoff.preflight().await?;
    handoff.target.kill_daemon().await?;
    let target_before_transfer = snapshot_cas_entries(&handoff.target.state_path)?;
    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff.refresh_routes()?;
    handoff.source.kill_daemon().await?;
    let mut gate = handoff
        .source
        .respawn_with_handoff_crash_gate(boundary, |command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    handoff.refresh_routes()?;
    let request = serde_json::json!({
        "chain_root_id":handoff.checkpoint.chain_root_id,
        "manifest_ref":handoff.checkpoint.manifest_ref,
        "remote":PORTABLE_TARGET_REMOTE,
        "target_credential_profile_id":PORTABLE_CREDENTIAL_PROFILE_ID,
        "preflight_id":preflight_id,
    });
    let source_bind = handoff.source.bind;
    let source_user_key = handoff
        .source
        .user_key
        .as_ref()
        .context("source user key missing")?
        .clone();
    let source_node_key = handoff
        .source
        .node_key
        .as_ref()
        .context("source node key missing")?
        .clone();
    let request_task = tokio::spawn(async move {
        post_handoff_request(source_bind, &source_user_key, &source_node_key, request).await
    });
    let cut_evidence = gate.wait_reached().await?;
    handoff.source.kill_daemon().await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), request_task).await;
    Ok((
        handoff,
        preflight_id,
        successor_id,
        target_before_transfer,
        cut_evidence,
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn real_portable_worker_completes_cross_site_handoff() -> Result<()> {
    let mut handoff = start_real_portable_handoff().await?;
    let (preflight_id, successor_id) = handoff.preflight().await?;
    handoff.target.kill_daemon().await?;
    let target_before_transfer = snapshot_cas_entries(&handoff.target.state_path)?;
    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff.refresh_routes()?;
    let (status, response) = handoff.handoff(&preflight_id).await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "real handoff returned {status}: {response}"
    );
    anyhow::ensure!(
        response
            .pointer("/result/placement_thread_id")
            .and_then(serde_json::Value::as_str)
            == Some(successor_id.as_str()),
        "handoff response changed its exact successor: {response}"
    );
    let operation_id = response
        .pointer("/result/operation_id")
        .and_then(serde_json::Value::as_str)
        .context("handoff response returned no operation id")?
        .to_owned();
    let measurements = response
        .pointer("/result/qualification_measurements")
        .context("qualified handoff returned no stage measurements")?;
    anyhow::ensure!(
        measurements
            .get("schema")
            .and_then(serde_json::Value::as_str)
            == Some("ryeos.worker_handoff_stage_measurements.v1")
            && measurements
                .get("total_handoff_ms")
                .and_then(serde_json::Value::as_u64)
                .is_some(),
        "qualified handoff returned malformed stage measurements: {measurements}"
    );
    let (target_status_code, target_status) = handoff
        .target
        .post_execute(
            "service:worker-executions/status",
            ".",
            serde_json::json!({"chain_root_id":handoff.checkpoint.chain_root_id}),
        )
        .await?;
    anyhow::ensure!(
        target_status_code == reqwest::StatusCode::OK
            && target_status
                .pointer("/result/handoff/state")
                .and_then(serde_json::Value::as_str)
                == Some("completed")
            && target_status
                .pointer("/result/handoff/phase")
                .and_then(serde_json::Value::as_str)
                == Some("completed")
            && target_status
                .pointer("/result/handoff/terminal_disposition")
                .and_then(serde_json::Value::as_str)
                == Some("completed")
            && target_status
                .pointer("/result/handoff/recovery_required")
                .and_then(serde_json::Value::as_bool)
                == Some(false),
        "target status did not explain the terminal handoff: {target_status_code} {target_status}"
    );
    let (retry_status, retry) = handoff.handoff(&preflight_id).await?;
    anyhow::ensure!(
        retry_status == reqwest::StatusCode::OK
            && retry
                .pointer("/result/placement_thread_id")
                .and_then(serde_json::Value::as_str)
                == Some(successor_id.as_str()),
        "completed handoff retry minted or returned another successor: {retry_status} {retry}"
    );
    assert_real_handoff_completed(&mut handoff, &successor_id).await?;
    let record = build_handoff_measurement_record(
        &handoff.source.state_path,
        &operation_id,
        &target_before_transfer,
        "portable_happy_path",
        None,
        Some(measurements),
        measured_millis(measurements, "total_handoff_ms")?,
    )?;
    let report = HandoffMeasurementReport {
        schema: "ryeos.worker_handoff_qualification_report.v1".to_owned(),
        records: vec![record],
    };
    let canonical = report.canonical_bytes()?;
    let retained: HandoffMeasurementReport = serde_json::from_slice(&canonical)?;
    anyhow::ensure!(
        retained == report,
        "canonical typed measurement report drifted"
    );

    // Operational sync jobs have an ordinary retention horizon. The
    // permanent node-signed receipt must replay the exact target response
    // without contacting the now-offline source or retaining that job.
    let target_store = open_daemon_state(&handoff.target.state_path)?;
    let receipt = target_store
        .worker_handoff_adoption_receipt(&operation_id)?
        .context("completed handoff retained no adoption receipt")?;
    let receipt_hash = target_store
        .worker_handoff_target_branch_hash(&operation_id)?
        .context("completed handoff retained no adoption receipt head")?;
    let (deleted_jobs, _) = target_store
        .with_state_db(|db| db.delete_terminal_sync_jobs_before("9999-12-31T23:59:59Z"))?;
    anyhow::ensure!(deleted_jobs > 0, "qualification retired no terminal jobs");
    anyhow::ensure!(
        target_store
            .with_state_db(|db| db.get_sync_job(&target_job_id(&operation_id)))?
            .is_none(),
        "target handoff job survived the retention qualification"
    );
    anyhow::ensure!(
        target_store.worker_handoff_adoption_receipt(&operation_id)? == Some(receipt.clone()),
        "adoption receipt did not survive terminal-job retention"
    );
    drop(target_store);
    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    let (retained_status, retained_response) = post_remote_adopt(
        handoff.target.bind,
        &handoff.source_fixture.node,
        &handoff.target_fixture.node,
        &receipt.request,
    )
    .await?;
    let retained_result: WorkerPlacementAdoptResult = serde_json::from_value(
        retained_response
            .get("result")
            .cloned()
            .context("retained adoption replay returned no typed result")?,
    )?;
    retained_result.validate()?;
    anyhow::ensure!(
        retained_result.terminal_attestation_hash() == receipt_hash,
        "retained adoption replay changed its terminal attestation head"
    );
    anyhow::ensure!(
        retained_status == reqwest::StatusCode::OK
            && retained_response
                .pointer("/result/response/operation_id")
                .and_then(serde_json::Value::as_str)
                == Some(operation_id.as_str())
            && retained_response
                .pointer("/result/response/placement_thread_id")
                .and_then(serde_json::Value::as_str)
                == Some(successor_id.as_str()),
        "retention-independent adoption replay failed: {retained_status} {retained_response}"
    );
    let mut opposite_operation = receipt.target_operation.clone();
    opposite_operation.role = WorkerHandoffJobRole::Source;
    opposite_operation.peer_remote_name = PORTABLE_TARGET_REMOTE.to_owned();
    opposite_operation.validate()?;
    let (opposite_status, _) = post_remote_abort(
        handoff.target.bind,
        &handoff.source_fixture.node,
        &handoff.target_fixture.node,
        &opposite_operation,
        &"c".repeat(64),
    )
    .await?;
    anyhow::ensure!(
        !opposite_status.is_success(),
        "retained adoption branch admitted the competing abort branch"
    );
    handoff.target.kill_daemon().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn failed_terminal_fetch_retries_in_the_same_source_process() -> Result<()> {
    let mut handoff = start_real_portable_handoff().await?;
    let (preflight_id, successor_id) = handoff.preflight().await?;
    handoff.source.kill_daemon().await?;
    handoff
        .source
        .respawn_with(|command| {
            command
                .env("HOSTNAME", "handoff-source")
                .env("RYEOS_TEST_HANDOFF_FAIL_TERMINAL_FETCH_ONCE", "1");
        })
        .await?;
    handoff.refresh_routes()?;
    let source_pid = handoff.source.child.id();

    let (first_status, first_response) = handoff.handoff(&preflight_id).await?;
    anyhow::ensure!(
        !first_status.is_success() && handoff.source.child.id() == source_pid,
        "injected post-response fetch failure did not return from the live source daemon: {first_status} {first_response}"
    );

    let (second_status, second_response) = handoff.handoff(&preflight_id).await?;
    anyhow::ensure!(
        second_status == reqwest::StatusCode::OK
            && second_response
                .pointer("/result/placement_thread_id")
                .and_then(serde_json::Value::as_str)
                == Some(successor_id.as_str())
            && handoff.source.child.id() == source_pid,
        "healthy retry in the same source process did not settle the exact operation: {second_status} {second_response}"
    );
    let operation_id = second_response
        .pointer("/result/operation_id")
        .and_then(serde_json::Value::as_str)
        .context("same-process retry returned no operation id")?;
    let evidence = live_retry_job_evidence(
        &handoff.source,
        &format!("worker-handoff-source:{operation_id}"),
        WorkerHandoffJobRole::Source,
    )
    .await?
    .context("same-process retry lost its source job")?;
    anyhow::ensure!(
        evidence.state == "completed"
            && evidence.attempt_count >= 2
            && evidence
                .attempts
                .values()
                .any(|attempt| attempt.state == "failed")
            && evidence
                .attempts
                .values()
                .any(|attempt| attempt.state == "completed")
            && evidence
                .attempts
                .values()
                .all(|attempt| attempt.state != "running"),
        "same-process retry did not settle the failed reservation and later success exactly: {evidence:#?}"
    );
    assert_real_handoff_completed(&mut handoff, &successor_id).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn explicit_handoff_acceptance_matrix_emits_retained_report() -> Result<()> {
    let selected_case = std::env::var("RYEOS_HANDOFF_QUALIFICATION_CASE")
        .ok()
        .filter(|value| !value.is_empty());
    let mut records = Vec::with_capacity(HANDOFF_ACCEPTANCE_MATRIX.len());
    for case in HANDOFF_ACCEPTANCE_MATRIX.iter().filter(|case| {
        selected_case
            .as_deref()
            .is_none_or(|selected| selected == case.case_id)
    }) {
        println!("qualifying explicit handoff case {}", case.case_id);
        let recovery_request = matches!(
            case.request_outcome_at_cut,
            RequestOutcomeAtCut::RecoveryInterrupted
        );
        let record = match (case.interrupted_node, recovery_request) {
            (HandoffNode::Source, false) => qualify_source_request_case(case).await?,
            (HandoffNode::Source, true) => qualify_source_recovery_case(case).await?,
            (HandoffNode::Target, false) => qualify_target_request_case(case).await?,
            (HandoffNode::Target, true) => qualify_target_recovery_case(case).await?,
        };
        records.push(record);
    }

    if let Some(selected_case) = selected_case.as_deref() {
        anyhow::ensure!(
            records.len() == 1 && records[0].case_id == selected_case,
            "unknown handoff qualification case `{selected_case}`"
        );
    } else {
        anyhow::ensure!(
            records
                .iter()
                .map(|record| record.case_id.as_str())
                .eq(HANDOFF_ACCEPTANCE_MATRIX.iter().map(|case| case.case_id)),
            "qualification report does not preserve the acceptance-matrix order"
        );
    }
    let report = HandoffMeasurementReport {
        schema: "ryeos.worker_handoff_qualification_report.v1".to_owned(),
        records,
    };
    let bytes = report.canonical_bytes()?;
    if let Some(path) = std::env::var_os("RYEOS_HANDOFF_QUALIFICATION_REPORT") {
        anyhow::ensure!(
            selected_case.is_none(),
            "a retained qualification report requires the complete acceptance matrix"
        );
        let path = std::path::PathBuf::from(path);
        anyhow::ensure!(
            path.is_absolute() && path.parent().is_some_and(Path::exists),
            "handoff qualification report path must have an existing absolute parent"
        );
        lillux::atomic_write_with_mode(&path, &bytes, 0o644)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("retain handoff qualification report {}", path.display()))?;
    }
    println!(
        "RYEOS_HANDOFF_QUALIFICATION_REPORT_JSON={}",
        String::from_utf8(bytes).context("canonical qualification report is not UTF-8")?
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn target_abort_receipt_survives_terminal_job_retention() -> Result<()> {
    let source = create_source_abort_authority()?;
    let (source_url, source_requests, source_server) =
        start_source_closure_server(&source.abort_payload).await?;
    let boundary = HandoffCrashBoundary::TargetAbortReceiptPublished;
    let elapsed = qualify_target_abort_boundary(
        &source,
        &source_url,
        source_requests.as_ref(),
        boundary,
        TargetAbortCutExpectation {
            phase: WorkerHandoffPhase::AbortAuthorized,
            state: SyncJobState::Running,
            abort_root_retained: true,
            reservation_state: "released",
            fence_terminal_disposition: "reservation_released",
        },
    )
    .await?;
    let report = HandoffMeasurementReport {
        schema: "ryeos.worker_handoff_qualification_report.v1".to_owned(),
        records: vec![build_abort_payload_measurement_record(
            "target_abort_retention",
            boundary,
            &source,
            elapsed,
        )?],
    };
    report.validate()?;
    source_server.abort();
    Ok(())
}

async fn crash_real_target_adoption_at(
    boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
) -> Result<(
    RealPortableHandoff,
    String,
    String,
    CasEntrySnapshot,
    HandoffPhaseCutEvidence,
)> {
    let mut handoff = start_real_portable_handoff().await?;
    let (preflight_id, successor_id) = handoff.preflight().await?;
    handoff.target.kill_daemon().await?;
    let target_before_transfer = snapshot_cas_entries(&handoff.target.state_path)?;
    let mut gate = handoff
        .target
        .respawn_with_handoff_crash_gate(boundary, |command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff.refresh_routes()?;
    let request = serde_json::json!({
        "chain_root_id":handoff.checkpoint.chain_root_id,
        "manifest_ref":handoff.checkpoint.manifest_ref,
        "remote":PORTABLE_TARGET_REMOTE,
        "target_credential_profile_id":PORTABLE_CREDENTIAL_PROFILE_ID,
        "preflight_id":preflight_id,
    });
    let source_bind = handoff.source.bind;
    let source_user_key = handoff
        .source
        .user_key
        .as_ref()
        .context("source user key missing")?
        .clone();
    let source_node_key = handoff
        .source
        .node_key
        .as_ref()
        .context("source node key missing")?
        .clone();
    let request_task = tokio::spawn(async move {
        post_handoff_request(source_bind, &source_user_key, &source_node_key, request).await
    });
    let cut_evidence = gate.wait_reached().await?;
    // The source is synchronously waiting on this exact target request. Start
    // both kills in the same poll turn so waiting for target process/UDS
    // cleanup cannot let the broken connection settle the source attempt.
    let (target_killed, source_killed) =
        tokio::join!(handoff.target.kill_daemon(), handoff.source.kill_daemon());
    target_killed?;
    source_killed?;
    let _ = tokio::time::timeout(Duration::from_secs(5), request_task).await;
    Ok((
        handoff,
        preflight_id,
        successor_id,
        target_before_transfer,
        cut_evidence,
    ))
}

async fn qualify_source_request_case(
    case: &HandoffAcceptanceCase,
) -> Result<HandoffMeasurementRecord> {
    let (mut handoff, preflight_id, successor_id, target_before_transfer, cut_evidence) =
        crash_real_source_handoff_at(case.boundary).await?;
    handoff.target.kill_daemon().await?;
    assert_real_handoff_snapshot(&handoff, &successor_id, case.at_cut, "at crash cut")?;
    let retry_evidence_at_cut = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Offline,
    )
    .await?;
    let recovery_timer = MonotonicTimer::start();
    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    handoff.refresh_routes()?;
    await_declared_startup_recovery(case, &mut handoff).await?;
    let retry_evidence_before_retry = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Live,
    )
    .await?;
    let (status, response) = handoff.handoff(&preflight_id).await?;
    let retry_evidence_after_retry = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Live,
    )
    .await?;
    assert_retry_disposition(
        case,
        &retry_evidence_at_cut,
        &retry_evidence_before_retry,
        &retry_evidence_after_retry,
        status,
        &response,
        &cut_evidence,
        &handoff,
        &preflight_id,
        &successor_id,
    )?;
    let stage_measurements = response
        .pointer("/result/qualification_measurements")
        .cloned();
    let total_handoff_recovery_ms = recovery_timer.elapsed_millis();
    match case.operator_outcome {
        OperatorOutcome::Completed => {
            anyhow::ensure!(
                status == reqwest::StatusCode::OK,
                "source crash at {} did not complete on exact retry: {status} {response}",
                case.boundary
            );
            assert_real_handoff_completed(&mut handoff, &successor_id).await?;
        }
        OperatorOutcome::AbortedSourceContinues => {
            anyhow::ensure!(
                !status.is_success(),
                "pre-cut source crash at {} admitted a handoff after abort recovery",
                case.boundary
            );
            assert_real_handoff_aborted(&mut handoff, &successor_id).await?;
        }
    }
    assert_real_handoff_snapshot(
        &handoff,
        &successor_id,
        case.after_recovery,
        "after recovery",
    )?;
    let operation_id = source_handoff_operation_id(
        &handoff.source.state_path,
        &handoff.checkpoint.chain_root_id,
    )?;
    build_handoff_measurement_record(
        &handoff.source.state_path,
        &operation_id,
        &target_before_transfer,
        case.case_id,
        Some(case.boundary),
        stage_measurements.as_ref(),
        total_handoff_recovery_ms,
    )
}

async fn qualify_source_recovery_case(
    case: &HandoffAcceptanceCase,
) -> Result<HandoffMeasurementRecord> {
    let initial_boundary = if case.case_id.ends_with("_no_target") {
        HandoffCrashBoundary::SourceExportPublished
    } else if case.case_id.ends_with("_with_target") {
        HandoffCrashBoundary::SourceBeforeWriterCut
    } else {
        anyhow::bail!("source recovery case has no target-presence context");
    };
    let (mut handoff, preflight_id, successor_id, target_before_transfer, cut_evidence) =
        crash_real_source_handoff_at(initial_boundary).await?;
    let recovery_cut_evidence = handoff
        .source
        .respawn_until_handoff_crash_boundary(case.boundary)
        .await?;
    anyhow::ensure!(
        recovery_cut_evidence.operation_id == cut_evidence.operation_id
            && recovery_cut_evidence.operation_digest == cut_evidence.operation_digest,
        "source recovery crash gate changed the full live operation"
    );
    handoff.source.kill_daemon().await?;
    handoff.target.kill_daemon().await?;
    assert_real_handoff_snapshot(
        &handoff,
        &successor_id,
        case.at_cut,
        "at recovery crash cut",
    )?;
    let retry_evidence_at_cut = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Offline,
    )
    .await?;
    let recovery_timer = MonotonicTimer::start();

    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    handoff.refresh_routes()?;
    await_declared_startup_recovery(case, &mut handoff).await?;
    let retry_evidence_before_retry = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Live,
    )
    .await?;
    let (status, response) = handoff.handoff(&preflight_id).await?;
    let retry_evidence_after_retry = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Live,
    )
    .await?;
    assert_retry_disposition(
        case,
        &retry_evidence_at_cut,
        &retry_evidence_before_retry,
        &retry_evidence_after_retry,
        status,
        &response,
        &recovery_cut_evidence,
        &handoff,
        &preflight_id,
        &successor_id,
    )?;
    anyhow::ensure!(
        !status.is_success(),
        "source abort recovery at {} admitted the discarded handoff",
        case.boundary
    );
    let total_handoff_recovery_ms = recovery_timer.elapsed_millis();
    assert_real_handoff_aborted(&mut handoff, &successor_id).await?;
    assert_real_handoff_snapshot(
        &handoff,
        &successor_id,
        case.after_recovery,
        "after recovery",
    )?;
    let operation_id = source_handoff_operation_id(
        &handoff.source.state_path,
        &handoff.checkpoint.chain_root_id,
    )?;
    build_handoff_measurement_record(
        &handoff.source.state_path,
        &operation_id,
        &target_before_transfer,
        case.case_id,
        Some(case.boundary),
        None,
        total_handoff_recovery_ms,
    )
}

async fn qualify_target_request_case(
    case: &HandoffAcceptanceCase,
) -> Result<HandoffMeasurementRecord> {
    let (mut handoff, preflight_id, successor_id, target_before_transfer, cut_evidence) =
        crash_real_target_adoption_at(case.boundary).await?;
    assert_real_handoff_snapshot(&handoff, &successor_id, case.at_cut, "at target crash cut")?;
    let retry_evidence_at_cut = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Offline,
    )
    .await?;
    let recovery_timer = MonotonicTimer::start();
    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    handoff.refresh_routes()?;
    await_declared_startup_recovery(case, &mut handoff).await?;
    let retry_evidence_before_retry = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Live,
    )
    .await?;
    let (status, response) = handoff.handoff(&preflight_id).await?;
    let retry_evidence_after_retry = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Live,
    )
    .await?;
    assert_retry_disposition(
        case,
        &retry_evidence_at_cut,
        &retry_evidence_before_retry,
        &retry_evidence_after_retry,
        status,
        &response,
        &cut_evidence,
        &handoff,
        &preflight_id,
        &successor_id,
    )?;
    let stage_measurements = response
        .pointer("/result/qualification_measurements")
        .cloned();
    let total_handoff_recovery_ms = recovery_timer.elapsed_millis();
    match case.operator_outcome {
        OperatorOutcome::Completed => {
            anyhow::ensure!(
                status == reqwest::StatusCode::OK,
                "target crash at {} did not complete on exact retry: {status} {response}",
                case.boundary
            );
            assert_real_handoff_completed(&mut handoff, &successor_id).await?;
        }
        OperatorOutcome::AbortedSourceContinues => {
            anyhow::ensure!(
                !status.is_success(),
                "pre-cut target crash at {} admitted a handoff after abort recovery",
                case.boundary
            );
            assert_real_handoff_aborted(&mut handoff, &successor_id).await?;
        }
    }
    assert_real_handoff_snapshot(
        &handoff,
        &successor_id,
        case.after_recovery,
        "after recovery",
    )?;
    let operation_id = source_handoff_operation_id(
        &handoff.source.state_path,
        &handoff.checkpoint.chain_root_id,
    )?;
    build_handoff_measurement_record(
        &handoff.source.state_path,
        &operation_id,
        &target_before_transfer,
        case.case_id,
        Some(case.boundary),
        stage_measurements.as_ref(),
        total_handoff_recovery_ms,
    )
}

async fn qualify_target_recovery_case(
    case: &HandoffAcceptanceCase,
) -> Result<HandoffMeasurementRecord> {
    let (mut handoff, preflight_id, successor_id, target_before_transfer, cut_evidence) =
        crash_real_source_handoff_at(HandoffCrashBoundary::SourceBeforeWriterCut).await?;
    handoff.target.kill_daemon().await?;
    let mut gate = handoff
        .target
        .respawn_with_handoff_crash_gate(case.boundary, |command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    handoff.refresh_routes()?;
    let recovery_cut_evidence = gate.wait_reached().await?;
    anyhow::ensure!(
        recovery_cut_evidence.operation_id == cut_evidence.operation_id,
        "target recovery crash gate changed the live operation identity"
    );
    // Preserve the exact recovery cut: do not let target cleanup latency give
    // the synchronously waiting source time to settle the interrupted abort.
    let (target_killed, source_killed) =
        tokio::join!(handoff.target.kill_daemon(), handoff.source.kill_daemon());
    target_killed?;
    source_killed?;
    assert_real_handoff_snapshot(
        &handoff,
        &successor_id,
        case.at_cut,
        "at target recovery crash cut",
    )?;
    let retry_evidence_at_cut = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Offline,
    )
    .await?;
    let cut_source_operation = retry_evidence_at_cut
        .source_job
        .as_ref()
        .context("target recovery cut retained no source operation")?;
    let cut_target_operation = retry_evidence_at_cut
        .target_job
        .as_ref()
        .context("target recovery cut retained no target operation")?;
    anyhow::ensure!(
        cut_evidence.operation_digest
            == ryeos_state::objects::canonical_value_digest(
                &cut_source_operation.operation.to_value()?
            )?
            && recovery_cut_evidence.operation_digest
                == ryeos_state::objects::canonical_value_digest(
                    &cut_target_operation.operation.to_value()?
                )?,
        "two-cut target recovery changed the full source or target operation"
    );
    let recovery_timer = MonotonicTimer::start();

    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    handoff.refresh_routes()?;
    await_declared_startup_recovery(case, &mut handoff).await?;
    let retry_evidence_before_retry = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Live,
    )
    .await?;
    let (status, response) = handoff.handoff(&preflight_id).await?;
    let retry_evidence_after_retry = observe_handoff_retry_evidence(
        &handoff,
        &successor_id,
        &cut_evidence.operation_id,
        RetryObservationMode::Live,
    )
    .await?;
    assert_retry_disposition(
        case,
        &retry_evidence_at_cut,
        &retry_evidence_before_retry,
        &retry_evidence_after_retry,
        status,
        &response,
        &recovery_cut_evidence,
        &handoff,
        &preflight_id,
        &successor_id,
    )?;
    anyhow::ensure!(
        !status.is_success(),
        "target abort recovery at {} admitted the discarded handoff",
        case.boundary
    );
    let total_handoff_recovery_ms = recovery_timer.elapsed_millis();
    assert_real_handoff_aborted(&mut handoff, &successor_id).await?;
    assert_real_handoff_snapshot(
        &handoff,
        &successor_id,
        case.after_recovery,
        "after recovery",
    )?;
    let operation_id = source_handoff_operation_id(
        &handoff.source.state_path,
        &handoff.checkpoint.chain_root_id,
    )?;
    build_handoff_measurement_record(
        &handoff.source.state_path,
        &operation_id,
        &target_before_transfer,
        case.case_id,
        Some(case.boundary),
        None,
        total_handoff_recovery_ms,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn projected_target_attachment_survives_worker_reap_before_receipt_recovery() -> Result<()> {
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let (mut handoff, preflight_id, successor_id, _, _) =
        crash_real_target_adoption_at(HandoffCrashBoundary::TargetProcessAttachmentProjected)
            .await?;
    let target_store = open_daemon_state(&handoff.target.state_path)?;
    let target_jobs = target_store.with_state_db(|db| {
        db.list_active_sync_jobs_by_operation_type(WORKER_SESSION_HANDOFF_OPERATION, 16)
    })?;
    let target_job = target_jobs
        .into_iter()
        .find(|job| {
            WorkerSessionHandoffJobOperation::from_value(job.operation.clone()).is_ok_and(
                |operation| {
                    operation.role == WorkerHandoffJobRole::Target
                        && operation.successor_placement_thread_id == successor_id
                },
            )
        })
        .context("projected attachment retained no target handoff job")?;
    let operation = WorkerSessionHandoffJobOperation::from_value(target_job.operation.clone())?;
    let progress = WorkerSessionHandoffProgress::from_value(
        target_job
            .result
            .clone()
            .context("projected attachment retained no progress")?,
    )?;
    anyhow::ensure!(
        progress.phase == WorkerHandoffPhase::ProcessAttached
            && target_store
                .worker_handoff_adoption_receipt(&operation.operation_id)?
                .is_none(),
        "attachment-projection cut crossed the permanent receipt boundary"
    );
    let session = target_store
        .dedicated_session(&successor_id)?
        .context("projected attachment retained no dedicated session")?;
    let worker_id = session
        .worker_instance_id
        .as_deref()
        .context("projected attachment retained no worker identity")?;
    let worker = target_store
        .worker_process(worker_id)?
        .context("projected attachment lost its worker process")?;
    let killed = ryeos_app::process::kill_by_action(
        &worker.process_identity,
        ryeos_app::process::ShutdownAction::Hard,
    );
    anyhow::ensure!(
        killed.success,
        "could not prove worker terminalization at attachment cut: {}",
        killed.method
    );
    target_store.fence_abandoned_worker_process(
        &worker.worker_instance_id,
        &successor_id,
        worker.boot_epoch,
        "reaped",
    )?;
    anyhow::ensure!(
        target_store
            .dedicated_session(&successor_id)?
            .is_some_and(|session| session.worker_instance_id.is_none()),
        "qualification did not clear the reaped worker binding before recovery"
    );
    drop(target_store);

    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff.refresh_routes()?;
    let (status, response) = handoff.handoff(&preflight_id).await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK
            && response
                .pointer("/result/placement_thread_id")
                .and_then(serde_json::Value::as_str)
                == Some(successor_id.as_str()),
        "reaped attachment recovery did not settle both handoff jobs: {status} {response}"
    );
    let operation_id = response
        .pointer("/result/operation_id")
        .and_then(serde_json::Value::as_str)
        .context("reaped attachment recovery returned no operation id")?;
    let target_store = open_daemon_state(&handoff.target.state_path)?;
    anyhow::ensure!(
        target_store
            .worker_handoff_adoption_receipt(operation_id)?
            .is_some()
            && target_store
                .with_state_db(|db| db.get_sync_job(&target_job_id(operation_id)))?
                .is_some_and(|job| job.state == SyncJobState::Completed),
        "reaped attachment recovery did not retain receipt and terminal job"
    );
    drop(target_store);
    handoff.source.kill_daemon().await?;
    handoff.target.kill_daemon().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn durable_stop_cut_on_created_remote_adoption_settles_before_target_recovery() -> Result<()>
{
    use ryeos_app::state_store::StopIntent;
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    // Stop at the exact current-format state in which the target owns the
    // signed continuation, but no worker/session has attached. Persist only
    // the cancellation tombstone: this is the durable cut after
    // `request_thread_stop` and before the request handler terminalizes the
    // thread.
    let (mut handoff, _preflight_id, successor_id, _, cut) =
        crash_real_target_adoption_at(HandoffCrashBoundary::TargetAdoptionProjected).await?;
    let target_store = open_daemon_state(&handoff.target.state_path)?;
    let target_job = find_handoff_job(
        &target_store,
        &handoff.checkpoint.chain_root_id,
        WorkerHandoffJobRole::Target,
    )?
    .context("adopted target retained no handoff job")?;
    let operation = WorkerSessionHandoffJobOperation::from_value(target_job.operation.clone())?;
    anyhow::ensure!(
        operation.operation_id == cut.operation_id
            && operation.successor_placement_thread_id == successor_id,
        "durable-stop fixture selected another target operation"
    );
    let attempts_before =
        target_store.with_state_db(|db| db.list_sync_job_attempts(&target_job.job_id))?;
    let attempt_coordinates_before = attempts_before
        .iter()
        .map(|attempt| {
            (
                attempt.attempt_id.clone(),
                attempt.attempt_number,
                attempt.worker_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        target_job.attempt_count == u64::try_from(attempts_before.len())?
            && attempts_before
                .iter()
                .any(|attempt| attempt.state == SyncJobAttemptState::Running),
        "target adoption cut retained no exact interrupted attempt"
    );
    let reservation_before = target_store
        .credential_profile_reservation_for_successor(&successor_id)?
        .context("adopted target retained no credential reservation")?;
    let profile_before = target_store
        .credential_profile(&reservation_before.profile_id)?
        .context("adopted target retained no credential profile")?;
    anyhow::ensure!(
        reservation_before.state == "reserved"
            && profile_before.lock_owner.as_deref()
                == Some(reservation_before.reservation_id.as_str())
            && target_store.dedicated_session(&successor_id)?.is_none(),
        "pre-attachment target cut already consumed or lost its reservation"
    );
    let stopped = target_store.request_thread_stop(&successor_id, StopIntent::Cancel)?;
    anyhow::ensure!(
        stopped.stop_requested_at_ms.is_some()
            && stopped.stop_intent == Some(StopIntent::Cancel)
            && stopped.process_identity.is_none(),
        "durable stop cut did not retain the exact unattached cancellation intent"
    );
    drop(target_store);

    // Target startup must run generic stop recovery before handing the
    // successor back to target-handoff recovery. The source remains offline:
    // no peer request can manufacture settlement.
    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    let settlement_timer = MonotonicTimer::start();
    loop {
        let settled = live_retry_job_evidence(
            &handoff.target,
            &target_job.job_id,
            WorkerHandoffJobRole::Target,
        )
        .await?
        .is_some_and(|job| {
            job.state == "failed" && job.phase == "target_terminal_before_attachment"
        });
        if settled {
            break;
        }
        anyhow::ensure!(
            settlement_timer.elapsed() < Duration::from_secs(20),
            "target recovery did not settle the durable pre-attachment stop:\n{}",
            stderr_tail(&handoff.target.drain_stderr_nonblocking().await, 200)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    handoff.target.kill_daemon().await?;

    let settled_store = open_daemon_state_with_trusted_nodes(
        &handoff.target.state_path,
        &[&handoff.source_fixture.node, &handoff.target_fixture.node],
    )?;
    let successor = settled_store
        .get_thread(&successor_id)?
        .context("settled target successor disappeared")?;
    let settled_job = settled_store
        .with_state_db(|db| db.get_sync_job(&target_job.job_id))?
        .context("settled target handoff job disappeared")?;
    let attempts_after =
        settled_store.with_state_db(|db| db.list_sync_job_attempts(&target_job.job_id))?;
    let attempt_coordinates_after = attempts_after
        .iter()
        .map(|attempt| {
            (
                attempt.attempt_id.clone(),
                attempt.attempt_number,
                attempt.worker_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    let reservation_after = settled_store
        .credential_profile_reservation_for_successor(&successor_id)?
        .context("settled target lost its credential reservation testimony")?;
    let profile_after = settled_store
        .credential_profile(&reservation_after.profile_id)?
        .context("settled target lost its credential profile")?;
    let terminal_head = settled_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &handoff.checkpoint.chain_root_id))?
        .context("settled target retained no terminal chain head")?;
    let failure_receipt = settled_store
        .worker_handoff_terminal_failure(&operation.operation_id)?
        .context("settled target retained no signed terminal-failure receipt")?;
    let failure_receipt_hash = settled_store
        .worker_handoff_target_branch_hash(&operation.operation_id)?
        .context("settled target retained no terminal-failure branch head")?;
    anyhow::ensure!(
        successor.status == "cancelled"
            && successor.started_at.is_none()
            && successor.runtime.stop_intent == Some(StopIntent::Cancel)
            && settled_store.dedicated_session(&successor_id)?.is_none()
            && settled_job.state == SyncJobState::Failed
            && settled_job.phase == "target_terminal_before_attachment"
            && settled_job.operation == target_job.operation
            && settled_job.attempt_count == target_job.attempt_count
            && attempt_coordinates_after == attempt_coordinates_before
            && attempts_after
                .iter()
                .all(|attempt| attempt.state != SyncJobAttemptState::Running)
            && settled_job.heads == [terminal_head.target_hash.clone()]
            && settled_job
                .roots
                .iter()
                .any(|root| root == &failure_receipt_hash)
            && failure_receipt.target_operation == operation
            && failure_receipt.failure.target_chain_head_hash == terminal_head.target_hash
            && failure_receipt.failure.terminal_status == "cancelled"
            && failure_receipt.failure.credential_disposition == "reservation_released"
            && reservation_after.reservation_id == reservation_before.reservation_id
            && reservation_after.state == "released"
            && profile_after.lock_owner.is_none(),
        "durable pre-attachment stop did not settle the exact thread/job/reservation authority"
    );
    let first_settlement = (
        settled_job.operation.clone(),
        settled_job.state,
        settled_job.phase.clone(),
        settled_job.heads.clone(),
        settled_job.attempt_count,
        attempt_coordinates_after,
        successor.status.clone(),
        successor.finished_at.clone(),
        reservation_after.state.clone(),
        profile_after.lock_owner.clone(),
    );
    drop(settled_store);

    // Once the permanent signed terminal branch and terminal operational job
    // agree, a later restart need not rescan or republish them. Prove startup
    // is a no-op over every authority coordinate.
    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff.target.kill_daemon().await?;
    let twice_settled_store = open_daemon_state_with_trusted_nodes(
        &handoff.target.state_path,
        &[&handoff.source_fixture.node, &handoff.target_fixture.node],
    )?;
    let twice_settled_job = twice_settled_store
        .with_state_db(|db| db.get_sync_job(&target_job.job_id))?
        .context("second target recovery lost its terminal job")?;
    let twice_settled_attempts =
        twice_settled_store.with_state_db(|db| db.list_sync_job_attempts(&target_job.job_id))?;
    let twice_settled_thread = twice_settled_store
        .get_thread(&successor_id)?
        .context("second target recovery lost its terminal successor")?;
    let twice_settled_reservation = twice_settled_store
        .credential_profile_reservation_for_successor(&successor_id)?
        .context("second target recovery lost reservation testimony")?;
    let twice_settled_profile = twice_settled_store
        .credential_profile(&twice_settled_reservation.profile_id)?
        .context("second target recovery lost its credential profile")?;
    anyhow::ensure!(
        twice_settled_store.worker_handoff_terminal_failure(&operation.operation_id)?
            == Some(failure_receipt.clone())
            && twice_settled_store.worker_handoff_target_branch_hash(&operation.operation_id)?
                == Some(failure_receipt_hash.clone()),
        "second target recovery changed its signed terminal-failure branch"
    );
    let twice_coordinates = twice_settled_attempts
        .iter()
        .map(|attempt| {
            (
                attempt.attempt_id.clone(),
                attempt.attempt_number,
                attempt.worker_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        first_settlement
            == (
                twice_settled_job.operation,
                twice_settled_job.state,
                twice_settled_job.phase,
                twice_settled_job.heads,
                twice_settled_job.attempt_count,
                twice_coordinates,
                twice_settled_thread.status,
                twice_settled_thread.finished_at,
                twice_settled_reservation.state,
                twice_settled_profile.lock_owner,
            ),
        "second target recovery changed terminal handoff authority"
    );
    drop(twice_settled_store);

    // The source must import the complete target terminal chain plus receipt,
    // prove it descends from the locally retained writer-cut head, and settle
    // Failed without reserving a replacement attempt on any later restart.
    let source_store = open_daemon_state_with_trusted_nodes(
        &handoff.source.state_path,
        &[&handoff.source_fixture.node, &handoff.target_fixture.node],
    )?;
    let source_job_id = format!("worker-handoff-source:{}", operation.operation_id);
    anyhow::ensure!(
        source_store
            .with_state_db(|db| db.get_sync_job(&source_job_id))?
            .is_some(),
        "durable stop cut retained no source handoff job"
    );
    let source_attempts_before =
        source_store.with_state_db(|db| db.list_sync_job_attempts(&source_job_id))?;
    let source_writer_cut_head = source_store
        .with_state_db(|db| db.read_generic_head_ref("chains", &handoff.checkpoint.chain_root_id))?
        .context("durable stop cut retained no source writer-cut head")?;
    drop(source_store);

    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    handoff.refresh_routes()?;
    let source_settlement_timer = MonotonicTimer::start();
    loop {
        let settled = live_retry_job_evidence(
            &handoff.source,
            &source_job_id,
            WorkerHandoffJobRole::Source,
        )
        .await?
        .is_some_and(|job| {
            job.state == "failed" && job.phase == "target_terminal_before_attachment"
        });
        if settled {
            break;
        }
        anyhow::ensure!(
            source_settlement_timer.elapsed() < Duration::from_secs(20),
            "source recovery did not fold the target terminal failure:\n{}",
            stderr_tail(&handoff.source.drain_stderr_nonblocking().await, 200)
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (status_code, status) = handoff
        .source
        .post_execute(
            "service:worker-executions/status",
            ".",
            serde_json::json!({"chain_root_id":handoff.checkpoint.chain_root_id}),
        )
        .await?;
    anyhow::ensure!(
        status_code == reqwest::StatusCode::OK
            && status
                .pointer("/result/handoff/terminal_result/target_chain_head_hash")
                .and_then(serde_json::Value::as_str)
                == Some(failure_receipt.failure.target_chain_head_hash.as_str()),
        "source status did not validate and expose the signed terminal failure: {status_code} {status}"
    );
    handoff.source.kill_daemon().await?;
    let source_settled_store = open_daemon_state_with_trusted_nodes(
        &handoff.source.state_path,
        &[&handoff.source_fixture.node, &handoff.target_fixture.node],
    )?;
    let source_job_after = source_settled_store
        .with_state_db(|db| db.get_sync_job(&source_job_id))?
        .context("source terminal failure settlement disappeared")?;
    let source_attempts_after =
        source_settled_store.with_state_db(|db| db.list_sync_job_attempts(&source_job_id))?;
    let retained_failure: ryeos_app::worker_handoff::WorkerPlacementFailureResponse =
        serde_json::from_value(
            source_job_after
                .result
                .clone()
                .context("source terminal failure has no typed result")?,
        )?;
    anyhow::ensure!(
        source_job_after.state == SyncJobState::Failed
            && source_job_after.heads == [failure_receipt.failure.target_chain_head_hash.clone()]
            && source_job_after
                .roots
                .iter()
                .any(|root| root == &failure_receipt_hash)
            && source_job_after
                .roots
                .iter()
                .any(|root| root == &failure_receipt.failure.target_chain_head_hash)
            && retained_failure == failure_receipt.failure
            && source_attempts_after.len() == source_attempts_before.len() + 1
            && source_attempts_after
                .last()
                .is_some_and(|attempt| attempt.state == SyncJobAttemptState::Completed),
        "source did not settle exactly once from the target-signed failure closure"
    );
    let authority = source_settled_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    ryeos_state::sync::verify_chain_closure_anchored_pinned(
        &authority.cas_store()?,
        &handoff.checkpoint.chain_root_id,
        &failure_receipt.failure.target_chain_head_hash,
        &source_writer_cut_head.target_hash,
    )?;
    drop(guard);
    drop(authority);
    drop(source_settled_store);

    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    handoff.source.kill_daemon().await?;
    let source_twice = open_daemon_state_with_trusted_nodes(
        &handoff.source.state_path,
        &[&handoff.source_fixture.node, &handoff.target_fixture.node],
    )?;
    let source_job_twice = source_twice
        .with_state_db(|db| db.get_sync_job(&source_job_id))?
        .context("second source restart lost terminal failure")?;
    let source_attempts_twice =
        source_twice.with_state_db(|db| db.list_sync_job_attempts(&source_job_id))?;
    anyhow::ensure!(
        source_job_twice == source_job_after && source_attempts_twice == source_attempts_after,
        "second source restart retried or changed a signed terminal failure"
    );
    drop(source_twice);
    handoff.target.kill_daemon().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn permanent_adoption_receipt_folds_after_many_failed_contact_attempts() -> Result<()> {
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let (mut handoff, preflight_id, successor_id, _, _) =
        crash_real_target_adoption_at(HandoffCrashBoundary::TargetBeforeCompletion).await?;
    let target_store = open_daemon_state(&handoff.target.state_path)?;
    let jobs = target_store.with_state_db(|db| {
        db.list_active_sync_jobs_by_operation_type(WORKER_SESSION_HANDOFF_OPERATION, 16)
    })?;
    let target_job = jobs
        .into_iter()
        .find(|job| {
            WorkerSessionHandoffJobOperation::from_value(job.operation.clone()).is_ok_and(
                |operation| {
                    operation.role == WorkerHandoffJobRole::Target
                        && operation.successor_placement_thread_id == successor_id
                },
            )
        })
        .context("receipt cut retained no target handoff job")?;
    let operation = WorkerSessionHandoffJobOperation::from_value(target_job.operation)?;
    anyhow::ensure!(
        target_store
            .worker_handoff_adoption_receipt(&operation.operation_id)?
            .is_some(),
        "completion cut occurred before permanent receipt publication"
    );
    target_store.with_state_db(|db| db.reconcile_interrupted_sync_job_attempts())?;
    record_failed_handoff_attempts(&target_store, &target_job.job_id, 32)?;
    drop(target_store);

    handoff
        .target
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-target");
        })
        .await?;
    handoff.refresh_routes()?;
    let (status, response) = handoff.handoff(&preflight_id).await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK
            && response
                .pointer("/result/placement_thread_id")
                .and_then(serde_json::Value::as_str)
                == Some(successor_id.as_str()),
        "receipt could not fold after repeated contact failures: {status} {response}"
    );
    assert_real_handoff_completed(&mut handoff, &successor_id).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn post_cut_handoff_without_a_receipt_redrives_after_many_peer_failures() -> Result<()> {
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let (mut handoff, preflight_id, successor_id, _, cut) =
        crash_real_source_handoff_at(HandoffCrashBoundary::SourceCommitProjected).await?;
    let source_store = open_daemon_state(&handoff.source.state_path)?;
    source_store.with_state_db(|db| db.reconcile_interrupted_sync_job_attempts())?;
    let source_job = find_handoff_job(
        &source_store,
        &handoff.checkpoint.chain_root_id,
        WorkerHandoffJobRole::Source,
    )?
    .context("post-cut retry qualification retained no source handoff job")?;
    let operation = WorkerSessionHandoffJobOperation::from_value(source_job.operation.clone())?;
    anyhow::ensure!(
        operation.operation_id == cut.operation_id
            && source_job.state != SyncJobState::Completed
            && source_job.state != SyncJobState::Failed
            && source_job.state != SyncJobState::Cancelled,
        "post-cut retry qualification did not retain the exact active operation"
    );
    let authority = source_store.pinned_state_authority()?;
    let guard = authority.acquire_shared_guard()?;
    let cas = authority.cas_store()?;
    let terminal_receipts = source_job
        .roots
        .iter()
        .map(|root| cas.get_object(root))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .filter(|value| {
            matches!(
                value.get("policy").and_then(serde_json::Value::as_str),
                Some(ryeos_app::worker_handoff::WORKER_HANDOFF_ADOPTION_RECEIPT_POLICY)
                    | Some(ryeos_app::worker_handoff::WORKER_HANDOFF_ABORT_FENCE_POLICY)
                    | Some(ryeos_app::worker_handoff::WORKER_HANDOFF_TERMINAL_FAILURE_POLICY)
            )
        })
        .count();
    drop(guard);
    anyhow::ensure!(
        terminal_receipts == 0,
        "post-cut retry qualification unexpectedly began with terminal testimony"
    );
    let repeated_attempt_count =
        record_failed_handoff_attempts(&source_store, &source_job.job_id, 96)?;
    let retained_attempts =
        source_store.with_state_db(|db| db.list_sync_job_attempts(&source_job.job_id))?;
    anyhow::ensure!(
        u64::try_from(retained_attempts.len())? == SYNC_JOB_UNBOUNDED_RETAINED_TERMINAL_ATTEMPTS,
        "post-cut retry qualification did not compact terminal diagnostics"
    );
    drop(source_store);

    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    handoff.refresh_routes()?;
    let (status, response) = handoff.handoff(&preflight_id).await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK
            && response
                .pointer("/result/placement_thread_id")
                .and_then(serde_json::Value::as_str)
                == Some(successor_id.as_str()),
        "post-cut handoff did not redrive after repeated peer failures: {status} {response}"
    );
    handoff.source.kill_daemon().await?;
    let source_store = open_daemon_state_with_trusted_nodes(
        &handoff.source.state_path,
        &[&handoff.source_fixture.node, &handoff.target_fixture.node],
    )?;
    let settled = source_store
        .with_state_db(|db| db.get_sync_job(&source_job.job_id))?
        .context("redriven source handoff disappeared")?;
    anyhow::ensure!(
        settled.state == SyncJobState::Completed
            && settled.attempts_are_unbounded()
            && settled.attempt_count > repeated_attempt_count,
        "redriven source handoff did not retain its unbounded exact-operation ledger"
    );
    handoff.target.kill_daemon().await?;
    Ok(())
}

fn record_failed_handoff_attempts(
    store: &StateStore,
    job_id: &str,
    additional_attempts: u64,
) -> Result<u64> {
    let initial = store
        .with_state_db(|db| db.get_sync_job(job_id))?
        .context("handoff job disappeared before repeated contact failures")?;
    anyhow::ensure!(
        initial.max_attempts == WORKER_SESSION_HANDOFF_MAX_ATTEMPTS
            && initial.attempts_are_unbounded(),
        "handoff job is not using the admitted unbounded authority-recovery lane"
    );
    let expected = initial.attempt_count.saturating_add(additional_attempts);
    while store
        .with_state_db(|db| db.get_sync_job(job_id))?
        .context("handoff job disappeared during repeated contact failures")?
        .attempt_count
        < expected
    {
        let job = store
            .with_state_db(|db| db.get_sync_job(job_id))?
            .context("handoff job disappeared during repeated contact failures")?;
        let attempt_id = format!(
            "qualification-repeated-failure:{}",
            job.attempt_count.saturating_add(1)
        );
        store.with_state_db(|db| {
            db.create_sync_job_attempt(&NewSyncJobAttempt {
                attempt_id: attempt_id.clone(),
                job_id: job.job_id.clone(),
                worker_id: Some("qualification".to_owned()),
                phase: "qualification_repeated_failure".to_owned(),
            })?;
            db.finish_sync_job_attempt(
                &attempt_id,
                &FinishSyncJobAttempt {
                    state: SyncJobAttemptState::Failed,
                    phase: "qualification_repeated_failure".to_owned(),
                    error: Some("synthetic peer contact failure".to_owned()),
                    result: None,
                },
            )
        })?;
    }
    Ok(expected)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn source_adoption_receipt_folds_offline_after_many_failed_contact_attempts() -> Result<()> {
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let (mut handoff, preflight_id, successor_id, _, cut) =
        crash_real_source_handoff_at(HandoffCrashBoundary::SourceBeforeCompletion).await?;
    let source_store = open_daemon_state(&handoff.source.state_path)?;
    source_store.with_state_db(|db| db.reconcile_interrupted_sync_job_attempts())?;
    let source_job = find_handoff_job(
        &source_store,
        &handoff.checkpoint.chain_root_id,
        WorkerHandoffJobRole::Source,
    )?
    .context("source receipt cut retained no source handoff job")?;
    let source_authority = source_store.pinned_state_authority()?;
    let source_guard = source_authority.acquire_shared_guard()?;
    let source_cas = source_authority.cas_store()?;
    let retained_adoption_receipts = source_job
        .roots
        .iter()
        .map(|root| Ok((root, source_cas.get_object(root)?)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(root, value)| value.map(|value| (root, value)))
        .filter(|(_, value)| {
            value.get("policy").and_then(serde_json::Value::as_str)
                == Some(ryeos_app::worker_handoff::WORKER_HANDOFF_ADOPTION_RECEIPT_POLICY)
        })
        .count();
    drop(source_guard);
    anyhow::ensure!(
        source_job.state == SyncJobState::Retryable
            && WorkerSessionHandoffJobOperation::from_value(source_job.operation.clone())?
                .operation_id
                == cut.operation_id
            && retained_adoption_receipts == 1,
        "source adoption cut did not retain the target-signed terminal receipt"
    );
    let attempt_count = record_failed_handoff_attempts(&source_store, &source_job.job_id, 32)?;
    drop(source_store);

    // Local terminal testimony must be sufficient even when the target is
    // unavailable, without consuming another contact attempt.
    handoff.target.kill_daemon().await?;
    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    let (status, response) = handoff.handoff(&preflight_id).await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK
            && response
                .pointer("/result/placement_thread_id")
                .and_then(serde_json::Value::as_str)
                == Some(successor_id.as_str()),
        "source adoption receipt did not fold offline after repeated failures: {status} {response}"
    );
    handoff.source.kill_daemon().await?;
    let source_store = open_daemon_state_with_trusted_nodes(
        &handoff.source.state_path,
        &[&handoff.source_fixture.node, &handoff.target_fixture.node],
    )?;
    anyhow::ensure!(
        source_store
            .with_state_db(|db| db.get_sync_job(&source_job.job_id))?
            .is_some_and(|job| {
                job.state == SyncJobState::Completed && job.attempt_count == attempt_count
            }),
        "source adoption receipt fold consumed an attempt or did not complete"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn source_abort_receipt_folds_offline_after_many_failed_contact_attempts() -> Result<()> {
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let (mut handoff, preflight_id, _successor_id, _, initial_cut) =
        crash_real_source_handoff_at(HandoffCrashBoundary::SourceBeforeWriterCut).await?;
    let recovery_cut = handoff
        .source
        .respawn_until_handoff_crash_boundary(HandoffCrashBoundary::SourceBeforeCompletion)
        .await?;
    anyhow::ensure!(
        recovery_cut.operation_id == initial_cut.operation_id,
        "source abort receipt cut changed the handoff operation"
    );
    handoff.source.kill_daemon().await?;

    let source_store = open_daemon_state(&handoff.source.state_path)?;
    source_store.with_state_db(|db| db.reconcile_interrupted_sync_job_attempts())?;
    let source_job = find_handoff_job(
        &source_store,
        &handoff.checkpoint.chain_root_id,
        WorkerHandoffJobRole::Source,
    )?
    .context("source abort receipt cut retained no source handoff job")?;
    let abort_progress = WorkerSessionHandoffProgress::from_value(
        source_job
            .result
            .clone()
            .context("source abort receipt cut retained no progress")?,
    )?;
    anyhow::ensure!(
        source_job.state == SyncJobState::Retryable
            && abort_progress.phase == WorkerHandoffPhase::AbortAuthorized,
        "source abort cut did not retain an active abort-authorized job"
    );
    let attempt_count = record_failed_handoff_attempts(&source_store, &source_job.job_id, 32)?;
    drop(source_store);

    handoff.target.kill_daemon().await?;
    handoff
        .source
        .respawn_with(|command| {
            command.env("HOSTNAME", "handoff-source");
        })
        .await?;
    let (status, _) = handoff.handoff(&preflight_id).await?;
    anyhow::ensure!(
        !status.is_success(),
        "source abort receipt fold admitted the discarded handoff"
    );
    handoff.source.kill_daemon().await?;
    let source_store = open_daemon_state_with_trusted_nodes(
        &handoff.source.state_path,
        &[&handoff.source_fixture.node, &handoff.target_fixture.node],
    )?;
    anyhow::ensure!(
        source_store
            .with_state_db(|db| db.get_sync_job(&source_job.job_id))?
            .is_some_and(|job| {
                job.state == SyncJobState::Cancelled && job.attempt_count == attempt_count
            }),
        "source abort receipt fold consumed an attempt or did not cancel"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn portable_worker_fixture_launches_and_binds_a_real_hosted_session() -> Result<()> {
    let project = tempfile::tempdir()?;
    std::fs::create_dir_all(project.path().join(".ai"))?;
    std::fs::write(
        project.path().join("fixture.txt"),
        b"portable handoff fixture\n",
    )?;
    let (mut daemon, fixture) = DaemonHarness::start_fast_with(
        |state_path, _user_space, fixture| plant_portable_worker(state_path, fixture),
        |_| {},
    )
    .await?;
    let project_path = project.path().display().to_string();
    let launch_body = serde_json::json!({
        "launch_id":"L-11111111111111111111111111111111",
        "item_ref":PORTABLE_EXECUTION_REF,
        "ref_bindings":{},
        "project_path":project_path,
        "parameters":{"credential_profile_id":PORTABLE_CREDENTIAL_PROFILE_ID},
        "execution_policy":ryeos_app::execution_policy::ExecutionPolicy::local_pinned_capture(
            ryeos_app::execution_policy::ExecutionResponse::Accepted,
        ),
    });
    let (status, launch) = daemon.post_json("/execute/launch", launch_body).await?;
    if !matches!(
        status,
        reqwest::StatusCode::OK | reqwest::StatusCode::ACCEPTED
    ) {
        let stderr = daemon.drain_stderr_nonblocking().await;
        anyhow::bail!("portable worker launch returned {status}: {launch}\n{stderr}");
    }
    let chain_root_id = launch
        .get("thread_id")
        .or_else(|| launch.get("chain_root_id"))
        .or_else(|| launch.pointer("/result/thread_id"))
        .or_else(|| launch.pointer("/result/chain_root_id"))
        .or_else(|| launch.pointer("/thread/chain_root_id"))
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("portable worker launch returned no chain root: {launch}"))?;
    let mut command_result = None;
    let mut last_command = None;
    for _ in 0..120 {
        let response = daemon
            .post_execute(
                "service:worker-executions/command",
                ".",
                serde_json::json!({
                    "chain_root_id":chain_root_id,
                    "idempotency_key":"portable-fixture-session-start",
                    "route_id":"session.start",
                    "payload":{},
                }),
            )
            .await?;
        if response.0 == reqwest::StatusCode::OK {
            command_result = Some(response.1);
            break;
        }
        last_command = Some(response);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _command = command_result.with_context(|| {
        format!("portable worker never accepted its first command: {last_command:?}")
    })?;
    let (status, terminated) = daemon
        .post_execute(
            "service:worker-executions/terminate",
            ".",
            serde_json::json!({
                "chain_root_id":chain_root_id,
                "reason":"completed",
            }),
        )
        .await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "portable worker termination returned {status}: {terminated}"
    );
    anyhow::ensure!(
        matches!(
            terminated
                .pointer("/result/state")
                .and_then(serde_json::Value::as_str),
            Some("freezing" | "frozen")
        ),
        "portable worker termination did not reserve its candidate: {terminated}"
    );
    let mut frozen_status = None;
    let mut last_status = None;
    for _ in 0..120 {
        let response = daemon
            .post_execute(
                "service:worker-executions/status",
                ".",
                serde_json::json!({"chain_root_id":chain_root_id}),
            )
            .await?;
        if response.0 == reqwest::StatusCode::OK
            && response
                .1
                .pointer("/result/state")
                .and_then(serde_json::Value::as_str)
                == Some("frozen")
        {
            frozen_status = Some(response.1);
            break;
        }
        last_status = Some(response);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _frozen_status = frozen_status
        .with_context(|| format!("portable worker candidate never froze: {last_status:?}"))?;
    let (status, checkpoint) = daemon
        .post_execute(
            "service:worker-executions/checkpoint",
            ".",
            serde_json::json!({"chain_root_id":chain_root_id}),
        )
        .await?;
    anyhow::ensure!(
        status == reqwest::StatusCode::OK,
        "portable worker checkpoint returned {status}: {checkpoint}; termination={terminated}"
    );
    anyhow::ensure!(
        checkpoint
            .pointer("/result/manifest_ref")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| value.starts_with("cas:")),
        "portable worker checkpoint returned no manifest authority: {checkpoint}"
    );
    daemon.kill_daemon().await?;
    let store = open_daemon_state(&daemon.state_path)?;
    let placement = store
        .current_chain_placement_thread_id(chain_root_id)?
        .context("portable worker chain has no placement")?;
    let session = store
        .dedicated_session(&placement)?
        .context("portable worker has no dedicated session")?;
    anyhow::ensure!(
        session.remote_thread_id.as_deref() == Some("handoff-fixture-session"),
        "portable worker did not bind its exact upstream session"
    );
    anyhow::ensure!(
        session.state == "frozen",
        "portable worker checkpoint did not retain the frozen source"
    );
    anyhow::ensure!(
        daemon_identity(&daemon.state_path)?.fingerprint() == fixture.node_fp(),
        "portable worker daemon changed node identity"
    );
    Ok(())
}
