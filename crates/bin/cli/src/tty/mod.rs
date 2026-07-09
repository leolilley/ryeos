//! Rye OS CLI TTY presentation.
//!
//! This is a normal stdout renderer, not the full TUI: every render reads
//! live local state and prints directly, with no on-disk projection.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ryeos_node::{LifecycleController, LifecycleStatus, LocalLifecycleEnv};
use ryeos_state::event_types::{outcome_code_is_failure, thread_terminal_outcome, ThreadOutcomeKind};
use serde_json::Value;

use crate::exec_stream::StreamOutcome;
use crate::error::{CliError, CliTransportError};
use crate::transport::http::SseEvent;
use crate::transport::signing::Signer;

const DEFAULT_TERMINAL_WIDTH: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyScreen {
    Home,
    Help,
}

pub async fn run(
    app_root: &Path,
    explicit_project: Option<&Path>,
    screen: TtyScreen,
) -> std::result::Result<(), CliError> {
    let project = resolve_project_for_display(explicit_project);
    let operator_problem = resolve_operator(app_root);
    let remote_url = remote_daemon_url();

    let rendered_lines = render(&loading_projection(screen, &project), 0)?;

    let live = build_live_projection(
        app_root,
        &project,
        operator_problem.as_deref(),
        screen,
        remote_url.as_deref(),
    )
    .await;
    render(&live, rendered_lines)?;

    Ok(())
}

/// `Some(problem)` when the operator signing key is missing or errors,
/// surfaced in the node detail line; `None` when it resolves fine.
fn resolve_operator(app_root: &Path) -> Option<String> {
    match Signer::resolve(app_root) {
        Ok(_) => None,
        Err(CliTransportError::SigningKeyMissing { .. }) => {
            Some("operator signing key missing".to_string())
        }
        Err(err) => Some(format!("operator signing key error: {err}")),
    }
}

#[derive(Debug, Clone)]
struct ProjectDisplay {
    key: Option<String>,
    label: String,
    detail: Option<String>,
}

