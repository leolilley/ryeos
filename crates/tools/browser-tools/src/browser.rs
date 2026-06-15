use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

const DEFAULT_SESSION_ID: &str = "default";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 100_000;
const DEFAULT_MAX_OUTPUT_CHARS: usize = 50_000;
const MAX_MAX_OUTPUT_CHARS: usize = 200_000;
const RUNNER_FILENAME: &str = "ryeos-playwright-runner.mjs";
const RUNNER_SOURCE: &str = include_str!("runner.mjs");

#[derive(Debug, Deserialize, Serialize)]
struct BrowserParams {
    action: BrowserAction,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_chars: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BrowserAction {
    Navigate,
    Snapshot,
    Screenshot,
    Click,
    Type,
    CloseSession,
}

#[derive(Debug, Deserialize)]
struct BrowserConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    node_path: Option<String>,
    #[serde(default = "default_playwright_package")]
    playwright_package: String,
    #[serde(default = "default_browser")]
    browser: String,
    #[serde(default = "default_true")]
    headless: bool,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default = "default_timeout_ms")]
    default_timeout_ms: u64,
    #[serde(default)]
    env: std::collections::BTreeMap<String, String>,
}
impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            node_path: None,
            playwright_package: default_playwright_package(),
            browser: default_browser(),
            headless: true,
            channel: None,
            default_timeout_ms: DEFAULT_TIMEOUT_MS,
            env: Default::default(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BrowserEnvelope {
    success: bool,
    action: BrowserAction,
    session_id: String,
    output: String,
    artifacts: Vec<BrowserArtifact>,
    truncated: bool,
    diagnostics: BrowserDiagnostics,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct BrowserArtifact {
    kind: String,
    path: String,
}
#[derive(Debug, Serialize)]
pub struct BrowserDiagnostics {
    app_root: String,
    config_path: String,
    state_dir: String,
    runner_path: String,
    node_path: Option<String>,
    searched_node: Vec<String>,
}
#[derive(Debug, Deserialize)]
struct RunnerEnvelope {
    success: bool,
    #[serde(default)]
    output: String,
    #[serde(default)]
    artifacts: Vec<BrowserArtifact>,
    #[serde(default)]
    error: Option<String>,
}
struct BrowserContext {
    app_root: PathBuf,
    config_path: PathBuf,
    state_dir: PathBuf,
    runner_path: PathBuf,
    config: BrowserConfig,
    node_path: Option<String>,
    searched_node: Vec<String>,
}

pub fn execute_json(raw: &str) -> anyhow::Result<BrowserEnvelope> {
    execute(serde_json::from_str(raw).context("parse browser params JSON")?)
}

fn execute(params: BrowserParams) -> anyhow::Result<BrowserEnvelope> {
    let action = params.action;
    let session_id =
        sanitize_session_id(params.session_id.as_deref().unwrap_or(DEFAULT_SESSION_ID))?;
    validate_action_inputs(&params)?;
    let ctx = browser_context()?;
    let timeout_ms = params
        .timeout_ms
        .unwrap_or(ctx.config.default_timeout_ms)
        .clamp(1_000, MAX_TIMEOUT_MS);
    let max_output_chars = params
        .max_output_chars
        .unwrap_or(DEFAULT_MAX_OUTPUT_CHARS)
        .clamp(1_000, MAX_MAX_OUTPUT_CHARS);
    if matches!(action, BrowserAction::CloseSession) {
        let _guard = SessionLock::acquire(&ctx.state_dir, &session_id)?;
        let dir = session_dir(&ctx.state_dir, &session_id);
        if dir.exists() {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("remove session dir {}", dir.display()))?;
        }
        return Ok(envelope(
            true,
            action,
            session_id,
            "closed browser session".into(),
            vec![],
            false,
            ctx.diagnostics(),
        ));
    }
    if !ctx.config.enabled {
        return Ok(envelope(
            false,
            action,
            session_id,
            integration_disabled_message(),
            vec![],
            false,
            ctx.diagnostics(),
        ));
    }
    ensure_runner(&ctx.runner_path)?;
    let Some(node_path) = ctx.node_path.clone() else {
        return Ok(envelope(
            false,
            action,
            session_id,
            missing_dependency_message(),
            vec![],
            false,
            ctx.diagnostics(),
        ));
    };
    let _guard = SessionLock::acquire(&ctx.state_dir, &session_id)?;
    let artifact_dir = artifact_dir(&ctx.state_dir)?;
    fs::create_dir_all(&artifact_dir)?;
    fs::create_dir_all(session_dir(&ctx.state_dir, &session_id))?;
    let request = serde_json::json!({ "params": params, "session_id": session_id, "session_dir": session_dir(&ctx.state_dir, &session_id), "artifact_dir": artifact_dir, "playwright_package": ctx.config.playwright_package, "browser": ctx.config.browser, "headless": ctx.config.headless, "channel": ctx.config.channel, "timeout_ms": timeout_ms });
    let runner = run_node_runner(
        &node_path,
        &ctx.runner_path,
        &request.to_string(),
        timeout_ms,
        &ctx.config.env,
    )?;
    let (output, truncated) =
        truncate_chars(&runner.error.unwrap_or(runner.output), max_output_chars);
    Ok(envelope(
        runner.success,
        action,
        session_id,
        output,
        runner.artifacts,
        truncated,
        ctx.diagnostics(),
    ))
}

fn envelope(
    success: bool,
    action: BrowserAction,
    session_id: String,
    output: String,
    artifacts: Vec<BrowserArtifact>,
    truncated: bool,
    diagnostics: BrowserDiagnostics,
) -> BrowserEnvelope {
    BrowserEnvelope {
        success,
        action,
        session_id,
        output,
        artifacts,
        truncated,
        diagnostics,
    }
}

fn validate_action_inputs(params: &BrowserParams) -> anyhow::Result<()> {
    match params.action {
        BrowserAction::Navigate => {
            let url = params
                .url
                .as_deref()
                .ok_or_else(|| anyhow!("navigate requires url"))?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                anyhow::bail!("url must start with http:// or https://");
            }
        }
        BrowserAction::Click => {
            reject_non_navigate_url(params)?;
            if params
                .selector
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            {
                anyhow::bail!("click requires selector");
            }
        }
        BrowserAction::Type => {
            reject_non_navigate_url(params)?;
            if params
                .selector
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
            {
                anyhow::bail!("type requires selector");
            }
            if params.text.as_deref().unwrap_or_default().is_empty() {
                anyhow::bail!("type requires text");
            }
        }
        BrowserAction::Snapshot | BrowserAction::Screenshot | BrowserAction::CloseSession => {
            reject_non_navigate_url(params)?
        }
    }
    Ok(())
}
fn reject_non_navigate_url(params: &BrowserParams) -> anyhow::Result<()> {
    if params.url.is_some() {
        anyhow::bail!("url is only supported for navigate");
    }
    Ok(())
}

