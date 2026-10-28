//! The command line's side of signed offline operations.
//!
//! The lifecycle commands that change a stopped node are request builders:
//! each turns its flags into an operation, arguments and inputs, and hands
//! them to the same library the daemon applies at startup. The library signs
//! the request with the local operator key on this node's volume, checks it
//! with the same envelope and preconditions, runs the same handler under the
//! node's state lock, and leaves the same node-signed receipt in the ledger.
//! What remains here is the host the handlers ask questions of, and the
//! rendering of what they report.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ryeos_app::execution_history_reset::{
    ExecutionHistoryResetProgress as ResetEvent, ExecutionHistoryResetReport,
    ExecutionHistoryScratchRepairReport, SchedulerHistoryResetParticipant,
};
use ryeos_node::offline_operation::{
    ImageSeed, LocalApplied, LocalDraft, OfflineHost, TypedReport, apply_local,
};
use serde_json::Value;

/// The reports a command renders in their own types.
#[derive(Default)]
pub struct Captured {
    pub execution_history: Option<ExecutionHistoryResetReport>,
    pub scratch: Option<ExecutionHistoryScratchRepairReport>,
    pub replay_indexes: Option<ryeos_state::operational::ReplayIndexResetReport>,
}

pub struct CliHost {
    image: Option<ImageSeed>,
    progress: RefCell<Option<crate::tty::ExecutionHistoryResetProgress>>,
    captured: RefCell<Captured>,
}

impl CliHost {
    pub fn new() -> Self {
        Self {
            image: None,
            progress: RefCell::new(None),
            captured: RefCell::new(Captured::default()),
        }
    }

    pub fn with_image_seed(mut self, root: PathBuf, substrate_image_digest: String) -> Self {
        self.image = Some(ImageSeed {
            root,
            substrate_image_digest,
        });
        self
    }

    pub fn with_progress(
        self,
        progress: Option<crate::tty::ExecutionHistoryResetProgress>,
    ) -> Self {
        *self.progress.borrow_mut() = progress;
        self
    }

    /// End the live progress line, if one was drawn.
    pub fn finish_progress(&self) -> Result<()> {
        if let Some(progress) = self.progress.borrow_mut().take() {
            progress.finish()?;
        }
        Ok(())
    }

    pub fn take_captured(&self) -> Captured {
        std::mem::take(&mut *self.captured.borrow_mut())
    }

    /// Apply one operation to the local stopped node.
    pub fn apply(
        &self,
        app_root: &Path,
        operation: &str,
        arguments: Value,
        inputs: BTreeMap<String, Vec<u8>>,
        expected_result: BTreeMap<String, String>,
    ) -> Result<LocalApplied> {
        apply_local(
            app_root,
            self,
            LocalDraft {
                operation: operation.to_owned(),
                arguments,
                inputs,
                expected_result,
            },
        )
        .with_context(|| format!("offline operation {operation}"))
    }
}

impl Default for CliHost {
    fn default() -> Self {
        Self::new()
    }
}

/// The scheduler's part in an execution-history reset, decided by the node's
/// installed composition as the daemon decides it.
pub fn scheduler_participant(
    app_root: &Path,
) -> Result<Option<Box<dyn SchedulerHistoryResetParticipant>>> {
    let selected = ryeos_api::scheduler_selection::scheduler_selected_for_app_root(
        app_root,
        cfg!(feature = "scheduling"),
    )
    .context("derive scheduler selection from the installed composition")?;
    #[cfg(feature = "scheduling")]
    {
        Ok(selected.then(|| {
            Box::new(ryeos_scheduler_service::SchedulerHistoryReset::new())
                as Box<dyn SchedulerHistoryResetParticipant>
        }))
    }
    #[cfg(not(feature = "scheduling"))]
    {
        // A build without the scheduler already refused a composition that
        // selects it, so there is no participant to take part.
        let _ = selected;
        Ok(None)
    }
}

impl OfflineHost for CliHost {
    fn image_seed(&self) -> Option<ImageSeed> {
        self.image.clone()
    }

    fn scheduler_history_participant(
        &self,
        app_root: &Path,
    ) -> Result<Option<Box<dyn SchedulerHistoryResetParticipant>>> {
        scheduler_participant(app_root)
    }

    fn observe_execution_history_reset(&self, event: &ResetEvent) {
        if let Some(progress) = self.progress.borrow_mut().as_mut() {
            progress.observe(event);
        }
    }

    fn observe_report(&self, report: TypedReport<'_>) {
        let mut captured = self.captured.borrow_mut();
        match report {
            TypedReport::ExecutionHistoryReset(report) => {
                captured.execution_history = Some(report.clone());
            }
            TypedReport::ExecutionHistoryScratch(report) => {
                captured.scratch = Some(report.clone());
            }
            TypedReport::ReplayIndexes(report) => captured.replay_indexes = Some(*report),
        }
    }
}

/// A string field of a handler's JSON result.
pub fn text<'a>(result: &'a Value, key: &str) -> Option<&'a str> {
    result.get(key).and_then(Value::as_str)
}

/// Read a bounded input file for an operation, refusing links.
pub fn read_input(path: &Path, what: &str, max_bytes: u64) -> Result<Vec<u8>> {
    lillux::read_regular_file_bounded_no_follow(path, max_bytes)
        .with_context(|| format!("read {what} {}", path.display()))
}
