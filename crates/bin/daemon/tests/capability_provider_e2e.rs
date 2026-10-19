//! Capability floor entries that name accepted grades and providers, judged
//! by a real `ryeosd` against what its host proves.
//!
//! The daemon here is a direct child of the test: no host service or
//! external controller delegates a cgroup subtree, so no per-scope name can
//! be met and every scope-constrained floor is proven from its refusal
//! side. The admitted side of those floors needs a delegating host (runit,
//! or the OCI-hook image) and is proven there. What this harness does prove
//! is the data path itself: grades and providers written in the signed
//! policy decide admission, every proof carries its provider, every
//! admission records which proof met each entry, and the refusals are the
//! exact texts the node writes.

mod common;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use lillux::process_control::ProofSource;
use ryeos_node::capability_report::{FloorEntryMet, SignedNodeCapabilityReport};
use serde_json::{Value, json};

use common::fast_fixture::{
    FAST_FIXTURE_TIME, FastFixtureProfile, populate_initialized_state_with_profile,
    register_core_bundle_at_state, seal_initialized_state, write_harness_bootstrap_config,
};

const ISOLATION_POLICY: &str = ".ai/node/policies/isolation.yaml";
const RESTART_INHIBITED: &str = ".ai/state/restart-inhibited.json";
const READY_LINE: &str = "daemon startup ready";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(180);

const FLOOR_REFUSED: &str =
    "startup refused before native reservation: this host cannot prove what node policy requires";
const NO_DELEGATED_SUBTREE: &str =
    "no host service or external controller delegated a writable cgroup subtree to this node";
const NO_SCOPES_CONFIGURED: &str =
    "node policy configures no process scopes, so no per-job cgroup is opened";
const PROCESS_GROUP_TRUSTED_ONLY: &str = "termination:process_group is only trusted-proven here \
     by host; the floor accepts measured (process group plus pidfd; a job that calls setsid or \
     double-forks escapes it)";

/// A disposable Core node whose signed isolation policy was rewritten before
/// the seal, running as a direct child of this test.
struct Launch {
    _node_dir: tempfile::TempDir,
    _user_dir: tempfile::TempDir,
    _socket_dir: tempfile::TempDir,
    app_root: PathBuf,
    home: PathBuf,
    stderr_path: PathBuf,
    child: Child,
}

impl Drop for Launch {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Launch {
    /// Prepare the Core fixture, let `edit` rewrite the local class of the
    /// isolation policy, re-sign it with the fixture node key, seal, and
    /// spawn.
    fn spawn(edit: impl FnOnce(&mut Value)) -> Result<Self> {
        // The fixture node runs under the profile's local class.
        Self::spawn_with_policy_document(|document| {
            edit(
                document
                    .pointer_mut("/policy/host_classes/local")
                    .context("fixture isolation policy has no local class")?,
            );
            Ok(())
        })
    }

