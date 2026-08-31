//! Process-crash qualification for the durable worker-handoff recovery path.
//!
//! Source-recovery fixtures write the exact durable job and signed chain while
//! the daemon is stopped. Target-request fixtures use a separately signed
//! source authority and the real authenticated remote-node request boundary.
//! The parent observes each named boundary over the inherited test-only pipe
//! and SIGKILLs the process, so no Rust unwinding or request cleanup can
//! manufacture the outcome.

#![cfg(all(unix, feature = "handoff-test-support"))]

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Json;
use axum::Router;
use axum::routing::post;
use base64::Engine as _;
use lillux::crypto::SigningKey;
use ryeos_app::identity::NodeIdentity;
use ryeos_app::runtime_db::{NewCredentialProfile, NewCredentialProfileReservation};
use ryeos_app::state_store::{NewEventRecord, NewThreadRecord, NodeIdentitySigner, StateStore};
use ryeos_app::worker_handoff::{
    WORKER_PLACEMENT_ABORT_SERVICE, WORKER_SESSION_HANDOFF_OPERATION, WorkerHandoffJobRole,
    WorkerHandoffPhase, WorkerPlacementAbortRequest, WorkerPlacementAbortResponse,
    WorkerSessionHandoffJobOperation, WorkerSessionHandoffProgress,
};
use ryeos_app::write_barrier::WriteBarrier;
use ryeos_state::{NewSyncJob, SyncJobState, SyncJobUpdate};
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

    let policy_body = serde_yaml::to_string(
        &ryeos_app::node_config::sections::persistent_sessions::PersistentSessionPolicyRecord {
            schema: 1,
            limits: ryeos_app::persistent_session::PersistentSessionPoolLimits::default(),
        },
    )?;
    let policy_path = state_path.join(".ai/node/persistent_sessions/policy.yaml");
    std::fs::create_dir_all(policy_path.parent().context("policy path has no parent")?)?;
    std::fs::write(
        policy_path,
        lillux::signature::sign_content_at(
            &policy_body,
            &fixture.node,
            "#",
            None,
            common::fast_fixture::FAST_FIXTURE_TIME,
        ),
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
            node_policy: ryeos_state::objects::CapturedNodeHistoryPolicyProvenance::MissingConfig,
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
    let runtime_state_dir = state_path.join(ryeos_engine::AI_DIR).join("state");
    let runtime_db_path = runtime_state_dir.join("runtime.sqlite3");
    let identity = daemon_identity(state_path)?;
    let signer = Arc::new(NodeIdentitySigner::from_identity(&identity));
    let mut head_trust = ryeos_state::refs::TrustStore::new();
    head_trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    StateStore::new_with_head_trust(
        state_path.to_path_buf(),
        runtime_state_dir,
        runtime_db_path,
        signer,
        WriteBarrier::new(),
        Arc::new(head_trust),
    )
}

