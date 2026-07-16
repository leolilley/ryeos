//! Shared helpers for real end-to-end ryeosd tests.
//!
//! These helpers spawn the actual `ryeosd` binary as a child process,
//! configure trust + system bundles in a tempdir, and provide an
//! HTTP client to talk to the daemon over TCP.
//!
//! Used by `cleanup_e2e.rs`. NOT used by `cleanup_invariants.rs`
//! (those are pure in-process invariant checks).

#![allow(dead_code)]

pub mod fast_fixture;
pub mod mock_provider;

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

fn daemon_stderr_log_path(harness_id: u64) -> Option<PathBuf> {
    std::env::var_os("RYEOSD_TEST_STDERR_DIR")
        .map(|dir| PathBuf::from(dir).join(format!("daemon-{harness_id}.stderr.log")))
}

async fn stop_and_collect_daemon_stderr(child: &mut Child, harness_id: u64) -> String {
    child.start_kill().ok();

    let mut stderr = String::new();
    if tokio::time::timeout(Duration::from_secs(2), child.wait())
        .await
        .is_err()
    {
        stderr.push_str("<daemon did not exit within 2s after kill>\n");
    }
    if let Some(mut pipe) = child.stderr.take() {
        match tokio::time::timeout(Duration::from_millis(500), pipe.read_to_string(&mut stderr))
            .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                stderr.push_str(&format!("<failed to read daemon stderr: {error}>\n"));
            }
            Err(_) => {
                stderr.push_str("<daemon stderr remained open after 500ms; drain abandoned>\n");
            }
        }
    }

    if let Some(path) = daemon_stderr_log_path(harness_id) {
        if let Ok(log) = std::fs::read_to_string(path) {
            if !stderr.is_empty() && !log.is_empty() {
                stderr.push_str("<daemon stderr log>\n");
            }
            stderr.push_str(&log);
        }
    }
    stderr
}

async fn wait_for_daemon_ready(
    child: &mut Child,
    bind: SocketAddr,
    harness_id: u64,
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
                    let stderr = stop_and_collect_daemon_stderr(child, harness_id).await;
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
            let stderr = stop_and_collect_daemon_stderr(child, harness_id).await;
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
    harness_id: u64,
    context: &str,
    deadline: Instant,
) -> anyhow::Result<()> {
    loop {
        if daemon_json.exists() {
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let stderr = stop_and_collect_daemon_stderr(child, harness_id).await;
            anyhow::bail!(
                "{context} exited with {status} before publishing {} — daemon stderr:\n{stderr}",
                daemon_json.display()
            );
        }
        if Instant::now() > deadline {
            let stderr = stop_and_collect_daemon_stderr(child, harness_id).await;
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

/// Path to the built `ryeos` CLI binary, which lives in the same `target/<profile>/`
/// directory as `ryeosd`. We build it on demand if it's not present, since
/// Cargo only auto-builds bins from the same package as the integration test.
pub fn ryeos_binary() -> PathBuf {
    let candidate = ryeosd_binary()
        .parent()
        .expect("ryeosd binary has parent dir")
        .join("ryeos");

    // Always ask Cargo to build once per test process. Cargo no-ops
    // when fresh; this guarantees the subprocess CLI matches the
    // current source rather than a stale pre-v0.4.0 artifact left
    // over from an earlier build. (See PLAN-V0.4.0-CLI-INTEGRATION-401-FIX.md.)
    static BUILD_RYEOS: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    BUILD_RYEOS.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = std::process::Command::new(&cargo)
            .args(["build", "-p", "ryeos-cli", "--bin", "ryeos"])
            .status()
            .expect("failed to invoke `cargo build -p ryeos-cli`");
        assert!(status.success(), "cargo build -p ryeos-cli failed");
    });

    assert!(
        candidate.exists(),
        "ryeos binary not found at {} after cargo build",
        candidate.display()
    );
    candidate
}

/// The repo workspace root (parent of `crates/bin/daemon/`).
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|p| p.join("bundles").is_dir())
        .expect("workspace root with bundles/ directory")
        .to_path_buf()
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

