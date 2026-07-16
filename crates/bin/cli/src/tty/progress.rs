use std::io::{self, Write};
use std::time::{Duration, Instant};

use ryeos_app::offline_gc::{OfflineThreadHistoryGcPhase, OfflineThreadHistoryGcProgress};
use ryeos_node::{
    LifecycleProgressObserver, LifecycleStatus, StartReport, StartupPhase, StopReport,
};

use super::TerminalCapabilities;

const MAX_BAR_WIDTH: usize = 18;
const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationKind {
    Install,
    Verify,
    Publish,
    Fetch,
    Run,
}

impl OperationKind {
    fn verb(self) -> &'static str {
        match self {
            Self::Install => "INSTALL",
            Self::Verify => "VERIFY",
            Self::Publish => "PUBLISH",
            Self::Fetch => "FETCH",
            Self::Run => "RUN",
        }
    }
}

pub struct OperationProgress {
    line: ProgressLine,
    operation: OperationKind,
}

impl OperationProgress {
    pub fn new(
        operation: OperationKind,
        label: &str,
        capabilities: TerminalCapabilities,
    ) -> io::Result<Option<Self>> {
        if !capabilities.tty() {
            return Ok(None);
        }
        let mut progress = Self {
            line: ProgressLine::new(capabilities),
            operation,
        };
        progress.line.render(operation.verb(), label, None, None)?;
        Ok(Some(progress))
    }

    pub fn update(&mut self, label: &str, detail: Option<&str>) -> io::Result<()> {
        self.line.render(self.operation.verb(), label, None, detail)
    }

    pub fn update_determinate(
        &mut self,
        label: &str,
        completed: usize,
        total: usize,
        detail: Option<&str>,
    ) -> io::Result<()> {
        let ratio = (total > 0).then(|| completed as f64 / total as f64);
        self.line
            .render(self.operation.verb(), label, ratio, detail)
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.line.clear()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleProgressAction {
    Boot,
    Shutdown,
}

impl LifecycleProgressAction {
    fn verb(self) -> &'static str {
        match self {
            Self::Boot => "BOOT",
            Self::Shutdown => "HALT",
        }
    }
}

pub struct LifecycleProgress {
    line: ProgressLine,
    action: LifecycleProgressAction,
}

impl LifecycleProgress {
    pub fn new(
        action: LifecycleProgressAction,
        capabilities: TerminalCapabilities,
    ) -> Option<Self> {
        capabilities.tty().then(|| Self {
            line: ProgressLine::new(capabilities),
            action,
        })
    }

    pub fn finish_start(mut self, report: &StartReport) -> io::Result<()> {
        self.line.clear()?;
        let elapsed = human_duration(self.line.started.elapsed());
        let qualifier = if report.already_running {
            "already online"
        } else {
            "boot complete"
        };
        let mut out = io::stdout().lock();
        let summary = format!(
            "{}  RYE/OS  {}  {} · {elapsed}",
            self.line.success_glyph(),
            self.line.success("NODE ONLINE"),
            self.line.dim(qualifier),
        );
        writeln!(
            out,
            "{}",
            super::clamp_visible(&summary, self.line.width.saturating_sub(1).max(1))
        )?;
        if let LifecycleStatus::Running { metadata, .. } = &report.status {
            let mut details = Vec::new();
            if let Some(pid) = metadata.pid {
                details.push(format!("pid {pid}"));
            }
            if let Some(bind) = &metadata.bind {
                details.push(format!("http://{bind}"));
            }
            if let Some(socket) = &metadata.uds_path {
                details.push(socket.display().to_string());
            }
            if !details.is_empty() {
                let details = format!("   {}", self.line.dim(&details.join("  ·  ")));
                writeln!(
                    out,
                    "{}",
                    super::clamp_visible(&details, self.line.width.saturating_sub(1).max(1))
                )?;
            }
        }
        out.flush()
    }

