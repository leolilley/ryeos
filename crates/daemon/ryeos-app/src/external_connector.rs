//! Controller-owned local connector for an admitted external candidate.
//!
//! The provider remains a normal local persistent session. Its command-backed
//! execution environment is one exact installed connector, configured through
//! sealed occurrence-private configuration (an exact read-only mount under
//! enforcement, an explicit descriptor link otherwise). Configuration is not
//! persisted; the provider can read its capability and is part of that trust
//! boundary. The relay accepts one
//! authenticated connector process and translates only exec-server protocol
//! bytes to the durable external-execution channel.

use lillux::time::Duration;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context as _, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use zeroize::Zeroizing;

use ryeos_external_execution_contract::{
    MAX_PROVIDER_CONFIGURATION_BYTES, ProviderConfigurationLaunchInput,
    ProviderConfigurationRequest, ProviderConfigurationResponse, ProviderConnectorExecutable,
};

use ryeos_state::external_execution::connector::{
    ExternalConnectorFault, ExternalConnectorHello, ExternalConnectorServerFrame,
    read_external_connector_client_frame, read_external_connector_hello,
    write_external_connector_server_frame,
};

use crate::external_placement::{
    ExternalProtocolOutput, InstalledExternalCandidateConnector, author_external_protocol_input,
    claim_external_protocol_output,
};
use crate::process::IdentityLiveness;
use crate::runtime_db::external_execution::connector::{
    ExternalConnectorPhase, ExternalConnectorPreparation,
};
use crate::state::AppState;
use crate::temp_dir_guard::TempDirGuard;
use crate::vault::external_connector::{
    ExternalConnectorCapability, ExternalConnectorCapabilityAccess,
};

const PROVIDER_CONFIGURATION_REQUEST_FD_ENV: &str = "RYEOS_PROVIDER_CONFIGURATION_REQUEST_FD";
const CONNECTOR_RUNTIME_ENDPOINT_NAME: &str = "external-candidate-connector";
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const RELAY_POLL: Duration = Duration::from_millis(10);
const LOCAL_IO_TIMEOUT: Duration = Duration::from_secs(30);
/// The first protocol reply crosses the external channel in both directions,
/// and each crossing can include a retried read after a dropped connection.
const CONNECTOR_HANDSHAKE_TIMEOUT_SECONDS: u64 = 120;
// Local observation grace, not permission to abandon remote writer ownership.
const RELAY_RETIREMENT_GRACE: Duration = Duration::from_secs(2);

/// Prepared occurrence-private provider configuration and its live relay.
/// Moving the relay into the persistent-session lifelines retains the listener
/// exactly as long as the local provider process can start or use it.
pub struct PreparedExternalCandidateConnector {
    configuration_observation: PreparedProviderConfigurationObservation,
    configuration_link: Option<lillux::EphemeralDescriptorFileLink>,
    descriptors: Vec<lillux::InheritedDescriptorAuthority>,
    mounts: Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
    mount_targets: Vec<lillux::EmptyMountTargetReservation>,
    relay: ExternalConnectorRelayLifeline,
}

/// Exact sealed configuration input held by the existing connector owner.
/// The constructor stays private: public adapter testimony alone cannot create
/// an observation of the bytes delivered to this process occurrence.
pub struct PreparedProviderConfigurationObservation {
    placement_thread_id: String,
    execution_binding_hash: String,
    qualification_home_id: Option<String>,
    input: ProviderConfigurationLaunchInput,
}

impl PreparedProviderConfigurationObservation {
    pub(crate) fn require_for(
        &self,
        placement: &str,
        execution_binding_hash: &str,
        qualification_home_id: &str,
    ) -> Result<&ProviderConfigurationLaunchInput> {
        self.input.validate()?;
        ensure!(
            self.placement_thread_id == placement
                && self.execution_binding_hash == execution_binding_hash
                && self.qualification_home_id.as_deref() == Some(qualification_home_id),
            "provider configuration input changed its original placement or binding"
        );
        Ok(&self.input)
    }
}

impl PreparedExternalCandidateConnector {
    pub fn into_parts(
        self,
    ) -> (
        Vec<ryeos_engine::isolation::IsolationReadOnlyMountAuthority>,
        Vec<lillux::InheritedDescriptorAuthority>,
        Vec<Box<dyn Send + Sync>>,
        crate::persistent_session::PersistentSessionCleanupObserver,
        PreparedProviderConfigurationObservation,
    ) {
        let retirement = Arc::new(ExternalConnectorRetirement {
            relay: self.relay,
            targets: Mutex::new(self.mount_targets),
        });
        let mut lifelines: Vec<Box<dyn Send + Sync>> = vec![Box::new(Arc::clone(&retirement))];
        if let Some(link) = self.configuration_link {
            lifelines.push(Box::new(link));
        }
        let settlement = Arc::new(move || {
            retirement.settle(lillux::time::MonotonicDeadline::after(
                RELAY_RETIREMENT_GRACE,
            ))
        });
        (
            self.mounts,
            self.descriptors,
            lifelines,
            settlement,
            self.configuration_observation,
        )
    }
}

// Keep both obligations alive if the session consumes a failing observer.
struct ExternalConnectorRetirement {
    relay: ExternalConnectorRelayLifeline,
    targets: Mutex<Vec<lillux::EmptyMountTargetReservation>>,
}

impl ExternalConnectorRetirement {
    fn settle(&self, deadline: lillux::time::MonotonicDeadline) -> Result<()> {
        self.relay.retire(deadline)?;
        let mut targets = self.targets.try_lock().map_err(|_| {
            anyhow::anyhow!("external connector mount reservation owner unavailable")
        })?;
        close_mount_targets(&mut targets)
    }
}

fn close_mount_targets(targets: &mut Vec<lillux::EmptyMountTargetReservation>) -> Result<()> {
    let mut failure: Option<anyhow::Error> = None;
    for target in targets.iter_mut() {
        if let Err(error) = target.close() {
            failure = Some(match failure {
                None => error,
                Some(previous) => previous.context(format!("additional mount cleanup: {error:#}")),
            });
        }
    }
    if let Some(error) = failure {
        return Err(error.context("external connector mount reservation settlement failed"));
    }
    targets.clear();
    Ok(())
}

struct ExternalConnectorRelayControl {
    stop: AtomicBool,
    interrupt: Mutex<Option<lillux::LocalDuplexStream>>,
}

struct ExternalConnectorRelayLifeline {
    control: Arc<ExternalConnectorRelayControl>,
    settlement: Mutex<RelaySettlement>,
}

struct RelaySettlement {
    thread: Option<lillux::task::HostTask<bool>>,
    failure: Option<&'static str>,
}