fn browser_context() -> anyhow::Result<BrowserContext> {
    let app_root = PathBuf::from(
        env::var("RYEOS_APP_ROOT")
            .context("RYEOS_APP_ROOT must be set to an absolute project/app root")?,
    );
    if !app_root.is_absolute() {
        anyhow::bail!("RYEOS_APP_ROOT must be an absolute path");
    }
    let config_path = app_root.join(".ai/config/browser/browser.yaml");
    let state_dir = app_root.join(".ai/state/cache/tools/rye/browser");
    let runner_path = state_dir.join("runner").join(RUNNER_FILENAME);
    let config = read_browser_config(&config_path)?;
    let mut searched_node = Vec::new();
    let node_path = resolve_node_path(config.node_path.as_deref(), &mut searched_node);
    Ok(BrowserContext {
        app_root,
        config_path,
        state_dir,
        runner_path,
        config,
        node_path,
        searched_node,
    })
}
impl BrowserContext {
    fn diagnostics(&self) -> BrowserDiagnostics {
        BrowserDiagnostics {
            app_root: self.app_root.display().to_string(),
            config_path: self.config_path.display().to_string(),
            state_dir: self.state_dir.display().to_string(),
            runner_path: self.runner_path.display().to_string(),
            node_path: self.node_path.clone(),
            searched_node: self.searched_node.clone(),
        }
    }
}
fn read_browser_config(path: &Path) -> anyhow::Result<BrowserConfig> {
    if !path.exists() {
        return Ok(BrowserConfig::default());
    }
    serde_yaml::from_str(&fs::read_to_string(path)?)
        .with_context(|| format!("parse config {}", path.display()))
}
fn resolve_node_path(configured: Option<&str>, searched: &mut Vec<String>) -> Option<String> {
    if let Some(path) = configured.filter(|p| !p.trim().is_empty()) {
        searched.push(format!("config:{path}"));
        if Path::new(path).is_file() {
            return Some(path.to_string());
        }
    }
    if let Ok(path) = env::var("NODE") {
        searched.push("NODE env var".into());
        if Path::new(&path).is_file() {
            return Some(path);
        }
    }
    searched.push("PATH:node".into());
    find_on_path("node")
}
fn ensure_runner(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, RUNNER_SOURCE).with_context(|| format!("write runner {}", path.display()))
}

