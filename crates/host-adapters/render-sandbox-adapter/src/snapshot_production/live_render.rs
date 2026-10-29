//! Live Render integration chain, driven through the adapter's own HTTP
//! client, strict parsers and validation rules with no daemon in front.
//!
//! The chain creates a source sandbox, uploads a guest-package-sized tar,
//! runs a command over the run stream, snapshots the filesystem, restores a
//! sandbox from the snapshot and terminates both sandboxes. It creates
//! billable Render resources, so it is `#[ignore]` and skips when no API key
//! is present. Every sandbox it creates is terminated again on every exit
//! path, and each id is printed on stderr as soon as it exists.
//!
//! Inputs, all read from the environment by the test only:
//! `RYEOS_LIVE_RENDER_API_KEY` (absent means skip),
//! `RYEOS_LIVE_RENDER_OWNER_ID`, `RYEOS_LIVE_RENDER_SANDBOX_GROUP_ID`,
//! `RYEOS_LIVE_RENDER_ROOTS_PEM` (path of the Render API root certificate
//! PEM) and optional `RYEOS_LIVE_RENDER_REGION` (default `oregon`).

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use base64::Engine as _;
use lillux::network::{NetworkCancellation, NetworkContext};
use lillux::time::{Duration, MonotonicDeadline};
use ryeos_external_execution_contract::runtime_snapshot::{
    RUNTIME_SNAPSHOT_INTENT_SCHEMA, RuntimeSnapshotIntent, RuntimeSnapshotSource,
};
use ryeos_external_execution_contract::{canonical_json, from_json_slice_strict};
use ryeos_http_transport::{HttpResponse, RequestBodySource};
use zeroize::Zeroizing;

use super::{
    BoundSnapshotCreation, MAX_SNAPSHOT_RESPONSE_BYTES, RenderSnapshotCreateResponse,
    SnapshotProductionSettings, SnapshotStatus, bind_snapshot_locator_for, creation_intent_for,
    observe_snapshot_available, read_control_json, same_instant, send_control,
    send_snapshot_create, send_snapshot_upload, snapshot_readiness_url,
};
use crate::activation_contact::RenderContact;
use crate::provider_spec::{EgressPolicy, PlanValue, ProviderSpec, RouteName};
use crate::proxy_route::ProxyOperation;
use crate::snapshot_bootstrap::response_matches;
use crate::snapshot_provider_spec::SnapshotProductionSpec;
use crate::snapshot_qualification::parse_verifier_stdout;
use crate::{
    ADAPTER_ID, CreateSandboxBody, MAX_API_RESPONSE_BYTES, NetworkPolicy, RenderPlan,
    RenderSandbox, RenderSandboxStatus, Settings, accepted_create_response, api_url,
    create_response_matches, exact_terminal_timestamp, read_response, send_api_request,
    validate_api_url, validate_settings,
};

/// Stands in for the snapshot id on settings used before any snapshot exists.
/// Only the owner and the TLS roots of such settings are ever consulted.
const UNBOUND_SNAPSHOT_ID: &str = "snp-unbound";
const SOURCE_LIFETIME_SECONDS: u32 = 600;
const RESTORE_LIFETIME_SECONDS: u32 = 900;
/// The real guest package is about this large; the adapter's two second
/// per-socket idle timeout is exercised against a body of this size.
const OWNER_EXECUTABLE_BYTES: usize = 12 * 1024 * 1024;
const RUN_COMMAND: &str = "ls -l /ryeos/guest-runtime";
const RUN_MARKER: &str = "controller-root.hex";
const RUN_WIRE_BYTES: usize = 64 * 1024;
const RUN_EVENT_BYTES: usize = 16 * 1024;
const RUN_EVENTS: u64 = 32;
const RUN_OUTPUT_BYTES: usize = 16 * 1024;
/// Absolute envelope handed to each adapter call, in place of the daemon's
/// per-operation deadline. The adapter's own setup and idle timeouts apply
/// inside it unchanged.
const CALL_DEADLINE: Duration = Duration::from_secs(180);
const RUNNING_WAIT: Duration = Duration::from_secs(300);
const RUNNING_POLL: Duration = Duration::from_secs(5);
const SNAPSHOT_WAIT: Duration = Duration::from_secs(600);
const SNAPSHOT_POLL: Duration = Duration::from_secs(15);
const TERMINATE_POLLS: u32 = 10;
const TERMINATE_POLL: Duration = Duration::from_secs(3);

