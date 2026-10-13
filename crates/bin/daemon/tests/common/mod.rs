//! Shared helpers for real end-to-end ryeosd tests.
//!
//! These helpers spawn the actual `ryeosd` binary as a child process,
//! configure trust + system bundles in a tempdir, and provide an
//! HTTP client to talk to the daemon over TCP.
//!
//! Used by `cleanup_e2e.rs`. NOT used by `cleanup_invariants.rs`
//! (those are pure in-process invariant checks).
//!
//! ## Running the daemon suite
//!
//! The harness initializes nodes from the authored `core`,
//! `core-scheduling`, `standard` and `full` init profiles. Every profile
//! except `core` installs the scheduling bundle (`service:scheduler/*`) and
//! all but `core-scheduling` also install the standard bundle
//! (`service:model-providers/*`). A daemon refuses to start when an installed
//! operational service has no compiled handler, so the suite runs with the
//! standard composition CI uses for these bundle sets. The handoff crash
//! recovery tests are compiled only with `handoff-test-support`:
//!
//! ```text
//! cargo build -p ryeos-cli --features scheduling,model-provider-setup
//! cargo test -p ryeosd --features scheduling,model-provider-setup,handoff-test-support --tests -- --test-threads=1
//! ```
//!
//! A default-feature build refuses before spawning a daemon whose selected
//! profile needs a feature the build lacks (see [`require_compiled_composition`]).
//!
//! Prerequisites:
//!
//! * the selected bundles are freshly published; editing non-test crate
//!   sources or a `Cargo.toml` makes them stale and [`require_bundles_prepared`]
//!   refuses;
//! * the `ryeos` CLI is built beside `ryeosd` (`target/debug/ryeos`) for the
//!   CLI-driven tests;
//! * the temporary directory is on a durable filesystem, because every
//!   authored profile except `hosted-node` requires `storage:durable_root`
//!   (point `TMPDIR` at a disk-backed directory when `/tmp` is a tmpfs).
//!
//! A directly spawned daemon has no protected host-runtime association, so
//! no delegated cgroup subtree exists and no per-scope capability can
//! qualify. The fast fixture therefore signs an isolation member derived
//! from the authored profile without its per-scope contract (see
//! `fast_fixture::direct_host_isolation`). Per-scope containment is
//! qualified only against an administrator-prepared host-backed node.

#![allow(dead_code)]

#[cfg(all(unix, feature = "crash-qualification-test-support"))]
pub mod dedicated_command_cut;
pub mod fast_fixture;
pub mod mock_provider;
#[cfg(all(unix, feature = "crash-qualification-test-support"))]
pub mod runtime_phase_cut;

#[cfg(all(unix, feature = "handoff-test-support"))]
use std::io::Read as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use base64::Engine;
use lillux::crypto::{Signer as _, SigningKey};
use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

const DEFAULT_DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const DAEMON_STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Parent-controlled, one-shot pause after the scoped attempt is durable but
/// before the START request can register a live child. Unlike a crash gate,
/// this permits a concurrent exact-owner RESUME to exercise the wait race.
#[cfg(all(unix, feature = "handoff-test-support"))]
pub struct ScopedReservedAttemptGate {
    channel: lillux::InheritedDuplexChannel,
}

#[cfg(all(unix, feature = "handoff-test-support"))]
impl ScopedReservedAttemptGate {
    pub fn pair() -> anyhow::Result<(Self, lillux::InheritedDuplexChannelChildAuthority)> {
        let (channel, child) = lillux::inherited_duplex_channel_pair()
            .map_err(anyhow::Error::msg)
            .context("create scoped reserved-attempt gate channel")?;
        Ok((Self { channel }, child))
    }

    pub fn attach(
        command: &mut Command,
        child: lillux::InheritedDuplexChannelChildAuthority,
    ) -> anyhow::Result<()> {
        command.arg("--scoped-reserved-attempt-gate");
        child
            .bind_to_command(
                command.as_std_mut(),
                ryeos_app::scoped_producer_start::test_support::RESERVED_ATTEMPT_GATE_FD_ENV,
            )
            .map_err(anyhow::Error::msg)
            .context("bind scoped reserved-attempt gate to daemon")
    }

    pub async fn wait_reached(&mut self) -> anyhow::Result<serde_json::Value> {
        let mut channel = self.channel.try_clone()?;
        let record = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
            let deadline = lillux::time::MonotonicDeadline::after(Duration::from_secs(180));
            let mut bounded = channel.with_deadline(deadline);
            let mut record = Vec::with_capacity(256);
            for _ in 0..=512 {
                let mut byte = [0u8; 1];
                bounded.read_exact(&mut byte)?;
                if byte[0] == b'\n' {
                    return Ok(record);
                }
                record.push(byte[0]);
            }
            anyhow::bail!("scoped reserved-attempt gate evidence exceeds 512 bytes")
        })
        .await
        .context("join scoped reserved-attempt gate reader")??;
        let evidence: serde_json::Value = serde_json::from_slice(&record)?;
        anyhow::ensure!(
            evidence["schema"] == "ryeos.scoped_reserved_attempt_gate.v1",
            "scoped reserved-attempt gate returned wrong schema"
        );
        Ok(evidence)
    }

    pub fn release(&mut self) -> anyhow::Result<()> {
        use std::io::Write as _;

        let deadline = lillux::time::MonotonicDeadline::after(Duration::from_secs(5));
        self.channel.with_deadline(deadline).write_all(b"R")?;
        Ok(())
    }
}

/// Parent end of the feature-only handoff crash gate.
///
/// The daemon writes closed evidence naming the exact durable boundary and
/// live operation identity to an inherited Lillux channel, then parks the
/// request thread. Once this reader validates that record, the test can
/// SIGKILL the whole daemon with no Rust unwinding or guard cleanup. That makes
/// the restart evidence equivalent to a process crash, not an in-process
/// injected error or a post-restart reconstruction.
#[cfg(all(unix, feature = "handoff-test-support"))]
pub struct HandoffCrashGate {
    expected: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    reader: Option<lillux::InheritedDuplexChannel>,
}

#[cfg(all(unix, feature = "handoff-test-support"))]
impl HandoffCrashGate {
    fn pair(
        expected: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    ) -> anyhow::Result<(Self, lillux::InheritedDuplexChannelChildAuthority)> {
        let (reader, writer) = lillux::inherited_duplex_channel_pair()
            .map_err(anyhow::Error::msg)
            .context("create handoff crash gate channel")?;
        Ok((
            Self {
                expected,
                reader: Some(reader),
            },
            writer,
        ))
    }

    fn attach_writer(
        command: &mut Command,
        writer: lillux::InheritedDuplexChannelChildAuthority,
        boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    ) -> anyhow::Result<()> {
        command
            .arg("--handoff-phase-cut-boundary")
            .arg(boundary.as_str());
        writer
            .bind_to_command(
                command.as_std_mut(),
                ryeos_app::worker_handoff::test_support::HANDOFF_PHASE_CUT_FD_ENV,
            )
            .map_err(anyhow::Error::msg)
            .context("bind handoff crash gate channel to daemon")
    }

    /// Wait until the child has durably crossed the configured boundary.
    pub async fn wait_reached(
        &mut self,
    ) -> anyhow::Result<ryeos_app::worker_handoff::test_support::HandoffPhaseCutEvidence> {
        let mut reader = self
            .reader
            .take()
            .context("handoff crash gate was already consumed")?;
        let observed = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
                let mut record = Vec::with_capacity(513);
                for _ in 0..=512 {
                    let mut byte = [0u8; 1];
                    reader
                        .read_exact(&mut byte)
                        .context("read handoff phase-cut evidence")?;
                    if byte[0] == b'\n' {
                        return Ok(record);
                    }
                    record.push(byte[0]);
                }
                anyhow::bail!("handoff phase-cut evidence exceeds 512 bytes")
            }),
        )
        .await
        .context("timed out waiting for handoff crash boundary")?
        .context("join handoff crash gate reader")??;
        let evidence: ryeos_app::worker_handoff::test_support::HandoffPhaseCutEvidence =
            serde_json::from_slice(&observed).context("decode handoff phase-cut evidence")?;
        evidence.validate()?;
        anyhow::ensure!(
            evidence.boundary == self.expected,
            "handoff crash gate expected `{}` but observed `{}`",
            self.expected,
            evidence.boundary
        );
        Ok(evidence)
    }
}

fn daemon_startup_deadline() -> anyhow::Result<Instant> {
    let timeout = match std::env::var("RYEOSD_TEST_STARTUP_TIMEOUT_SECS") {
        Ok(raw) => {
            let seconds = raw.parse::<u64>().with_context(|| {
                format!("parse RYEOSD_TEST_STARTUP_TIMEOUT_SECS value `{raw}` as seconds")
            })?;
            anyhow::ensure!(
                seconds > 0,
                "RYEOSD_TEST_STARTUP_TIMEOUT_SECS must be greater than zero"
            );
            Duration::from_secs(seconds)
        }
        Err(std::env::VarError::NotPresent) => DEFAULT_DAEMON_STARTUP_TIMEOUT,
        Err(error) => {
            return Err(anyhow::anyhow!(
                "read RYEOSD_TEST_STARTUP_TIMEOUT_SECS: {error}"
            ));
        }
    };
    Ok(Instant::now() + timeout)
}

