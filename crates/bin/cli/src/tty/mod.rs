//! Rye OS CLI TTY presentation.
//!
//! This is a normal stdout renderer, not the full TUI: every render reads
//! live local state and prints directly, with no on-disk projection.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ryeos_node::{LifecycleController, LifecycleStatus, LocalLifecycleEnv};
use ryeos_state::event_types::{
    outcome_code_is_failure, thread_terminal_outcome, ThreadOutcomeKind,
};
use serde_json::Value;
use unicode_width::UnicodeWidthChar;

use crate::error::{CliError, CliTransportError};
use crate::exec_stream::StreamOutcome;
use crate::transport::http::SseEvent;
use crate::transport::signing::Signer;

mod capabilities;
mod diagnostic;
mod document;
pub(crate) mod help_flow;
pub(crate) mod interaction;
pub(crate) mod onboarding_flow;
pub(crate) mod onboarding_journal;
pub(crate) mod onboarding_spec;
mod progress;
mod result;
mod theme;
pub use capabilities::TerminalCapabilities;
pub use diagnostic::{Diagnostic, DiagnosticLevel};
pub use document::{Document, Hint, Row, Section, StatusBanner};
pub use progress::{
    LifecycleProgress, LifecycleProgressAction, OfflineGcProgress, OperationKind, OperationProgress,
};
pub use result::{write_json, write_machine_diagnostics, write_raw};
pub use theme::Tone;

