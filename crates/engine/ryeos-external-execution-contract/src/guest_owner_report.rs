//! The guest occurrence owner's only stdout: secret-free startup report lines.
//!
//! Each line is one canonical JSON object naming only a closed step word:
//! `{"guest_owner":"phase","step":"<word>"}` as the owner enters a startup
//! step (and again while a long step runs), then exactly one terminal line:
//! `{"guest_owner":"held"}` once the held supervisor is released, or
//! `{"guest_owner":"refused","step":"<word>"}` when a step refused. Before
//! the terminal line no error text is ever written. After `held` the owner
//! keeps supervising the released supervisor and writes one more line when it
//! exits: `{"guest_owner":"supervisor_exit","status":{"code":N},"stderr_tail":"..."}`
//! (or `{"signal":N}`), carrying a bounded, sanitized tail of the supervisor's
//! standard error. That tail is untrusted text for logs only; nothing may
//! parse it for authority. Provider stream framing stays with the adapter;
//! this module sees only the owner's stdout bytes.

use std::fmt;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// One owner startup step: the phase being entered, or the step that refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestOwnerStep {
    AssignmentArgument,
    AccountTransition,
    RuntimeRoot,
    PrivateRoots,
    ImportRead,
    ImportVerify,
    AdmissionExpired,
    PackageIdentity,
    OccurrenceRoot,
    PrivateSourceEntry,
    PathRebind,
    PrivateSourceCeiling,
    Stage,
    Install,
    LaunchPreparation,
    HeldPrepare,
    MountPreparation,
    SignalRegistration,
    Release,
    SupervisorExit,
    Lifetime,
    Other,
}

impl GuestOwnerStep {
    /// Every step in startup order.
    pub const ALL: [Self; 22] = [
        Self::AssignmentArgument,
        Self::AccountTransition,
        Self::RuntimeRoot,
        Self::PrivateRoots,
        Self::ImportRead,
        Self::ImportVerify,
        Self::AdmissionExpired,
        Self::PackageIdentity,
        Self::OccurrenceRoot,
        Self::PrivateSourceEntry,
        Self::PathRebind,
        Self::PrivateSourceCeiling,
        Self::Stage,
        Self::Install,
        Self::LaunchPreparation,
        Self::HeldPrepare,
        Self::MountPreparation,
        Self::SignalRegistration,
        Self::Release,
        Self::SupervisorExit,
        Self::Lifetime,
        Self::Other,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AssignmentArgument => "assignment_argument",
            Self::AccountTransition => "account_transition",
            Self::RuntimeRoot => "runtime_root",
            Self::PrivateRoots => "private_roots",
            Self::ImportRead => "import_read",
            Self::ImportVerify => "import_verify",
            Self::AdmissionExpired => "admission_expired",
            Self::PackageIdentity => "package_identity",
            Self::OccurrenceRoot => "occurrence_root",
            Self::PrivateSourceEntry => "private_source_entry",
            Self::PathRebind => "path_rebind",
            Self::PrivateSourceCeiling => "private_source_ceiling",
            Self::Stage => "stage",
            Self::Install => "install",
            Self::LaunchPreparation => "launch_preparation",
            Self::HeldPrepare => "held_prepare",
            Self::MountPreparation => "mount_preparation",
            Self::SignalRegistration => "signal_registration",
            Self::Release => "release",
            Self::SupervisorExit => "supervisor_exit",
            Self::Lifetime => "lifetime",
            Self::Other => "other",
        }
    }

    pub fn from_word(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|step| step.as_str() == word)
    }

    /// The class attached to an error, or `Other` when no step classified it.
    pub fn of(error: &anyhow::Error) -> Self {
        error.downcast_ref::<Self>().copied().unwrap_or(Self::Other)
    }
}

impl fmt::Display for GuestOwnerStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for GuestOwnerStep {}

/// Attach a refusal step to a failing owner step. An error already
/// classified where it was created keeps that step.
pub trait RefusedAt<T> {
    fn refused_at(self, step: GuestOwnerStep) -> Result<T>;
}

impl<T> RefusedAt<T> for Result<T> {
    fn refused_at(self, step: GuestOwnerStep) -> Result<T> {
        self.map_err(|error| {
            if error.downcast_ref::<GuestOwnerStep>().is_some() {
                error
            } else {
                error.context(step)
            }
        })
    }
}

