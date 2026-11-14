//! Schedule authority and bounds, end to end through the daemon.
//!
//! A schedule's signed lifetime bounds pause it by itself once used up, and a
//! fire refused because the schedule's pinned grant was revoked records a
//! typed outcome and pauses the schedule. In both cases the typed reason is
//! recorded beside `enabled: false`.

#![cfg(all(unix, feature = "scheduling"))]

use ryeosd::test_support as common;

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use common::DaemonHarness;
use lillux::crypto::SigningKey;
use serde_json::{Value, json};

/// POST a service through `/execute` as the harness operator; return
/// `(status, body)`.
async fn exec(h: &DaemonHarness, item_ref: &str, params: Value) -> (reqwest::StatusCode, Value) {
    h.post_execute(item_ref, ".", params)
        .await
        .unwrap_or_else(|error| panic!("post {item_ref}: {error:#}"))
}

fn result(status: reqwest::StatusCode, body: &Value, what: &str) -> Value {
    assert!(
        status.is_success(),
        "{what}: expected success, got {status}; body={body:#}"
    );
    body.get("result")
        .cloned()
        .unwrap_or_else(|| panic!("{what}: response has no result; body={body:#}"))
}

/// A projectless interval registration. Fires need not succeed: the lifetime
/// counts boundaries, not successful runs.
fn interval_registration(schedule_id: &str, interval_secs: u64) -> Value {
    json!({
        "schedule_id": schedule_id,
        "item_ref": "directive:test/hello",
        "ref_bindings": {"model": "directive:test/hello"},
        "schedule_type": "interval",
        "expression": interval_secs.to_string(),
        "params": {},
        "timezone": "UTC",
        "misfire_policy": "skip",
        "overlap_policy": "allow",
        "lateness_grace_secs": 60,
        "enabled": true,
        "capabilities": ["ryeos.execute.directive.test/*"],
        "execution_policy": {
            "schema_version": 2,
            "ownership": "daemon_owned",
            "recovery": "restart_recoverable",
            "response": "accepted",
            "target": {"kind": "here"},
            "environment": {"kind": "none"},
            "project": {"kind": "projectless"}
        },
    })
}