fn resolve_project_for_display(explicit_project: Option<&Path>) -> ProjectDisplay {
    if let Some(path) = explicit_project {
        let abs = absolutize(path);
        return match abs.canonicalize() {
            Ok(canonical) if canonical.is_dir() => {
                let key = canonical.display().to_string();
                ProjectDisplay {
                    label: path_label(&canonical),
                    detail: Some(key.clone()),
                    key: Some(key),
                }
            }
            Ok(canonical) => ProjectDisplay {
                key: None,
                label: canonical.display().to_string(),
                detail: Some("not a directory".to_string()),
            },
            Err(err) => ProjectDisplay {
                key: None,
                label: abs.display().to_string(),
                detail: Some(format!("unavailable: {err}")),
            },
        };
    }

    let Ok(cwd) = std::env::current_dir().and_then(|cwd| cwd.canonicalize()) else {
        return ProjectDisplay {
            key: None,
            label: "none".to_string(),
            detail: Some("current directory unavailable".to_string()),
        };
    };

    for ancestor in cwd.ancestors() {
        if ancestor.join(ryeos_engine::AI_DIR).is_dir() {
            let key = ancestor.display().to_string();
            return ProjectDisplay {
                label: path_label(ancestor),
                detail: Some(key.clone()),
                key: Some(key),
            };
        }
    }

    ProjectDisplay {
        key: None,
        label: "none".to_string(),
        detail: None,
    }
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn path_label(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_else(|| path.to_str().unwrap_or("project"))
        .to_string()
}

fn remote_daemon_url() -> Option<String> {
    std::env::var("RYEOSD_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn loading_projection(screen: TtyScreen, project: &ProjectDisplay) -> TtyHomeFile {
    TtyHomeFile {
        screen,
        sections: TtyHomeSections {
            node: TtyNodeSummary {
                status: "loading".to_string(),
                detail: Some("reading local Rye OS state".to_string()),
            },
            project: Some(TtyProjectSummary::from_project(project)),
            commands: TtyCommandSummary {
                count: None,
                detail: Some(loading_command_detail(screen).to_string()),
            },
            items: screen_items(screen, false, Vec::new()),
        },
    }
}

async fn build_live_projection(
    app_root: &Path,
    project: &ProjectDisplay,
    operator_problem: Option<&str>,
    screen: TtyScreen,
    remote_url: Option<&str>,
) -> TtyHomeFile {
    let mut node = match remote_url {
        Some(remote_url) => TtyNodeSummary {
            status: "remote override".to_string(),
            detail: Some(format!("RYEOSD_URL={remote_url}")),
        },
        None => lifecycle_summary(app_root).await,
    };
    let snapshot = crate::node_descriptors::load_verified_snapshot(app_root);
    let (command_count, has_tui_command, verified_items) = match snapshot {
        Ok(snapshot) => {
            let descriptors =
                crate::node_descriptors::load_command_descriptors_from_snapshot(&snapshot);
            let has_tui_command = descriptors
                .iter()
                .any(|command| command.tokens.len() == 1 && command.tokens[0] == "tui");
            (
                Some(
                    snapshot.commands.len()
                        + crate::lifecycle_commands::local_command_descriptors().len(),
                ),
                has_tui_command,
                verified_command_items(descriptors),
            )
        }
        Err(_) => (
            Some(crate::lifecycle_commands::local_command_descriptors().len()),
            false,
            Vec::new(),
        ),
    };

    if let Some(problem) = operator_problem {
        node.detail = Some(match node.detail {
            Some(detail) => format!("{detail}; {problem}"),
            None => problem.to_string(),
        });
    }

    TtyHomeFile {
        screen,
        sections: TtyHomeSections {
            node,
            project: Some(TtyProjectSummary::from_project(project)),
            commands: TtyCommandSummary {
                count: command_count,
                detail: command_count
                    .is_none()
                    .then(|| "run `ryeos node doctor` for diagnostics".to_string()),
            },
            items: screen_items(screen, has_tui_command, verified_items),
        },
    }
}

fn verified_command_items(
    descriptors: Vec<crate::node_descriptors::LoadedCommandDescriptor>,
) -> Vec<TtyItem> {
    let mut items = descriptors
        .into_iter()
        .filter(|command| !(command.tokens.len() == 1 && command.tokens[0].len() <= 1))
        .map(|command| command_item(command.tokens, command.description))
        .collect::<Vec<_>>();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    items
}

async fn lifecycle_summary(app_root: &Path) -> TtyNodeSummary {
    let env = match LocalLifecycleEnv::load(Some(app_root.to_path_buf())) {
        Ok(env) => env,
        Err(err) => {
            return TtyNodeSummary {
                status: "config error".to_string(),
                detail: Some(err.to_string()),
            }
        }
    };
    let controller = LifecycleController::from_env(env);
    match controller.status().await {
        Ok(LifecycleStatus::NotInitialized { diagnostics }) => TtyNodeSummary {
            status: "not initialized".to_string(),
            detail: Some(diagnostics.message),
        },
        Ok(LifecycleStatus::Stopped { app_root }) => TtyNodeSummary {
            status: "stopped".to_string(),
            detail: Some(format!("app root: {}", app_root.display())),
        },
        Ok(LifecycleStatus::Running { metadata }) => {
            let mut detail = Vec::new();
            if let Some(pid) = metadata.pid {
                detail.push(format!("pid {pid}"));
            }
            if let Some(bind) = metadata.bind {
                detail.push(format!("http://{bind}"));
            }
            TtyNodeSummary {
                status: "running".to_string(),
                detail: (!detail.is_empty()).then(|| detail.join(" · ")),
            }
        }
        Ok(LifecycleStatus::Stale { diagnostics, .. }) => TtyNodeSummary {
            status: "stale".to_string(),
            detail: Some(diagnostics.message),
        },
        Ok(LifecycleStatus::Unresponsive { diagnostics, .. }) => TtyNodeSummary {
            status: "busy".to_string(),
            detail: Some(diagnostics.message),
        },
        Ok(LifecycleStatus::Starting { pid, started_at, .. }) => TtyNodeSummary {
            status: "starting".to_string(),
            detail: Some(format!("pid {pid} · since {started_at}")),
        },
        Err(err) => TtyNodeSummary {
            status: "status error".to_string(),
            detail: Some(err.to_string()),
        },
    }
}

fn screen_items(
    screen: TtyScreen,
    has_tui_command: bool,
    verified_items: Vec<TtyItem>,
) -> Vec<TtyItem> {
    let mut items = match screen {
        TtyScreen::Home => home_items(),
        TtyScreen::Help => help_items(verified_items),
    };
    if has_tui_command {
        items.insert(
            0,
            command_item(vec!["tui".to_string()], "open terminal workspace".to_string()),
        );
    }
    items
}

fn home_items() -> Vec<TtyItem> {
    vec![
        command_item(
            vec!["help".to_string()],
            "open the compact TTY help screen".to_string(),
        ),
        command_item(
            vec!["node".to_string(), "status".to_string()],
            "show local node lifecycle status".to_string(),
        ),
        command_item(
            vec!["node".to_string(), "doctor".to_string()],
            "diagnose local node startup and config".to_string(),
        ),
    ]
}

fn help_items(verified_items: Vec<TtyItem>) -> Vec<TtyItem> {
    let mut items = crate::lifecycle_commands::local_command_descriptors()
        .iter()
        .map(|command| {
            command_item(
                command.tokens.iter().map(|token| (*token).to_string()).collect(),
                command.summary.to_string(),
            )
        })
        .collect::<Vec<_>>();
    items.extend(verified_items);
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items.dedup_by(|a, b| a.label == b.label);
    items
}

fn command_item(tokens: Vec<String>, detail: String) -> TtyItem {
    TtyItem {
        label: tokens.join(" "),
        detail: Some(detail),
    }
}

fn loading_command_detail(screen: TtyScreen) -> &'static str {
    match screen {
        TtyScreen::Home => "loading verified command snapshot",
        TtyScreen::Help => "loading TTY help and verified command snapshot",
    }
}

#[derive(Debug, Clone)]
struct TtyHomeFile {
    screen: TtyScreen,
    sections: TtyHomeSections,
}

#[derive(Debug, Clone)]
struct TtyHomeSections {
    node: TtyNodeSummary,
    project: Option<TtyProjectSummary>,
    commands: TtyCommandSummary,
    items: Vec<TtyItem>,
}

#[derive(Debug, Clone)]
struct TtyNodeSummary {
    status: String,
    detail: Option<String>,
}

#[derive(Debug, Clone)]
struct TtyProjectSummary {
    label: String,
    root: Option<String>,
    detail: Option<String>,
}

impl TtyProjectSummary {
    fn from_project(project: &ProjectDisplay) -> Self {
        Self {
            label: project.label.clone(),
            root: project.key.clone(),
            detail: project.detail.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct TtyCommandSummary {
    count: Option<usize>,
    detail: Option<String>,
}

#[derive(Debug, Clone)]
struct TtyItem {
    label: String,
    detail: Option<String>,
}

pub fn render_command_loading(command: &str, route: &str) -> io::Result<usize> {
    let lines = command_frame_lines(CommandFrame {
        title: "RYE OS COMMAND",
        phase: "loading",
        command,
        status: "contacting daemon",
        detail: Some(route),
        payload: &[],
    });
    render_command_frame(&lines, 0)
}

pub fn render_command_result(
    command: &str,
    payload: &Value,
    previous_lines: usize,
) -> io::Result<usize> {
    let result = payload.get("result").unwrap_or(payload);
    let mut rows = Vec::new();
    if let Some(execution) = payload.get("execution") {
        if let Some(source) = execution.get("source_root").and_then(Value::as_str) {
            rows.push(("source_root".to_string(), source.to_string()));
        }
        if let Some(state) = execution.get("state_root").and_then(Value::as_str) {
            rows.push(("state_root".to_string(), state.to_string()));
        }
    }
    if let Some(outcome) = result.get("outcome_code").and_then(Value::as_str) {
        rows.push(("outcome".to_string(), outcome.to_string()));
    }
    if let Some(error) = result.get("error").filter(|value| !value.is_null()) {
        rows.push(("error".to_string(), value_summary(error)));
    }
    if let Some(artifacts) = result.get("artifacts").and_then(Value::as_array) {
        rows.push(("artifacts".to_string(), artifacts.len().to_string()));
    }

    let display_value = result.get("result").unwrap_or(result);
    append_value_rows("result", display_value, &mut rows);
    if rows.is_empty() {
        rows.push(("result".to_string(), value_summary(display_value)));
    }

    let status = if result_indicates_failure(result) {
        "error"
    } else {
        "complete"
    };
    let row_refs = rows
        .iter()
        .map(|(label, value)| (label.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    let lines = command_frame_lines(CommandFrame {
        title: "RYE OS COMMAND",
        phase: "live",
        command,
        status,
        detail: None,
        payload: &row_refs,
    });
    render_command_frame(&lines, previous_lines)
}

/// Minimum gap between repaints for non-status-changing events, so a fast
/// stream of ordinary events doesn't thrash the terminal with one reflow
/// per event. Status-changing events (started, error, terminal) always
/// repaint regardless of this gap.
const REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

pub struct TtyStreamPresenter {
    command: String,
    previous_lines: usize,
    thread_id: Option<String>,
    status: String,
    events: Vec<(String, String)>,
    last_render: Option<std::time::Instant>,
}

impl TtyStreamPresenter {
    pub fn new(command: impl Into<String>) -> io::Result<Self> {
        Self::with_previous(command, 0)
    }

    pub fn with_previous(command: impl Into<String>, previous_lines: usize) -> io::Result<Self> {
        let mut presenter = Self {
            command: command.into(),
            previous_lines,
            thread_id: None,
            status: "opening stream".to_string(),
            events: Vec::new(),
            last_render: None,
        };
        presenter.render()?;
        Ok(presenter)
    }

    pub fn render_event(&mut self, ev: &SseEvent) -> io::Result<StreamOutcome> {
        let data: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
        let inner = data.get("payload").unwrap_or(&data);
        match ev.event.as_str() {
            "stream_started" => {
                self.status = "streaming".to_string();
                self.thread_id = data
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .map(String::from);
                let thread_id = self.thread_id.clone().unwrap_or_default();
                self.push_event("stream_started", &thread_id);
                self.render()?;
                Ok(StreamOutcome::Continue)
            }
            "stream_error" => {
                let code = data
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("stream_error");
                let msg = data.get("error").and_then(Value::as_str).unwrap_or("");
                let detail = format!("{code}: {msg}");
                self.status = "error".to_string();
                self.push_event("stream_error", &detail);
                self.render()?;
                Ok(StreamOutcome::Failed(detail))
            }
            event => match thread_terminal_outcome(event) {
                Some(ThreadOutcomeKind::Success) => {
                    self.status = "complete".to_string();
                    self.push_event(event, &value_summary(inner));
                    self.render()?;
                    Ok(StreamOutcome::Done)
                }
                Some(ThreadOutcomeKind::Failure) => {
                    let detail = stream_failure_reason(inner, event);
                    self.status = "error".to_string();
                    self.push_event(event, &detail);
                    self.render()?;
                    Ok(StreamOutcome::Failed(detail))
                }
                None => {
                    self.push_event(event, &value_summary(inner));
                    self.render_if_due()?;
                    Ok(StreamOutcome::Continue)
                }
            },
        }
    }

    fn push_event(&mut self, event: &str, detail: &str) {
        self.events.push((event.to_string(), detail.to_string()));
        if self.events.len() > 10 {
            let excess = self.events.len() - 10;
            self.events.drain(0..excess);
        }
    }

    /// Repaint only if `REPAINT_INTERVAL` has elapsed since the last paint.
    /// State is already updated via `push_event`; the next allowed repaint
    /// shows it.
    fn render_if_due(&mut self) -> io::Result<()> {
        let due = match self.last_render {
            Some(at) => at.elapsed() >= REPAINT_INTERVAL,
            None => true,
        };
        if due {
            self.render()?;
        }
        Ok(())
    }

    fn render(&mut self) -> io::Result<()> {
        let mut owned = Vec::new();
        if let Some(thread_id) = &self.thread_id {
            owned.push(("thread".to_string(), thread_id.clone()));
        }
        for (event, detail) in &self.events {
            owned.push((event.clone(), detail.clone()));
        }
        let refs = owned
            .iter()
            .map(|(label, value)| (label.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        let lines = command_frame_lines(CommandFrame {
            title: "RYE OS STREAM",
            phase: "live",
            command: &self.command,
            status: &self.status,
            detail: None,
            payload: &refs,
        });
        self.previous_lines = render_command_frame(&lines, self.previous_lines)?;
        self.last_render = Some(std::time::Instant::now());
        Ok(())
    }
}

struct CommandFrame<'a> {
    title: &'static str,
    phase: &'static str,
    command: &'a str,
    status: &'a str,
    detail: Option<&'a str>,
    payload: &'a [(&'a str, &'a str)],
}

fn command_frame_lines(frame: CommandFrame<'_>) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(frame.title.to_string());
    lines.push(format!("{:<9} {}", "phase", frame.phase));
    lines.push(format!("{:<9} {}", "command", empty_dash(frame.command)));
    lines.push(format!("{:<9} {}", "status", frame.status));
    if let Some(detail) = frame.detail {
        lines.push(format!("{:<9} {}", "detail", detail));
    }
    if !frame.payload.is_empty() {
        lines.push(String::new());
        for (label, value) in frame.payload.iter().take(16) {
            lines.push(format!("{:<13} {}", label, value));
        }
    }
    lines.push(String::new());
    lines
}

fn render_command_frame(lines: &[String], previous_lines: usize) -> io::Result<usize> {
    let width = terminal_width();
    let lines = lines
        .iter()
        .map(|line| clamp_line(line, width))
        .collect::<Vec<_>>();
    write_frame(&mut io::stdout(), &lines, previous_lines)?;
    Ok(lines.len())
}

fn append_value_rows(prefix: &str, value: &Value, rows: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter().take(12) {
                rows.push((format!("{prefix}.{key}"), value_summary(value)));
            }
        }
        Value::Array(values) => {
            rows.push((prefix.to_string(), format!("{} item(s)", values.len())));
            for (idx, value) in values.iter().take(8).enumerate() {
                rows.push((format!("{prefix}[{idx}]"), value_summary(value)));
            }
        }
        _ => rows.push((prefix.to_string(), value_summary(value))),
    }
}

fn value_summary(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => value.clone(),
        Value::Array(values) => format!("{} item(s)", values.len()),
        Value::Object(map) => {
            let mut fields = map
                .iter()
                .filter_map(|(key, value)| {
                    scalar_summary(value).map(|value| format!("{key}={value}"))
                })
                .take(4)
                .collect::<Vec<_>>();
            if fields.is_empty() {
                fields.push(format!("{} field(s)", map.len()));
            }
            fields.join(" · ")
        }
    }
}

/// `result` is already unwrapped from any `payload`/envelope wrapper by the
/// caller. Failure is a non-null `error` field, or an `outcome_code` the
/// shared vocabulary (`ryeos_state::event_types`) names as a failure.
fn result_indicates_failure(result: &Value) -> bool {
    if result.get("error").is_some_and(|error| !error.is_null()) {
        return true;
    }
    result
        .get("outcome_code")
        .and_then(Value::as_str)
        .is_some_and(outcome_code_is_failure)
}

fn scalar_summary(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("null".to_string()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::String(value) => Some(value.clone()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn empty_dash(value: &str) -> &str {
    if value.trim().is_empty() {
        "-"
    } else {
        value
    }
}

fn stream_failure_reason(payload: &Value, fallback: &str) -> String {
    if let Some(error) = payload.get("error").and_then(Value::as_str) {
        return error.to_string();
    }
    if let Some(error) = payload.get("error").filter(|value| !value.is_null()) {
        return value_summary(error);
    }
    payload
        .get("outcome_code")
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

fn render(home: &TtyHomeFile, previous_lines: usize) -> io::Result<usize> {
    let width = terminal_width();
    let lines = render_lines(home)
        .into_iter()
        .map(|line| clamp_line(&line, width))
        .collect::<Vec<_>>();
    write_frame(&mut io::stdout(), &lines, previous_lines)?;
    Ok(lines.len())
}

fn render_lines(home: &TtyHomeFile) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(match home.screen {
        TtyScreen::Home => "RYE OS".to_string(),
        TtyScreen::Help => "RYE OS HELP".to_string(),
    });
    lines.push(match home.screen {
        TtyScreen::Home => "portable verified execution".to_string(),
        TtyScreen::Help => "verified command surface".to_string(),
    });
    lines.push(String::new());
    lines.push(format!(
        "{:<9} {}{}",
        "node",
        home.sections.node.status,
        detail_suffix(home.sections.node.detail.as_deref())
    ));
    if let Some(project) = &home.sections.project {
        lines.push(format!(
            "{:<9} {}{}",
            "project",
            project.label,
            detail_suffix(project.detail.as_deref().or(project.root.as_deref()))
        ));
    }
    let command_label = home
        .sections
        .commands
        .count
        .map(|count| format!("{count} available"))
        .unwrap_or_else(|| "unavailable".to_string());
    lines.push(format!(
        "{:<9} {}{}",
        "commands",
        command_label,
        detail_suffix(home.sections.commands.detail.as_deref())
    ));
    lines.push(String::new());
    match home.screen {
        TtyScreen::Home => render_home_items(&home.sections.items, &mut lines),
        TtyScreen::Help => render_help_items(&home.sections.items, &mut lines),
    }
    lines.push(String::new());
    lines
}

fn render_home_items(items: &[TtyItem], lines: &mut Vec<String>) {
    lines.push("items".to_string());
    for item in items {
        lines.push(format!(
            "  {:<24} {}",
            item.label,
            item.detail.as_deref().unwrap_or_default()
        ));
    }
}

fn render_help_items(items: &[TtyItem], lines: &mut Vec<String>) {
    lines.push("commands".to_string());
    let visible = items.iter().take(24).collect::<Vec<_>>();
    if visible.is_empty() {
        lines.push("  no commands available".to_string());
    } else {
        for item in visible {
            lines.push(format!(
                "  {:<28} {}",
                item.label,
                item.detail.as_deref().unwrap_or_default()
            ));
        }
        let remaining = items.len().saturating_sub(24);
        if remaining > 0 {
            lines.push(format!(
                "  ... {remaining} more · use `ryeos commands` for the full reference"
            ));
        }
    }
}

fn write_frame(
    out: &mut impl Write,
    lines: &[String],
    previous_lines: usize,
) -> io::Result<()> {
    if previous_lines == 0 {
        for line in lines {
            writeln!(out, "{line}")?;
        }
    } else {
        write!(out, "\x1b[{previous_lines}F")?;
        let line_count = lines.len().max(previous_lines);
        for idx in 0..line_count {
            write!(out, "\x1b[2K")?;
            if let Some(line) = lines.get(idx) {
                write!(out, "{line}")?;
            }
            writeln!(out)?;
        }
    }
    out.flush()
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|width| *width >= 20)
        .unwrap_or(DEFAULT_TERMINAL_WIDTH)
}

fn clamp_line(line: &str, width: usize) -> String {
    let mut out = String::new();
    let max_chars = width.saturating_sub(1).max(1);
    let mut chars = line.chars();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return line.to_string();
        };
        out.push(ch);
    }
    if chars.next().is_some() {
        if max_chars > 1 {
            out.pop();
        }
        out.push('…');
        out
    } else {
        line.to_string()
    }
}

fn detail_suffix(detail: Option<&str>) -> String {
    detail
        .filter(|value| !value.trim().is_empty())
        .map(|value| format!(" · {value}"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_failure_detection_uses_error_and_outcome_code() {
        assert!(result_indicates_failure(&serde_json::json!({
            "error": "boom"
        })));
        assert!(result_indicates_failure(&serde_json::json!({
            "outcome_code": "exit:1"
        })));
        assert!(!result_indicates_failure(&serde_json::json!({
            "outcome_code": "success"
        })));
        assert!(!result_indicates_failure(&serde_json::json!({})));
    }
}