/// Path to the built `ryeosd` binary (set by Cargo for integration tests
/// in this crate).
pub fn ryeosd_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ryeosd"))
}

fn ryeosd_command() -> Command {
    let mut command = Command::new(ryeosd_binary());
    // Debug daemon builds cross the default 2 MiB Tokio worker stack while
    // constructing the fully verified inventory/launch closure used by these
    // real-process integration tests. Release installs are optimized below
    // that boundary. Keep the harness deterministic while still honoring a
    // caller-supplied larger stack.
    command.env(
        "RUST_MIN_STACK",
        std::env::var_os("RUST_MIN_STACK").unwrap_or_else(|| "4194304".into()),
    );
    command
}

/// Monotonic per-process counter used to give each spawned daemon a
/// unique stderr log file name without relying on a port number that
/// isn't known until after bind.
fn next_harness_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

/// Read the actual bound `SocketAddr` from a daemon.json file.
///
/// The daemon writes its real listen address (including any
/// kernel-assigned ephemeral port when the caller passed `:0`) into
/// `daemon.json` after binding. Test harnesses must call this AFTER
/// the daemon.json-existence wait so they connect to the correct port.
pub fn read_actual_bind(daemon_json_path: &Path) -> anyhow::Result<SocketAddr> {
    let body = std::fs::read_to_string(daemon_json_path)
        .with_context(|| format!("read daemon.json at {}", daemon_json_path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parse daemon.json at {}", daemon_json_path.display()))?;
    let s = v.get("bind").and_then(|x| x.as_str()).ok_or_else(|| {
        anyhow::anyhow!(
            "daemon.json at {} missing 'bind' field",
            daemon_json_path.display()
        )
    })?;
    s.parse()
        .with_context(|| format!("parse 'bind' value '{s}' from daemon.json"))
}

fn daemon_stderr_log_path(fallback_dir: &Path, harness_id: u64) -> PathBuf {
    let directory = std::env::var_os("RYEOSD_TEST_STDERR_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback_dir.to_path_buf());
    directory.join(format!(
        "daemon-{}-{harness_id}.stderr.log",
        std::process::id()
    ))
}

fn daemon_stderr_stdio(path: &Path) -> anyhow::Result<Stdio> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create daemon stderr directory {}", parent.display()))?;
    }
    let file = std::fs::File::create(path)
        .with_context(|| format!("create daemon stderr log {}", path.display()))?;
    Ok(Stdio::from(file))
}

async fn stop_and_collect_daemon_stderr(child: &mut Child, stderr_path: &Path) -> String {
    child.start_kill().ok();

    let mut stderr = String::new();
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        stderr.push_str("<daemon did not exit within 2s after kill>\n");
    }
    match std::fs::read_to_string(stderr_path) {
        Ok(log) => {
            if !stderr.is_empty() && !log.is_empty() {
                stderr.push_str("<daemon stderr log>\n");
            }
            stderr.push_str(&log);
        }
        Err(error) => stderr.push_str(&format!(
            "<failed to read daemon stderr log {}: {error}>\n",
            stderr_path.display()
        )),
    }
    stderr
}

async fn wait_for_daemon_ready(
    child: &mut Child,
    bind: SocketAddr,
    stderr_path: &Path,
    context: &str,
    deadline: Instant,
) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("http://{bind}/_ryeos/ready");

    loop {
        let detail = match client
            .get(&url)
            .timeout(Duration::from_millis(200))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) => {
                let status = response.status();
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|error| format!("<failed to read response body: {error}>"));
                if body.contains("node_startup_failed") {
                    let stderr = stop_and_collect_daemon_stderr(child, stderr_path).await;
                    anyhow::bail!(
                        "{context} reported a terminal startup failure at {url}: HTTP {status}: \
                         {body}\ndaemon stderr:\n{stderr}"
                    );
                }
                format!("HTTP {status}: {body}")
            }
            Err(error) => error.to_string(),
        };

        if Instant::now() > deadline {
            let stderr = stop_and_collect_daemon_stderr(child, stderr_path).await;
            anyhow::bail!(
                "{context} never became ready at {url}; last probe: {detail}\n\
                 daemon stderr:\n{stderr}"
            );
        }
        tokio::time::sleep(DAEMON_STARTUP_POLL_INTERVAL).await;
    }
}

async fn wait_for_daemon_discovery(
    child: &mut Child,
    daemon_json: &Path,
    stderr_path: &Path,
    context: &str,
    deadline: Instant,
) -> anyhow::Result<()> {
    loop {
        if daemon_json.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let stderr = stop_and_collect_daemon_stderr(child, stderr_path).await;
            anyhow::bail!(
                "{context} exited with {status} before publishing {} — daemon stderr:\n{stderr}",
                daemon_json.display()
            );
        }
        if Instant::now() > deadline {
            let stderr = stop_and_collect_daemon_stderr(child, stderr_path).await;
            anyhow::bail!(
                "{context} never published {} — daemon stderr:\n{stderr}",
                daemon_json.display()
            );
        }
        tokio::time::sleep(DAEMON_STARTUP_POLL_INTERVAL).await;
    }
}

/// Resolve the projection instance selected by the current recovery
/// generation. Tests must follow the same pointer as production rather than
/// assuming a mutable well-known SQLite filename.
pub fn selected_projection_path(app_root: &Path) -> anyhow::Result<PathBuf> {
    let runtime_state_dir = app_root.join(ryeos_engine::AI_DIR).join("state");
    let generation = ryeos_state::RecoveryStore::from_runtime_state_dir(&runtime_state_dir)?
        .read_generation()?
        .context("thread projection has no selected recovery generation")?;
    Ok(runtime_state_dir.join(generation.projection_file))
}

/// Path to the explicitly prepared `ryeos` CLI binary beside `ryeosd`.
///
/// The validation queue builds this artifact with its declared source, lock,
/// target and feature selection before allocating the integration test. A test
/// must not start another Cargo process or infer freshness from file presence.
pub fn ryeos_binary() -> PathBuf {
    let candidate = ryeosd_binary()
        .parent()
        .expect("ryeosd binary has parent dir")
        .join("ryeos");

    assert!(
        candidate.is_file(),
        "prepared ryeos CLI is unavailable at {}; explicitly build the selected CLI artifact in the validation queue before running this integration test",
        candidate.display()
    );
    candidate
}

/// The repo workspace root (parent of `crates/bin/daemon/`).
pub fn workspace_root() -> PathBuf {
    ryeos_engine::test_support::workspace_root()
}

/// Returns the workspace's core bundle directory (`bundles/core`).
///
/// **Read-only source for test copies** — do NOT pass this directly to
/// the daemon as `--app-root` (the daemon mutates that dir).
/// Use [`copy_core_to_temp`] for an isolated writable copy.
///
/// This IS safe to pass for `RYEOS_APP_ROOT` (read-only bundle
/// discovery) and as a source for `copy_dir_recursive`.
pub fn workspace_core_dir() -> PathBuf {
    workspace_root().join("bundles/core")
}

/// Create an isolated app root, install Core at its ordinary bundle path, and
/// return `(tempdir, app_root)`.
///
/// A bundle root is never also a node app root: doing so would put node-owned
/// state below the registered bundle and make that state appear to be
/// publisher-authored bundle content.
pub fn copy_core_to_temp() -> (TempDir, PathBuf) {
    require_bundles_prepared(&["core"])
        .expect("Core test bundle must be explicitly prepared before daemon tests");
    copy_core_to_temp_from_prepared()
}

fn copy_core_to_temp_prepared(bundle_names: &[&str]) -> anyhow::Result<(TempDir, PathBuf)> {
    require_compiled_composition(bundle_names)?;
    require_bundles_prepared(bundle_names)?;
    Ok(copy_core_to_temp_from_prepared())
}

fn copy_core_to_temp_from_prepared() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir for isolated app root");
    let app_root = tmp.path().join("app-root");
    let src = workspace_core_dir();
    let dst = app_root.join(".ai/bundles/core");
    copy_dir_recursive(&src, &dst).expect("copy core bundle to temp");
    (tmp, app_root)
}

/// Refuse before spawning a daemon whose selected bundles install operational
/// services this test build has no handler for. The daemon would refuse the
/// same composition at startup; failing here names the build command instead.
pub fn require_compiled_composition(bundle_names: &[&str]) -> anyhow::Result<()> {
    let needs = [
        ("scheduling", "scheduling", cfg!(feature = "scheduling")),
        ("standard", "model-provider-setup", cfg!(feature = "model-provider-setup")),
    ];
    let missing = needs
        .iter()
        .filter(|(bundle, _, compiled)| !compiled && bundle_names.contains(bundle))
        .map(|(bundle, feature, _)| format!("{bundle} bundle needs feature `{feature}`"))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        missing.is_empty(),
        "this ryeosd test build cannot start the selected composition ({}); run the daemon \
         suite with `cargo test -p ryeosd --features scheduling,model-provider-setup`",
        missing.join(", ")
    );
    Ok(())
}