fn seed_source_exported_handoff(
    state_path: &Path,
    source_site_id: &str,
    target_site_id: &str,
) -> Result<SeededSourceHandoff> {
    let store = open_daemon_state(state_path)?;
    let source_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    let successor_thread_id = ryeos_app::thread_lifecycle::new_thread_id();
    let item_ref = "system:handoff-crash-recovery-fixture";
    store.create_thread_for_test(&NewThreadRecord {
        thread_id: source_thread_id.clone(),
        chain_root_id: source_thread_id.clone(),
        // This fixture root deliberately has no executable process. The real
        // hosted-worker process lifecycle is qualified separately; using the
        // daemon-owned bookkeeping profile prevents generic process recovery
        // from inventing an unrelated launch while preserving the same signed
        // thread and handoff recovery contracts.
        kind: "system_task".to_owned(),
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
                max_attempts: 16,
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
) -> Result<(String, tokio::task::JoinHandle<()>)> {
    let response = Arc::new(closure_response(payload)?);
    let app = Router::new().route(
        "/objects/closure/get",
        post(move || {
            let response = Arc::clone(&response);
            async move { Json((*response).clone()) }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}"), task))
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
            max_attempts: 16,
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
            operation_id: source.operation.operation_id.clone(),
            chain_root_id: source.operation.chain_root_id.clone(),
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
        operation_id: operation.operation_id.clone(),
        chain_root_id: operation.chain_root_id.clone(),
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

async fn qualify_target_abort_boundary(
    source: &SourceAbortAuthority,
    source_url: &str,
    boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    expected_cut: TargetAbortCutExpectation,
) -> Result<()> {
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
    target.kill_daemon().await?;
    assert_target_abort_state(
        &target.state_path,
        source,
        TargetAbortCutExpectation {
            phase: WorkerHandoffPhase::AbortAuthorized,
            state: SyncJobState::Cancelled,
            abort_root_retained: true,
            reservation_state: "released",
        },
    )?;
    Ok(())
}

async fn launch_and_checkpoint_portable_worker(
    daemon: &DaemonHarness,
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
    anyhow::ensure!(
        frozen,
        "portable worker candidate never froze: {last_status:?}"
    );
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
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let (mut daemon, _fixture) = DaemonHarness::start_fast().await?;
    daemon.kill_daemon().await?;
    let seeded = seed_source_exported_handoff(
        &daemon.state_path,
        DAEMON_SOURCE_SITE_ID,
        REMOTE_TARGET_SITE_ID,
    )?;

    daemon
        .respawn_until_handoff_crash_boundary(HandoffCrashBoundary::SourceBeforeAbortPublication)
        .await?;
    daemon.kill_daemon().await?;
    assert_source_recovery_state(
        &daemon.state_path,
        &seeded,
        WorkerHandoffPhase::SourceExported,
        &seeded.source_head_hash,
        0,
    )?;

    daemon
        .respawn_until_handoff_crash_boundary(HandoffCrashBoundary::SourceAbortPublished)
        .await?;
    daemon.kill_daemon().await?;
    let store = open_daemon_state(&daemon.state_path)?;
    let abort_head_hash = store
        .with_state_db(|db| db.read_generic_head_ref("chains", &seeded.operation.chain_root_id))?
        .context("source abort publication produced no chain head")?
        .target_hash;
    drop(store);
    anyhow::ensure!(
        abort_head_hash != seeded.source_head_hash,
        "source abort publication did not advance the chain"
    );
    assert_source_recovery_state(
        &daemon.state_path,
        &seeded,
        WorkerHandoffPhase::SourceExported,
        &abort_head_hash,
        1,
    )?;

    daemon
        .respawn_until_handoff_crash_boundary(HandoffCrashBoundary::SourceAbortProjected)
        .await?;
    daemon.kill_daemon().await?;
    assert_source_recovery_state(
        &daemon.state_path,
        &seeded,
        WorkerHandoffPhase::AbortAuthorized,
        &abort_head_hash,
        1,
    )?;

    // One ungated boot runs the ordinary recovery path through its missing
    // remote configuration failure. That expected external failure must not
    // duplicate the signed abort or roll the job back to source_exported.
    daemon.respawn_with(|_| {}).await?;
    daemon.kill_daemon().await?;
    assert_source_recovery_state(
        &daemon.state_path,
        &seeded,
        WorkerHandoffPhase::AbortAuthorized,
        &abort_head_hash,
        1,
    )?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn target_abort_request_and_recovery_survive_sigkill_at_each_durable_seam() -> Result<()> {
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let source = create_source_abort_authority()?;
    let (source_url, source_server) = start_source_closure_server(&source.abort_payload).await?;
    let cases = [
        (
            HandoffCrashBoundary::TargetBeforeAbortEvidenceStage,
            TargetAbortCutExpectation {
                phase: WorkerHandoffPhase::TargetPrepared,
                state: SyncJobState::Running,
                abort_root_retained: false,
                reservation_state: "reserved",
            },
        ),
        (
            HandoffCrashBoundary::TargetAbortEvidenceStaged,
            TargetAbortCutExpectation {
                phase: WorkerHandoffPhase::AbortAuthorized,
                state: SyncJobState::Running,
                abort_root_retained: true,
                reservation_state: "reserved",
            },
        ),
        (
            HandoffCrashBoundary::TargetAbortEvidenceVerified,
            TargetAbortCutExpectation {
                phase: WorkerHandoffPhase::AbortAuthorized,
                state: SyncJobState::Running,
                abort_root_retained: true,
                reservation_state: "reserved",
            },
        ),
        (
            HandoffCrashBoundary::TargetAbortReservationReleased,
            TargetAbortCutExpectation {
                phase: WorkerHandoffPhase::AbortAuthorized,
                state: SyncJobState::Running,
                abort_root_retained: true,
                reservation_state: "released",
            },
        ),
        (
            HandoffCrashBoundary::TargetAbortCompletedBeforeResponse,
            TargetAbortCutExpectation {
                phase: WorkerHandoffPhase::AbortAuthorized,
                state: SyncJobState::Cancelled,
                abort_root_retained: true,
                reservation_state: "released",
            },
        ),
    ];

    for (boundary, expected) in cases {
        qualify_target_abort_boundary(&source, &source_url, boundary, expected)
            .await
            .with_context(|| format!("qualify target abort boundary `{boundary}`"))?;
    }
    source_server.abort();
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
        &source,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn real_portable_worker_completes_cross_site_handoff() -> Result<()> {
    let mut handoff = start_real_portable_handoff().await?;
    let (preflight_id, successor_id) = handoff.preflight().await?;
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
    let report_path = handoff
        ._project
        .path()
        .join(".ai/worker-handoff-qualification.json");
    std::fs::write(
        &report_path,
        lillux::canonical_json(measurements)?.as_bytes(),
    )?;
    let retained: serde_json::Value = serde_json::from_slice(&std::fs::read(&report_path)?)?;
    anyhow::ensure!(
        retained == *measurements,
        "retained measurement report drifted"
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
    assert_real_handoff_completed(&mut handoff, &successor_id).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn real_source_writer_cut_sigkill_recovers_without_replacing_authority() -> Result<()> {
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let mut handoff = start_real_portable_handoff().await?;
    let (preflight_id, successor_id) = handoff.preflight().await?;
    handoff.source.kill_daemon().await?;
    let mut gate = handoff
        .source
        .respawn_with_handoff_crash_gate(HandoffCrashBoundary::SourceBeforeWriterCut, |command| {
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
    gate.wait_reached().await?;
    handoff.source.kill_daemon().await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), request_task).await;
    let source_store = open_daemon_state(&handoff.source.state_path)?;
    anyhow::ensure!(
        source_store.current_chain_placement_thread_id(&handoff.checkpoint.chain_root_id)?
            == Some(handoff.checkpoint.chain_root_id.clone()),
        "pre-cut SIGKILL unexpectedly changed writer placement"
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
        status == reqwest::StatusCode::OK,
        "handoff retry returned {status}: {response}"
    );
    assert_real_handoff_completed(&mut handoff, &successor_id).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn real_target_adoption_sigkill_recovers_exact_source_commit() -> Result<()> {
    use ryeos_app::worker_handoff::test_support::HandoffCrashBoundary;

    let mut handoff = start_real_portable_handoff().await?;
    let (preflight_id, successor_id) = handoff.preflight().await?;
    handoff.target.kill_daemon().await?;
    let mut gate = handoff
        .target
        .respawn_with_handoff_crash_gate(
            HandoffCrashBoundary::TargetBeforeAdoptionPublication,
            |command| {
                command.env("HOSTNAME", "handoff-target");
            },
        )
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
    gate.wait_reached().await?;
    handoff.target.kill_daemon().await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), request_task).await;
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
        "handoff retry after target adoption crash returned {status}: {response}"
    );
    assert_real_handoff_completed(&mut handoff, &successor_id).await
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
