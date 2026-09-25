//! Protected external-candidate supervisor executable.
//!
//! A provider adapter may only launch this executable after arranging the
//! fixed descriptor contract below. It accepts no authority through argv,
//! ambient environment, project paths, provider credentials, or node grants.

use ryeos_external_candidate_supervisor::runtime::ExternalCandidateSupervisorOutcome;
use std::io::Write as _;

fn main() {
    match ryeos_external_candidate_supervisor::entrypoint::run_from_inherited() {
        Ok(outcome) => {
            let code = match &outcome {
                ExternalCandidateSupervisorOutcome::ExportApplied
                | ExternalCandidateSupervisorOutcome::CommandTerminatedApplied => 0,
                ExternalCandidateSupervisorOutcome::ExecutionDeadline
                | ExternalCandidateSupervisorOutcome::ChannelExpired => 124,
                ExternalCandidateSupervisorOutcome::RevokedApplied
                | ExternalCandidateSupervisorOutcome::RevokedClaimedUnknown => 75,
                ExternalCandidateSupervisorOutcome::RecoveryOnly { .. } => 76,
            };
            if let Ok(value) = serde_json::to_value(&outcome)
                && let Ok(line) = lillux::canonical_json(&value)
            {
                let mut stdout = std::io::stdout();
                let _ = writeln!(stdout, "{line}");
            }
            std::process::exit(code);
        }
        Err(error) => {
            eprintln!("ryeos-external-candidate-supervisor: {error:#}");
            std::process::exit(126);
        }
    }
}