/// Require prebuilt, published bundle inputs without rebuilding or signing.
/// Selected E2E controls use this gate so tests never invoke the development
/// publisher key implicitly.
pub fn require_bundles_prepared(bundle_names: &[&str]) -> anyhow::Result<()> {
    let root = workspace_root();
    for name in bundle_names {
        let manifest_ref = root
            .join("bundles")
            .join(name)
            .join(".ai/refs/bundles/manifest");
        let ref_metadata = std::fs::symlink_metadata(&manifest_ref)
            .map_err(|error| {
                anyhow::anyhow!(
                    "prepared {name} bundle manifest ref is unavailable at {}; prepare the selected bundle set before running this E2E: {error}",
                    manifest_ref.display()
                )
            })?;
        anyhow::ensure!(
            ref_metadata.file_type().is_file(),
            "prepared {name} bundle manifest ref must be a regular non-symlink file at {}",
            manifest_ref.display()
        );
        let published = ref_metadata.modified()?;
        let bundle_root = root.join("bundles").join(name);
        for relative in [".ai/manifest.source.yaml", ".ai/manifest.yaml"] {
            let path = bundle_root.join(relative);
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                anyhow::anyhow!(
                    "prepared {name} bundle is missing required file {}; prepare the selected bundle set: {error}",
                    path.display()
                )
            })?;
            anyhow::ensure!(
                metadata.file_type().is_file(),
                "prepared {name} bundle file must be a regular non-symlink file at {}",
                path.display()
            );
        }
        if *name == "core" {
            let bin_root = bundle_root.join(".ai/bin");
            let has_core_tools = std::fs::read_dir(&bin_root)
                .map_err(|error| {
                    anyhow::anyhow!(
                        "prepared Core tools are missing at {}; prepare the selected bundle set: {error}",
                        bin_root.display()
                    )
                })?
                .filter_map(Result::ok)
                .any(|entry| entry.path().join("ryeos-core-tools").is_file());
            anyhow::ensure!(
                has_core_tools,
                "prepared Core bundle has no ryeos-core-tools binary under {}; prepare the selected bundle set",
                bin_root.display()
            );
        }
        anyhow::ensure!(
            !bundle_inputs_newer_than(&root, published)
                && !bundle_tree_has_newer(&bundle_root, &bundle_root, published)?,
            "prepared {name} bundle artifacts are stale; prepare the selected bundle set before running this E2E"
        );
    }
    Ok(())
}

/// The publisher writes the bundle-root `PUBLISHER_TRUST.toml` after it
/// commits the manifest ref. The document names the signing key and does not
/// feed the published artifacts, so its later mtime is not staleness.
fn bundle_tree_has_newer(
    bundle_root: &Path,
    path: &Path,
    published: std::time::SystemTime,
) -> anyhow::Result<bool> {
    for entry in std::fs::read_dir(path)
        .map_err(|error| anyhow::anyhow!("inspect prepared bundle {}: {error}", path.display()))?
    {
        let entry = entry?;
        let entry_path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            anyhow::bail!(
                "prepared bundle contains a symlink at {}; refusing stale-artifact check",
                entry_path.display()
            );
        }
        if file_type.is_dir() {
            if bundle_tree_has_newer(bundle_root, &entry_path, published)? {
                return Ok(true);
            }
        } else if path == bundle_root && entry.file_name() == "PUBLISHER_TRUST.toml" {
            continue;
        } else if entry
            .metadata()?
            .modified()
            .is_ok_and(|modified| modified > published)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Bundle validation is read-only: test harnesses require prepared artifacts and never
/// rebuild, sign, or publish workspace bundles implicitly.

/// Walk the source roots that affect bundle binary contents and return
/// `true` if a workspace manifest or production crate source has an mtime
/// newer than `published`. Integration tests, benches, and examples cannot
/// affect release binaries and must not force a full bundle rebuild.
///
/// We do not check bundle item YAMLs: publishing intentionally preserves
/// unchanged signed source items, while rebuilding their manifest closure.
/// A user editing a bundle YAML directly without republishing is a
/// known-acceptable hole — that path is rare and republishing is one
/// command.
fn bundle_inputs_newer_than(root: &Path, published: std::time::SystemTime) -> bool {
    [root.join("Cargo.toml"), root.join("Cargo.lock")]
        .iter()
        .any(|path| file_is_newer(path, published))
        || dir_has_newer(&root.join("crates"), published)
}

fn dir_has_newer(path: &Path, published: std::time::SystemTime) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| matches!(name, "tests" | "benches" | "examples"))
            {
                continue;
            }
            if dir_has_newer(&p, published) {
                return true;
            }
        } else if (p.file_name().is_some_and(|name| name == "Cargo.toml")
            || p.file_name().is_some_and(|name| name == "build.rs")
            || p.extension().is_some_and(|ext| ext == "rs"))
            && file_is_newer(&p, published)
        {
            return true;
        }
    }
    false
}

fn file_is_newer(path: &Path, published: std::time::SystemTime) -> bool {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .is_ok_and(|modified| modified > published)
}

/// Recursive directory copy (Unix, no special handling).
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Path to the fixture trusted-signers TOML for the bundle signer.
fn fixture_trusted_signer_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/trusted_signers")
}

/// Configure a tempdir as an app root: pre-populate
/// `<app-root>/.ai/config/keys/trusted/` with the fixture trusted signers
/// so the core bundle's items verify under the daemon's trust store.
pub fn populate_trusted_keys(app_root: &Path) {
    let trusted_dst = app_root.join(".ai/config/keys/trusted");
    std::fs::create_dir_all(&trusted_dst).expect("create app-root trusted keys dir");
    for entry in
        std::fs::read_dir(fixture_trusted_signer_dir()).expect("read fixture trusted_signers")
    {
        let entry = entry.expect("trusted_signer entry");
        let name = entry.file_name();
        std::fs::copy(entry.path(), trusted_dst.join(&name)).expect("copy fixture trusted signer");
    }
}

/// A live ryeosd daemon child process bound to `bind`, with state under
/// `app_root`. Drop kills the child and best-effort cleans up the UDS.
pub struct DaemonHarness {
    /// Outer tempdir for UDS socket (RAII cleanup).
    _state_dir_outer: TempDir,
    /// Tempdir holding the copied core bundle. The daemon writes into this copy,
    /// not the workspace tree.
    _core_bundle_tmp: TempDir,
    /// Path the daemon was launched with as `--app-root`. Use this for
    /// reading `daemon.json`, audit files, etc.
    pub state_path: PathBuf,
    pub user_space: TempDir,
    pub bind: SocketAddr,
    pub uds_path: PathBuf,
    pub child: Child,
    /// Regular file continuously receiving daemon stderr. A file is used rather
    /// than an unread pipe so verbose test logging can never backpressure and
    /// stall the daemon under test.
    stderr_log_path: PathBuf,
    /// Operator signing key from the fast fixture. Used by `post_execute` to
    /// sign requests. `None` when the daemon was started via `start()`
    /// instead of `start_fast()`.
    pub user_key: Option<SigningKey>,
    /// Node identity key from the fast fixture. Used to compute the
    /// daemon's principal_id for audience binding.
    pub node_key: Option<SigningKey>,
}

#[derive(Clone, Copy)]
enum FastBundleSelection {
    PreparedCoreOnly,
    PreparedCoreScheduling,
    PreparedStandard,
    PreparedFull,
}

impl DaemonHarness {
    /// Retain the disposable node and its exact bundle/user inputs for diagnosis
    /// if an accepted operation fails or observation times out. The daemon is
    /// still killed on drop; this grants no permission to relaunch that work.
    /// A successful test may restore cleanup after waiting for daemon exit.
    pub fn retain_evidence_on_drop(&mut self, retain: bool) {
        self._state_dir_outer.disable_cleanup(retain);
        self._core_bundle_tmp.disable_cleanup(retain);
        self.user_space.disable_cleanup(retain);
    }

    /// Spawn a fresh daemon. Blocks until `daemon.json` appears (or times out).
    pub async fn start() -> anyhow::Result<Self> {
        Self::start_with(|_cmd| {}).await
    }

    /// Spawn a fresh daemon, allowing the caller to mutate the `Command`
    /// (e.g. add extra env vars or args) before spawn.
    pub async fn start_with<F: FnOnce(&mut Command)>(tweak: F) -> anyhow::Result<Self> {
        Self::start_with_pre_init(|_, _| Ok(()), tweak).await
    }

