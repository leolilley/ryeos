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
//! `core-scheduling`, `standard` and `standard-central-auth` init profiles.
//! Every profile except `core` installs the scheduling bundle (`service:scheduler/*`) and
//! all but `core-scheduling` also install the directive bundle
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
//! `fast_fixture::direct_host_isolation`), and the suites that need an
//! exclusive session skip. `RYEOS_TEST_REQUIRE_PROCESS_SCOPES=1` selects the
//! real mode instead (see [`process_scopes`]): the authored isolation member
//! is kept, the node starts under a protected host-runtime binding inside a
//! delegated cgroup subtree, and an unqualified host fails rather than skips.

pub mod callback_probe;
#[cfg(all(unix, feature = "crash-qualification-test-support"))]
pub mod dedicated_command_cut;
pub mod fast_fixture;
pub mod mock_provider;
pub mod process_scopes;
pub mod remote_delegation;
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

/// Path to the built `ryeosd` binary. Cargo builds it beside the integration
/// test executables: the test executable lives in `<profile>/deps/` and the
/// binary in `<profile>/`.
pub fn ryeosd_binary() -> PathBuf {
    let test_executable = std::env::current_exe().expect("resolve the running test executable");
    let profile_directory = test_executable
        .parent()
        .and_then(Path::parent)
        .expect("test executable sits in <profile>/deps");
    profile_directory.join(format!("ryeosd{}", std::env::consts::EXE_SUFFIX))
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

/// The last `lines` lines of a daemon's stderr with the backtrace frames
/// removed. A panic or error run with `RUST_BACKTRACE` set ends in dozens of
/// frame lines (`  16: symbol` and `             at /path:line`) after a
/// `stack backtrace:` line; a plain tail shows only those and loses the line
/// that names the error. Only the lines of that section are dropped, so an
/// error's own `Caused by:` list (`    0: message`) stays.
pub fn stderr_digest(log: &str, lines: usize) -> String {
    fn is_frame(line: &str) -> bool {
        let trimmed = line.trim_start();
        let numbered = trimmed
            .split_once(": ")
            .map(|(number, _)| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false);
        numbered || trimmed.starts_with("at ")
    }
    let mut in_backtrace = false;
    let mut kept: Vec<&str> = Vec::new();
    for line in log.lines() {
        if line.trim() == "stack backtrace:" {
            in_backtrace = true;
            continue;
        }
        if in_backtrace && is_frame(line) {
            continue;
        }
        in_backtrace = false;
        kept.push(line);
    }
    kept[kept.len().saturating_sub(lines)..].join("\n")
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
    let client = ryeos_http_egress::client_builder()
        .build()
        .expect("the HTTP client builds");
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

/// Path to the explicitly prepared `ryeos` CLI binary beside `ryeosd`.
///
/// The validation queue builds this artifact with its declared source, lock,
/// target and feature selection before allocating the integration test. A test
/// must not start another Cargo process or infer freshness from file presence.
///
/// `RYEOS_TEST_CLI_BINARY` names a differently featured CLI built into its own
/// target directory, so a qualification build never replaces the CLI beside
/// the daemon.
pub fn ryeos_binary() -> PathBuf {
    let candidate = match std::env::var_os("RYEOS_TEST_CLI_BINARY").filter(|v| !v.is_empty()) {
        Some(explicit) => PathBuf::from(explicit),
        None => ryeosd_binary()
            .parent()
            .expect("ryeosd binary has parent dir")
            .join("ryeos"),
    };

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
        (
            "directive",
            "model-provider-setup",
            cfg!(feature = "model-provider-setup"),
        ),
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
/// commits the manifest ref, and the signed `.ai/manifest.yaml` after the ref
/// too. Both are publish outputs rather than inputs, so their later mtimes are
/// not staleness.
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
        } else if (path == bundle_root && entry.file_name() == "PUBLISHER_TRUST.toml")
            || (path == bundle_root.join(".ai") && entry.file_name() == "manifest.yaml")
        {
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
///
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

/// Pin `verifying_key` in the node signer registry under `app_root`.
///
/// The record is signed by the app root's node key and rendered with a
/// fixed timestamp so fixture bytes are stable across runs. When the app
/// root has no node identity yet, one is created first. Returns the pinned
/// fingerprint.
pub fn pin_signer(
    app_root: &Path,
    verifying_key: &lillux::crypto::VerifyingKey,
    role: ryeos_engine::trust::SignerRole,
    scope: ryeos_engine::trust::SignerScope,
) -> String {
    let fingerprint = lillux::signature::compute_fingerprint(verifying_key);
    let label = format!("fixture {}", &fingerprint[..12]);
    let key_path = app_root.join(".ai/node/identity/private_key.pem");
    if !key_path.exists() {
        return ryeos_engine::test_support::pin_signer(
            app_root,
            verifying_key,
            &label,
            role,
            scope,
        );
    }
    let node_key = lillux::crypto::load_signing_key(&key_path).expect("load fixture node key");
    ryeos_engine::test_support::pin_signer_chained(
        app_root,
        &node_key,
        verifying_key,
        &label,
        role,
        scope,
        Some(fast_fixture::FAST_FIXTURE_TIME),
    )
}

/// Pin the fixture bundle signers (the keys in `tests/fixtures/trusted_signers`)
/// into the app root's signer registry so the core bundle's items verify
/// under the daemon's trust store.
pub fn populate_trusted_keys(app_root: &Path) {
    use lillux::crypto::DecodePublicKey as _;
    for entry in
        std::fs::read_dir(fixture_trusted_signer_dir()).expect("read fixture trusted_signers")
    {
        let entry = entry.expect("trusted_signer entry");
        let text = std::fs::read_to_string(entry.path()).expect("read fixture trusted signer");
        let doc: toml::Value = toml::from_str(&text).expect("parse fixture trusted signer");
        let pem = doc["public_key"]["pem"]
            .as_str()
            .expect("fixture trusted signer public_key.pem");
        let vk = lillux::crypto::VerifyingKey::from_public_key_pem(pem)
            .expect("decode fixture trusted signer key");
        pin_signer(
            app_root,
            &vk,
            ryeos_engine::trust::SignerRole::Publisher,
            ryeos_engine::trust::SignerScope::everywhere(),
        );
    }
}

/// Trust store holding the fixture bundle signers, loaded from a fresh
/// node signer registry in a leaked temporary app root.
pub fn fixture_trust_store() -> ryeos_engine::trust::TrustStore {
    let app_root = tempfile::tempdir().expect("fixture app root").keep();
    populate_trusted_keys(&app_root);
    ryeos_engine::trust::TrustStore::load_node_registry(&app_root)
        .expect("load fixture signer registry")
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
    CoreOnly,
    CoreScheduling,
    CoreKnowledge,
    Standard,
    StandardCentralAuth,
    Development,
    DevelopmentHosted,
}

impl DaemonHarness {
    /// Register `checkout` to the owner `key` acts for on this node, as the
    /// operator's offline registration would, under the project `name`. A key
    /// that is not the node operator may run a live project only at a
    /// checkout registered to its owner, so a test that has a second
    /// principal run a live project registers it first. The daemon reads
    /// registrations on every request, so no restart is needed.
    pub fn register_checkout_for_key(
        &self,
        key: &lillux::crypto::SigningKey,
        checkout: &Path,
        name: &str,
    ) -> anyhow::Result<()> {
        let canonical = checkout.canonicalize()?;
        let trust = ryeos_engine::trust::TrustStore::load_node_registry(&self.state_path)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let fingerprint = format!(
            "fp:{}",
            lillux::signature::compute_fingerprint(&key.verifying_key())
        );
        let owner = trust
            .owner_for(&fingerprint)
            .map_err(|refusal| anyhow::anyhow!("{refusal}"))?
            .principal;
        let node = ryeos_node_runtime::identity::NodeIdentity::load(
            &self.state_path.join(".ai/node/identity/private_key.pem"),
        )?;
        let identity = ryeos_state::project_identity::ProjectIdentity::new(owner, name.parse()?);
        ryeos_node::project_registry::register(
            &ryeos_node::project_registry::RegistrationChange {
                app_root: &self.state_path,
                node: &node,
                request_digest: &"0".repeat(64),
                path: &canonical,
                identity: &identity,
            },
        )?;
        Ok(())
    }

    /// The daemon's stderr log so far, for tests that must see a refusal the
    /// retained thread record deliberately reduces to a closed code.
    pub fn stderr_so_far(&self) -> String {
        std::fs::read_to_string(&self.stderr_log_path).unwrap_or_default()
    }

    /// The last `lines` lines of the daemon's stderr log, for timeout panics
    /// that must show the shutdown path without flooding the CI log.
    pub fn stderr_tail(&self, lines: usize) -> String {
        let log = self.stderr_so_far();
        let all: Vec<&str> = log.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }

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

        // Plant app-root operator identity + signer records and daemon-local node
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
    /// node-signed signer registry records (mirrors `bootstrap::init`
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
            FastBundleSelection::Standard,
            |_state_path, _user_space, _fixture| Ok(()),
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
            FastBundleSelection::CoreScheduling,
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
            FastBundleSelection::CoreOnly,
            |_state_path, _user_space, _fixture| Ok(()),
            |_| {},
        )
        .await
    }

    /// [`start_fast_core_only_prepared`] with the `plant` hook of
    /// [`start_fast_with`], for a test that adds one signed fixture bundle to
    /// the core-only node before it starts.
    pub async fn start_fast_core_only_with<S>(
        plant: S,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture)>
    where
        S: FnOnce(&Path, &Path, &fast_fixture::FastFixture) -> anyhow::Result<()>,
    {
        Self::start_fast_with_optional_identities(
            None,
            None,
            FastBundleSelection::CoreOnly,
            plant,
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
    /// `register_workflow_bundles(state_path, fixture)` from your `plant`
    /// closure if the test needs workflow bundle services/runtimes.
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
            FastBundleSelection::Standard,
            plant,
            tweak,
        )
        .await
    }

    /// Like [`start_fast_with`], but under the authored Standard-central-auth
    /// composition: Core, Scheduling, the workflow bundles and central-auth.
    pub async fn start_fast_standard_central_auth_with<S, F>(
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
            FastBundleSelection::StandardCentralAuth,
            plant,
            tweak,
        )
        .await
    }

    /// Like [`start_fast_with`], but under the authored Development
    /// composition, which adds central-auth, environment-qualification, the
    /// Render sandbox adapter and the claude-code worker bundle.
    pub async fn start_fast_development_with<S, F>(
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
            FastBundleSelection::Development,
            plant,
            tweak,
        )
        .await
    }

    /// [`start_fast_development_with`] under the Development profile's
    /// `hosted` host class: the trusted process-group lane a hosting
    /// provider runs, where `sessions:trusted_process_group` is provided
    /// (the default Development class, `local`, enforces isolation and
    /// refuses it). The fixture drops only the two enclosure-init floor
    /// entries a direct child cannot prove; see
    /// `fast_fixture::hosted_host_enclosure_floor`.
    pub async fn start_fast_development_hosted_with<S, F>(
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
            FastBundleSelection::DevelopmentHosted,
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
            FastBundleSelection::Standard,
            plant,
            tweak,
        )
        .await
    }

    /// A node initialized with the shipped Core-knowledge profile and its own node
    /// and operator keys: the smallest shipped selection that provides the
    /// `knowledge` kind, for a test that needs a second node that does not
    /// already hold the bundle it pulls.
    pub async fn start_fast_core_knowledge_with_node_and_user_keys(
        node_key: SigningKey,
        user_key: SigningKey,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture)> {
        Self::start_fast_with_optional_identities(
            Some(node_key),
            Some(user_key),
            FastBundleSelection::CoreKnowledge,
            |_state_path, _user_space, _fixture| Ok(()),
            |_| {},
        )
        .await
    }

    /// [`start_fast_core_knowledge_with_node_and_user_keys`] with a `plant`
    /// step that runs after the profile's policy generation is written and
    /// before the initialization is sealed, so a test can replace one policy
    /// member (re-signed with the fixture's node key) and have the sealed
    /// generation carry it.
    pub async fn start_fast_core_knowledge_with_node_and_user_keys_and_plant<S>(
        node_key: SigningKey,
        user_key: SigningKey,
        plant: S,
    ) -> anyhow::Result<(Self, fast_fixture::FastFixture)>
    where
        S: FnOnce(&Path, &Path, &fast_fixture::FastFixture) -> anyhow::Result<()>,
    {
        Self::start_fast_with_optional_identities(
            Some(node_key),
            Some(user_key),
            FastBundleSelection::CoreKnowledge,
            plant,
            |_| {},
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
            FastBundleSelection::Standard,
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
            FastBundleSelection::CoreOnly => copy_core_to_temp_prepared(&["core"])?,
            FastBundleSelection::CoreScheduling => {
                copy_core_to_temp_prepared(&["core", "scheduling"])?
            }
            FastBundleSelection::CoreKnowledge => {
                copy_core_to_temp_prepared(&["core", "knowledge"])?
            }
            FastBundleSelection::Standard => copy_core_to_temp_prepared(
                fast_fixture::FastFixtureProfile::Standard
                    .exact_bundles()
                    .expect("Standard profile has an exact bundle inventory"),
            )?,
            FastBundleSelection::StandardCentralAuth => copy_core_to_temp_prepared(
                fast_fixture::FastFixtureProfile::StandardCentralAuth
                    .exact_bundles()
                    .expect("Standard-central-auth profile has an exact bundle inventory"),
            )?,
            FastBundleSelection::Development | FastBundleSelection::DevelopmentHosted => {
                copy_core_to_temp_prepared(
                    fast_fixture::FastFixtureProfile::Development
                        .exact_bundles()
                        .expect("Development profile has an exact bundle inventory"),
                )?
            }
        };

        let selected_profile = match bundle_selection {
            FastBundleSelection::CoreOnly => fast_fixture::FastFixtureProfile::Core,
            FastBundleSelection::CoreScheduling => fast_fixture::FastFixtureProfile::CoreScheduling,
            FastBundleSelection::CoreKnowledge => fast_fixture::FastFixtureProfile::CoreKnowledge,
            FastBundleSelection::Standard => fast_fixture::FastFixtureProfile::Standard,
            FastBundleSelection::StandardCentralAuth => {
                fast_fixture::FastFixtureProfile::StandardCentralAuth
            }
            FastBundleSelection::Development => fast_fixture::FastFixtureProfile::Development,
            FastBundleSelection::DevelopmentHosted => {
                fast_fixture::FastFixtureProfile::DevelopmentHosted
            }
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
        match bundle_selection {
            FastBundleSelection::CoreKnowledge => {
                fast_fixture::register_core_knowledge_composition(&state_path, &fixture)?
            }
            FastBundleSelection::Standard => {
                fast_fixture::register_standard_composition(&state_path, &fixture)?
            }
            FastBundleSelection::StandardCentralAuth => {
                fast_fixture::register_standard_central_auth_composition(&state_path, &fixture)?
            }
            FastBundleSelection::Development | FastBundleSelection::DevelopmentHosted => {
                fast_fixture::register_development_composition(&state_path, &fixture)?
            }
            FastBundleSelection::CoreOnly | FastBundleSelection::CoreScheduling => {}
        }
        plant(&state_path, user_space.path(), &fixture)?;
        fast_fixture::validate_profile_bundle_records(&state_path, &fixture, selected_profile)?;
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

        let scoped = process_scopes::required();
        // A hosted-class node's floor asks for the enclosure, not for
        // per-execution scopes, so it starts as the init of its own enclosure;
        // every other node starts in a delegated cgroup subtree.
        let placement = if matches!(bundle_selection, FastBundleSelection::DevelopmentHosted) {
            process_scopes::Placement::Enclosure
        } else {
            process_scopes::Placement::Controller
        };
        if scoped {
            process_scopes::prepare(
                &state_path,
                &[
                    state_dir_outer.path(),
                    user_space.path(),
                    core_bundle_tmp.path(),
                ],
                placement,
            )?;
        }
        let mut cmd = match process_scopes::launch_command(&state_path)? {
            Some(mut cmd) => {
                cmd.env("HOSTNAME", "testhost").env(
                    "RUST_MIN_STACK",
                    std::env::var_os("RUST_MIN_STACK").unwrap_or_else(|| "4194304".into()),
                );
                cmd
            }
            None => {
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
                    .env("HOME", user_space.path());
                cmd
            }
        };
        cmd.stdout(Stdio::null())
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
        .await
        .map_err(|error| {
            if scoped {
                error.context(process_scopes::describe_missing(&state_path))
            } else {
                error
            }
        })?;

        let actual_bind = read_actual_bind(&daemon_json)?;

        wait_for_daemon_ready(
            &mut child,
            actual_bind,
            &stderr_log_path,
            "daemon (fast fixture path)",
            startup_deadline,
        )
        .await?;
        if scoped {
            match placement {
                process_scopes::Placement::Controller => {
                    process_scopes::assert_scope_capabilities_provided(&state_path)?
                }
                process_scopes::Placement::Enclosure => {
                    process_scopes::assert_enclosure_capabilities_provided(&state_path)?
                }
            }
        }

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

        // These fixtures keep model configuration on the directive itself;
        // the directive runtime defaults the `model` binding to the executed
        // directive, so no binding is sent.
        let body = serde_json::json!({
            "item_ref": item_ref,
            "ref_bindings": {},
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
            .post_execute(
                "service:threads/list",
                ".",
                serde_json::json!({ "limit": limit }),
            )
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
        // A thread the daemon does not know answers NotFound, as one that is
        // not the caller's does.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        anyhow::ensure!(status.is_success(), "threads/get failed: {status} {body}");
        Ok(body.get("result").filter(|value| !value.is_null()).cloned())
    }

    /// Persisted events of one thread as `(event_type, payload)` in chain
    /// order, read through `service:events/replay` (core bundle). `None`
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
    /// chain order, read through `service:events/chain_replay` (core
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

    /// Wait until the daemon's own terminal event for `thread_id`
    /// (`thread_completed`, `thread_failed`, `thread_cancelled`,
    /// `thread_killed`, `thread_timed_out` or `thread_continued`) is
    /// persisted. The daemon commits that event in the same transaction as the
    /// thread's terminal snapshot, so once it is visible `threads/get` returns
    /// the terminal status and result. Runtime-authored events such as
    /// `graph_completed` precede the daemon's finalization and do not carry
    /// this guarantee. Returns whether the event appeared before `timeout`;
    /// callers keep their own bounded record poll as the fallback.
    pub async fn wait_for_thread_terminal_event(
        &self,
        thread_id: &str,
        timeout: std::time::Duration,
    ) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self
                .principal_events()
                .await
                .iter()
                .any(|(thread, event, _)| {
                    thread == thread_id
                        && ryeos_api::routes::invokers::stream_helpers::is_terminal(event)
                })
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
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
                let payload = event
                    .get("payload")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
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

        let mut req = ryeos_http_egress::client_builder()
            .build()
            .expect("the HTTP client builds")
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

    /// Send one request with no signature, cookie or credential of any kind,
    /// as a stranger would. Returns (status, response headers, body bytes).
    pub async fn raw_request(
        &self,
        method: reqwest::Method,
        route_path: &str,
    ) -> anyhow::Result<(reqwest::StatusCode, reqwest::header::HeaderMap, Vec<u8>)> {
        let resp = ryeos_http_egress::client_builder()
            .build()
            .expect("the HTTP client builds")
            .request(method, format!("http://{}{}", self.bind, route_path))
            .send()
            .await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await?.to_vec();
        Ok((status, headers, body))
    }

    /// Path the daemon writes standalone-mode audit records to.
    pub fn standalone_audit_path(&self) -> PathBuf {
        self.state_path.join(".ai/state/audit/standalone.ndjson")
    }

    /// A replacement daemon needs the node's state lock, its endpoint and (in
    /// the real process-scope mode) its controller leaf, each of which the
    /// running daemon still holds. Replace means the old one is gone first.
    async fn stop_live_daemon_before_respawn(&mut self) -> anyhow::Result<()> {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill_daemon().await?;
        }
        Ok(())
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
        let waited = tokio::time::timeout(Duration::from_secs(120), self.child.wait()).await;
        let status = match waited {
            Ok(status) => status.context("observe daemon exit after orderly stop request")?,
            Err(_) => anyhow::bail!(
                "daemon did not stop within 120s of its orderly stop request; stderr log {}\n--- stderr tail (last 200 lines) ---\n{}",
                self.stderr_log_path.display(),
                self.stderr_tail(200)
            ),
        };
        anyhow::ensure!(
            status.success(),
            "daemon orderly stop exited with {status}; stderr log {}\n--- stderr tail (backtrace frames dropped) ---\n{}",
            self.stderr_log_path.display(),
            std::fs::read_to_string(&self.stderr_log_path)
                .map(|log| stderr_digest(&log, 40))
                .unwrap_or_default()
        );
        Ok(())
    }

    /// The command that starts this harness's node again on its resolved
    /// endpoint: a direct child normally, a node under its protected
    /// host-runtime binding in the real process-scope mode.
    fn respawn_command(&self, bind: SocketAddr) -> anyhow::Result<Command> {
        if process_scopes::required() {
            // The controller hands the node only its app root, so the
            // resolved endpoint a restart must keep goes into the node's own
            // bootstrap config instead of flags.
            fast_fixture::write_harness_bootstrap_config(&self.state_path, bind, &self.uds_path)?;
            let mut cmd = process_scopes::launch_command(&self.state_path)?
                .context("this node was not prepared for the real process-scope mode")?;
            cmd.env("HOSTNAME", "testhost").env(
                "RUST_MIN_STACK",
                std::env::var_os("RUST_MIN_STACK").unwrap_or_else(|| "4194304".into()),
            );
            return Ok(cmd);
        }
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
            .env("HOME", self.user_space.path());
        Ok(cmd)
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
        self.stop_live_daemon_before_respawn().await?;
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

        let mut cmd = self.respawn_command(bind)?;
        cmd.stdout(Stdio::null())
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
        self.stop_live_daemon_before_respawn().await?;
        let bind = self.bind;
        let harness_id = next_harness_id();
        let stderr_log_path = daemon_stderr_log_path(self._state_dir_outer.path(), harness_id);
        let daemon_json = self.state_path.join("daemon.json");
        let _ = std::fs::remove_file(&daemon_json);
        let (gate, writer) = HandoffCrashGate::pair(boundary)?;
        let mut cmd = self.respawn_command(bind)?;
        cmd.stdout(Stdio::null())
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
        self.stop_live_daemon_before_respawn().await?;
        let bind = self.bind;
        let harness_id = next_harness_id();
        let stderr_log_path = daemon_stderr_log_path(self._state_dir_outer.path(), harness_id);
        let daemon_json = self.state_path.join("daemon.json");
        let _ = std::fs::remove_file(&daemon_json);
        let (mut gate, writer) = HandoffCrashGate::pair(boundary)?;

        let mut command = self.respawn_command(bind)?;
        command
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
        let mut cmd = self.respawn_command(bind)?;
        cmd.stdout(Stdio::null())
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
            FastBundleSelection::Standard,
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

    /// Start the stopped node again, as an operator does after a stopped-node
    /// change. Fails when the daemon refuses to boot on the state left behind.
    pub async fn restart_node(&mut self) -> anyhow::Result<()> {
        self.node.restart().await
    }

    /// Stop the node in order, so a stopped-node service can run again.
    pub async fn stop_node(&mut self) -> anyhow::Result<()> {
        self.node.stop_gracefully().await
    }

    /// The names `service:bundle/list` reports from the running node.
    pub async fn listed_bundle_names(&self) -> anyhow::Result<Vec<String>> {
        let (status, body) = self
            .node
            .post_execute("service:bundle/list", ".", serde_json::json!({}))
            .await?;
        anyhow::ensure!(status.as_u16() == 200, "bundle/list: {status} {body:#}");
        Ok(body["result"]["bundles"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("bundle/list carried no bundles: {body:#}"))?
            .iter()
            .filter_map(|entry| entry["name"].as_str().map(str::to_owned))
            .collect())
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
    let content_hash = ryeos_request_signing::content_hash(
        method, path, &body_hash, &timestamp, &nonce, &audience,
    );
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

#[cfg(test)]
mod stderr_digest_tests {
    use super::stderr_digest;

    #[test]
    fn the_error_line_survives_a_backtrace_longer_than_the_tail() {
        let mut log = String::from(
            "INFO started\nError: state lock held by another process\n\nCaused by:\n    0: refused\n",
        );
        log.push_str("stack backtrace:\n");
        for frame in 0..60 {
            log.push_str(&format!(
                "{frame:>4}: ryeosd::frame_{frame}\n             at /src/lib.rs:{frame}\n"
            ));
        }
        let digest = stderr_digest(&log, 10);
        assert!(
            digest.contains("Error: state lock held by another process"),
            "{digest}"
        );
        assert!(digest.contains("Caused by:"), "{digest}");
        assert!(digest.contains("0: refused"), "{digest}");
        assert!(!digest.contains("frame_"), "{digest}");
        assert!(!digest.contains("/src/lib.rs"), "{digest}");
    }

    #[test]
    fn a_log_without_frames_keeps_its_last_lines() {
        let log = "a\nb\nc\nd\n";
        assert_eq!(stderr_digest(log, 2), "c\nd");
    }
}