fn note(message: &str) {
    eprintln!("live_render: {message}");
}

/// Everything the chain needs, built once from the environment.
struct Live {
    network: NetworkContext,
    cancellation: NetworkCancellation,
    credential: Zeroizing<String>,
    spec: ProviderSpec,
    snapshot_spec: SnapshotProductionSpec,
    owner_id: String,
    region: String,
    group_id: String,
    roots: Vec<String>,
}

impl Live {
    fn deadline(&self) -> MonotonicDeadline {
        MonotonicDeadline::after(CALL_DEADLINE)
    }

    fn settings(&self, snapshot_id: &str) -> Settings {
        Settings {
            schema: 2,
            owner_id: self.owner_id.clone(),
            plan: RenderPlan::Starter,
            region: self.region.clone(),
            snapshot_id: snapshot_id.into(),
            tls_roots_der_base64: self.roots.clone(),
        }
    }

    fn snapshot_settings(&self) -> SnapshotProductionSettings {
        SnapshotProductionSettings {
            schema: 1,
            owner_id: self.owner_id.clone(),
            region: self.region.clone(),
            plan: RenderPlan::Starter,
            sandbox_group_id: self.group_id.clone(),
            tls_roots_der_base64: self.roots.clone(),
        }
    }
}

/// Terminates every tracked sandbox when it goes out of scope, including
/// during a panic. The POST is best effort here; the explicit termination
/// step is the one that observes the terminal state.
struct Guard<'a> {
    live: &'a Live,
    ids: RefCell<Vec<String>>,
}

impl<'a> Guard<'a> {
    fn new(live: &'a Live) -> Self {
        Self {
            live,
            ids: RefCell::new(Vec::new()),
        }
    }

    fn track(&self, id: &str) {
        note(&format!(
            "sandbox {id} exists; terminate it by hand if this run is killed"
        ));
        if let Ok(mut ids) = self.ids.try_borrow_mut()
            && !ids.iter().any(|known| known == id)
        {
            ids.push(id.into());
        }
    }

    fn release(&self, id: &str) {
        if let Ok(mut ids) = self.ids.try_borrow_mut() {
            ids.retain(|known| known != id);
        }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let ids = std::mem::take(self.ids.get_mut());
        for id in ids {
            match terminate_post(self.live, &id) {
                Ok(status) => note(&format!("cleanup terminate POST for {id} returned {status}")),
                Err(error) => note(&format!(
                    "cleanup terminate POST for {id} failed ({error:#}); terminate it by hand"
                )),
            }
        }
    }
}

/// Track an id found in a create response before that response is strictly
/// validated, so a response the adapter refuses still gets cleaned up.
fn track_created(guard: &Guard<'_>, body: &[u8]) {
    let id = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("id").and_then(|id| id.as_str().map(str::to_owned)));
    match id {
        Some(id) => guard.track(&id),
        None => note("create response carried no id to track"),
    }
}

fn route_url(
    live: &Live,
    route: RouteName,
    id: Option<&str>,
    settings: &Settings,
) -> Result<url::Url> {
    let (url, target) = api_url(&live.spec, route, id, settings, None)?;
    validate_api_url(
        &url,
        &target.path_segments,
        target.owner_id_query.as_deref(),
        target.file_path_query.as_deref(),
    )?;
    Ok(url)
}

fn api(
    live: &Live,
    settings: &Settings,
    url: &url::Url,
    method: &str,
    body: Option<Vec<u8>>,
) -> Result<HttpResponse> {
    send_api_request(
        &live.network,
        url,
        method,
        body,
        &live.credential,
        live.deadline(),
        settings,
        &live.cancellation,
    )
}

