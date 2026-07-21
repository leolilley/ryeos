use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ryeos_app::node_config::NodeConfigSnapshot;
use ryeos_runtime::{
    CommandArgumentArity, CommandArgumentKind, CommandAvailability, CommandProjectResolution,
};

use super::interaction::{
    Event, EventReader, Frame, InputAction, Key, KeyEvent, ListItem, ListState, Pager,
    TerminalGuard, TextInput,
};
use super::{theme, Console, Tone};
use crate::error::CliError;
use crate::help::{CachedHelpRow, DescriptorHelpRow, ResolvedCommandHelp};

const MINIMUM_WIDTH: u16 = 40;
const MINIMUM_HEIGHT: u16 = 12;
const MAXIMUM_INDEX_ROWS: usize = 8;
const MAXIMUM_DETAIL_ROWS: usize = 10;
const EVENT_TICK: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
enum HelpEntry {
    Local {
        tokens: String,
        description: String,
        category: String,
    },
    Cached(CachedHelpRow),
    Installed(DescriptorHelpRow),
}

impl HelpEntry {
    fn tokens(&self) -> &str {
        match self {
            Self::Local { tokens, .. } => tokens,
            Self::Cached(row) => &row.tokens,
            Self::Installed(row) => &row.tokens,
        }
    }

    fn description(&self) -> &str {
        match self {
            Self::Local { description, .. } => description,
            Self::Cached(row) => &row.description,
            Self::Installed(row) => &row.description,
        }
    }

    fn search_text(&self) -> String {
        match self {
            Self::Local {
                tokens,
                description,
                category,
            } => format!("{tokens} {description} {category}"),
            Self::Cached(row) => format!(
                "{} {} {} {}",
                row.tokens,
                row.aliases.join(" "),
                row.description,
                row.category
            ),
            Self::Installed(row) => format!(
                "{} {} {} {}",
                row.tokens,
                row.aliases.join(" "),
                row.description,
                row.category
            ),
        }
    }

    fn matches_tokens(&self, tokens: &str) -> bool {
        self.tokens() == tokens
            || matches!(self,
                Self::Cached(row) if row.aliases.iter().any(|alias| alias == tokens)
            )
            || matches!(self,
                Self::Installed(row) if row.aliases.iter().any(|alias| alias == tokens)
            )
    }
}

struct Discovery {
    snapshot: NodeConfigSnapshot,
    entries: Vec<HelpEntry>,
}

struct DetailState {
    entry: HelpEntry,
    base: String,
    enrichment: String,
    pager: Pager,
}

type ResolutionResult = Result<ResolvedCommandHelp, String>;
type ResolutionTask = tokio::task::JoinHandle<(String, ResolutionResult)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Index,
    Detail,
}

pub(crate) fn supported_geometry() -> bool {
    crossterm::terminal::size()
        .map(|(width, height)| width >= MINIMUM_WIDTH && height >= MINIMUM_HEIGHT)
        .unwrap_or(false)
}