impl ExternalConnectorRelayLifeline {
    fn interrupt(&self, deadline: lillux::time::MonotonicDeadline) -> Result<()> {
        self.control.stop.store(true, Ordering::Release);
        let interrupt = loop {
            match self.control.interrupt.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    anyhow::bail!("external connector interruption owner poisoned")
                }
                Err(std::sync::TryLockError::WouldBlock) if !deadline.has_elapsed() => {
                    lillux::time::sleep(deadline.remaining().min(RELAY_POLL));
                }
                Err(_) => anyhow::bail!("external connector interruption deadline expired"),
            }
        };
        if let Some(interrupt) = interrupt.as_ref() {
            interrupt
                .shutdown()
                .context("interrupt external connector relay")?;
        }
        Ok(())
    }

    fn retire(&self, deadline: lillux::time::MonotonicDeadline) -> Result<()> {
        self.interrupt(deadline)?;
        let mut settlement = self
            .settlement
            .try_lock()
            .map_err(|_| anyhow::anyhow!("external connector settlement owner unavailable"))?;
        if let Some(failure) = settlement.failure {
            anyhow::bail!("{failure}");
        }
        if let Some(thread) = settlement.thread.take() {
            match thread.join_until(deadline) {
                Err(thread) => {
                    settlement.thread = Some(thread);
                    anyhow::bail!(
                        "external connector retirement deadline expired; ownership retained"
                    );
                }
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    settlement.failure = Some("external connector durable closure failed")
                }
                Ok(Err(_)) => {
                    settlement.failure = Some("external connector relay panicked during retirement")
                }
            }
        }
        if let Some(failure) = settlement.failure {
            anyhow::bail!("{failure}");
        }
        Ok(())
    }
}

impl Drop for ExternalConnectorRelayLifeline {
    fn drop(&mut self) {
        // Best-effort interruption only. Destruction is never retirement proof.
        let _ = self.interrupt(lillux::time::MonotonicDeadline::after(Duration::ZERO));
    }
}

fn connector_configuration(
    adapter: &crate::external_placement::InstalledExternalProviderConfiguration,
    connector: &InstalledExternalCandidateConnector,
    executable_delivery: ProviderConnectorExecutable,
    endpoint: &Path,
    placement: &str,
    execution_binding_hash: &str,
    capability: &ExternalConnectorCapability,
) -> Result<ProviderConfigurationResponse> {
    ensure!(
        endpoint.is_absolute(),
        "external connector endpoint must be absolute"
    );
    let endpoint = endpoint
        .to_str()
        .context("external connector endpoint path is not UTF-8")?;
    let connector_executable = connector.captured_executable();
    let request = ProviderConfigurationRequest {
        schema: 1,
        protocol: ryeos_external_execution_contract::PROVIDER_CONFIGURATION_PROTOCOL.into(),
        provider_declaration_id: adapter.declaration().id.clone(),
        connector_executable: executable_delivery,
        connector_endpoint: endpoint.to_owned(),
        placement_thread_id: placement.to_owned(),
        execution_binding_hash: execution_binding_hash.to_owned(),
        connector_capability: capability.expose_for_connector_configuration().to_owned(),
        connector_handshake_timeout_seconds: CONNECTOR_HANDSHAKE_TIMEOUT_SECONDS,
    };
    let request = lillux::sealed_memfd(
        c"ryeos-provider-configuration-request",
        &request.canonical_bytes()?,
    )
    .map_err(anyhow::Error::msg)?;
    let mut process = lillux::SubprocessRequest {
        cmd: String::new(),
        argv0: Some("ryeos-provider-configuration-adapter".into()),
        args: vec!["render".into()],
        cwd: Some("/".into()),
        envs: vec![(
            PROVIDER_CONFIGURATION_REQUEST_FD_ENV.into(),
            request
                .inherited_descriptor()
                .map_err(anyhow::Error::msg)?
                .to_string(),
        )],
        stdin_data: None,
        timeout: 5.0,
        limits: Some(lillux::SubprocessLimits {
            max_open_files: Some(64),
            max_stdout_bytes: Some(MAX_PROVIDER_CONFIGURATION_BYTES as u64),
            max_stderr_bytes: Some(64 * 1024),
            ..lillux::SubprocessLimits::default()
        }),
        // The image is already held as an exact executable authority. Keeping its
        // source descriptor avoids choosing a fixed child coordinate that could
        // collide with another inherited authority as the source closure grows.
        inherited_fds: vec![connector_executable, request],
        inherited_fd_mappings: Vec::new(),
        supervised_status: None,
    };
    adapter
        .executable()
        .bind_as_subprocess_executable_at_source(&mut process)
        .map_err(anyhow::Error::msg)?;
    let mut result = lillux::run(process);
    let encoded = Zeroizing::new(std::mem::take(&mut result.stdout));
    let diagnostics = Zeroizing::new(std::mem::take(&mut result.stderr));
    // The adapter reads an occurrence capability. Its diagnostics are private,
    // including parse errors that may quote request values.
    ensure!(
        result.success,
        "provider configuration adapter failed (exit_code={}, timed_out={}, output_limit_exceeded={}, launcher_refused={}, spawn_refusal={})",
        result.exit_code,
        result.timed_out,
        result.output_limit_exceeded.is_some(),
        result.launcher_refusal.is_some(),
        if result.pid == 0 && diagnostics.starts_with("Failed to spawn:") {
            diagnostics.as_str()
        } else {
            "none"
        },
    );
    ensure!(
        !encoded.is_empty() && encoded.len() <= MAX_PROVIDER_CONFIGURATION_BYTES,
        "external connector configuration exceeds its bound"
    );
    ProviderConfigurationResponse::decode(encoded.as_bytes())
        .context("decode current bounded provider configuration response")
}

fn create_listener() -> Result<(
    lillux::OwnerPrivateLocalDuplexListener,
    lillux::InheritedDescriptorAuthority,
    Arc<TempDirGuard>,
)> {
    let parent = lillux::platform::host_temporary_root()?;
    let random = lillux::crypto::generate_random_bytes::<32>();
    let name = OsString::from(format!("ryeos-xc-{}", &lillux::sha256_hex(&random)[..16]));
    let root = parent.create_child(&name, 0o700)?;
    root.tighten_owner_private_directory()?;
    let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&root, "c")?;
    let listener_name = listener
        .endpoint()
        .file_name()
        .context("external connector listener has no file name")?;
    let listener_mount = root
        .open_inherited_mount_entry(listener_name)?
        .context("external connector listener disappeared before mount capture")?;
    ensure!(
        listener_mount.mount_entry_kind()? == lillux::OpenMountEntryKind::UnixSocket,
        "external connector listener mount source is not a Unix socket"
    );
    let guard = Arc::new(TempDirGuard::new_pinned(parent, name, Arc::new(root)));
    Ok((listener, listener_mount, guard))
}

/// Prepare the one local connector only after the remote occurrence has
/// authenticated, reached Ready, and received its exact Release.
/// Close every connector row an ended daemon generation left open. A relay
/// owner is a thread of the daemon that prepared the row, so after a restart
/// no open row has an owner. The old owner writes the closure itself unless
/// its daemon could no longer write service SQL (a native-capacity latch);
/// then the row stays `connected`, and because an allocation cannot settle
/// while a connector is open, every cleanup pass for that placement is
/// refused and its Q credential custody stays held. Each row is judged by
/// [`dispose_open_connector`]; a retained row is reported. Runs after the
/// dead generation's Worker groups were settled, which is what ends the
/// peers. Returns how many rows were closed.
pub fn close_connectors_left_by_prior_generations(state: &AppState) -> Result<usize> {
    let mut closed = 0usize;
    for record in state.state_store.open_external_connectors()? {
        let placement = record.placement_thread_id.clone();
        let liveness = record
            .peer_process_identity
            .as_ref()
            .map(crate::process::exact_process_liveness);
        let reason = match dispose_open_connector(record.phase, liveness) {
            OpenConnectorDisposition::Close(reason) => reason,
            OpenConnectorDisposition::Retain(why) => {
                tracing::error!(
                    placement = %placement,
                    "external connector left open by a previous daemon stays open: {why}"
                );
                continue;
            }
        };
        state
            .state_store
            .close_external_connector(&placement, reason)
            .with_context(|| format!("close external connector left open for {placement}"))?;
        tracing::warn!(
            placement = %placement,
            reason,
            "closed external connector left open by a previous daemon"
        );
        closed = closed.saturating_add(1);
    }
    Ok(closed)
}

