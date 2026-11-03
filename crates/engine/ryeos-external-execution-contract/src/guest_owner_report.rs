//! The guest occurrence owner's only stdout: secret-free startup report lines.
//!
//! Each line is one canonical JSON object naming only a closed step word:
//! `{"guest_owner":"phase","step":"<word>"}` as the owner enters a startup
//! step (and again while a long step runs), then exactly one terminal line:
//! `{"guest_owner":"held"}` once the held supervisor is released, or
//! `{"guest_owner":"refused","step":"<word>"}` when a step refused. No error
//! text is ever written, so a lifecycle adapter may relay what it saw to the
//! controller as a closed activation report. Provider stream framing stays
//! with the adapter; this module sees only the owner's stdout bytes.

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

/// One complete owner report line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOwnerReport {
    Phase(GuestOwnerStep),
    Held,
    Refused(GuestOwnerStep),
}

const HELD_LINE: &str = r#"{"guest_owner":"held"}"#;
const PHASE_PREFIX: &str = r#"{"guest_owner":"phase","step":""#;
const REFUSED_PREFIX: &str = r#"{"guest_owner":"refused","step":""#;
const STEP_SUFFIX: &str = r#""}"#;

/// The longest accepted report line, excluding its newline.
pub const MAX_GUEST_OWNER_REPORT_LINE_BYTES: usize = 128;
/// The most report lines the owner's stdout may carry before its terminal
/// line; phase repeats are bounded by the owner's own interval and lifetime.
pub const MAX_GUEST_OWNER_REPORT_LINES: u32 = 8192;

impl GuestOwnerReport {
    /// The exact canonical line, newline included.
    pub fn line(self) -> String {
        match self {
            Self::Phase(step) => format!("{PHASE_PREFIX}{step}{STEP_SUFFIX}\n"),
            Self::Held => format!("{HELD_LINE}\n"),
            Self::Refused(step) => format!("{REFUSED_PREFIX}{step}{STEP_SUFFIX}\n"),
        }
    }

    /// Parse one line without its newline. Only the exact canonical bytes
    /// are accepted.
    pub fn parse_line(line: &str) -> Result<Self> {
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
/// the first terminal line ends the report. Bytes after it are never examined.
#[derive(Debug, Default)]
pub struct GuestOwnerStdout {
    pending: String,
    lines: u32,
    last_phase: Option<GuestOwnerStep>,
}

impl GuestOwnerStdout {
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recent phase line seen.
    pub fn last_phase(&self) -> Option<GuestOwnerStep> {
        self.last_phase
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
                GuestOwnerReport::Held => return Ok(Some(GuestOwnerTerminal::Held)),
                GuestOwnerReport::Refused(step) => {
                    return Ok(Some(GuestOwnerTerminal::Refused(step)));
                }
            }
            chunk = &chunk[end + 1..];
        }
    }
}

/// What a lifecycle adapter observed of the owner during first activation,
/// with the last phase the owner reported. It is transport testimony only:
/// `held` is not Ready, and no other outcome proves the supervisor absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestOwnerActivationReport {
    Held {
        last_phase: Option<GuestOwnerStep>,
    },
    Refused {
        step: GuestOwnerStep,
        last_phase: Option<GuestOwnerStep>,
    },
    /// The run stream carried the command's exit before any terminal line.
    ExitedBeforeReport {
        last_phase: Option<GuestOwnerStep>,
    },
    /// The owner's stdout was not exact report lines.
    MalformedReport {
        last_phase: Option<GuestOwnerStep>,
    },
    /// The activation deadline arrived with the stream still open.
    NoReportBeforeDeadline {
        last_phase: Option<GuestOwnerStep>,
    },
    /// The stream ended, failed or exceeded its bound before a terminal line.
    StreamLost {
        last_phase: Option<GuestOwnerStep>,
    },
}

impl GuestOwnerActivationReport {
    pub fn last_phase(&self) -> Option<GuestOwnerStep> {
        match *self {
            Self::Held { last_phase }
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
        assert!(!GuestOwnerActivationReport::Held { last_phase: None }.owner_ended_without_held());
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