/// How the released supervisor ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestSupervisorStatus {
    Code(i32),
    Signal(i32),
}

impl fmt::Display for GuestSupervisorStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(code) => write!(formatter, "with code {code}"),
            Self::Signal(signal) => write!(formatter, "on signal {signal}"),
        }
    }
}

/// The longest stderr tail a supervisor exit report carries, in bytes.
pub const MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES: usize = 4096;
/// The longest accepted supervisor exit line, excluding its newline: the tail
/// at its bound with every byte escaped, plus the fixed framing.
pub const MAX_GUEST_SUPERVISOR_EXIT_LINE_BYTES: usize =
    2 * MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES + 256;

/// The released supervisor's end, as the owner observed it. The tail is
/// untrusted text for logging only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestSupervisorExit {
    pub status: GuestSupervisorStatus,
    pub stderr_tail: String,
}

impl GuestSupervisorExit {
    /// Build a report from raw stderr bytes: invalid UTF-8 is replaced,
    /// control characters never survive (a newline becomes a visible
    /// separator), and only the last bytes within the bound are kept.
    pub fn from_raw_tail(status: GuestSupervisorStatus, raw: &[u8]) -> Self {
        const OMITTED: &str = "\u{2026}";
        let mut text = String::new();
        for character in String::from_utf8_lossy(raw).trim_end().chars() {
            match character {
                '\n' => text.push_str(" | "),
                character if character.is_control() => text.push('?'),
                character => text.push(character),
            }
        }
        if text.len() > MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES {
            let mut start = text.len() - (MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES - OMITTED.len());
            while !text.is_char_boundary(start) {
                start += 1;
            }
            text = format!("{OMITTED}{}", &text[start..]);
        }
        Self {
            status,
            stderr_tail: text,
        }
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.stderr_tail.len() <= MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES
                && !self.stderr_tail.chars().any(char::is_control),
            "guest supervisor stderr tail is not bounded printable text"
        );
        Ok(())
    }

    fn line(&self) -> String {
        format!(
            "{}\n",
            serde_json::to_string(&SupervisorExitLine {
                guest_owner: "supervisor_exit",
                status: self.status,
                stderr_tail: &self.stderr_tail,
            })
            .expect("supervisor exit line serializes")
        )
    }

    fn parse_line(line: &str) -> Result<Self> {
        ensure!(
            line.len() <= MAX_GUEST_SUPERVISOR_EXIT_LINE_BYTES,
            "guest supervisor exit line exceeds its bound"
        );
        let parsed: OwnedSupervisorExitLine = serde_json::from_str(line)
            .map_err(|_| anyhow::anyhow!("guest supervisor exit line is not exact"))?;
        let exit = Self {
            status: parsed.status,
            stderr_tail: parsed.stderr_tail,
        };
        exit.validate()?;
        ensure!(
            parsed.guest_owner == "supervisor_exit" && exit.line().trim_end() == line,
            "guest supervisor exit line is not canonical"
        );
        Ok(exit)
    }
}

#[derive(Serialize)]
struct SupervisorExitLine<'a> {
    guest_owner: &'static str,
    status: GuestSupervisorStatus,
    stderr_tail: &'a str,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedSupervisorExitLine {
    guest_owner: String,
    status: GuestSupervisorStatus,
    stderr_tail: String,
}

/// One complete owner report line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestOwnerReport {
    Phase(GuestOwnerStep),
    Held,
    Refused(GuestOwnerStep),
    SupervisorExit(GuestSupervisorExit),
}

const HELD_LINE: &str = r#"{"guest_owner":"held"}"#;
const PHASE_PREFIX: &str = r#"{"guest_owner":"phase","step":""#;
const REFUSED_PREFIX: &str = r#"{"guest_owner":"refused","step":""#;
const EXIT_PREFIX: &str = r#"{"guest_owner":"supervisor_exit""#;
const STEP_SUFFIX: &str = r#""}"#;

/// The longest accepted report line other than a supervisor exit, excluding
/// its newline.
pub const MAX_GUEST_OWNER_REPORT_LINE_BYTES: usize = 128;
/// The most report lines the owner's stdout may carry before its terminal
/// line; phase repeats are bounded by the owner's own interval and lifetime.
pub const MAX_GUEST_OWNER_REPORT_LINES: u32 = 8192;