pub(crate) async fn run(
    console: &Console,
    app_root: &Path,
    project_path: &str,
    initial_tokens: Option<&[String]>,
) -> Result<(), CliError> {
    let initial_target = initial_tokens.map(|tokens| tokens.join(" "));
    let cached_entries = crate::help::read_cached_descriptor_rows(app_root)
        .into_iter()
        .map(HelpEntry::Cached)
        .collect::<Vec<_>>();
    let mut entries = local_entries();
    entries.extend(cached_entries);
    let (mut width, mut height) = terminal_geometry(console);
    let mut list = ListState::new(to_list_items(entries), list_rows(height));
    let mut filter = TextInput::new(120);
    let mut filtering = false;
    let mut legend = false;
    let mut node_status = "checking".to_string();
    let mut verification_status = "verifying commands".to_string();
    let mut snapshot = None;
    let mut view = View::Index;
    let mut detail = None;
    let mut resolution_cache = HashMap::<String, ResolutionResult>::new();
    let mut events = EventReader::new(EVENT_TICK)?;
    let mut guard = TerminalGuard::enter()?;
    let mut frame = Frame::stdout();

    let discovery_root = app_root.to_path_buf();
    let mut discovery_task = Some(tokio::task::spawn_blocking(move || {
        let snapshot = crate::node_descriptors::load_verified_snapshot(&discovery_root)?;
        let descriptor_rows = crate::help::descriptor_rows(&snapshot);
        if let Err(error) =
            crate::help::write_verified_descriptor_cache(&discovery_root, &snapshot, &descriptor_rows)
        {
            tracing::debug!(error = %error, "could not refresh help descriptor cache");
        }
        let entries = descriptor_rows
            .into_iter()
            .map(HelpEntry::Installed)
            .collect();
        Ok::<_, anyhow::Error>(Discovery { snapshot, entries })
    }));
    let status_root = app_root.to_path_buf();
    let mut status_task = Some(tokio::spawn(async move {
        super::lifecycle_summary(status_root).await.status
    }));
    let mut resolution_task = None;

    if let Some(target) = initial_target.as_deref() {
        if let Some(entry) = list
            .find(|item| item.value.matches_tokens(target))
            .map(|item| item.value.clone())
        {
            open_detail(
                entry,
                &snapshot,
                app_root,
                project_path,
                width,
                height,
                &mut detail,
                &mut resolution_task,
                &resolution_cache,
            );
            view = View::Detail;
        }
    }

    let interaction_result: Result<(), CliError> = async {
        let mut dirty = true;
        loop {
            if dirty {
                render(
                    &mut frame,
                    console,
                    view,
                    &list,
                    &filter,
                    filtering,
                    legend,
                    &node_status,
                    &verification_status,
                    detail.as_ref(),
                    width,
                )?;
            }

            let event = events.next().await?;
            if matches!(event, Event::Tick) {
                dirty = false;
            if discovery_task
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished)
            {
                dirty = true;
                let outcome = discovery_task.take().expect("finished task present").await;
                match outcome {
                    Ok(Ok(discovery)) => {
                        verification_status = "verified command surface".to_string();
                        snapshot = Some(discovery.snapshot);
                        let mut entries = local_entries();
                        entries.extend(discovery.entries);
                        list.replace_items(to_list_items(entries));
                        let target = detail
                            .as_ref()
                            .filter(|detail| matches!(&detail.entry, HelpEntry::Cached(_)))
                            .map(|detail| detail.entry.tokens().to_string())
                            .or_else(|| (view == View::Index).then(|| initial_target.clone()).flatten());
                        if let Some(target) = target {
                            if let Some(entry) = list
                                .find(|item| {
                                    matches!(&item.value, HelpEntry::Installed(_))
                                        && item.value.matches_tokens(&target)
                                })
                                .map(|item| item.value.clone())
                            {
                                open_detail(
                                    entry,
                                    &snapshot,
                                    app_root,
                                    project_path,
                                    width,
                                    height,
                                    &mut detail,
                                    &mut resolution_task,
                                    &resolution_cache,
                                );
                                view = View::Detail;
                            } else if detail
                                .as_ref()
                                .is_some_and(|detail| matches!(&detail.entry, HelpEntry::Cached(_)))
                            {
                                if let Some(task) = resolution_task.take() {
                                    task.abort();
                                }
                                detail = None;
                                view = View::Index;
                                verification_status =
                                    "verified command surface; cached command is no longer installed"
                                        .to_string();
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        verification_status = format!("descriptor warning: {error:#}");
                        discard_unverified_cache(
                            &mut list,
                            &filter,
                            &mut view,
                            &mut detail,
                            &mut resolution_task,
                        );
                    }
                    Err(error) => {
                        verification_status = format!("descriptor task failed: {error}");
                        discard_unverified_cache(
                            &mut list,
                            &filter,
                            &mut view,
                            &mut detail,
                            &mut resolution_task,
                        );
                    }
                }
            }
            if status_task
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished)
            {
                dirty = true;
                node_status = match status_task.take().expect("finished task present").await {
                    Ok(status) => status,
                    Err(error) => format!("status error: {error}"),
                };
            }
            if resolution_task
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished)
            {
                dirty = true;
                let outcome = resolution_task.take().expect("finished task present").await;
                if let Some(detail) = detail.as_mut() {
                    detail.enrichment = match outcome {
                        Ok((tokens, result)) => {
                            resolution_cache.insert(tokens, result.clone());
                            resolution_detail(&result)
                        }
                        Err(error) => format!(
                            "\nADVANCED RESOLUTION ERROR\nmetadata task failed: {error}\n"
                        ),
                    };
                    detail
                        .pager
                        .replace_source(format!("{}{}", detail.base, detail.enrichment));
                }
            }
                continue;
            }

            match event {
            Event::Terminate => {
                return Err(CliError::Local {
                    detail: "interactive help terminated by signal".to_string(),
                })
            }
            Event::Key(key) => {
                if key.is_control('c') || (view == View::Index && !filtering && key.key == Key::Char('q')) {
                    break;
                }
                match view {
                    View::Index => handle_index_key(
                        key,
                        &mut list,
                        &mut filter,
                        &mut filtering,
                        &mut legend,
                    ),
                    View::Detail => {
                        if key.key == Key::Char('q') {
                            break;
                        }
                        if matches!(key.key, Key::Escape | Key::Left | Key::Backspace) {
                            if let Some(task) = resolution_task.take() {
                                task.abort();
                            }
                            detail = None;
                            view = View::Index;
                        } else if key.key == Key::Char('?') {
                            legend = !legend;
                        } else if let Some(detail) = detail.as_mut() {
                            handle_pager_key(key, &mut detail.pager);
                        }
                    }
                }
                if view == View::Index && key.key == Key::Enter {
                    if let Some(entry) = list.selected().map(|item| item.value.clone()) {
                        open_detail(
                            entry,
                            &snapshot,
                            app_root,
                            project_path,
                            width,
                            height,
                            &mut detail,
                            &mut resolution_task,
                            &resolution_cache,
                        );
                        view = View::Detail;
                        filtering = false;
                    }
                }
            }
            Event::Paste(value) => {
                if view == View::Index {
                    filtering = true;
                    if filter.paste(&value) == InputAction::Changed {
                        list.set_filter(filter.value());
                    }
                }
            }
            Event::Resize {
                width: new_width,
                height: new_height,
            } => {
                width = new_width;
                height = new_height;
                if width < MINIMUM_WIDTH || height < MINIMUM_HEIGHT {
                    verification_status = format!(
                        "terminal too small: {width}x{height}; minimum {MINIMUM_WIDTH}x{MINIMUM_HEIGHT}"
                    );
                }
                list.set_viewport_rows(list_rows(height));
                if let Some(detail) = detail.as_mut() {
                    detail.pager.set_geometry(
                        usize::from(width).saturating_sub(4).max(1),
                        pager_rows(height),
                    );
                }
            }
            Event::Tick => unreachable!("ticks are handled before input dispatch"),
            }
            dirty = true;
        }
        Ok(())
    }
    .await;

    if let Some(task) = discovery_task {
        task.abort();
    }
    if let Some(task) = status_task {
        task.abort();
    }
    if let Some(task) = resolution_task {
        task.abort();
    }
    let clear_result = frame.clear().map_err(CliError::from);
    let restore_result = guard.restore().map_err(CliError::from);
    interaction_result.and(clear_result).and(restore_result)
}