/// Release the configuration mount targets that connectors of ended daemon
/// generations left reserved. A Q connector reserves its target inside the
/// credential home, which every Q start on that credential shares; the live
/// relay owner releases it on retirement, and a daemon that died holding the
/// connector never does, so the next Q start on the credential is refused
/// with the destination occupied. For every Q local start of a previous
/// generation whose connector is no longer open, the target named by its
/// binding is released when it still has a reservation's exact shape (an
/// empty, owner-read-only regular file); anything else at that name is left
/// alone and reported. Returns how many targets were released.
pub fn release_mount_targets_left_by_prior_generations(state: &AppState) -> Result<usize> {
    let mut released = 0usize;
    for start in state
        .state_store
        .prior_generation_qualification_local_starts()?
    {
        let placement = start.placement_thread_id.clone();
        let Some(custody) = start.credential_custody.as_ref() else {
            continue;
        };
        if !state.state_store.external_connector_retired(&placement)? {
            // Its peer is still alive; the connector still owns the target.
            continue;
        }
        let Some(allocation) = state.state_store.external_allocation(&placement)? else {
            continue;
        };
        let Some(binding) = state
            .state_store
            .retained_external_binding(&allocation.reservation.binding_hash)?
        else {
            continue;
        };
        let contract = binding.backend_contract();
        let destination = match contract.workload.session_artifacts() {
            Ok(artifacts) => artifacts.provider_configuration_destination.to_owned(),
            Err(_) => continue,
        };
        let home_path = crate::private_artifact_home::home_path(
            &state.config.runtime_state_dir(),
            &custody.home_id,
        )?;
        let Some(home) = lillux::PinnedDirectory::open(&home_path)? else {
            continue;
        };
        home.require_owner_private_directory()?;
        match release_stale_mount_target(&home, OsStr::new(&destination))? {
            MountTargetRelease::Released => {
                tracing::warn!(
                    placement = %placement,
                    destination = %destination,
                    "released a connector mount target a previous daemon left reserved"
                );
                released = released.saturating_add(1);
            }
            MountTargetRelease::Absent => {}
            MountTargetRelease::Retained(why) => tracing::error!(
                placement = %placement,
                destination = %destination,
                "a connector mount target of a previous daemon stays: {why}"
            ),
        }
    }
    Ok(released)
}

/// What became of one reserved mount-target name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountTargetRelease {
    /// The reservation placeholder was there and is removed.
    Released,
    /// Nothing is at that name.
    Absent,
    /// Something other than a reservation placeholder is at that name; it
    /// is not touched, and the text says why.
    Retained(&'static str),
}

/// Remove the reservation placeholder at `destination` in `home` if, and
/// only if, it has exactly the shape a reservation creates: a regular file,
/// empty, mode 0400. A configuration file, a link or a directory at that
/// name is never removed.
pub(crate) fn release_stale_mount_target(
    home: &lillux::PinnedDirectory,
    destination: &OsStr,
) -> Result<MountTargetRelease> {
    let Some(entry) = home.entry_no_follow(destination)? else {
        return Ok(MountTargetRelease::Absent);
    };
    if entry.entry_type != lillux::PinnedEntryType::Regular {
        return Ok(MountTargetRelease::Retained("it is not a regular file"));
    }
    let Some(file) = home.open_pinned_regular(destination, false)? else {
        return Ok(MountTargetRelease::Absent);
    };
    if file.observation()?.size() != 0 {
        return Ok(MountTargetRelease::Retained("it holds content"));
    }
    if entry.mode & 0o777 != 0o400 {
        return Ok(MountTargetRelease::Retained(
            "its mode is not a reservation's owner-read-only",
        ));
    }
    home.remove_pinned_regular_if_same(&file)?;
    Ok(MountTargetRelease::Released)
}