/// Copy the core bundle to an isolated temp dir and return `(tempdir, path)`.
/// The daemon can safely write into the copy without polluting the workspace.
pub fn copy_core_to_temp() -> (TempDir, PathBuf) {
    ensure_bundles_fresh();
    let tmp = tempfile::tempdir().expect("tempdir for core bundle copy");
    let src = workspace_core_dir();
    let dst = tmp.path().join("core");
    copy_dir_recursive(&src, &dst).expect("copy core bundle to temp");
    (tmp, dst)
}

/// Ensure published bundle artifacts under `bundles/{core,standard}/`
/// reflect the current source tree. If any tracked source file is newer
/// than the standard bundle's published manifest ref, re-run
/// `scripts/populate-bundles.sh` to rebuild release binaries, restage
/// them, and re-sign + republish both bundles.
///
/// This guards against the silent stale-artifact failure mode that
/// otherwise bites every E2E that resolves runtimes through bundle CAS:
/// touch a runtime crate, forget to republish, watch tests fail with
/// confusing config / verification errors. Runs at most once per test
/// process via [`std::sync::OnceLock`].
///
/// Set `RYEOS_TEST_SKIP_BUNDLE_REFRESH=1` to opt out (e.g. in CI where
/// the bundles were already published in an earlier step).
pub fn ensure_bundles_fresh() {
    use std::sync::OnceLock;
    static GUARD: OnceLock<()> = OnceLock::new();
    GUARD.get_or_init(|| {
        if std::env::var("RYEOS_TEST_SKIP_BUNDLE_REFRESH").as_deref() == Ok("1") {
            return;
        }
        let root = workspace_root();

        // Cross-process lock: under `cargo test --workspace`, multiple
        // test binaries race to check/refresh bundles. Use an flock-style
        // lock file under a repo-stable temp directory so only one process
        // refreshes at a time without requiring target/ to exist.
        let lock_dir = root.join(".tmp").join("locks");
        std::fs::create_dir_all(&lock_dir).expect("create bundle refresh lock dir");
        let lock_path = lock_dir.join("bundle-refresh.lock");
        let _lock = std::fs::File::create(&lock_path)
            .expect("create bundle refresh lock file");
        // Block until we have exclusive access.
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = _lock.as_raw_fd();
            let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
            assert!(ret == 0, "flock on bundle refresh lock failed");
        }

        // Re-check staleness inside the lock — another process may have
        // already refreshed while we waited.
        // Publication rewrites this ref only after the standard bundle's
        // manifest closure has been committed. Do not use a source item's
        // embedded signature timestamp here: unchanged signed YAML remains
        // byte-for-byte stable across publication, which made every later test
        // incorrectly treat a freshly published bundle as stale.
        let representative = root.join("bundles/standard/.ai/refs/bundles/manifest");
        let publish_time = representative
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
        let needs_refresh = match publish_time {
            None => true,
            Some(m) => bundle_inputs_newer_than(&root, m),
        };
        if !needs_refresh {
            return;
        }
        eprintln!("[ryeosd-tests] bundle artifacts stale — running populate-bundles.sh");
        let key = root.join(".dev-keys/PUBLISHER_DEV.pem");
        let status = std::process::Command::new("bash")
            .arg(root.join("scripts/populate-bundles.sh"))
            .arg("--key").arg(&key)
            .arg("--owner").arg("ryeos-dev")
            .arg("--all")
            .current_dir(&root)
            .status()
            .expect("failed to invoke scripts/populate-bundles.sh");
        assert!(
            status.success(),
            "populate-bundles.sh failed (exit {status}); fix the build or set RYEOS_TEST_SKIP_BUNDLE_REFRESH=1",
        );
        // Lock released when _lock is dropped at end of scope.
    });
}

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
    /// Captured stderr (joined async) — populated on drop for diagnostics.
    pub stderr_buf: Option<String>,
    /// Operator signing key from the fast fixture. Used by `post_execute` to
    /// sign requests. `None` when the daemon was started via `start()`
    /// instead of `start_fast()`.
    pub user_key: Option<SigningKey>,
    /// Node identity key from the fast fixture. Used to compute the
    /// daemon's principal_id for audience binding.
    pub node_key: Option<SigningKey>,
}

