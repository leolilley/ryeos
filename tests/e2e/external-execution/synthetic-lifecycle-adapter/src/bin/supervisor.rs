use std::io::Write as _;

use ryeos_external_candidate_supervisor::runtime::ExternalCandidateSupervisorOutcome;

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
                let _ = writeln!(std::io::stdout(), "{line}");
            }
            std::process::exit(code);
        }
        Err(error) => {
            eprintln!("ryeos-synthetic-external-candidate-supervisor: {error:#}");
            std::process::exit(126);
        }
    }
}
