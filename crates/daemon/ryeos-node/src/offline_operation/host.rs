//! What the process applying offline operations supplies to their handlers.
//!
//! Handlers never read the process environment. Anything that depends on how
//! this process was started or built arrives here, from the caller that owns
//! that knowledge: the daemon's startup, or the command-line tool.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use ryeos_app::execution_history_reset::{
    ExecutionHistoryResetProgress, ExecutionHistoryResetReport,
    ExecutionHistoryScratchRepairReport, SchedulerHistoryResetParticipant,
};
use ryeos_node_runtime::identity::NodeIdentity;
use ryeos_node_runtime::state_lock::StateLock;
use serde_json::Value;

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

/// What `node.remote.pin` asks the host to write: the remote's descriptor,
/// pinned under `name` in the node's own remotes file without contacting it.
pub struct RemotePin<'a> {
    pub app_root: &'a Path,
    /// Held by the application for the whole call.
    pub state_lock: &'a StateLock,
    pub name: &'a str,
    /// The descriptor bytes, digest-verified.
    pub descriptor: &'a [u8],
}

/// What `node.bundle.replace` asks the host to do: install the bundle the
/// archive holds under `name`, replacing the installed one, with the same
/// verification a local `bundle install --replace` makes.
pub struct BundleReplace<'a> {
    pub app_root: &'a Path,
    /// Held by the application for the whole call.
    pub state_lock: &'a StateLock,
    pub node: &'a NodeIdentity,
    pub name: &'a str,
    /// A gzip-compressed tar of the bundle tree, digest-verified.
    pub archive: &'a [u8],
    pub preserve_runtime_artifacts: bool,
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

    /// Pin a remote from its descriptor. The remotes file and its schema
    /// belong to a crate above this one, so the host that has it does the
    /// write; a host without it refuses.
    fn pin_remote(&self, _request: RemotePin<'_>) -> Result<Value> {
        bail!("this process cannot pin remotes; the operation is applied when the daemon starts")
    }

    /// Replace a bundle from an archive. Verification needs the node's full
    /// configuration composed as the daemon composes it, so only the daemon's
    /// startup can do it; every other host refuses.
    fn replace_bundle(&self, _request: BundleReplace<'_>) -> Result<Value> {
        bail!(
            "this process cannot replace bundles; the operation is applied when the daemon starts"
        )
    }
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
