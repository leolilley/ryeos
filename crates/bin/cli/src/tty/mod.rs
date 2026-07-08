//! Rye OS CLI TTY presentation.
//!
//! This is a normal stdout renderer, not the full TUI. Cached data is only a
//! presentation projection and is never used for dispatch or authorization.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result as AnyhowResult};
use ryeos_app::principal::{PrincipalPaths, PrincipalResolver, PrincipalStore};
use ryeos_node::{LifecycleController, LifecycleStatus, LocalLifecycleEnv};
use serde::{Deserialize, Serialize};

use crate::error::{CliError, CliTransportError};
use crate::transport::signing::Signer;

const TTY_HOME_VERSION: u32 = 1;
const TTY_CONFIG_VERSION: u32 = 1;
const DEFAULT_TERMINAL_WIDTH: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtyScreen {
    Home,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TtyEntryKind {
    Bare,
    ExplicitScreen,
}

pub async fn run(
    app_root: &Path,
    explicit_project: Option<&Path>,
    screen: TtyScreen,
    debug: bool,
) -> std::result::Result<(), CliError> {
    let app_root_key = normalize_path_for_key(app_root);
    let app_root_path = PathBuf::from(&app_root_key);
    let entry_kind = match screen {
        TtyScreen::Home => TtyEntryKind::Bare,
        TtyScreen::Help => TtyEntryKind::ExplicitScreen,
    };
    let screen = configured_screen(&app_root_path, screen, entry_kind, debug);
    let project = resolve_project_for_display(explicit_project);
    let signer = resolve_operator(app_root);
    let remote_url = remote_daemon_url();
    let cache_enabled = remote_url.is_none();
    let mut rendered_lines = 0;

    if let (Some(operator), true, true) = (
        signer.operator_principal_id.as_ref(),
        project.cacheable,
        cache_enabled,
    ) {
        let resolver = AppRootPrincipalResolver {
            root: app_root_path.clone(),
        };
        if let Ok(store) = PrincipalStore::resolve_with(&resolver, operator) {
            let path = store.paths().ryeos_tty_home();
            match load_optional_tty_home(&path) {
                Ok(Some(cached))
                    if cache_matches(&cached, &app_root_key, operator, &project.key, screen) =>
                {
                    rendered_lines = render(&cached, RenderMode::Cached, rendered_lines)?;
                }
                Ok(_) => {}
                Err(err) => debug_warn(debug, format!("ignore tty home cache: {err:#}")),
            }
        }
    }

    if rendered_lines == 0 {
        rendered_lines = render(
            &loading_projection(
                &app_root_key,
                signer.operator_principal_id.as_deref(),
                &project,
                screen,
            ),
            RenderMode::Live,
            rendered_lines,
        )?;
    }

    let live = build_live_projection(
        app_root,
        &app_root_key,
        &project,
        &signer,
        screen,
        remote_url.as_deref(),
    )
    .await;
    render(&live, RenderMode::Live, rendered_lines)?;

    if let (Some(operator), true, true, true) = (
        signer.operator_principal_id.as_ref(),
        project.cacheable,
        signer.cache_writable,
        cache_enabled,
    ) {
        let resolver = AppRootPrincipalResolver {
            root: app_root_path,
        };
        match PrincipalStore::locked_with(&resolver, operator).await {
            Ok(locked) => {
                let path = locked.paths().ryeos_tty_home();
                if let Err(err) = locked.write_yaml(&path, &live) {
                    debug_warn(debug, format!("write tty home cache: {err:#}"));
                }
            }
            Err(err) => debug_warn(debug, format!("lock tty home cache: {err:#}")),
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct AppRootPrincipalResolver {
    root: PathBuf,
}

impl PrincipalResolver for AppRootPrincipalResolver {
    fn resolve(&self, principal_id: &str) -> AnyhowResult<PrincipalPaths> {
        if principal_id.trim().is_empty() {
            anyhow::bail!("principal id is required");
        }
        Ok(PrincipalPaths::new(self.root.clone()))
    }
}

#[derive(Debug, Clone)]
struct OperatorState {
    operator_principal_id: Option<String>,
    cache_writable: bool,
    key_status: SourceStatus,
}

fn resolve_operator(app_root: &Path) -> OperatorState {
    match Signer::resolve(app_root) {
        Ok(signer) => OperatorState {
            operator_principal_id: Some(format!("fp:{}", signer.fingerprint)),
            cache_writable: true,
            key_status: SourceStatus::live(),
        },
        Err(CliTransportError::SigningKeyMissing { .. }) => OperatorState {
            operator_principal_id: None,
            cache_writable: false,
            key_status: SourceStatus::missing("operator signing key missing"),
        },
        Err(err) => OperatorState {
            operator_principal_id: None,
            cache_writable: false,
            key_status: SourceStatus::error(format!("operator signing key error: {err}")),
        },
    }
}

#[derive(Debug, Clone)]
struct ProjectDisplay {
    key: Option<String>,
    label: String,
    detail: Option<String>,
    cacheable: bool,
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
                    cacheable: true,
                }
            }
            Ok(canonical) => ProjectDisplay {
                key: None,
                label: canonical.display().to_string(),
                detail: Some("not a directory".to_string()),
                cacheable: false,
            },
            Err(err) => ProjectDisplay {
                key: None,
                label: abs.display().to_string(),
                detail: Some(format!("unavailable: {err}")),
                cacheable: false,
            },
        };
    }

    let Ok(cwd) = std::env::current_dir().and_then(|cwd| cwd.canonicalize()) else {
        return ProjectDisplay {
            key: None,
            label: "none".to_string(),
            detail: Some("current directory unavailable".to_string()),
            cacheable: false,
        };
    };

    for ancestor in cwd.ancestors() {
        if ancestor.join(ryeos_engine::AI_DIR).is_dir() {
            let key = ancestor.display().to_string();
            return ProjectDisplay {
                label: path_label(ancestor),
                detail: Some(key.clone()),
                key: Some(key),
                cacheable: true,
            };
        }
    }

    ProjectDisplay {
        key: None,
        label: "none".to_string(),
        detail: None,
        cacheable: true,
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

fn normalize_path_for_key(path: &Path) -> String {
    let abs = absolutize(path);
    abs.canonicalize()
        .unwrap_or(abs)
        .display()
        .to_string()
}

fn remote_daemon_url() -> Option<String> {
    std::env::var("RYEOSD_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn load_optional_tty_home(path: &Path) -> AnyhowResult<Option<TtyHomeFile>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_yaml::from_str(&raw)
            .map(Some)
            .with_context(|| format!("parse {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn configured_screen(
    app_root: &Path,
    default: TtyScreen,
    entry_kind: TtyEntryKind,
    debug: bool,
) -> TtyScreen {
    if entry_kind != TtyEntryKind::Bare {
        return default;
    }

    let resolver = AppRootPrincipalResolver {
        root: app_root.to_path_buf(),
    };
    let store = match PrincipalStore::resolve_with(
        &resolver,
        ryeos_app::principal::LOCAL_PRINCIPAL_ID,
    ) {
        Ok(store) => store,
        Err(err) => {
            debug_warn(debug, format!("resolve tty config path: {err:#}"));
            return default;
        }
    };
    match load_optional_tty_config(&store.paths().ryeos_tty_config()) {
        Ok(Some(config)) if config.version == TTY_CONFIG_VERSION => config.bare_action.screen(),
        Ok(Some(config)) => {
            debug_warn(
                debug,
                format!("ignore tty config version {}", config.version),
            );
            default
        }
        Ok(None) => default,
        Err(err) => {
            debug_warn(debug, format!("ignore tty config: {err:#}"));
            default
        }
    }
}

fn load_optional_tty_config(path: &Path) -> AnyhowResult<Option<TtyConfigFile>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_yaml::from_str(&raw)
            .map(Some)
            .with_context(|| format!("parse {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
    }
}

fn cache_matches(
    cached: &TtyHomeFile,
    app_root: &str,
    operator_principal_id: &str,
    project_root: &Option<String>,
    screen: TtyScreen,
) -> bool {
    cached.version == TTY_HOME_VERSION
        && cached.screen == screen
        && cached.app_root == app_root
        && cached.operator_principal_id == operator_principal_id
        && &cached.project_root == project_root
}

fn loading_projection(
    app_root: &str,
    operator_principal_id: Option<&str>,
    project: &ProjectDisplay,
    screen: TtyScreen,
) -> TtyHomeFile {
    TtyHomeFile {
        version: TTY_HOME_VERSION,
        screen,
        generated_at: lillux::time::iso8601_now(),
        app_root: app_root.to_string(),
        operator_principal_id: operator_principal_id.unwrap_or("missing").to_string(),
        project_root: project.key.clone(),
        source: TtyHomeSource {
            node: SourceStatus::loading(),
            node_config: SourceStatus::loading(),
        },
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
            actions: screen_actions(screen, false),
        },
    }
}

async fn build_live_projection(
    app_root: &Path,
    app_root_key: &str,
    project: &ProjectDisplay,
    signer: &OperatorState,
    screen: TtyScreen,
    remote_url: Option<&str>,
) -> TtyHomeFile {
    let (node, node_status) = match remote_url {
        Some(remote_url) => (
            TtyNodeSummary {
                status: "remote override".to_string(),
                detail: Some(format!("RYEOSD_URL={remote_url}")),
            },
            SourceStatus::live(),
        ),
        None => lifecycle_summary(app_root).await,
    };
    let snapshot = crate::node_descriptors::load_verified_snapshot(app_root);
    let (node_config_status, command_count, has_tui_command) = match snapshot {
        Ok(snapshot) => {
            let has_tui_command = crate::node_descriptors::load_command_descriptors_from_snapshot(
                &snapshot,
            )
            .iter()
            .any(|command| command.tokens.len() == 1 && command.tokens[0] == "tui");
            (
                SourceStatus::live(),
                Some(snapshot.commands.len()),
                has_tui_command,
            )
        }
        Err(err) => (
            SourceStatus::error(format!("verified node config: {err:#}")),
            None,
            false,
        ),
    };

    let mut node = node;
    if !matches!(signer.key_status.state, SourceState::Live) {
        node.detail = Some(match node.detail {
            Some(detail) => format!("{detail}; {}", signer.key_status.label()),
            None => signer.key_status.label(),
        });
    }

    TtyHomeFile {
        version: TTY_HOME_VERSION,
        screen,
        generated_at: lillux::time::iso8601_now(),
        app_root: app_root_key.to_string(),
        operator_principal_id: signer
            .operator_principal_id
            .clone()
            .unwrap_or_else(|| "missing".to_string()),
        project_root: project.key.clone(),
        source: TtyHomeSource {
            node: node_status,
            node_config: node_config_status,
        },
        sections: TtyHomeSections {
            node,
            project: Some(TtyProjectSummary::from_project(project)),
            commands: TtyCommandSummary {
                count: command_count,
                detail: command_count
                    .is_none()
                    .then(|| "run `ryeos node doctor` for diagnostics".to_string()),
            },
            actions: screen_actions(screen, has_tui_command),
        },
    }
}

async fn lifecycle_summary(app_root: &Path) -> (TtyNodeSummary, SourceStatus) {
    let env = match LocalLifecycleEnv::load(Some(app_root.to_path_buf())) {
        Ok(env) => env,
        Err(err) => {
            return (
                TtyNodeSummary {
                    status: "config error".to_string(),
                    detail: Some(err.to_string()),
                },
                SourceStatus::error(format!("local lifecycle config: {err:#}")),
            )
        }
    };
    let controller = LifecycleController::from_env(env);
    match controller.status().await {
        Ok(LifecycleStatus::NotInitialized { diagnostics }) => (
            TtyNodeSummary {
                status: "not initialized".to_string(),
                detail: Some(diagnostics.message),
            },
            SourceStatus::missing("not initialized"),
        ),
        Ok(LifecycleStatus::Stopped { app_root }) => (
            TtyNodeSummary {
                status: "stopped".to_string(),
                detail: Some(format!("app root: {}", app_root.display())),
            },
            SourceStatus::live(),
        ),
        Ok(LifecycleStatus::Running { metadata }) => {
            let mut detail = Vec::new();
            if let Some(pid) = metadata.pid {
                detail.push(format!("pid {pid}"));
            }
            if let Some(bind) = metadata.bind {
                detail.push(format!("http://{bind}"));
            }
            (
                TtyNodeSummary {
                    status: "running".to_string(),
                    detail: (!detail.is_empty()).then(|| detail.join(" · ")),
                },
                SourceStatus::live(),
            )
        }
        Ok(LifecycleStatus::Stale { diagnostics, .. }) => (
            TtyNodeSummary {
                status: "stale".to_string(),
                detail: Some(diagnostics.message),
            },
            SourceStatus::error("stale daemon metadata"),
        ),
        Ok(LifecycleStatus::Unresponsive { diagnostics, .. }) => (
            TtyNodeSummary {
                status: "busy".to_string(),
                detail: Some(diagnostics.message),
            },
            SourceStatus::error("daemon is running but not answering"),
        ),
        Ok(LifecycleStatus::Starting { pid, started_at, .. }) => (
            TtyNodeSummary {
                status: "starting".to_string(),
                detail: Some(format!("pid {pid} · since {started_at}")),
            },
            SourceStatus::loading(),
        ),
        Err(err) => (
            TtyNodeSummary {
                status: "status error".to_string(),
                detail: Some(err.to_string()),
            },
            SourceStatus::error(format!("local lifecycle status: {err:#}")),
        ),
    }
}

fn screen_actions(screen: TtyScreen, has_tui_command: bool) -> Vec<TtyAction> {
    let mut actions = match screen {
        TtyScreen::Home => home_actions(),
        TtyScreen::Help => help_actions(),
    };
    if has_tui_command {
        actions.insert(
            0,
            TtyAction {
                label: "tui".to_string(),
                command: "tui".to_string(),
                description: "open terminal workspace".to_string(),
            },
        );
    }
    actions
}

fn home_actions() -> Vec<TtyAction> {
    vec![
        TtyAction {
            label: "help".to_string(),
            command: "help".to_string(),
            description: "open the compact TTY help screen".to_string(),
        },
        TtyAction {
            label: "status".to_string(),
            command: "node status".to_string(),
            description: "show local node lifecycle status".to_string(),
        },
        TtyAction {
            label: "doctor".to_string(),
            command: "node doctor".to_string(),
            description: "diagnose local node startup and config".to_string(),
        },
    ]
}

fn help_actions() -> Vec<TtyAction> {
    vec![
        TtyAction {
            label: "open".to_string(),
            command: "help <command>".to_string(),
            description: "show focused help for one command".to_string(),
        },
        TtyAction {
            label: "list".to_string(),
            command: "commands".to_string(),
            description: "print the full verified command list".to_string(),
        },
        TtyAction {
            label: "all".to_string(),
            command: "help --all".to_string(),
            description: "print the exhaustive CLI reference".to_string(),
        },
        TtyAction {
            label: "status".to_string(),
            command: "node status".to_string(),
            description: "show local node lifecycle status".to_string(),
        },
        TtyAction {
            label: "doctor".to_string(),
            command: "node doctor".to_string(),
            description: "diagnose local node startup and config".to_string(),
        },
    ]
}

fn loading_command_detail(screen: TtyScreen) -> &'static str {
    match screen {
        TtyScreen::Home => "loading verified command snapshot",
        TtyScreen::Help => "loading TTY help and verified command snapshot",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TtyHomeFile {
    version: u32,
    screen: TtyScreen,
    generated_at: String,
    app_root: String,
    operator_principal_id: String,
    project_root: Option<String>,
    source: TtyHomeSource,
    sections: TtyHomeSections,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TtyConfigFile {
    version: u32,
    bare_action: TtyBareAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TtyBareAction {
    Screen { screen: TtyScreen },
}

impl TtyBareAction {
    fn screen(&self) -> TtyScreen {
        match self {
            Self::Screen { screen } => *screen,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TtyHomeSource {
    node: SourceStatus,
    node_config: SourceStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceStatus {
    state: SourceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl SourceStatus {
    fn live() -> Self {
        Self {
            state: SourceState::Live,
            message: None,
        }
    }

    fn loading() -> Self {
        Self {
            state: SourceState::Loading,
            message: None,
        }
    }

    fn missing(message: impl Into<String>) -> Self {
        Self {
            state: SourceState::Missing,
            message: Some(message.into()),
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            state: SourceState::Error,
            message: Some(message.into()),
        }
    }

    fn label(&self) -> String {
        self.message
            .clone()
            .unwrap_or_else(|| format!("{:?}", self.state).to_ascii_lowercase())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SourceState {
    Missing,
    Loading,
    Live,
    Error,
}

impl SourceState {
    fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Loading => "loading",
            Self::Live => "live",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TtyHomeSections {
    node: TtyNodeSummary,
    project: Option<TtyProjectSummary>,
    commands: TtyCommandSummary,
    actions: Vec<TtyAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TtyNodeSummary {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TtyProjectSummary {
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TtyCommandSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TtyAction {
    label: String,
    command: String,
    description: String,
}

enum RenderMode {
    Cached,
    Live,
}

fn render(home: &TtyHomeFile, mode: RenderMode, previous_lines: usize) -> io::Result<usize> {
    let width = terminal_width();
    let lines = render_lines(home, mode)
        .into_iter()
        .map(|line| clamp_line(&line, width))
        .collect::<Vec<_>>();
    write_frame(&mut io::stdout(), &lines, previous_lines)?;
    Ok(lines.len())
}

fn render_lines(home: &TtyHomeFile, mode: RenderMode) -> Vec<String> {
    let source = match mode {
        RenderMode::Cached => "cached",
        RenderMode::Live => "live",
    };
    let mut lines = Vec::new();
    lines.push(match home.screen {
        TtyScreen::Home => "RYE OS".to_string(),
        TtyScreen::Help => "RYE OS HELP".to_string(),
    });
    lines.push(match home.screen {
        TtyScreen::Home => "portable verified execution".to_string(),
        TtyScreen::Help => "compact TTY help".to_string(),
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
    lines.push(format!("{:<9} {} · {}", "source", source, home.generated_at));
    lines.push(format!(
        "{:<9} node {} · node config {}",
        "state",
        home.source.node.state.label(),
        home.source.node_config.state.label()
    ));
    lines.push(String::new());
    for action in &home.sections.actions {
        lines.push(format!(
            "  {:<8} {:<18} {}",
            action.label, action.command, action.description
        ));
    }
    lines.push(String::new());
    lines
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

fn debug_warn(debug: bool, message: String) {
    if debug {
        eprintln!("ryeos tty: {message}");
    }
}