    /// Spawn a fresh daemon, with a pre-init hook that runs **after**
    /// the state and user-space tempdirs are chosen but **before** the
    /// daemon process is spawned. The hook receives
    /// `(state_path, user_space)` and may write files into either tree
    /// (e.g. signed bundle registrations, audit records) so that the
    /// daemon's Phase 1 bootstrap and engine init pick them up.
    ///
    /// Populates the user-space identity/trust artifacts via the fast
    /// fixture before invoking `pre_init`. The daemon refuses to start
    /// when those artifacts are missing — `ryeos init` is the
    /// operator-side path that owns them — so the harness pre-creates
    /// them here so individual tests can focus on whatever they want to
    /// exercise.
    pub async fn start_with_pre_init<S, F>(pre_init: S, tweak: F) -> anyhow::Result<Self>
    where
        S: FnOnce(&Path, &Path) -> anyhow::Result<()>,
        F: FnOnce(&mut Command),
    {
        let state_dir_outer = tempfile::tempdir()?;
        let user_space = tempfile::tempdir()?;

        // Copy core bundle to an isolated temp dir so the daemon writes
        // state (identity, vault, DB, daemon.json) into the copy, not
        // the workspace tree.
        let (core_bundle_tmp, app_root) = copy_core_to_temp();

        // Plant app-root operator identity + trust docs and daemon-local node
        // key / vault / public identity so the daemon's startup
        // `repair_daemon_local` invariants pass. The fixture is
        // intentionally pre-applied rather than relying on (now-gone)
        // daemon auto-init for operator artifacts.
        let fixture = fast_fixture::populate_initialized_state_with_profile(
            &app_root,
            user_space.path(),
            fast_fixture::FastFixtureProfile::Core,
        )?;
        fast_fixture::register_core_bundle_at_state(&app_root, &fixture)?;

        pre_init(&app_root, user_space.path())?;
        fast_fixture::seal_initialized_state(&app_root)?;

        // Bind `:0` and let the kernel assign an ephemeral port. The
        // daemon writes the real address back to daemon.json — no
        // port-pick TOCTOU race across concurrent test binaries.
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let harness_id = next_harness_id();
        // UDS socket in a temp dir (avoids writing socket into workspace tree)
        let uds_path = state_dir_outer.path().join("ryeosd.sock");
        fast_fixture::write_harness_bootstrap_config(&app_root, bind, &uds_path)?;
        let stderr_log_path = daemon_stderr_log_path(state_dir_outer.path(), harness_id);

        let mut cmd = ryeosd_command();
        cmd.arg("--app-root")
            .arg(&app_root)
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--uds-path")
            .arg(&uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &app_root)
            .env("HOME", user_space.path())
            .stdout(Stdio::null())
            .stderr(daemon_stderr_stdio(&stderr_log_path)?)
            .kill_on_drop(true);

        tweak(&mut cmd);

        let startup_deadline = daemon_startup_deadline()?;
        let mut child = cmd.spawn()?;

        let daemon_json = app_root.join("daemon.json");
        wait_for_daemon_discovery(
            &mut child,
            &daemon_json,
            &stderr_log_path,
            "daemon",
            startup_deadline,
        )
        .await?;

        // Read the actual bound address — required when we passed :0.
        let actual_bind = read_actual_bind(&daemon_json)?;

        wait_for_daemon_ready(
            &mut child,
            actual_bind,
            &stderr_log_path,
            "daemon",
            startup_deadline,
        )
        .await?;

        Ok(Self {
            _state_dir_outer: state_dir_outer,
            _core_bundle_tmp: core_bundle_tmp,
            state_path: app_root.to_path_buf(),
            user_space,
            bind: actual_bind,
            uds_path,
            child,
            stderr_log_path,
            user_key: None,
            node_key: None,
        })
    }

    /// Spawn a fresh daemon using the fast fixture: state and user-space
    /// are pre-populated with deterministic keys, vault keypair, and
    /// self-signed trust docs (mirrors `bootstrap::init` byte-equivalent
    /// state). Daemon launches WITHOUT `` since
    /// initialization is already complete — any drift surfaces as a
    /// loud failure rather than silent re-init.
    ///
    /// Returns the harness paired with the deterministic
    /// [`fast_fixture::FastFixture`] keys so callers can sign their own
    /// items (directives, routes, providers, …) with
    /// `fixture.publisher`.
    pub async fn start_fast() -> anyhow::Result<(Self, fast_fixture::FastFixture)> {
        Self::start_fast_with_optional_identities(
            None,
            None,
            FastBundleSelection::PreparedStandard,
            |state_path, _user_space, fixture| {
                fast_fixture::register_standard_composition(state_path, fixture)
            },
            |_| {},
        )
        .await
    }

    /// Start an authenticated Core+Scheduling daemon from already prepared
    /// bundle artifacts. This path never rebuilds or signs workspace bundles.
    pub async fn start_fast_core_scheduling() -> anyhow::Result<(Self, fast_fixture::FastFixture)> {
        Self::start_fast_with_optional_identities(
            None,
            None,
            FastBundleSelection::PreparedCoreScheduling,
            |state_path, _user_space, fixture| {
                fast_fixture::register_scheduling_bundle(state_path, fixture)
            },
            |_| {},
        )
        .await
    }

    /// Start an authenticated Core-only daemon from already prepared bundle
    /// artifacts. This path never rebuilds or signs workspace bundles.
    pub async fn start_fast_core_only_prepared() -> anyhow::Result<(Self, fast_fixture::FastFixture)>
    {
        Self::start_fast_with_optional_identities(
            None,
            None,
            FastBundleSelection::PreparedCoreOnly,
            |_state_path, _user_space, _fixture| Ok(()),
            |_| {},
        )
        .await
    }

    /// Like [`start_fast`] but with two hooks:
    ///
    /// * `plant`: runs after `populate_initialized_state` and receives
    ///   `(state_path, user_space, &FastFixture)` — sign and place
    ///   bundle/directive/route content with `fixture.publisher`.
    /// * `tweak`: mutates the `Command` (env, args, …) before spawn.
    ///
    /// Note: only the core bundle is registered. Call
    /// `register_standard_bundle(state_path, fixture)` from your `plant`
    /// closure if the test needs standard bundle services/runtimes.
    pub async fn start_fast_with<S, F>(
        plant: S,
        tweak: F,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture)>
    where
        S: FnOnce(&Path, &Path, &fast_fixture::FastFixture) -> anyhow::Result<()>,
        F: FnOnce(&mut Command),
    {
        Self::start_fast_with_optional_identities(
            None,
            None,
            FastBundleSelection::PreparedFull,
            plant,
            tweak,
        )
        .await
    }

    /// Like [`start_fast_with`], but initializes the daemon with an explicit
    /// node identity. This is required by cross-site tests: two daemons using
    /// the ordinary deterministic fixture would authenticate as one node.
    pub async fn start_fast_with_node_key<S, F>(
        node_key: SigningKey,
        plant: S,
        tweak: F,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture)>
    where
        S: FnOnce(&Path, &Path, &fast_fixture::FastFixture) -> anyhow::Result<()>,
        F: FnOnce(&mut Command),
    {
        Self::start_fast_with_optional_identities(
            Some(node_key),
            None,
            FastBundleSelection::PreparedFull,
            plant,
            tweak,
        )
        .await
    }

    /// Cross-site fixture with distinct node and operator identities. This is
    /// the faithful shape for forwarding tests: the source operator must be an
    /// origin-bound remote authority on the target, never its local operator.
    pub async fn start_fast_with_node_and_user_keys<S, F>(
        node_key: SigningKey,
        user_key: SigningKey,
        plant: S,
        tweak: F,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture)>
    where
        S: FnOnce(&Path, &Path, &fast_fixture::FastFixture) -> anyhow::Result<()>,
        F: FnOnce(&mut Command),
    {
        Self::start_fast_with_optional_identities(
            Some(node_key),
            Some(user_key),
            FastBundleSelection::PreparedFull,
            plant,
            tweak,
        )
        .await
    }