impl DaemonHarness {
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
        let _ = fast_fixture::populate_initialized_state(&app_root, user_space.path())?;

        pre_init(&app_root, user_space.path())?;

        // Bind `:0` and let the kernel assign an ephemeral port. The
        // daemon writes the real address back to daemon.json — no
        // port-pick TOCTOU race across concurrent test binaries.
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let harness_id = next_harness_id();
        // UDS socket in a temp dir (avoids writing socket into workspace tree)
        let uds_path = state_dir_outer.path().join("ryeosd.sock");

        let mut cmd = Command::new(ryeosd_binary());
        cmd.arg("--app-root")
            .arg(&app_root)
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--uds-path")
            .arg(&uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &app_root)
            .env("HOME", user_space.path())
            // When RYEOSD_TEST_STDERR_DIR is set, mirror daemon stderr
            // to a stable on-disk file (named per-harness-id) so test
            // failures can dump diagnostics post-mortem. Otherwise
            // pipe so drain_stderr_nonblocking can read it directly.
            .stdout(Stdio::null())
            .stderr(
                std::env::var_os("RYEOSD_TEST_STDERR_DIR")
                    .and_then(|d| {
                        let path = std::path::PathBuf::from(d)
                            .join(format!("daemon-{harness_id}.stderr.log"));
                        std::fs::File::create(&path).ok().map(Stdio::from)
                    })
                    .unwrap_or_else(Stdio::piped),
            )
            .kill_on_drop(true);

        tweak(&mut cmd);

        let startup_deadline = daemon_startup_deadline()?;
        let mut child = cmd.spawn()?;

        let daemon_json = app_root.join("daemon.json");
        wait_for_daemon_discovery(
            &mut child,
            &daemon_json,
            harness_id,
            "daemon",
            startup_deadline,
        )
        .await?;

        // Read the actual bound address — required when we passed :0.
        let actual_bind = read_actual_bind(&daemon_json)?;

        wait_for_daemon_ready(
            &mut child,
            actual_bind,
            harness_id,
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
            stderr_buf: None,
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
        Self::start_fast_with(
            |state_path, _user_space, fixture| {
                fast_fixture::register_standard_bundle(state_path, fixture)
            },
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
        let state_dir_outer = tempfile::tempdir()?;
        let user_space = tempfile::tempdir()?;

        // Copy core bundle to temp so fast fixture writes don't pollute workspace.
        let (core_bundle_tmp, state_path) = copy_core_to_temp();

        let fixture = fast_fixture::populate_initialized_state(&state_path, user_space.path())?;
        // The harness copies `bundles/core` to `state_path`. Register it
        // so `bootstrap::verify_initialized` sees at least one bundle. Tests
        // that need additional bundles call `register_standard_bundle` from
        // their `plant` hook.
        fast_fixture::register_core_bundle_at_state(&state_path, &fixture)?;
        plant(&state_path, user_space.path(), &fixture)?;

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

        let mut cmd = Command::new(ryeosd_binary());
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
            .stderr(
                std::env::var_os("RYEOSD_TEST_STDERR_DIR")
                    .and_then(|d| {
                        let path = std::path::PathBuf::from(d)
                            .join(format!("daemon-{harness_id}.stderr.log"));
                        std::fs::File::create(&path).ok().map(Stdio::from)
                    })
                    .unwrap_or_else(Stdio::piped),
            )
            .kill_on_drop(true);

        tweak(&mut cmd);

        let startup_deadline = daemon_startup_deadline()?;
        let mut child = cmd.spawn()?;

        let daemon_json = state_path.join("daemon.json");
        wait_for_daemon_discovery(
            &mut child,
            &daemon_json,
            harness_id,
            "daemon (fast fixture path)",
            startup_deadline,
        )
        .await?;

        let actual_bind = read_actual_bind(&daemon_json)?;

        wait_for_daemon_ready(
            &mut child,
            actual_bind,
            harness_id,
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
            stderr_buf: None,
            user_key: Some(fixture.user.clone()),
            node_key: Some(fixture.node.clone()),
        };
        Ok((harness, fixture))
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
        });
        self.post_json("/execute", body).await
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
                Err(_) => break,
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

