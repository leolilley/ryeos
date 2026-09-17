//! Installed OCI prestart hook for the hard-contained workflow product.
//!
//! The runtime supplies bounded standard OCI state on stdin. All kernel and
//! namespace interpretation remains in Lillux. This executable only composes
//! the resulting opaque grant with RyeOS's existing protected binding.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

const APP_ROOT: &str = "/data/app";
const BINDING_DIRECTORY: &str = "/run/ryeos";
const BINDING_NAME: &str = "host-runtime.json";
const CONTROLLER_UID: u32 = 10001;
const CONTROLLER_GID: u32 = 10001;

fn main() -> Result<()> {
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref() != Some(std::ffi::OsStr::new("prestart"))
        || arguments.next().is_some()
    {
        bail!("usage: ryeos-lillux-oci-hook prestart");
    }
    lillux::require_administrator()?;
    let mut bytes = Vec::new();
    std::io::stdin()
        .take((lillux::OciHookState::MAX_DOCUMENT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let state = lillux::OciHookState::parse_bounded(&bytes).map_err(anyhow::Error::msg)?;
    let (process_scopes, lifecycle) =
        lillux::ProcessScopeConfiguration::prepare_oci_hook(&state).map_err(anyhow::Error::msg)?;
    lillux::ProcessScopeConfiguration::enter_oci_mount_namespace(&state)
        .map_err(anyhow::Error::msg)?;

    let app_root = lillux::PinnedDirectory::open(Path::new(APP_ROOT))?
        .context("contained OCI app root is absent")?;
    let account = lillux::ControllerAccount::unix(CONTROLLER_UID, CONTROLLER_GID);
    let binding = ryeos_node::host_runtime::HostRuntimeBinding::capture_oci_observed(
        &app_root,
        PathBuf::from(APP_ROOT),
        account,
        process_scopes,
        lifecycle,
    )?;
    let directory = lillux::PinnedDirectory::open_owned_hierarchy(Path::new(BINDING_DIRECTORY), 0)?
        .context("contained OCI binding directory is absent")?;
    let bytes = lillux::canonical_json(&serde_json::to_value(binding)?)?.into_bytes();
    let existing = directory.open_pinned_regular(BINDING_NAME.as_ref(), false)?;
    if let Some(existing) = &existing {
        existing.require_owner(0)?;
    }
    directory.atomic_write_pinned_if_same(
        BINDING_NAME.as_ref(),
        existing.as_ref(),
        &bytes,
        0o600,
    )?;
    Ok(())
}