impl GuestOwnerReport {
    /// The exact canonical line, newline included.
    pub fn line(&self) -> String {
        match self {
            Self::Phase(step) => format!("{PHASE_PREFIX}{step}{STEP_SUFFIX}\n"),
            Self::Held => format!("{HELD_LINE}\n"),
            Self::Refused(step) => format!("{REFUSED_PREFIX}{step}{STEP_SUFFIX}\n"),
            Self::SupervisorExit(exit) => exit.line(),
        }
    }

    /// Parse one line without its newline. Only the exact canonical bytes
    /// are accepted.
    pub fn parse_line(line: &str) -> Result<Self> {
        if line.starts_with(EXIT_PREFIX) {
            return GuestSupervisorExit::parse_line(line).map(Self::SupervisorExit);
        }
        ensure!(
            line.len() <= MAX_GUEST_OWNER_REPORT_LINE_BYTES,
            "guest owner report line exceeds its bound"
        );
        if line == HELD_LINE {
            return Ok(Self::Held);
        }
        let step = |prefix: &str| {
            line.strip_prefix(prefix)
                .and_then(|rest| rest.strip_suffix(STEP_SUFFIX))
                .and_then(GuestOwnerStep::from_word)
        };
        step(PHASE_PREFIX)
            .map(Self::Phase)
            .or_else(|| step(REFUSED_PREFIX).map(Self::Refused))
            .ok_or_else(|| anyhow::anyhow!("guest owner stdout is not an exact report line"))
    }
}

/// The owner's terminal startup line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOwnerTerminal {
    Held,
    Refused(GuestOwnerStep),
}

/// Bounded accumulator over the owner's stdout as delivered in arbitrary
/// chunks. Every complete line must be a report; phases are remembered and
/// the first terminal line ends the report. Bytes after it are never examined
/// here; the rest of the chunk that completed it is kept for a caller that
/// goes on to read what follows a `held` line.
#[derive(Debug, Default)]
pub struct GuestOwnerStdout {
    pending: String,
    lines: u32,
    last_phase: Option<GuestOwnerStep>,
    after_terminal: String,
}

impl GuestOwnerStdout {
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recent phase line seen.
    pub fn last_phase(&self) -> Option<GuestOwnerStep> {
        self.last_phase
    }

    /// The part of the chunk after the terminal line, unexamined.
    pub fn after_terminal(&self) -> &str {
        &self.after_terminal
    }

    /// Append one chunk. Returns the terminal report once its line is complete.
    pub fn push(&mut self, mut chunk: &str) -> Result<Option<GuestOwnerTerminal>> {
        loop {
            let end = chunk.find('\n');
            let taken = &chunk[..end.unwrap_or(chunk.len())];
            ensure!(
                self.pending.len().saturating_add(taken.len()) <= MAX_GUEST_OWNER_REPORT_LINE_BYTES,
                "guest owner stdout exceeds its report line bound"
            );
            self.pending.push_str(taken);
            let Some(end) = end else {
                return Ok(None);
            };
            self.lines += 1;
            ensure!(
                self.lines <= MAX_GUEST_OWNER_REPORT_LINES,
                "guest owner stdout exceeds its report line count"
            );
            let report = GuestOwnerReport::parse_line(&self.pending)?;
            self.pending.clear();
            match report {
                GuestOwnerReport::Phase(step) => self.last_phase = Some(step),
                GuestOwnerReport::Held => {
                    self.after_terminal = chunk[end + 1..].to_owned();
                    return Ok(Some(GuestOwnerTerminal::Held));
                }
                GuestOwnerReport::Refused(step) => {
                    self.after_terminal = chunk[end + 1..].to_owned();
                    return Ok(Some(GuestOwnerTerminal::Refused(step)));
                }
                GuestOwnerReport::SupervisorExit(_) => {
                    anyhow::bail!("guest owner reported a supervisor exit before held")
                }
            }
            chunk = &chunk[end + 1..];
        }
    }
}

/// Bounded accumulator over the owner's stdout after its `held` line. The
/// supervisor shares the owner's stdout, so anything may follow; only the
/// first exact supervisor exit line is taken, and every other line is dropped
/// without being kept.
#[derive(Debug, Default)]
pub struct GuestOwnerPostHeldStdout {
    pending: String,
    discarding: bool,
    lines: u32,
}