    pub fn finish_stop(mut self, report: &StopReport) -> io::Result<()> {
        self.line.clear()?;
        let elapsed = human_duration(self.line.started.elapsed());
        let qualifier = if report.already_stopped {
            "already offline"
        } else {
            "shutdown complete"
        };
        let mut out = io::stdout().lock();
        let summary = format!(
            "{}  RYE/OS  {}  {} · {elapsed}",
            self.line.success_glyph(),
            self.line.success("NODE OFFLINE"),
            self.line.dim(qualifier),
        );
        writeln!(
            out,
            "{}",
            super::clamp_visible(&summary, self.line.width.saturating_sub(1).max(1))
        )?;
        out.flush()
    }
}

impl LifecycleProgressObserver for LifecycleProgress {
    fn observe(&mut self, status: &LifecycleStatus) {
        let (label, ratio, detail) = match self.action {
            LifecycleProgressAction::Boot => boot_progress(status),
            LifecycleProgressAction::Shutdown => shutdown_progress(status),
        };
        let _ = self
            .line
            .render(self.action.verb(), &label, ratio, detail.as_deref());
    }
}

pub struct OfflineGcProgress {
    line: ProgressLine,
    last_phase: Option<OfflineThreadHistoryGcPhase>,
    last_rendered: Instant,
}

impl OfflineGcProgress {
    pub fn new(enabled: bool, capabilities: TerminalCapabilities) -> Option<Self> {
        (enabled && capabilities.tty()).then(|| Self {
            line: ProgressLine::new(capabilities),
            last_phase: None,
            last_rendered: Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(Instant::now),
        })
    }

    pub fn observe(&mut self, progress: &OfflineThreadHistoryGcProgress) {
        let phase_changed = self.last_phase != Some(progress.phase);
        let phase_finished = matches!(
            (progress.completed, progress.total),
            (Some(completed), Some(total)) if completed == total
        );
        if !phase_changed
            && !phase_finished
            && self.last_rendered.elapsed() < Duration::from_millis(50)
        {
            return;
        }
        let (label, ratio) = gc_progress(progress);
        let detail = match (progress.completed, progress.total) {
            (Some(completed), Some(total)) => Some(format!(
                "{}/{} chain heads",
                grouped(completed),
                grouped(total)
            )),
            _ => None,
        };
        let _ = self.line.render("CLEAR", label, ratio, detail.as_deref());
        self.last_phase = Some(progress.phase);
        self.last_rendered = Instant::now();
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.line.clear()
    }
}

struct ProgressLine {
    started: Instant,
    frame: usize,
    active: bool,
    color: bool,
    unicode: bool,
    width: usize,
}

impl ProgressLine {
    fn new(capabilities: TerminalCapabilities) -> Self {
        Self {
            started: Instant::now(),
            frame: 0,
            active: false,
            color: capabilities.color,
            unicode: capabilities.unicode,
            width: capabilities.width,
        }
    }

    fn render(
        &mut self,
        verb: &str,
        label: &str,
        ratio: Option<f64>,
        detail: Option<&str>,
    ) -> io::Result<()> {
        self.frame = self.frame.wrapping_add(1);
        let spinner = if self.unicode {
            SPINNER[self.frame % SPINNER.len()]
        } else {
            '.'
        };
        let bar_width = progress_bar_width(self.width);
        let bar = ratio
            .map(|ratio| determinate_bar(ratio, bar_width))
            .unwrap_or_else(|| pulse_bar(self.frame, bar_width));
        let elapsed = human_duration(self.started.elapsed());
        let detail = detail
            .filter(|detail| !detail.is_empty())
            .and_then(|detail| non_redundant_detail(label, detail))
            .map(|detail| format!(" · {detail}"))
            .unwrap_or_default();
        let plain = format!("{spinner}  RYE/OS  {verb:<5}  {bar}  {label}{detail} · {elapsed}");
        let plain = super::clamp_visible(&plain, self.width.saturating_sub(1).max(1));
        let rendered = if self.color {
            colorize_progress_line(&plain, spinner)
        } else {
            plain
        };
        let mut err = io::stderr().lock();
        write!(err, "\r\x1b[2K{rendered}")?;
        err.flush()?;
        self.active = true;
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let mut err = io::stderr().lock();
        write!(err, "\r\x1b[2K")?;
        err.flush()?;
        self.active = false;
        Ok(())
    }