/// What start recovery does with one connector row it found open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenConnectorDisposition {
    /// Close it with this reason.
    Close(&'static str),
    /// Leave it open; the text says why.
    Retain(&'static str),
}

/// Judge one open connector row from its phase and, for a connected row,
/// the liveness of its exact peer. A `prepared` row never had a peer and is
/// abandoned. A `connected` row is closed as `peer_absent` once its peer is
/// proved dead or stale; a peer still alive, one this host cannot prove, or
/// a connected row with no peer identity keeps the row, exactly as an
/// unproved Worker group keeps its row. A closed row is not open.
pub(crate) fn dispose_open_connector(
    phase: ExternalConnectorPhase,
    peer_liveness: Option<IdentityLiveness>,
) -> OpenConnectorDisposition {
    match (phase, peer_liveness) {
        (ExternalConnectorPhase::Prepared, _) => {
            OpenConnectorDisposition::Close("startup_abandoned")
        }
        (ExternalConnectorPhase::Connected, Some(IdentityLiveness::DeadOrStale)) => {
            OpenConnectorDisposition::Close("peer_absent")
        }
        (ExternalConnectorPhase::Connected, Some(IdentityLiveness::Alive)) => {
            OpenConnectorDisposition::Retain("its peer process is still alive")
        }
        (ExternalConnectorPhase::Connected, Some(IdentityLiveness::Unavailable)) => {
            OpenConnectorDisposition::Retain("its peer liveness is unproved on this host")
        }
        (ExternalConnectorPhase::Connected, None) => {
            OpenConnectorDisposition::Retain("its peer identity is absent")
        }
        (ExternalConnectorPhase::Closed, _) => {
            OpenConnectorDisposition::Retain("it is already closed")
        }
    }
}

pub fn prepare_external_candidate_connector(
    state: &AppState,
    placement: &str,
    state_root: &Path,
) -> Result<PreparedExternalCandidateConnector> {
    prepare_connector(state, placement, state_root, None)
}

/// Q delivery borrows the original credential-home descriptor used by the
/// held Worker, rather than reopening a separately supplied directory path.
pub fn prepare_qualification_candidate_connector(
    state: &AppState,
    placement: &str,
    credential_home: &lillux::PinnedDirectory,
) -> Result<PreparedExternalCandidateConnector> {
    prepare_connector(
        state,
        placement,
        credential_home.path(),
        Some(credential_home),
    )
}

fn prepare_connector(
    state: &AppState,
    placement: &str,
    state_root: &Path,
    qualification_home: Option<&lillux::PinnedDirectory>,
) -> Result<PreparedExternalCandidateConnector> {
    ensure!(
        state_root.is_absolute(),
        "external connector provider state root must be absolute"
    );
    let allocation = state
        .state_store
        .external_allocation(placement)?
        .context("external connector has no retained allocation")?;
    let qualification_home_id = if matches!(
        &allocation.reservation.owner,
        crate::runtime_db::external_execution::ExternalAllocationOwner::QualificationCandidate(_)
    ) {
        let original_home = qualification_home
            .context("Q connector requires the original held credential-home descriptor")?;
        original_home.require_owner_private_directory()?;
        original_home.ensure_path_binding()?;
        let start = state
            .state_store
            .require_live_qualification_local_credential_custody(placement)?;
        let custody = start
            .credential_custody
            .as_ref()
            .context("Q connector lost credential custody")?;
        let expected_home = crate::private_artifact_home::home_path(
            &state.config.runtime_state_dir(),
            &custody.home_id,
        )?;
        ensure!(
            state_root == expected_home,
            "Q connector configuration changed its admitted credential home"
        );
        Some(custody.home_id.clone())
    } else {
        ensure!(
            qualification_home.is_none(),
            "ordinary connector cannot borrow Q credential custody"
        );
        None
    };
    let retained = state
        .state_store
        .retained_external_binding(&allocation.reservation.binding_hash)?
        .context("external connector lost its retained placement binding")?;
    let contract = retained.backend_contract();
    let artifact = state.external_candidate_connectors.qualify(&contract)?;
    let configuration_adapter = state.external_provider_configurations.qualify(&contract)?;
    let channel = state.state_store.external_execution_channel(placement)?;
    ensure!(
        channel.execution_binding_hash == allocation.reservation.binding_hash,
        "external connector channel changed its execution binding"
    );

    let capability_generation = state
        .state_store
        .external_connector_capability_generation(placement)?;
    let access = ExternalConnectorCapabilityAccess::new(&capability_generation)?;
    let capability = access.decode(
        state
            .vault
            .ensure_external_connector_capability(&access)
            .context("provision protected external connector capability")?,
    )?;
    let capability_hash = capability.capability_hash()?;

    let preparation = state.state_store.prepare_external_connector(
        placement,
        &capability_generation,
        &capability_hash,
    )?;
    let ExternalConnectorPreparation::Fresh(connector) = preparation else {
        anyhow::bail!(
            "external connector preparation is already durable; replacement startup is not authorized"
        );
    };
    ensure!(
        connector.phase
            == crate::runtime_db::external_execution::connector::ExternalConnectorPhase::Prepared,
        "fresh external connector occurrence is not prepared"
    );

    let mut mount_targets = Vec::new();
    let prepared = (|| -> Result<PreparedExternalCandidateConnector> {
        let (listener, listener_mount, runtime_root) = create_listener()?;
        let (connector_endpoint, connector_mount) = if state.isolation.is_enforced() {
            let destination = ryeos_state::objects::session_runtime_endpoint_destination(
                CONNECTOR_RUNTIME_ENDPOINT_NAME,
            )?;
            let mount =
                ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new_runtime_endpoint(
                    listener.endpoint().to_path_buf(),
                    CONNECTOR_RUNTIME_ENDPOINT_NAME,
                    listener_mount,
                )?;
            (destination, Some(mount))
        } else {
            (listener.endpoint().to_path_buf(), None)
        };
        let connector_executable = artifact.captured_executable();
        let mut mounts: Vec<_> = connector_mount.into_iter().collect();
        let executable_delivery = if state.isolation.is_enforced() {
            let destination = Path::new(ryeos_state::objects::EXECUTION_RUNTIME_REALIZATIONS_ROOT)
                .join("external-connector")
                .join("connector");
            mounts.push(
                ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new_execution_runtime(
                    connector_executable.path().to_path_buf(),
                    destination.clone(),
                    connector_executable.clone(),
                ),
            );
            ProviderConnectorExecutable::NamespacePath {
                path: destination
                    .to_str()
                    .context("connector mount path is not UTF-8")?
                    .to_owned(),
            }
        } else {
            ProviderConnectorExecutable::InheritedDescriptor {
                descriptor: connector_executable
                    .inherited_descriptor()
                    .map_err(anyhow::Error::msg)?,
            }
        };
        let configuration = connector_configuration(
            &configuration_adapter,
            &artifact,
            executable_delivery,
            &connector_endpoint,
            placement,
            &channel.execution_binding_hash,
            &capability,
        )?;
        let configuration_handle = lillux::sealed_memfd(
            c"ryeos-external-connector-environments",
            configuration.configuration().as_bytes(),
        )
        .map_err(anyhow::Error::msg)?;
        let selected = contract.workload.session_artifacts()?;
        let configuration_input = ProviderConfigurationLaunchInput {
            schema: 1,
            configuration_sha256: lillux::sha256_hex(configuration.configuration().as_bytes()),
            configuration_bytes: u64::try_from(configuration.configuration().len())?,
            command_environment: configuration.command_environment().clone(),
            provider_declaration_id: selected.provider_declaration_id.to_owned(),
            configuration_destination: selected.provider_configuration_destination.to_owned(),
            configuration_adapter_artifact_hash: selected
                .configuration_adapter_artifact_hash
                .to_owned(),
            configuration_adapter_artifact_bytes: selected.configuration_adapter_artifact_bytes,
        };
        configuration_input.validate()?;
        let configuration_observation = PreparedProviderConfigurationObservation {
            placement_thread_id: placement.to_owned(),
            execution_binding_hash: channel.execution_binding_hash.clone(),
            qualification_home_id,
            input: configuration_input,
        };
        let ordinary_directory = if qualification_home.is_none() {
            Some(
                lillux::PinnedDirectory::open(state_root)?
                    .context("external connector provider state root is missing")?,
            )
        } else {
            None
        };
        let state_directory = qualification_home
            .or(ordinary_directory.as_ref())
            .context("external connector provider state directory is missing")?;
        state_directory.require_owner_private_directory()?;
        state_directory.ensure_path_binding()?;
        let destination = &configuration_adapter
            .declaration()
            .configuration_destination;
        let (configuration_link, descriptors) = if state.isolation.is_enforced() {
            // Explicit controller-owned state preparation, not a sandbox write
            // into an admitted source. Persistent targets contain no secret bytes.
            mount_targets.push(
                state_directory.reserve_empty_mount_target(std::ffi::OsStr::new(destination))?,
            );
            mounts.push(
                ryeos_engine::isolation::IsolationReadOnlyMountAuthority::new_state_overlay(
                    configuration_handle.path().to_path_buf(),
                    state_root.join(destination),
                    configuration_handle,
                ),
            );
            (None, Vec::new())
        } else {
            let link = state_directory.install_ephemeral_descriptor_file_link(
                std::ffi::OsStr::new(destination),
                &configuration_handle,
            )?;
            (Some(link), vec![configuration_handle, connector_executable])
        };

        let control = Arc::new(ExternalConnectorRelayControl {
            stop: AtomicBool::new(false),
            interrupt: Mutex::new(None),
        });
        let thread_control = Arc::clone(&control);
        let thread_state = state.clone();
        let thread_placement = placement.to_owned();
        let thread_binding = channel.execution_binding_hash;
        let thread_generation = capability_generation;
        let thread_capability_hash = capability_hash;
        let task_name = format!("external-connector-{}", short_coordinate(placement));
        let thread = lillux::task::spawn_host_task(&task_name, move || {
            run_connector_owner(
                thread_state,
                thread_placement,
                thread_binding,
                thread_generation,
                thread_capability_hash,
                capability,
                artifact,
                listener,
                runtime_root,
                thread_control,
            )
        });
        let thread = thread.context("start external connector relay owner")?;
        Ok(PreparedExternalCandidateConnector {
            configuration_observation,
            configuration_link,
            descriptors,
            mounts,
            mount_targets: std::mem::take(&mut mount_targets),
            relay: ExternalConnectorRelayLifeline {
                control,
                settlement: Mutex::new(RelaySettlement {
                    thread: Some(thread),
                    failure: None,
                }),
            },
        })
    })();
    if let Err(error) = prepared {
        let cleanup = close_mount_targets(&mut mount_targets);
        let closure = state
            .state_store
            .close_external_connector(placement, "startup_abandoned");
        let mut error = error;
        let incomplete = cleanup.is_err() || closure.is_err();
        if let Err(cleanup) = cleanup {
            error = error.context(format!("mount preparation cleanup unproved: {cleanup:#}"));
        }
        if let Err(closure) = closure {
            error = error.context(format!(
                "connector preparation closure unproved: {closure:#}"
            ));
        }
        return Err(if incomplete {
            error.context(crate::persistent_session::PersistentSessionCleanupUnproved)
        } else {
            error
        });
    }
    prepared
}

fn short_coordinate(value: &str) -> String {
    lillux::sha256_hex(value.as_bytes())[..16].to_owned()
}

fn publish_interrupt(
    control: &ExternalConnectorRelayControl,
    stream: &lillux::LocalDuplexStream,
) -> Result<bool> {
    let mut interrupt = control
        .interrupt
        .lock()
        .map_err(|_| anyhow::anyhow!("external connector interruption owner poisoned"))?;
    *interrupt = Some(stream.try_clone()?);
    // Pair publication with the stopper: it either finds this stream under the
    // same lock, or we observe the stop it published before looking for one.
    if control.stop.load(Ordering::Acquire) {
        stream.shutdown()?;
        return Ok(false);
    }
    Ok(true)
}

fn run_connector_owner(
    state: AppState,
    placement: String,
    execution_binding_hash: String,
    capability_generation: String,
    capability_hash: String,
    capability: ExternalConnectorCapability,
    artifact: Arc<InstalledExternalCandidateConnector>,
    listener: lillux::OwnerPrivateLocalDuplexListener,
    _runtime_root: Arc<TempDirGuard>,
    control: Arc<ExternalConnectorRelayControl>,
) -> bool {
    let (reason, error) = match accept_connector(&listener, &control) {
        Ok(Some(mut stream)) => match publish_interrupt(&control, &stream) {
            Ok(true) => run_authenticated_connector(
                &state,
                &placement,
                &execution_binding_hash,
                &capability_generation,
                &capability_hash,
                &capability,
                &artifact,
                &mut stream,
                &control,
            ),
            Ok(false) => ("startup_abandoned", None),
            Err(error) => ("startup_abandoned", Some(error)),
        },
        Ok(None) => ("startup_abandoned", None),
        Err(error) => ("startup_abandoned", Some(error)),
    };
    if let Ok(mut interrupt) = control.interrupt.lock() {
        *interrupt = None;
    }
    let closed = state
        .state_store
        .close_external_connector(&placement, reason)
        .is_ok();
    if !closed {
        tracing::error!(
            placement = %placement,
            reason,
            "external connector durable closure failed"
        );
    }
    // Protocol and decoder errors can contain peer-controlled/private bytes.
    // Publish only the closed category and the authoritative occurrence; a
    // disconnect also names its I/O kind, a closed vocabulary of the kernel's.
    if let Some(error) = &error {
        tracing::warn!(
            placement = %placement,
            reason,
            disconnect_kind = ?disconnect_kind(error),
            "external connector relay terminated"
        );
    }
    closed
}

/// A connector peer whose identity was captured and proved against the
/// retained containment of its qualification Worker (namespace or cgroup
/// membership, between pidfd probes). The field is private and the only
/// constructor performs that capture, so the containment branch of the
/// historical peer check is reachable only by peers admitted this way.
pub(crate) struct ContainedQualificationPeer {
    identity: lillux::ExactProcessIdentity,
    /// The exact containment record this membership was read against. The
    /// durable admission compares it with the Worker row instead of
    /// repeating the slow read under the state-store lock.
    proven_against: lillux::ProcessScopeRecovery,
}

impl ContainedQualificationPeer {
    fn capture(
        peer: &lillux::local_ipc::AuthenticatedUnixPeer,
        containment: &lillux::ProcessScopeRecovery,
    ) -> Result<Self> {
        Ok(Self {
            identity: peer.contained_exact_process_identity(containment)?,
            proven_against: containment.clone(),
        })
    }

    pub(crate) fn proven_against(&self) -> &lillux::ProcessScopeRecovery {
        &self.proven_against
    }

    pub(crate) fn identity(&self) -> &lillux::ExactProcessIdentity {
        &self.identity
    }
}

enum AdmittedPeer {
    Group(lillux::ExactProcessIdentity),
    Contained(ContainedQualificationPeer),
}

fn accept_connector(
    listener: &lillux::OwnerPrivateLocalDuplexListener,
    control: &ExternalConnectorRelayControl,
) -> Result<Option<lillux::LocalDuplexStream>> {
    while !control.stop.load(Ordering::Acquire) {
        if let Some(stream) =
            listener.accept_before(lillux::time::MonotonicDeadline::after(ACCEPT_POLL))?
        {
            return Ok(Some(stream));
        }
    }
    Ok(None)
}

fn run_authenticated_connector(
    state: &AppState,
    placement: &str,
    execution_binding_hash: &str,
    capability_generation: &str,
    capability_hash: &str,
    capability: &ExternalConnectorCapability,
    artifact: &InstalledExternalCandidateConnector,
    stream: &mut lillux::LocalDuplexStream,
    control: &ExternalConnectorRelayControl,
) -> (&'static str, Option<anyhow::Error>) {
    // Closed, controller-selected diagnostic vocabulary. Never derive this
    // label from peer bytes or stringify the authentication error.
    let mut stage = "peer_identity";
    let authenticated = (|| -> Result<()> {
        let peer = stream.authenticated_peer()?;
        stage = "executable_image";
        artifact.verify_peer(&peer)?;
        stage = "hello_frame";
        let hello = read_external_connector_hello(
            &mut stream.with_deadline(lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT)),
        )?;
        stage = "occurrence_capability";
        authenticate_hello(
            &hello,
            placement,
            execution_binding_hash,
            capability_hash,
            capability,
        )?;
        stage = "exact_process_identity";
        let allocation = state
            .state_store
            .external_allocation(placement)?
            .context("external connector lost its retained allocation")?;
        let admitted_peer = if matches!(
            allocation.reservation.owner,
            crate::runtime_db::external_execution::ExternalAllocationOwner::QualificationCandidate(
                _
            )
        ) {
            let worker = state
                .state_store
                .qualification_worker_process(placement)?
                .context("qualification connector has no attached local Worker")?;
            ensure!(
                worker.phase
                    == crate::runtime_db::external_execution::QualificationWorkerProcessPhase::Attached
                    && worker.channel_binding_digest
                        == state.state_store.external_execution_channel(placement)?.digest()?,
                "qualification connector changed its attached Worker or channel"
            );
            match worker.process_identity.process_scope.as_ref() {
                // Trusted lane: the connector is held only by the Worker's
                // process group. Lillux observes both the socket peer and
                // group leader through pinned process descriptors; the
                // connection transaction rejoins their boot/group birth
                // tuple to this retained Worker.
                None => {
                    let group = u32::try_from(worker.process_identity.group_leader_pid)
                        .context("qualification connector Worker group is outside range")?;
                    AdmittedPeer::Group(peer.exact_process_identity(Some(group))?)
                }
                // Retained containment: the connector may run in its own
                // process group or session. Key on the containment itself.
                // Lillux re-probes the peer's pidfd on both sides of the
                // membership read, so the numeric PID read is attributable
                // to this incarnation.
                Some(scope) => AdmittedPeer::Contained(
                    ContainedQualificationPeer::capture(&peer, scope).context(
                        "qualification connector peer is not inside its Worker's retained containment",
                    )?,
                ),
            }
        } else {
            AdmittedPeer::Group(peer.exact_process_identity(None)?)
        };
        stage = "durable_connection";
        let first_connection = match &admitted_peer {
            AdmittedPeer::Group(identity) => state.state_store.connect_external_connector(
                placement,
                capability_generation,
                capability_hash,
                identity,
            )?,
            AdmittedPeer::Contained(contained) => {
                state.state_store.connect_external_connector_contained(
                    placement,
                    capability_generation,
                    capability_hash,
                    contained,
                )?
            }
        };
        ensure!(
            first_connection,
            "external connector connection is already uncertain"
        );
        stage = "ready_frame";
        write_server_frame(
            stream,
            &ExternalConnectorServerFrame::Ready {
                placement_thread_id: placement.to_owned(),
                execution_binding_hash: execution_binding_hash.to_owned(),
            },
        )?;
        Ok(())
    })();
    if let Err(error) = authenticated {
        tracing::warn!(
            placement = %placement,
            stage,
            "external connector authentication refused"
        );
        let _ = write_server_frame(
            stream,
            &ExternalConnectorServerFrame::Fault {
                code: ExternalConnectorFault::AuthenticationRefused,
            },
        );
        return ("authentication_refused", Some(error));
    }

    match relay_authenticated_protocol(state, placement, stream, control) {
        Ok(()) => ("completed", None),
        Err(error) if control.stop.load(Ordering::Acquire) => ("controller_shutdown", Some(error)),
        Err(error) if is_disconnect(&error) => ("disconnected", Some(error)),
        Err(error) => ("protocol_fault", Some(error)),
    }
}

