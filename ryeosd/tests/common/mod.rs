//! Shared helpers for real end-to-end ryeosd tests.
//!
//! These helpers spawn the actual `ryeosd` binary as a child process,
//! configure trust + system bundles in a tempdir, and provide an
//! HTTP client to talk to the daemon over TCP.
//!
//! Used by `cleanup_e2e.rs`. NOT used by `cleanup_invariants.rs`
//! (those are pure in-process invariant checks).

pub mod fast_fixture;
pub mod mock_provider;

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

/// Path to the built `ryeosd` binary (set by Cargo for integration tests
/// in this crate).
pub fn ryeosd_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ryeosd"))
}

/// Path to the built `rye` CLI binary, which lives in the same `target/<profile>/`
/// directory as `ryeosd`. We build it on demand if it's not present, since
/// Cargo only auto-builds bins from the same package as the integration test.
pub fn rye_binary() -> PathBuf {
    let candidate = ryeosd_binary()
        .parent()
        .expect("ryeosd binary has parent dir")
        .join("rye");
    if !candidate.exists() {
        // Build it. This blocks the test until cargo finishes; it should
        // be a no-op once the binary is up-to-date.
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = std::process::Command::new(&cargo)
            .args(["build", "-p", "ryeos-cli", "--bin", "rye"])
            .status()
            .expect("failed to invoke `cargo build -p ryeos-cli`");
        assert!(status.success(), "cargo build -p ryeos-cli failed");
    }
    assert!(
        candidate.exists(),
        "rye binary not found at {} after cargo build",
        candidate.display()
    );
    candidate
}

/// The repo workspace root (parent of `ryeosd/`).
pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("ryeosd has parent dir")
        .to_path_buf()
}

/// Path to the workspace core bundle (source for test copies).
/// DO NOT pass this directly to the daemon as --system-space-dir —
/// the daemon mutates that dir. Use [`copy_core_to_temp`] for the
/// Returns the workspace's core bundle directory — used as a read-only
/// daemon's system space dir.
///
/// This IS safe to use for `RYE_SYSTEM_SPACE` (read-only bundle
/// discovery) and as a source for `copy_dir_all`.
pub fn workspace_core_dir() -> PathBuf {
    workspace_root().join("ryeos-bundles/core")
}

/// Copy the core bundle to an isolated temp dir and return `(tempdir, path)`.
/// The daemon can safely write into the copy without polluting the workspace.
pub fn copy_core_to_temp() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir for core bundle copy");
    let src = workspace_core_dir();
    let dst = tmp.path().join("core");
    copy_dir_recursive(&src, &dst).expect("copy core bundle to temp");
    (tmp, dst)
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