    fn success(&self, value: &str) -> String {
        super::theme::style(value, super::Tone::Success, self.color)
    }

    fn dim(&self, value: &str) -> String {
        super::theme::style(value, super::Tone::Secondary, self.color)
    }

    fn success_glyph(&self) -> String {
        self.success("◆")
    }
}

fn progress_bar_width(terminal_width: usize) -> usize {
    match terminal_width {
        0..=47 => 5,
        48..=79 => 8,
        80..=99 => 12,
        _ => MAX_BAR_WIDTH,
    }
}

fn non_redundant_detail<'a>(label: &str, detail: &'a str) -> Option<&'a str> {
    let detail = detail.trim();
    if detail.eq_ignore_ascii_case(label.trim()) {
        return None;
    }
    if detail.len() >= label.len()
        && detail
            .get(..label.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(label))
    {
        let remainder = detail[label.len()..]
            .trim_start_matches(|ch: char| matches!(ch, ' ' | '·' | ':' | '-' | '—'));
        return (!remainder.is_empty()).then_some(remainder);
    }
    Some(detail)
}

impl Drop for ProgressLine {
    fn drop(&mut self) {
        let _ = self.clear();
    }
}

fn boot_progress(status: &LifecycleStatus) -> (String, Option<f64>, Option<String>) {
    match status {
        LifecycleStatus::Starting { startup, .. } => {
            let within = match (startup.chains_done, startup.chains_total) {
                (Some(done), Some(total)) if total > 0 => done as f64 / total as f64,
                _ => 0.0,
            };
            let (stage, label) = startup_phase(startup.phase);
            let ratio = ((stage as f64) + within.clamp(0.0, 1.0)) / 9.0;
            let detail = match (startup.chains_done, startup.chains_total) {
                (Some(done), Some(total)) => Some(format!(
                    "{}/{} chains",
                    grouped(done as usize),
                    grouped(total as usize)
                )),
                _ => startup.message.clone(),
            };
            (label.to_string(), Some(ratio.clamp(0.0, 1.0)), detail)
        }
        LifecycleStatus::Running { .. } => ("publishing readiness".to_string(), Some(1.0), None),
        LifecycleStatus::Stopped { .. } | LifecycleStatus::Stale { .. } => {
            ("launching daemon".to_string(), Some(0.0), None)
        }
        LifecycleStatus::Unresponsive { .. } => {
            ("waiting for lifecycle control".to_string(), None, None)
        }
        LifecycleStatus::Failed { startup, .. } => (
            format!("failed during {}", startup.phase.as_str()),
            None,
            startup.error.clone(),
        ),
        LifecycleStatus::NotInitialized { .. } => {
            ("node is not initialized".to_string(), Some(0.0), None)
        }
    }
}

fn shutdown_progress(status: &LifecycleStatus) -> (String, Option<f64>, Option<String>) {
    match status {
        LifecycleStatus::Running { metadata, .. } => (
            "draining daemon".to_string(),
            None,
            metadata.pid.map(|pid| format!("pid {pid}")),
        ),
        LifecycleStatus::Starting { .. } => ("stopping boot sequence".to_string(), None, None),
        LifecycleStatus::Unresponsive { .. } => {
            ("waiting for process exit".to_string(), None, None)
        }
        LifecycleStatus::Failed { .. } => ("stopping failed daemon".to_string(), None, None),
        LifecycleStatus::Stopped { .. }
        | LifecycleStatus::Stale { .. }
        | LifecycleStatus::NotInitialized { .. } => {
            ("state lock released".to_string(), Some(1.0), None)
        }
    }
}

fn startup_phase(phase: StartupPhase) -> (usize, &'static str) {
    match phase {
        StartupPhase::Bootstrapping => (0, "loading verified node configuration"),
        StartupPhase::OpeningProjection => (1, "opening thread projection"),
        StartupPhase::RebuildingProjection => (2, "rebuilding thread projection"),
        StartupPhase::ReplayingHeadChanges => (3, "replaying durable head changes"),
        StartupPhase::RecoveringSchedulerProjection => (4, "recovering scheduler projection"),
        StartupPhase::ReconcilingThreads => (5, "reconciling thread state"),
        StartupPhase::ReconcilingFollow => (6, "reconciling follow state"),
        StartupPhase::ReconcilingScheduler => (7, "arming scheduler"),
        StartupPhase::Ready => (9, "publishing readiness"),
        StartupPhase::Failed => (8, "startup failed"),
    }
}