fn authenticate_hello(
    hello: &ExternalConnectorHello,
    placement: &str,
    execution_binding_hash: &str,
    capability_hash: &str,
    capability: &ExternalConnectorCapability,
) -> Result<()> {
    ensure!(
        hello.placement_thread_id == placement
            && hello.execution_binding_hash == execution_binding_hash,
        "external connector hello changed its admitted execution"
    );
    let presented = hello.capability_hash()?;
    capability.authenticate_hash(&presented)?;
    ensure!(
        presented == capability_hash,
        "external connector capability changed after preparation"
    );
    Ok(())
}

enum InputRelayOutcome {
    Closed,
    Failed(anyhow::Error),
}

fn relay_authenticated_protocol(
    state: &AppState,
    placement: &str,
    stream: &mut lillux::LocalDuplexStream,
    control: &ExternalConnectorRelayControl,
) -> Result<()> {
    let reader = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let input_state = state.clone();
    let input_placement = placement.to_owned();
    let input_writer = Arc::clone(&writer);
    let (input_tx, input_rx) = mpsc::sync_channel(1);
    let task_name = format!("external-connector-input-{}", short_coordinate(placement));
    let input = lillux::task::spawn_host_task(&task_name, move || {
        let outcome =
            match relay_connector_input(&input_state, &input_placement, reader, &input_writer) {
                Ok(()) => InputRelayOutcome::Closed,
                Err(error) => InputRelayOutcome::Failed(error),
            };
        let _ = input_tx.send(outcome);
    })?;

    run_output_and_settle_input(stream, input, || {
        let mut input_closed = false;
        loop {
            match input_rx.try_recv() {
                Ok(InputRelayOutcome::Closed) => input_closed = true,
                Ok(InputRelayOutcome::Failed(error)) => break Err(error),
                Err(mpsc::TryRecvError::Disconnected) if !input_closed => {
                    break Err(anyhow::anyhow!(
                        "external connector input owner disappeared"
                    ));
                }
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {}
            }
            if control.stop.load(Ordering::Acquire) {
                break Err(anyhow::anyhow!("external connector relay was stopped"));
            }
            match claim_external_protocol_output(state, placement)? {
                ExternalProtocolOutput::Idle => lillux::time::sleep(RELAY_POLL),
                ExternalProtocolOutput::Uncertain { .. } => {
                    let _ = write_shared_frame(
                        &writer,
                        &ExternalConnectorServerFrame::Fault {
                            code: ExternalConnectorFault::OutputUncertain,
                        },
                    );
                    break Err(anyhow::anyhow!(
                        "external connector output delivery is uncertain"
                    ));
                }
                ExternalProtocolOutput::Claimed(permit) => {
                    let frame = if permit.is_eof() {
                        ExternalConnectorServerFrame::ProtocolEof {
                            remote_sequence: permit.sequence(),
                            remote_frame_digest: permit.frame_digest().to_owned(),
                        }
                    } else {
                        ExternalConnectorServerFrame::ProtocolBytes {
                            remote_sequence: permit.sequence(),
                            remote_frame_digest: permit.frame_digest().to_owned(),
                            bytes_base64: STANDARD.encode(permit.bytes()),
                        }
                    };
                    let eof = permit.is_eof();
                    write_shared_frame(&writer, &frame)?;
                    permit.finish()?;
                    if eof {
                        break Ok(());
                    }
                }
            }
        }
    })
}

