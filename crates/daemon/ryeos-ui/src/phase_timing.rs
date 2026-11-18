//! Debug-level wall-time lines for the phases of a request handler.
//!
//! Each line carries the phase's elapsed time and the registered-generation
//! verifications that ran while it was open: how many, and how long they took.
//! The lines use the target `ryeos_ui::timing` and are off at the daemon's
//! default log level; `RUST_LOG=...,ryeos_ui::timing=debug` turns them on.
//!
//! The verification figures come from a process-wide tally, so a phase that
//! overlaps another request's work also counts that request's verifications.

use ryeos_engine::isolation::{GenerationVerificationTotals, generation_verification_totals};

/// One open phase. The line is written when the value is dropped, so a phase
/// that ends in an error is still reported.
pub(crate) struct PhaseTimer {
    request: &'static str,
    phase: &'static str,
    started: std::time::Instant,
    verifications: GenerationVerificationTotals,
}

impl PhaseTimer {
    pub(crate) fn start(request: &'static str, phase: &'static str) -> Self {
        Self {
            request,
            phase,
            started: std::time::Instant::now(),
            verifications: generation_verification_totals(),
        }
    }
}

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        let verifications = generation_verification_totals().since(self.verifications);
        tracing::debug!(
            target: "ryeos_ui::timing",
            request = self.request,
            phase = self.phase,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            generation_verifications = verifications.count,
            generation_verification_ms = verifications.elapsed.as_millis() as u64,
            "request phase timing"
        );
    }
}