impl GuestOwnerPostHeldStdout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one chunk. Returns the supervisor exit once its line is complete.
    pub fn push(&mut self, mut chunk: &str) -> Option<GuestSupervisorExit> {
        loop {
            let end = chunk.find('\n');
            let taken = &chunk[..end.unwrap_or(chunk.len())];
            if !self.discarding {
                if self.pending.len().saturating_add(taken.len())
                    > MAX_GUEST_SUPERVISOR_EXIT_LINE_BYTES
                {
                    self.discarding = true;
                    self.pending.clear();
                } else {
                    self.pending.push_str(taken);
                }
            }
            let end = end?;
            self.lines = self.lines.saturating_add(1);
            let line = std::mem::take(&mut self.pending);
            let discarded = std::mem::take(&mut self.discarding);
            if self.lines > MAX_GUEST_OWNER_REPORT_LINES {
                return None;
            }
            if !discarded
                && let Ok(GuestOwnerReport::SupervisorExit(exit)) =
                    GuestOwnerReport::parse_line(&line)
            {
                return Some(exit);
            }
            chunk = &chunk[end + 1..];
        }
    }
}

/// What a lifecycle adapter observed of the owner during first activation,
/// with the last phase the owner reported. It is transport testimony only:
/// `held` is not Ready, and no other outcome proves the supervisor absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestOwnerActivationReport {
    /// The supervisor was released. `supervisor_exit` is present only when
    /// the owner also reported the supervisor's end while the adapter was
    /// still reading; its absence proves nothing about the supervisor.
    Held {
        last_phase: Option<GuestOwnerStep>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supervisor_exit: Option<GuestSupervisorExit>,
    },
    Refused {
        step: GuestOwnerStep,
        last_phase: Option<GuestOwnerStep>,
    },
    /// The run stream carried the command's exit before any terminal line.
    ExitedBeforeReport { last_phase: Option<GuestOwnerStep> },
    /// The owner's stdout was not exact report lines.
    MalformedReport { last_phase: Option<GuestOwnerStep> },
    /// The activation deadline arrived with the stream still open.
    NoReportBeforeDeadline { last_phase: Option<GuestOwnerStep> },
    /// The stream ended, failed or exceeded its bound before a terminal line.
    StreamLost { last_phase: Option<GuestOwnerStep> },
}

impl GuestOwnerActivationReport {
    pub fn last_phase(&self) -> Option<GuestOwnerStep> {
        match *self {
            Self::Held { last_phase, .. }
            | Self::Refused { last_phase, .. }
            | Self::ExitedBeforeReport { last_phase }
            | Self::MalformedReport { last_phase }
            | Self::NoReportBeforeDeadline { last_phase }
            | Self::StreamLost { last_phase } => last_phase,
        }
    }

    /// The owner itself ended startup without reaching held: the supervisor
    /// can no longer be released by this occurrence's owner.
    pub fn owner_ended_without_held(&self) -> bool {
        matches!(
            self,
            Self::Refused { .. } | Self::ExitedBeforeReport { .. } | Self::MalformedReport { .. }
        )
    }
}

struct LastPhase(Option<GuestOwnerStep>);

impl fmt::Display for LastPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(step) => write!(formatter, "last reported {step}"),
            None => formatter.write_str("reported no phase"),
        }
    }
}