/// Every output result, including an early `?`, must interrupt and join the
/// input owner before durable connector closure may be recorded. Joining this
/// local relay does not establish remote candidate or occurrence termination.
fn run_output_and_settle_input(
    stream: &lillux::LocalDuplexStream,
    input: lillux::task::HostTask<()>,
    output: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let result = output();
    let shutdown = stream
        .shutdown()
        .context("interrupt external connector input");
    let joined = input
        .join()
        .map_err(|_| anyhow::anyhow!("external connector input owner panicked"));
    let cleanup = shutdown.and(joined);
    match (result, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!(
            "external connector input settlement also failed: {cleanup:#}"
        ))),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), cleanup) => cleanup,
    }
}

fn relay_connector_input(
    state: &AppState,
    placement: &str,
    mut reader: lillux::LocalDuplexStream,
    writer: &Mutex<lillux::LocalDuplexStream>,
) -> Result<()> {
    let mut expected_sequence = 1_u64;
    loop {
        let frame = read_external_connector_client_frame(&mut reader)?;
        ensure!(
            frame.local_sequence() == expected_sequence,
            "external connector input sequence is not contiguous"
        );
        expected_sequence = expected_sequence
            .checked_add(1)
            .context("external connector input sequence overflow")?;
        let Some(bytes) = frame.protocol_bytes()? else {
            // The peer's stdin ended. Carry that to the guest so the
            // candidate's stdin closes too; the output relay keeps running
            // until the guest's own EOF, which is the connector's normal end.
            if let Err(error) =
                crate::external_placement::request_external_protocol_input_close(state, placement)
            {
                let _ = write_shared_frame(
                    writer,
                    &ExternalConnectorServerFrame::Fault {
                        code: ExternalConnectorFault::InputUncertain,
                    },
                );
                return Err(error).context("retain external connector input closure");
            }
            return Ok(());
        };
        let remote = match author_external_protocol_input(state, placement, &bytes) {
            Ok(remote) => remote,
            Err(error) => {
                let _ = write_shared_frame(
                    writer,
                    &ExternalConnectorServerFrame::Fault {
                        code: ExternalConnectorFault::InputUncertain,
                    },
                );
                return Err(error).context("retain external connector input");
            }
        };
        write_shared_frame(
            writer,
            &ExternalConnectorServerFrame::InputApplied {
                local_sequence: frame.local_sequence(),
                remote_sequence: remote.sequence(),
                remote_frame_digest: remote.frame_digest().to_owned(),
            },
        )?;
    }
}