fn discard_unverified_cache(
    list: &mut ListState<HelpEntry>,
    filter: &TextInput,
    view: &mut View,
    detail: &mut Option<DetailState>,
    resolution_task: &mut Option<ResolutionTask>,
) {
    let filter_value = filter.value().to_string();
    list.replace_items(to_list_items(local_entries()));
    list.set_filter(&filter_value);
    if detail
        .as_ref()
        .is_some_and(|detail| matches!(&detail.entry, HelpEntry::Cached(_)))
    {
        if let Some(task) = resolution_task.take() {
            task.abort();
        }
        *detail = None;
        *view = View::Index;
    }
}

fn handle_index_key(
    key: KeyEvent,
    list: &mut ListState<HelpEntry>,
    filter: &mut TextInput,
    filtering: &mut bool,
    legend: &mut bool,
) {
    if key.key == Key::Escape && !filter.is_empty() {
        filter.clear();
        list.set_filter("");
        *filtering = false;
        return;
    }
    if key.key == Key::Char('?') {
        *legend = !*legend;
        return;
    }
    if !*filtering {
        match key.key {
            Key::Up | Key::Char('k') => list.previous(),
            Key::Down | Key::Char('j') => list.next(),
            Key::PageUp => list.page_up(),
            Key::PageDown => list.page_down(),
            Key::Home => list.first(),
            Key::End => list.last(),
            Key::Char('/') => *filtering = true,
            Key::Char(value) if !key.modifiers.control && !value.is_control() => {
                *filtering = true;
                if filter.handle_key(key) == InputAction::Changed {
                    list.set_filter(filter.value());
                }
            }
            _ if key.is_control('u') => list.page_up(),
            _ if key.is_control('d') => list.page_down(),
            _ => {}
        }
        return;
    }

    if key.key == Key::Escape {
        if !filter.is_empty() {
            filter.clear();
            list.set_filter("");
        }
        *filtering = false;
        return;
    }
    match key.key {
        Key::Up => {
            list.previous();
            return;
        }
        Key::Down => {
            list.next();
            return;
        }
        Key::PageUp => {
            list.page_up();
            return;
        }
        Key::PageDown => {
            list.page_down();
            return;
        }
        Key::Home => {
            list.first();
            return;
        }
        Key::End => {
            list.last();
            return;
        }
        _ if key.is_control('u') => {
            list.page_up();
            return;
        }
        _ if key.is_control('d') => {
            list.page_down();
            return;
        }
        _ => {}
    }
    match filter.handle_key(key) {
        InputAction::Changed => list.set_filter(filter.value()),
        InputAction::Submit => *filtering = false,
        InputAction::Cancel => *filtering = false,
        InputAction::Unchanged => {}
    }
}