fn terminate_post(live: &Live, id: &str) -> Result<u16> {
    let settings = live.settings(UNBOUND_SNAPSHOT_ID);
    let url = route_url(live, RouteName::TerminateById, Some(id), &settings)?;
    Ok(api(live, &settings, &url, "POST", None)?.status)
}

fn get_sandbox_body(
    live: &Live,
    settings: &Settings,
    url: &url::Url,
) -> Result<(u16, Vec<u8>)> {
    read_response(api(live, settings, url, "GET", None)?)
        .map_err(|()| anyhow!("sandbox GET response was not bounded JSON"))
}

fn parse_sandbox(body: &[u8]) -> Result<RenderSandbox> {
    from_json_slice_strict(
        body,
        usize::try_from(MAX_API_RESPONSE_BYTES).unwrap_or(usize::MAX),
    )
}

fn get_sandbox(live: &Live, settings: &Settings, url: &url::Url, id: &str) -> Result<RenderSandbox> {
    let (status, body) = get_sandbox_body(live, settings, url)?;
    ensure!(status == 200, "sandbox GET returned status {status}");
    let sandbox = parse_sandbox(&body)?;
    ensure!(sandbox.id == id, "sandbox GET returned another sandbox");
    Ok(sandbox)
}

fn wait_running(live: &Live, settings: &Settings, id: &str, label: &str) -> Result<RenderSandbox> {
    let started = Instant::now();
    let url = route_url(live, RouteName::ById, Some(id), settings)?;
    loop {
        let sandbox = get_sandbox(live, settings, &url, id)?;
        match sandbox.status {
            RenderSandboxStatus::Running => {
                note(&format!(
                    "{label} sandbox {id} running after {} s",
                    started.elapsed().as_secs()
                ));
                return Ok(sandbox);
            }
            RenderSandboxStatus::Creating | RenderSandboxStatus::Resuming => {}
            other => bail!("{label} sandbox {id} reached status {other:?}"),
        }
        ensure!(
            started.elapsed() < RUNNING_WAIT,
            "{label} sandbox {id} not running within {} s",
            RUNNING_WAIT.as_secs()
        );
        std::thread::sleep(RUNNING_POLL);
    }
}

fn put_octal(field: &mut [u8], value: u64) -> Result<()> {
    let digits = field.len() - 1;
    let text = format!("{value:0digits$o}");
    ensure!(text.len() == digits, "tar numeric field overflow");
    field[..digits].copy_from_slice(text.as_bytes());
    field[digits] = 0;
    Ok(())
}

fn append_tar_entry(
    tar: &mut Vec<u8>,
    name: &str,
    mode: u32,
    data: &[u8],
    directory: bool,
) -> Result<()> {
    ensure!(name.len() <= 100, "tar entry name is too long");
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    put_octal(&mut header[100..108], u64::from(mode))?;
    put_octal(&mut header[108..116], 0)?;
    put_octal(&mut header[116..124], 0)?;
    put_octal(&mut header[124..136], data.len() as u64)?;
    put_octal(&mut header[136..148], 0)?;
    header[148..156].fill(b' ');
    header[156] = if directory { b'5' } else { b'0' };
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().map(|byte| u32::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    tar.extend_from_slice(&header);
    tar.extend_from_slice(data);
    let padding = (512 - data.len() % 512) % 512;
    tar.resize(tar.len() + padding, 0);
    Ok(())
}

/// The adapter's four-entry guest package shape, with an owner executable of
/// the size the real package has. The filler is pseudo-random so no hop on the
/// path can shrink it.
fn guest_package_tar() -> Result<Vec<u8>> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut owner = Vec::with_capacity(OWNER_EXECUTABLE_BYTES + 8);
    while owner.len() < OWNER_EXECUTABLE_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        owner.extend_from_slice(&state.to_le_bytes());
    }
    owner.truncate(OWNER_EXECUTABLE_BYTES);
    let mut tar = Vec::with_capacity(OWNER_EXECUTABLE_BYTES + 8192);
    append_tar_entry(&mut tar, "bin/", 0o755, &[], true)?;
    append_tar_entry(
        &mut tar,
        "bin/ryeos-external-guest-occurrence-owner",
        0o755,
        &owner,
        false,
    )?;
    append_tar_entry(&mut tar, "controller-root.hex", 0o644, &[b'a'; 64], false)?;
    append_tar_entry(&mut tar, "guest-owner-profile.json", 0o644, b"{}\n", false)?;
    tar.resize(tar.len() + 1024, 0);
    Ok(tar)
}

