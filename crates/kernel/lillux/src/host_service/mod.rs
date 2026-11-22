//! Native host-service integration behind one platform-neutral lifecycle
//! contract.
//!
//! A client supplies an exact administrator-approved launch specification and
//! receives an opaque, protected state directory. The native adapter owns
//! service-manager files, activation, and narrow control-channel delegation;
//! the client owns durable lifecycle semantics in that state directory.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::{ControllerAccount, PinnedDirectory};

mod installation;
mod runit;

/// Exact host-selected process image for one supervised service. This is an
/// administrator-only host-maintenance input, never node policy or a worker
/// request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostServiceLaunch {
    pub schema_version: u32,
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub account: ControllerAccount,
}

/// Native supervision operations expressed without manager-specific terms.
pub trait HostServiceController: Send + Sync {
    fn check_available(&self) -> Result<()>;
    fn request_up(&self) -> Result<()>;
    fn request_down(&self) -> Result<()>;
    /// Want the service down without signalling a running process: the
    /// supervisor does not restart it after its next exit. For a supervised
    /// process that is already exiting on its own and must not be restarted.
    fn inhibit_restart(&self) -> Result<()>;
    /// Want the service down and have the supervisor end its exact process
    /// with the definitive signal. The supervisor owns that process identity,
    /// so no pid read from elsewhere is promoted into signal authority. Only
    /// for a process a graceful request could not end.
    fn force_down(&self) -> Result<()>;
    /// Want the service down and return only once the supervisor reports its
    /// process gone, or refuse when `timeout` passes first. A down request
    /// alone returns while the process may still be exiting.
    fn wait_down(&self, timeout: std::time::Duration) -> Result<()>;
    /// Opaque native boot disposition retained by a client's upgrade journal.
    fn capture_upgrade_state(&self) -> Result<serde_json::Value>;
    fn restore_upgrade_state(&self, state: &serde_json::Value) -> Result<()>;
}

/// One exact native service association. `state_directory` remains root-owned;
/// the native adapter may grant the selected controller read/traversal access
/// to public association testimony and a narrowly delegated private child.
pub struct HostServiceInstallation {
    pub state_directory: PinnedDirectory,
    /// Exact native launch contract recovered from the administrator-owned
    /// service definition. Clients must compare this with their own state
    /// before trusting the returned directory.
    pub launch: HostServiceLaunch,
    pub controller: Box<dyn HostServiceController>,
}

/// Discover one existing native service. An occupied but malformed native
/// association is an error, never permission to spawn directly.
pub fn discover_host_service(name: &str) -> Result<Option<HostServiceInstallation>> {
    runit::discover(name)
}

/// Publish one administrator-approved native service realization. It remains
/// down until its client requests its ordinary lifecycle transition.
pub fn provision_host_service(name: &str, launch: &HostServiceLaunch) -> Result<()> {
    runit::provision(name, launch)
}

/// One native service association found by enumeration. Enumeration reads
/// only the recorded launch; it never validates that the launch can still run,
/// so an association whose client is gone is still named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostServiceAssociation {
    pub name: String,
    /// The recorded launch, or why it could not be read. An association
    /// without a readable launch record is never retired by name.
    pub launch: std::result::Result<HostServiceLaunch, String>,
    /// Whether the service manager is currently set to supervise it.
    pub activated: bool,
}

/// Enumerate native service associations whose name starts with `prefix`.
/// `None` means this host has no readable native service directory: the
/// service manager is absent, or its directory is not readable by the caller.
pub fn list_host_services(prefix: &str) -> Result<Option<Vec<HostServiceAssociation>>> {
    runit::list(prefix)
}

/// Retire one native service association: bring it down, deactivate it, wait
/// (bounded) for the service manager to release it, then remove its service
/// definition and its logs. `expected` must equal the launch the association
/// records, so a caller retires exactly what it inspected. `Ok(false)` means
/// no association existed; leftovers of an interrupted retirement are removed.
pub fn retire_host_service(name: &str, expected: &HostServiceLaunch) -> Result<bool> {
    runit::retire(name, expected)
}

pub use installation::{
    exec_install_transaction, run_as_administrator, validate_install_transaction,
};