fn handle_pager_key(key: KeyEvent, pager: &mut Pager) {
    match key.key {
        Key::Up | Key::Char('k') => pager.up(),
        Key::Down | Key::Char('j') => pager.down(),
        Key::PageUp => pager.page_up(),
        Key::PageDown => pager.page_down(),
        Key::Home => pager.home(),
        Key::End => pager.end(),
        _ if key.is_control('u') => pager.page_up(),
        _ if key.is_control('d') => pager.page_down(),
        _ => {}
    }
}

fn open_detail(
    entry: HelpEntry,
    snapshot: &Option<NodeConfigSnapshot>,
    app_root: &Path,
    project_path: &str,
    width: u16,
    height: u16,
    detail: &mut Option<DetailState>,
    resolution_task: &mut Option<ResolutionTask>,
    resolution_cache: &HashMap<String, ResolutionResult>,
) {
    if let Some(task) = resolution_task.take() {
        task.abort();
    }
    let base = descriptor_detail(&entry);
    let enrichment = resolution_cache
        .get(entry.tokens())
        .map(resolution_detail)
        .unwrap_or_default();
    let pager = Pager::new(
        format!("{base}{enrichment}"),
        usize::from(width).saturating_sub(4).max(1),
        pager_rows(height),
    );
    if !resolution_cache.contains_key(entry.tokens()) {
        if let (HelpEntry::Installed(row), Some(snapshot)) = (&entry, snapshot) {
            if row.descriptor.execute_ref().is_some() {
                let descriptor = row.descriptor.clone();
                let snapshot = snapshot.clone();
                let app_root = PathBuf::from(app_root);
                let project_path = project_path.to_string();
                let tokens = entry.tokens().to_string();
                *resolution_task = Some(tokio::task::spawn_blocking(move || {
                    (
                        tokens,
                        crate::help::resolve_selected_command_help(
                            &descriptor,
                            &snapshot,
                            &app_root,
                            &project_path,
                        ),
                    )
                }));
            }
        }
    }
    *detail = Some(DetailState {
        entry,
        base,
        enrichment,
        pager,
    });
}