    async fn start_fast_with_optional_identities<S, F>(
        node_key: Option<SigningKey>,
        user_key: Option<SigningKey>,
        bundle_selection: FastBundleSelection,
        plant: S,
        tweak: F,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture)>
    where
        S: FnOnce(&Path, &Path, &fast_fixture::FastFixture) -> anyhow::Result<()>,
        F: FnOnce(&mut Command),
    {
        let state_dir_outer = tempfile::tempdir()?;
        let user_space = tempfile::tempdir()?;

        // Copy core bundle to temp so fast fixture writes don't pollute workspace.
        let (core_bundle_tmp, state_path) = match bundle_selection {
            FastBundleSelection::PreparedCoreOnly => copy_core_to_temp_prepared(&["core"])?,
            FastBundleSelection::PreparedCoreScheduling => {
                copy_core_to_temp_prepared(&["core", "scheduling"])?
            }
            FastBundleSelection::PreparedStandard => {
                copy_core_to_temp_prepared(&["core", "scheduling", "standard"])?
            }
            FastBundleSelection::PreparedFull => copy_core_to_temp_prepared(
                fast_fixture::FastFixtureProfile::Full
                    .exact_bundles()
                    .expect("Full profile has an exact bundle inventory"),
            )?,
        };

        let selected_profile = match bundle_selection {
            FastBundleSelection::PreparedCoreOnly => fast_fixture::FastFixtureProfile::Core,
            FastBundleSelection::PreparedCoreScheduling => {
                fast_fixture::FastFixtureProfile::CoreScheduling
            }
            FastBundleSelection::PreparedStandard => fast_fixture::FastFixtureProfile::Standard,
            FastBundleSelection::PreparedFull => fast_fixture::FastFixtureProfile::Full,
        };
        let fixture = match (node_key, user_key) {
            (Some(node_key), Some(user_key)) => {
                fast_fixture::populate_initialized_state_with_profile_and_identities(
                    &state_path,
                    user_space.path(),
                    node_key,
                    user_key,
                    selected_profile,
                )?
            }
            (Some(node_key), None) => {
                fast_fixture::populate_initialized_state_with_profile_and_identities(
                    &state_path,
                    user_space.path(),
                    node_key,
                    fast_fixture::user_signing_key(),
                    selected_profile,
                )?
            }
            (None, None) => fast_fixture::populate_initialized_state_with_profile_and_identities(
                &state_path,
                user_space.path(),
                fast_fixture::node_signing_key(),
                fast_fixture::user_signing_key(),
                selected_profile,
            )?,
            (None, Some(_)) => unreachable!("explicit operator identity requires a node identity"),
        };
        // The harness copies `bundles/core` to `state_path`. Register it
        // so `bootstrap::verify_initialized` sees at least one bundle.
        fast_fixture::register_core_bundle_at_state(&state_path, &fixture)?;
        if matches!(bundle_selection, FastBundleSelection::PreparedFull) {
            fast_fixture::register_full_composition(&state_path, &fixture)?;
        }
        plant(&state_path, user_space.path(), &fixture)?;
        fast_fixture::validate_profile_bundle_records(
            &state_path,
            &fixture,
            selected_profile,
        )?;
        fast_fixture::seal_initialized_state(&state_path)?;

        // Authorize the user key (wildcard scope) so `post_execute` can sign
        // requests — unless the `plant` closure already wrote an authorized
        // key for it (e.g. a capability-restricted key for a cap-rejection
        // test), in which case we must not clobber it.
        let user_fp = lillux::signature::compute_fingerprint(&fixture.user.verifying_key());
        let user_key_path = state_path
            .join(ryeos_engine::AI_DIR)
            .join("node")
            .join("auth")
            .join("authorized_keys")
            .join(format!("{user_fp}.toml"));
        if !user_key_path.exists() {
            fast_fixture::write_authorized_key_signed_by(
                &state_path,
                &fixture.user,
                &fixture.node,
            )?;
        }

        // Bind `:0` and read the real address from daemon.json (no
        // cross-process port-pick race).
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let harness_id = next_harness_id();
        // UDS socket in a temp dir (avoids writing socket into workspace tree)
        let uds_path = state_dir_outer.path().join("ryeosd.sock");
        fast_fixture::write_harness_bootstrap_config(&state_path, bind, &uds_path)?;
        let stderr_log_path = daemon_stderr_log_path(state_dir_outer.path(), harness_id);

        let mut cmd = ryeosd_command();
        // NOTE: NO . The fast fixture is the init.
        cmd.arg("--app-root")
            .arg(&state_path)
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--uds-path")
            .arg(&uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &state_path)
            .env("HOME", user_space.path())
            .stdout(Stdio::null())
            .stderr(daemon_stderr_stdio(&stderr_log_path)?)
            .kill_on_drop(true);

        tweak(&mut cmd);

        let startup_deadline = daemon_startup_deadline()?;
        let mut child = cmd.spawn()?;

        let daemon_json = state_path.join("daemon.json");
        wait_for_daemon_discovery(
            &mut child,
            &daemon_json,
            &stderr_log_path,
            "daemon (fast fixture path)",
            startup_deadline,
        )
        .await?;

        let actual_bind = read_actual_bind(&daemon_json)?;

        wait_for_daemon_ready(
            &mut child,
            actual_bind,
            &stderr_log_path,
            "daemon (fast fixture path)",
            startup_deadline,
        )
        .await?;

        let harness = Self {
            _state_dir_outer: state_dir_outer,
            _core_bundle_tmp: core_bundle_tmp,
            state_path,
            user_space,
            bind: actual_bind,
            uds_path,
            child,
            stderr_log_path,
            user_key: Some(fixture.user.clone()),
            node_key: Some(fixture.node.clone()),
        };
        Ok((harness, fixture))
    }

    /// Spawn a fast-fixture daemon with one exact handoff crash boundary
    /// connected to the parent over an inherited Unix stream.
    ///
    /// This helper only exists in the explicit test-support build. The caller
    /// waits for [`HandoffCrashGate::wait_reached`], SIGKILLs this harness, and
    /// then uses [`respawn_with`] without a gate to exercise normal recovery.
    #[cfg(all(unix, feature = "handoff-test-support"))]
    pub async fn start_fast_with_handoff_crash_gate<S>(
        plant: S,
        boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture, HandoffCrashGate)>
    where
        S: FnOnce(&Path, &Path, &fast_fixture::FastFixture) -> anyhow::Result<()>,
    {
        let (gate, writer) = HandoffCrashGate::pair(boundary)?;
        let (harness, fixture) = Self::start_fast_with(plant, move |command| {
            HandoffCrashGate::attach_writer(command, writer, boundary)
                .expect("bind handoff crash gate to daemon");
        })
        .await?;
        Ok((harness, fixture, gate))
    }

    /// Spawn a signed fast-fixture daemon with a releaseable gate at the
    /// committed scoped-child reservation, for same-daemon START/RESUME races.
    #[cfg(all(unix, feature = "handoff-test-support"))]
    pub async fn start_fast_with_scoped_reserved_attempt_gate<S>(
        plant: S,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture, ScopedReservedAttemptGate)>
    where
        S: FnOnce(&Path, &Path, &fast_fixture::FastFixture) -> anyhow::Result<()>,
    {
        let (gate, child) = ScopedReservedAttemptGate::pair()?;
        let (harness, fixture) = Self::start_fast_with(plant, move |command| {
            ScopedReservedAttemptGate::attach(command, child)
                .expect("bind scoped reserved-attempt gate to daemon");
        })
        .await?;
        Ok((harness, fixture, gate))
    }

    /// Cross-site variant of [`start_fast_with_handoff_crash_gate`] with an
    /// independently selected node identity.
    #[cfg(all(unix, feature = "handoff-test-support"))]
    pub async fn start_fast_with_node_key_and_handoff_crash_gate<S>(
        node_key: SigningKey,
        plant: S,
        boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture, HandoffCrashGate)>
    where
        S: FnOnce(&Path, &Path, &fast_fixture::FastFixture) -> anyhow::Result<()>,
    {
        let (gate, writer) = HandoffCrashGate::pair(boundary)?;
        let (harness, fixture) = Self::start_fast_with_node_key(node_key, plant, move |command| {
            HandoffCrashGate::attach_writer(command, writer, boundary)
                .expect("bind handoff crash gate to daemon");
        })
        .await?;
        Ok((harness, fixture, gate))
    }

