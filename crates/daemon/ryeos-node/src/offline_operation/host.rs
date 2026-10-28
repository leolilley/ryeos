//! What the process applying offline operations supplies to their handlers.
//!
//! Handlers never read the process environment. Anything that depends on how
//! this process was started or built arrives here, from the caller that owns
//! that knowledge: the daemon's startup, or the command-line tool.

use std::path::{Path, PathBuf};

use anyhow::Result;
use ryeos_app::execution_history_reset::{
    ExecutionHistoryResetProgress, ExecutionHistoryResetReport,
    ExecutionHistoryScratchRepairReport, SchedulerHistoryResetParticipant,
};

/// The immutable image tree a policy replacement is seeded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSeed {
    /// Root of the seed tree (it holds the bundle directories and the
    /// publisher-signed init profiles).
    pub root: PathBuf,
    /// Digest of the substrate image this process runs, `sha256:<hex>`.
    pub substrate_image_digest: String,
}

/// A handler's report in its own type, for a host that renders it.
pub enum TypedReport<'a> {
    ExecutionHistoryReset(&'a ExecutionHistoryResetReport),
    ExecutionHistoryScratch(&'a ExecutionHistoryScratchRepairReport),
    ReplayIndexes(&'a ryeos_state::operational::ReplayIndexResetReport),
}

pub trait OfflineHost {
    /// The image seed this process was started with, if any. Policy
    /// replacement refuses without one.
    fn image_seed(&self) -> Option<ImageSeed>;

    /// The scheduler's part in an execution-history reset: `Some` when the
    /// node's installed composition selects the scheduler, `None` when it
    /// does not, and an error when that cannot be decided or this build
    /// cannot run the scheduler the composition selects.
    fn scheduler_history_participant(
        &self,
        app_root: &Path,
    ) -> Result<Option<Box<dyn SchedulerHistoryResetParticipant>>>;

    /// Called as an execution-history reset advances. Interactive callers
    /// render it; the default ignores it.
    fn observe_execution_history_reset(&self, _event: &ExecutionHistoryResetProgress) {}

    /// Called with a handler's report before it is recorded as JSON. The
    /// receipt carries only the JSON; an interactive host that renders the
    /// typed report keeps it from here.
    fn observe_report(&self, _report: TypedReport<'_>) {}
}

/// A host with no image seed and no scheduler: enough for every operation
/// whose handler needs neither.
#[derive(Debug, Default, Clone, Copy)]
pub struct BareHost;

impl OfflineHost for BareHost {
    fn image_seed(&self) -> Option<ImageSeed> {
        None
    }

    fn scheduler_history_participant(
        &self,
        _app_root: &Path,
    ) -> Result<Option<Box<dyn SchedulerHistoryResetParticipant>>> {
        Ok(None)
    }
}