fn render(
    frame: &mut Frame<std::io::Stdout>,
    console: &Console,
    view: View,
    list: &ListState<HelpEntry>,
    filter: &TextInput,
    filtering: bool,
    legend: bool,
    node_status: &str,
    verification_status: &str,
    detail: Option<&DetailState>,
    width: u16,
) -> std::io::Result<()> {
    let capabilities = console.capabilities();
    let node_status = super::sanitize_terminal_inline(node_status);
    let verification_status = super::sanitize_terminal_inline(verification_status);
    let mut lines = vec![format!(
        "  {}                                      {}",
        theme::style("RYEOS HELP", Tone::Neutral, capabilities.color),
        theme::style(&node_status, Tone::Secondary, capabilities.color)
    ),
    format!(
        "  {}",
        theme::style(&verification_status, Tone::Secondary, capabilities.color)
    ),
    String::new()];
    match view {
        View::Index => {
            let filter_prompt = if filtering || !filter.is_empty() {
                format!("/ {}", filter.value())
            } else {
                "/ search commands…".to_string()
            };
            lines.push(format!("  {filter_prompt}"));
            lines.push(String::new());
            let token_width = usize::from(width).saturating_sub(22).clamp(12, 28);
            if list.visible_len() == 0 {
                lines.push("    no matching commands".to_string());
            } else {
                for (visible_index, item) in list.visible_window() {
                    let selected = Some(visible_index) == list.selected_visible_index();
                    let marker = if selected { ">" } else { " " };
                    let tokens = super::clamp_visible(
                        &super::sanitize_terminal_inline(item.value.tokens()),
                        token_width,
                    );
                    let prefix = format!("  {marker} {tokens:token_width$}  ");
                    let available = usize::from(width)
                        .saturating_sub(super::visible_width(&prefix) + 1)
                        .max(1);
                    let description = super::clamp_visible(
                        &super::sanitize_terminal_inline(item.value.description()),
                        available,
                    );
                    lines.push(if selected {
                        theme::style(
                            &format!("{prefix}{description}"),
                            Tone::Active,
                            capabilities.color,
                        )
                    } else {
                        format!("{prefix}{description}")
                    });
                }
            }
            lines.push(String::new());
            lines.push(if legend {
                "  ↑/↓ move · ctrl-u/ctrl-d page · / filter · enter open · q quit".to_string()
            } else {
                "  ↑/↓ move  ·  enter open  ·  / filter  ·  ? keys  ·  q quit".to_string()
            });
        }
        View::Detail => {
            if let Some(detail) = detail {
                lines.extend(
                    detail
                        .pager
                        .visible_lines()
                        .iter()
                        .map(|line| format!("  {}", super::sanitize_terminal_inline(line))),
                );
                lines.push(String::new());
                lines.push(if legend {
                    format!(
                        "  {} · ↑/↓ scroll · ctrl-u/ctrl-d page · esc/← back · q quit",
                        detail.entry.tokens()
                    )
                } else {
                    "  ↑/↓ scroll  ·  back esc/←  ·  ? keys  ·  q quit".to_string()
                });
            }
        }
    }
    frame.render(&lines)
}