const NODE_ENV_ALLOWLIST: &[&str] = &[
    "HOME",
    "PATH",
    "TMPDIR",
    "TEMP",
    "TMP",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "PLAYWRIGHT_BROWSERS_PATH",
];
fn run_node_runner(
    node_path: &str,
    runner_path: &Path,
    request_json: &str,
    timeout_ms: u64,
    extra_env: &std::collections::BTreeMap<String, String>,
) -> anyhow::Result<RunnerEnvelope> {
    let mut command = Command::new(node_path);
    command
        .arg(runner_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear();
    for name in NODE_ENV_ALLOWLIST {
        if let Some(value) = env::var_os(name) {
            command.env(name, value);
        }
    }
    command.envs(extra_env);
    configure_process_group(&mut command);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn node runner {node_path}"))?;
    child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("node runner stdin unavailable"))?
        .write_all(request_json.as_bytes())?;
    drop(child.stdin.take());
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("node runner stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("node runner stderr unavailable"))?;
    let stdout_reader = read_limited_async(stdout, MAX_MAX_OUTPUT_CHARS * 4);
    let stderr_reader = read_limited_async(stderr, MAX_MAX_OUTPUT_CHARS * 4);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = stdout_reader.join().unwrap_or_default();
            let stderr = stderr_reader.join().unwrap_or_default();
            if status.success() {
                return serde_json::from_str(&stdout).with_context(|| {
                    format!("parse node runner JSON output; stderr: {}", stderr.trim())
                });
            }
            return Ok(RunnerEnvelope {
                success: false,
                output: String::new(),
                artifacts: Vec::new(),
                error: Some(if stderr.trim().is_empty() {
                    stdout
                } else {
                    stderr
                }),
            });
        }
        if Instant::now() >= deadline {
            kill_node_process_tree(&mut child);
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Ok(RunnerEnvelope {
                success: false,
                output: String::new(),
                artifacts: Vec::new(),
                error: Some(format!("browser action timed out after {timeout_ms}ms")),
            });
        }
        thread::sleep(Duration::from_millis(25));
    }
}
#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}
fn read_limited_async<R: Read + Send + 'static>(
    mut reader: R,
    max_bytes: usize,
) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut buf = [0_u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let remaining = max_bytes.saturating_sub(bytes.len());
                    if remaining > 0 {
                        bytes.extend_from_slice(&buf[..n.min(remaining)]);
                    }
                }
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}
#[cfg(unix)]
fn kill_node_process_tree(child: &mut std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
}
#[cfg(not(unix))]
fn kill_node_process_tree(child: &mut std::process::Child) {
    let _ = child.kill();
}