/// Pick an unused TCP port by binding `127.0.0.1:0`, reading the assigned
/// port, then dropping the listener. There is a small race between this
/// function returning and a child binding the same port, but for local
/// integration tests it is acceptable.
pub fn pick_free_port() -> u16 {
    let listener = StdTcpListener::bind(("127.0.0.1", 0)).expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Configure a tempdir as a USER_SPACE: pre-populate
/// `<user>/.ai/config/keys/trusted/` with the fixture trusted signers
/// so the core bundle's items verify under the daemon's trust store.
pub fn populate_user_space(user_space: &Path) {
    let trusted_dst = user_space.join(".ai/config/keys/trusted");
    std::fs::create_dir_all(&trusted_dst).expect("create user trusted keys dir");
    for entry in std::fs::read_dir(fixture_trusted_signer_dir())
        .expect("read fixture trusted_signers")
    {
        let entry = entry.expect("trusted_signer entry");
        let name = entry.file_name();
        std::fs::copy(entry.path(), trusted_dst.join(&name)).expect("copy fixture trusted signer");
    }
}

/// A live ryeosd daemon child process bound to `bind`, with state under
/// `system_space_dir`. Drop kills the child and best-effort cleans up the UDS.
pub struct DaemonHarness {
    /// Outer tempdir for UDS socket (RAII cleanup).
    _state_dir_outer: TempDir,
    /// Tempdir holding the copied core bundle. The daemon writes into this copy,
    /// not the workspace tree.
    _core_bundle_tmp: TempDir,
    /// Path the daemon was launched with as `--system-space-dir`. Use this for
    /// reading `daemon.json`, audit files, etc.
    pub state_path: PathBuf,
    pub user_space: TempDir,
    pub bind: SocketAddr,
    pub uds_path: PathBuf,
    pub child: Child,
    /// Captured stderr (joined async) — populated on drop for diagnostics.
    pub stderr_buf: Option<String>,
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
    /// (e.g. signed bundle registrations) so that the daemon's Phase 1
    /// bootstrap and engine init pick them up.
    ///
    /// Used by `dispatch_pin.rs` to pre-register the `standard` bundle
    /// so the daemon's `RuntimeRegistry` discovers
    /// `runtime:directive-runtime` at startup — without that, the V5.3
    /// runtime refs would not resolve in tests.
    pub async fn start_with_pre_init<S, F>(
        pre_init: S,
        tweak: F,
    ) -> anyhow::Result<Self>
    where
        S: FnOnce(&Path, &Path) -> anyhow::Result<()>,
        F: FnOnce(&mut Command),
    {
        let state_dir_outer = tempfile::tempdir()?;
        let user_space = tempfile::tempdir()?;
        populate_user_space(user_space.path());

        // Copy core bundle to an isolated temp dir so the daemon writes
        // state (identity, vault, DB, daemon.json) into the copy, not
        // the workspace tree.
        let (core_bundle_tmp, system_space_dir) = copy_core_to_temp();

        pre_init(&system_space_dir, user_space.path())?;

        let port = pick_free_port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        // UDS socket in a temp dir (avoids writing socket into workspace tree)
        let uds_path = state_dir_outer.path().join("ryeosd.sock");

        let mut cmd = Command::new(ryeosd_binary());
        cmd.arg("--init-if-missing")
            .arg("--system-space-dir").arg(&system_space_dir)
            .arg("--bind").arg(bind.to_string())
            .arg("--uds-path").arg(&uds_path)
            .env("HOSTNAME", "testhost")
            .env("USER_SPACE", user_space.path())
            .env("HOME", user_space.path())
            // When RYEOSD_TEST_STDERR_DIR is set, mirror daemon stderr
            // to a stable on-disk file (named per-port) so test
            // failures can dump diagnostics post-mortem. Otherwise
            // pipe so drain_stderr_nonblocking can read it directly.
            .stdout(Stdio::null())
            .stderr(
                std::env::var_os("RYEOSD_TEST_STDERR_DIR")
                    .and_then(|d| {
                        let path = std::path::PathBuf::from(d)
                            .join(format!("daemon-{port}.stderr.log"));
                        std::fs::File::create(&path).ok().map(Stdio::from)
                    })
                    .unwrap_or_else(Stdio::piped)
            )
            .kill_on_drop(true);

        tweak(&mut cmd);

        let child = cmd.spawn()?;

        let daemon_json = system_space_dir.join("daemon.json");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if daemon_json.exists() {
                break;
            }
            if Instant::now() > deadline {
                // Drain stderr so the failure message includes the
                // daemon's own diagnostics. Stderr may be either a
                // piped handle or the on-disk log file.
                let mut child = child;
                child.start_kill().ok();
                let mut buf = String::new();
                if let Some(dir) = std::env::var_os("RYEOSD_TEST_STDERR_DIR") {
                    let path = std::path::PathBuf::from(dir)
                        .join(format!("daemon-{port}.stderr.log"));
                    buf = std::fs::read_to_string(&path).unwrap_or_default();
                }
                if let Some(mut stderr) = child.stderr.take() {
                    stderr.read_to_string(&mut buf).await.ok();
                }
                anyhow::bail!(
                    "daemon.json never appeared at {} — daemon stderr:\n{}",
                    daemon_json.display(),
                    buf
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Best-effort: also wait for the HTTP listener to actually accept.
        let client = reqwest::Client::new();
        let url = format!("http://{bind}/health");
        let connect_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if client.get(&url).timeout(Duration::from_millis(200)).send().await.is_ok() {
                break;
            }
            if Instant::now() > connect_deadline {
                anyhow::bail!("daemon /health never became reachable at {url}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Ok(Self {
            _state_dir_outer: state_dir_outer,
            _core_bundle_tmp: core_bundle_tmp,
            state_path: system_space_dir.to_path_buf(),
            user_space,
            bind,
            uds_path,
            child,
            stderr_buf: None,
        })
    }

    /// Spawn a fresh daemon using the fast fixture: state and user-space
    /// are pre-populated with deterministic keys, vault keypair, and
    /// self-signed trust docs (mirrors `bootstrap::init` byte-equivalent
    /// state). Daemon launches WITHOUT `--init-if-missing` since
    /// initialization is already complete — any drift surfaces as a
    /// loud failure rather than silent re-init.
    ///
    /// Returns the harness paired with the deterministic
    /// [`fast_fixture::FastFixture`] keys so callers can sign their own
    /// items (directives, routes, providers, …) with
    /// `fixture.publisher`.
    pub async fn start_fast() -> anyhow::Result<(Self, fast_fixture::FastFixture)> {
        Self::start_fast_with(|_, _, _| Ok(()), |_| {}).await
    }

    /// Like [`start_fast`] but with two hooks:
    ///
    /// * `plant`: runs after `populate_initialized_state` and receives
    ///   `(state_path, user_space, &FastFixture)` — sign and place
    ///   bundle/directive/route content with `fixture.publisher`.
    /// * `tweak`: mutates the `Command` (env, args, …) before spawn.
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
        plant(&state_path, user_space.path(), &fixture)?;

        let port = pick_free_port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        // UDS socket in a temp dir (avoids writing socket into workspace tree)
        let uds_path = state_dir_outer.path().join("ryeosd.sock");

        let mut cmd = Command::new(ryeosd_binary());
        // NOTE: NO --init-if-missing. The fast fixture is the init.
        cmd.arg("--system-space-dir").arg(&state_path)
            .arg("--bind").arg(bind.to_string())
            .arg("--uds-path").arg(&uds_path)
            .env("HOSTNAME", "testhost")
            .env("USER_SPACE", user_space.path())
            .env("HOME", user_space.path())
            .stdout(Stdio::null())
            .stderr(
                std::env::var_os("RYEOSD_TEST_STDERR_DIR")
                    .and_then(|d| {
                        let path = std::path::PathBuf::from(d)
                            .join(format!("daemon-{port}.stderr.log"));
                        std::fs::File::create(&path).ok().map(Stdio::from)
                    })
                    .unwrap_or_else(Stdio::piped)
            )
            .kill_on_drop(true);

        tweak(&mut cmd);

        let child = cmd.spawn()?;

        let daemon_json = state_path.join("daemon.json");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if daemon_json.exists() {
                break;
            }
            if Instant::now() > deadline {
                let mut child = child;
                child.start_kill().ok();
                let mut buf = String::new();
                if let Some(dir) = std::env::var_os("RYEOSD_TEST_STDERR_DIR") {
                    let path = std::path::PathBuf::from(dir)
                        .join(format!("daemon-{port}.stderr.log"));
                    buf = std::fs::read_to_string(&path).unwrap_or_default();
                }
                if let Some(mut stderr) = child.stderr.take() {
                    stderr.read_to_string(&mut buf).await.ok();
                }
                anyhow::bail!(
                    "daemon.json never appeared at {} (fast fixture path) — daemon stderr:\n{}",
                    daemon_json.display(),
                    buf
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let client = reqwest::Client::new();
        let url = format!("http://{bind}/health");
        let connect_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if client.get(&url).timeout(Duration::from_millis(200)).send().await.is_ok() {
                break;
            }
            if Instant::now() > connect_deadline {
                anyhow::bail!("daemon /health never became reachable at {url} (fast fixture path)");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let harness = Self {
            _state_dir_outer: state_dir_outer,
            _core_bundle_tmp: core_bundle_tmp,
            state_path,
            user_space,
            bind,
            uds_path,
            child,
            stderr_buf: None,
        };
        Ok((harness, fixture))
    }

    /// POST `/execute` to the daemon and return (status, json body).
    pub async fn post_execute(
        &self,
        item_ref: &str,
        project_path: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<(reqwest::StatusCode, serde_json::Value)> {
        let body = serde_json::json!({
            "item_ref": item_ref,
            "project_path": project_path,
            "parameters": params,
        });
        let resp = reqwest::Client::new()
            .post(format!("http://{}/execute", self.bind))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let value: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
        Ok((status, value))
    }

    /// Path the daemon writes standalone-mode audit records to.
    pub fn standalone_audit_path(&self) -> PathBuf {
        self.state_path.join(".ai/state/audit/standalone.ndjson")
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
        }).await;
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
    populate_user_space(user_space.path());

    // Copy core bundle to temp so daemon init doesn't mutate workspace.
    let (core_tmp, core_path) = copy_core_to_temp();

    let mut cmd = Command::new(ryeosd_binary());
    cmd.arg("--init-if-missing")
        .arg("--system-space-dir").arg(&core_path)
        .arg("--uds-path").arg(state_dir.path().join("ryeosd.sock"))
        .arg("run-service")
        .arg(service_ref);
    if let Some(p) = params_json {
        cmd.arg("--params").arg(p);
    }
    cmd.env("HOSTNAME", "testhost")
        .env("USER_SPACE", user_space.path())
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
    Ok((
        std::process::Output { status, stdout: stdout_buf, stderr: stderr_buf },
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