    /// Prepare the Core fixture, let `edit` rewrite the whole signed
    /// isolation member (its `policy` document holds the document-level
    /// `version` and the `host_classes` map), re-sign it with the fixture
    /// node key, seal, and spawn.
    fn spawn_with_policy_document(edit: impl FnOnce(&mut Value) -> Result<()>) -> Result<Self> {
        let (node_dir, app_root) = common::copy_core_to_temp();
        let user_dir = tempfile::tempdir()?;
        let socket_dir = tempfile::tempdir()?;
        let fixture = populate_initialized_state_with_profile(
            &app_root,
            user_dir.path(),
            FastFixtureProfile::Core,
        )?;
        register_core_bundle_at_state(&app_root, &fixture)?;
        let policy_path = app_root.join(ISOLATION_POLICY);
        let raw = std::fs::read_to_string(&policy_path)?;
        let mut document: Value =
            serde_yaml::from_str(&lillux::signature::strip_signature_lines(&raw))?;
        edit(&mut document)?;
        std::fs::write(
            &policy_path,
            lillux::signature::sign_content_at(
                &serde_yaml::to_string(&document)?,
                &fixture.node,
                "#",
                None,
                FAST_FIXTURE_TIME,
            ),
        )?;
        seal_initialized_state(&app_root)?;
        let uds_path = socket_dir.path().join("ryeosd.sock");
        write_harness_bootstrap_config(&app_root, "127.0.0.1:0".parse()?, &uds_path)?;
        let stderr_path = socket_dir.path().join("ryeosd.stderr.log");
        let child = Command::new(common::ryeosd_binary())
            .arg("--app-root")
            .arg(&app_root)
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--uds-path")
            .arg(&uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &app_root)
            .env("HOME", user_dir.path())
            .env("RUST_MIN_STACK", "4194304")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr_path)?)
            .spawn()?;
        Ok(Self {
            home: user_dir.path().to_path_buf(),
            _node_dir: node_dir,
            _user_dir: user_dir,
            _socket_dir: socket_dir,
            app_root,
            stderr_path,
            child,
        })
    }

    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    /// Wait for the daemon to exit on its own; the stderr it wrote.
    fn wait_exit(&mut self) -> Result<(std::process::ExitStatus, String)> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok((status, self.stderr()));
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "daemon did not exit:\n{}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Wait for the readiness line; the daemon keeps running.
    fn wait_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if self.stderr().contains(READY_LINE) {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!(
                    "daemon exited with {status} before ready:\n{}",
                    self.stderr()
                );
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "daemon never ready:\n{}",
                self.stderr()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn report(&self) -> Result<SignedNodeCapabilityReport> {
        SignedNodeCapabilityReport::read_verified(&self.app_root)?
            .context("the daemon recorded no capability report")
    }

    fn restart_inhibition(&self) -> Option<Value> {
        let raw = std::fs::read(self.app_root.join(RESTART_INHIBITED)).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    fn doctor(&self, json: bool) -> Result<String> {
        let mut command = Command::new(common::ryeos_binary());
        command
            .args(["node", "doctor", "--no-bundles", "--app-root"])
            .arg(&self.app_root);
        if json {
            command.arg("--json");
        }
        let output = command
            .env("RYEOS_APP_ROOT", &self.app_root)
            .env("RYEOSD_BIN", common::ryeosd_binary())
            .env("HOME", &self.home)
            .env("NO_COLOR", "1")
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "ryeos node doctor exit={:?}\nstderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }
}

/// A refused start records the floor refusal as the restart inhibition and
/// returns its exact text.
fn floor_refusal(launch: &mut Launch) -> Result<String> {
    let (status, stderr) = launch.wait_exit()?;
    assert!(!status.success(), "start admitted:\n{stderr}");
    assert!(
        stderr.contains(FLOOR_REFUSED),
        "no floor refusal in stderr:\n{stderr}"
    );
    let inhibition = launch
        .restart_inhibition()
        .context("no restart inhibition recorded")?;
    assert_eq!(
        inhibition["reason"], "capability_floor_refused",
        "{inhibition}"
    );
    let detail = inhibition["detail"]
        .as_str()
        .context("inhibition without detail")?
        .to_owned();
    assert!(detail.starts_with(FLOOR_REFUSED), "{detail}");
    assert!(
        !launch.report()?.report.floor.admitted,
        "report records the floor as admitted"
    );
    Ok(detail)
}

fn scoped_floor(policy: &mut Value, termination_entry: Value) {
    policy["process_scopes"] = json!({ "mode": "required", "control_timeout_ms": 5000 });
    policy["requires_capabilities"] = json!([
        "accounting",
        "recovery:scope",
        "storage:durable_root",
        termination_entry,
        "time",
    ]);
}

fn process_group_floor(policy: &mut Value, process_group_entry: Value) {
    policy["requires_capabilities"] = json!([
        "accounting",
        "storage:durable_root",
        process_group_entry,
        "time"
    ]);
}

#[test]
fn scope_floor_constrained_to_the_oci_provider_is_refused_here_and_inhibits_restart() -> Result<()>
{
    let mut launch = Launch::spawn(|policy| {
        scoped_floor(
            policy,
            json!({ "name": "termination:scope", "providers": ["external_controller:oci"] }),
        );
    })?;
    let detail = floor_refusal(&mut launch)?;
    // Without a delegating provider there is no proof to judge by provider:
    // the refusal names the absent delegation, for both scope names.
    assert!(
        detail.contains(&format!(
            "\n  requires recovery:scope: {NO_DELEGATED_SUBTREE}"
        )),
        "{detail}"
    );
    assert!(
        detail.contains(&format!(
            "\n  requires termination:scope: {NO_DELEGATED_SUBTREE}"
        )),
        "{detail}"
    );
    let floor = launch.report()?.report.floor;
    assert_eq!(floor.unmet["termination:scope"], NO_DELEGATED_SUBTREE);
    assert_eq!(floor.unmet["recovery:scope"], NO_DELEGATED_SUBTREE);
    assert!(
        !floor.met_by.contains_key("termination:scope"),
        "{:?}",
        floor.met_by
    );
    assert_eq!(floor.requires[3].providers(), ["external_controller:oci"]);
    Ok(())
}

#[test]
fn trusted_grade_name_is_refused_unless_the_floor_accepts_trusted() -> Result<()> {
    let mut bare =
        Launch::spawn(|policy| process_group_floor(policy, json!("termination:process_group")))?;
    let detail = floor_refusal(&mut bare)?;
    assert!(
        detail.contains(&format!(
            "\n  requires termination:process_group: {PROCESS_GROUP_TRUSTED_ONLY}"
        )),
        "{detail}"
    );
    assert_eq!(
        bare.report()?.report.floor.unmet["termination:process_group"],
        PROCESS_GROUP_TRUSTED_ONLY
    );
    drop(bare);

    let mut accepting = Launch::spawn(|policy| {
        process_group_floor(
            policy,
            json!({ "name": "termination:process_group", "accept": ["trusted"] }),
        );
    })?;
    accepting.wait_ready()?;
    let floor = accepting.report()?.report.floor;
    assert!(floor.admitted, "{:?}", floor.unmet);
    assert_eq!(
        floor.met_by["termination:process_group"],
        FloorEntryMet {
            name: "termination:process_group".to_owned(),
            source: ProofSource::Trusted,
            provider: "host".to_owned(),
        }
    );
    assert!(accepting.restart_inhibition().is_none());
    Ok(())
}

#[test]
fn every_proof_names_its_provider_and_unnamed_scopes_say_why_they_are_missing() -> Result<()> {
    let mut launch = Launch::spawn(|_| {})?;
    launch.wait_ready()?;
    let report = launch.report()?.report;
    assert!(report.floor.admitted, "{:?}", report.floor.unmet);
    for (name, evidence) in &report.provided {
        assert!(
            !evidence.provider.is_empty(),
            "{name} proven without a provider"
        );
        assert!(!evidence.basis.is_empty(), "{name} proven without a basis");
    }
    for (label, met) in &report.floor.met_by {
        assert_eq!(met.provider, "host", "{label} met by {met:?}");
        assert!(
            report.provided.contains_key(&met.name),
            "{label} met by an unproven name"
        );
    }
    assert_eq!(report.floor.met_by.len(), report.floor.requires.len());
    // No provider delegated a subtree and the fixture configures no scopes,
    // so every scope name, quiescence included, is missing for that reason
    // rather than probed. The "not probed" basis for an unnamed quiescence
    // appears only where a provider delegates a subtree.
    for name in [
        "limits:scope",
        "quiescence:scope",
        "recovery:scope",
        "termination:scope",
    ] {
        assert_eq!(report.missing[name], NO_SCOPES_CONFIGURED, "{name}");
        assert!(
            !report.provided.contains_key(name),
            "{name} proven without a provider delegation"
        );
    }
    Ok(())
}

#[test]
fn doctor_shows_the_provider_of_every_proof_and_of_every_met_entry() -> Result<()> {
    let mut launch = Launch::spawn(|_| {})?;
    launch.wait_ready()?;
    let doctor: Value = serde_json::from_str(&launch.doctor(true)?)?;
    let capabilities = doctor["checks"]
        .as_array()
        .context("doctor without checks")?
        .iter()
        .find(|check| check["name"] == "capabilities")
        .context("doctor without a capabilities check")?;
    assert_ne!(capabilities["status"], "fail", "{capabilities}");
    let detail = &capabilities["detail"];
    let provided = detail["provided"]
        .as_object()
        .context("doctor without provided")?;
    assert!(!provided.is_empty());
    for (name, evidence) in provided {
        assert!(
            evidence["provider"]
                .as_str()
                .is_some_and(|provider| !provider.is_empty()),
            "{name}: {evidence}"
        );
        assert!(evidence["source"].as_str().is_some(), "{name}: {evidence}");
    }
    let met_by = detail["floor"]["met_by"]
        .as_object()
        .context("doctor without met_by")?;
    assert!(!met_by.is_empty());
    for (label, met) in met_by {
        assert_eq!(met["provider"], "host", "{label}: {met}");
    }
    assert_eq!(detail["floor"]["admitted"], true);

    let text = launch.doctor(false)?;
    assert!(text.contains("proves time (measured by host): "), "{text}");
    assert!(
        text.contains("met time by time from host (measured)"),
        "{text}"
    );
    Ok(())
}

#[test]
fn isolation_policy_version_9_is_refused_as_foreign() -> Result<()> {
    // The version belongs to the policy document that holds the host
    // classes, not to any one class. A foreign document version is refused
    // when the initialized state is sealed, before any daemon can start on
    // it; the refusal is the policy section's own text, and no restart
    // inhibition is written because no start was attempted.
    let error = match Launch::spawn_with_policy_document(|document| {
        let policy = document
            .get_mut("policy")
            .and_then(Value::as_object_mut)
            .context("fixture isolation member has no policy document")?;
        anyhow::ensure!(
            policy.contains_key("host_classes"),
            "fixture isolation policy document has no host_classes map"
        );
        anyhow::ensure!(
            policy.get("version") == Some(&json!(10)),
            "fixture isolation policy document is not version 10: {:?}",
            policy.get("version")
        );
        policy.insert("version".to_owned(), json!(9));
        Ok(())
    }) {
        Ok(mut launch) => {
            let (status, stderr) = launch.wait_exit()?;
            anyhow::bail!("a version 9 policy was sealed and started: {status}\n{stderr}");
        }
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("unsupported node isolation policy version 9 (expected 10)"),
        "{error}"
    );
    Ok(())
}

#[test]
fn nested_sandbox_key_is_refused() -> Result<()> {
    // The removed key is refused by the policy's own shape at the sealed
    // init, before any daemon can start; only the refused key is pinned
    // because the loader's wrapping text may differ.
    let error = match Launch::spawn(|policy| {
        policy["process_scopes"] =
            json!({ "mode": "required", "control_timeout_ms": 5000, "nested_sandbox": true });
    }) {
        Ok(mut launch) => {
            let (status, stderr) = launch.wait_exit()?;
            anyhow::bail!(
                "a policy with nested_sandbox was sealed and started: {status}\n{stderr}"
            );
        }
        Err(error) => format!("{error:#}"),
    };
    assert!(error.contains("unknown field `nested_sandbox`"), "{error}");
    Ok(())
}