/// The DER base64 of every certificate in a PEM file, in the sorted unique
/// order the adapter's settings validation requires.
fn roots_from_pem(path: &str) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).context("read Render API roots PEM")?;
    let mut roots = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines().map(str::trim) {
        if line == "-----BEGIN CERTIFICATE-----" {
            current = Some(String::new());
        } else if line == "-----END CERTIFICATE-----" {
            roots.extend(current.take());
        } else if let Some(body) = current.as_mut() {
            body.push_str(line);
        }
    }
    roots.sort();
    roots.dedup();
    ensure!(!roots.is_empty(), "roots PEM holds no certificate");
    Ok(roots)
}

fn required_env(name: &str) -> Result<String> {
    let value = std::env::var(name).with_context(|| format!("{name} is required"))?;
    ensure!(!value.is_empty(), "{name} is empty");
    Ok(value)
}

fn run_command(contact: &RenderContact<'_>, command: &str) -> Result<String> {
    const RESPONSE_BYTES: u64 = RUN_WIRE_BYTES as u64;
    let operation = ProxyOperation::RunStream;
    let token = contact.mint(operation, Some(command))?;
    let body = canonical_json(&serde_json::json!({ "command": command }))?;
    let response = contact.send_proxy(
        token,
        operation,
        RequestBodySource::from_bytes(body),
        "application/json",
        "text/event-stream",
        RESPONSE_BYTES,
    )?;
    ensure!(
        response.status == 200
            && response
                .headers
                .iter()
                .filter(|header| header.name.eq_ignore_ascii_case("content-type"))
                .count()
                == 1
            && response.headers.iter().any(|header| {
                header.name.eq_ignore_ascii_case("content-type")
                    && header.value() == b"text/event-stream"
            }),
        "run did not return an exact event stream (status {})",
        response.status
    );
    let mut stream = Vec::new();
    response
        .body
        .take(RESPONSE_BYTES + 1)
        .read_to_end(&mut stream)?;
    ensure!(
        stream.len() as u64 <= RESPONSE_BYTES,
        "run stream exceeds its bound"
    );
    let stdout = parse_verifier_stdout(
        &stream,
        RUN_WIRE_BYTES,
        RUN_EVENT_BYTES,
        RUN_EVENTS,
        RUN_OUTPUT_BYTES,
        None,
    )?;
    Ok(String::from_utf8(stdout)?)
}

fn wait_snapshot_available(
    live: &Live,
    snapshot_settings: &SnapshotProductionSettings,
    creation_intent: &super::SnapshotCreationIntent,
    creation: &BoundSnapshotCreation,
    url: &url::Url,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let (status, body) = read_control_json(send_control(
            &live.network,
            url,
            "GET",
            None,
            &live.credential,
            snapshot_settings,
            live.deadline(),
            &live.cancellation,
        )?)?;
        match observe_snapshot_available(
            &live.snapshot_spec,
            creation_intent,
            creation,
            status,
            &body,
        ) {
            Ok(observed) => {
                let read: RenderSnapshotCreateResponse =
                    from_json_slice_strict(&body, MAX_SNAPSHOT_RESPONSE_BYTES)?;
                ensure!(
                    same_instant(&read.requested_at, &creation.requested_at)
                        && same_instant(&read.expires_at, &creation.expires_at),
                    "readiness timestamps are not the creation instants"
                );
                note(&format!(
                    "snapshot {} available after {} s: {} bytes, requestedAt create={} read={}",
                    creation.snapshot_id,
                    started.elapsed().as_secs(),
                    observed.size_bytes,
                    creation.requested_at,
                    read.requested_at
                ));
                return Ok(());
            }
            Err(refusal) => {
                let read: RenderSnapshotCreateResponse =
                    from_json_slice_strict(&body, MAX_SNAPSHOT_RESPONSE_BYTES)
                        .context("snapshot read does not parse strictly")?;
                if read.status != SnapshotStatus::Creating {
                    return Err(refusal.context("snapshot read refused although not creating"));
                }
            }
        }
        ensure!(
            started.elapsed() < SNAPSHOT_WAIT,
            "snapshot {} not available within {} s",
            creation.snapshot_id,
            SNAPSHOT_WAIT.as_secs()
        );
        std::thread::sleep(SNAPSHOT_POLL);
    }
}