    /// Re-spawn the daemon against the same `state_path`, `user_space`,
    /// `bind`, and `uds_path`. The caller passes a `tweak` closure to
    /// set env vars or args (e.g. `RUST_LOG`).
    ///
    /// Must be called after [`kill_daemon`]. The reconciler runs
    /// automatically at startup and picks up any orphaned threads.
    ///
    /// **No ``** is passed — state is already initialized.
    pub async fn respawn_with<F: FnOnce(&mut Command)>(&mut self, tweak: F) -> anyhow::Result<()> {
        // Bind `:0` and read the new actual address back from daemon.json.
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let harness_id = next_harness_id();

        // Remove the old daemon.json so we can detect when the new
        // daemon writes its own (with the new bind address).
        let daemon_json = self.state_path.join("daemon.json");
        let _ = std::fs::remove_file(&daemon_json);

        let mut cmd = Command::new(ryeosd_binary());
        cmd.arg("--app-root")
            .arg(&self.state_path)
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--uds-path")
            .arg(&self.uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &self.state_path)
            .env("HOME", self.user_space.path())
            .stdout(Stdio::null())
            .stderr(
                std::env::var_os("RYEOSD_TEST_STDERR_DIR")
                    .and_then(|d| {
                        let path = std::path::PathBuf::from(d)
                            .join(format!("daemon-{harness_id}.stderr.log"));
                        std::fs::File::create(&path).ok().map(Stdio::from)
                    })
                    .unwrap_or_else(Stdio::piped),
            )
            .kill_on_drop(true);

        tweak(&mut cmd);

        let startup_deadline = daemon_startup_deadline()?;
        self.child = cmd.spawn()?;

        wait_for_daemon_discovery(
            &mut self.child,
            &daemon_json,
            harness_id,
            "respawned daemon",
            startup_deadline,
        )
        .await?;
        self.bind = read_actual_bind(&daemon_json)?;

        wait_for_daemon_ready(
            &mut self.child,
            self.bind,
            harness_id,
            "respawned daemon",
            startup_deadline,
        )
        .await?;

        Ok(())
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
    /// For tests that need to kill orphaned subprocesses between
    /// daemon death and respawn, use [`kill_daemon`] + [`respawn_with`]
    /// instead.
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
        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let harness_id = next_harness_id();
        let daemon_json = self.state_path.join("daemon.json");
        let _ = std::fs::remove_file(&daemon_json);
        let mut cmd = Command::new(ryeosd_binary());
        cmd.arg("--app-root")
            .arg(&self.state_path)
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--uds-path")
            .arg(&self.uds_path)
            .env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &self.state_path)
            .env("HOME", self.user_space.path())
            .stdout(Stdio::null())
            .stderr(
                std::env::var_os("RYEOSD_TEST_STDERR_DIR")
                    .and_then(|d| {
                        let path = std::path::PathBuf::from(d)
                            .join(format!("daemon-{harness_id}.stderr.log"));
                        std::fs::File::create(&path).ok().map(Stdio::from)
                    })
                    .unwrap_or_else(Stdio::piped),
            )
            .kill_on_drop(true);

        tweak(&mut cmd);