/// Poll `scheduler.explain` until `done` accepts it.
async fn explain_until(
    h: &DaemonHarness,
    schedule_id: &str,
    timeout: Duration,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let (status, body) = exec(
            h,
            "service:scheduler/explain",
            json!({"schedule_id": schedule_id}),
        )
        .await;
        let explained = result(status, &body, "scheduler.explain");
        if done(&explained) {
            return explained;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "schedule {schedule_id} did not reach the expected state in time: {explained:#}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn max_fires_pauses_the_schedule_with_a_typed_reason() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");

    let mut registration = interval_registration("bounded-by-count", 1);
    registration["max_fires"] = json!(2);
    let (status, body) = exec(&h, "service:scheduler/register", registration).await;
    let registered = result(status, &body, "register bounded schedule");
    assert_eq!(registered["max_fires"], 2, "{registered:#}");

    let explained = explain_until(&h, "bounded-by-count", Duration::from_secs(30), |e| {
        e["enabled"] == false
    })
    .await;
    assert_eq!(
        explained["paused_reason"], "max_fires_reached",
        "{explained:#}"
    );
    assert_eq!(explained["lifetime"]["max_fires"], 2, "{explained:#}");
    assert!(explained["next_fire_at"].is_null(), "{explained:#}");

    let (status, body) = exec(
        &h,
        "service:scheduler/show_fires",
        json!({"schedule_id": "bounded-by-count"}),
    )
    .await;
    let fires = result(status, &body, "show_fires");
    let fires = fires["fires"].as_array().expect("fire list");
    assert_eq!(
        fires.len(),
        2,
        "exactly the two admitted boundaries are recorded: {fires:#?}"
    );

    // Resuming a used-up lifetime is refused with the typed reason.
    let (status, body) = exec(
        &h,
        "service:scheduler/resume",
        json!({"schedule_id": "bounded-by-count"}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{body:#}");
    assert!(
        body.to_string().contains("schedule_lifetime_exhausted"),
        "resume must name the typed refusal: {body:#}"
    );
}

// ── A schedule owned by a second, revocable principal ─────────────────────

/// The scopes the second principal is granted: enough to register, inspect,
/// resume and reauthorize its schedule, and the Core doctor capabilities the
/// schedule fires with.
const OWNER_SCOPES: &[&str] = &[
    "ryeos.execute.service.scheduler/explain",
    "ryeos.execute.service.scheduler/pause",
    "ryeos.execute.service.scheduler/reauthorize",
    "ryeos.execute.service.scheduler/register",
    "ryeos.execute.service.scheduler/resume",
    "ryeos.execute.service.scheduler/show_fires",
    "ryeos.execute.tool.ryeos/core/node/doctor",
    "ryeos.read.project.live",
];

struct SecondPrincipal {
    key: SigningKey,
}

impl SecondPrincipal {
    fn new() -> Self {
        Self {
            key: SigningKey::from_bytes(&[0x7c; 32]),
        }
    }

    fn fingerprint(&self) -> String {
        lillux::signature::compute_fingerprint(&self.key.verifying_key())
    }

    fn grant_path(&self, h: &DaemonHarness) -> PathBuf {
        h.state_path
            .join(".ai/node/auth/authorized_keys")
            .join(format!("{}.toml", self.fingerprint()))
    }

    /// The operator creates this principal's grant: a new generation each
    /// time, because the grant records its creation instant.
    async fn grant(&self, h: &DaemonHarness, scopes: &[&str]) {
        let public_key =
            base64::engine::general_purpose::STANDARD.encode(self.key.verifying_key().to_bytes());
        let (status, body) = exec(
            h,
            "service:identity/authorize-key",
            json!({
                "public_key": format!("ed25519:{public_key}"),
                "label": "schedule owner",
                "scopes": scopes,
            }),
        )
        .await;
        result(status, &body, "authorize the schedule owner");
    }

    /// Revoke by removing the grant file, as an operator does today.
    fn revoke(&self, h: &DaemonHarness) {
        std::fs::remove_file(self.grant_path(h)).expect("remove the owner grant");
    }

    /// POST a service through `/execute`, signed by this principal.
    async fn exec(
        &self,
        h: &DaemonHarness,
        item_ref: &str,
        params: Value,
    ) -> (reqwest::StatusCode, Value) {
        let body = json!({
            "item_ref": item_ref,
            "ref_bindings": {},
            "project_path": null,
            "parameters": params,
            "execution_policy": ryeos_app::execution_policy::ExecutionPolicy::projectless(
                ryeos_app::execution_policy::ExecutionResponse::Wait,
            ),
        });
        let bytes = serde_json::to_vec(&body).expect("encode request");
        let node_key = h.node_key.as_ref().expect("fast fixture node key");
        let mut request = reqwest::Client::new()
            .post(format!("http://{}/execute", h.bind))
            .header("content-type", "application/json")
            .body(bytes.clone());
        for (name, value) in
            common::build_signed_headers_for_bytes(&self.key, node_key, "POST", "/execute", &bytes)
        {
            request = request.header(name, value);
        }
        let response = request.send().await.expect("send owner request");
        let status = response.status();
        let value = response.json().await.unwrap_or_else(|_| json!({}));
        (status, value)
    }
}

/// A live read-only project the Core doctor tool can inspect, as the doctor
/// schedule in `scheduler_e2e.rs` uses.
fn doctor_project(h: &DaemonHarness) -> PathBuf {
    let project = h.user_space.path().join("authority-schedule-project");
    let probe = project.join(".ai/fixtures/authority-probe/.ai");
    std::fs::create_dir_all(&probe).expect("create doctor probe source");
    std::fs::write(
        project.join(".ai/manifest.source.yaml"),
        "name: authority-schedule-project\nversion: \"0.1.0\"\n",
    )
    .expect("write project manifest");
    std::fs::write(
        probe.join("manifest.source.yaml"),
        "name: authority-probe\nversion: \"0.1.0\"\n",
    )
    .expect("write probe manifest");
    project.canonicalize().expect("resolve project root")
}

fn doctor_registration(h: &DaemonHarness, schedule_id: &str, project: &std::path::Path) -> Value {
    json!({
        "schedule_id": schedule_id,
        "item_ref": "tool:ryeos/core/node/doctor",
        "ref_bindings": {},
        "schedule_type": "interval",
        "expression": "5",
        "params": {
            "source": ".ai/fixtures/authority-probe",
            "registry_roots": [h.state_path.join(".ai/bundles/core").display().to_string()]
        },
        "timezone": "UTC",
        "misfire_policy": "skip",
        "overlap_policy": "skip",
        "lateness_grace_secs": 60,
        "enabled": true,
        "project_root": project.display().to_string(),
        "capabilities": [
            "ryeos.execute.tool.ryeos/core/node/doctor",
            "ryeos.read.project.live"
        ],
        "execution_policy": {
            "schema_version": 2,
            "ownership": "daemon_owned",
            "recovery": "restart_recoverable",
            "response": "accepted",
            "target": {"kind": "here"},
            "environment": {"kind": "none"},
            "project": {
                "kind": "live_direct",
                "access": "read_only",
                "child_policy": {"kind": "inherit"}
            }
        },
    })
}

/// Wait until the schedule's fire journal records `outcome`. The owner cannot
/// ask the daemon while its grant is revoked, so the test reads the journal.
async fn journal_records(h: &DaemonHarness, schedule_id: &str, outcome: &str) {
    let journal = h
        .state_path
        .join(".ai/state/schedules")
        .join(schedule_id)
        .join("fires.jsonl");
    let needle = format!("\"outcome\":\"{outcome}\"");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if std::fs::read_to_string(&journal).is_ok_and(|journal| journal.contains(&needle)) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "schedule {schedule_id} never recorded a fire with outcome {outcome}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_grant_pauses_schedule_with_typed_outcome() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");
    let owner = SecondPrincipal::new();
    owner.grant(&h, OWNER_SCOPES).await;
    let project = doctor_project(&h);

    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/register",
            doctor_registration(&h, "revocable-owner", &project),
        )
        .await;
    result(status, &body, "owner registers the schedule");

    // Revoke before the first boundary: that fire is refused, typed, and the
    // schedule pauses itself.
    owner.revoke(&h);
    journal_records(&h, "revocable-owner", "authority_revoked").await;

    // Grant the key again: a new generation, so the owner can ask, and the
    // schedule's pinned generation is still not the current one.
    owner.grant(&h, OWNER_SCOPES).await;
    let explained = explain_as(&h, &owner, "revocable-owner", |e| e["enabled"] == false).await;
    assert_eq!(
        explained["paused_reason"], "authority_revoked",
        "{explained:#}"
    );
    assert_eq!(explained["authority"]["valid"], false, "{explained:#}");
    assert_eq!(
        explained["authority"]["refusal"], "grant_generation_changed",
        "{explained:#}"
    );

    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/show_fires",
            json!({"schedule_id": "revocable-owner"}),
        )
        .await;
    let fires = result(status, &body, "owner reads fire history");
    let fires = fires["fires"].as_array().expect("fire list");
    assert_eq!(fires.len(), 1, "one refused fire, then paused: {fires:#?}");
    assert_eq!(fires[0]["status"], "failed", "{fires:#?}");
    assert_eq!(fires[0]["outcome"], "authority_revoked", "{fires:#?}");
    assert!(fires[0]["admitted_capsule_hash"].is_null(), "{fires:#?}");

    // Resume refuses while the pinned generation is superseded.
    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/resume",
            json!({"schedule_id": "revocable-owner"}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{body:#}");
    assert!(
        body.to_string().contains("schedule_authority_invalid"),
        "{body:#}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rewritten_grant_then_reauthorize_restores_fires() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");
    let owner = SecondPrincipal::new();
    owner.grant(&h, OWNER_SCOPES).await;
    let project = doctor_project(&h);
    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/register",
            doctor_registration(&h, "reauthorized-owner", &project),
        )
        .await;
    result(status, &body, "owner registers the schedule");

    // Re-issue the grant with the same scopes before the first boundary: a
    // new generation, so the fire is refused as changed and the schedule
    // pauses.
    owner.revoke(&h);
    // A grant records its creation to the second; identical scopes in the
    // same second would be byte-identical, the same generation.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    owner.grant(&h, OWNER_SCOPES).await;
    journal_records(&h, "reauthorized-owner", "authority_changed").await;
    let explained = explain_as(&h, &owner, "reauthorized-owner", |e| e["enabled"] == false).await;
    assert_eq!(
        explained["paused_reason"], "authority_changed",
        "{explained:#}"
    );

    // Only the owner may re-pin; anyone else is told nothing exists.
    let (status, body) = exec(
        &h,
        "service:scheduler/reauthorize",
        json!({"schedule_id": "reauthorized-owner"}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{body:#}");

    // A grant that no longer covers the schedule's capabilities cannot re-pin it.
    owner.revoke(&h);
    let narrowed: Vec<&str> = OWNER_SCOPES
        .iter()
        .copied()
        .filter(|scope| *scope != "ryeos.read.project.live")
        .collect();
    owner.grant(&h, &narrowed).await;
    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/reauthorize",
            json!({"schedule_id": "reauthorized-owner"}),
        )
        .await;
    assert_eq!(status, reqwest::StatusCode::CONFLICT, "{body:#}");
    assert!(
        body.to_string()
            .contains("schedule_reauthorize_not_covered"),
        "{body:#}"
    );
    assert!(
        body.to_string().contains("ryeos.read.project.live"),
        "{body:#}"
    );

    // A covering grant re-pins it; resume then fires again.
    owner.revoke(&h);
    owner.grant(&h, OWNER_SCOPES).await;
    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/reauthorize",
            json!({"schedule_id": "reauthorized-owner"}),
        )
        .await;
    let reauthorized = result(status, &body, "owner reauthorizes the schedule");
    assert_eq!(reauthorized["reauthorized"], true, "{reauthorized:#}");
    assert_eq!(reauthorized["enabled"], false, "{reauthorized:#}");
    assert_ne!(
        reauthorized["principal_grant_hash"], reauthorized["previous_principal_grant_hash"],
        "{reauthorized:#}"
    );
    let explained = explain_as(&h, &owner, "reauthorized-owner", |_| true).await;
    assert_eq!(explained["authority"]["valid"], true, "{explained:#}");

    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/resume",
            json!({"schedule_id": "reauthorized-owner"}),
        )
        .await;
    result(status, &body, "owner resumes the reauthorized schedule");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let (status, body) = owner
            .exec(
                &h,
                "service:scheduler/show_fires",
                json!({"schedule_id": "reauthorized-owner"}),
            )
            .await;
        let fires = result(status, &body, "owner reads fire history");
        let fires = fires["fires"].as_array().expect("fire list");
        if fires
            .iter()
            .any(|fire| matches!(fire["status"].as_str(), Some("dispatched" | "completed")))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reauthorized schedule did not dispatch a fire: {fires:#?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

impl SecondPrincipal {
    /// As [`Self::grant`], with an explicit lifetime in seconds.
    async fn grant_for(&self, h: &DaemonHarness, scopes: &[&str], lifetime_secs: u64) {
        let public_key =
            base64::engine::general_purpose::STANDARD.encode(self.key.verifying_key().to_bytes());
        let (status, body) = exec(
            h,
            "service:identity/authorize-key",
            json!({
                "public_key": format!("ed25519:{public_key}"),
                "label": "schedule owner",
                "scopes": scopes,
                "lifetime_secs": lifetime_secs,
            }),
        )
        .await;
        result(
            status,
            &body,
            "authorize the schedule owner for a short lifetime",
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn grant_renewal_keeps_schedule_firing() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");
    let owner = SecondPrincipal::new();
    owner.grant(&h, OWNER_SCOPES).await;
    let project = doctor_project(&h);
    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/register",
            doctor_registration(&h, "renewed-owner", &project),
        )
        .await;
    result(status, &body, "owner registers the schedule");

    // A grant records its expiry to the second: renewing within the second it
    // was issued would be the same generation.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let (status, body) = exec(
        &h,
        "service:identity/renew-key",
        json!({"fingerprint": owner.fingerprint()}),
    )
    .await;
    let renewed = result(status, &body, "operator renews the owner grant");
    assert_eq!(
        renewed["schedules"]["resealed"],
        json!(["renewed-owner"]),
        "{renewed:#}"
    );
    assert_eq!(renewed["schedules"]["paused"], json!([]), "{renewed:#}");

    // The schedule is still valid and enabled under the new generation, and
    // fires.
    let explained = explain_as(&h, &owner, "renewed-owner", |_| true).await;
    assert_eq!(explained["authority"]["valid"], true, "{explained:#}");
    assert_eq!(explained["enabled"], true, "{explained:#}");
    journal_records(&h, "renewed-owner", "dispatched").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grant_expiry_pauses_schedule_with_typed_outcome() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");
    let owner = SecondPrincipal::new();
    // The grant lapses before the schedule's first five-second boundary.
    owner.grant_for(&h, OWNER_SCOPES, 3).await;
    let project = doctor_project(&h);
    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/register",
            doctor_registration(&h, "expiring-owner", &project),
        )
        .await;
    result(status, &body, "owner registers the schedule");

    journal_records(&h, "expiring-owner", "authority_expired").await;

    // Renewing the lapsed grant supersedes the generation the paused schedule
    // pins, so it is re-sealed (and stays paused until the owner resumes it).
    let (status, body) = exec(
        &h,
        "service:identity/renew-key",
        json!({"fingerprint": owner.fingerprint()}),
    )
    .await;
    let renewed = result(status, &body, "operator renews the lapsed grant");
    assert_eq!(
        renewed["schedules"]["resealed"],
        json!(["expiring-owner"]),
        "{renewed:#}"
    );
    let explained = explain_as(&h, &owner, "expiring-owner", |e| e["enabled"] == false).await;
    assert_eq!(
        explained["paused_reason"], "authority_expired",
        "{explained:#}"
    );
    assert_eq!(explained["authority"]["valid"], true, "{explained:#}");
}

#[tokio::test(flavor = "multi_thread")]
async fn revoke_pauses_schedule_before_next_boundary() {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling()
        .await
        .expect("start Core+Scheduling daemon");
    let owner = SecondPrincipal::new();
    owner.grant(&h, OWNER_SCOPES).await;
    let project = doctor_project(&h);
    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/register",
            doctor_registration(&h, "revoked-early", &project),
        )
        .await;
    result(status, &body, "owner registers the schedule");

    // Revocation pauses it in the same operation, before any boundary.
    let (status, body) = exec(
        &h,
        "service:identity/revoke-key",
        json!({"fingerprint": owner.fingerprint(), "reason": "key compromised"}),
    )
    .await;
    let revoked = result(status, &body, "operator revokes the owner grant");
    assert_eq!(
        revoked["schedules"]["paused"],
        json!([{
            "schedule_id": "revoked-early",
            "reason": "authority_revoked",
            "already_paused": false,
        }]),
        "{revoked:#}"
    );

    // A new grant for the same key does not revive it.
    owner.grant(&h, OWNER_SCOPES).await;
    let explained = explain_as(&h, &owner, "revoked-early", |e| e["enabled"] == false).await;
    assert_eq!(
        explained["paused_reason"], "authority_revoked",
        "{explained:#}"
    );
    assert_eq!(explained["authority"]["valid"], false, "{explained:#}");
    tokio::time::sleep(Duration::from_secs(7)).await;
    let (status, body) = owner
        .exec(
            &h,
            "service:scheduler/show_fires",
            json!({"schedule_id": "revoked-early"}),
        )
        .await;
    let fires = result(status, &body, "owner reads fire history");
    assert_eq!(
        fires["fires"],
        json!([]),
        "no boundary fired after the revocation: {fires:#}"
    );
}

/// Poll `scheduler.explain` as `owner` until `done` accepts it.
async fn explain_as(
    h: &DaemonHarness,
    owner: &SecondPrincipal,
    schedule_id: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = owner
            .exec(
                h,
                "service:scheduler/explain",
                json!({"schedule_id": schedule_id}),
            )
            .await;
        let explained = result(status, &body, "owner explains the schedule");
        if done(&explained) {
            return explained;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "schedule {schedule_id} did not reach the expected state: {explained:#}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