/// POST terminate, then observe the terminal state through the adapter's own
/// strict sandbox parser and terminal check.
fn terminate_and_observe(
    live: &Live,
    guard: &Guard<'_>,
    settings: &Settings,
    id: &str,
    label: &str,
) -> Result<()> {
    let url = route_url(live, RouteName::TerminateById, Some(id), settings)?;
    match api(live, settings, &url, "POST", None) {
        Ok(response) => note(&format!(
            "{label} sandbox {id} terminate POST returned {}",
            response.status
        )),
        Err(error) => note(&format!(
            "{label} sandbox {id} terminate POST failed ({error:#})"
        )),
    }
    let by_id = route_url(live, RouteName::ById, Some(id), settings)?;
    for attempt in 1..=TERMINATE_POLLS {
        let (status, body) = get_sandbox_body(live, settings, &by_id)?;
        ensure!(status == 200, "post-terminate GET returned status {status}");
        let sandbox = match parse_sandbox(&body) {
            Ok(sandbox) => sandbox,
            Err(error) => {
                note(&format!(
                    "post-terminate body refused by the adapter parser ({error:#}): {}",
                    String::from_utf8_lossy(&body)
                ));
                return Err(error.context("post-terminate sandbox shape"));
            }
        };
        if let Some(terminated_at) =
            exact_terminal_timestamp(&sandbox, id, settings, &EgressPolicy::deny_all())
        {
            ensure!(
                sandbox.terminated_at.as_deref() == Some(terminated_at),
                "terminal timestamp is not the sandbox's terminatedAt"
            );
            note(&format!(
                "{label} sandbox {id} terminal at {terminated_at} (poll {attempt})"
            ));
            guard.release(id);
            return Ok(());
        }
        note(&format!(
            "{label} sandbox {id} status {:?}, not yet terminal (poll {attempt})",
            sandbox.status
        ));
        std::thread::sleep(TERMINATE_POLL);
    }
    bail!("{label} sandbox {id} not terminal after {TERMINATE_POLLS} polls")
}

