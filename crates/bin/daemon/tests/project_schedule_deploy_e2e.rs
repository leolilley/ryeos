//! Project schedule deployment through the real push and apply-snapshot
//! routes of a running daemon.
//!
//! A Core+Scheduling node deploys a pushed project's schedule declarations
//! with its live AI surfaces; a Core node refuses the same push at apply
//! because it has no scheduler service. In a crash-qualification build the
//! apply handler parks at one exact durable boundary between apply-journal
//! publication and schedule-session finalization; the harness SIGKILLs the
//! daemon there and restarts it, and the deploy must come back either fully
//! restored to the previously deployed generation (its exact declaration
//! bytes and schedules) or fully completed — never mixed.

#![cfg(all(unix, feature = "scheduling"))]
#![recursion_limit = "256"]

use ryeosd::test_support as common;
#[path = "common/project_apply.rs"]
mod project_apply;
#[path = "common/project_push.rs"]
mod project_push;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tempfile::TempDir;

use common::DaemonHarness;

const SCHEDULE_ID: &str = "project-deploy-doctor";
const RETIRED_ID: &str = "project-deploy-doctor-retired";
const DECLARATION: &str = ".ai/config/schedules/deploy.yaml";

/// One pushed project generation's schedule declarations.
#[derive(Clone, Copy, Debug)]
enum Generation {
    /// Two yearly schedules: [`SCHEDULE_ID`] on January 1 and [`RETIRED_ID`].
    A,
    /// Updates [`SCHEDULE_ID`] to February 1 and deletes [`RETIRED_ID`].
    B,
    /// Declares nothing: the declaration file is omitted from the pushed
    /// tree, so every schedule of the previous generation is deleted.
    C,
}

impl Generation {
    /// Whether this generation declares no schedules at all.
    fn declares_nothing(self) -> bool {
        self.schedules().is_empty()
    }

    /// `(schedule_id, cron expression)` pairs this generation declares.
    fn schedules(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::A => &[(SCHEDULE_ID, "0 0 0 1 1 *"), (RETIRED_ID, "0 0 0 1 1 *")],
            Self::B => &[(SCHEDULE_ID, "0 0 0 1 2 *")],
            Self::C => &[],
        }
    }
}

/// A live target project and the source tree pushed into it.
struct ProjectFixture {
    _live: TempDir,
    _source: TempDir,
    live_path: String,
    live_root: PathBuf,
    source_root: PathBuf,
}

impl ProjectFixture {
    fn new() -> Result<Self> {
        let live = TempDir::new()?;
        let live_root = live.path().canonicalize()?;
        let live_path = live_root
            .to_str()
            .context("live project path is not UTF-8")?
            .to_owned();
        let source = TempDir::new()?;
        let source_root = source.path().canonicalize()?;
        // Both checkouts name the same project in their manifests: the push
        // target and the snapshot's declared name must agree.
        for root in [&live_root, &source_root] {
            std::fs::create_dir_all(root.join(".ai"))?;
            std::fs::write(root.join(".ai/manifest.yaml"), "name: schedule-project\n")?;
        }
        Ok(Self {
            _live: live,
            _source: source,
            live_path,
            live_root,
            source_root,
        })
    }

    fn live_declaration(&self) -> PathBuf {
        self.live_root.join(DECLARATION)
    }

    fn declaration(&self, generation: Generation) -> String {
        schedule_declaration(&self.live_path, generation)
    }

    /// Make the source tree exactly `generation`, capture it with `parent`,
    /// and publish it as the caller's HEAD.
    async fn push(
        &self,
        h: &DaemonHarness,
        generation: Generation,
        parent: Option<&str>,
    ) -> Result<String> {
        let declaration = self.source_root.join(DECLARATION);
        // An empty tree has no snapshot entries, so omission needs no file.
        if generation.declares_nothing() {
            match std::fs::remove_file(&declaration) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            std::fs::create_dir_all(declaration.parent().context("declaration has a parent")?)?;
            std::fs::write(&declaration, self.declaration(generation))?;
        }
        let captured = project_push::capture_ai_only_snapshot(&self.source_root, parent)?;
        Ok(project_push::push_snapshot(h, &self.live_path, &captured)
            .await?
            .snapshot_hash)
    }
}