pub(crate) fn sanitize_terminal_inline(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Console {
    capabilities: TerminalCapabilities,
}

impl Console {
    pub fn new(capabilities: TerminalCapabilities) -> Self {
        Self { capabilities }
    }

    pub fn detect(force_plain: bool) -> Self {
        Self::new(TerminalCapabilities::detect(force_plain))
    }

    pub fn capabilities(&self) -> TerminalCapabilities {
        self.capabilities
    }

    pub fn document(&self, document: &Document) -> io::Result<()> {
        let lines = self.document_lines(document);
        self.write_stdout(&lines)
    }

    pub fn text(&self, value: &str) -> io::Result<()> {
        let lines = value.lines().map(str::to_owned).collect::<Vec<_>>();
        self.write_stdout(&lines)
    }

    pub fn stream_fragment(&self, value: &str) -> io::Result<()> {
        let mut out = io::stdout().lock();
        write!(out, "{value}")?;
        out.flush()
    }

    pub fn success(&self, status: &StatusBanner) -> io::Result<()> {
        self.status(status)
    }

    pub fn status(&self, status: &StatusBanner) -> io::Result<()> {
        let mut lines = Vec::new();
        if self.capabilities.tty() {
            let glyph = theme::style(
                theme::glyph(status.tone, self.capabilities.unicode),
                status.tone,
                self.capabilities.color,
            );
            let heading = theme::style(&status.heading, status.tone, self.capabilities.color);
            let detail = status
                .detail
                .as_deref()
                .map(|value| {
                    theme::style(
                        &format!("  ·  {value}"),
                        Tone::Secondary,
                        self.capabilities.color,
                    )
                })
                .unwrap_or_default();
            lines.push(format!("{glyph}  RYEOS  {heading}{detail}"));
        } else {
            let detail = status
                .detail
                .as_deref()
                .map(|value| format!(": {value}"))
                .unwrap_or_default();
            lines.push(format!("{}{}", status.heading.to_ascii_lowercase(), detail));
        }
        append_rows(&mut lines, &status.rows, self.capabilities);
        self.write_stdout(&lines)
    }

    pub fn warning(&self, diagnostic: &Diagnostic) -> io::Result<()> {
        self.diagnostic(diagnostic)
    }

    pub fn info(&self, diagnostic: &Diagnostic) -> io::Result<()> {
        self.diagnostic(diagnostic)
    }

    pub fn error(&self, diagnostic: &Diagnostic) -> io::Result<()> {
        self.diagnostic(diagnostic)
    }

    pub fn progress(
        &self,
        operation: OperationKind,
        label: &str,
    ) -> io::Result<Option<OperationProgress>> {
        OperationProgress::new(operation, label, self.capabilities)
    }

    pub fn diagnostic(&self, diagnostic: &Diagnostic) -> io::Result<()> {
        let mut lines = Vec::new();
        if self.capabilities.tty() {
            let tone = diagnostic.level.tone();
            let glyph = theme::style(
                theme::glyph(tone, self.capabilities.unicode),
                tone,
                self.capabilities.color,
            );
            let heading = diagnostic
                .heading
                .as_deref()
                .unwrap_or(match diagnostic.level {
                    DiagnosticLevel::Info => "INFO",
                    DiagnosticLevel::Warning => "WARNING",
                    DiagnosticLevel::Error => "COMMAND FAILED",
                });
            let heading = theme::style(heading, tone, self.capabilities.color);
            lines.push(format!("{glyph}  RYEOS  {heading}"));
            lines.extend(
                wrap_words(
                    &diagnostic.message,
                    self.capabilities.width.saturating_sub(4).max(8),
                )
                .into_iter()
                .map(|value| {
                    format!(
                        "   {}",
                        theme::style(&value, Tone::Neutral, self.capabilities.color)
                    )
                }),
            );
            for value in &diagnostic.context {
                lines.extend(
                    wrap_words(value, self.capabilities.width.saturating_sub(4).max(8))
                        .into_iter()
                        .map(|value| {
                            format!(
                                "   {}",
                                theme::style(&value, Tone::Secondary, self.capabilities.color,)
                            )
                        }),
                );
            }
            if let Some(hint) = &diagnostic.hint {
                lines.push(String::new());
                let prefix = "   hint  ";
                let available = self
                    .capabilities
                    .width
                    .saturating_sub(visible_width(prefix) + 1)
                    .max(8);
                for (index, value) in wrap_words(&hint.0, available).into_iter().enumerate() {
                    let value = theme::style(&value, Tone::Secondary, self.capabilities.color);
                    lines.push(if index == 0 {
                        format!("{prefix}{value}")
                    } else {
                        format!("{}{value}", " ".repeat(visible_width(prefix)))
                    });
                }
            }
        } else {
            let prefix = match diagnostic.level {
                DiagnosticLevel::Info => "ryeos: info: ",
                DiagnosticLevel::Warning => "ryeos: warning: ",
                DiagnosticLevel::Error => "ryeos: ",
            };
            lines.push(format!("{prefix}{}", diagnostic.message));
            lines.extend(
                diagnostic
                    .context
                    .iter()
                    .map(|value| format!("ryeos: {value}")),
            );
            if let Some(hint) = &diagnostic.hint {
                lines.push(format!("ryeos: hint: {}", hint.0));
            }
        }
        self.write_stderr(&lines)
    }

    fn document_lines(&self, document: &Document) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(title) = &document.title {
            lines.push(if self.capabilities.tty() {
                theme::style(
                    &format!("RYEOS  {title}"),
                    Tone::Neutral,
                    self.capabilities.color,
                )
            } else {
                title.clone()
            });
        }
        for section in &document.sections {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            if let Some(heading) = &section.heading {
                lines.push(if self.capabilities.tty() {
                    theme::style(
                        &heading.to_ascii_lowercase(),
                        Tone::Neutral,
                        self.capabilities.color,
                    )
                } else {
                    heading.to_ascii_uppercase()
                });
            }
            append_rows(&mut lines, &section.rows, self.capabilities);
        }
        if !document.hints.is_empty() {
            lines.push(String::new());
            for hint in &document.hints {
                if !self.capabilities.tty() {
                    lines.push(hint.0.clone());
                    continue;
                }
                let prefix = "hint  ";
                let available = self
                    .capabilities
                    .width
                    .saturating_sub(visible_width(prefix) + 1)
                    .max(8);
                for (index, value) in wrap_words(&hint.0, available).into_iter().enumerate() {
                    let line = if index == 0 {
                        format!("{prefix}{value}")
                    } else {
                        format!("{}{value}", " ".repeat(visible_width(prefix)))
                    };
                    lines.push(theme::style(
                        &line,
                        Tone::Secondary,
                        self.capabilities.color,
                    ));
                }
            }
        }
        lines
    }

    fn write_stdout(&self, lines: &[String]) -> io::Result<()> {
        let mut out = io::stdout().lock();
        write_lines(
            &mut out,
            lines,
            self.capabilities.tty().then_some(self.capabilities.width),
        )
    }

    fn write_stderr(&self, lines: &[String]) -> io::Result<()> {
        let mut out = io::stderr().lock();
        write_lines(
            &mut out,
            lines,
            self.capabilities.tty().then_some(self.capabilities.width),
        )
    }
}