fn gc_progress(progress: &OfflineThreadHistoryGcProgress) -> (&'static str, Option<f64>) {
    let within = match (progress.completed, progress.total) {
        (Some(completed), Some(total)) if total > 0 => completed as f64 / total as f64,
        (Some(_), Some(0)) => 1.0,
        _ => 0.0,
    };
    let (label, ratio) = match progress.phase {
        OfflineThreadHistoryGcPhase::CapturingAuthority => ("capturing storage authority", 0.03),
        OfflineThreadHistoryGcPhase::InspectingHistory => ("verifying discard set", 0.08),
        OfflineThreadHistoryGcPhase::PublishingIntent => ("publishing recovery marker", 0.12),
        OfflineThreadHistoryGcPhase::RetiringChainHeads => (
            "retiring thread history",
            0.15 + within.clamp(0.0, 1.0) * 0.60,
        ),
        OfflineThreadHistoryGcPhase::RebuildingProjection => ("publishing empty projection", 0.78),
        OfflineThreadHistoryGcPhase::ClearingRuntime => ("clearing execution recovery", 0.84),
        OfflineThreadHistoryGcPhase::ClearingScheduler => ("clearing scheduler history", 0.90),
        OfflineThreadHistoryGcPhase::Finalizing => ("committing maintenance state", 0.95),
        OfflineThreadHistoryGcPhase::SweepingCas => ("sweeping unreachable CAS data", 0.97),
        OfflineThreadHistoryGcPhase::Complete => ("maintenance complete", 1.0),
    };
    (label, Some(ratio))
}

fn determinate_bar(ratio: f64, width: usize) -> String {
    let filled = (ratio.clamp(0.0, 1.0) * width as f64).round() as usize;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

fn pulse_bar(frame: usize, width: usize) -> String {
    let pulse = 4.min(width);
    let span = width.saturating_sub(pulse).max(1);
    let cycle = span.saturating_mul(2);
    let offset = frame % cycle.max(1);
    let start = if offset <= span {
        offset
    } else {
        cycle - offset
    };
    let mut bar = String::with_capacity(width + 2);
    bar.push('[');
    for index in 0..width {
        bar.push(if (start..start + pulse).contains(&index) {
            '█'
        } else {
            '░'
        });
    }
    bar.push(']');
    bar
}

fn colorize_progress_line(line: &str, spinner: char) -> String {
    let remainder = line.strip_prefix(spinner).unwrap_or(line);
    let spinner = super::theme::style(&spinner.to_string(), super::Tone::Active, true);
    let remainder = super::theme::style(remainder, super::Tone::Secondary, true);
    format!("{spinner}{remainder}")
}

fn human_duration(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.1}s", millis as f64 / 1_000.0)
    } else {
        let seconds = duration.as_secs();
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

fn grouped(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index != 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bars_have_stable_width_and_clamp() {
        assert_eq!(determinate_bar(-1.0, 4), "[░░░░]");
        assert_eq!(determinate_bar(0.5, 4), "[██░░]");
        assert_eq!(determinate_bar(2.0, 4), "[████]");
        assert_eq!(pulse_bar(3, 8).chars().count(), 10);
        assert_eq!(progress_bar_width(79), 8);
        assert_eq!(progress_bar_width(80), 12);
        assert_eq!(progress_bar_width(120), MAX_BAR_WIDTH);
    }

    #[test]
    fn grouped_counts_are_readable() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(51_886), "51,886");
    }

    #[test]
    fn repeated_progress_detail_does_not_consume_width() {
        assert_eq!(
            non_redundant_detail("opening thread projection", "opening thread projection"),
            None
        );
        assert_eq!(
            non_redundant_detail(
                "opening thread projection",
                "opening thread projection · /data/state"
            ),
            Some("/data/state")
        );
    }
}
