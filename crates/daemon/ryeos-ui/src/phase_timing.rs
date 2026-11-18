//! Debug-level wall-time lines for the phases of a request handler.
//!
//! Each line carries the phase's elapsed time, and for the two kinds of
//! engine work that dominate it, how many ran while the phase was open and how
//! long they took: registered-generation verifications and handler subprocess
//! launches (each parse, compose and effective validation is one launch).
//! The lines use the target `ryeos_ui::timing` and are off at the daemon's
//! default log level; `RUST_LOG=...,ryeos_ui::timing=debug` turns them on.
//!
//! The work figures come from process-wide tallies, so a phase that overlaps
//! another request's work also counts that request's work.

use ryeos_engine::work_tally::{WorkTotals, generation_verification_totals, handler_launch_totals};

/// One open phase. The line is written when the value is dropped, so a phase
/// that ends in an error is still reported.
pub(crate) struct PhaseTimer {
    request: &'static str,
    phase: &'static str,
    started: std::time::Instant,
    verifications: WorkTotals,
    handler_launches: WorkTotals,
}

impl PhaseTimer {
    pub(crate) fn start(request: &'static str, phase: &'static str) -> Self {
        Self {
            request,
            phase,
            started: std::time::Instant::now(),
            verifications: generation_verification_totals(),
            handler_launches: handler_launch_totals(),
        }
    }
}

impl Drop for PhaseTimer {
    fn drop(&mut self) {
        let verifications = generation_verification_totals().since(self.verifications);
        let handler_launches = handler_launch_totals().since(self.handler_launches);
        tracing::debug!(
            target: "ryeos_ui::timing",
            request = self.request,
            phase = self.phase,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            generation_verifications = verifications.count,
            generation_verification_ms = verifications.elapsed.as_millis() as u64,
            handler_launches = handler_launches.count,
            handler_launch_ms = handler_launches.elapsed.as_millis() as u64,
            "request phase timing"
        );
    }
}