struct SessionLock {
    file: File,
}
impl SessionLock {
    fn acquire(state_dir: &Path, session_id: &str) -> anyhow::Result<Self> {
        let lock_dir = state_dir.join("locks");
        fs::create_dir_all(&lock_dir)?;
        let path = lock_dir.join(format!("{session_id}.lock"));
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .open(&path)?;
        for _ in 0..20 {
            if flock_exclusive_nonblocking(&file)? {
                file.set_len(0)?;
                writeln!(file, "{}", std::process::id())?;
                return Ok(Self { file });
            }
            thread::sleep(Duration::from_millis(50));
        }
        anyhow::bail!("session `{session_id}` is locked by another browser action")
    }
}
impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = flock_unlock(&self.file);
    }
}
#[cfg(unix)]
fn flock_exclusive_nonblocking(file: &File) -> anyhow::Result<bool> {
    use std::os::fd::AsRawFd;
    let r = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if r == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) || err.raw_os_error() == Some(libc::EAGAIN) {
        return Ok(false);
    }
    Err(err).context("flock session lock")
}
#[cfg(unix)]
fn flock_unlock(file: &File) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()).context("unlock session lock")
    }
}
#[cfg(not(unix))]
fn flock_exclusive_nonblocking(_file: &File) -> anyhow::Result<bool> {
    Ok(true)
}
#[cfg(not(unix))]
fn flock_unlock(_file: &File) -> anyhow::Result<()> {
    Ok(())
}

fn sanitize_session_id(value: &str) -> anyhow::Result<String> {
    if value.is_empty() || value.len() > 80 {
        anyhow::bail!("session_id must be 1-80 characters");
    }
    if value == "." || value == ".." {
        anyhow::bail!("session_id may not be '.' or '..'");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        anyhow::bail!("session_id may only contain ASCII letters, numbers, '.', '_' and '-'");
    }
    Ok(value.to_string())
}
fn session_dir(state_dir: &Path, session_id: &str) -> PathBuf {
    state_dir.join("sessions").join(session_id)
}
fn artifact_dir(state_dir: &Path) -> anyhow::Result<PathBuf> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok(state_dir
        .join("artifacts")
        .join(format!("{}-{}", now.as_secs(), now.subsec_nanos())))
}
fn truncate_chars(value: &str, max_chars: usize) -> (String, bool) {
    if value.chars().count() <= max_chars {
        return (value.to_string(), false);
    }
    let mut out = value.chars().take(max_chars).collect::<String>();
    out.push_str("\n... [output truncated]");
    (out, true)
}
fn find_on_path(binary: &str) -> Option<String> {
    for dir in env::split_paths(&env::var_os("PATH")?) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}
fn integration_disabled_message() -> String {
    "browser integration is disabled. Set enabled: true in .ai/config/browser/browser.yaml to run the Node/Playwright runner; without it the signed Rust facade only validates inputs and reports diagnostics.".into()
}
fn missing_dependency_message() -> String {
    "browser tool requires Node + Playwright. Configure .ai/config/browser/browser.yaml with node_path: /abs/path/to/node, or ensure node is on PATH and playwright is installed.".into()
}
fn default_playwright_package() -> String {
    "playwright".into()
}
fn default_browser() -> String {
    "chromium".into()
}
fn default_true() -> bool {
    true
}
fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_bad_session_ids() {
        assert!(sanitize_session_id("ok.session-1").is_ok());
        assert!(sanitize_session_id("../bad").is_err());
        assert!(sanitize_session_id(".").is_err());
        assert!(sanitize_session_id("..").is_err());
    }
    #[test]
    fn navigate_requires_http_url() {
        assert!(
            execute_json(r#"{"action":"navigate","url":"file:///tmp/x"}"#)
                .unwrap_err()
                .to_string()
                .contains("http:// or https://")
        );
    }
    #[test]
    fn non_navigate_actions_reject_url() {
        assert!(
            execute_json(r#"{"action":"snapshot","url":"https://example.com"}"#)
                .unwrap_err()
                .to_string()
                .contains("url is only supported for navigate")
        );
    }
    #[test]
    fn second_lock_is_rejected() {
        let dir = env::temp_dir().join(format!("ryeos-browser-lock-test-{}", std::process::id()));
        let _guard = SessionLock::acquire(&dir, "s").unwrap();
        assert!(SessionLock::acquire(&dir, "s").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