/// Project-backed schedules that never fire within a test: yearly crons for a
/// Core tool reading the live project.
fn schedule_declaration(live_path: &str, generation: Generation) -> String {
    let mut text =
        String::from("category: schedules\nversion: 1.0.0\nschema_version: 1.0.0\nschedules:\n");
    for (schedule_id, expression) in generation.schedules() {
        text.push_str(&format!(
            r#"  - schedule_id: {schedule_id}
    item_ref: tool:ryeos/core/node/doctor
    ref_bindings: {{}}
    schedule_type: cron
    expression: "{expression}"
    timezone: UTC
    misfire_policy: skip
    overlap_policy: skip
    lateness_grace_secs: 60
    enabled: true
    capabilities:
      - ryeos.execute.tool.ryeos/core/node/doctor
      - ryeos.read.project.live
    execution_policy:
      schema_version: 2
      ownership: daemon_owned
      recovery: restart_recoverable
      response: accepted
      target:
        kind: here
      environment:
        kind: none
      project:
        kind: live_direct
        access: read_only
        child_policy:
          kind: inherit
    project_root: {live_path}
    params: {{}}
"#
        ));
    }
    text
}

fn service_result(body: &Value) -> &Value {
    body.get("result").unwrap_or(body)
}

/// `(schedule_id, expression)` of every project schedule the node's scheduler
/// projection lists, sorted.
async fn listed_schedules(h: &DaemonHarness) -> Result<Vec<(String, String)>> {
    let (status, body) = h
        .post_execute("service:scheduler/list", ".", json!({}))
        .await?;
    anyhow::ensure!(
        status.is_success(),
        "scheduler/list refused with {status}: {body}"
    );
    let schedules = service_result(&body)["schedules"]
        .as_array()
        .with_context(|| format!("scheduler/list returned no schedules array: {body}"))?;
    let mut listed = Vec::new();
    for schedule in schedules {
        let id = schedule["schedule_id"].as_str().unwrap_or_default();
        if id == SCHEDULE_ID || id == RETIRED_ID {
            listed.push((
                id.to_owned(),
                schedule["expression"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ));
        }
    }
    listed.sort();
    Ok(listed)
}

/// The node is exactly at `generation`: deployed ref, live declaration bytes
/// and every projected schedule (updates and deletions included) match it.
async fn assert_generation(
    h: &DaemonHarness,
    project: &ProjectFixture,
    generation: Generation,
    deployed: &str,
) -> Result<()> {
    assert_eq!(
        project_apply::deployed_snapshot(h, &project.live_path)
            .await?
            .as_deref(),
        Some(deployed),
        "deployed ref must name generation {generation:?}"
    );
    if generation.declares_nothing() {
        assert!(
            !project.live_declaration().exists(),
            "generation {generation:?} declares nothing, so no live declaration may remain"
        );
    } else {
        assert_eq!(
            std::fs::read_to_string(project.live_declaration())
                .context("live schedule declaration must be materialized")?,
            project.declaration(generation),
            "live schedule declaration must be generation {generation:?}'s bytes"
        );
    }
    let mut expected: Vec<(String, String)> = generation
        .schedules()
        .iter()
        .map(|(id, expression)| ((*id).to_owned(), (*expression).to_owned()))
        .collect();
    expected.sort();
    assert_eq!(
        listed_schedules(h).await?,
        expected,
        "scheduler projection must be generation {generation:?}'s schedules"
    );
    Ok(())
}

/// No apply transaction workspace may outlive settlement or recovery.
fn assert_no_apply_transaction_left(live_root: &Path) -> Result<()> {
    let transactions = live_root.join(".ai/state/project-apply-transactions");
    match std::fs::read_dir(&transactions) {
        Ok(entries) => {
            let left: Vec<_> = entries
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<std::io::Result<_>>()?;
            assert!(
                left.is_empty(),
                "apply transactions survived settlement: {left:?}"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Apply a pushed HEAD and require success, returning the report.
async fn apply_ok(
    h: &DaemonHarness,
    project: &ProjectFixture,
    target: &str,
    expected_deployed: Option<&str>,
) -> Result<Value> {
    let (status, body) =
        project_apply::apply_snapshot(h, &project.live_path, target, expected_deployed).await?;
    anyhow::ensure!(
        status.is_success(),
        "apply-snapshot refused with {status}: {body}"
    );
    Ok(service_result(&body).clone())
}

/// Deploy generation A cleanly and return its snapshot hash.
async fn deploy_a(h: &DaemonHarness, project: &ProjectFixture) -> Result<String> {
    let a = project.push(h, Generation::A, None).await?;
    let report = apply_ok(h, project, &a, None).await?;
    assert_eq!(report["schedules"]["declared"], 2, "{report}");
    assert_eq!(report["schedules"]["created"], 2, "{report}");
    assert_generation(h, project, Generation::A, &a).await?;
    Ok(a)
}

#[tokio::test(flavor = "multi_thread")]
async fn core_scheduling_push_deploys_project_schedules() -> Result<()> {
    let (h, _fixture) = DaemonHarness::start_fast_core_scheduling().await?;
    let project = ProjectFixture::new()?;
    let a = deploy_a(&h, &project).await?;

    let b = project.push(&h, Generation::B, Some(&a)).await?;
    let report = apply_ok(&h, &project, &b, Some(&a)).await?;
    assert_eq!(report["schedules"]["updated"], 1, "{report}");
    assert_eq!(report["schedules"]["deleted"], 1, "{report}");
    assert_generation(&h, &project, Generation::B, &b).await?;

    let report = apply_ok(&h, &project, &b, Some(&a)).await?;
    assert_eq!(report["idempotent"], true, "{report}");
    assert_no_apply_transaction_left(&project.live_root)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn omitting_every_declaration_deletes_all_schedules_and_restart_is_idempotent() -> Result<()>
{
    let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling().await?;
    let project = ProjectFixture::new()?;
    let a = deploy_a(&h, &project).await?;

    let c = project.push(&h, Generation::C, Some(&a)).await?;
    let report = apply_ok(&h, &project, &c, Some(&a)).await?;
    assert_eq!(report["schedules"]["declared"], 0, "{report}");
    assert_eq!(report["schedules"]["deleted"], 2, "{report}");
    assert_generation(&h, &project, Generation::C, &c).await?;
    assert_no_apply_transaction_left(&project.live_root)?;

    h.stop_gracefully().await?;
    h.respawn_with(|_| {}).await?;
    assert_generation(&h, &project, Generation::C, &c).await?;
    let report = apply_ok(&h, &project, &c, Some(&a)).await?;
    assert_eq!(report["idempotent"], true, "{report}");
    assert_generation(&h, &project, Generation::C, &c).await?;
    assert_no_apply_transaction_left(&project.live_root)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn core_refuses_a_push_declaring_schedules() -> Result<()> {
    let (h, _fixture) = DaemonHarness::start_fast_core_only_prepared().await?;
    let project = ProjectFixture::new()?;
    let target = project.push(&h, Generation::A, None).await?;

    let (status, body) =
        project_apply::apply_snapshot(&h, &project.live_path, &target, None).await?;
    assert!(
        !status.is_success(),
        "Core must refuse a deploy declaring schedules; got {status}: {body}"
    );
    assert!(
        body.to_string()
            .contains("declares schedules but the selected composition has no scheduler service"),
        "refusal must name the missing scheduler service: {body}"
    );
    assert_eq!(
        project_apply::deployed_snapshot(&h, &project.live_path).await?,
        None,
        "a refused deploy must not publish a deployed ref"
    );
    assert!(
        !project.live_root.join(".ai/config").exists(),
        "a refused deploy must not materialize any live AI surface"
    );
    assert_no_apply_transaction_left(&project.live_root)?;
    // A Core node never selects the scheduler, so it holds no scheduler
    // database, journal or fire history even when the build carries one.
    let runtime_state = h.state_path.join(ryeos_engine::AI_DIR).join("state");
    let scheduler_entries = std::fs::read_dir(&runtime_state)
        .with_context(|| format!("list {}", runtime_state.display()))?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|name| name.starts_with("scheduler") || name == "schedules")
        .collect::<Vec<_>>();
    assert!(
        scheduler_entries.is_empty(),
        "a Core node must not create scheduler state: {scheduler_entries:?}"
    );
    // Nor does it publish a signed schedule source for the refused project,
    // or even create the directory one would live in.
    let node_schedules = h
        .state_path
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("schedules");
    match std::fs::symlink_metadata(&node_schedules) {
        Ok(metadata) => anyhow::bail!(
            "a Core node must not create {} (a {:?})",
            node_schedules.display(),
            metadata.file_type()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("observe {}", node_schedules.display()));
        }
    }
    Ok(())
}

#[cfg(feature = "crash-qualification-test-support")]
mod crash {
    use super::*;
    use lillux::time::{Duration, MonotonicTimer};

    /// What restart recovery must have produced for one boundary.
    #[derive(Clone, Copy, Debug)]
    enum Recovered {
        /// The deployed ref never moved: live surfaces and schedules are the
        /// previous generation's.
        Restored,
        /// The deployed ref moved: live surfaces and schedules are the
        /// target generation's.
        Completed,
    }

    /// Wait until the daemon announces the parked boundary on stderr.
    async fn wait_for_cut(h: &mut DaemonHarness, boundary: &str) -> Result<()> {
        let marker = format!(
            "{}project_apply:{boundary}",
            ryeos_app::env_contract::TEST_CRASH_CUT_REACHED_PREFIX
        );
        let timer = MonotonicTimer::start();
        loop {
            let stderr = h.drain_stderr_nonblocking().await;
            if stderr.lines().any(|line| line == marker) {
                return Ok(());
            }
            anyhow::ensure!(
                timer.elapsed() < Duration::from_secs(120),
                "daemon did not reach apply crash cut {boundary} within 120s; stderr:\n{stderr}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// POST apply-snapshot from a detached task so the request may hang at
    /// the parked boundary while the test observes and kills the daemon.
    fn spawn_apply(
        h: &DaemonHarness,
        body: Value,
    ) -> Result<tokio::task::JoinHandle<Result<reqwest::StatusCode>>> {
        let user_key = h.user_key.clone().context("fast fixture user key")?;
        let node_key = h.node_key.clone().context("fast fixture node key")?;
        let url = format!("http://{}/project/apply-snapshot", h.bind);
        Ok(tokio::spawn(async move {
            let bytes = serde_json::to_vec(&body)?;
            let mut request = reqwest::Client::new()
                .post(url)
                .header("content-type", "application/json")
                .body(bytes.clone());
            for (name, value) in common::build_signed_headers_for_bytes(
                &user_key,
                &node_key,
                "POST",
                "/project/apply-snapshot",
                &bytes,
            ) {
                request = request.header(name, value);
            }
            Ok(request.send().await?.status())
        }))
    }

    /// Deploy generation A cleanly, then crash at `boundary` while applying
    /// B (one update and one deletion) and restart. Restored must bring back
    /// A's exact declaration bytes and schedules, not an empty project.
    async fn crash_at(boundary: &'static str, expected: Recovered) -> Result<()> {
        let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling().await?;
        let project = ProjectFixture::new()?;
        let a = deploy_a(&h, &project).await?;
        h.stop_gracefully().await?;
        h.respawn_with(|command| {
            command.env(
                ryeos_app::env_contract::PROJECT_APPLY_TEST_CUT_ENV,
                boundary,
            );
        })
        .await?;

        let b = project.push(&h, Generation::B, Some(&a)).await?;
        let request = spawn_apply(
            &h,
            project_apply::apply_request(&project.live_path, &b, Some(&a)),
        )?;
        wait_for_cut(&mut h, boundary).await?;
        h.kill_daemon().await?;
        if let Ok(status) = request.await? {
            assert!(
                !status.is_success(),
                "a request parked at {boundary} must not report success after SIGKILL"
            );
        }

        // Startup recovery settles the retained journal before readiness.
        h.respawn_with(|_| {}).await?;
        match expected {
            Recovered::Restored => {
                assert_generation(&h, &project, Generation::A, &a).await?;
                assert_no_apply_transaction_left(&project.live_root)?;
                // A second restart finds nothing left to recover.
                h.stop_gracefully().await?;
                h.respawn_with(|_| {}).await?;
                assert_generation(&h, &project, Generation::A, &a).await?;
                assert_no_apply_transaction_left(&project.live_root)?;
                let report = apply_ok(&h, &project, &b, Some(&a)).await?;
                assert_eq!(report["schedules"]["updated"], 1, "{report}");
                assert_eq!(report["schedules"]["deleted"], 1, "{report}");
                assert_generation(&h, &project, Generation::B, &b).await?;
            }
            Recovered::Completed => {
                assert_generation(&h, &project, Generation::B, &b).await?;
                let report = apply_ok(&h, &project, &b, Some(&a)).await?;
                assert_eq!(report["idempotent"], true, "{report}");
            }
        }
        assert_no_apply_transaction_left(&project.live_root)?;
        Ok(())
    }

    /// Every regular file under `root`, relative path to bytes, for a
    /// byte-identity comparison of a kept transaction directory.
    fn directory_bytes(
        root: &Path,
    ) -> Result<std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>> {
        fn walk(
            base: &Path,
            dir: &Path,
            out: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) -> Result<()> {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                let kind = entry.file_type()?;
                let relative = path.strip_prefix(base)?.to_path_buf();
                if kind.is_dir() {
                    out.insert(relative, Vec::new());
                    walk(base, &path, out)?;
                } else {
                    out.insert(relative, std::fs::read(&path)?);
                }
            }
            Ok(())
        }
        let mut out = std::collections::BTreeMap::new();
        walk(root, root, &mut out)?;
        Ok(out)
    }

    /// `ryeos node doctor --json` check named `name`.
    fn doctor_check(h: &DaemonHarness, name: &str) -> Result<Value> {
        let output = std::process::Command::new(common::ryeos_binary())
            .args(["node", "doctor", "--no-bundles", "--json", "--app-root"])
            .arg(&h.state_path)
            .env("RYEOS_APP_ROOT", &h.state_path)
            .env("RYEOSD_BIN", common::ryeosd_binary())
            .env("HOME", h.user_space.path())
            .env("NO_COLOR", "1")
            .output()?;
        let doctor: Value = serde_json::from_slice(&output.stdout).with_context(|| {
            format!(
                "doctor stdout is not JSON; stderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        })?;
        doctor["checks"]
            .as_array()
            .context("doctor without checks")?
            .iter()
            .find(|check| check["name"] == name)
            .cloned()
            .with_context(|| format!("doctor without a {name} check"))
    }

    /// A crashed apply whose journal can no longer be read must not block
    /// start: the daemon quarantines the job, keeps its transaction
    /// directory byte-for-byte, reports it to `node doctor`, and a new apply
    /// on the same project still succeeds without touching that directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreadable_journal_is_quarantined_and_does_not_block_start() -> Result<()> {
        poisoned_journal_scenario(false).await
    }

    /// The same scenario, plus the real `ryeos node doctor` binary reading the
    /// report. It needs the prepared CLI artifact, which a validation queue
    /// builds; a bare runner has none.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_quarantined_apply_is_named_by_node_doctor() -> Result<()> {
        poisoned_journal_scenario(true).await
    }

    async fn poisoned_journal_scenario(with_doctor: bool) -> Result<()> {
        let boundary = "journal_published";
        let (mut h, _fixture) = DaemonHarness::start_fast_core_scheduling().await?;
        let project = ProjectFixture::new()?;
        let a = deploy_a(&h, &project).await?;
        h.stop_gracefully().await?;
        h.respawn_with(|command| {
            command.env(
                ryeos_app::env_contract::PROJECT_APPLY_TEST_CUT_ENV,
                boundary,
            );
        })
        .await?;
        let b = project.push(&h, Generation::B, Some(&a)).await?;
        let request = spawn_apply(
            &h,
            project_apply::apply_request(&project.live_path, &b, Some(&a)),
        )?;
        wait_for_cut(&mut h, boundary).await?;
        h.kill_daemon().await?;
        let _ = request.await?;

        // Poison the crashed apply: rewrite its journal as the retired
        // schema-1 shape no reader exists for.
        let transactions = project
            .live_root
            .join(".ai/state/project-apply-transactions");
        let mut dirs = std::fs::read_dir(&transactions)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(dirs.len(), 1, "exactly the crashed transaction: {dirs:?}");
        let transaction = dirs.remove(0);
        let journal_path = transaction.join("journal.json");
        let mut journal: Value = serde_json::from_slice(&std::fs::read(&journal_path)?)?;
        journal["schema"] = json!(1);
        for surface in journal["surfaces"].as_array_mut().context("surfaces")? {
            surface.as_object_mut().context("surface")?.remove("shape");
        }
        std::fs::write(&journal_path, serde_json::to_vec(&journal)?)?;
        let kept = directory_bytes(&transaction)?;

        // The daemon boots instead of refusing to start.
        h.respawn_with(|_| {}).await?;
        assert!(
            h.stderr_so_far().contains("quarantined"),
            "startup did not log the quarantine:\n{}",
            h.stderr_tail(40)
        );
        assert_eq!(directory_bytes(&transaction)?, kept);

        let state_dir = ryeos_engine::roots::RuntimeRoot::new(h.state_path.clone()).state();
        let report = ryeos_app::sync_apply_quarantine_report::read_report(&state_dir)?
            .context("startup wrote no sync apply quarantine report")?;
        assert_eq!(report.jobs.len(), 1, "{report:?}");
        let entry = &report.jobs[0];
        assert_eq!(entry.code, "journal_unreadable");
        assert_eq!(
            entry.transaction_path.as_deref(),
            transaction.to_str(),
            "{entry:?}"
        );

        let (status, body) = h
            .post_execute(
                "service:sync/jobs/list",
                ".",
                json!({"state": "failed", "limit": 50}),
            )
            .await?;
        anyhow::ensure!(status.is_success(), "sync/jobs/list refused: {body}");
        let jobs = service_result(&body)["jobs"]
            .as_array()
            .with_context(|| format!("no jobs array: {body}"))?;
        let job = jobs
            .iter()
            .find(|job| job["job_id"] == entry.job_id.as_str())
            .with_context(|| format!("quarantined job missing from {body}"))?;
        assert_eq!(job["phase"], "quarantined", "{job}");
        assert_eq!(job["result"]["outcome"], "quarantined", "{job}");

        if with_doctor {
            let check = doctor_check(&h, "sync_apply_quarantine")?;
            assert_eq!(check["status"], "warning", "{check}");
            let detail = serde_json::to_string(&check["detail"])?;
            assert!(detail.contains(transaction.to_str().unwrap()), "{detail}");
            assert!(detail.contains("never restored"), "{detail}");
        }

        // Nothing blocks the project: a new apply succeeds and leaves the
        // quarantined directory untouched.
        let report = apply_ok(&h, &project, &b, Some(&a)).await?;
        assert_eq!(report["schedules"]["updated"], 1, "{report}");
        assert_generation(&h, &project, Generation::B, &b).await?;
        assert_eq!(directory_bytes(&transaction)?, kept);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn crash_after_journal_publication_restores_the_previous_generation() -> Result<()> {
        crash_at("journal_published", Recovered::Restored).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn crash_after_schedule_preparation_restores_the_previous_generation() -> Result<()> {
        crash_at("schedules_prepared", Recovered::Restored).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn crash_after_ref_publication_completes_the_target_generation() -> Result<()> {
        crash_at("ref_published", Recovered::Completed).await
    }
}