fn run_chain(live: &Live, guard: &Guard<'_>) -> Result<()> {
    // 1. Source sandbox.
    let settings = live.settings(UNBOUND_SNAPSHOT_ID);
    validate_settings(&settings)?;
    let projection = live.spec.bootstrap_create_projection(
        &live.owner_id,
        PlanValue::Starter,
        &live.region,
        SOURCE_LIFETIME_SECONDS,
    )?;
    let create_url = route_url(
        live,
        live.spec
            .bootstrap_create_route()
            .context("provider spec has no bootstrap create route")?,
        None,
        &settings,
    )?;
    let create_body = canonical_json(&CreateSandboxBody {
        owner_id: projection.owner_id.clone(),
        plan: RenderPlan::Starter,
        region: projection.region.clone(),
        timeout_seconds: projection.timeout_seconds,
        network_policy: NetworkPolicy::requested(&projection.network_policy),
        snapshot_id: None,
    })?;
    let (status, body) = read_response(api(live, &settings, &create_url, "POST", Some(create_body))?)
        .map_err(|()| anyhow!("source create response was not bounded JSON"))?;
    track_created(guard, &body);
    ensure!(status == 201, "source create returned status {status}");
    let created = parse_sandbox(&body)?;
    ensure!(
        response_matches(&created, &projection),
        "source create response refused by the adapter's validation"
    );
    let source_id = created.id;
    let source = wait_running(live, &settings, &source_id, "source")?;
    ensure!(
        response_matches(&source, &projection) && source.sandbox_group_id == live.group_id,
        "running source refused by the adapter's validation"
    );

    // 2. Upload a package-sized tar through the adapter's upload path. It
    // fetches and validates the source, mints the upload token, binds the
    // proxy route and streams the sealed file under SETUP/IDLE timeouts.
    let tar = guest_package_tar()?;
    let package_dir = tempfile::tempdir()?;
    let package_path = package_dir.path().join("package");
    std::fs::write(&package_path, &tar)?;
    std::fs::set_permissions(&package_path, std::fs::Permissions::from_mode(0o400))?;
    let package = lillux::PinnedDirectory::open(package_dir.path())?
        .context("package directory vanished")?
        .open_pinned_regular(OsStr::new("package"), false)?
        .context("package file vanished")?
        .inherited_descriptor_authority()?;
    let controller_key = lillux::crypto::SigningKey::from_bytes(&[43; 32]).verifying_key();
    let mut intent = RuntimeSnapshotIntent {
        schema: RUNTIME_SNAPSHOT_INTENT_SCHEMA,
        operation_id: String::new(),
        owner_principal: format!("fp:{}", "1".repeat(64)),
        provider_id: ADAPTER_ID.into(),
        source_occurrence_id: source_id.clone(),
        source_bootstrap_operation_id: None,
        source_created_at: None,
        source_timeout_seconds: None,
        provider_group_id: live.group_id.clone(),
        production_profile_digest: "2".repeat(64),
        adapter_artifact_hash: "3".repeat(64),
        provider_spec_digest: "4".repeat(64),
        settings_digest: "5".repeat(64),
        source: RuntimeSnapshotSource::CapturedProduct {
            product_witness_hash: "1".repeat(64),
        },
        guest_runtime_manifest_hash: "2".repeat(64),
        owner_executable_sha256: "3".repeat(64),
        controller_public_root: format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(controller_key.to_bytes())
        ),
        upload_sha256: lillux::sha256_hex(&tar),
        upload_bytes: tar.len() as u64,
        attempt_deadline_ms: lillux::time::timestamp_millis() + 3_600_000,
    };
    intent.operation_id = intent.derived_operation_id()?;
    intent.validate()?;
    let snapshot_settings = live.snapshot_settings();
    snapshot_settings.validate_for_intent(&intent)?;
    let started = Instant::now();
    let uploaded = send_snapshot_upload(
        &intent,
        &live.snapshot_spec,
        &snapshot_settings,
        &package,
        &live.network,
        &live.credential,
        live.deadline(),
        &live.cancellation,
    )?;
    note(&format!(
        "uploaded {} bytes, provider status {}, {} ms",
        tar.len(),
        uploaded.provider_status,
        started.elapsed().as_millis()
    ));

    // 3. Run a short command and parse the SSE exactly as the verifier does.
    let source_contact = RenderContact::with_credential(
        &live.network,
        &live.spec,
        &settings,
        &source_id,
        live.deadline(),
        &live.cancellation,
        live.credential.clone(),
    );
    let stdout = run_command(&source_contact, RUN_COMMAND)?;
    ensure!(
        stdout.contains(RUN_MARKER),
        "source run output lacks the uploaded files"
    );
    note("source run stream parsed");

    // 4. Snapshot create (202, creating) and readiness through the adapter.
    let creation_intent = creation_intent_for(&intent, &snapshot_settings);
    creation_intent.validate()?;
    let creation = send_snapshot_create(
        &intent,
        &creation_intent,
        &live.snapshot_spec,
        &snapshot_settings,
        &live.network,
        &live.credential,
        live.deadline(),
        &live.cancellation,
    )?;
    note(&format!("snapshot {} requested", creation.snapshot_id));
    let locator = bind_snapshot_locator_for(&intent, &creation, &snapshot_settings)?;
    let readiness_url =
        snapshot_readiness_url(&live.snapshot_spec, &locator, &snapshot_settings.owner_id)?;
    wait_snapshot_available(
        live,
        &snapshot_settings,
        &creation_intent,
        &creation,
        &readiness_url,
    )?;

    // 5. Restore from the snapshot and check its filesystem.
    let restore_settings = live.settings(&creation.snapshot_id);
    validate_settings(&restore_settings)?;
    let restore_projection = live.spec.qualification_create_projection(
        &live.owner_id,
        PlanValue::Starter,
        &live.region,
        &creation.snapshot_id,
        RESTORE_LIFETIME_SECONDS,
    )?;
    let restore_url = route_url(
        live,
        live.spec
            .qualification_create_route()
            .context("provider spec has no qualification create route")?,
        None,
        &restore_settings,
    )?;
    let restore_body = canonical_json(&CreateSandboxBody {
        owner_id: restore_projection.owner_id.clone(),
        plan: RenderPlan::Starter,
        region: restore_projection.region.clone(),
        timeout_seconds: restore_projection.timeout_seconds,
        network_policy: NetworkPolicy::requested(&restore_projection.network_policy),
        snapshot_id: Some(restore_projection.snapshot_id.clone()),
    })?;
    let (status, body) = read_response(api(
        live,
        &restore_settings,
        &restore_url,
        "POST",
        Some(restore_body),
    )?)
    .map_err(|()| anyhow!("restore create response was not bounded JSON"))?;
    track_created(guard, &body);
    let restored = accepted_create_response(status, &body, &restore_projection)
        .with_context(|| format!("restore create (status {status}) refused by the adapter"))?;
    let restored_id = restored.id;
    let running = wait_running(live, &restore_settings, &restored_id, "restored")?;
    ensure!(
        create_response_matches(&running, &restore_projection)
            && running.terminated_at.is_none()
            && running.sandbox_group_id == live.group_id,
        "running restored sandbox refused by the adapter's validation"
    );
    let restored_contact = RenderContact::with_credential(
        &live.network,
        &live.spec,
        &restore_settings,
        &restored_id,
        live.deadline(),
        &live.cancellation,
        live.credential.clone(),
    );
    let restored_stdout = run_command(&restored_contact, RUN_COMMAND)?;
    ensure!(
        restored_stdout.contains(RUN_MARKER),
        "restored filesystem lacks the uploaded files"
    );
    note("restored run stream parsed; snapshot carries the upload");

    // 6. Terminate both and observe the terminal shape.
    terminate_and_observe(live, guard, &restore_settings, &restored_id, "restored")?;
    terminate_and_observe(live, guard, &settings, &source_id, "source")?;
    Ok(())
}

