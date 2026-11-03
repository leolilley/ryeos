//! The guest occurrence owner's only stdout: one secret-free startup report.
//!
//! The owner writes exactly one canonical JSON line before its supervisor
//! runs: `{"guest_owner":"held"}` once the held supervisor is released, or
//! `{"guest_owner":"refused","step":"<word>"}` when a startup step refused.
//! A refusal carries only a closed step word, never error text, so a
//! lifecycle adapter may relay it to the controller as one stable exit code.
//! Provider stream framing stays with the adapter; this module sees only the
//! owner's stdout bytes.

use std::fmt;

use anyhow::{Result, ensure};

/// The owner startup step that refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOwnerRefusal {
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

impl GuestOwnerRefusal {
    /// Every step in exit-code order.
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

impl fmt::Display for GuestOwnerRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for GuestOwnerRefusal {}

/// Attach a refusal step to a failing owner step. An error already
/// classified where it was created keeps that step.
pub trait RefusedAt<T> {
    fn refused_at(self, step: GuestOwnerRefusal) -> Result<T>;
}

impl<T> RefusedAt<T> for Result<T> {
    fn refused_at(self, step: GuestOwnerRefusal) -> Result<T> {
        self.map_err(|error| {
            if error.downcast_ref::<GuestOwnerRefusal>().is_some() {
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
    Held,
    Refused(GuestOwnerRefusal),
}

const HELD_LINE: &str = r#"{"guest_owner":"held"}"#;
const REFUSED_PREFIX: &str = r#"{"guest_owner":"refused","step":""#;
const REFUSED_SUFFIX: &str = r#""}"#;

/// The longest accepted report line, excluding its newline.
pub const MAX_GUEST_OWNER_REPORT_LINE_BYTES: usize = 128;

impl GuestOwnerReport {
    /// The exact canonical line, newline included.
    pub fn line(self) -> String {
        match self {
            Self::Held => format!("{HELD_LINE}\n"),
            Self::Refused(step) => format!("{REFUSED_PREFIX}{step}{REFUSED_SUFFIX}\n"),
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
        line.strip_prefix(REFUSED_PREFIX)
            .and_then(|rest| rest.strip_suffix(REFUSED_SUFFIX))
            .and_then(GuestOwnerRefusal::from_word)
            .map(Self::Refused)
            .ok_or_else(|| anyhow::anyhow!("guest owner stdout is not an exact report line"))
    }
}

/// Bounded accumulator over the owner's stdout as delivered in arbitrary
/// chunks. The first complete line must be a report; anything else is
/// malformed. Bytes after the first report are never examined.
#[derive(Debug, Default)]
pub struct GuestOwnerStdout {
    pending: String,
}

impl GuestOwnerStdout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one chunk. Returns the report once its line is complete.
    pub fn push(&mut self, chunk: &str) -> Result<Option<GuestOwnerReport>> {
        let end = chunk.find('\n');
        let taken = &chunk[..end.unwrap_or(chunk.len())];
        ensure!(
            self.pending.len().saturating_add(taken.len()) <= MAX_GUEST_OWNER_REPORT_LINE_BYTES,
            "guest owner stdout exceeds its report bound"
        );
        self.pending.push_str(taken);
        match end {
            Some(_) => GuestOwnerReport::parse_line(&self.pending).map(Some),
            None => Ok(None),
        }
    }
}

/// What a lifecycle adapter observed of the owner before it reported held.
/// Each outcome has one stable adapter exit code so the controller can name
/// it without any guest or provider text crossing the adapter boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOwnerActivationFailure {
    /// The owner exited before any report line.
    ExitedUnreported,
    /// The owner's stdout was not an exact report line.
    MalformedReport,
    Refused(GuestOwnerRefusal),
}

/// First adapter exit code of the owner range; the range is contiguous and
/// disjoint from every adapter's own fail-closed classes.
pub const GUEST_OWNER_EXIT_CODE_FIRST: i32 = 38;
const REFUSED_EXIT_CODE_FIRST: i32 = GUEST_OWNER_EXIT_CODE_FIRST + 2;
/// Last adapter exit code of the owner range.
pub const GUEST_OWNER_EXIT_CODE_LAST: i32 =
    REFUSED_EXIT_CODE_FIRST + GuestOwnerRefusal::ALL.len() as i32 - 1;
// Stay below the shell and signal exit codes.
const _: () = assert!(GUEST_OWNER_EXIT_CODE_LAST < 64);

impl GuestOwnerActivationFailure {
    pub const fn adapter_exit_code(self) -> i32 {
        match self {
            Self::ExitedUnreported => GUEST_OWNER_EXIT_CODE_FIRST,
            Self::MalformedReport => GUEST_OWNER_EXIT_CODE_FIRST + 1,
            // `ALL` is in declaration order, so the discriminant is the index.
            Self::Refused(step) => REFUSED_EXIT_CODE_FIRST + step as i32,
        }
    }

    pub fn from_adapter_exit_code(code: i32) -> Option<Self> {
        match code {
            GUEST_OWNER_EXIT_CODE_FIRST => Some(Self::ExitedUnreported),
            code if code == GUEST_OWNER_EXIT_CODE_FIRST + 1 => Some(Self::MalformedReport),
            code if (REFUSED_EXIT_CODE_FIRST..=GUEST_OWNER_EXIT_CODE_LAST).contains(&code) => Some(
                Self::Refused(GuestOwnerRefusal::ALL[(code - REFUSED_EXIT_CODE_FIRST) as usize]),
            ),
            _ => None,
        }
    }
}

impl fmt::Display for GuestOwnerActivationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExitedUnreported => formatter.write_str("guest owner exited before reporting"),
            Self::MalformedReport => formatter.write_str("guest owner report is malformed"),
            Self::Refused(step) => write!(formatter, "guest owner refused at {step}"),
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
            GuestOwnerReport::Refused(GuestOwnerRefusal::Stage).line(),
            "{\"guest_owner\":\"refused\",\"step\":\"stage\"}\n"
        );
        let mut reports = vec![GuestOwnerReport::Held];
        reports.extend(GuestOwnerRefusal::ALL.map(GuestOwnerReport::Refused));
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
    fn report_parsing_refuses_noncanonical_and_unknown_lines() {
        for line in [
            "",
            "held",
            "{\"guest_owner\": \"held\"}",
            "{\"guest_owner\":\"held\"} ",
            "{\"guest_owner\":\"refused\",\"step\":\"unknown\"}",
            "{\"guest_owner\":\"refused\",\"step\":\"Stage\"}",
            "{\"step\":\"stage\",\"guest_owner\":\"refused\"}",
            "{\"guest_owner\":\"refused\"}",
        ] {
            assert!(GuestOwnerReport::parse_line(line).is_err(), "{line}");
        }
        assert!(GuestOwnerReport::parse_line(&"x".repeat(4096)).is_err());
    }

    #[test]
    fn stdout_accumulator_reports_once_across_chunks() {
        let mut stdout = GuestOwnerStdout::new();
        assert_eq!(stdout.push("{\"guest_owner\"").unwrap(), None);
        assert_eq!(stdout.push(":\"he").unwrap(), None);
        assert_eq!(
            stdout.push("ld\"}\ntrailing").unwrap(),
            Some(GuestOwnerReport::Held)
        );
        let mut stdout = GuestOwnerStdout::new();
        assert_eq!(
            stdout
                .push(&GuestOwnerReport::Refused(GuestOwnerRefusal::ImportVerify).line())
                .unwrap(),
            Some(GuestOwnerReport::Refused(GuestOwnerRefusal::ImportVerify))
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
    }

    #[test]
    fn exit_codes_are_contiguous_distinct_and_decode() {
        let mut failures = vec![
            GuestOwnerActivationFailure::ExitedUnreported,
            GuestOwnerActivationFailure::MalformedReport,
        ];
        failures.extend(GuestOwnerRefusal::ALL.map(GuestOwnerActivationFailure::Refused));
        let codes: Vec<i32> = failures.iter().map(|f| f.adapter_exit_code()).collect();
        assert_eq!(
            codes,
            (GUEST_OWNER_EXIT_CODE_FIRST..=GUEST_OWNER_EXIT_CODE_LAST).collect::<Vec<_>>()
        );
        for (index, step) in GuestOwnerRefusal::ALL.into_iter().enumerate() {
            assert_eq!(step as usize, index);
        }
        for (failure, code) in failures.iter().zip(codes) {
            assert_eq!(
                GuestOwnerActivationFailure::from_adapter_exit_code(code),
                Some(*failure)
            );
        }
        for code in [
            0,
            1,
            2,
            25,
            GUEST_OWNER_EXIT_CODE_FIRST - 1,
            GUEST_OWNER_EXIT_CODE_LAST + 1,
            127,
            -1,
        ] {
            assert_eq!(
                GuestOwnerActivationFailure::from_adapter_exit_code(code),
                None
            );
        }
        assert_eq!(
            GuestOwnerActivationFailure::Refused(GuestOwnerRefusal::Install).to_string(),
            "guest owner refused at install"
        );
    }

    #[test]
    fn first_refusal_step_wins_and_unclassified_is_other() {
        let error = Err::<(), _>(anyhow::anyhow!("private text"))
            .refused_at(GuestOwnerRefusal::Stage)
            .refused_at(GuestOwnerRefusal::Other)
            .unwrap_err();
        assert_eq!(GuestOwnerRefusal::of(&error), GuestOwnerRefusal::Stage);
        assert_eq!(
            GuestOwnerRefusal::of(&anyhow::anyhow!("unclassified")),
            GuestOwnerRefusal::Other
        );
    }
}