impl fmt::Display for GuestOwnerActivationReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let last = LastPhase(self.last_phase());
        match self {
            Self::Held {
                supervisor_exit: Some(exit),
                ..
            } if exit.stderr_tail.is_empty() => {
                write!(
                    formatter,
                    "guest owner held; supervisor exited {}",
                    exit.status
                )
            }
            Self::Held {
                supervisor_exit: Some(exit),
                ..
            } => write!(
                formatter,
                "guest owner held; supervisor exited {}: {}",
                exit.status, exit.stderr_tail
            ),
            Self::Held { .. } => formatter.write_str("guest owner held"),
            Self::Refused { step, .. } => write!(formatter, "guest owner refused at {step}"),
            Self::ExitedBeforeReport { .. } => {
                write!(formatter, "guest owner {last}; exited before held")
            }
            Self::MalformedReport { .. } => {
                write!(formatter, "guest owner {last}; stdout report malformed")
            }
            Self::NoReportBeforeDeadline { .. } => {
                write!(formatter, "guest owner {last}; no held before deadline")
            }
            Self::StreamLost { .. } => {
                write!(formatter, "guest owner {last}; run stream lost before held")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_lines_round_trip_exactly() {
        assert_eq!(
            GuestOwnerReport::Held.line(),
            "{\"guest_owner\":\"held\"}\n"
        );
        assert_eq!(
            GuestOwnerReport::Refused(GuestOwnerStep::Stage).line(),
            "{\"guest_owner\":\"refused\",\"step\":\"stage\"}\n"
        );
        assert_eq!(
            GuestOwnerReport::Phase(GuestOwnerStep::Install).line(),
            "{\"guest_owner\":\"phase\",\"step\":\"install\"}\n"
        );
        let mut reports = vec![GuestOwnerReport::Held];
        reports.extend(GuestOwnerStep::ALL.map(GuestOwnerReport::Refused));
        reports.extend(GuestOwnerStep::ALL.map(GuestOwnerReport::Phase));
        for report in reports {
            let line = report.line();
            assert!(line.len() <= MAX_GUEST_OWNER_REPORT_LINE_BYTES + 1);
            assert_eq!(
                GuestOwnerReport::parse_line(line.strip_suffix('\n').unwrap()).unwrap(),
                report
            );
        }
    }

    #[test]
    fn step_words_match_their_serde_names() {
        for step in GuestOwnerStep::ALL {
            assert_eq!(
                serde_json::to_string(&step).unwrap(),
                format!("\"{}\"", step.as_str())
            );
        }
        for (index, step) in GuestOwnerStep::ALL.into_iter().enumerate() {
            assert_eq!(step as usize, index);
        }
    }

    #[test]
    fn report_parsing_refuses_noncanonical_and_unknown_lines() {
        for line in [
            "",
            "held",
            "{\"guest_owner\": \"held\"}",
            "{\"guest_owner\":\"held\"} ",
            "{\"guest_owner\":\"refused\",\"step\":\"unknown\"}",
            "{\"guest_owner\":\"phase\",\"step\":\"Stage\"}",
            "{\"step\":\"stage\",\"guest_owner\":\"refused\"}",
            "{\"guest_owner\":\"phase\"}",
        ] {
            assert!(GuestOwnerReport::parse_line(line).is_err(), "{line}");
        }
        assert!(GuestOwnerReport::parse_line(&"x".repeat(4096)).is_err());
    }

    #[test]
    fn stdout_accumulator_tracks_phases_until_one_terminal_line() {
        let mut stdout = GuestOwnerStdout::new();
        let phases = format!(
            "{}{}",
            GuestOwnerReport::Phase(GuestOwnerStep::ImportVerify).line(),
            GuestOwnerReport::Phase(GuestOwnerStep::Stage).line()
        );
        assert_eq!(stdout.push(&phases[..30]).unwrap(), None);
        assert_eq!(stdout.last_phase(), None);
        assert_eq!(stdout.push(&phases[30..]).unwrap(), None);
        assert_eq!(stdout.last_phase(), Some(GuestOwnerStep::Stage));
        assert_eq!(stdout.push("{\"guest_owner\"").unwrap(), None);
        assert_eq!(
            stdout.push(":\"held\"}\ntrailing garbage\n").unwrap(),
            Some(GuestOwnerTerminal::Held)
        );
        assert_eq!(stdout.last_phase(), Some(GuestOwnerStep::Stage));
        assert_eq!(stdout.after_terminal(), "trailing garbage\n");
        let mut stdout = GuestOwnerStdout::new();
        let refused = format!(
            "{}{}",
            GuestOwnerReport::Phase(GuestOwnerStep::Install).line(),
            GuestOwnerReport::Refused(GuestOwnerStep::Install).line()
        );
        assert_eq!(
            stdout.push(&refused).unwrap(),
            Some(GuestOwnerTerminal::Refused(GuestOwnerStep::Install))
        );
    }

    #[test]
    fn stdout_accumulator_refuses_garbage_and_oversize() {
        assert!(
            GuestOwnerStdout::new()
                .push("panicked at main.rs\n")
                .is_err()
        );
        assert!(GuestOwnerStdout::new().push("\n").is_err());
        let mut stdout = GuestOwnerStdout::new();
        assert!(stdout.push(&"x".repeat(100)).unwrap().is_none());
        assert!(stdout.push(&"x".repeat(100)).is_err());
        assert!(GuestOwnerStdout::new().push(&"y".repeat(1 << 20)).is_err());
        let phase = GuestOwnerReport::Phase(GuestOwnerStep::Stage).line();
        let mut stdout = GuestOwnerStdout::new();
        for _ in 0..MAX_GUEST_OWNER_REPORT_LINES {
            assert!(stdout.push(&phase).unwrap().is_none());
        }
        assert!(stdout.push(&phase).is_err());
    }

    #[test]
    fn activation_report_is_closed_json_and_names_the_last_phase() {
        let report = GuestOwnerActivationReport::NoReportBeforeDeadline {
            last_phase: Some(GuestOwnerStep::Stage),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            json,
            "{\"outcome\":\"no_report_before_deadline\",\"last_phase\":\"stage\"}"
        );
        assert_eq!(
            serde_json::from_str::<GuestOwnerActivationReport>(&json).unwrap(),
            report
        );
        assert!(
            serde_json::from_str::<GuestOwnerActivationReport>(
                "{\"outcome\":\"held\",\"last_phase\":null,\"detail\":\"x\"}"
            )
            .is_err()
        );
        assert_eq!(
            report.to_string(),
            "guest owner last reported stage; no held before deadline"
        );
        assert_eq!(
            GuestOwnerActivationReport::StreamLost { last_phase: None }.to_string(),
            "guest owner reported no phase; run stream lost before held"
        );
        assert_eq!(
            GuestOwnerActivationReport::Refused {
                step: GuestOwnerStep::Install,
                last_phase: Some(GuestOwnerStep::Install)
            }
            .to_string(),
            "guest owner refused at install"
        );
        assert!(
            GuestOwnerActivationReport::ExitedBeforeReport { last_phase: None }
                .owner_ended_without_held()
        );
        assert!(
            !GuestOwnerActivationReport::Held {
                last_phase: None,
                supervisor_exit: None
            }
            .owner_ended_without_held()
        );
    }

    fn exit(code: i32, tail: &str) -> GuestSupervisorExit {
        GuestSupervisorExit::from_raw_tail(GuestSupervisorStatus::Code(code), tail.as_bytes())
    }

    #[test]
    fn supervisor_exit_line_is_exact_and_round_trips() {
        let report = GuestOwnerReport::SupervisorExit(exit(126, "launcher refused: \"x\\y\""));
        let line = report.line();
        assert_eq!(
            line,
            "{\"guest_owner\":\"supervisor_exit\",\"status\":{\"code\":126},\"stderr_tail\":\"launcher refused: \\\"x\\\\y\\\"\"}\n"
        );
        assert_eq!(
            GuestOwnerReport::parse_line(line.strip_suffix('\n').unwrap()).unwrap(),
            report
        );
        let signal = GuestOwnerReport::SupervisorExit(GuestSupervisorExit::from_raw_tail(
            GuestSupervisorStatus::Signal(9),
            b"",
        ));
        assert_eq!(
            signal.line(),
            "{\"guest_owner\":\"supervisor_exit\",\"status\":{\"signal\":9},\"stderr_tail\":\"\"}\n"
        );
        assert_eq!(
            GuestOwnerReport::parse_line(signal.line().trim_end()).unwrap(),
            signal
        );
    }

    #[test]
    fn supervisor_exit_tail_is_sanitized_and_bounded() {
        let raw = b"first\nsecond\x1b[31m\x00\xff end";
        let sanitized = GuestSupervisorExit::from_raw_tail(GuestSupervisorStatus::Code(1), raw);
        assert_eq!(sanitized.stderr_tail, "first | second?[31m?\u{fffd} end");
        let long = "a".repeat(3 * MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES);
        let bounded = exit(1, &format!("{long}tail"));
        assert!(bounded.stderr_tail.len() <= MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES);
        assert!(bounded.stderr_tail.starts_with('\u{2026}'));
        assert!(bounded.stderr_tail.ends_with("aatail"));
        let wide = exit(1, &"\u{1f980}".repeat(5000));
        assert!(wide.stderr_tail.len() <= MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES);
        let quotes = exit(1, &"\"".repeat(5000));
        assert!(
            GuestOwnerReport::SupervisorExit(quotes).line().len()
                <= MAX_GUEST_SUPERVISOR_EXIT_LINE_BYTES + 1
        );
    }

    #[test]
    fn supervisor_exit_parsing_refuses_malformed_lines() {
        let good = GuestOwnerReport::SupervisorExit(exit(3, "boom")).line();
        let good = good.trim_end();
        assert!(GuestOwnerReport::parse_line(good).is_ok());
        let oversize = format!(
            "{{\"guest_owner\":\"supervisor_exit\",\"status\":{{\"code\":1}},\"stderr_tail\":\"{}\"}}",
            "x".repeat(MAX_GUEST_SUPERVISOR_STDERR_TAIL_BYTES + 1)
        );
        let reordered =
            "{\"guest_owner\":\"supervisor_exit\",\"stderr_tail\":\"\",\"status\":{\"code\":1}}";
        let spaced = good.replace(",\"status\"", ", \"status\"");
        let extra = good.replace("\"boom\"}", "\"boom\",\"extra\":1}");
        for line in [
            oversize.as_str(),
            reordered,
            spaced.as_str(),
            extra.as_str(),
            "{\"guest_owner\":\"supervisor_exit\"}",
            "{\"guest_owner\":\"supervisor_exit\",\"status\":{\"code\":\"1\"},\"stderr_tail\":\"\"}",
            "{\"guest_owner\":\"supervisor_exit\",\"status\":{\"code\":1,\"signal\":2},\"stderr_tail\":\"\"}",
            "{\"guest_owner\":\"supervisor_exit\",\"status\":{\"code\":1},\"stderr_tail\":\"a\\nb\"}",
            "{\"guest_owner\":\"supervisor_exit\",\"status\":{\"code\":1},\"stderr_tail\":\"\\u001b\"}",
        ] {
            assert!(GuestOwnerReport::parse_line(line).is_err(), "{line}");
        }
        // A pre-held accumulator refuses an exit outright.
        assert!(GuestOwnerStdout::new().push(&format!("{good}\n")).is_err());
    }

    #[test]
    fn post_held_accumulator_takes_only_the_first_exact_exit() {
        let line = GuestOwnerReport::SupervisorExit(exit(7, "cause")).line();
        let mut stdout = GuestOwnerPostHeldStdout::new();
        assert_eq!(stdout.push("supervisor noise\n{\"guest"), None);
        assert_eq!(stdout.push(&"y".repeat(20_000)), None);
        assert_eq!(stdout.push("\n"), None);
        assert_eq!(stdout.push(&line[..40]), None, "a split line stays pending");
        assert_eq!(stdout.push(&line[40..]), Some(exit(7, "cause")));
        let mut stdout = GuestOwnerPostHeldStdout::new();
        let refused = GuestOwnerReport::Refused(GuestOwnerStep::SupervisorExit).line();
        assert_eq!(
            stdout.push(&format!("{refused}{line}")),
            Some(exit(7, "cause"))
        );
    }

    #[test]
    fn held_report_carries_its_supervisor_exit() {
        let report = GuestOwnerActivationReport::Held {
            last_phase: Some(GuestOwnerStep::Release),
            supervisor_exit: Some(exit(126, "launcher refused")),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<GuestOwnerActivationReport>(&json).unwrap(),
            report
        );
        assert_eq!(
            report.to_string(),
            "guest owner held; supervisor exited with code 126: launcher refused"
        );
        let silent = GuestOwnerActivationReport::Held {
            last_phase: None,
            supervisor_exit: Some(GuestSupervisorExit::from_raw_tail(
                GuestSupervisorStatus::Signal(9),
                b"",
            )),
        };
        assert_eq!(
            silent.to_string(),
            "guest owner held; supervisor exited on signal 9"
        );
        assert_eq!(
            serde_json::to_string(&GuestOwnerActivationReport::Held {
                last_phase: None,
                supervisor_exit: None
            })
            .unwrap(),
            "{\"outcome\":\"held\",\"last_phase\":null}"
        );
    }

    #[test]
    fn first_refusal_step_wins_and_unclassified_is_other() {
        let error = Err::<(), _>(anyhow::anyhow!("private text"))
            .refused_at(GuestOwnerStep::Stage)
            .refused_at(GuestOwnerStep::Other)
            .unwrap_err();
        assert_eq!(GuestOwnerStep::of(&error), GuestOwnerStep::Stage);
        assert_eq!(
            GuestOwnerStep::of(&anyhow::anyhow!("unclassified")),
            GuestOwnerStep::Other
        );
    }
}
