//! Real process-scope qualification mode for the end-to-end harness.
//!
//! By default a harness daemon is a direct child of the test process: it has
//! no protected host-runtime association, so the fast fixture signs a node
//! isolation member without the per-scope contract and the suites that need
//! an exclusive session skip when the host cannot provide one. That leaves
//! the exclusive-session lane unexercised.
//!
//! Setting [`REQUIRE_PROCESS_SCOPES_ENV`] to `1` changes three things:
//!
//! * the fast fixture keeps the authored isolation member and capability
//!   floor, including `process_scopes` and the per-scope capability names;
//! * the daemon is started the way an external supervisor starts one: the
//!   test process, running as an administrator on a disposable host, provisions
//!   a delegated cgroup subtree, writes the protected host-runtime binding
//!   and starts `ryeosd host-runtime --binding <path>`, which places the node
//!   in its controller leaf and drops to the node account;
//! * a host that cannot qualify the scopes is a failure ([`skip_or_fail_unqualified`]
//!   panics) and the node's own capability report must show the per-scope
//!   floor entries provided ([`assert_scope_capabilities_provided`]).
//!
//! The mode never weakens an authored profile. It needs an administrator
//! test process, the node account named by [`SCOPE_ACCOUNT_ENV`] as
//! `uid:gid`, and a host with a writable cgroup-v2 hierarchy; without them the
//! start fails with the cause.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context as _, Result};

/// Selects the real process-scope mode when set to `1`.
pub const REQUIRE_PROCESS_SCOPES_ENV: &str = "RYEOS_TEST_REQUIRE_PROCESS_SCOPES";
/// `uid:gid` of the node account the daemon runs as in the real mode.
pub const SCOPE_ACCOUNT_ENV: &str = "RYEOS_TEST_SCOPE_ACCOUNT";

const NODE_MEMORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const NODE_PROCESSES: u32 = 8192;
/// Administrator-owned directory holding the daemon image and bindings. The
/// external controller refuses an image or binding whose directories are not
/// owned by the administrator, so neither can sit in the test's temp tree.
const ADMIN_ROOT: &str = "/var/lib/ryeos-e2e-scopes";

/// Whether this run requires real process scopes.
pub fn required() -> bool {
    match std::env::var(REQUIRE_PROCESS_SCOPES_ENV).as_deref() {
        Ok("1") => true,
        Ok("") | Ok("0") | Err(std::env::VarError::NotPresent) => false,
        Ok(other) => panic!("{REQUIRE_PROCESS_SCOPES_ENV} must be `1` or unset, not `{other}`"),
        Err(error) => panic!("{REQUIRE_PROCESS_SCOPES_ENV} is unreadable: {error}"),
    }
}

/// The one place a suite reports that its host cannot qualify process scopes.
/// Without the real mode this prints the reason so the caller can skip; with
/// it, an unqualified host is a failure, never a skip.
pub fn skip_or_fail_unqualified(reason: &str) {
    if required() {
        panic!(
            "{REQUIRE_PROCESS_SCOPES_ENV}=1 but this host cannot qualify process scopes: {reason}"
        );
    }
    eprintln!("skipped: this host cannot qualify process scopes: {reason}");
}