        let startup_deadline = daemon_startup_deadline()?;
        self.child = cmd.spawn()?;

        // Wait for the restarted daemon to publish daemon.json with
        // its newly-bound address.
        wait_for_daemon_discovery(
            &mut self.child,
            &daemon_json,
            harness_id,
            "restarted daemon",
            startup_deadline,
        )
        .await?;
        self.bind = read_actual_bind(&daemon_json)?;

        wait_for_daemon_ready(
            &mut self.child,
            self.bind,
            harness_id,
            "restarted daemon",
            startup_deadline,
        )
        .await?;

        Ok(())
    }

    /// Convenience wrapper around [`restart_with`] that passes an
    /// identity tweak (no additional env vars / args).
    pub async fn restart(&mut self) -> anyhow::Result<()> {
        self.restart_with(|_| {}).await
    }

    /// Drain whatever has accumulated on the child's stderr handle
    /// **without blocking** on EOF. Used by tests that need to print
    /// diagnostics on assertion failure without waiting for the
    /// daemon to exit. After the call the stderr handle is gone, so
    /// only call once per harness.
    pub async fn drain_stderr_nonblocking(&mut self) -> String {
        // When RYEOSD_TEST_STDERR_DIR is set, the harness redirects
        // stderr to a per-port file there; read that. Otherwise, fall
        // through and drain the piped handle.
        if let Some(dir) = std::env::var_os("RYEOSD_TEST_STDERR_DIR") {
            let path = std::path::PathBuf::from(dir)
                .join(format!("daemon-{}.stderr.log", self.bind.port()));
            if let Ok(s) = std::fs::read_to_string(&path) {
                return s;
            }
        }
        use tokio::time::{timeout, Duration};
        let Some(mut stderr) = self.child.stderr.take() else {
            return String::new();
        };
        let mut buf = Vec::new();
        let _ = timeout(Duration::from_millis(500), async {
            let mut chunk = [0u8; 8192];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
        })
        .await;
        String::from_utf8_lossy(&buf).into_owned()
    }
}

impl Drop for DaemonHarness {
    fn drop(&mut self) {
        // Best-effort: kill the child synchronously. tokio::process::Child
        // sets KILLONDROP via kill_on_drop(true), but we also want to drain
        // stderr for diagnostics if a test fails.
        if let Some(stderr) = self.child.stderr.take() {
            // Drain on a tokio runtime if one is around; otherwise discard.
            // We can't await here, so just give up on stderr capture in Drop.
            drop(stderr);
        }
        let _ = self.child.start_kill();
    }
}

/// Persistent standalone harness for multi-step `run-service` tests.
///
/// Holds tempdirs alive across multiple `run_service()` calls so that
/// state (installed bundles, node identity, trust store) persists.
/// The harness installs core under `.ai/bundles/core/` AND registers
/// it via `.ai/node/bundles/core.yaml` so preflight's
/// `discover_installed_bundle_roots` finds it.
///
/// Use this when a test needs to run several `ryeosd run-service`
/// invocations against the same state (e.g. install → list → remove).
pub struct StandaloneHarness {
    /// Keeps the core-bundle temp copy alive for the harness lifetime.
    _core_tmp: TempDir,
    /// Persistent app root (the temp copy of core).
    pub app_root: PathBuf,
    /// Persistent app root dir.
    pub user_space: TempDir,
    /// UDS path (unused for standalone but required by CLI).
    uds_path: PathBuf,
    /// Fixture keys for signing.
    pub fixture: fast_fixture::FastFixture,
}