fn write_shared_frame(
    writer: &Mutex<lillux::LocalDuplexStream>,
    frame: &ExternalConnectorServerFrame,
) -> Result<()> {
    let mut writer = writer
        .lock()
        .map_err(|_| anyhow::anyhow!("external connector writer is poisoned"))?;
    write_server_frame(&mut writer, frame)
}

fn write_server_frame(
    stream: &mut lillux::LocalDuplexStream,
    frame: &ExternalConnectorServerFrame,
) -> Result<()> {
    write_external_connector_server_frame(
        &mut stream.with_deadline(lillux::time::MonotonicDeadline::after(LOCAL_IO_TIMEOUT)),
        frame,
    )
}

/// The I/O kind of the first disconnect-class cause, when there is one.
fn disconnect_kind(error: &anyhow::Error) -> Option<std::io::ErrorKind> {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map(std::io::Error::kind)
        .find(|kind| {
            matches!(
                kind,
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
}

fn is_disconnect(error: &anyhow::Error) -> bool {
    disconnect_kind(error).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_observation_refuses_other_occurrence_binding_or_credential_home() {
        use ryeos_external_execution_contract::{
            ProviderCommandEnvironmentProjection, ProviderCommandEnvironmentTransport,
        };
        let observation = PreparedProviderConfigurationObservation {
            placement_thread_id: format!("Q-{}", "a".repeat(64)),
            execution_binding_hash: "b".repeat(64),
            qualification_home_id: Some("original-home".into()),
            input: ProviderConfigurationLaunchInput {
                schema: 1,
                configuration_sha256: "c".repeat(64),
                configuration_bytes: 128,
                command_environment: ProviderCommandEnvironmentProjection {
                    environment_id: "external".into(),
                    default_environment_id: "external".into(),
                    include_local: false,
                    transport: ProviderCommandEnvironmentTransport::CommandConnector,
                },
                provider_declaration_id: "provider".into(),
                configuration_destination: "environments.toml".into(),
                configuration_adapter_artifact_hash: "d".repeat(64),
                configuration_adapter_artifact_bytes: 4096,
            },
        };
        let placement = observation.placement_thread_id.clone();
        let binding = observation.execution_binding_hash.clone();
        observation
            .require_for(&placement, &binding, "original-home")
            .unwrap();
        assert!(
            observation
                .require_for("other-placement", &binding, "original-home")
                .is_err()
        );
        assert!(
            observation
                .require_for(&placement, &"e".repeat(64), "original-home")
                .is_err()
        );
        assert!(
            observation
                .require_for(&placement, &binding, "replacement-home")
                .is_err()
        );
        let ordinary = PreparedProviderConfigurationObservation {
            qualification_home_id: None,
            ..observation
        };
        assert!(
            ordinary
                .require_for(&placement, &binding, "original-home")
                .is_err()
        );
    }

    fn relay_fixture(
        task: impl FnOnce() -> bool + Send + 'static,
    ) -> ExternalConnectorRelayLifeline {
        ExternalConnectorRelayLifeline {
            control: Arc::new(ExternalConnectorRelayControl {
                stop: AtomicBool::new(false),
                interrupt: Mutex::new(None),
            }),
            settlement: Mutex::new(RelaySettlement {
                thread: Some(lillux::task::spawn_host_task("relay-retirement-test", task).unwrap()),
                failure: None,
            }),
        }
    }

    #[test]
    fn relay_timeout_retains_exact_owner_after_observer_reference_drops() {
        let (release, wait) = mpsc::channel();
        let retained = Arc::new(relay_fixture(move || {
            wait.recv().unwrap();
            true
        }));
        let observer = Arc::clone(&retained);
        let error = observer
            .retire(lillux::time::MonotonicDeadline::after(Duration::ZERO))
            .unwrap_err();
        assert!(error.to_string().contains("ownership retained"));
        drop(observer);
        assert!(retained.settlement.lock().unwrap().thread.is_some());
        release.send(()).unwrap();
        retained
            .retire(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                2,
            )))
            .unwrap();
        assert!(retained.settlement.lock().unwrap().thread.is_none());
    }

    #[test]
    fn failed_durable_relay_closure_cannot_become_success_on_second_retirement() {
        let relay = relay_fixture(|| false);
        for _ in 0..2 {
            let error = relay
                .retire(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                    2,
                )))
                .unwrap_err();
            assert!(error.to_string().contains("durable closure failed"));
        }
    }

    #[test]
    fn relay_panic_cannot_become_success_on_second_retirement() {
        let relay = relay_fixture(|| panic!("fixture relay panic"));
        for _ in 0..2 {
            let error = relay
                .retire(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                    2,
                )))
                .unwrap_err();
            assert!(error.to_string().contains("panicked during retirement"));
        }
    }

    #[test]
    fn cancellation_before_stream_publication_closes_the_late_stream() {
        use std::io::Read as _;
        let (_root, mut peer, server) = connected_streams();
        let relay = relay_fixture(|| true);
        relay
            .interrupt(lillux::time::MonotonicDeadline::after(Duration::ZERO))
            .unwrap();
        assert!(!publish_interrupt(&relay.control, &server).unwrap());
        let mut byte = [0];
        assert_eq!(
            peer.with_deadline(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                2
            )))
            .read(&mut byte)
            .unwrap(),
            0
        );
        relay
            .retire(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                2,
            )))
            .unwrap();
    }

    #[test]
    fn interruption_waits_for_publication_lock_within_one_deadline() {
        let relay = Arc::new(relay_fixture(|| true));
        let publication = relay.control.interrupt.lock().unwrap();
        let retiring = Arc::clone(&relay);
        let task = lillux::task::spawn_host_task("relay-publication-race", move || {
            retiring.retire(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                2,
            )))
        })
        .unwrap();
        let bound = lillux::time::MonotonicDeadline::after(Duration::from_secs(1));
        while !relay.control.stop.load(Ordering::Acquire) && !bound.has_elapsed() {
            lillux::time::sleep(Duration::from_millis(1));
        }
        assert!(relay.control.stop.load(Ordering::Acquire));
        drop(publication);
        task.join().unwrap().unwrap();
    }

    #[test]
    fn mount_reservation_remains_until_relay_durably_closes() {
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let name = std::ffi::OsStr::new("reserved");
        let reservation = directory.reserve_empty_mount_target(name).unwrap();
        let (release, wait) = mpsc::channel();
        let owner = Arc::new(ExternalConnectorRetirement {
            relay: relay_fixture(move || {
                wait.recv().unwrap();
                true
            }),
            targets: Mutex::new(vec![reservation]),
        });
        let observer = Arc::clone(&owner);
        assert!(
            observer
                .settle(lillux::time::MonotonicDeadline::after(Duration::ZERO))
                .is_err()
        );
        drop(observer);
        assert!(directory.reserve_empty_mount_target(name).is_err());
        assert_eq!(owner.targets.lock().unwrap().len(), 1);
        release.send(()).unwrap();
        owner
            .settle(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                2,
            )))
            .unwrap();
        assert!(owner.targets.lock().unwrap().is_empty());
        directory
            .reserve_empty_mount_target(name)
            .unwrap()
            .close()
            .unwrap();
    }

    #[test]
    fn durable_close_failure_preserves_mount_reservation() {
        let root = tempfile::tempdir().unwrap();
        let directory = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        directory.tighten_owner_private_directory().unwrap();
        let name = std::ffi::OsStr::new("reserved");
        let owner = ExternalConnectorRetirement {
            relay: relay_fixture(|| false),
            targets: Mutex::new(vec![directory.reserve_empty_mount_target(name).unwrap()]),
        };
        for _ in 0..2 {
            assert!(
                owner
                    .settle(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                        2
                    )))
                    .is_err()
            );
            assert_eq!(owner.targets.lock().unwrap().len(), 1);
            assert!(directory.reserve_empty_mount_target(name).is_err());
        }
    }

    fn connected_streams() -> (
        tempfile::TempDir,
        lillux::LocalDuplexStream,
        lillux::LocalDuplexStream,
    ) {
        let root = tempfile::tempdir().unwrap();
        let authority = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        authority.tighten_owner_private_directory().unwrap();
        let listener = lillux::OwnerPrivateLocalDuplexListener::bind(&authority, "relay").unwrap();
        let client = lillux::LocalDuplexStream::connect(listener.endpoint()).unwrap();
        let server = listener
            .accept_before(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                2,
            )))
            .unwrap()
            .unwrap();
        (root, client, server)
    }

    #[test]
    fn output_error_interrupts_and_joins_the_blocked_input_owner() {
        use std::io::Read as _;
        let (_root, _peer, server) = connected_streams();
        let mut reader = server.try_clone().unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = Arc::clone(&finished);
        let input = lillux::task::spawn_host_task("connector-blocked-input-test", move || {
            let mut byte = [0];
            let count = reader
                .with_deadline(lillux::time::MonotonicDeadline::after(Duration::from_secs(
                    2,
                )))
                .read(&mut byte)
                .unwrap();
            assert_eq!(count, 0, "shutdown must interrupt the blocked reader");
            task_finished.store(true, Ordering::Release);
        })
        .unwrap();
        let error = run_output_and_settle_input(&server, input, || {
            Err::<(), _>(anyhow::anyhow!("output publication failed"))?;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "output publication failed");
        assert!(finished.load(Ordering::Acquire));
    }

    #[test]
    fn input_panic_cannot_be_reported_as_clean_relay_completion() {
        let (_root, _peer, server) = connected_streams();
        let input = lillux::task::spawn_host_task("connector-panic-test", || {
            panic!("input owner failed");
        })
        .unwrap();
        let error = run_output_and_settle_input(&server, input, || Ok(())).unwrap_err();
        assert!(error.to_string().contains("input owner panicked"));
    }

    #[test]
    fn a_stale_mount_target_is_released_only_in_its_reservation_shape() {
        let root = tempfile::tempdir().unwrap();
        let home = lillux::PinnedDirectory::open(root.path()).unwrap().unwrap();
        home.tighten_owner_private_directory().unwrap();
        let destination = OsStr::new("environments.toml");
        assert_eq!(
            release_stale_mount_target(&home, destination).unwrap(),
            MountTargetRelease::Absent
        );
        // A reservation whose owner died without closing it: drop releases the
        // handles and leaves the namespace reservation in place.
        let reservation = home.reserve_empty_mount_target(destination).unwrap();
        drop(reservation);
        assert!(root.path().join("environments.toml").exists());
        assert_eq!(
            release_stale_mount_target(&home, destination).unwrap(),
            MountTargetRelease::Released
        );
        assert!(!root.path().join("environments.toml").exists());
        assert_eq!(
            release_stale_mount_target(&home, destination).unwrap(),
            MountTargetRelease::Absent
        );
        // A real configuration file at that name is never touched.
        std::fs::write(root.path().join("environments.toml"), b"default = \"x\"\n").unwrap();
        assert!(matches!(
            release_stale_mount_target(&home, destination).unwrap(),
            MountTargetRelease::Retained(_)
        ));
        assert!(root.path().join("environments.toml").exists());
        // An empty file with a writable mode is not a reservation either.
        std::fs::remove_file(root.path().join("environments.toml")).unwrap();
        std::fs::write(root.path().join("environments.toml"), b"").unwrap();
        assert!(matches!(
            release_stale_mount_target(&home, destination).unwrap(),
            MountTargetRelease::Retained(_)
        ));
        // A directory at that name is not touched.
        std::fs::remove_file(root.path().join("environments.toml")).unwrap();
        std::fs::create_dir(root.path().join("environments.toml")).unwrap();
        assert!(matches!(
            release_stale_mount_target(&home, destination).unwrap(),
            MountTargetRelease::Retained(_)
        ));
    }

    #[test]
    fn an_open_connector_closes_only_when_its_peer_is_proved_gone() {
        use ExternalConnectorPhase::{Closed, Connected, Prepared};
        use OpenConnectorDisposition::{Close, Retain};
        assert_eq!(
            dispose_open_connector(Prepared, None),
            Close("startup_abandoned")
        );
        assert_eq!(
            dispose_open_connector(Prepared, Some(IdentityLiveness::Alive)),
            Close("startup_abandoned")
        );
        assert_eq!(
            dispose_open_connector(Connected, Some(IdentityLiveness::DeadOrStale)),
            Close("peer_absent")
        );
        assert!(matches!(
            dispose_open_connector(Connected, Some(IdentityLiveness::Alive)),
            Retain(_)
        ));
        assert!(matches!(
            dispose_open_connector(Connected, Some(IdentityLiveness::Unavailable)),
            Retain(_)
        ));
        assert!(matches!(dispose_open_connector(Connected, None), Retain(_)));
        assert!(matches!(
            dispose_open_connector(Closed, Some(IdentityLiveness::DeadOrStale)),
            Retain(_)
        ));
    }

    #[test]
    fn hello_authentication_joins_every_retained_coordinate() {
        let access = ExternalConnectorCapabilityAccess::new(&"a".repeat(64)).unwrap();
        let capability = access
            .decode(Zeroizing::new(access.generate_value().unwrap()))
            .unwrap();
        let hash = capability.capability_hash().unwrap();
        let hello = ExternalConnectorHello::new(
            "T-fixture".into(),
            "b".repeat(64),
            capability.expose_for_connector_configuration().to_owned(),
        )
        .unwrap();
        authenticate_hello(&hello, "T-fixture", &"b".repeat(64), &hash, &capability).unwrap();
        assert!(
            authenticate_hello(&hello, "T-other", &"b".repeat(64), &hash, &capability).is_err()
        );
        assert!(
            authenticate_hello(&hello, "T-fixture", &"c".repeat(64), &hash, &capability).is_err()
        );
        assert!(
            authenticate_hello(
                &hello,
                "T-fixture",
                &"b".repeat(64),
                &"d".repeat(64),
                &capability,
            )
            .is_err()
        );
    }
}