#[test]
#[ignore = "creates billable Render sandboxes; needs RYEOS_LIVE_RENDER_* environment"]
fn live_render_chain() -> Result<()> {
    let api_key = match std::env::var("RYEOS_LIVE_RENDER_API_KEY") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => {
            note("skipped: RYEOS_LIVE_RENDER_API_KEY is not set");
            return Ok(());
        }
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("RYEOS_LIVE_RENDER_API_KEY is not valid unicode")
        }
    };
    ensure!(
        !api_key.is_empty() && !api_key.bytes().any(|byte| byte.is_ascii_control()),
        "RYEOS_LIVE_RENDER_API_KEY has an invalid format"
    );
    let live = Live {
        network: NetworkContext::from_config_bytes(
            &std::fs::read("/etc/resolv.conf")?,
            &std::fs::read("/etc/hosts")?,
        )?,
        cancellation: NetworkCancellation::default(),
        credential: Zeroizing::new(api_key),
        spec: ProviderSpec::parse(
            include_bytes!("../../fixtures/provider-spec.json"),
            &lillux::sha256_hex(include_bytes!("../../fixtures/settings.schema.json")),
        )?,
        snapshot_spec: SnapshotProductionSpec::parse(include_bytes!(
            "../../fixtures/snapshot-production-spec.json"
        ))?,
        owner_id: required_env("RYEOS_LIVE_RENDER_OWNER_ID")?,
        region: std::env::var("RYEOS_LIVE_RENDER_REGION").unwrap_or_else(|_| "oregon".into()),
        group_id: required_env("RYEOS_LIVE_RENDER_SANDBOX_GROUP_ID")?,
        roots: roots_from_pem(&required_env("RYEOS_LIVE_RENDER_ROOTS_PEM")?)?,
    };
    let guard = Guard::new(&live);
    run_chain(&live, &guard)
}