fn append_rows(lines: &mut Vec<String>, rows: &[Row], capabilities: TerminalCapabilities) {
    let key_width = rows
        .iter()
        .filter_map(|row| row.key.as_ref().map(|key| visible_width(key)))
        .max()
        .unwrap_or(0)
        .min(24);
    for row in rows {
        match &row.key {
            Some(key) if capabilities.tty() => {
                if visible_width(key) > key_width {
                    lines.push(format!(
                        "  {}",
                        theme::style(key, Tone::Secondary, capabilities.color)
                    ));
                    for value in wrap_words(&row.value, capabilities.width.saturating_sub(5).max(8))
                    {
                        lines.push(format!(
                            "    {}",
                            theme::style(&value, row.tone, capabilities.color)
                        ));
                    }
                    continue;
                }
                let marker = if let Some(marker) = &row.marker {
                    theme::style(marker, row.tone, capabilities.color)
                } else if row.tone == Tone::Neutral {
                    " ".to_string()
                } else {
                    theme::style(
                        theme::glyph(row.tone, capabilities.unicode),
                        row.tone,
                        capabilities.color,
                    )
                };
                let key = theme::style(
                    &format!("{key:key_width$}"),
                    Tone::Secondary,
                    capabilities.color,
                );
                let prefix = format!("{marker} {key}  ");
                let available = capabilities
                    .width
                    .saturating_sub(visible_width(&prefix) + 1)
                    .max(8);
                for (index, value) in wrap_words(&row.value, available).into_iter().enumerate() {
                    let value = theme::style(&value, row.tone, capabilities.color);
                    lines.push(if index == 0 {
                        format!("{prefix}{value}")
                    } else {
                        format!("{}{value}", " ".repeat(visible_width(&prefix)))
                    });
                }
            }
            Some(key) => {
                let prefix = format!("{key}: ");
                let available = capabilities
                    .width
                    .saturating_sub(visible_width(&prefix) + 1)
                    .max(8);
                for (index, value) in wrap_words(&row.value, available).into_iter().enumerate() {
                    lines.push(if index == 0 {
                        format!("{prefix}{value}")
                    } else {
                        format!("{}{value}", " ".repeat(visible_width(&prefix)))
                    });
                }
            }
            None if capabilities.tty() => {
                for value in wrap_words(&row.value, capabilities.width.saturating_sub(3).max(8)) {
                    let value = theme::style(&value, row.tone, capabilities.color);
                    lines.push(format!("  {value}"));
                }
            }
            None => lines.extend(wrap_words(
                &row.value,
                capabilities.width.saturating_sub(1).max(8),
            )),
        }
    }
}