impl StandaloneHarness {
    /// Create a fully initialized standalone harness:
    /// - Fast-fixture state (node identity, vault, trust)
    /// - Core bundle copied into `.ai/bundles/core/` (disk install)
    /// - Core bundle registered in `.ai/node/bundles/core.yaml`
    /// - Standard bundle registered (path points to workspace)
    ///
    /// After this, `run_service()` can invoke any OfflineOnly service
    /// and preflight will find installed bundles for dependency discovery.
    pub fn new_initialized() -> anyhow::Result<Self> {
        let user_space = tempfile::tempdir()?;
        let (core_tmp, app_root) = copy_core_to_temp();
        let fixture = fast_fixture::populate_initialized_state(&app_root, user_space.path())?;
        fast_fixture::register_core_bundle_at_state(&app_root, &fixture)?;
        fast_fixture::register_standard_bundle(&app_root, &fixture)?;

        // Install core under .ai/bundles/core/ so preflight's
        // discover_installed_bundle_roots finds it. Copy from the
        // workspace source (not app_root itself — that would
        // recurse into the .ai/bundles/ subtree we're creating).
        let bundles_root = app_root.join(".ai/bundles");
        let core_install = bundles_root.join("core");
        let core_src = workspace_core_dir();
        copy_dir_recursive(&core_src, &core_install)
            .with_context(|| format!("install core into {}", core_install.display()))?;

        let uds_path = app_root.join("ryeosd.sock");
        Ok(Self {
            _core_tmp: core_tmp,
            app_root,
            user_space,
            uds_path,
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
        let mut cmd = tokio::process::Command::new(ryeosd_binary());
        cmd.arg("--app-root")
            .arg(&self.app_root)
            .arg("--uds-path")
            .arg(&self.uds_path)
            .arg("run-service")
            .arg(service_ref);
        if let Some(p) = params_json {
            cmd.arg("--params").arg(p);
        }
        cmd.env("HOSTNAME", "testhost")
            .env("RYEOS_APP_ROOT", &self.app_root)
            .env("HOME", self.user_space.path())
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

/// Run `ryeosd run-service <ref> ...` standalone (no daemon), with a
/// fresh state_dir + user_space pair, returning (output, state_dir, user_space).
///
/// The tempdirs are returned so callers can keep them alive for follow-up
/// inspection or for a subsequent harness.
pub async fn run_service_standalone(
    state_dir: TempDir,
    user_space: TempDir,
    service_ref: &str,
    params_json: Option<&str>,
) -> anyhow::Result<(std::process::Output, TempDir, TempDir)> {
    // Use the fast fixture to set up a fully initialized state with bundles.
    let (core_tmp, state_path) = copy_core_to_temp();
    let fixture = fast_fixture::populate_initialized_state(&state_path, user_space.path())?;
    fast_fixture::register_core_bundle_at_state(&state_path, &fixture)?;
    fast_fixture::register_standard_bundle(&state_path, &fixture)?;
    drop(ryeos_app::runtime_db::RuntimeDb::open(
        &state_path.join(".ai/state/runtime.sqlite3"),
    )?);

    let mut cmd = Command::new(ryeosd_binary());
    cmd.arg("--app-root")
        .arg(&state_path)
        .arg("--uds-path")
        .arg(state_dir.path().join("ryeosd.sock"))
        .arg("run-service")
        .arg(service_ref);
    if let Some(p) = params_json {
        cmd.arg("--params").arg(p);
    }
    cmd.env("HOSTNAME", "testhost")
        .env("RYEOS_APP_ROOT", &state_path)
        .env("HOME", user_space.path())
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
    // core_tmp cleaned up here — child has exited so files are closed.
    let _ = core_tmp;
    Ok((
        std::process::Output {
            status,
            stdout: stdout_buf,
            stderr: stderr_buf,
        },
        state_dir,
        user_space,
    ))
}

/// Convenience: fresh tempdirs + run_service_standalone.
pub async fn run_service_standalone_fresh(
    service_ref: &str,
    params_json: Option<&str>,
) -> anyhow::Result<(std::process::Output, TempDir, TempDir)> {
    let state_dir = tempfile::tempdir()?;
    let user_space = tempfile::tempdir()?;
    run_service_standalone(state_dir, user_space, service_ref, params_json).await
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