    /// POST `/execute` to the daemon and return (status, json body).
    ///
    /// When the harness was created via `start_fast`, the request is
    /// signed with the user key. Otherwise the request is sent unsigned
    /// (for old test paths that don't require auth).
    pub async fn post_execute(
        &self,
        item_ref: &str,
        project_path: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<(reqwest::StatusCode, serde_json::Value)> {
        // The live HTTP contract does not accept relative project paths.
        // Historical tests used `.` merely to mean "no project overlay";
        // represent that intent with the current omitted-project shape so the
        // daemon creates its isolated execution workspace. Tests exercising a
        // real project pass its absolute tempdir path instead.
        let project_path = (project_path != ".").then_some(project_path);

        // The directive runtime's signed launch contract requires an explicit
        // model identity. These fixtures keep model configuration on the
        // directive itself, so the correct binding is the independently
        // authorized directive ref repeated in the `model` slot.
        let ref_bindings = if item_ref.starts_with("directive:") {
            serde_json::json!({ "model": item_ref })
        } else {
            serde_json::json!({})
        };
        let body = serde_json::json!({
            "item_ref": item_ref,
            "ref_bindings": ref_bindings,
            "project_path": project_path,
            "parameters": params,
            "execution_policy": if project_path.is_some() {
                ryeos_app::execution_policy::ExecutionPolicy::local_live(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                )
            } else {
                ryeos_app::execution_policy::ExecutionPolicy::projectless(
                    ryeos_app::execution_policy::ExecutionResponse::Wait,
                )
            },
        });
        self.post_json("/execute", body).await
    }

    /// Threads visible to this harness's signed principal, through
    /// `service:threads/list`. A running daemon's projection is never opened
    /// from the test process; threads requested by another principal are not
    /// listed.
    pub async fn list_threads(&self, limit: usize) -> anyhow::Result<Vec<ListedThread>> {
        let (status, body) = self
            .post_execute("service:threads/list", ".", serde_json::json!({ "limit": limit }))
            .await?;
        anyhow::ensure!(status.is_success(), "threads/list failed: {status} {body}");
        let threads = body
            .pointer("/result/threads")
            .cloned()
            .context("threads/list returned no thread array")?;
        Ok(serde_json::from_value(threads)?)
    }

    /// One thread through `service:threads/get`: `None` when the daemon does
    /// not know it, otherwise the `{thread, result, artifacts, facets}` view.
    pub async fn get_thread(&self, thread_id: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let (status, body) = self
            .post_execute(
                "service:threads/get",
                ".",
                serde_json::json!({ "thread_id": thread_id }),
            )
            .await?;
        anyhow::ensure!(status.is_success(), "threads/get failed: {status} {body}");
        Ok(body.get("result").filter(|value| !value.is_null()).cloned())
    }

    /// Persisted events of one thread as `(event_type, payload)` in chain
    /// order, read through `service:events/replay` (Standard bundle). `None`
    /// when the daemon does not know the thread yet. A running daemon's
    /// projection is never opened from the test process.
    pub async fn thread_events(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<Option<Vec<(String, serde_json::Value)>>> {
        Ok(self
            .replay_events(
                "service:events/replay",
                serde_json::json!({ "thread_id": thread_id }),
            )
            .await?
            .map(|events| {
                events
                    .into_iter()
                    .map(|(_, event_type, payload)| (event_type, payload))
                    .collect()
            }))
    }

    /// Persisted events of one chain as `(thread_id, event_type, payload)` in
    /// chain order, read through `service:events/chain_replay` (Standard
    /// bundle). `None` when the daemon does not know the chain root yet.
    pub async fn chain_events(
        &self,
        chain_root_id: &str,
    ) -> anyhow::Result<Option<Vec<(String, String, serde_json::Value)>>> {
        self.replay_events(
            "service:events/chain_replay",
            serde_json::json!({ "chain_root_id": chain_root_id }),
        )
        .await
    }

    /// Every persisted event of every chain this harness's principal can
    /// list, as `(thread_id, event_type, payload)`, each chain in chain order.
    /// Read through `service:threads/list` and `service:events/chain_replay`;
    /// empty while the daemon cannot answer, so callers may poll it.
    pub async fn principal_events(&self) -> Vec<(String, String, serde_json::Value)> {
        let Ok(threads) = self.list_threads(500).await else {
            return Vec::new();
        };
        let chain_roots = threads
            .iter()
            .map(|thread| thread.chain_root_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut events = Vec::new();
        for chain_root in chain_roots {
            if let Ok(Some(chain)) = self.chain_events(&chain_root).await {
                events.extend(chain);
            }
        }
        events
    }

    /// Page through one replay service until its cursor is exhausted.
    async fn replay_events(
        &self,
        service_ref: &str,
        scope: serde_json::Value,
    ) -> anyhow::Result<Option<Vec<(String, String, serde_json::Value)>>> {
        let mut events = Vec::new();
        let mut after_chain_seq: Option<i64> = None;
        loop {
            let mut params = scope.clone();
            params["after_chain_seq"] = serde_json::json!(after_chain_seq);
            params["limit"] = serde_json::json!(500);
            let (status, body) = self.post_execute(service_ref, ".", params).await?;
            if status == reqwest::StatusCode::NOT_FOUND {
                return Ok(None);
            }
            anyhow::ensure!(status.is_success(), "{service_ref} failed: {status} {body}");
            let page = body
                .pointer("/result/events")
                .and_then(serde_json::Value::as_array)
                .with_context(|| format!("{service_ref} returned no event array"))?;
            for event in page {
                let field = |name: &str| {
                    event
                        .get(name)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .with_context(|| format!("replayed event has no {name}: {event}"))
                };
                let payload = event.get("payload").cloned().unwrap_or(serde_json::Value::Null);
                events.push((field("thread_id")?, field("event_type")?, payload));
            }
            match body
                .pointer("/result/next_cursor")
                .and_then(serde_json::Value::as_i64)
            {
                Some(cursor) => after_chain_seq = Some(cursor),
                None => return Ok(Some(events)),
            }
        }
    }

    /// Like [`Self::thread_events`] but the thread must exist.
    pub async fn existing_thread_events(
        &self,
        thread_id: &str,
    ) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
        self.thread_events(thread_id)
            .await?
            .with_context(|| format!("events/replay does not know thread {thread_id}"))
    }

    /// POST JSON to an authenticated daemon route and return
    /// (status, json body). When the harness has fast-fixture keys, the
    /// request is signed for the exact route path.
    pub async fn post_json(
        &self,
        route_path: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<(reqwest::StatusCode, serde_json::Value)> {
        let body_bytes = serde_json::to_vec(&body)?;

        let mut req = reqwest::Client::new()
            .post(format!("http://{}{}", self.bind, route_path))
            .header("content-type", "application/json")
            .body(body_bytes.clone());

        if let (Some(user_key), Some(node_key)) = (&self.user_key, &self.node_key) {
            let headers =
                build_signed_headers_for_bytes(user_key, node_key, "POST", route_path, &body_bytes);
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }

        let resp = req.send().await?;
        let status = resp.status();
        let value: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
        Ok((status, value))
    }

    /// Path the daemon writes standalone-mode audit records to.
    pub fn standalone_audit_path(&self) -> PathBuf {
        self.state_path.join(".ai/state/audit/standalone.ndjson")
    }

    /// SIGKILL the daemon child and wait for it to exit and for the
    /// UDS socket to disappear. Does **not** re-spawn — call
    /// [`respawn_with`] afterward.
    ///
    /// This split enables the caller to perform actions (e.g. kill
    /// an orphaned subprocess) between daemon death and respawn, so
    /// the new daemon's reconciler sees the correct process state.
    pub async fn kill_daemon(&mut self) -> anyhow::Result<()> {
        self.child
            .start_kill()
            .map_err(|e| anyhow::anyhow!("failed to SIGKILL daemon child: {e}"))?;

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_status)) => break,
                Ok(None) => {
                    if Instant::now() > deadline {
                        anyhow::bail!("daemon child did not exit within 5s after SIGKILL");
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => {
                    return Err(error).context("observe exact daemon child exit after SIGKILL");
                }
            }
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !self.uds_path.exists() {
                break;
            }
            if Instant::now() > deadline {
                let _ = std::fs::remove_file(&self.uds_path);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Ok(())
    }

    /// Stop the daemon in order, as a service manager does: SIGTERM to the
    /// exact `ryeosd` process authenticated over its own UDS endpoint, then
    /// wait for a successful exit. Stopped-node commands need a node that
    /// completed a first start; a SIGKILL would instead leave crash recovery
    /// for the next start.
    #[cfg(unix)]
    pub async fn stop_gracefully(&mut self) -> anyhow::Result<()> {
        use std::os::fd::AsFd as _;

        let stream = std::os::unix::net::UnixStream::connect(&self.uds_path)
            .with_context(|| format!("connect daemon endpoint {}", self.uds_path.display()))?;
        let peer = lillux::local_ipc::AuthenticatedUnixPeer::capture(stream.as_fd())?;
        peer.require_executable_name(std::ffi::OsStr::new("ryeosd"))?;
        drop(stream);
        peer.request_termination()
            .context("request orderly daemon stop")?;
        let status = tokio::time::timeout(Duration::from_secs(120), self.child.wait())
            .await
            .context("daemon did not stop within 120s of its orderly stop request")?
            .context("observe daemon exit after orderly stop request")?;
        anyhow::ensure!(
            status.success(),
            "daemon orderly stop exited with {status}; stderr log {}",
            self.stderr_log_path.display()
        );
        Ok(())
    }

    /// Re-spawn the daemon against the same `state_path`, `user_space`,
    /// `bind`, and `uds_path`. The caller passes a `tweak` closure to
    /// set env vars or args (e.g. `RUST_LOG`).
    ///
    /// Must be called after [`kill_daemon`]. The reconciler runs
    /// automatically at startup and picks up any orphaned threads.
    ///
    /// **No ``** is passed — state is already initialized.
    pub async fn respawn_with<F: FnOnce(&mut Command)>(&mut self, tweak: F) -> anyhow::Result<()> {
        // A restarted node keeps the same configured endpoint. Cross-node
        // recovery peers must not need an out-of-band route rewrite merely
        // because this test process replaced the daemon generation.
        let bind = self.bind;
        let harness_id = next_harness_id();
        let stderr_log_path = daemon_stderr_log_path(self._state_dir_outer.path(), harness_id);

        // Remove the old daemon.json so we can detect when the new
        // daemon writes its own (with the new bind address).
        let daemon_json = self.state_path.join("daemon.json");
        let _ = std::fs::remove_file(&daemon_json);

        let mut cmd = ryeosd_command();
        cmd.arg("--app-root")
            .arg(&self.state_path)
            // The initial disposable daemon records the requested `:0` bind,
            // while recovery must preserve the resolved endpoint so its peer
            // can redrive without rewriting route authority.
            .arg("--force")
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--uds-path")
            .arg(&self.uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &self.state_path)
            .env("HOME", self.user_space.path())
            .stdout(Stdio::null())
            .stderr(daemon_stderr_stdio(&stderr_log_path)?)
            .kill_on_drop(true);

        tweak(&mut cmd);

        let startup_deadline = daemon_startup_deadline()?;
        self.child = cmd.spawn()?;

        wait_for_daemon_discovery(
            &mut self.child,
            &daemon_json,
            &stderr_log_path,
            "respawned daemon",
            startup_deadline,
        )
        .await?;
        self.bind = read_actual_bind(&daemon_json)?;

        wait_for_daemon_ready(
            &mut self.child,
            self.bind,
            &stderr_log_path,
            "respawned daemon",
            startup_deadline,
        )
        .await?;
        self.stderr_log_path = stderr_log_path;

        Ok(())
    }

    /// Re-spawn a stopped fast-fixture daemon with one test-only handoff crash
    /// gate. Unlike the startup-recovery helper below, this waits for normal
    /// readiness and returns the parent gate so a test can issue a live request,
    /// observe its exact durable cut, and SIGKILL the process.
    #[cfg(all(unix, feature = "handoff-test-support"))]
    pub async fn respawn_with_handoff_crash_gate(
        &mut self,
        boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
        tweak: impl FnOnce(&mut Command),
    ) -> anyhow::Result<HandoffCrashGate> {
        let bind = self.bind;
        let harness_id = next_harness_id();
        let stderr_log_path = daemon_stderr_log_path(self._state_dir_outer.path(), harness_id);
        let daemon_json = self.state_path.join("daemon.json");
        let _ = std::fs::remove_file(&daemon_json);
        let (gate, writer) = HandoffCrashGate::pair(boundary)?;
        let mut cmd = ryeosd_command();
        cmd.arg("--app-root")
            .arg(&self.state_path)
            .arg("--force")
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--uds-path")
            .arg(&self.uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &self.state_path)
            .env("HOME", self.user_space.path())
            .stdout(Stdio::null())
            .stderr(daemon_stderr_stdio(&stderr_log_path)?)
            .kill_on_drop(true);
        HandoffCrashGate::attach_writer(&mut cmd, writer, boundary)?;
        tweak(&mut cmd);

        let startup_deadline = daemon_startup_deadline()?;
        self.child = cmd.spawn()?;
        wait_for_daemon_discovery(
            &mut self.child,
            &daemon_json,
            &stderr_log_path,
            "gated respawned daemon",
            startup_deadline,
        )
        .await?;
        self.bind = read_actual_bind(&daemon_json)?;
        wait_for_daemon_ready(
            &mut self.child,
            self.bind,
            &stderr_log_path,
            "gated respawned daemon",
            startup_deadline,
        )
        .await?;
        self.stderr_log_path = stderr_log_path;
        Ok(gate)
    }

    /// Re-spawn against the same durable app root and return only after normal
    /// startup recovery reaches one exact handoff boundary.
    ///
    /// Unlike [`respawn_with`], this deliberately does not wait for daemon
    /// readiness: recovery can reach the cut before the ready surface is
    /// published. On success the child is still alive and parked at the cut;
    /// callers must [`kill_daemon`] it before a normal respawn.
    #[cfg(all(unix, feature = "handoff-test-support"))]
    pub async fn respawn_until_handoff_crash_boundary(
        &mut self,
        boundary: ryeos_app::worker_handoff::test_support::HandoffCrashBoundary,
    ) -> anyhow::Result<ryeos_app::worker_handoff::test_support::HandoffPhaseCutEvidence> {
        let bind = self.bind;
        let harness_id = next_harness_id();
        let stderr_log_path = daemon_stderr_log_path(self._state_dir_outer.path(), harness_id);
        let daemon_json = self.state_path.join("daemon.json");
        let _ = std::fs::remove_file(&daemon_json);
        let (mut gate, writer) = HandoffCrashGate::pair(boundary)?;

        let mut command = ryeosd_command();
        command
            .arg("--app-root")
            .arg(&self.state_path)
            .arg("--force")
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--uds-path")
            .arg(&self.uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &self.state_path)
            .env("HOME", self.user_space.path())
            .stdout(Stdio::null())
            .stderr(daemon_stderr_stdio(&stderr_log_path)?)
            .kill_on_drop(true);
        HandoffCrashGate::attach_writer(&mut command, writer, boundary)?;

        self.child = command.spawn()?;
        self.stderr_log_path = stderr_log_path.clone();
        match gate.wait_reached().await {
            Ok(evidence) => Ok(evidence),
            Err(error) => {
                let stderr =
                    stop_and_collect_daemon_stderr(&mut self.child, &stderr_log_path).await;
                Err(error).with_context(|| {
                    format!(
                        "respawned daemon did not reach handoff boundary `{boundary}`; daemon stderr:\n{stderr}"
                    )
                })
            }
        }
    }

    /// Kill the daemon child, wait for cleanup, and re-spawn against
    /// the same `state_path`, `user_space`, `bind`, and `uds_path`.
    /// The caller passes a `tweak` closure to set any additional env
    /// vars or args (e.g. `RUST_LOG`).
    ///
    /// After restart, the reconciler runs automatically and picks up
    /// any orphaned threads from the previous daemon run.
    ///
    /// **No ``** is passed — the state directory is
    /// already initialized from the original spawn.
    ///
    /// For tests that need to kill orphaned subprocesses between daemon death
    /// and respawn, use [`kill_daemon`] + [`respawn_with`] instead.
    pub async fn restart_with<F: FnOnce(&mut Command)>(&mut self, tweak: F) -> anyhow::Result<()> {
        // 1. SIGKILL the current child.
        self.child
            .start_kill()
            .map_err(|e| anyhow::anyhow!("failed to SIGKILL daemon child: {e}"))?;

        // 2. Wait for the child to exit.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_status)) => break,
                Ok(None) => {
                    if Instant::now() > deadline {
                        anyhow::bail!("daemon child did not exit within 5s after SIGKILL");
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(_) => break, // Child already reaped.
            }
        }

        // 3. Wait for UDS socket file to disappear (or force-remove).
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !self.uds_path.exists() {
                break;
            }
            if Instant::now() > deadline {
                let _ = std::fs::remove_file(&self.uds_path);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // 4. Re-spawn with `:0`; read actual bind from daemon.json.
        let bind = self.bind;
        let harness_id = next_harness_id();
        let stderr_log_path = daemon_stderr_log_path(self._state_dir_outer.path(), harness_id);
        let daemon_json = self.state_path.join("daemon.json");
        let _ = std::fs::remove_file(&daemon_json);
        let mut cmd = ryeosd_command();
        cmd.arg("--app-root")
            .arg(&self.state_path)
            .arg("--force")
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--uds-path")
            .arg(&self.uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &self.state_path)
            .env("HOME", self.user_space.path())
            .stdout(Stdio::null())
            .stderr(daemon_stderr_stdio(&stderr_log_path)?)
            .kill_on_drop(true);

        tweak(&mut cmd);

        let startup_deadline = daemon_startup_deadline()?;
        self.child = cmd.spawn()?;

        // Wait for the restarted daemon to publish daemon.json with
        // its newly-bound address.
        wait_for_daemon_discovery(
            &mut self.child,
            &daemon_json,
            &stderr_log_path,
            "restarted daemon",
            startup_deadline,
        )
        .await?;
        self.bind = read_actual_bind(&daemon_json)?;

        wait_for_daemon_ready(
            &mut self.child,
            self.bind,
            &stderr_log_path,
            "restarted daemon",
            startup_deadline,
        )
        .await?;
        self.stderr_log_path = stderr_log_path;

        Ok(())
    }

    /// Convenience wrapper around [`restart_with`] that passes an
    /// identity tweak (no additional env vars / args).
    pub async fn restart(&mut self) -> anyhow::Result<()> {
        self.restart_with(|_| {}).await
    }

    /// Read whatever has accumulated in the daemon's stderr log without
    /// waiting for the daemon to exit. The daemon writes directly to this
    /// regular file, so diagnostics never create pipe backpressure.
    pub async fn drain_stderr_nonblocking(&mut self) -> String {
        match std::fs::read_to_string(&self.stderr_log_path) {
            Ok(stderr) => stderr,
            Err(error) => format!(
                "<failed to read daemon stderr log {}: {error}>\n",
                self.stderr_log_path.display()
            ),
        }
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        // Kill the daemon and wait for its exit. Runtimes it launched run in
        // their own process groups and deliberately survive daemon death for
        // restart recovery, so a SIGKILLed or stopped daemon can leave them
        // reparented to init holding inherited descriptors. Once the daemon is
        // gone, terminate every execution still attached in its Runtime store.
        let _ = self.child.start_kill();
        let deadline = Instant::now() + Duration::from_secs(10);
        let exited = loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) | Err(_) => break false,
            }
        };
        if exited {
            terminate_attached_executions(&self.state_path);
        }
    }
}

/// Hard-terminate every execution still attached in an exited node's Runtime
/// store. Each attachment is an exact process identity, so a recycled PID or
/// process group is never signalled. The store is read through the native
/// test gate only after the daemon has exited.
fn terminate_attached_executions(state_path: &Path) {
    let runtime_db = state_path
        .join(ryeos_engine::AI_DIR)
        .join("state/runtime.sqlite3");
    if !runtime_db.is_file() {
        return;
    }
    let attached = std::panic::catch_unwind(|| {
        ryeos_app::test_support::read_existing_runtime(&runtime_db, |connection| {
            let mut statement = connection.prepare(
                "SELECT process_identity FROM thread_runtime WHERE process_identity IS NOT NULL",
            )?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    });
    let Ok(attached) = attached else {
        eprintln!(
            "harness teardown could not read attached executions from {}",
            runtime_db.display()
        );
        return;
    };
    for raw in attached {
        match serde_json::from_str::<ryeos_app::process::ExecutionProcessIdentity>(&raw) {
            Ok(identity) if ryeos_app::process::execution_group_alive(&identity) => {
                let result = ryeos_app::process::kill_by_action(
                    &identity,
                    ryeos_app::process::ShutdownAction::Hard,
                );
                if !result.success {
                    eprintln!(
                        "harness teardown could not terminate attached execution {raw}: {}",
                        result.method
                    );
                }
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("harness teardown found an undecodable process identity: {error}");
            }
        }
    }
}

/// Persistent standalone harness for multi-step `run-service` tests.
///
/// Holds a stopped node alive across multiple `run_service()` calls so that
/// state (installed bundles, node identity, trust store) persists. Core is
/// installed under `.ai/bundles/core/` and registered via
/// `.ai/node/bundles/core.yaml`, so preflight's
/// `discover_installed_bundle_roots` finds it.
///
/// Use this when a test needs to run several `ryeosd run-service`
/// invocations against the same state (e.g. install → list → remove).
pub struct StandaloneHarness {
    /// The node, started once and stopped in order; owns every tempdir.
    node: DaemonHarness,
    /// Persistent app root of the stopped node.
    pub app_root: PathBuf,
    /// Fixture keys for signing.
    pub fixture: fast_fixture::FastFixture,
}

impl StandaloneHarness {
    /// Create a stopped node from the authored Standard profile. Stopped-node
    /// commands refuse a node that never completed a first start, so the node
    /// is started once as a daemon and stopped in order before any
    /// `run_service()` call.
    pub async fn new_initialized() -> anyhow::Result<Self> {
        Self::new_initialized_authorizing(&[]).await
    }

    /// Like [`Self::new_initialized`], but the sealed node policy also grants
    /// explicit command-registration authority to each named bundle. A bundle
    /// installed later by `bundle/install` must already hold that authority:
    /// installation refuses a prospective bundle set the node policy does not
    /// cover, and the policy generation cannot change after initialization.
    pub async fn new_initialized_authorizing(bundle_names: &[&str]) -> anyhow::Result<Self> {
        let (mut node, fixture) = DaemonHarness::start_fast_with_optional_identities(
            None,
            None,
            FastBundleSelection::PreparedStandard,
            |state_path, _user_space, fixture| {
                fast_fixture::register_standard_composition(state_path, fixture)?;
                for bundle_name in bundle_names {
                    fast_fixture::authorize_fixture_bundle(state_path, bundle_name, &fixture.node)?;
                }
                Ok(())
            },
            |_| {},
        )
        .await?;
        node.stop_gracefully().await?;
        let app_root = node.state_path.clone();
        Ok(Self {
            node,
            app_root,
            fixture,
        })
    }

    /// Run `ryeosd run-service <service_ref> [--params <json>]` against
    /// the persistent state. Returns the process output.
    pub async fn run_service(
        &self,
        service_ref: &str,
        params_json: Option<&str>,
    ) -> anyhow::Result<std::process::Output> {
        let mut cmd = ryeosd_command();
        cmd.arg("--app-root")
            .arg(&self.app_root)
            .arg("--uds-path")
            .arg(&self.node.uds_path)
            .arg("run-service")
            .arg(service_ref);
        if let Some(p) = params_json {
            cmd.arg("--params").arg(p);
        }
        cmd.env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &self.app_root)
            .env("HOME", self.node.user_space.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();
        if let Some(mut s) = child.stdout.take() {
            s.read_to_end(&mut stdout_buf).await?;
        }
        if let Some(mut s) = child.stderr.take() {
            s.read_to_end(&mut stderr_buf).await?;
        }
        let status = child.wait().await?;
        Ok(std::process::Output {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        })
    }
}

/// Run `ryeosd run-service <ref> ...` standalone against a fresh node.
///
/// Stopped-node commands refuse a node that has never completed a first
/// start, so the node is initialized from the authored Standard profile,
/// started once as a daemon and stopped in order before the standalone
/// command runs. The stopped harness is returned so callers keep its state
/// alive for follow-up inspection.
pub async fn run_service_standalone_fresh(
    service_ref: &str,
    params_json: Option<&str>,
) -> anyhow::Result<(std::process::Output, DaemonHarness)> {
    let (mut node, _fixture) = DaemonHarness::start_fast().await?;
    node.stop_gracefully().await?;

    let mut cmd = ryeosd_command();
    cmd.arg("--app-root")
        .arg(&node.state_path)
        .arg("--uds-path")
        .arg(&node.uds_path)
        .arg("run-service")
        .arg(service_ref);
    if let Some(p) = params_json {
        cmd.arg("--params").arg(p);
    }
    cmd.env("HOSTNAME", "testhost")
        .env("RYEOS_APP_ROOT", &node.state_path)
        .env("HOME", node.user_space.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn()?;
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    if let Some(mut s) = child.stdout.take() {
        s.read_to_end(&mut stdout_buf).await?;
    }
    if let Some(mut s) = child.stderr.take() {
        s.read_to_end(&mut stderr_buf).await?;
    }
    let status = child.wait().await?;
    Ok((
        std::process::Output {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        },
        node,
    ))
}

/// Build ryeos-signed auth headers for a test request.
///
/// Signs with `user_key` using the daemon's principal_id (from `node_key`)
/// as the audience. Returns a vec of (header_name, header_value) pairs.
pub fn build_signed_headers_for_bytes(
    user_key: &SigningKey,
    node_key: &SigningKey,
    method: &str,
    path: &str,
    body: &[u8],
) -> Vec<(String, String)> {
    let fp = lillux::signature::compute_fingerprint(&user_key.verifying_key());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let nonce = format!("{:016x}", rand::random::<u64>());

    // Audience = daemon's principal_id (from node key).
    let audience = format!(
        "fp:{}",
        lillux::signature::compute_fingerprint(&node_key.verifying_key())
    );

    let body_hash = lillux::cas::sha256_hex(body);
    let string_to_sign = format!(
        "ryeos-request-v1\n{}\n{}\n{}\n{}\n{}\n{}",
        method.to_uppercase(),
        path,
        body_hash,
        timestamp,
        nonce,
        audience,
    );
    let content_hash = lillux::cas::sha256_hex(string_to_sign.as_bytes());
    let sig: lillux::crypto::Signature = user_key.sign(content_hash.as_bytes());
    let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());

    vec![
        ("x-ryeos-key-id".into(), format!("fp:{fp}")),
        ("x-ryeos-timestamp".into(), timestamp),
        ("x-ryeos-nonce".into(), nonce),
        ("x-ryeos-signature".into(), sig_b64),
    ]
}

/// One row of `service:threads/list`, as the requesting principal sees it.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ListedThread {
    pub thread_id: String,
    pub chain_root_id: String,
    pub kind: String,
    pub status: String,
    pub item_ref: String,
    pub upstream_thread_id: Option<String>,
    pub successor_thread_id: Option<String>,
    pub requested_by: Option<String>,
    pub project_root: Option<String>,
    #[serde(default)]
    pub project_authority: Option<ryeos_state::objects::ExecutionProjectAuthority>,
    pub admitted_launch_capsule_hash: Option<String>,
}

/// Open a stopped daemon's node Store through the native test gate, trusting
/// only that node's own signing key. The daemon must already have exited.
pub fn open_stopped_node_store(
    state_path: &Path,
) -> anyhow::Result<ryeos_app::test_support::TestStateStore> {
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/private_key.pem");
    let identity = ryeos_node_runtime::identity::NodeIdentity::load(&identity_path)
        .with_context(|| format!("load node identity {}", identity_path.display()))?;
    let signer = std::sync::Arc::new(ryeos_app::state_store::NodeIdentitySigner::from_identity(
        &identity,
    ));
    let mut trust = ryeos_state::refs::TrustStore::new();
    trust.insert(identity.fingerprint().to_owned(), *identity.verifying_key());
    ryeos_app::test_support::TestStateStore::open(state_path, signer, std::sync::Arc::new(trust))
}