fn wrap_words(value: &str, width: usize) -> Vec<String> {
    if value.is_empty() {
        return vec![String::new()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for mut word in value.split_whitespace() {
        let separator = usize::from(!line.is_empty());
        if visible_width(&line) + separator + visible_width(word) <= width {
            if separator == 1 {
                line.push(' ');
            }
            line.push_str(word);
        } else {
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            while visible_width(word) > width {
                let split = visible_prefix_end(word, width);
                lines.push(word[..split].to_string());
                word = &word[split..];
            }
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

fn visible_prefix_end(value: &str, max_width: usize) -> usize {
    let mut width = 0;
    let mut end = 0;
    for (index, ch) in value.char_indices() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > max_width {
            break;
        }
        width += ch_width;
        end = index + ch.len_utf8();
    }
    end.max(value.chars().next().map(char::len_utf8).unwrap_or(0))
}

fn write_lines(
    out: &mut impl Write,
    lines: &[String],
    terminal_width: Option<usize>,
) -> io::Result<()> {
    for line in lines {
        if let Some(width) = terminal_width {
            for wrapped in wrap_visible_preserving(line, width.saturating_sub(1).max(1)) {
                writeln!(out, "{wrapped}")?;
            }
        } else {
            writeln!(out, "{line}")?;
        }
    }
    out.flush()
}

fn wrap_visible_preserving(value: &str, max_width: usize) -> Vec<&str> {
    if value.is_empty() {
        return vec![value];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    let mut width = 0;
    let mut escape = 0_u8;
    for (index, ch) in value.char_indices() {
        let ch_width = match escape {
            1 if ch == '[' => {
                escape = 2;
                0
            }
            1 => {
                escape = 0;
                0
            }
            2 if ('@'..='~').contains(&ch) => {
                escape = 0;
                0
            }
            2 => 0,
            _ if ch == '\x1b' => {
                escape = 1;
                0
            }
            _ => ch.width().unwrap_or(0),
        };
        if ch_width > 0 && width + ch_width > max_width {
            lines.push(&value[start..index]);
            start = index;
            width = 0;
        }
        width += ch_width;
    }
    lines.push(&value[start..]);
    lines
}

pub(crate) fn visible_width(value: &str) -> usize {
    let mut width = 0;
    let mut escape = 0_u8;
    for ch in value.chars() {
        match escape {
            1 if ch == '[' => escape = 2,
            1 => escape = 0,
            2 if ('@'..='~').contains(&ch) => escape = 0,
            2 => {}
            _ if ch == '\x1b' => escape = 1,
            _ => width += ch.width().unwrap_or(0),
        }
    }
    width
}

pub(crate) fn clamp_visible(value: &str, max_width: usize) -> String {
    if visible_width(value) <= max_width {
        return value.to_string();
    }
    let ellipsis = if value.is_ascii() {
        ".".repeat(max_width.min(3))
    } else if max_width == 0 {
        String::new()
    } else {
        "…".to_string()
    };
    let ellipsis_width = visible_width(&ellipsis);
    let target = max_width.saturating_sub(ellipsis_width);
    let mut out = String::new();
    let mut width = 0;
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            out.push(ch);
            if let Some(next) = chars.next() {
                out.push(next);
                if next == '[' {
                    for parameter in chars.by_ref() {
                        out.push(parameter);
                        if ('@'..='~').contains(&parameter) {
                            break;
                        }
                    }
                }
            }
            continue;
        }
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > target {
            break;
        }
        width += ch_width;
        out.push(ch);
    }
    out.push_str(&ellipsis);
    if value.contains('\x1b') {
        out.push_str("\x1b[0m");
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyScreen {
    Home,
    Help,
}

pub async fn run(
    console: &Console,
    app_root: &Path,
    explicit_project: Option<&Path>,
    screen: TtyScreen,
) -> std::result::Result<(), CliError> {
    let project = resolve_project_for_display(explicit_project);
    let remote_url = remote_daemon_url();

    let mut loading_frame = 0;
    let mut rendered_lines = render(
        console,
        &loading_projection(screen, &project, loading_frame),
        0,
    )?;
    let live = build_live_projection(app_root, &project, screen, remote_url.as_deref());
    tokio::pin!(live);
    let mut ticker = tokio::time::interval(theme::SPINNER_TICK_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    let live = loop {
        tokio::select! {
            live = &mut live => break live,
            _ = ticker.tick() => {
                loading_frame = loading_frame.wrapping_add(1);
                rendered_lines = render(
                    console,
                    &loading_projection(screen, &project, loading_frame),
                    rendered_lines,
                )?;
            }
        }
    };
    render(console, &live, rendered_lines)?;

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

fn loading_projection(
    screen: TtyScreen,
    project: &ProjectDisplay,
    loading_frame: usize,
) -> TtyHomeFile {
    TtyHomeFile {
        screen,
        loading_frame: Some(loading_frame),
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
    screen: TtyScreen,
    remote_url: Option<&str>,
) -> TtyHomeFile {
    // Keep all disk-heavy verification and effective-item resolution off the
    // async render task. Otherwise this future blocks the `select!` in `run`
    // and the loading spinner cannot repaint until discovery is already done.
    // Lifecycle probing and command discovery are independent, so start both
    // immediately and let their latency overlap.
    let lifecycle_root = app_root.to_path_buf();
    let remote_url = remote_url.map(str::to_owned);
    let node_task = tokio::spawn(async move {
        match remote_url {
            Some(remote_url) => TtyNodeSummary {
                status: "remote override".to_string(),
                detail: Some(format!("RYEOSD_URL={remote_url}")),
            },
            None => lifecycle_summary(lifecycle_root).await,
        }
    });

    let discovery_root = app_root.to_path_buf();
    let project_key = project.key.clone();
    let discovery_task = tokio::task::spawn_blocking(move || {
        build_command_projection(&discovery_root, project_key.as_deref())
    });

    let (node, commands) = tokio::join!(node_task, discovery_task);
    let mut node = node.unwrap_or_else(|error| TtyNodeSummary {
        status: "status error".to_string(),
        detail: Some(format!("lifecycle status task failed: {error}")),
    });
    let commands = commands.unwrap_or_else(|error| CommandProjection {
        command_count: Some(crate::lifecycle_commands::local_command_descriptors().len()),
        has_tui_command: false,
        verified_items: Vec::new(),
        operator_problem: Some(format!("command discovery task failed: {error}")),
    });

    if let Some(problem) = commands.operator_problem.as_deref() {
        node.detail = Some(match node.detail {
            Some(detail) => format!("{detail}; {problem}"),
            None => problem.to_string(),
        });
    }

    TtyHomeFile {
        screen,
        loading_frame: None,
        sections: TtyHomeSections {
            node,
            project: Some(TtyProjectSummary::from_project(project)),
            commands: TtyCommandSummary {
                count: commands.command_count,
                detail: commands
                    .command_count
                    .is_none()
                    .then(|| "run `ryeos node doctor` for diagnostics".to_string()),
            },
            items: screen_items(screen, commands.has_tui_command, commands.verified_items),
        },
    }
}

#[derive(Debug)]
struct CommandProjection {
    command_count: Option<usize>,
    has_tui_command: bool,
    verified_items: Vec<TtyItem>,
    operator_problem: Option<String>,
}

fn build_command_projection(app_root: &Path, project_key: Option<&str>) -> CommandProjection {
    let operator_problem = resolve_operator(app_root);
    match crate::node_descriptors::load_verified_snapshot(app_root) {
        Ok(snapshot) => {
            let rows = crate::help::command_rows(&snapshot, app_root, project_key.unwrap_or("."));
            CommandProjection {
                command_count: Some(
                    snapshot.commands.len()
                        + crate::lifecycle_commands::local_command_descriptors().len(),
                ),
                has_tui_command: rows.iter().any(|command| command.tokens == "tui"),
                verified_items: rows
                    .into_iter()
                    .map(|row| command_item(vec![row.tokens], row.description))
                    .collect(),
                operator_problem,
            }
        }
        Err(_) => CommandProjection {
            command_count: Some(crate::lifecycle_commands::local_command_descriptors().len()),
            has_tui_command: false,
            verified_items: Vec::new(),
            operator_problem,
        },
    }
}

async fn lifecycle_summary(app_root: PathBuf) -> TtyNodeSummary {
    let env =
        match tokio::task::spawn_blocking(move || LocalLifecycleEnv::load(Some(app_root))).await {
            Ok(Ok(env)) => env,
            Ok(Err(err)) => {
                return TtyNodeSummary {
                    status: "config error".to_string(),
                    detail: Some(err.to_string()),
                };
            }
            Err(err) => {
                return TtyNodeSummary {
                    status: "config error".to_string(),
                    detail: Some(format!("lifecycle config task failed: {err}")),
                };
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
        Ok(LifecycleStatus::Running { metadata, .. }) => {
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
            status: "unresponsive".to_string(),
            detail: Some(diagnostics.message),
        },
        Ok(LifecycleStatus::Starting {
            metadata, startup, ..
        }) => TtyNodeSummary {
            status: "starting".to_string(),
            detail: Some(format!(
                "pid {} · {} · {}ms elapsed",
                metadata.pid.unwrap_or_default(),
                startup.phase.as_str(),
                startup.elapsed_ms,
            )),
        },
        Ok(LifecycleStatus::Failed { metadata, startup }) => TtyNodeSummary {
            status: "failed".to_string(),
            detail: Some(format!(
                "pid {} · {} · {}",
                metadata.pid.unwrap_or_default(),
                startup.phase.as_str(),
                startup
                    .error
                    .as_deref()
                    .unwrap_or("unknown startup failure"),
            )),
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
            command_item(
                vec!["tui".to_string()],
                "open terminal workspace".to_string(),
            ),
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
                command
                    .tokens
                    .iter()
                    .map(|token| (*token).to_string())
                    .collect(),
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
    loading_frame: Option<usize>,
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

pub fn render_command_result(
    console: &Console,
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
    append_value_rows(
        "result",
        display_value,
        &mut rows,
        console.capabilities().width,
    );
    if rows.is_empty() {
        rows.push(("result".to_string(), value_summary(display_value)));
    }

    let thread_id = result
        .get("thread_id")
        .or_else(|| display_value.get("thread_id"))
        .and_then(Value::as_str);
    let detached = result
        .get("outcome_code")
        .and_then(Value::as_str)
        .is_some_and(|value| matches!(value, "accepted" | "detached"));
    if detached || thread_id.is_some() {
        if let Some(thread_id) = thread_id {
            rows.push(("thread".to_string(), thread_id.to_string()));
        }
        rows.push((
            "hint".to_string(),
            thread_id
                .map(|thread_id| format!("run `ryeos follow {thread_id}` to watch progress"))
                .unwrap_or_else(|| "use the returned thread ID to follow progress".to_string()),
        ));
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
    let lines = command_frame_lines(
        CommandFrame {
            title: "RYEOS COMMAND",
            phase: "live",
            command,
            status,
            detail: None,
            payload: &row_refs,
        },
        console.capabilities(),
    );
    render_command_frame(console, &lines, previous_lines)
}

pub(crate) fn structured_result_failure(payload: &Value) -> Option<String> {
    let result = payload.get("result").unwrap_or(payload);
    result_indicates_failure(result).then(|| stream_failure_reason(result, "command failed"))
}

/// Minimum gap between repaints for non-status-changing events, so a fast
/// stream of ordinary events doesn't thrash the terminal with one reflow
/// per event. Status-changing events (started, error, terminal) always
/// repaint regardless of this gap.
const REPAINT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

pub struct TtyStreamPresenter {
    console: Console,
    command: String,
    previous_lines: usize,
    thread_id: Option<String>,
    status: String,
    events: Vec<(String, String)>,
    last_render: Option<std::time::Instant>,
}

impl TtyStreamPresenter {
    pub fn new(console: Console, command: impl Into<String>) -> io::Result<Self> {
        Self::with_previous(console, command, 0)
    }

    pub fn with_previous(
        console: Console,
        command: impl Into<String>,
        previous_lines: usize,
    ) -> io::Result<Self> {
        let mut presenter = Self {
            console,
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
        let lines = command_frame_lines(
            CommandFrame {
                title: "RYEOS STREAM",
                phase: "live",
                command: &self.command,
                status: &self.status,
                detail: None,
                payload: &refs,
            },
            self.console.capabilities(),
        );
        self.previous_lines = render_command_frame(&self.console, &lines, self.previous_lines)?;
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

fn command_frame_lines(frame: CommandFrame<'_>, capabilities: TerminalCapabilities) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(theme::style(frame.title, Tone::Neutral, capabilities.color));
    append_frame_row(
        &mut lines,
        "phase",
        frame.phase,
        Tone::Secondary,
        capabilities,
    );
    append_frame_row(
        &mut lines,
        "command",
        empty_dash(frame.command),
        Tone::Neutral,
        capabilities,
    );
    append_frame_row(
        &mut lines,
        "status",
        frame.status,
        frame_status_tone(frame.status),
        capabilities,
    );
    if let Some(detail) = frame.detail {
        append_frame_row(&mut lines, "detail", detail, Tone::Secondary, capabilities);
    }
    if !frame.payload.is_empty() {
        lines.push(String::new());
        for (label, value) in frame.payload {
            let tone = match *label {
                "error" => Tone::Failure,
                "hint" => Tone::Secondary,
                _ => Tone::Neutral,
            };
            append_frame_row(&mut lines, label, value, tone, capabilities);
        }
    }
    lines.push(String::new());
    lines
}

fn append_frame_row(
    lines: &mut Vec<String>,
    label: &str,
    value: &str,
    tone: Tone,
    capabilities: TerminalCapabilities,
) {
    const KEY_WIDTH: usize = 13;
    let label = clamp_visible(label, KEY_WIDTH);
    let label = format!("{label:<width$}", width = KEY_WIDTH);
    let key = theme::style(&label, Tone::Secondary, capabilities.color);
    let prefix = format!("  {key} ");
    let available = capabilities
        .width
        .saturating_sub(visible_width(&prefix) + 1)
        .max(8);
    for (index, value) in wrap_words(value, available).into_iter().enumerate() {
        let value = theme::style(&value, tone, capabilities.color);
        lines.push(if index == 0 {
            format!("{prefix}{value}")
        } else {
            format!("{}{value}", " ".repeat(visible_width(&prefix)))
        });
    }
}

fn frame_status_tone(status: &str) -> Tone {
    match status {
        "complete" | "completed" | "success" => Tone::Success,
        "error" | "failed" | "cancelled" => Tone::Failure,
        "opening stream" | "streaming" | "running" => Tone::Active,
        _ => Tone::Neutral,
    }
}

fn render_command_frame(
    console: &Console,
    lines: &[String],
    previous_lines: usize,
) -> io::Result<usize> {
    let width = console.capabilities().width;
    let lines = lines
        .iter()
        .map(|line| clamp_visible(line, width.saturating_sub(1).max(1)))
        .collect::<Vec<_>>();
    write_frame(&mut io::stdout(), &lines, previous_lines)?;
    Ok(lines.len())
}

fn append_value_rows(prefix: &str, value: &Value, rows: &mut Vec<(String, String)>, width: usize) {
    match value {
        Value::Object(map) => {
            if map.is_empty() {
                rows.push((prefix.to_string(), "empty object".to_string()));
            } else if map.values().all(|value| scalar_summary(value).is_some()) {
                const FIELD_LIMIT: usize = 12;
                for (key, value) in map.iter().take(FIELD_LIMIT) {
                    rows.push((format!("{prefix}.{key}"), value_summary(value)));
                }
                if map.len() > FIELD_LIMIT {
                    rows.push((
                        prefix.to_string(),
                        format!("… {} more field(s)", map.len() - FIELD_LIMIT),
                    ));
                }
            } else {
                let pretty =
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
                const LINE_LIMIT: usize = 32;
                let pretty_lines = pretty.lines().collect::<Vec<_>>();
                for (index, line) in pretty_lines.iter().take(LINE_LIMIT).enumerate() {
                    rows.push((
                        if index == 0 {
                            prefix.to_string()
                        } else {
                            String::new()
                        },
                        (*line).to_string(),
                    ));
                }
                if pretty_lines.len() > LINE_LIMIT {
                    rows.push((
                        String::new(),
                        format!("… {} more line(s)", pretty_lines.len() - LINE_LIMIT),
                    ));
                }
            }
        }
        Value::Array(values) => {
            if values.is_empty() {
                rows.push((prefix.to_string(), "0 items · no results".to_string()));
                return;
            }
            rows.push((prefix.to_string(), format!("{} item(s)", values.len())));
            let columns = values
                .first()
                .and_then(Value::as_object)
                .map(|object| {
                    object
                        .iter()
                        .filter(|(_, value)| scalar_summary(value).is_some())
                        .map(|(key, _)| key.as_str())
                        .take(3)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let tabular = !columns.is_empty()
                && values.iter().all(|value| {
                    value.as_object().is_some_and(|object| {
                        columns
                            .iter()
                            .all(|column| object.get(*column).and_then(scalar_summary).is_some())
                    })
                });
            if tabular {
                let available = width.saturating_sub(18).max(12);
                let column_width = available
                    .saturating_sub((columns.len() - 1) * 3)
                    .checked_div(columns.len())
                    .unwrap_or(4)
                    .max(4);
                let format_cells = |cells: Vec<String>| {
                    cells
                        .into_iter()
                        .map(|cell| clamp_visible(&cell, column_width))
                        .collect::<Vec<_>>()
                        .join(" | ")
                };
                rows.push((
                    "".to_string(),
                    format_cells(columns.iter().map(|v| (*v).to_string()).collect()),
                ));
                const ITEM_LIMIT: usize = 8;
                for (index, value) in values.iter().take(ITEM_LIMIT).enumerate() {
                    let object = value.as_object().expect("validated table row");
                    let cells = columns
                        .iter()
                        .map(|column| value_summary(&object[*column]))
                        .collect();
                    rows.push((format!("[{index}]"), format_cells(cells)));
                }
                if values.len() > ITEM_LIMIT {
                    rows.push((
                        String::new(),
                        format!("… {} more item(s)", values.len() - ITEM_LIMIT),
                    ));
                }
            } else {
                const ITEM_LIMIT: usize = 8;
                for (idx, value) in values.iter().take(ITEM_LIMIT).enumerate() {
                    rows.push((format!("{prefix}[{idx}]"), value_summary(value)));
                }
                if values.len() > ITEM_LIMIT {
                    rows.push((
                        prefix.to_string(),
                        format!("… {} more item(s)", values.len() - ITEM_LIMIT),
                    ));
                }
            }
        }
        Value::Null => rows.push((prefix.to_string(), "no result".to_string())),
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

fn render(console: &Console, home: &TtyHomeFile, previous_lines: usize) -> io::Result<usize> {
    let capabilities = console.capabilities();
    let width = capabilities.width;
    let lines = render_lines(home, capabilities)
        .into_iter()
        .map(|line| clamp_visible(&line, width.saturating_sub(1).max(1)))
        .collect::<Vec<_>>();
    write_frame(&mut io::stdout(), &lines, previous_lines)?;
    Ok(lines.len())
}

fn render_lines(home: &TtyHomeFile, capabilities: TerminalCapabilities) -> Vec<String> {
    let mut lines = Vec::new();
    let title = match home.screen {
        TtyScreen::Home => "RYEOS".to_string(),
        TtyScreen::Help => "RYEOS HELP".to_string(),
    };
    lines.push(theme::style(&title, Tone::Neutral, capabilities.color));
    let subtitle = match home.screen {
        TtyScreen::Home => "portable verified execution".to_string(),
        TtyScreen::Help => "verified command surface".to_string(),
    };
    lines.push(theme::style(&subtitle, Tone::Secondary, capabilities.color));
    lines.push(String::new());
    let mut node_row = Row::key_value(
        "node",
        format!(
            "{}{}",
            home.sections.node.status,
            detail_suffix(home.sections.node.detail.as_deref())
        ),
    )
    .with_tone(node_status_tone(&home.sections.node.status));
    if let Some(frame) = home.loading_frame {
        node_row = node_row.with_marker(theme::spinner(frame, capabilities.unicode));
    }
    let mut summary = vec![node_row];
    if let Some(project) = &home.sections.project {
        summary.push(Row::key_value(
            "project",
            format!(
                "{}{}",
                project.label,
                detail_suffix(project.detail.as_deref().or(project.root.as_deref()))
            ),
        ));
    }
    let command_label = home
        .sections
        .commands
        .count
        .map(|count| format!("{count} available"))
        .unwrap_or_else(|| "unavailable".to_string());
    summary.push(Row::key_value(
        "commands",
        format!(
            "{}{}",
            command_label,
            detail_suffix(home.sections.commands.detail.as_deref())
        ),
    ));
    append_rows(&mut lines, &summary, capabilities);
    lines.push(String::new());
    match home.screen {
        TtyScreen::Home => render_home_items(&home.sections.items, &mut lines, capabilities),
        TtyScreen::Help => render_help_items(&home.sections.items, &mut lines, capabilities),
    }
    lines.push(String::new());
    lines
}

fn node_status_tone(status: &str) -> Tone {
    match status {
        "running" => Tone::Success,
        "loading" | "starting" | "remote override" => Tone::Active,
        "stale" | "unresponsive" | "not initialized" => Tone::Warning,
        "failed" | "config error" | "status error" => Tone::Failure,
        _ => Tone::Neutral,
    }
}

fn render_home_items(
    items: &[TtyItem],
    lines: &mut Vec<String>,
    capabilities: TerminalCapabilities,
) {
    lines.push(theme::style("items", Tone::Neutral, capabilities.color));
    let rows = items
        .iter()
        .map(|item| Row::key_value(&item.label, item.detail.as_deref().unwrap_or_default()))
        .collect::<Vec<_>>();
    append_rows(lines, &rows, capabilities);
}

fn render_help_items(
    items: &[TtyItem],
    lines: &mut Vec<String>,
    capabilities: TerminalCapabilities,
) {
    lines.push(theme::style("commands", Tone::Neutral, capabilities.color));
    let visible = items.iter().take(24).collect::<Vec<_>>();
    if visible.is_empty() {
        append_rows(lines, &[Row::text("no commands available")], capabilities);
    } else {
        let rows = visible
            .into_iter()
            .map(|item| Row::key_value(&item.label, item.detail.as_deref().unwrap_or_default()))
            .collect::<Vec<_>>();
        append_rows(lines, &rows, capabilities);
        let remaining = items.len().saturating_sub(24);
        if remaining > 0 {
            append_rows(
                lines,
                &[Row::text(format!(
                    "… {remaining} more · use `ryeos commands` for the full reference"
                ))
                .with_tone(Tone::Secondary)],
                capabilities,
            );
        }
    }
}

fn write_frame(out: &mut impl Write, lines: &[String], previous_lines: usize) -> io::Result<()> {
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

    #[test]
    fn large_results_report_omitted_items_without_hiding_following_rows() {
        let value = Value::Array((0..10).map(Value::from).collect());
        let mut rows = Vec::new();
        append_value_rows("result", &value, &mut rows, 80);
        assert!(rows.iter().any(|(_, value)| value == "… 2 more item(s)"));

        rows.push(("hint".to_string(), "follow this thread".to_string()));
        let refs = rows
            .iter()
            .map(|(label, value)| (label.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        let lines = command_frame_lines(
            CommandFrame {
                title: "RYEOS COMMAND",
                phase: "live",
                command: "execute",
                status: "complete",
                detail: None,
                payload: &refs,
            },
            TerminalCapabilities::plain(80),
        );
        assert!(lines.iter().any(|line| line.contains("follow this thread")));
    }

    #[test]
    fn compact_help_wraps_descriptions_instead_of_truncating_them() {
        let capabilities = TerminalCapabilities {
            mode: capabilities::HumanOutputMode::Tty,
            color: false,
            unicode: true,
            width: 60,
        };
        let home = TtyHomeFile {
            screen: TtyScreen::Help,
            loading_frame: None,
            sections: TtyHomeSections {
                node: TtyNodeSummary {
                    status: "running".to_string(),
                    detail: None,
                },
                project: None,
                commands: TtyCommandSummary {
                    count: Some(1),
                    detail: None,
                },
                items: vec![TtyItem {
                    label: "bundle install".to_string(),
                    detail: Some(
                        "Install or replace a downstream bundle while preserving verified metadata"
                            .to_string(),
                    ),
                }],
            },
        };

        let lines = render_lines(&home, capabilities);
        assert!(lines.iter().any(|line| line.contains("preserving")));
        assert!(lines.iter().any(|line| line.contains("metadata")));
        assert!(!lines.iter().any(|line| line.contains("pack...")));
        assert!(lines.iter().all(|line| visible_width(line) < 60));
    }

    #[test]
    fn loading_projection_advances_its_node_spinner() {
        let capabilities = TerminalCapabilities {
            mode: capabilities::HumanOutputMode::Tty,
            color: false,
            unicode: true,
            width: 80,
        };
        let project = ProjectDisplay {
            key: None,
            label: "none".to_string(),
            detail: None,
        };
        let first = render_lines(
            &loading_projection(TtyScreen::Help, &project, 0),
            capabilities,
        );
        let second = render_lines(
            &loading_projection(TtyScreen::Help, &project, 1),
            capabilities,
        );
        let first_node = first
            .iter()
            .find(|line| line.contains("node"))
            .expect("first loading frame has node row");
        let second_node = second
            .iter()
            .find(|line| line.contains("node"))
            .expect("second loading frame has node row");
        assert_ne!(first_node, second_node);
        assert!(first_node.starts_with('⠋'));
        assert!(second_node.starts_with('⠙'));
    }

    #[test]
    fn word_wrapping_preserves_long_error_tokens() {
        let token = format!("vault:{}", "x".repeat(160));
        let lines = wrap_words(&token, 24);
        assert!(lines.iter().all(|line| visible_width(line) <= 24));
        assert_eq!(lines.concat(), token);
    }

    #[test]
    fn plain_line_output_preserves_complete_error_bodies() {
        let line = format!(
            "ryeos: daemon returned HTTP 500: {{\"error\":\"inventory build failed: {}\"}}",
            "x".repeat(256)
        );
        let mut plain = Vec::new();
        write_lines(&mut plain, std::slice::from_ref(&line), None).expect("write plain line");
        assert_eq!(
            String::from_utf8(plain).expect("utf-8 output"),
            format!("{line}\n")
        );

        let mut tty = Vec::new();
        write_lines(&mut tty, std::slice::from_ref(&line), Some(80)).expect("write tty line");
        let tty = String::from_utf8(tty).expect("utf-8 output");
        assert!(tty.contains("inventory build failed"));
        assert_eq!(tty.lines().collect::<String>(), line);
        assert!(tty.lines().all(|line| visible_width(line) < 80));
    }

    #[test]
    fn command_frame_status_uses_semantic_gruvbox_tone() {
        assert_eq!(frame_status_tone("complete"), Tone::Success);
        assert_eq!(frame_status_tone("streaming"), Tone::Active);
        assert_eq!(frame_status_tone("error"), Tone::Failure);
        assert_eq!(node_status_tone("running"), Tone::Success);
        assert_eq!(node_status_tone("unresponsive"), Tone::Warning);
    }

    #[test]
    fn visible_width_ignores_ansi_and_counts_wide_unicode() {
        assert_eq!(visible_width("\x1b[31mred\x1b[0m"), 3);
        assert_eq!(visible_width("◆界"), 3);
    }

    #[test]
    fn visible_clamp_preserves_width_and_resets_ansi() {
        let rendered = clamp_visible("\x1b[31mabcdef\x1b[0m", 4);
        assert!(visible_width(&rendered) <= 4);
        assert!(rendered.ends_with("\x1b[0m"));
    }

    #[test]
    fn visible_clamp_respects_one_and_two_column_viewports() {
        assert_eq!(visible_width(&clamp_visible("abcdef", 1)), 1);
        assert_eq!(visible_width(&clamp_visible("abcdef", 2)), 2);
    }
}