struct Registration {
    binding: PathBuf,
    daemon: PathBuf,
    owned: Vec<PathBuf>,
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Registration>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Registration>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn account() -> Result<(u32, u32)> {
    let raw = std::env::var(SCOPE_ACCOUNT_ENV).with_context(|| {
        format!("{SCOPE_ACCOUNT_ENV} (uid:gid of the node account) is required")
    })?;
    let (uid, gid) = raw
        .split_once(':')
        .with_context(|| format!("{SCOPE_ACCOUNT_ENV} must be `uid:gid`"))?;
    Ok((uid.parse()?, gid.parse()?))
}

fn chown_tree(paths: &[PathBuf], uid: u32, gid: u32) -> Result<()> {
    for path in paths {
        let output = std::process::Command::new("chown")
            .arg("-R")
            .arg(format!("{uid}:{gid}"))
            .arg(path)
            .output()
            .context("run chown")?;
        anyhow::ensure!(
            output.status.success(),
            "chown -R {uid}:{gid} {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Copy the daemon image under the administrator-owned directory, once per
/// source image, and return its path.
fn installed_daemon() -> Result<PathBuf> {
    let source = super::ryeosd_binary();
    let metadata =
        std::fs::metadata(&source).with_context(|| format!("stat {}", source.display()))?;
    let modified = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let directory = Path::new(ADMIN_ROOT).join("bin");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let installed = directory.join(format!("ryeosd-{}-{modified}", metadata.len()));
    if !installed.exists() {
        let staging = directory.join(format!(".ryeosd-{}", std::process::id()));
        std::fs::copy(&source, &staging)
            .with_context(|| format!("copy {} to {}", source.display(), staging.display()))?;
        std::fs::rename(&staging, &installed)
            .with_context(|| format!("publish {}", installed.display()))?;
    }
    Ok(installed)
}

/// A harness path whose test channel (an inherited descriptor, extra
/// arguments or extra environment) the protected host-runtime entry cannot
/// carry: the controller hands the node only its app root and an empty
/// environment. In the real mode such a start fails with the cause, so a test
/// that needs the channel is never silently run without it.
pub fn refuse_unsupported_channel(what: &str) -> Result<()> {
    anyhow::ensure!(
        !required(),
        "{what} cannot run in the real process-scope mode: the protected host-runtime entry carries no inherited descriptor, argument or environment into the node"
    );
    Ok(())
}

/// In the real mode, fail when a test's launch tweak added anything the
/// protected host-runtime entry would silently drop. `HOSTNAME` is the one
/// accepted variable: the node does not read it.
pub fn reject_dropped_tweaks(command: &tokio::process::Command) -> Result<()> {
    if !required() {
        return Ok(());
    }
    let std_command = command.as_std();
    let extra_args: Vec<String> = std_command
        .get_args()
        .skip(3)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let extra_env: Vec<String> = std_command
        .get_envs()
        .filter(|(name, _)| *name != std::ffi::OsStr::new("HOSTNAME"))
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    anyhow::ensure!(
        extra_args.is_empty() && extra_env.is_empty(),
        "the test added launch arguments {extra_args:?} or environment {extra_env:?} that the protected host-runtime entry cannot carry into the node"
    );
    Ok(())
}

/// A temporary directory the node can use. A directory the test creates is
/// owned by the test process, which in the real mode is an administrator, and
/// the node account could not enter it; here it belongs to the node account.
/// Without the real mode this is an ordinary temporary directory.
pub fn tempdir() -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    if required() {
        let (uid, gid) = account()?;
        chown_tree(&[directory.path().to_path_buf()], uid, gid)?;
    }
    Ok(directory)
}

/// Provision the delegation and protected binding for the node at
/// `state_path` and make `owned` (every tree the node reads or writes) belong
/// to the node account. Called once, before the first start.
pub fn prepare(state_path: &Path, owned: &[&Path]) -> Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    lillux::require_administrator().with_context(|| {
        format!("{REQUIRE_PROCESS_SCOPES_ENV}=1 needs the test process to run as an administrator")
    })?;
    let (uid, gid) = account()?;
    let mut owned: Vec<PathBuf> = owned.iter().map(|path| path.to_path_buf()).collect();
    owned.push(state_path.to_path_buf());
    chown_tree(&owned, uid, gid)?;

    let account = lillux::ControllerAccount::unix(uid, gid);
    let node_resources = lillux::ProcessScopeResourceLimits {
        maximum_memory_bytes: NODE_MEMORY_BYTES,
        maximum_processes: NODE_PROCESSES,
    };
    let label = format!(
        "ryeos-e2e-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::SeqCst)
    );
    let process_scopes = lillux::ProcessScopeConfiguration::provision_host_delegation(&label)
        .map_err(anyhow::Error::msg)
        .context("provision the delegated cgroup subtree")?;
    let app_root =
        lillux::PinnedDirectory::open(state_path)?.context("the node app root is absent")?;
    let identity_path = state_path
        .join(ryeos_engine::AI_DIR)
        .join("node/identity/public-identity.json");
    let identity: ryeos_node_runtime::identity::PublicIdentityDoc = serde_json::from_slice(
        &std::fs::read(&identity_path)
            .with_context(|| format!("read {}", identity_path.display()))?,
    )?;
    let binding = ryeos_node::host_runtime::HostRuntimeBinding::capture(
        &app_root,
        identity.verified_fingerprint()?,
        account,
        node_resources,
        process_scopes,
    )
    .context("capture the host-runtime binding")?;
    let bindings = Path::new(ADMIN_ROOT).join("bindings");
    std::fs::create_dir_all(&bindings).with_context(|| format!("create {}", bindings.display()))?;
    let binding_path = bindings.join(format!("{label}.json"));
    std::fs::write(&binding_path, serde_json::to_vec(&binding)?)
        .with_context(|| format!("write {}", binding_path.display()))?;
    registry().lock().unwrap().insert(
        state_path.to_path_buf(),
        Registration {
            binding: binding_path,
            daemon: installed_daemon()?,
            owned,
        },
    );
    Ok(())
}

/// The command that starts the node at `state_path` under its protected
/// binding, or `None` when the node is not in the real mode. Files the test
/// process wrote since the last start are handed back to the node account
/// first.
pub fn launch_command(state_path: &Path) -> Result<Option<tokio::process::Command>> {
    let registry = registry().lock().unwrap();
    let Some(registration) = registry.get(state_path) else {
        return Ok(None);
    };
    let (uid, gid) = account()?;
    chown_tree(&registration.owned, uid, gid)?;
    let mut command = tokio::process::Command::new(&registration.daemon);
    command
        .arg("host-runtime")
        .arg("--binding")
        .arg(&registration.binding);
    Ok(Some(command))
}

/// Take the node's trees back for the test process, which in the real mode is
/// an administrator. Offline operations that must be owned by the effective
/// user (re-sealing the initialization transaction) need this between stopping
/// the node and starting it again; [`launch_command`] hands the trees back to
/// the node account. A node not in the real mode is untouched.
pub fn reclaim_for_test(state_path: &Path) -> Result<()> {
    let owned = match registry().lock().unwrap().get(state_path) {
        Some(registration) => registration.owned.clone(),
        None => return Ok(()),
    };
    chown_tree(&owned, 0, 0)
}

/// Fail with the cause when the node's own capability report does not show
/// every per-scope floor entry provided. The report is the node's record of
/// what its host proved at start, so this judges the real qualification, not
/// the harness's setup.
pub fn assert_scope_capabilities_provided(state_path: &Path) -> Result<()> {
    let report =
        ryeos_node::capability_report::SignedNodeCapabilityReport::read_verified(state_path)?
            .context("the node recorded no capability report")?
            .report;
    let mut scope_names: Vec<&str> = report
        .floor
        .requires
        .iter()
        .flat_map(|entry| entry.names())
        .map(String::as_str)
        .filter(|name| lillux::process_control::is_scope_capability(name))
        .collect();
    scope_names.sort_unstable();
    scope_names.dedup();
    for needed in ["recovery:scope", "termination:scope"] {
        anyhow::ensure!(
            scope_names.contains(&needed),
            "the node floor does not require {needed}: the authored process-scope contract was not kept (it requires {scope_names:?})"
        );
    }
    let missing: Vec<String> = scope_names
        .iter()
        .copied()
        .filter(|name| !report.provided.contains_key(*name))
        .map(|name| {
            format!(
                "{name}: {}",
                report
                    .missing
                    .get(name)
                    .map(String::as_str)
                    .unwrap_or("not provided")
            )
        })
        .collect();
    anyhow::ensure!(
        missing.is_empty() && report.floor.admitted,
        "the node did not qualify its per-scope capabilities; missing: [{}]; unmet floor: {:?}",
        missing.join("; "),
        report.floor.unmet
    );
    Ok(())
}

/// What the node's report says is missing, for a start that failed.
pub fn describe_missing(state_path: &Path) -> String {
    match ryeos_node::capability_report::SignedNodeCapabilityReport::read_verified(state_path) {
        Ok(Some(signed)) => format!(
            "node capability report: missing {:?}; unmet floor {:?}",
            signed.report.missing, signed.report.floor.unmet
        ),
        Ok(None) => "the node recorded no capability report".to_owned(),
        Err(error) => format!("the node capability report is unreadable: {error:#}"),
    }
}