fn descriptor_detail(entry: &HelpEntry) -> String {
    if let HelpEntry::Cached(row) = entry {
        return format!(
            "ryeos {}\n{}\n\nCached descriptor summary\nVerified command detail is still loading.\n",
            row.tokens, row.description
        );
    }
    let HelpEntry::Installed(row) = entry else {
        return local_detail(entry.tokens(), entry.description());
    };
    let command = &row.descriptor.command;
    let mut output = format!("ryeos {}\n{}\n", row.tokens, row.description);
    output.push_str("\nUSAGE\n");
    if let Some(help) = command.help.as_ref().and_then(|help| help.usage.as_ref()) {
        output.push_str(help);
        output.push('\n');
    } else if command.forms.is_empty() {
        output.push_str(&format!("ryeos {}\n", row.tokens));
    } else {
        for form in &command.forms {
            let fields = form
                .slots
                .iter()
                .map(|slot| {
                    let name = slot.field.replace('_', "-");
                    if command.defaults.contains_key(&slot.field) {
                        format!("[<{name}>]")
                    } else {
                        format!("<{name}>")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            output.push_str(&format!("ryeos {} {fields}\n", row.tokens));
        }
    }
    if !command.arguments.is_empty() {
        output.push_str("\nPOSITIONAL FIELDS\n");
        let mut arguments = command.arguments.iter().collect::<Vec<_>>();
        arguments.sort_by_key(|argument| argument.positional);
        for argument in arguments {
            let arity = match argument.arity {
                CommandArgumentArity::One => "",
                CommandArgumentArity::Optional => " optional",
                CommandArgumentArity::Variadic => " variadic",
            };
            output.push_str(&format!(
                "{}  {}{}{}\n",
                argument.name,
                argument_kind(argument.kind),
                arity,
                argument
                    .description
                    .as_deref()
                    .map(|description| format!(" · {description}"))
                    .unwrap_or_default()
            ));
        }
    }
    if !command.control_flags.is_empty() || command.parameter_binding.is_some() {
        output.push_str("\nFLAGS\n");
        for flag in &command.control_flags {
            let value = if flag.binding.takes_value() {
                " <value>"
            } else {
                ""
            };
            output.push_str(&format!("--{}{}  {}\n", flag.flag, value, flag.help));
        }
        if let Some(binding) = &command.parameter_binding {
            if let Some(input_flag) = &binding.input_flag {
                output.push_str(&format!(
                    "--{input_flag} <file>  read JSON parameters from file or stdin\n"
                ));
            }
        }
    }
    if !row.aliases.is_empty() {
        output.push_str(&format!("\nALIASES\n{}\n", row.aliases.join(", ")));
    }
    output.push_str("\nREQUIREMENTS\n");
    let project = command
        .project
        .as_ref()
        .map(|project| match project.resolution {
            CommandProjectResolution::None => "none",
            CommandProjectResolution::Optional => "project optional",
            CommandProjectResolution::Required => "project required",
        })
        .unwrap_or("none");
    output.push_str(&format!("project  {project}\n"));
    if let Some(availability) = row.availability {
        output.push_str(&format!(
            "node     {}\n",
            availability_label(availability)
        ));
    }
    if let Some(help) = &command.help {
        if !help.examples.is_empty() {
            output.push_str("\nEXAMPLES\n");
            for example in &help.examples {
                output.push_str(example);
                output.push('\n');
            }
        }
    }
    output.push_str("\nADVANCED\n");
    output.push_str(&format!("dispatch  {}\n", row.dispatch_kind));
    if let Some(target) = &row.target_ref {
        output.push_str(&format!("target    {target}\n"));
    }
    if row.descriptor.execute_ref().is_some() {
        output.push_str("metadata  resolving selected target…\n");
    }
    output
}

fn local_detail(tokens: &str, description: &str) -> String {
    let (usage, options) = match tokens {
        "init" => (
            "ryeos init [--non-interactive | --json] [--app-root <DIR>] [--source <DIR>] [--trust-file <FILE>]...",
            "--non-interactive  run without onboarding prompts\n--json             emit the structured report\n--app-root <DIR>   application root\n--source <DIR>     packaged bundle source\n--trust-file <FILE> additional publisher trust document (repeatable)",
        ),
        "setup" => (
            "ryeos setup [--app-root <DIR>]",
            "--app-root <DIR>  application root",
        ),
        "identity" => (
            "ryeos identity [--json] [--app-root <DIR>]",
            "--json            emit structured identity data\n--app-root <DIR>  application root",
        ),
        "start" => (
            "ryeos start [--app-root <DIR>] [--bind <ADDR>] [--uds-path <PATH>]",
            "--app-root <DIR>  application root\n--bind <ADDR>      daemon bind address\n--uds-path <PATH>  Unix-domain socket path",
        ),
        "stop" => (
            "ryeos stop [--force] [--app-root <DIR>]",
            "--force           force stop after graceful shutdown fails\n--app-root <DIR>  application root",
        ),
        "node status" => (
            "ryeos node status [--json] [--app-root <DIR>]",
            "--json            emit structured status\n--app-root <DIR>  application root",
        ),
        "node doctor" => (
            "ryeos node doctor [--json] [--no-bundles] [--app-root <DIR>]",
            "--json            emit structured diagnostics\n--no-bundles      skip bundle diagnostics\n--app-root <DIR>  application root",
        ),
        "node gc" => (
            "ryeos node gc [--dry-run] [--sweep-cas] [--app-root <DIR>]",
            "--dry-run         report without mutation\n--sweep-cas       sweep newly unreachable CAS objects\n--app-root <DIR>  application root",
        ),
        "help" => (
            "ryeos help [<tokens>...] [--plain]",
            "--plain  deterministic non-interactive output",
        ),
        "help --all" | "commands" => ("ryeos commands", ""),
        _ => ("ryeos <command> [OPTIONS]", ""),
    };
    let mut output = format!("ryeos {tokens}\n{description}\n\nUSAGE\n{usage}\n");
    if !options.is_empty() {
        output.push_str("\nOPTIONS\n");
        output.push_str(options);
        output.push('\n');
    }
    output
}

fn resolved_detail(metadata: &ResolvedCommandHelp) -> String {
    let mut output = String::from("\nVERIFIED TARGET METADATA\n");
    if !metadata.description.is_empty() {
        output.push_str(&format!("description  {}\n", metadata.description));
    }
    if let Some(availability) = &metadata.availability {
        output.push_str(&format!("availability {availability}\n"));
    }
    output.push_str(&format!(
        "dispatch     {}\n",
        if metadata.is_offline {
            "offline capable"
        } else {
            "daemon"
        }
    ));
    for (field, kind) in &metadata.schema {
        output.push_str(&format!("field        {field}: {kind}\n"));
    }
    for capability in &metadata.required_caps {
        output.push_str(&format!("capability   {capability}\n"));
    }
    output
}

fn resolution_detail(result: &ResolutionResult) -> String {
    match result {
        Ok(metadata) => resolved_detail(metadata),
        Err(error) => format!("\nADVANCED RESOLUTION ERROR\n{error}\n"),
    }
}

fn local_entries() -> Vec<HelpEntry> {
    crate::lifecycle_commands::local_command_descriptors()
        .iter()
        .filter(|descriptor| descriptor.tokens != ["help", "--all"])
        .map(|descriptor| HelpEntry::Local {
            tokens: descriptor.tokens.join(" "),
            description: descriptor.summary.to_string(),
            category: descriptor.category.to_string(),
        })
        .collect()
}

fn to_list_items(entries: Vec<HelpEntry>) -> Vec<ListItem<HelpEntry>> {
    entries
        .into_iter()
        .map(|entry| ListItem::new(entry.tokens(), entry.search_text(), entry.clone()))
        .collect()
}

fn terminal_geometry(console: &Console) -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((
        u16::try_from(console.capabilities().width).unwrap_or(u16::MAX),
        24,
    ))
}

fn list_rows(height: u16) -> usize {
    usize::from(height)
        .saturating_sub(8)
        .clamp(1, MAXIMUM_INDEX_ROWS)
}

fn pager_rows(height: u16) -> usize {
    usize::from(height)
        .saturating_sub(6)
        .clamp(1, MAXIMUM_DETAIL_ROWS)
}

fn argument_kind(kind: CommandArgumentKind) -> &'static str {
    match kind {
        CommandArgumentKind::String => "string",
        CommandArgumentKind::CanonicalRef => "canonical-ref",
        CommandArgumentKind::Path => "path",
        CommandArgumentKind::Json => "json",
    }
}

fn availability_label(availability: CommandAvailability) -> &'static str {
    match availability {
        CommandAvailability::Auto => "resolved automatically",
        CommandAvailability::Daemon => "daemon required",
        CommandAvailability::Offline => "offline",
        CommandAvailability::Both => "offline or daemon",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_navigation_fixture() -> Vec<HelpEntry> {
        vec![
            HelpEntry::Local {
                tokens: "node status".to_string(),
                description: "Show node status".to_string(),
                category: "node".to_string(),
            },
            HelpEntry::Local {
                tokens: "node doctor".to_string(),
                description: "Diagnose the node".to_string(),
                category: "node".to_string(),
            },
        ]
    }

    #[test]
    fn help_viewports_stay_compact_on_tall_terminals() {
        assert_eq!(list_rows(200), MAXIMUM_INDEX_ROWS);
        assert_eq!(pager_rows(200), MAXIMUM_DETAIL_ROWS);
    }

    #[test]
    fn help_viewports_still_fit_the_minimum_terminal() {
        assert_eq!(list_rows(MINIMUM_HEIGHT), 4);
        assert_eq!(pager_rows(MINIMUM_HEIGHT), 6);
    }

    #[test]
    fn arrow_keys_navigate_results_while_filtering() {
        let mut list = ListState::new(to_list_items(filter_navigation_fixture()), 2);
        let mut filter = TextInput::new(120);
        assert_eq!(filter.paste("node"), InputAction::Changed);
        list.set_filter(filter.value());
        let mut filtering = true;
        let mut legend = false;

        handle_index_key(
            KeyEvent::plain(Key::Down),
            &mut list,
            &mut filter,
            &mut filtering,
            &mut legend,
        );

        assert_eq!(list.selected_visible_index(), Some(1));
        assert_eq!(filter.value(), "node");
        assert!(filtering);
    }
}
