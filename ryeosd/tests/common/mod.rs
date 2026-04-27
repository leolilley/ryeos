//! Shared helpers for real end-to-end ryeosd tests.
//!
//! These helpers spawn the actual `ryeosd` binary as a child process,
//! configure trust + system bundles in a tempdir, and provide an
//! HTTP client to talk to the daemon over TCP.
//!
//! Used by `cleanup_e2e.rs`. NOT used by `cleanup_invariants.rs`
//! (those are pure in-process invariant checks).

#![allow(dead_code)] // helpers are only used by some integration test bins

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
            .args(["build", "-p", "rye-cli", "--bin", "rye"])
            .status()
            .expect("failed to invoke `cargo build -p rye-cli`");
        assert!(status.success(), "cargo build -p rye-cli failed");
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

/// Path to the system data dir we hand to the daemon (the bundled
/// `ryeos-bundles/core` tree, which is signed by the fixture key).
pub fn system_data_dir() -> PathBuf {
    workspace_root().join("ryeos-bundles/core")
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
/// `state_path` (which is `<_state_dir_outer>/state`). Drop kills the
/// child and best-effort cleans up the UDS.
pub struct DaemonHarness {
    /// Outer tempdir, kept alive for RAII cleanup. Daemon's actual
    /// `--state-dir` is the `state` subdir inside it.
    _state_dir_outer: TempDir,
    /// Path the daemon was launched with as `--state-dir`. Use this for
    /// reading `daemon.json`, audit files, etc. Equivalent to
    /// `state_dir.path()` in the old API.
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

        // Use a NON-EXISTENT subdir so `--init-if-missing` actually triggers
        // init. (`init-if-missing` skips when the marker file exists.)
        let state_path = state_dir_outer.path().join("state");

        pre_init(&state_path, user_space.path())?;

        let port = pick_free_port();
        let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let uds_path = state_path.join("ryeosd.sock");

        let mut cmd = Command::new(ryeosd_binary());
        cmd.arg("--init-if-missing")
            .arg("--state-dir").arg(&state_path)
            .arg("--bind").arg(bind.to_string())
            .arg("--uds-path").arg(&uds_path)
            .env("RYE_SYSTEM_SPACE", system_data_dir())
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
                    .unwrap_or_else(|| Stdio::piped())
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
            state_path,
            user_space,
            bind,
            uds_path,
            child,
            stderr_buf: None,
        })
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

    // Use a NON-EXISTENT subdir so `--init-if-missing` actually triggers
    // init. Daemon's `--init-if-missing` runs before subcommand dispatch
    // so run-service inherits the init.
    let state_path = state_dir.path().join("state");

    let mut cmd = Command::new(ryeosd_binary());
    cmd.arg("--init-if-missing")
        .arg("--state-dir").arg(&state_path)
        .arg("--uds-path").arg(state_path.join("ryeosd.sock"))
        .arg("run-service")
        .arg(service_ref);
    if let Some(p) = params_json {
        cmd.arg("--params").arg(p);
    }
    cmd.env("RYE_SYSTEM_SPACE", system_data_dir())
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
